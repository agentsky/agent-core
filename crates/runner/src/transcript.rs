//! The session's total cost as the CLI restores it on `--resume`.
//!
//! Claude Code appends
//! `{"type":"cost-state","sessionId":…,"totalCostUSD":…,…}` to a session's
//! transcript when a process exits, and none when one is killed. A
//! `--resume`d process starts its running `total_cost_usd` from a
//! `cost-state` line of the transcript, so its first result reports that
//! total plus the turn's own cost. [`restored_total`] reads the same total,
//! so the runner can take it off.
//!
//! The transcript is in the session's directory, which the agent can
//! write, and the CLI's loader has more paths than the runner can follow:
//! past [`CLI_INDEX_BYTES`] it files lines by their first bytes before it
//! parses them, and it restores the `cost-state` line of the session its
//! last message names, skipping any line that fails its schema. So the
//! runner doesn't mirror the loader. It reads a total only from a
//! transcript it is sure the loader reads one way, and otherwise the total
//! is unknown: the file is a regular file, reached without following a
//! symlink, of at most [`CLI_INDEX_BYTES`], ending its last line; every
//! line is blank or a JSON object with no key written twice; every line
//! with a `sessionId`, and every message, carries the session's own id;
//! and every `cost-state` line is one the CLI writes (see [`cost_state`]).
//! The total is then the last `cost-state` line's, or 0 without one.
//!
//! That is the total the CLI restores only if nothing changes the file
//! between the runner's read and the CLI's. So the session manager reads
//! it only for a process it starts in a container it has just started,
//! where no process of the agent's ran before the CLI; in a container an
//! earlier process ran in, one the agent left there could rewrite the
//! file, and the first turn's cost is unknown. With both, the unknown side
//! is the runner's: an agent can make its first turn's cost unknown, not
//! move it. A Docker test checks the pinned CLI restores whatever total
//! the runner reads, for transcripts the CLI wrote itself and for the ones
//! an agent could write to tell the two apart.

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use core_types::SessionId;
use rustix::fs::{Mode, OFlags};
use serde::de::{Deserialize, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use store::CostUnknown;

use crate::stream::MAX_PROCESS_TOTAL_USD;

/// The largest transcript the runner reads a total from: Claude Code
/// 2.1.285 loads a larger one with an index pass that files lines by their
/// first bytes (`{"type":"attribution-snapshot"`, say) before it parses
/// them, and compacts it, so a line's type there isn't what the runner
/// parses.
pub(crate) const CLI_INDEX_BYTES: u64 = 5 * 1024 * 1024;

/// The types of the lines the CLI loads as messages; a resumed session's
/// `cost-state` line is looked up by its last message's `sessionId`.
const MESSAGE_TYPES: [&str; 5] = ["user", "assistant", "system", "attachment", "progress"];

/// What the CLI will restore for `session`, as
/// [`ClaudeProcess::count_cost_from`](crate::ClaudeProcess::count_cost_from)
/// takes it: [`restored_total`], read off the async runtime.
pub(crate) async fn restored_cost(
    session_dir: &Path,
    session: SessionId,
) -> Result<f64, CostUnknown> {
    let dir = session_dir.to_owned();
    let restored = tokio::task::spawn_blocking(move || restored_total(&dir, session))
        .await
        .unwrap_or(Err(CostUnknown::TranscriptUnreadable));
    if let Err(reason) = restored {
        tracing::warn!(%session, %reason, "the total a resumed process restores isn't known; its first turn has no cost");
    }
    restored
}

/// What the CLI will restore, in US dollars, for `session`, whose
/// directory is `session_dir` (`sessions/<id>/` on its volume, as agentd
/// sees it): the last `cost-state` line's total, or 0 when the transcript
/// has none. Why not, when that isn't known: the transcript is missing
/// (the CLI then refuses the `--resume`) or can't be read, it is past
/// [`CLI_INDEX_BYTES`], or it isn't one the [module docs](self) say the
/// runner is sure of. The transcript is `claude/projects/<id>/<id>.jsonl`
/// there, since the runner names the project directory after the session.
pub(crate) fn restored_total(session_dir: &Path, session: SessionId) -> Result<f64, CostUnknown> {
    let id = session.to_string();
    let file = match open_transcript(session_dir, &id) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(%session, error = %err, "couldn't open the transcript to read its restored cost");
            return Err(CostUnknown::TranscriptUnreadable);
        }
    };
    let mut bytes = Vec::new();
    file.take(CLI_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CostUnknown::TranscriptUnreadable)?;
    if u64::try_from(bytes.len()).is_ok_and(|len| len > CLI_INDEX_BYTES) {
        return Err(CostUnknown::TranscriptTooLarge);
    }
    last_cost_state(&bytes, &id).ok_or(CostUnknown::TranscriptUnrecognized)
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

/// The total the CLI restores from `transcript`, the session `id`'s, of
/// at most [`CLI_INDEX_BYTES`], as the [module docs](self) give it: the
/// last `cost-state` line's, 0 without one, and `None` unless the runner
/// is sure.
fn last_cost_state(transcript: &[u8], id: &str) -> Option<f64> {
    if transcript.last().is_some_and(|last| *last != b'\n') {
        return None;
    }
    let mut total = 0.0;
    for line in transcript.split(|b| *b == b'\n') {
        if let Some(line_total) = line_total(line, id)? {
            total = line_total;
        }
    }
    Some(total)
}

/// What one line says: `Some(Some(total))` for a `cost-state` line the CLI
/// restores, `Some(None)` for a blank line or one of another type the
/// [module docs](self) allow, and `None` for any other.
fn line_total(line: &[u8], id: &str) -> Option<Option<f64>> {
    if line.trim_ascii().is_empty() {
        return Some(None);
    }
    let Strict(Value::Object(entry)) = serde_json::from_slice(line).ok()? else {
        return None;
    };
    match entry.get("sessionId") {
        Some(session) if session.as_str() != Some(id) => return None,
        None if entry.contains_key("uuid") || entry.contains_key("parentUuid") => return None,
        _ => {}
    }
    match entry.get("type") {
        None => Some(None),
        Some(Value::String(kind)) if kind == "cost-state" => cost_state(line, &entry, id).map(Some),
        Some(Value::String(kind))
            if MESSAGE_TYPES.contains(&kind.as_str()) && !entry.contains_key("sessionId") =>
        {
            None
        }
        Some(Value::String(_)) => Some(None),
        Some(_) => None,
    }
}

/// A JSON value in which no object has a key written twice. `JSON.parse`
/// keeps a repeated key's last value where a line's first bytes can say
/// otherwise, so such a line is one the runner can't be sure of.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON with no key written twice")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(Strict(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some((key, Strict(value))) = map.next_entry::<String, Strict>()? {
            if object.insert(key, value).is_some() {
                return Err(A::Error::custom("a key written twice"));
            }
        }
        Ok(Value::Object(object))
    }
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

/// The total of `entry`, the parsed `line`, a `cost-state` line of the
/// session `id`, if the runner is sure the CLI restores it: it starts with
/// the bytes the CLI writes, `{"type":"cost-state","sessionId":"<id>",`,
/// holds only the keys the CLI writes, and every amount is a finite
/// number from 0 to half the CLI's bound (so the two parsers' rounding
/// can't fall on different sides of it), with model names printable ASCII
/// and the token sums in bounds, as its zod schema requires.
fn cost_state(line: &[u8], entry: &Map<String, Value>, id: &str) -> Option<f64> {
    let prefix = format!(r#"{{"type":"cost-state","sessionId":"{id}","#);
    if !line.starts_with(prefix.as_bytes())
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
/// CLI's schema as [`cost_state`] requires it to.
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
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use super::*;
    use crate::test_util::TempDir;

    const ID: &str = "3b0f5c2e-8d41-4a6b-9c1e-2f7a5d9e0b13";
    const OTHER: &str = "99999999-9999-4999-8999-999999999999";
    const U1: &str = "11111111-1111-4111-8111-111111111111";
    const A1: &str = "22222222-2222-4222-8222-222222222222";

    /// The path of `session`'s transcript under `dir`, with its directory
    /// made.
    fn transcript_in(dir: &TempDir, session: SessionId) -> PathBuf {
        let id = session.to_string();
        let dir = dir.0.join("claude").join("projects").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{id}.jsonl"))
    }

    fn session() -> SessionId {
        ID.parse().unwrap()
    }

    fn cost_value(total: f64, session: &str) -> Map<String, Value> {
        let Value::Object(line) = serde_json::json!({
            "type": "cost-state",
            "sessionId": session,
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
        }) else {
            unreachable!()
        };
        line
    }

    /// `line` as the CLI writes it: `type` first, then `sessionId`, then
    /// the rest, compact.
    fn written(mut line: Map<String, Value>) -> String {
        match (line.remove("type"), line.remove("sessionId")) {
            (Some(kind), Some(session)) => {
                let rest = Value::Object(line).to_string();
                let rest = &rest[1..rest.len() - 1];
                let comma = if rest.is_empty() { "" } else { "," };
                format!(r#"{{"type":{kind},"sessionId":{session}{comma}{rest}}}"#)
            }
            (kind, session) => {
                line.extend(kind.map(|kind| ("type".to_owned(), kind)));
                line.extend(session.map(|session| ("sessionId".to_owned(), session)));
                Value::Object(line).to_string()
            }
        }
    }

    fn cost_line(total: f64) -> String {
        written(cost_value(total, ID))
    }

    fn changed(change: impl FnOnce(&mut Map<String, Value>)) -> String {
        let mut line = cost_value(9.0, ID);
        change(&mut line);
        written(line)
    }

    fn model(line: &mut Map<String, Value>) -> &mut Map<String, Value> {
        line["modelUsage"]["claude-opus-4-5-20251101"]
            .as_object_mut()
            .unwrap()
    }

    fn user(uuid: &str, parent: Option<&str>, session: &str, at: &str) -> String {
        serde_json::json!({
            "parentUuid": parent,
            "isSidechain": false,
            "userType": "external",
            "cwd": "/volume/work",
            "sessionId": session,
            "version": "2.1.285",
            "type": "user",
            "message": {"role": "user", "content": "hello"},
            "uuid": uuid,
            "timestamp": at,
        })
        .to_string()
    }

    fn assistant(uuid: &str, parent: &str, session: &str) -> String {
        serde_json::json!({
            "parentUuid": parent,
            "isSidechain": false,
            "userType": "external",
            "cwd": "/volume/work",
            "sessionId": session,
            "version": "2.1.285",
            "type": "assistant",
            "message": {
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-5-20251101",
                "content": [{"type": "text", "text": "hi there"}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 10, "output_tokens": 5},
            },
            "requestId": "req_1",
            "uuid": uuid,
            "timestamp": "2026-09-30T10:00:01.000Z",
        })
        .to_string()
    }

    /// A user message and its reply in the session `session`.
    fn exchange(session: &str) -> Vec<String> {
        vec![
            user(U1, None, session, "2026-09-30T10:00:00.000Z"),
            assistant(A1, U1, session),
        ]
    }

    /// A line of `len` bytes, or the shortest there is, of padding the CLI
    /// keeps but never reads a cost from.
    fn note(len: usize) -> String {
        let empty = r#"{"type":"x-note","pad":""}"#;
        format!(
            r#"{{"type":"x-note","pad":"{}"}}"#,
            "p".repeat(len.saturating_sub(empty.len()))
        )
    }

    fn write(path: &Path, lines: &[String]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn restored(lines: &[String]) -> Option<f64> {
        restored_or_why(lines).ok()
    }

    fn restored_or_why(lines: &[String]) -> Result<f64, CostUnknown> {
        let dir = TempDir::new();
        write(&transcript_in(&dir, session()), lines);
        restored_total(&dir.0, session())
    }

    /// `lines` padded with [`note`]s after the first, so the file is `len`
    /// bytes long.
    fn padded_to(len: u64, mut lines: Vec<String>) -> Vec<String> {
        let mut left = len - lines.iter().map(|line| line.len() as u64 + 1).sum::<u64>();
        let mut pad = Vec::new();
        while left > 0 {
            let piece = match left {
                0..=60_000 => left,
                60_001..60_100 => 30_000,
                _ => 60_000,
            };
            let line = note(usize::try_from(piece - 1).unwrap());
            assert_eq!(line.len() as u64 + 1, piece);
            left -= piece;
            pad.push(line);
        }
        lines.splice(1..1, pad);
        lines
    }

    #[test]
    fn without_a_cost_line_the_total_is_zero_and_without_a_transcript_unknown() {
        let dir = TempDir::new();
        let session = session();
        let unreadable = Err(CostUnknown::TranscriptUnreadable);
        assert_eq!(restored_total(&dir.0, session), unreadable);
        assert_eq!(restored_total(&dir.0.join("missing"), session), unreadable);
        let path = transcript_in(&dir, session);
        assert_eq!(
            restored_total(&dir.0, session),
            unreadable,
            "the CLI refuses to resume without a transcript"
        );
        write(&path, &exchange(ID));
        assert_eq!(restored_total(&dir.0, session), Ok(0.0));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(restored_total(&dir.0, session), Ok(0.0));
        std::fs::write(&path, b"\n\n").unwrap();
        assert_eq!(restored_total(&dir.0, session), Ok(0.0));
    }

    #[test]
    fn the_last_cost_line_wins_even_with_other_lines_after_it() {
        let dir = TempDir::new();
        let session = session();
        let path = transcript_in(&dir, session);
        let mut lines = exchange(ID);
        lines.extend([
            cost_line(0.25),
            note(40),
            cost_line(0.75),
            note(5),
            user(
                "33333333-3333-4333-8333-333333333333",
                Some(A1),
                ID,
                "2026-09-30T10:05:00.000Z",
            ),
        ]);
        write(&path, &lines);
        assert_eq!(restored_total(&dir.0, session), Ok(0.75));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(cost_line(1.5).as_bytes())
            .unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            Err(CostUnknown::TranscriptUnrecognized),
            "the CLI ends every line it writes, so a last line without its newline is unknown"
        );
    }

    #[test]
    fn the_total_of_a_tool_turn_the_cli_wrote_is_read() {
        let lines: Vec<String> = testkit::fixtures::TOOL_TURN_TRANSCRIPT
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(restored(&lines), Some(0.0112));
        let mut appended = lines.clone();
        appended.push(written(cost_value(900.0, OTHER)));
        assert_eq!(
            restored_or_why(&appended),
            Err(CostUnknown::TranscriptUnrecognized)
        );
    }

    #[test]
    fn a_transcript_past_the_clis_index_threshold_is_unknown() {
        let mut lines = exchange(ID);
        lines.push(cost_line(0.5));
        assert_eq!(
            restored(&padded_to(CLI_INDEX_BYTES, lines.clone())),
            Some(0.5)
        );
        assert_eq!(
            restored_or_why(&padded_to(CLI_INDEX_BYTES + 1, lines)),
            Err(CostUnknown::TranscriptTooLarge),
            "past 5 MiB the CLI files lines by their first bytes before it parses them"
        );
    }

    #[test]
    fn lines_of_other_types_are_passed_over() {
        let mut lines = exchange(ID);
        lines.extend([
            cost_line(0.125),
            r#"{"type":"summary","summary":"cost-state","leafUuid":"22222222-2222-4222-8222-222222222222"}"#
                .to_owned(),
            r#"{"type":"file-history-snapshot","totalCostUSD":3}"#.to_owned(),
            r#"{"no":"type","totalCostUSD":3}"#.to_owned(),
            format!(r#"{{"type":"last-prompt","sessionId":"{ID}","lastPrompt":"x"}}"#),
            String::new(),
            "  ".to_owned(),
            note(200_000),
        ]);
        assert_eq!(restored(&lines), Some(0.125));
    }

    #[test]
    fn a_key_written_twice_anywhere_is_unknown() {
        let first = |key: &str| cost_line(3.0).replacen('{', &format!("{{{key},"), 1);
        let last = |key: &str| {
            let line = cost_line(3.0);
            format!("{},{key}}}", &line[..line.len() - 1])
        };
        let twice = [
            first(r#""type":"user""#),
            last(r#""type":"user""#),
            last(r#""totalCostUSD":0.5"#),
            cost_line(3.0).replacen(
                r#""inputTokens":10"#,
                r#""inputTokens":10,"inputTokens":11"#,
                1,
            ),
            user(U1, None, ID, "2026-09-30T10:05:00.000Z").replacen(
                '{',
                &format!(r#"{{"sessionId":"{OTHER}","#),
                1,
            ),
        ];
        for line in twice {
            let mut lines = exchange(ID);
            lines.extend([cost_line(0.125), line.clone()]);
            assert_eq!(restored(&lines), None, "{line}");
        }
    }

    #[test]
    fn only_a_line_the_cli_surely_restores_is_taken() {
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
            (
                changed(|line| {
                    let usage = line["modelUsage"]["claude-opus-4-5-20251101"].clone();
                    line["modelUsage"]["y".repeat(70 * 1024)] = usage;
                }),
                9.0,
            ),
        ] {
            let mut lines = exchange(ID);
            lines.extend([cost_line(0.125), last.clone(), note(5)]);
            assert_eq!(restored(&lines), Some(total), "{last}");
        }
    }

    #[test]
    fn a_cost_line_the_runner_cant_take_makes_the_total_unknown() {
        let cost = cost_line(9.0);
        let unsure = [
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
            Value::Object(cost_value(9.0, ID)).to_string(),
            cost.replacen(r#""type":"cost-state""#, r#""type": "cost-state""#, 1),
            cost.replacen(r#""cost-state""#, r#""cost\u002dstate""#, 1),
            format!(r#"{{"type":"artifact-autoreact-ledger",{}"#, &cost[1..]),
            format!(r#"{{"type":"attribution-snapshot",{}"#, &cost[1..]),
            format!(" {cost}"),
            format!("\0{cost}"),
        ];
        for last in unsure {
            let mut lines = exchange(ID);
            lines.extend([cost_line(0.125), last.clone(), note(5)]);
            assert_eq!(restored(&lines), None, "{last}");
        }

        for last in [
            "cost-state, not JSON",
            "[1]",
            r#"["cost-state", 0]"#,
            r#"[{"type":"cost-state","totalCostUSD":0}]"#,
            r#""cost-state""#,
            "0",
            "true",
            "null",
        ] {
            let mut lines = exchange(ID);
            lines.extend([cost_line(0.125), last.to_owned(), note(5)]);
            assert_eq!(restored(&lines), None, "{last}");
        }
    }

    #[test]
    fn a_line_of_another_session_or_a_message_without_one_is_unknown() {
        let leaf = user(
            "33333333-3333-4333-8333-333333333333",
            Some(A1),
            OTHER,
            "2099-01-01T00:00:00.000Z",
        );
        let mut leaf2 = exchange(ID);
        leaf2.extend([written(cost_value(7.0, OTHER)), leaf, cost_line(5.0)]);
        let mut leaf3 = exchange(OTHER);
        leaf3.extend([written(cost_value(7.0, OTHER)), cost_line(5.0)]);
        let mut unsessioned = exchange(ID);
        unsessioned.extend([
            cost_line(5.0),
            r#"{"parentUuid":"22222222-2222-4222-8222-222222222222","type":"user","message":{"role":"user","content":"x"},"uuid":"44444444-4444-4444-8444-444444444444"}"#.to_owned(),
        ]);
        let mut untyped = exchange(ID);
        untyped.extend([
            cost_line(5.0),
            r#"{"uuid":"44444444-4444-4444-8444-444444444444","parentUuid":null}"#.to_owned(),
        ]);
        let mut numbered = exchange(ID);
        numbered.extend([
            cost_line(5.0),
            r#"{"type":"queue-operation","sessionId":7}"#.to_owned(),
        ]);
        for (case, lines) in [
            ("leaf2", leaf2),
            ("leaf3", leaf3),
            ("unsessioned", unsessioned),
            ("untyped", untyped),
            ("numbered", numbered),
        ] {
            assert_eq!(restored(&lines), None, "{case}");
        }
    }

    #[test]
    fn nothing_is_followed_through_a_symlink_and_only_a_file_is_read() {
        let dir = TempDir::new();
        let session = session();
        let elsewhere = TempDir::new();
        let target = transcript_in(&elsewhere, session);
        write(&target, &[cost_line(4.0)]);

        let path = transcript_in(&dir, session);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let unreadable = Err(CostUnknown::TranscriptUnreadable);
        assert_eq!(restored_total(&dir.0, session), unreadable);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(dir.0.join("claude")).unwrap();
        std::os::unix::fs::symlink(elsewhere.0.join("claude"), dir.0.join("claude")).unwrap();
        assert_eq!(restored_total(&dir.0, session), unreadable);

        std::fs::remove_file(dir.0.join("claude")).unwrap();
        let path = transcript_in(&dir, session);
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            restored_total(&dir.0, session),
            unreadable,
            "a directory isn't a transcript"
        );
    }

    /// The transcripts the Docker test gives the pinned CLI and the runner:
    /// what a session's CLI writes, and what an agent could append to make
    /// the two read different totals.
    fn cli_cases() -> Vec<(&'static str, Vec<String>)> {
        let with = |mut lines: Vec<String>, more: Vec<String>| {
            lines.extend(more);
            lines
        };
        let cost = cost_line(0.25);
        let ledger = format!(r#"{{"type":"artifact-autoreact-ledger",{}"#, &cost[1..]);
        let attribution = format!(r#"{{"type":"attribution-snapshot",{}"#, &cost[1..]);
        let leaf = user(
            "33333333-3333-4333-8333-333333333333",
            Some(A1),
            OTHER,
            "2099-01-01T00:00:00.000Z",
        );
        let big = |line: String| {
            padded_to(
                CLI_INDEX_BYTES + 200_000,
                with(exchange(ID), vec![cost_line(5.0), line]),
            )
        };
        let last = cost_line(5.0);
        vec![
            ("no cost", exchange(ID)),
            ("baseline", with(exchange(ID), vec![cost_line(5.0)])),
            (
                "two costs",
                with(
                    exchange(ID),
                    vec![cost_line(2.0), note(100), cost_line(5.0)],
                ),
            ),
            (
                "just under the threshold",
                padded_to(CLI_INDEX_BYTES, with(exchange(ID), vec![cost_line(5.0)])),
            ),
            (
                "ledger",
                with(exchange(ID), vec![cost_line(5.0), ledger.clone()]),
            ),
            ("ledger_big", big(ledger)),
            ("attr_big", big(attribution)),
            (
                "leaf2",
                with(
                    exchange(ID),
                    vec![written(cost_value(7.0, OTHER)), leaf, cost_line(5.0)],
                ),
            ),
            (
                "leaf3",
                with(
                    exchange(OTHER),
                    vec![written(cost_value(7.0, OTHER)), cost_line(5.0)],
                ),
            ),
            (
                "duplicates",
                with(
                    exchange(ID),
                    vec![
                        cost_line(5.0),
                        format!(r#"{},"totalCostUSD":0.5}}"#, &last[..last.len() - 1]),
                    ],
                ),
            ),
        ]
    }

    /// How the stub Messages API answers.
    #[derive(Clone, Copy)]
    enum Api {
        /// A 400 for every request, so a resumed CLI ends its turn at once,
        /// its result's total the one it restored.
        Refuse,
        /// A `Write` call for the file `note.txt` while the request offers
        /// the tool and holds no tool result yet, and a text reply that
        /// ends the turn otherwise: a real turn with a tool, as the CLI
        /// records one.
        WriteANote,
    }

    /// Serves `api` on a free local port, and returns the port.
    fn stub_api(api: Api) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || answer(stream, api));
            }
        });
        port
    }

    fn answer(mut stream: std::net::TcpStream, api: Api) {
        let mut head = Vec::new();
        let mut byte = [0; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|len| len.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        let _ = stream.read_exact(&mut body);
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let (status, kind, body) = match api {
            _ if !head.starts_with("post /v1/messages") => {
                ("404 Not Found", "application/json", "{}".to_owned())
            }
            Api::Refuse => (
                "400 Bad Request",
                "application/json",
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"refused"}}"#
                    .to_owned(),
            ),
            Api::WriteANote => ("200 OK", "text/event-stream", write_a_note(&request)),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    /// The SSE stream [`Api::WriteANote`] answers `request` with.
    fn write_a_note(request: &Value) -> String {
        let offers_write = request["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "Write"));
        let has_result = request.to_string().contains(r#""type":"tool_result""#);
        let model = request["model"].as_str().unwrap_or("claude-sonnet-4-5");
        let (block, delta, stop) = if offers_write && !has_result {
            let input = serde_json::json!({
                "file_path": "/volume/s/work/note.txt",
                "content": "a note\n",
            });
            (
                serde_json::json!({"type": "tool_use", "id": "toolu_01note", "name": "Write", "input": {}}),
                serde_json::json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                "tool_use",
            )
        } else {
            (
                serde_json::json!({"type": "text", "text": ""}),
                serde_json::json!({"type": "text_delta", "text": "Done."}),
                "end_turn",
            )
        };
        let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        [
            serde_json::json!({"type": "message_start", "message": {
                "id": id, "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 1_200, "output_tokens": 1,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            }}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": block}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": delta}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": null},
                "usage": {"output_tokens": 40}}),
            serde_json::json!({"type": "message_stop"}),
        ]
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
    }

    /// The user the container runs the CLI as: the session directory's
    /// owner, as agentd's Docker sandbox runs it when agentd isn't root, so
    /// the runner can read what the CLI writes; `None` for the image's own
    /// user, whom a root runner opens the directory to.
    fn container_user(dir: &Path) -> Option<String> {
        use std::os::unix::fs::MetadataExt as _;
        let owner = std::fs::metadata(dir).unwrap();
        (owner.uid() != 0).then(|| format!("{}:{}", owner.uid(), owner.gid()))
    }

    fn open_to_all(path: &Path) {
        let mode = if path.is_dir() { 0o777 } else { 0o666 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                open_to_all(&entry.unwrap().path());
            }
        }
    }

    /// Runs the pinned CLI in `image` with `args`, on the session in
    /// `dir` laid out as the runner lays it out, against the API stub on
    /// `port`, and returns its result's `total_cost_usd`.
    fn cli_total(image: &str, dir: &Path, port: u16, args: &[&str]) -> Option<f64> {
        for sub in ["home", "work"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let user = container_user(dir);
        if user.is_none() {
            open_to_all(dir);
        }
        let output = std::process::Command::new("timeout")
            .arg("180")
            .args(["docker", "run", "--rm", "--network", "host", "-i"])
            .args(user.iter().flat_map(|user| ["--user", user.as_str()]))
            .arg("-v")
            .arg(format!("{}:/volume/s", dir.display()))
            .args(["-w", "/volume/s/work"])
            .args(["-e", "HOME=/volume/s/home"])
            .args(["-e", "CLAUDE_CONFIG_DIR=/volume/s/claude"])
            .args(["-e", &format!("CLAUDE_CODE_PROJECT_DIR_NAME={ID}")])
            .args(["-e", "ANTHROPIC_API_KEY=sk-ant-api03-placeholder"])
            .args(["-e", &format!("ANTHROPIC_BASE_URL=http://127.0.0.1:{port}")])
            .args(["-e", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"])
            .args(["-e", "DISABLE_AUTOUPDATER=1"])
            .arg(image)
            .arg("claude")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result: Value = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .unwrap_or(Value::Null);
        let total = result["total_cost_usd"].as_f64();
        if total.is_none() {
            eprintln!(
                "the CLI gave no total: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        total
    }

    /// The total the pinned CLI in `image` restores when it resumes the
    /// session in `dir`, against the refusing stub on `port`.
    fn cli_restored(image: &str, dir: &Path, port: u16) -> Option<f64> {
        let args = ["-p", "hi", "--output-format", "json", "--resume", ID];
        cli_total(image, dir, port, &args)
    }

    /// A session whose one turn the pinned CLI in `image` ran itself,
    /// writing a file with a tool against the stub on `port`, and the total
    /// it reported.
    fn cli_tool_turn(image: &str, dir: &Path, port: u16) -> Option<f64> {
        let args = [
            "-p",
            "Write a note.",
            "--output-format",
            "json",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            ID,
        ];
        cli_total(image, dir, port, &args)
    }

    #[test]
    #[ignore = "needs docker and the sandbox image"]
    fn docker_the_pinned_cli_restores_whatever_total_the_runner_reads() {
        let image = std::env::var("AGENT_CORE_SANDBOX_IMAGE")
            .unwrap_or_else(|_| "agent-core/sandbox:dev".to_owned());
        let port = stub_api(Api::Refuse);
        let mut read = Vec::new();

        let tool_turn = TempDir::new();
        let reported = cli_tool_turn(&image, &tool_turn.0, stub_api(Api::WriteANote));
        let transcript = std::fs::read_to_string(transcript_in(&tool_turn, session())).unwrap();
        eprintln!("the CLI's own tool turn, reporting {reported:?}:\n{transcript}");
        assert!(
            transcript.contains(r#""name":"Write""#) && transcript.contains("tool_result"),
            "the CLI ran the tool"
        );
        assert!(reported.is_some_and(|total| total > 0.0));
        let runner = restored_total(&tool_turn.0, session()).ok();
        let cli = cli_restored(&image, &tool_turn.0, port);
        eprintln!("tool turn: the runner reads {runner:?}, the CLI restored {cli:?}");
        assert_eq!(runner, reported, "the runner reads the CLI's own tool turn");
        read.push(("tool turn", runner, cli));

        for (case, lines) in cli_cases() {
            let dir = TempDir::new();
            write(&transcript_in(&dir, session()), &lines);
            let runner = restored_total(&dir.0, session()).ok();
            let cli = cli_restored(&image, &dir.0, port);
            eprintln!("{case}: the runner reads {runner:?}, the CLI restored {cli:?}");
            read.push((case, runner, cli));
        }
        for (case, runner, cli) in &read {
            if let Some(total) = runner {
                assert_eq!(
                    cli.map(|cli| (cli - total).abs() < 1e-9),
                    Some(true),
                    "{case}: the runner reads {total}, the CLI restored {cli:?}"
                );
            }
        }
        let taken: Vec<_> = read
            .iter()
            .filter(|(_, runner, _)| runner.is_some())
            .map(|(case, _, _)| *case)
            .collect();
        assert_eq!(
            taken,
            [
                "tool turn",
                "no cost",
                "baseline",
                "two costs",
                "just under the threshold"
            ],
            "the runner reads the CLI's own transcripts, and nothing an agent appended"
        );
    }
}
