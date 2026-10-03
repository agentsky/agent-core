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
/// already. The child writes its process id to `pid_file` first.
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

/// Whether process `pid` is gone, exiting or has a SIGKILL pending: what a
/// kill leaves in `/proc` at once.
///
/// # Panics
///
/// If this host has no `/proc`, so that every process would look gone.
pub fn killed(pid: u32) -> bool {
    const SIGKILL_BIT: u64 = 1 << 8;
    const PF_EXITING: u64 = 0x4;
    assert!(
        std::fs::metadata("/proc/self/stat").is_ok(),
        "no /proc on this host, so whether a child was killed can't be told"
    );
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let (_, rest) = stat
        .rsplit_once(") ")
        .expect("/proc/<pid>/stat names the command in parentheses");
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let exiting = fields
        .first()
        .is_some_and(|state| matches!(*state, "Z" | "X"))
        || fields
            .get(6)
            .and_then(|flags| flags.parse::<u64>().ok())
            .is_some_and(|flags| flags & PF_EXITING != 0);
    let pending = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            line.strip_prefix("SigPnd:")
                .or_else(|| line.strip_prefix("ShdPnd:"))
        })
        .any(|mask| u64::from_str_radix(mask.trim(), 16).is_ok_and(|mask| mask & SIGKILL_BIT != 0));
    exiting || pending
}
