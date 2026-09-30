//! The session's total cost as the CLI restores it on `--resume`.
//!
//! Claude Code appends `{"type":"cost-state","totalCostUSD":…,…}` to a
//! session's transcript when a process exits, and none when one is killed.
//! A `--resume`d process starts its running `total_cost_usd` from the last
//! line of the transcript that parses as a `cost-state` line, so its first
//! result reports that total plus the turn's own cost. [`restored_total`]
//! finds the same line, so the runner can take it off.
//!
//! The transcript is in the session's directory, which the agent can
//! write. So nothing on the way to it is followed if it is a symlink, the
//! file must be a regular file, and at most [`MAX_SCAN_BYTES`] of it, read
//! from its end, are searched. Whatever the runner can't read within those
//! bounds could be the line the CLI takes, so it makes the total unknown
//! rather than lead to an earlier line: a line longer than
//! [`MAX_COST_LINE_BYTES`], one that isn't a JSON object, a `cost-state`
//! line whose `totalCostUSD` isn't a plausible total, or a start of the
//! file past the scan. The CLI parses JSON more leniently than the runner
//! in places, so a line the runner can't parse is unknown too.

use std::borrow::Cow;
use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use core_types::SessionId;
use rustix::fs::{Mode, OFlags};
use serde::Deserialize;
use serde_json::Value;

use crate::stream::plausible_total;

/// How much of a transcript is searched, from its end, for the last
/// `cost-state` line.
pub(crate) const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// How much is read at a time.
const CHUNK_BYTES: u64 = 64 * 1024;

/// The longest line read. The CLI's `cost-state` lines are a few hundred
/// bytes; a longer line after the last one the runner finds makes the
/// total unknown, since it can't tell whether the CLI would take it.
const MAX_COST_LINE_BYTES: usize = 64 * 1024;

/// What the CLI will restore for `session`, as
/// [`ClaudeProcess::count_cost_from`](crate::ClaudeProcess::count_cost_from)
/// takes it: [`restored_total`], read off the async runtime.
pub(crate) async fn restored_cost(session_dir: &Path, session: SessionId) -> Option<f64> {
    let dir = session_dir.to_owned();
    let restored = tokio::task::spawn_blocking(move || restored_total(&dir, session))
        .await
        .ok()
        .flatten();
    if restored.is_none() {
        tracing::warn!(%session, "the total a resumed process restores isn't known; its first turn has no cost");
    }
    restored
}

/// What the CLI will restore, in US dollars, for `session`, whose
/// directory is `session_dir` (`sessions/<id>/` on its volume, as agentd
/// sees it): the last `cost-state` line's total, or 0 when the transcript
/// has none. `None` when that isn't known: the transcript is missing (the
/// CLI then refuses the `--resume`) or can't be read, or the line the CLI
/// would take can't be read within the bounds the [module docs](self)
/// give. The transcript is `claude/projects/<id>/<id>.jsonl` there, since
/// the runner names the project directory after the session.
pub(crate) fn restored_total(session_dir: &Path, session: SessionId) -> Option<f64> {
    match open_transcript(session_dir, &session.to_string()) {
        Ok(file) => last_cost_state(&file, MAX_SCAN_BYTES, CHUNK_BYTES),
        Err(err) => {
            tracing::warn!(%session, error = %err, "couldn't open the transcript to read its restored cost");
            None
        }
    }
}

/// Opens the transcript without following a symlink anywhere below
/// `session_dir`.
fn open_transcript(session_dir: &Path, id: &str) -> rustix::io::Result<File> {
    const DIR: OFlags = OFlags::PATH
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const FILE: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);
    let mut dir = rustix::fs::open(session_dir, DIR, Mode::empty())?;
    for name in ["claude", "projects", id] {
        dir = rustix::fs::openat(&dir, name, DIR, Mode::empty())?;
    }
    let fd = rustix::fs::openat(&dir, format!("{id}.jsonl"), FILE, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(rustix::io::Errno::INVAL);
    }
    Ok(File::from(fd))
}

/// The total of the last `cost-state` line in `file`, searching back from
/// its end, `chunk` bytes at a time, at most `max_scan` bytes: 0 when the
/// whole file has none, and `None` when it isn't known.
fn last_cost_state(file: &File, max_scan: u64, chunk: u64) -> Option<f64> {
    let len = file.metadata().ok()?.len();
    let mut end = len;
    let mut line = Vec::new();
    while end > 0 {
        if len - end >= max_scan {
            return None;
        }
        let start = end.saturating_sub(chunk);
        let mut bytes = vec![0; usize::try_from(end - start).ok()?];
        file.read_exact_at(&mut bytes, start).ok()?;
        let mut pieces = bytes.rsplit(|b| *b == b'\n');
        let mut piece = pieces.next()?;
        for earlier in pieces {
            line = joined(piece, &line)?;
            if let Some(total) = cost_state(&line)? {
                return Some(total);
            }
            line.clear();
            piece = earlier;
        }
        line = joined(piece, &line)?;
        end = start;
    }
    Some(cost_state(&line)?.unwrap_or(0.0))
}

/// `piece` followed by `after`, the rest of its line, or `None` once the
/// line is longer than [`MAX_COST_LINE_BYTES`].
fn joined(piece: &[u8], after: &[u8]) -> Option<Vec<u8>> {
    if piece.len() + after.len() > MAX_COST_LINE_BYTES {
        return None;
    }
    let mut line = piece.to_vec();
    line.extend_from_slice(after);
    Some(line)
}

/// The fields of a transcript line the runner reads. A line with either
/// twice doesn't parse, so it is unknown rather than read as the CLI
/// might.
#[derive(Deserialize)]
struct Entry<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(rename = "totalCostUSD")]
    total: Option<Value>,
}

/// What one line says: `Some(Some(total))` for a `cost-state` line with a
/// plausible total, `Some(None)` for a blank line or one of another type,
/// and `None` for a line the runner can't read.
fn cost_state(line: &[u8]) -> Option<Option<f64>> {
    if line.trim_ascii().is_empty() {
        return Some(None);
    }
    let entry: Entry<'_> = serde_json::from_slice(line).ok()?;
    if entry.kind.as_deref() != Some("cost-state") {
        return Some(None);
    }
    let total = entry.total?.as_f64().filter(|total| plausible_total(*total))?;
    Some(Some(total))
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

    fn scan(path: &Path, max_scan: u64, chunk: u64) -> Option<f64> {
        last_cost_state(&File::open(path).unwrap(), max_scan, chunk)
    }

    #[test]
    fn without_a_cost_line_the_total_is_zero_and_without_a_transcript_unknown() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        assert_eq!(restored_total(&dir.0, session), None);
        assert_eq!(restored_total(&dir.0.join("missing"), session), None);
        let path = dir.transcript(session);
        assert_eq!(
            restored_total(&dir.0, session),
            None,
            "the CLI refuses to resume without a transcript"
        );
        write(&path, &[message(10), message(20)]);
        assert_eq!(restored_total(&dir.0, session), Some(0.0));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(restored_total(&dir.0, session), Some(0.0));
        std::fs::write(&path, b"\n\n").unwrap();
        assert_eq!(restored_total(&dir.0, session), Some(0.0));
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
        assert_eq!(restored_total(&dir.0, session), Some(0.75));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(cost_line(1.5).as_bytes())
            .unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            Some(1.5),
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
                Some(0.5),
                "chunk {chunk}"
            );
        }
        write(&path, &[cost_line(0.5)]);
        for chunk in [1, 7, 1_000] {
            assert_eq!(scan(&path, u64::MAX, chunk), Some(0.5));
        }
    }

    #[test]
    fn a_cost_line_too_far_from_the_end_is_unknown() {
        let dir = Dir::new();
        let path = dir.transcript(SessionId::new_v4());
        write(&path, &[cost_line(0.5), message(200)]);
        assert_eq!(scan(&path, 100, 16), None);
        assert_eq!(scan(&path, 10_000, 16), Some(0.5));
        write(&path, &[message(200)]);
        assert_eq!(
            scan(&path, 100, 16),
            None,
            "an unread start could still hold one"
        );
    }

    #[test]
    fn lines_of_other_types_are_passed_over_and_an_escaped_type_is_read() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        let path = dir.transcript(session);
        write(
            &path,
            &[
                cost_line(0.125),
                r#"{"type":"user","text":"cost-state","totalCostUSD":3}"#.to_owned(),
                r#"{"no":"type","totalCostUSD":3}"#.to_owned(),
                String::new(),
                "  ".to_owned(),
                message(MAX_COST_LINE_BYTES - 64),
            ],
        );
        assert_eq!(restored_total(&dir.0, session), Some(0.125));
        assert_eq!(scan(&path, u64::MAX, 1_000), Some(0.125));
        write(
            &path,
            &[
                cost_line(0.125),
                r#"{"type":"cost\u002dstate","totalCostUSD":2.5}"#.to_owned(),
            ],
        );
        assert_eq!(
            restored_total(&dir.0, session),
            Some(2.5),
            "the CLI parses the escape, so the runner does too"
        );
    }

    #[test]
    fn a_last_cost_line_the_runner_cant_take_makes_the_total_unknown() {
        let dir = Dir::new();
        let session = SessionId::new_v4();
        let path = dir.transcript(session);
        let oversized = format!(
            r#"{{"type":"cost-state","totalCostUSD":1e6,"pad":"{}"}}"#,
            "y".repeat(70 * 1024)
        );
        let read: Vec<_> = [
            oversized,
            r#"{"type":"cost-state","totalCostUSD":-1}"#.to_owned(),
            r#"{"type":"cost-state","totalCostUSD":1e17}"#.to_owned(),
            r#"{"type":"cost-state","totalCostUSD":"2"}"#.to_owned(),
        ]
        .into_iter()
        .map(|last| {
            write(&path, &[cost_line(0.125), last, message(5)]);
            restored_total(&dir.0, session)
        })
        .collect();
        assert_eq!(read, [None; 4], "oversized, negative, huge and a string");

        for last in [
            r#"{"type":"cost-state"}"#,
            r#"{"type":"cost-state","totalCostUSD":null}"#,
            r#"{"type":"user","type":"cost-state","totalCostUSD":1}"#,
            "cost-state, not JSON",
            "[1]",
        ] {
            write(&path, &[cost_line(0.125), last.to_owned(), message(5)]);
            assert_eq!(restored_total(&dir.0, session), None, "{last}");
        }
        write(&path, &[cost_line(0.125), message(MAX_COST_LINE_BYTES)]);
        assert_eq!(
            restored_total(&dir.0, session),
            None,
            "any line too long to read could be the CLI's"
        );
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
        assert_eq!(restored_total(&dir.0, session), None);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(dir.0.join("claude")).unwrap();
        std::os::unix::fs::symlink(elsewhere.0.join("claude"), dir.0.join("claude")).unwrap();
        assert_eq!(restored_total(&dir.0, session), None);

        std::fs::remove_file(dir.0.join("claude")).unwrap();
        let path = dir.transcript(session);
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            None,
            "a directory isn't a transcript"
        );
    }
}
