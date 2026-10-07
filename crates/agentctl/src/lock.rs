//! `agentctl lock -- <command>`: run a command while holding the scope's
//! `shared/` lock.
//!
//! agentctl acquires a lease, polling while another lease holds the lock,
//! runs the command in a process group of its own, renews the lease while
//! the command runs, and releases it when the command exits.
//!
//! A lease is timed on agentctl's own monotonic clock, from when it sent the
//! request that agentd answered with the lease's `seconds_left`, so skew
//! between agentd's clock and the sandbox's doesn't matter. agentctl relies
//! on it until two seconds before that many seconds have passed: one for the
//! whole seconds agentd keeps leases in, and one because another command may
//! take the lock the moment the lease runs out. A lease that leaves too
//! little time to renew it is given back at once, and `lock` fails.
//!
//! A lease it can't renew is lost: agentd refused the renewal, or no
//! renewal succeeded before that deadline. agentctl then kills the
//! command's whole process group, so no process the command started writes
//! under the next holder, and exits with status 1. A renewal that stalls is
//! abandoned at the deadline too; it never delays the kill. A renewal that
//! failed on agentd's side (an internal error, such as a busy database) or
//! in transit is retried until the deadline.
//!
//! `SIGTERM`, `SIGINT` and `SIGHUP` are handled from the start. During an
//! acquire, agentctl lets a request already sent finish, for up to two
//! seconds, and gives back the lease if it was granted. While the command
//! runs, agentctl passes the signal on to the command's process group,
//! gives it up to two seconds to exit (never past the lease's deadline),
//! then kills the group. Either way it releases the lease and exits with
//! 128 plus the signal. A process that leaves the group (with `setsid`,
//! say) escapes the kill, and one the command leaves running when it exits
//! on its own is not stopped. An agentctl killed outright stops renewing,
//! and the lease expires on its own.

use std::ffi::OsString;
use std::process::{ExitCode, ExitStatus};
use std::time::Duration;

use core_types::{CtlErrorCode, LeaseId, LockRequest, LockResponse};
use tokio::process::Child;
use tokio::time::Instant;

use crate::client::{Client, Failure};

/// The longest `lock` waits for the lock, whatever `--timeout` says, so a
/// huge timeout can't overflow the deadline.
const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// The first wait between acquire attempts.
const FIRST_RETRY: Duration = Duration::from_millis(100);
/// The longest wait between acquire attempts.
const MAX_RETRY: Duration = Duration::from_secs(1);
/// The shortest wait between renewals.
const MIN_RENEW_WAIT: Duration = Duration::from_millis(200);
/// How much shorter than its `seconds_left` a lease may really be: leases
/// are kept in whole seconds.
const ROUNDING: Duration = Duration::from_secs(1);
/// How long before a lease's expiry agentctl stops relying on it: the lock
/// can be taken over at the expiry itself.
const SAFETY_MARGIN: Duration = Duration::from_secs(1);
/// The least time a new lease must leave before agentctl stops relying on
/// it: enough to wait [`MIN_RENEW_WAIT`] and then renew it. With
/// [`ROUNDING`] and [`SAFETY_MARGIN`], a lease must last three seconds.
const MIN_TIME_LEFT: Duration = Duration::from_millis(500);
/// How long an acquire already sent may take to finish after a stop signal,
/// so a lease it was granted can be given back.
const ACQUIRE_GRACE: Duration = Duration::from_secs(2);
/// How long the command may take to exit after agentctl passes a stop
/// signal on to it, before its process group is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How often agentctl checks whether the command has exited after passing a
/// stop signal on.
const STOP_POLL: Duration = Duration::from_millis(20);
/// How long releasing the lease may take. The lease expires on its own if
/// agentd doesn't answer, so a signal never waits long for it.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a lease whose `seconds_left` the clock can't hold isn't relied on.
const TOO_LONG: &str = "agentd granted a lease longer than the clock can hold";

/// Runs `command` under the lock and returns its exit status.
///
/// # Errors
///
/// If the lock can't be had within `timeout` (at most [`MAX_WAIT`]), its
/// lease is too short to hold, the command can't be started, or the lease is
/// lost while it runs.
pub async fn run(
    client: &Client,
    timeout: Duration,
    command: &[OsString],
) -> Result<ExitCode, String> {
    let (program, args) = command.split_first().ok_or("lock needs a command")?;
    let mut stop = Stop::install();
    let (lease, deadline) = match acquire(client, timeout, &mut stop).await? {
        Acquired::Held { lease, deadline } => (lease, deadline),
        Acquired::Stopped(signal) => return Ok(signal_code(signal)),
    };
    let mut child = tokio::process::Command::new(program);
    child.args(args).kill_on_drop(true);
    #[cfg(unix)]
    child.process_group(0);
    let outcome = match child.spawn() {
        Ok(mut child) => hold(client, lease, deadline, &mut child, &mut stop).await,
        Err(err) => Err(format!("can't run {}: {err}", program.to_string_lossy())),
    };
    let released = release(client, lease, &mut stop).await;
    let ended = outcome?;
    report_release(released);
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

/// How [`acquire`] ended, short of an error.
enum Acquired {
    /// agentctl holds `lease`, and may rely on it until `deadline`.
    Held { lease: LeaseId, deadline: Instant },
    /// agentctl got this signal, and holds no lease.
    Stopped(i32),
}

async fn acquire(client: &Client, timeout: Duration, stop: &mut Stop) -> Result<Acquired, String> {
    let timeout = timeout.min(MAX_WAIT);
    let give_up = Instant::now() + timeout;
    let mut wait = FIRST_RETRY;
    let mut told = false;
    loop {
        let sent = Instant::now();
        let request = client.send(&LockRequest::Acquire);
        tokio::pin!(request);
        let answer = tokio::select! {
            answer = &mut request => answer?,
            signal = stop.recv() => {
                if let Ok(Ok(LockResponse::Held { lease, .. })) =
                    tokio::time::timeout(ACQUIRE_GRACE, request).await
                {
                    report_release(release(client, lease, stop).await);
                }
                return Ok(Acquired::Stopped(signal));
            }
        };
        match answer {
            LockResponse::Held {
                lease,
                seconds_left,
                ..
            } => {
                let Some(deadline) = reliable_until(sent, seconds_left) else {
                    let _ = release(client, lease, stop).await;
                    return Err(format!("couldn't hold the shared/ lock ({TOO_LONG})"));
                };
                if deadline.saturating_duration_since(Instant::now()) < MIN_TIME_LEFT {
                    let _ = release(client, lease, stop).await;
                    return Err(format!(
                        "the shared/ lock's lease is too short to hold (agentd granted \
                         {seconds_left}s; agentctl needs at least 3s)"
                    ));
                }
                return Ok(Acquired::Held { lease, deadline });
            }
            LockResponse::Busy | LockResponse::Released => {}
        }
        if Instant::now() + wait > give_up {
            return Err(format!(
                "gave up after {}s waiting for the shared/ lock; another command holds it",
                timeout.as_secs()
            ));
        }
        if !told {
            eprintln!("agentctl: waiting for the shared/ lock");
            told = true;
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            signal = stop.recv() => return Ok(Acquired::Stopped(signal)),
        }
        wait = (wait * 2).min(MAX_RETRY);
    }
}

/// Waits for `child`, renewing `lease` about three times per lease period,
/// and stops it on a stop signal or when the lease can no longer be relied
/// on, which is at `deadline` unless a renewal moves it.
async fn hold(
    client: &Client,
    lease: LeaseId,
    mut deadline: Instant,
    child: &mut Child,
    stop: &mut Stop,
) -> Result<Ended, String> {
    let mut failure = None;
    loop {
        let until = deadline;
        let renewal = async move {
            tokio::time::sleep(renew_wait(until.saturating_duration_since(Instant::now()))).await;
            let sent = Instant::now();
            let limit = until.saturating_duration_since(sent);
            let renewed = client
                .send_within(&LockRequest::Renew { lease }, limit)
                .await;
            (sent, renewed)
        };
        let reason = tokio::select! {
            status = child.wait() => {
                return status
                    .map(Ended::Exited)
                    .map_err(|err| format!("waiting for the command failed: {err}"));
            }
            signal = stop.recv() => {
                let grace = STOP_GRACE.min(deadline.saturating_duration_since(Instant::now()));
                interrupt(child, signal, grace).await;
                return Ok(Ended::Signalled(signal));
            }
            () = tokio::time::sleep_until(deadline) => failure
                .take()
                .unwrap_or_else(|| "agentd didn't renew it in time".to_owned()),
            (sent, renewed) = renewal => match renewed {
                Ok(LockResponse::Held { seconds_left, .. }) => match reliable_until(sent, seconds_left) {
                    Some(until) => {
                        deadline = until;
                        failure = None;
                        continue;
                    }
                    None => TOO_LONG.to_owned(),
                },
                Ok(LockResponse::Busy | LockResponse::Released) => "the lease expired".to_owned(),
                Err(Failure::Refused(err)) if err.code == CtlErrorCode::Internal => {
                    failure = Some(err.message);
                    continue;
                }
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

/// Gives `lease` back, within [`RELEASE_TIMEOUT`] and unless another stop
/// signal comes first. Returns why it failed, if it did.
async fn release(client: &Client, lease: LeaseId, stop: &mut Stop) -> Option<String> {
    let request = LockRequest::Release { lease };
    tokio::select! {
        released = client.send_within(&request, RELEASE_TIMEOUT) => {
            released.err().map(|err| err.to_string())
        }
        _ = stop.recv() => Some("interrupted".to_owned()),
    }
}

/// Tells the model that releasing the lease failed, if it did.
fn report_release(failure: Option<String>) {
    if let Some(err) = failure {
        eprintln!("agentctl: releasing the shared/ lock failed ({err}); it expires on its own");
    }
}

/// Passes `signal` on to `child`'s process group, waits up to `grace` for
/// `child` to exit, then kills the group and reaps `child`.
///
/// `child` is left unreaped while it is waited for, so its process id, the
/// group's id, can't be reused before the group is killed.
async fn interrupt(child: &mut Child, signal: i32, grace: Duration) {
    #[cfg(unix)]
    if let (Some(group), Some(signal)) = (
        group(child),
        rustix::process::Signal::from_named_raw(signal),
    ) {
        use rustix::process::{WaitId, WaitIdOptions, kill_process_group, waitid};

        if kill_process_group(group, signal).is_ok() {
            let give_up = Instant::now() + grace;
            let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
            while Instant::now() < give_up
                && matches!(waitid(WaitId::Pid(group), options), Ok(None))
            {
                tokio::time::sleep(STOP_POLL).await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (signal, grace);
    kill(child).await;
}

/// Kills `child`'s process group, which is everything it started that
/// stayed in it, then `child` itself, and reaps it.
async fn kill(child: &mut Child) {
    #[cfg(unix)]
    if let Some(group) = group(child) {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

/// The id of `child`'s process group, which is its own process id, while
/// it hasn't been reaped.
#[cfg(unix)]
fn group(child: &Child) -> Option<rustix::process::Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
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

/// When agentctl stops relying on a lease that agentd said, answering a
/// request sent at `sent`, has `seconds_left`: [`ROUNDING`] and
/// [`SAFETY_MARGIN`] before `seconds_left` have passed since `sent`.
/// agentd measured it no earlier than `sent`, so the time the answer took
/// only makes this earlier. `None` for a `seconds_left` past what the
/// clock can hold, which is not relied on at all.
fn reliable_until(sent: Instant, seconds_left: u64) -> Option<Instant> {
    sent.checked_add(Duration::from_secs(seconds_left).saturating_sub(ROUNDING + SAFETY_MARGIN))
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
    use super::*;

    #[test]
    fn a_lease_is_relied_on_until_two_seconds_before_its_seconds_left() {
        let sent = Instant::now();
        assert_eq!(
            reliable_until(sent, 30),
            Some(sent + Duration::from_secs(28))
        );
        assert_eq!(reliable_until(sent, 3), Some(sent + Duration::from_secs(1)));
        assert_eq!(reliable_until(sent, 2), Some(sent));
        assert_eq!(reliable_until(sent, 0), Some(sent));
    }

    #[test]
    fn a_lease_too_long_for_the_clock_is_not_relied_on() {
        let sent = Instant::now();
        assert_eq!(reliable_until(sent, u64::MAX), None);
    }

    #[test]
    fn the_shortest_lease_agentctl_holds_lasts_three_seconds() {
        let sent = Instant::now();
        assert!(reliable_until(sent, 3).unwrap() - sent >= MIN_TIME_LEFT);
        assert!(reliable_until(sent, 2).unwrap() - sent < MIN_TIME_LEFT);
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
