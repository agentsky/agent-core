//! The session's total cost as the CLI restores it on `--resume`.
//!
//! Claude Code appends
//! `{"type":"cost-state","sessionId":…,"totalCostUSD":…,…}` to a session's
//! transcript when a process exits, and none when one is killed. A
//! `--resume`d process starts its running `total_cost_usd` from the last
//! line of the transcript that is a `cost-state` line of the session and
//! passes the CLI's schema, skipping any other, so its first result
//! reports that total plus the turn's own cost. [`restored_total`] finds
//! the same line, so the runner can take it off.
//!
//! The transcript is in the session's directory, which the agent can
//! write. So nothing on the way to it is followed if it is a symlink, the
//! file must be a regular file, and at most [`MAX_SCAN_BYTES`] of it, read
//! from its end, are searched. Whatever the runner can't be sure the CLI
//! skips could be the line the CLI takes, so it makes the total unknown
//! rather than lead to an earlier line: a line longer than
//! [`MAX_COST_LINE_BYTES`], one that isn't a JSON object, one whose `type`
//! isn't a string, a `cost-state` line the runner can't be sure the CLI
//! takes (see [`cost_state`]), or a start of the file past the scan.

use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use core_types::SessionId;
use rustix::fs::{Mode, OFlags};
use serde_json::{Map, Value};

use crate::stream::MAX_PROCESS_TOTAL_USD;

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
    let id = session.to_string();
    match open_transcript(session_dir, &id) {
        Ok(file) => last_cost_state(&file, &id, MAX_SCAN_BYTES, CHUNK_BYTES),
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

/// The total of the last `cost-state` line in `file` the CLI restores for
/// the session `id`, searching back from its end, `chunk` bytes at a time,
/// at most `max_scan` bytes: 0 when the whole file has none, and `None`
/// when it isn't known.
fn last_cost_state(file: &File, id: &str, max_scan: u64, chunk: u64) -> Option<f64> {
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
            if let Some(total) = cost_state(&line, id)? {
                return Some(total);
            }
            line.clear();
            piece = earlier;
        }
        line = joined(piece, &line)?;
        end = start;
    }
    Some(cost_state(&line, id)?.unwrap_or(0.0))
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

/// The keys of a `cost-state` line, as Claude Code 2.1.285 writes one.
const COST_STATE_KEYS: [&str; 12] = [
    "type",
    "sessionId",
    "totalCostUSD",
    "totalAPIDuration",
    "totalAPIDurationWithoutRetries",
    "totalToolDuration",
    "totalLinesAdded",
    "totalLinesRemoved",
    "totalDuration",
    "startTime",
    "modelUsage",
    "hasUnknownModelCost",
];

/// The keys of a `cost-state` line whose value is an amount below
/// [`CLI_MAX_AMOUNT`].
const COST_STATE_AMOUNTS: [&str; 7] = [
    "totalAPIDuration",
    "totalAPIDurationWithoutRetries",
    "totalToolDuration",
    "totalLinesAdded",
    "totalLinesRemoved",
    "totalDuration",
    "startTime",
];

/// The keys of one model's entry in a `cost-state` line's `modelUsage`,
/// each an amount below [`CLI_MAX_AMOUNT`]; `thinkingTokens` may be left
/// out.
const MODEL_USAGE_KEYS: [&str; 7] = [
    "inputTokens",
    "outputTokens",
    "thinkingTokens",
    "cacheReadInputTokens",
    "cacheCreationInputTokens",
    "webSearchRequests",
    "costUSD",
];

/// The token counts whose sum over a `cost-state` line's models the CLI
/// bounds by [`CLI_MAX_AMOUNT`].
const MODEL_USAGE_SUMS: [&str; 4] = [
    "inputTokens",
    "outputTokens",
    "cacheReadInputTokens",
    "cacheCreationInputTokens",
];

/// The largest amount the CLI accepts in a `cost-state` line, besides
/// `totalCostUSD`, which is at most [`MAX_PROCESS_TOTAL_USD`].
const CLI_MAX_AMOUNT: f64 = 1e15;

/// What one line says: `Some(Some(total))` for a `cost-state` line the CLI
/// restores for `session`, `Some(None)` for a blank line or an object
/// whose `type` is another string or missing, which the CLI never takes
/// for one, and `None` for anything else.
///
/// The CLI parses each line with `JSON.parse`, so a key written twice
/// holds its last value, as in the [`Map`] here, and it restores a line
/// only if its `type` is `cost-state`, its `sessionId` is the session's
/// and it passes the CLI's schema, skipping it otherwise. The runner takes
/// a line only where it is sure the CLI does: every key one the CLI
/// writes, every amount a finite number from 0 to half the CLI's bound
/// (so the two parsers' rounding can't disagree across it), model names
/// printable ASCII. Every other line whose `type` is `cost-state`, or
/// isn't a string, and every line that isn't a JSON object, is unknown.
fn cost_state(line: &[u8], session: &str) -> Option<Option<f64>> {
    if line.trim_ascii().is_empty() {
        return Some(None);
    }
    let entry: Map<String, Value> = serde_json::from_slice(line).ok()?;
    match entry.get("type") {
        None => Some(None),
        Some(Value::String(kind)) if kind != "cost-state" => Some(None),
        Some(Value::String(_)) => restored(&entry, session).map(Some),
        Some(_) => None,
    }
}

/// The total of `entry`, a `cost-state` line, if the CLI restores it for
/// `session`; see [`cost_state`].
fn restored(entry: &Map<String, Value>, session: &str) -> Option<f64> {
    if entry.get("sessionId")?.as_str()? != session
        || !entry
            .keys()
            .all(|key| COST_STATE_KEYS.contains(&key.as_str()))
    {
        return None;
    }
    for key in COST_STATE_AMOUNTS {
        amount(entry.get(key), CLI_MAX_AMOUNT)?;
    }
    if let Some(flag) = entry.get("hasUnknownModelCost") {
        flag.as_bool()?;
    }
    model_usage(entry.get("modelUsage")?.as_object()?)?;
    amount(entry.get("totalCostUSD"), MAX_PROCESS_TOTAL_USD)
}

/// `Some(())` if `models`, a `cost-state` line's `modelUsage`, passes the
/// CLI's schema as [`cost_state`] requires.
fn model_usage(models: &Map<String, Value>) -> Option<()> {
    let mut sums = [0.0; MODEL_USAGE_SUMS.len()];
    for (name, usage) in models {
        let plain_name = !name.is_empty()
            && name != "__proto__"
            && name.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
        let usage = usage.as_object().filter(|_| plain_name)?;
        if !usage
            .keys()
            .all(|key| MODEL_USAGE_KEYS.contains(&key.as_str()))
        {
            return None;
        }
        for key in MODEL_USAGE_KEYS {
            let value = usage.get(key);
            if value.is_none() && key == "thinkingTokens" {
                continue;
            }
            let count = amount(value, CLI_MAX_AMOUNT)?;
            if let Some(sum) = MODEL_USAGE_SUMS.iter().position(|summed| *summed == key) {
                sums[sum] += count;
            }
        }
    }
    let in_bounds = sums.iter().all(|sum| *sum <= CLI_MAX_AMOUNT / 2.0);
    in_bounds.then_some(())
}

/// `value` if it is a finite number from 0 to half of `max`.
fn amount(value: Option<&Value>, max: f64) -> Option<f64> {
    value?
        .as_f64()
        .filter(|amount| amount.is_finite() && (0.0..=max / 2.0).contains(amount))
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

    const ID: &str = "3b0f5c2e-8d41-4a6b-9c1e-2f7a5d9e0b13";

    fn session() -> SessionId {
        ID.parse().unwrap()
    }

    fn cost_value(total: f64) -> Value {
        serde_json::json!({
            "type": "cost-state",
            "sessionId": ID,
            "totalCostUSD": total,
            "totalAPIDuration": 5_120,
            "totalAPIDurationWithoutRetries": 5_004,
            "totalToolDuration": 310,
            "totalLinesAdded": 12,
            "totalLinesRemoved": 3,
            "totalDuration": 9_870,
            "startTime": 1_790_000_000_000_u64,
            "modelUsage": {
                "claude-opus-4-5-20251101": {
                    "inputTokens": 10,
                    "outputTokens": 120,
                    "thinkingTokens": 0,
                    "cacheReadInputTokens": 18_000,
                    "cacheCreationInputTokens": 2_400,
                    "webSearchRequests": 0,
                    "costUSD": total,
                },
            },
            "hasUnknownModelCost": false,
        })
    }

    fn cost_line(total: f64) -> String {
        cost_value(total).to_string()
    }

    fn changed(change: impl FnOnce(&mut Map<String, Value>)) -> String {
        let mut line = cost_value(9.0);
        change(line.as_object_mut().unwrap());
        line.to_string()
    }

    fn model(line: &mut Map<String, Value>) -> &mut Map<String, Value> {
        line["modelUsage"]["claude-opus-4-5-20251101"]
            .as_object_mut()
            .unwrap()
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
        last_cost_state(&File::open(path).unwrap(), ID, max_scan, chunk)
    }

    #[test]
    fn without_a_cost_line_the_total_is_zero_and_without_a_transcript_unknown() {
        let dir = Dir::new();
        let session = session();
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
        let session = session();
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
        let path = dir.transcript(session());
        let lines = [message(3), cost_line(0.5), message(40), message(1)];
        write(&path, &lines);
        for chunk in 1..=40 {
            assert_eq!(scan(&path, u64::MAX, chunk), Some(0.5), "chunk {chunk}");
        }
        write(&path, &[cost_line(0.5)]);
        for chunk in [1, 7, 1_000] {
            assert_eq!(scan(&path, u64::MAX, chunk), Some(0.5));
        }
    }

    #[test]
    fn a_cost_line_too_far_from_the_end_is_unknown() {
        let dir = Dir::new();
        let path = dir.transcript(session());
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
        let session = session();
        let path = dir.transcript(session);
        write(
            &path,
            &[
                cost_line(0.125),
                r#"{"type":"user","text":"cost-state","totalCostUSD":3}"#.to_owned(),
                r#"{"no":"type","totalCostUSD":3}"#.to_owned(),
                changed(|line| {
                    line.remove("type");
                }),
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
                cost_line(2.5).replace(r#""cost-state""#, r#""cost\u002dstate""#),
            ],
        );
        assert_eq!(
            restored_total(&dir.0, session),
            Some(2.5),
            "the CLI parses the escape, so the runner does too"
        );
    }

    #[test]
    fn a_key_written_twice_holds_its_last_value_as_in_json_parse() {
        let dir = Dir::new();
        let session = session();
        let path = dir.transcript(session);
        let first = |key: &str| cost_line(3.0).replacen('{', &format!("{{{key},"), 1);
        let last = |key: &str| {
            let line = cost_line(3.0);
            format!("{},{key}}}", &line[..line.len() - 1])
        };
        let read: Vec<_> = [
            first(r#""type":"user""#),
            last(r#""type":"user""#),
            first(r#""totalCostUSD":"three""#),
            last(r#""totalCostUSD":0.5"#),
            last(r#""totalCostUSD":"three""#),
        ]
        .into_iter()
        .map(|last| {
            write(&path, &[cost_line(0.125), last]);
            restored_total(&dir.0, session)
        })
        .collect();
        assert_eq!(read, [Some(3.0), Some(0.125), Some(3.0), Some(0.5), None]);
    }

    #[test]
    fn only_a_line_the_cli_surely_restores_is_taken() {
        let dir = Dir::new();
        let session = session();
        let path = dir.transcript(session);
        for (last, total) in [
            (cost_line(0.0), 0.0),
            (
                changed(|line| {
                    line.remove("hasUnknownModelCost");
                    model(line).remove("thinkingTokens");
                }),
                9.0,
            ),
            (
                changed(|line| {
                    line["modelUsage"] = serde_json::json!({});
                }),
                9.0,
            ),
            (
                changed(|line| {
                    let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                    line["modelUsage"]["claude haiku 4.5"] = usage;
                }),
                9.0,
            ),
        ] {
            write(&path, &[cost_line(0.125), last.clone(), message(5)]);
            assert_eq!(restored_total(&dir.0, session), Some(total), "{last}");
        }
    }

    #[test]
    fn a_last_cost_line_the_runner_cant_take_makes_the_total_unknown() {
        let dir = Dir::new();
        let session = session();
        let path = dir.transcript(session);
        let oversized = changed(|line| {
            let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
            line["modelUsage"]["y".repeat(70 * 1024)] = usage;
        });
        let unsure = [
            oversized,
            changed(|line| line["totalCostUSD"] = (-1).into()),
            changed(|line| line["totalCostUSD"] = 1e17.into()),
            changed(|line| line["totalCostUSD"] = 6e8.into()),
            changed(|line| line["totalCostUSD"] = "2".into()),
            changed(|line| line["totalCostUSD"] = Value::Null),
            changed(|line| {
                line.remove("totalCostUSD");
            }),
            changed(|line| {
                line.remove("sessionId");
            }),
            changed(|line| line["sessionId"] = uuid::Uuid::new_v4().to_string().into()),
            changed(|line| line["sessionId"] = ID.to_uppercase().into()),
            changed(|line| {
                line.remove("startTime");
            }),
            changed(|line| line["startTime"] = 9e14.into()),
            changed(|line| line["totalDuration"] = (-0.5).into()),
            changed(|line| line["totalLinesAdded"] = "12".into()),
            changed(|line| line["hasUnknownModelCost"] = Value::Null),
            changed(|line| line["hasUnknownModelCost"] = 0.into()),
            changed(|line| {
                line.insert("extra".into(), true.into());
            }),
            changed(|line| line["modelUsage"] = Value::Array(Vec::new())),
            changed(|line| {
                line.remove("modelUsage");
            }),
            changed(|line| model(line)["thinkingTokens"] = Value::Null),
            changed(|line| {
                model(line).remove("costUSD");
            }),
            changed(|line| model(line)["cacheCreationInputTokens"] = 6e14.into()),
            changed(|line| {
                model(line).insert("extra".into(), 0.into());
            }),
            changed(|line| {
                let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                line["modelUsage"][""] = usage;
            }),
            changed(|line| {
                let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                line["modelUsage"]["claude\u{200b}opus"] = usage;
            }),
            changed(|line| {
                let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                line["modelUsage"]["__proto__"] = usage;
            }),
            changed(|line| {
                let mut usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                usage["outputTokens"] = 3e14.into();
                line["modelUsage"]["a"] = usage.clone();
                line["modelUsage"]["b"] = usage;
            }),
            changed(|line| line["type"] = serde_json::json!(["cost-state"])),
            changed(|line| line["type"] = Value::Null),
            changed(|line| line["type"] = 1.into()),
        ];
        for last in unsure {
            write(&path, &[cost_line(0.125), last.clone(), message(5)]);
            assert_eq!(restored_total(&dir.0, session), None, "{last}");
        }

        for last in [
            "cost-state, not JSON",
            "\0{}",
            "[1]",
            r#"["cost-state", 0]"#,
            r#"[{"type":"cost-state","totalCostUSD":0}]"#,
            r#""cost-state""#,
            "0",
            "true",
            "null",
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
        let session = session();
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
