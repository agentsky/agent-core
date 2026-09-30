//! `agentctl lock -- <command>`: run a command while holding the scope's
//! `shared/` lock.
//!
//! agentctl acquires a lease, polling while another lease holds the lock,
//! runs the command in a process group of its own, renews the lease while
//! the command runs, and releases it when the command exits.
//!
//! A lease it can't renew is lost: agentd refused the renewal, or no
//! renewal succeeded before the last second of the lease, which is when
//! agentctl stops relying on it (lease times are whole seconds, and another
//! command may take the lock the moment the lease runs out). agentctl then
//! kills the command's whole process group, so no process the command
//! started writes under the next holder, and exits with status 1. A renewal
//! that stalls is abandoned at that point too; it never delays the kill.
//!
//! `SIGTERM`, `SIGINT` and `SIGHUP` are handled from the start: agentctl
//! kills the command's process group if it is running, releases the lease
//! if it holds one, and exits with 128 plus the signal. A process that
//! leaves the group (with `setsid`, say) escapes the kill, and one the
//! command leaves running when it exits on its own is not stopped. An
//! agentctl killed outright stops renewing, and the lease expires on its
//! own.

use std::ffi::OsString;
use std::process::{ExitCode, ExitStatus};
use std::time::Duration;

use core_types::{LeaseId, LockRequest, LockResponse};
use time::OffsetDateTime;
use tokio::process::Child;
use tokio::time::Instant;

use crate::client::{Client, Failure};

/// The first wait between acquire attempts.
const FIRST_RETRY: Duration = Duration::from_millis(100);
/// The longest wait between acquire attempts.
const MAX_RETRY: Duration = Duration::from_secs(1);
/// The shortest wait between renewals.
const MIN_RENEW_WAIT: Duration = Duration::from_millis(200);
/// How long before a lease's expiry agentctl stops relying on it. Lease
/// times are whole seconds, and the lock can be taken over at the expiry
/// itself.
const SAFETY_MARGIN: time::Duration = time::Duration::SECOND;
/// How long releasing the lease may take. The lease expires on its own if
/// agentd doesn't answer, so a signal never waits long for it.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `command` under the lock and returns its exit status.
///
/// # Errors
///
/// If the lock can't be had within `timeout`, the command can't be started,
/// or the lease is lost while it runs.
pub async fn run(
    client: &Client,
    timeout: Duration,
    command: &[OsString],
) -> Result<ExitCode, String> {
    let (program, args) = command.split_first().ok_or("lock needs a command")?;
    let mut stop = Stop::install();
    let (lease, expires_at) = tokio::select! {
        acquired = acquire(client, timeout) => acquired?,
        signal = stop.recv() => return Ok(signal_code(signal)),
    };
    let mut child = tokio::process::Command::new(program);
    child.args(args).kill_on_drop(true);
    #[cfg(unix)]
    child.process_group(0);
    let outcome = match child.spawn() {
        Ok(mut child) => hold(client, lease, expires_at, &mut child, &mut stop).await,
        Err(err) => Err(format!("can't run {}: {err}", program.to_string_lossy())),
    };
    let release = LockRequest::Release { lease };
    let released = tokio::select! {
        released = client.send_within(&release, RELEASE_TIMEOUT) => {
            released.err().map(|err| err.to_string())
        }
        _ = stop.recv() => Some("interrupted".to_owned()),
    };
    let ended = outcome?;
    if let Some(err) = released {
        eprintln!("agentctl: releasing the shared/ lock failed ({err}); it expires on its own");
    }
    Ok(match ended {
        Ended::Exited(status) => exit_code(status),
        Ended::Signalled(signal) => signal_code(signal),
    })
}

/// How the command under the lock ended.
enum Ended {
    /// It exited on its own.
    Exited(ExitStatus),
    /// agentctl got this signal, and stopped the command.
    Signalled(i32),
}

async fn acquire(client: &Client, timeout: Duration) -> Result<(LeaseId, OffsetDateTime), String> {
    let deadline = Instant::now() + timeout;
    let mut wait = FIRST_RETRY;
    let mut told = false;
    loop {
        match client.send(&LockRequest::Acquire).await? {
            LockResponse::Held { lease, expires_at } => return Ok((lease, expires_at)),
            LockResponse::Busy | LockResponse::Released => {}
        }
        if Instant::now() + wait > deadline {
            return Err(format!(
                "gave up after {}s waiting for the shared/ lock; another command holds it",
                timeout.as_secs()
            ));
        }
        if !told {
            eprintln!("agentctl: waiting for the shared/ lock");
            told = true;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_RETRY);
    }
}

/// Waits for `child`, renewing `lease` about three times per lease period,
/// and kills it on a stop signal or when the lease can no longer be relied
/// on.
async fn hold(
    client: &Client,
    lease: LeaseId,
    mut expires_at: OffsetDateTime,
    child: &mut Child,
    stop: &mut Stop,
) -> Result<Ended, String> {
    let mut failure = None;
    loop {
        let left = time_left(expires_at, OffsetDateTime::now_utc());
        let deadline = Instant::now() + left;
        let renewal = async {
            tokio::time::sleep(renew_wait(left)).await;
            let limit = deadline.saturating_duration_since(Instant::now());
            client
                .send_within(&LockRequest::Renew { lease }, limit)
                .await
        };
        let reason = tokio::select! {
            status = child.wait() => {
                return status
                    .map(Ended::Exited)
                    .map_err(|err| format!("waiting for the command failed: {err}"));
            }
            signal = stop.recv() => {
                kill(child).await;
                return Ok(Ended::Signalled(signal));
            }
            () = tokio::time::sleep_until(deadline) => failure
                .take()
                .unwrap_or_else(|| "agentd didn't renew it in time".to_owned()),
            renewed = renewal => match renewed {
                Ok(LockResponse::Held { expires_at: at, .. }) => {
                    expires_at = at;
                    failure = None;
                    continue;
                }
                Ok(LockResponse::Busy | LockResponse::Released) => "the lease expired".to_owned(),
                Err(Failure::Refused(err)) => err.message,
                Err(Failure::Transport(message)) => {
                    failure = Some(message);
                    continue;
                }
            },
        };
        kill(child).await;
        return Err(format!(
            "lost the shared/ lock ({reason}); stopped the command"
        ));
    }
}

/// Kills `child`'s process group, which is everything it started that
/// stayed in it, then `child` itself, and reaps it.
async fn kill(child: &mut Child) {
    #[cfg(unix)]
    if let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

/// The signals that stop `agentctl lock`: `SIGTERM`, `SIGINT` and
/// `SIGHUP`, handled from [`install`](Self::install) on.
#[cfg(unix)]
struct Stop(Option<[tokio::signal::unix::Signal; 3]>);

#[cfg(unix)]
impl Stop {
    const KINDS: [tokio::signal::unix::SignalKind; 3] = [
        tokio::signal::unix::SignalKind::terminate(),
        tokio::signal::unix::SignalKind::interrupt(),
        tokio::signal::unix::SignalKind::hangup(),
    ];

    /// Installs the handlers. If they can't be installed, the signals keep
    /// their default action, and [`recv`](Self::recv) never completes.
    fn install() -> Self {
        use tokio::signal::unix::signal;

        let [term, int, hup] = Self::KINDS;
        match (signal(term), signal(int), signal(hup)) {
            (Ok(term), Ok(int), Ok(hup)) => Self(Some([term, int, hup])),
            _ => Self(None),
        }
    }

    /// Completes with the signal's number when one of them arrives. A
    /// signal that arrived while nothing was waiting is reported by the next
    /// call.
    async fn recv(&mut self) -> i32 {
        let [term, int, hup] = Self::KINDS;
        let Some([on_term, on_int, on_hup]) = &mut self.0 else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = on_term.recv() => term.as_raw_value(),
            _ = on_int.recv() => int.as_raw_value(),
            _ = on_hup.recv() => hup.as_raw_value(),
        }
    }
}

/// Only Unix has the signals.
#[cfg(not(unix))]
struct Stop;

#[cfg(not(unix))]
impl Stop {
    fn install() -> Self {
        Self
    }

    /// Never completes.
    async fn recv(&mut self) -> i32 {
        std::future::pending().await
    }
}

/// How long a lease that runs out at `expires_at` can still be relied on:
/// until [`SAFETY_MARGIN`] before its expiry.
fn time_left(expires_at: OffsetDateTime, now: OffsetDateTime) -> Duration {
    Duration::try_from(expires_at - SAFETY_MARGIN - now).unwrap_or(Duration::ZERO)
}

/// How long to wait before renewing a lease that can be relied on for
/// `left`: a third of it, and at least [`MIN_RENEW_WAIT`].
fn renew_wait(left: Duration) -> Duration {
    (left / 3).max(MIN_RENEW_WAIT)
}

/// The exit status to pass on: the command's code, or 128 plus the signal
/// that killed it, as shells report it.
fn exit_code(status: ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1));
    }
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return signal_code(signal);
    }
    ExitCode::FAILURE
}

/// 128 plus `signal`, as shells report a command the signal ended.
fn signal_code(signal: i32) -> ExitCode {
    ExitCode::from(u8::try_from((128 + signal) & 0xff).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn a_lease_is_relied_on_until_a_second_before_it_runs_out() {
        let now = datetime!(2026-09-30 12:00:00 UTC);
        assert_eq!(
            time_left(now + time::Duration::seconds(30), now),
            Duration::from_secs(29)
        );
        assert_eq!(
            time_left(now + time::Duration::milliseconds(1_500), now),
            Duration::from_millis(500)
        );
        assert_eq!(time_left(now + time::Duration::SECOND, now), Duration::ZERO);
        assert_eq!(time_left(now, now), Duration::ZERO);
    }

    #[test]
    fn renewals_come_at_a_third_of_the_time_left() {
        assert_eq!(renew_wait(Duration::from_secs(30)), Duration::from_secs(10));
        assert_eq!(renew_wait(Duration::ZERO), MIN_RENEW_WAIT);
        assert_eq!(renew_wait(Duration::from_millis(300)), MIN_RENEW_WAIT);
    }

    #[cfg(unix)]
    #[test]
    fn exit_codes_pass_through_and_signals_add_128() {
        use std::os::unix::process::ExitStatusExt as _;

        assert_eq!(exit_code(ExitStatus::from_raw(0)), ExitCode::SUCCESS);
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), ExitCode::from(3));
        assert_eq!(exit_code(ExitStatus::from_raw(9)), ExitCode::from(137));
    }
}
