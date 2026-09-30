//! The session's total cost as the CLI restores it on `--resume`.
//!
//! Claude Code appends `{"type":"cost-state","totalCostUSD":…,…}` to a
//! session's transcript when a process exits, and none when one is killed.
//! A `--resume`d process starts its running `total_cost_usd` from the last
//! such line, so its first result reports that total plus the turn's own
//! cost. [`restored_total`] reads the same line, so the runner can take it
//! off.
//!
//! The transcript is in the session's directory, which the agent can
//! write. So nothing on the way to it is followed if it is a symlink, the
//! file must be a regular file, and at most [`MAX_SCAN_BYTES`] of it, read
//! from its end, are searched. The CLI restores from the same line, so
//! whatever the agent wrote there, the difference is still the turn's cost,
//! unless something in the container changes the line between this read
//! and the CLI's.

use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use core_types::SessionId;
use rustix::fs::{Mode, OFlags};
use serde_json::Value;

/// How much of a transcript is searched, from its end, for the last
/// `cost-state` line.
pub(crate) const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// How much is read at a time.
const CHUNK_BYTES: u64 = 64 * 1024;

/// The longest line read as a possible `cost-state` line. The CLI's are a
/// few hundred bytes; longer lines are messages and tool output.
const MAX_COST_LINE_BYTES: usize = 64 * 1024;

/// What the CLI restores as the running total of a `--resume`d process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Restored {
    /// This total, in US dollars: the last `cost-state` line's, or 0
    /// when the transcript has none.
    Total(f64),
    /// The transcript couldn't be read, or its last `cost-state` line lies
    /// further from its end than [`MAX_SCAN_BYTES`].
    Unknown,
}

/// What the CLI will restore for `session`, as
/// [`ClaudeProcess::count_cost_from`](crate::ClaudeProcess::count_cost_from)
/// takes it: [`restored_total`], read off the async runtime, or `None` when
/// it isn't known.
pub(crate) async fn restored_cost(session_dir: &Path, session: SessionId) -> Option<f64> {
    let dir = session_dir.to_owned();
    match tokio::task::spawn_blocking(move || restored_total(&dir, session)).await {
        Ok(Restored::Total(total)) => Some(total),
        Ok(Restored::Unknown) | Err(_) => {
            tracing::warn!(%session, "the total a resumed process restores isn't known; its first turn has no cost");
            None
        }
    }
}

/// What the CLI will restore for `session`, whose directory is
/// `session_dir` (`sessions/<id>/` on its volume, as agentd sees it). The
/// transcript is `claude/projects/<id>/<id>.jsonl` there, since the runner
/// names the project directory after the session.
pub(crate) fn restored_total(session_dir: &Path, session: SessionId) -> Restored {
    let id = session.to_string();
    match open_transcript(session_dir, &id) {
        Ok(Some(file)) => {
            last_cost_state(&file, MAX_SCAN_BYTES, CHUNK_BYTES).unwrap_or(Restored::Unknown)
        }
        Ok(None) => Restored::Total(0.0),
        Err(err) => {
            tracing::warn!(%session, error = %err, "couldn't open the transcript to read its restored cost");
            Restored::Unknown
        }
    }
}

/// Opens the transcript without following a symlink anywhere below
/// `session_dir`: `Ok(None)` when it doesn't exist.
fn open_transcript(session_dir: &Path, id: &str) -> rustix::io::Result<Option<File>> {
    const DIR: OFlags = OFlags::PATH
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const FILE: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);
    let found = |result: rustix::io::Result<_>| match result {
        Ok(fd) => Ok(Some(fd)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(err) => Err(err),
    };
    let Some(mut dir) = found(rustix::fs::open(session_dir, DIR, Mode::empty()))? else {
        return Ok(None);
    };
    for name in ["claude", "projects", id] {
        let Some(next) = found(rustix::fs::openat(&dir, name, DIR, Mode::empty()))? else {
            return Ok(None);
        };
        dir = next;
    }
    let Some(fd) = found(rustix::fs::openat(
        &dir,
        format!("{id}.jsonl"),
        FILE,
        Mode::empty(),
    ))?
    else {
        return Ok(None);
    };
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(rustix::io::Errno::INVAL);
    }
    Ok(Some(File::from(fd)))
}

/// The last `cost-state` line's total in `file`, searching back from its
/// end, `chunk` bytes at a time, at most `max_scan` bytes. `None` when
/// reading fails.
fn last_cost_state(file: &File, max_scan: u64, chunk: u64) -> Option<Restored> {
    let len = file.metadata().ok()?.len();
    let mut end = len;
    let mut carry = Some(Vec::new());
    while end > 0 {
        if len - end >= max_scan {
            return Some(Restored::Unknown);
        }
        let start = end.saturating_sub(chunk);
        let mut bytes = vec![0; usize::try_from(end - start).ok()?];
        file.read_exact_at(&mut bytes, start).ok()?;
        let pieces: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        let (head, lines) = match pieces.split_first() {
            Some((first, rest)) if start > 0 => (Some(*first), rest),
            _ => (None, &pieces[..]),
        };
        match lines.split_last() {
            None => carry = head.and_then(|head| joined(head, carry.take())),
            Some((latest, earlier)) => {
                let latest = joined(latest, carry.take());
                let earlier = earlier
                    .iter()
                    .rev()
                    .filter(|line| line.len() <= MAX_COST_LINE_BYTES);
                if let Some(total) = latest
                    .as_deref()
                    .into_iter()
                    .chain(earlier.copied())
                    .find_map(cost_state)
                {
                    return Some(Restored::Total(total));
                }
                carry = head
                    .filter(|head| head.len() <= MAX_COST_LINE_BYTES)
                    .map(<[u8]>::to_vec);
            }
        }
        end = start;
    }
    Some(Restored::Total(0.0))
}

/// `piece` followed by what came after it on its line, or `None` once
/// that is longer than any `cost-state` line.
fn joined(piece: &[u8], after: Option<Vec<u8>>) -> Option<Vec<u8>> {
    let after = after?;
    if piece.len() + after.len() > MAX_COST_LINE_BYTES {
        return None;
    }
    let mut line = piece.to_vec();
    line.extend_from_slice(&after);
    Some(line)
}

/// The total of a `cost-state` line: a finite number of at least 0.
fn cost_state(line: &[u8]) -> Option<f64> {
    if !line
        .windows(b"cost-state".len())
        .any(|w| w == b"cost-state")
    {
        return None;
    }
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("type")?.as_str()? != "cost-state" {
        return None;
    }
    value
        .get("totalCostUSD")?
        .as_f64()
        .filter(|total| total.is_finite() && *total >= 0.0)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("transcript-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn transcript(&self, session: SessionId) -> PathBuf {
            let id = session.to_string();
            let dir = self.0.join("claude").join("projects").join(&id);
            std::fs::create_dir_all(&dir).unwrap();
            dir.join(format!("{id}.jsonl"))
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cost_line(total: f64) -> String {
        format!(r#"{{"type":"cost-state","totalCostUSD":{total},"modelUsage":{{}}}}"#)
    }

    fn message(len: usize) -> String {
        format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(len)
        )
    }

    fn write(path: &Path, lines: &[String]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn scan(path: &Path, max_scan: u64, chunk: u64) -> Option<Restored> {
        last_cost_state(&File::open(path).unwrap(), max_scan, chunk)
    }

    #[test]
    fn without_a_transcript_or_a_cost_line_the_total_is_zero() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.0));
        assert_eq!(
            restored_total(&dir.0.join("missing"), session),
            Restored::Total(0.0)
        );
        let path = dir.transcript(session);
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.0));
        write(&path, &[message(10), message(20)]);
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.0));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.0));
    }

    #[test]
    fn the_last_cost_line_wins_even_with_messages_after_it() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        let path = dir.transcript(session);
        write(
            &path,
            &[
                message(5),
                cost_line(0.25),
                message(5),
                cost_line(0.75),
                message(5),
                message(5),
            ],
        );
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.75));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(cost_line(1.5).as_bytes())
            .unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            Restored::Total(1.5),
            "a last line without its newline counts"
        );
    }

    #[test]
    fn lines_are_found_across_chunks() {
        let dir = Dir::new();
        let path = dir.transcript(SessionId::new_v4());
        let lines = [message(3), cost_line(0.5), message(40), message(1)];
        write(&path, &lines);
        for chunk in 1..=40 {
            assert_eq!(
                scan(&path, u64::MAX, chunk),
                Some(Restored::Total(0.5)),
                "chunk {chunk}"
            );
        }
        write(&path, &[cost_line(0.5)]);
        for chunk in [1, 7, 1_000] {
            assert_eq!(scan(&path, u64::MAX, chunk), Some(Restored::Total(0.5)));
        }
    }

    #[test]
    fn a_cost_line_too_far_from_the_end_is_unknown() {
        let dir = Dir::new();
        let path = dir.transcript(SessionId::new_v4());
        write(&path, &[cost_line(0.5), message(200)]);
        assert_eq!(scan(&path, 100, 16), Some(Restored::Unknown));
        assert_eq!(scan(&path, 10_000, 16), Some(Restored::Total(0.5)));
        write(&path, &[message(200)]);
        assert_eq!(
            scan(&path, 100, 16),
            Some(Restored::Unknown),
            "an unread start could still hold one"
        );
    }

    #[test]
    fn values_that_arent_totals_and_overlong_lines_are_passed_over() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        let path = dir.transcript(session);
        let long = format!(
            r#"{{"type":"cost-state","totalCostUSD":9,"pad":"{}"}}"#,
            "y".repeat(MAX_COST_LINE_BYTES)
        );
        write(
            &path,
            &[
                cost_line(0.125),
                r#"{"type":"cost-state","totalCostUSD":-1}"#.to_owned(),
                r#"{"type":"cost-state","totalCostUSD":"2"}"#.to_owned(),
                r#"{"type":"cost-state"}"#.to_owned(),
                r#"{"type":"user","text":"cost-state","totalCostUSD":3}"#.to_owned(),
                "cost-state, not JSON".to_owned(),
                long,
            ],
        );
        assert_eq!(restored_total(&dir.0, session), Restored::Total(0.125));
        assert_eq!(scan(&path, u64::MAX, 1_000), Some(Restored::Total(0.125)));
    }

    #[test]
    fn nothing_is_followed_through_a_symlink_and_only_a_file_is_read() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        let elsewhere = Dir::new();
        let target = elsewhere.transcript(session);
        write(&target, &[cost_line(4.0)]);

        let path = dir.transcript(session);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(restored_total(&dir.0, session), Restored::Unknown);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(dir.0.join("claude")).unwrap();
        std::os::unix::fs::symlink(elsewhere.0.join("claude"), dir.0.join("claude")).unwrap();
        assert_eq!(restored_total(&dir.0, session), Restored::Unknown);

        std::fs::remove_file(dir.0.join("claude")).unwrap();
        let path = dir.transcript(session);
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            Restored::Unknown,
            "a directory isn't a transcript"
        );
    }
}
