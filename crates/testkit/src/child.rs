//! [`NotingStdin`]: whether dropping a child's handles killed it before
//! closing its stdin.
//!
//! A process told to end by its stdin closing starts to exit, and an
//! instrumented one writes its coverage profile then. A kill that lands
//! during that write leaves a truncated `.profraw` that `llvm-profdata`
//! refuses to merge, so code that drops a child's handles should kill it
//! while it still waits for input.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

/// A child's stdin that notes, when dropped, whether the child was killed
/// already. The child writes its process id to `pid_file` first. The note is
/// taken before the pipe closes, so unless it was shut down earlier, a child
/// gone by then can't have ended because its input did.
pub struct NotingStdin {
    inner: Pin<Box<dyn AsyncWrite + Send>>,
    pid_file: PathBuf,
    killed_first: Arc<AtomicBool>,
}

impl NotingStdin {
    /// Wraps `inner`, and returns the flag the drop sets.
    pub fn new(
        inner: Pin<Box<dyn AsyncWrite + Send>>,
        pid_file: PathBuf,
    ) -> (Self, Arc<AtomicBool>) {
        let killed_first = Arc::new(AtomicBool::new(false));
        let noting = Self {
            inner,
            pid_file,
            killed_first: Arc::clone(&killed_first),
        };
        (noting, killed_first)
    }
}

impl AsyncWrite for NotingStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

impl Drop for NotingStdin {
    fn drop(&mut self) {
        let killed_first = std::fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .is_some_and(killed);
        self.killed_first.store(killed_first, Ordering::SeqCst);
    }
}

/// Whether process `pid` is gone or has a SIGKILL pending: what a kill of
/// the process or its group leaves in `/proc` until the process is reaped.
///
/// It reads `/proc/<pid>/status` once, since any thread's tokio runtime can
/// reap a child dropped with `kill_on_drop`. The kill's SIGKILL stays in
/// `ShdPnd` through the exit and as a zombie; once reaped, the file can't be
/// read, or reads `Threads: 0` if the reap landed during the read. A process
/// that exited without a kill shows none of these until it is reaped.
///
/// # Panics
///
/// If this host has no `/proc`, so that every process would look gone.
pub fn killed(pid: u32) -> bool {
    const SIGKILL_BIT: u64 = 1 << 8;
    assert!(
        std::fs::metadata("/proc/self/status").is_ok(),
        "no /proc on this host, so whether a child was killed can't be told"
    );
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return true;
    };
    status.lines().any(|line| match line.split_once(':') {
        Some(("Threads", threads)) => threads.trim() == "0",
        Some(("SigPnd" | "ShdPnd", mask)) => {
            u64::from_str_radix(mask.trim(), 16).is_ok_and(|mask| mask & SIGKILL_BIT != 0)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kill_shows_from_when_it_is_sent_until_after_the_reap() {
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert!(!killed(child.id()));
        child.kill().unwrap();
        assert!(killed(child.id()));
        child.wait().unwrap();
        assert!(killed(child.id()));
    }

    #[test]
    fn a_zombie_that_exited_by_itself_was_not_killed() {
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        let status = format!("/proc/{}/status", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !std::fs::read_to_string(&status).is_ok_and(|status| status.contains("\nState:\tZ")) {
            assert!(std::time::Instant::now() < deadline, "cat never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!killed(child.id()));
        child.wait().unwrap();
    }
}
