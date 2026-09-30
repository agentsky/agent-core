//! `agentctl lock -- <command>`: run a command while holding the scope's
//! `shared/` lock.
//!
//! agentctl acquires a lease, polling while another lease holds the lock,
//! runs the command, renews the lease while the command runs, and releases
//! it when the command exits. A lease it can't renew, because agentd
//! refused or couldn't be reached before the lease ran out, is lost:
//! another command may take the lock, so agentctl stops the command rather
//! than let it write unguarded, and exits with status 1. On `SIGTERM`,
//! `SIGINT` or `SIGHUP` it stops the command, releases the lease, and exits
//! with 128 plus the signal. An agentctl killed outright stops renewing, and
//! the lease expires on its own.

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
    let (lease, expires_at) = acquire(client, timeout).await?;
    let child = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .spawn();
    let outcome = match child {
        Ok(mut child) => hold(client, lease, expires_at, &mut child).await,
        Err(err) => Err(format!("can't run {}: {err}", program.to_string_lossy())),
    };
    let released = client.send(&LockRequest::Release { lease }).await;
    let ended = outcome?;
    if let Err(err) = released {
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

/// Waits for `child`, renewing `lease` about three times per lease period.
async fn hold(
    client: &Client,
    lease: LeaseId,
    mut expires_at: OffsetDateTime,
    child: &mut Child,
) -> Result<Ended, String> {
    let stop = stop_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            status = child.wait() => {
                return status
                    .map(Ended::Exited)
                    .map_err(|err| format!("waiting for the command failed: {err}"));
            }
            signal = &mut stop => {
                let _ = child.kill().await;
                return Ok(Ended::Signalled(signal));
            }
            () = tokio::time::sleep(renew_wait(expires_at, OffsetDateTime::now_utc())) => {}
        }
        let reason = match client.send(&LockRequest::Renew { lease }).await {
            Ok(LockResponse::Held { expires_at: at, .. }) => {
                expires_at = at;
                continue;
            }
            Ok(LockResponse::Busy | LockResponse::Released) => "the lease expired".to_owned(),
            Err(Failure::Refused(err)) => err.message,
            Err(Failure::Transport(message)) if OffsetDateTime::now_utc() >= expires_at => message,
            Err(Failure::Transport(_)) => continue,
        };
        let _ = child.kill().await;
        return Err(format!(
            "lost the shared/ lock ({reason}); stopped the command"
        ));
    }
}

/// Completes with the signal's number on the first `SIGTERM`, `SIGINT` or
/// `SIGHUP`. Never completes if the handlers can't be installed.
#[cfg(unix)]
async fn stop_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};

    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => SignalKind::terminate().as_raw_value(),
        _ = int.recv() => SignalKind::interrupt().as_raw_value(),
        _ = hup.recv() => SignalKind::hangup().as_raw_value(),
    }
}

/// Never completes: only Unix has the signals.
#[cfg(not(unix))]
async fn stop_signal() -> i32 {
    std::future::pending().await
}

/// How long to wait before renewing a lease that runs out at `expires_at`:
/// a third of what is left, and at least [`MIN_RENEW_WAIT`].
fn renew_wait(expires_at: OffsetDateTime, now: OffsetDateTime) -> Duration {
    let left = Duration::try_from(expires_at - now).unwrap_or(Duration::ZERO);
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
    fn renewals_come_at_a_third_of_the_time_left() {
        let now = datetime!(2026-09-30 12:00:00 UTC);
        assert_eq!(
            renew_wait(now + time::Duration::seconds(30), now),
            Duration::from_secs(10)
        );
        assert_eq!(renew_wait(now, now), MIN_RENEW_WAIT);
        assert_eq!(
            renew_wait(now - time::Duration::seconds(5), now),
            MIN_RENEW_WAIT
        );
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
