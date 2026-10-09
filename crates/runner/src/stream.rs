//! Reading a turn from `claude`'s stream-json output, and what a turn gives.

use std::fmt;
use std::time::Duration;

use core_types::SessionId;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::launch::TOOLS;

/// The longest stdout line read whole: 16 MiB. A longer line is skipped
/// and logged with its length. Result lines are far shorter: their text is
/// the final reply.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The longest code (a result's subtype or terminal reason, an assistant
/// line's error code) that is kept.
const MAX_CODE_LEN: usize = 64;

/// What the name of a tool the process wasn't given is kept as.
const OTHER_TOOL: &str = "<other>";

/// How one turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// The CLI printed its `result` line. The process is still running and
    /// takes the next turn, even when the result is an error.
    Finished(TurnResult),
    /// The process ended before the `result` line, or stopped reading its
    /// input. Start a new process with
    /// [`SessionStart::Resume`](crate::SessionStart::Resume) for the next
    /// turn, or with [`SessionStart::New`](crate::SessionStart::New) if the
    /// session had never started and [`TurnStats::init_seen`] is false. If
    /// [`ClaudeProcess::may_be_alive`](crate::ClaudeProcess::may_be_alive)
    /// is true, stop the container first.
    Crashed {
        /// The exit code, or 128 plus the signal, when it could be read.
        exit_code: Option<i32>,
        /// What the turn printed before it ended.
        stats: TurnStats,
    },
    /// The turn took longer than the configured timeout, and its process
    /// was killed. Resume as after a crash. The kill may not have taken
    /// (under Docker it can signal nothing): if
    /// [`ClaudeProcess::may_be_alive`](crate::ClaudeProcess::may_be_alive)
    /// is true, stop the container before starting another process in it.
    TimedOut {
        /// What the turn printed before it was killed.
        stats: TurnStats,
    },
}

impl TurnOutcome {
    /// The turn's structural metadata, however it ended.
    pub fn stats(&self) -> &TurnStats {
        match self {
            Self::Finished(result) => &result.stats,
            Self::Crashed { stats, .. } | Self::TimedOut { stats } => stats,
        }
    }

    /// Whether the turn finished with a result that isn't an error.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Finished(result) if !result.is_error)
    }

    /// Whether the turn ended as the CLI ends when it refuses a `--resume`
    /// because the session has no transcript: an error result with subtype
    /// `error_during_execution` before the turn's `system`/`init` line, so
    /// the CLI never read the message. The process exits after it.
    ///
    /// It is that refusal only on the first turn of a process started with
    /// [`SessionStart::Resume`](crate::SessionStart::Resume). Then the
    /// session never started: start the next process with
    /// [`SessionStart::New`](crate::SessionStart::New) under the same id,
    /// and run the turn again there.
    pub fn resume_refused(&self) -> bool {
        matches!(
            self,
            Self::Finished(result)
                if result.is_error
                    && !result.stats.init_seen
                    && result.subtype.as_deref() == Some("error_during_execution")
        )
    }
}

/// A turn's `result` line, and the turn's [`TurnStats`].
///
/// `Debug` shows the reply's length, never its text.
#[derive(Clone, PartialEq)]
pub struct TurnResult {
    /// Whether the turn failed. It decides failure, not
    /// [`subtype`](Self::subtype): an unreachable API gives `subtype:
    /// "success"` with `is_error: true`. A result line without the field
    /// counts as an error unless its subtype is `success`.
    pub is_error: bool,
    /// What kind of failure an error result is; `None` when
    /// [`is_error`](Self::is_error) is false.
    pub error_kind: Option<ErrorKind>,
    /// The line's `subtype`, such as `success` or
    /// `error_during_execution`.
    pub subtype: Option<String>,
    /// The reply text, or the CLI's error text for an error result. It may
    /// hold anything the model wrote, so it is delivered, never logged.
    pub result: Option<String>,
    /// Why the turn ended, such as `completed` or `api_error`.
    pub terminal_reason: Option<String>,
    /// The API's HTTP status for an API error, such as 429 or 401.
    pub api_error_status: Option<u16>,
    /// Token counts for the turn. The CLI reports them per turn.
    pub usage: Option<Usage>,
    /// The turn's cost in US dollars, as the CLI reckons it: the rise in
    /// [`process_total_cost_usd`](Self::process_total_cost_usd) since the
    /// process's previous result, never below 0. The first result of a
    /// process started with `--resume` rises from 0, so it holds the total
    /// the CLI restored as well as the turn's own cost.
    pub cost_usd: Option<f64>,
    /// The line's `total_cost_usd`: the CLI's running total for its
    /// process, not the turn's cost. A process started with `--session-id`
    /// counts from 0. One started with `--resume` counts from the total
    /// the CLI saved in the transcript when the session's last process
    /// exited: Claude Code 2.1.285 appends a `cost-state` line then, and
    /// none for a process that was killed.
    pub process_total_cost_usd: Option<f64>,
    /// The session the CLI reports.
    pub session_id: Option<SessionId>,
    /// Structural metadata from the lines before the result.
    pub stats: TurnStats,
}

impl fmt::Debug for TurnResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnResult")
            .field("is_error", &self.is_error)
            .field("error_kind", &self.error_kind)
            .field("subtype", &self.subtype)
            .field("result_len", &self.result.as_ref().map(String::len))
            .field("terminal_reason", &self.terminal_reason)
            .field("api_error_status", &self.api_error_status)
            .field("usage", &self.usage)
            .field("cost_usd", &self.cost_usd)
            .field("process_total_cost_usd", &self.process_total_cost_usd)
            .field("session_id", &self.session_id)
            .field("stats", &self.stats)
            .finish()
    }
}

/// A result's token counts. A missing count is 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens not read from or written to the cache.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation_input_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
}

/// Structural metadata about a turn, for diagnostics. It never holds a
/// message body, a tool input or tool output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnStats {
    /// Whether the CLI printed its `system`/`init` line for the turn, which
    /// it does once it has read the message. The session's transcript
    /// holds the message from then on, so the next process must resume.
    pub init_seen: bool,
    /// How many `assistant` lines the turn printed, synthetic error
    /// messages included.
    pub assistant_messages: u32,
    /// The names of the tools the turn called, in order, one per call. A
    /// tool the process wasn't given is kept as `<other>`, since the model
    /// writes the name.
    pub tool_calls: Vec<String>,
    /// The last `error` code on an `assistant` line: the CLI's kind of API
    /// error, such as `authentication_failed`, `rate_limit` or
    /// `server_error`.
    pub api_error: Option<String>,
    /// Lines of other types, skipped: `rate_limit_event`, `system` lines
    /// other than `init`, types this driver doesn't know, and JSON that
    /// isn't an object.
    pub ignored_lines: u32,
    /// Lines that weren't JSON, or longer than [`MAX_LINE_BYTES`], skipped.
    pub malformed_lines: u32,
    /// How long the turn took.
    pub duration: Duration,
}

/// What kind of failure an error result is. agentd tells the thread, and
/// the turn's requester, which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The credential's rate limit, usage limit or credit is used up.
    UsageLimit,
    /// The credential was refused: an expired login or a revoked key.
    Auth,
    /// Anything else.
    Other,
}

/// Lowercase phrases in an error result's text that mean a usage limit.
const USAGE_LIMIT_PHRASES: [&str; 8] = [
    "usage limit",
    "rate limit",
    "rate_limit",
    "hit your limit",
    "credit balance",
    "out of credits",
    "insufficient credit",
    "quota",
];

impl ErrorKind {
    /// Classifies an error result:
    ///
    /// 1. `api_error_status` 429 is [`UsageLimit`](Self::UsageLimit), and
    ///    401 or 403 is [`Auth`](Self::Auth).
    /// 2. Otherwise the CLI's error code from the turn's `assistant` line:
    ///    `rate_limit` and `billing_error` are `UsageLimit`, and
    ///    `authentication_failed` is `Auth`.
    /// 3. Otherwise text that says a rate limit, usage limit or credit is
    ///    used up (such as "usage limit reached" or "credit balance is too
    ///    low") is `UsageLimit`.
    /// 4. Anything else is [`Other`](Self::Other).
    pub fn classify(api_error_status: Option<u16>, api_error: Option<&str>, text: &str) -> Self {
        match (api_error_status, api_error) {
            (Some(429), _) => Self::UsageLimit,
            (Some(401 | 403), _) => Self::Auth,
            (_, Some("rate_limit" | "billing_error")) => Self::UsageLimit,
            (_, Some("authentication_failed")) => Self::Auth,
            _ => {
                let text = text.to_lowercase();
                if USAGE_LIMIT_PHRASES
                    .iter()
                    .any(|phrase| text.contains(phrase))
                {
                    Self::UsageLimit
                } else {
                    Self::Other
                }
            }
        }
    }
}

/// A `result` line's fields, before the turn's stats are attached.
///
/// `Debug` shows the reply's length, never its text.
#[derive(Clone, PartialEq)]
pub(crate) struct ResultLine {
    is_error: bool,
    subtype: Option<String>,
    result: Option<String>,
    terminal_reason: Option<String>,
    api_error_status: Option<u16>,
    usage: Option<Usage>,
    total_cost_usd: Option<f64>,
    session_id: Option<SessionId>,
}

impl fmt::Debug for ResultLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultLine")
            .field("is_error", &self.is_error)
            .field("subtype", &self.subtype)
            .field("result_len", &self.result.as_ref().map(String::len))
            .field("terminal_reason", &self.terminal_reason)
            .field("api_error_status", &self.api_error_status)
            .field("usage", &self.usage)
            .field("total_cost_usd", &self.total_cost_usd)
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl ResultLine {
    /// The turn's result. `process_total` is the running
    /// `total_cost_usd` of the process's previous results, 0 on a new
    /// process; the turn's cost is what this line adds to it, and it is
    /// moved on to this line's total.
    pub(crate) fn into_result(self, stats: TurnStats, process_total: &mut f64) -> TurnResult {
        let cost_usd = self.total_cost_usd.map(|total| {
            let cost = (total - *process_total).max(0.0);
            *process_total = total;
            cost
        });
        let error_kind = self.is_error.then(|| {
            ErrorKind::classify(
                self.api_error_status,
                stats.api_error.as_deref(),
                self.result.as_deref().unwrap_or_default(),
            )
        });
        TurnResult {
            is_error: self.is_error,
            error_kind,
            subtype: self.subtype,
            result: self.result,
            terminal_reason: self.terminal_reason,
            api_error_status: self.api_error_status,
            usage: self.usage,
            cost_usd,
            process_total_cost_usd: self.total_cost_usd,
            session_id: self.session_id,
            stats,
        }
    }
}

/// One of the CLI's codes from a line, such as `error_during_execution`,
/// kept only in the form the CLI's codes take: at most [`MAX_CODE_LEN`]
/// bytes of lowercase ASCII letters, digits and `_`. Anything else could be
/// text, and is dropped.
fn code(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let plain = !text.is_empty()
        && text.len() <= MAX_CODE_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    plain.then(|| text.to_owned())
}

/// A `tool_use` block's tool name, if it is one of the tools the process
/// was given, and [`OTHER_TOOL`] otherwise. The model writes the name, so
/// nothing else is kept.
fn tool_name(value: Option<&Value>) -> String {
    let name = value.and_then(Value::as_str).unwrap_or_default();
    TOOLS
        .split(',')
        .find(|tool| *tool == name)
        .unwrap_or(OTHER_TOOL)
        .to_owned()
}

fn parse_result(line: &Value) -> ResultLine {
    let subtype = code(line.get("subtype"));
    let is_error = line
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(subtype.as_deref() != Some("success"));
    let usage = line.get("usage").filter(|u| u.is_object()).map(|u| {
        let count = |key| u.get(key).and_then(Value::as_u64).unwrap_or(0);
        Usage {
            input_tokens: count("input_tokens"),
            output_tokens: count("output_tokens"),
            cache_creation_input_tokens: count("cache_creation_input_tokens"),
            cache_read_input_tokens: count("cache_read_input_tokens"),
        }
    });
    ResultLine {
        is_error,
        subtype,
        result: line
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned),
        terminal_reason: code(line.get("terminal_reason")),
        api_error_status: line
            .get("api_error_status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok()),
        usage,
        total_cost_usd: line.get("total_cost_usd").and_then(Value::as_f64),
        session_id: line
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse().ok()),
    }
}

/// Folds an `assistant` line into the stats: the count, the tool names of
/// its `tool_use` blocks and its error code. Nothing else is read.
fn note_assistant(line: &Value, stats: &mut TurnStats) {
    stats.assistant_messages = stats.assistant_messages.saturating_add(1);
    if let Some(error) = code(line.get("error")) {
        stats.api_error = Some(error);
    }
    let blocks = line
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array);
    for block in blocks.into_iter().flatten() {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            stats.tool_calls.push(tool_name(block.get("name")));
        }
    }
}

/// Parses one line, folding it into `stats`, and returns the result line
/// if it is one.
pub(crate) fn note_line(bytes: &[u8], stats: &mut TurnStats) -> Option<ResultLine> {
    let line: Value = match serde_json::from_slice(bytes) {
        Ok(line) => line,
        Err(error) => {
            stats.malformed_lines = stats.malformed_lines.saturating_add(1);
            tracing::warn!(len = bytes.len(), %error, "skipping a stdout line that isn't JSON");
            return None;
        }
    };
    let kind = line.get("type").and_then(Value::as_str);
    match kind {
        Some("result") => return Some(parse_result(&line)),
        Some("assistant") => note_assistant(&line, stats),
        Some("user") => {}
        Some("system") if line.get("subtype").and_then(Value::as_str) == Some("init") => {
            stats.init_seen = true;
        }
        _ => {
            stats.ignored_lines = stats.ignored_lines.saturating_add(1);
            tracing::debug!(len = bytes.len(), "skipping a stdout line of another type");
        }
    }
    None
}

/// One read from stdout.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RawLine {
    /// A line, in the buffer, without its newline. The last line may lack
    /// one.
    Line,
    /// A line longer than the cap, of this many bytes, which was skipped.
    TooLong(usize),
    /// The end of the stream.
    Eof,
}

/// Reads the next line into `buf`, keeping at most `max` bytes of it.
pub(crate) async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<RawLine> {
    buf.clear();
    let mut total = 0usize;
    let mut too_long = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if total == 0 {
                return Ok(RawLine::Eof);
            }
            break;
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        total = total.saturating_add(chunk.len());
        if !too_long {
            if total > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = newline.map_or(available.len(), |at| at + 1);
        reader.consume(used);
        if newline.is_some() {
            if total == 0 {
                continue;
            }
            break;
        }
    }
    Ok(if too_long {
        RawLine::TooLong(total)
    } else {
        RawLine::Line
    })
}

/// Reads lines until a `result` line, folding the others into `stats`.
/// Returns `None` at the end of the stream or on a read error.
pub(crate) async fn read_turn<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    stats: &mut TurnStats,
) -> Option<ResultLine> {
    loop {
        match read_line(reader, buf, MAX_LINE_BYTES).await {
            Ok(RawLine::Line) => {
                if let Some(result) = note_line(buf, stats) {
                    return Some(result);
                }
            }
            Ok(RawLine::TooLong(len)) => {
                stats.malformed_lines = stats.malformed_lines.saturating_add(1);
                tracing::warn!(
                    len,
                    max = MAX_LINE_BYTES,
                    "skipping an overlong stdout line"
                );
            }
            Ok(RawLine::Eof) => return None,
            Err(error) => {
                tracing::warn!(%error, "reading claude's stdout failed");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use testkit::fixtures;

    use super::*;

    async fn turns(capture: &str) -> (Vec<TurnResult>, TurnStats) {
        let mut reader = capture.as_bytes();
        let mut buf = Vec::new();
        let mut results = Vec::new();
        let mut process_total = 0.0;
        loop {
            let mut stats = TurnStats::default();
            match read_turn(&mut reader, &mut buf, &mut stats).await {
                Some(line) => results.push(line.into_result(stats, &mut process_total)),
                None => return (results, stats),
            }
        }
    }

    #[tokio::test]
    async fn the_captured_tool_turns_parse() {
        let (results, rest) = turns(fixtures::TOOL_TURNS).await;
        assert_eq!(rest, TurnStats::default());
        let [first, second] = results.as_slice() else {
            panic!("{results:?}");
        };
        let session: SessionId = "82ac83b6-83dc-4a7c-9561-6fdc5aed4da8".parse().unwrap();
        assert!(!first.is_error);
        assert_eq!(first.error_kind, None);
        assert_eq!(first.subtype.as_deref(), Some("success"));
        assert_eq!(
            first.result.as_deref(),
            Some("Hello from the capture server.")
        );
        assert_eq!(first.terminal_reason.as_deref(), Some("completed"));
        assert_eq!(first.api_error_status, None);
        assert_eq!(
            first.usage,
            Some(Usage {
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            })
        );
        assert!((first.cost_usd.unwrap() - 0.00014).abs() < 1e-12);
        assert_eq!(first.cost_usd, first.process_total_cost_usd);
        assert_eq!(first.session_id, Some(session));
        assert_eq!(
            first.stats,
            TurnStats {
                init_seen: true,
                assistant_messages: 2,
                tool_calls: vec!["Bash".into()],
                api_error: None,
                ignored_lines: 1,
                malformed_lines: 0,
                duration: Duration::ZERO,
            }
        );
        assert!(!second.is_error);
        assert_eq!(second.usage.unwrap().output_tokens, 5);
        assert!(
            (second.process_total_cost_usd.unwrap() - 0.00021).abs() < 1e-12,
            "the CLI reports its process's running total"
        );
        assert!(
            (second.cost_usd.unwrap() - 0.00007).abs() < 1e-12,
            "the second turn costs only what it added: {:?}",
            second.cost_usd
        );
        assert_eq!(second.stats.assistant_messages, 1);
        assert!(second.stats.tool_calls.is_empty());
        assert_eq!(second.stats.ignored_lines, 0);
        assert!(second.stats.init_seen);
    }

    #[tokio::test]
    async fn the_captured_errors_parse_and_classify() {
        let (results, _) = turns(fixtures::UNREACHABLE).await;
        let [unreachable] = results.as_slice() else {
            panic!("{results:?}");
        };
        assert!(unreachable.is_error);
        assert_eq!(unreachable.subtype.as_deref(), Some("success"));
        assert_eq!(unreachable.terminal_reason.as_deref(), Some("api_error"));
        assert_eq!(unreachable.api_error_status, None);
        assert_eq!(unreachable.stats.api_error.as_deref(), Some("server_error"));
        assert_eq!(unreachable.error_kind, Some(ErrorKind::Other));

        let (results, _) = turns(fixtures::AUTH_ERROR).await;
        let [auth] = results.as_slice() else {
            panic!("{results:?}");
        };
        assert!(auth.is_error);
        assert_eq!(auth.api_error_status, Some(401));
        assert_eq!(
            auth.stats.api_error.as_deref(),
            Some("authentication_failed")
        );
        assert_eq!(auth.error_kind, Some(ErrorKind::Auth));
        assert_eq!(auth.stats.assistant_messages, 1);

        let (results, _) = turns(fixtures::RESUME_MISSING).await;
        let [missing] = results.as_slice() else {
            panic!("{results:?}");
        };
        assert!(missing.is_error);
        assert_eq!(missing.subtype.as_deref(), Some("error_during_execution"));
        assert_eq!(missing.result, None);
        assert_eq!(missing.terminal_reason, None);
        assert_eq!(missing.api_error_status, None);
        assert!(!missing.stats.init_seen);
        assert_eq!(missing.error_kind, Some(ErrorKind::Other));
        assert!(TurnOutcome::Finished(missing.clone()).resume_refused());
        assert!(!TurnOutcome::Finished(auth.clone()).resume_refused());
        assert!(!TurnOutcome::Finished(unreachable.clone()).resume_refused());

        let (results, rest) = turns(fixtures::API_RETRY).await;
        assert!(results.is_empty());
        assert_eq!(rest.ignored_lines, 1);
    }

    #[tokio::test]
    async fn every_captured_line_is_json_the_driver_reads() {
        for (name, capture) in fixtures::ALL {
            let (_, rest) = turns(capture).await;
            assert_eq!(rest.malformed_lines, 0, "{name}");
        }
    }

    #[tokio::test]
    async fn only_the_captured_resume_refusal_is_one() {
        let (results, _) = turns(fixtures::TOOL_TURNS).await;
        for result in results {
            assert!(!TurnOutcome::Finished(result).resume_refused());
        }
        let mut stats = TurnStats::default();
        let line = br#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        let mut read = note_line(line, &mut stats)
            .unwrap()
            .into_result(stats, &mut 0.0);
        assert!(TurnOutcome::Finished(read.clone()).resume_refused());
        read.stats.init_seen = true;
        assert!(
            !TurnOutcome::Finished(read).resume_refused(),
            "an error after the CLI read the message is not a refusal"
        );
        let crashed = TurnOutcome::Crashed {
            exit_code: Some(1),
            stats: TurnStats::default(),
        };
        assert!(!crashed.resume_refused());
    }

    #[test]
    fn a_turn_costs_what_it_adds_to_the_process_total() {
        let result = |total: &str, process_total: &mut f64| {
            let line = format!(r#"{{"type":"result","subtype":"success"{total}}}"#);
            let mut stats = TurnStats::default();
            note_line(line.as_bytes(), &mut stats)
                .unwrap()
                .into_result(stats, process_total)
        };
        let mut process_total = 0.0;
        let first = result(r#","total_cost_usd":0.5"#, &mut process_total);
        assert_eq!(first.cost_usd, Some(0.5));
        assert_eq!(first.process_total_cost_usd, Some(0.5));
        let second = result(r#","total_cost_usd":0.75"#, &mut process_total);
        assert_eq!(second.cost_usd, Some(0.25));
        assert_eq!(second.process_total_cost_usd, Some(0.75));
        let none = result("", &mut process_total);
        assert_eq!(none.cost_usd, None);
        assert_eq!(
            process_total, 0.75,
            "a result without a total moves nothing"
        );
        let lower = result(r#","total_cost_usd":0.125"#, &mut process_total);
        assert_eq!(lower.cost_usd, Some(0.0), "a falling total costs nothing");
        assert_eq!(process_total, 0.125);
        let third = result(r#","total_cost_usd":0.25"#, &mut process_total);
        assert_eq!(third.cost_usd, Some(0.125));
    }

    #[test]
    fn classification_follows_status_then_code_then_text() {
        use ErrorKind::*;
        let cases = [
            (Some(429), None, "", UsageLimit),
            (Some(401), None, "", Auth),
            (Some(403), Some("rate_limit"), "usage limit", Auth),
            (Some(429), Some("authentication_failed"), "", UsageLimit),
            (None, Some("rate_limit"), "", UsageLimit),
            (None, Some("billing_error"), "", UsageLimit),
            (None, Some("authentication_failed"), "", Auth),
            (
                None,
                Some("server_error"),
                "Claude AI usage limit reached|1759000000",
                UsageLimit,
            ),
            (None, None, "Credit balance is too low", UsageLimit),
            (None, None, "You've hit your limit · resets 3pm", UsageLimit),
            (Some(500), None, "Internal server error", Other),
            (Some(529), Some("server_error"), "Overloaded", Other),
            (None, None, "", Other),
        ];
        for (status, code, text, expected) in cases {
            assert_eq!(
                ErrorKind::classify(status, code, text),
                expected,
                "{status:?} {code:?} {text:?}"
            );
        }
    }

    #[test]
    fn result_fields_are_read_leniently() {
        let mut stats = TurnStats::default();
        let line = br#"{"type":"result","subtype":"error_max_turns","api_error_status":"x","usage":{"input_tokens":"n","output_tokens":3},"total_cost_usd":"free","session_id":"not-a-uuid","terminal_reason":"has space","extra":{"nested":[1]}}"#;
        let result = note_line(line, &mut stats)
            .unwrap()
            .into_result(stats, &mut 0.0);
        assert!(result.is_error, "no is_error and not success");
        assert_eq!(result.subtype.as_deref(), Some("error_max_turns"));
        assert_eq!(result.api_error_status, None);
        assert_eq!(
            result.usage,
            Some(Usage {
                output_tokens: 3,
                ..Usage::default()
            })
        );
        assert_eq!(result.cost_usd, None);
        assert_eq!(result.process_total_cost_usd, None);
        assert_eq!(result.session_id, None);
        assert_eq!(result.terminal_reason, None);
        assert_eq!(result.error_kind, Some(ErrorKind::Other));

        let mut stats = TurnStats::default();
        let ok = note_line(br#"{"type":"result","subtype":"success"}"#, &mut stats).unwrap();
        let ok = ok.into_result(stats, &mut 0.0);
        assert!(!ok.is_error);
        assert_eq!(ok.error_kind, None);
        assert_eq!(ok.usage, None);

        let mut stats = TurnStats::default();
        let big = br#"{"type":"result","is_error":true,"api_error_status":70000}"#;
        assert_eq!(note_line(big, &mut stats).unwrap().api_error_status, None);
    }

    #[test]
    fn lines_are_sorted_into_the_stats() {
        let mut stats = TurnStats::default();
        let lines: [&[u8]; 11] = [
            br#"{"type":"system","subtype":"init","session_id":"x"}"#,
            br#"{"type":"system","subtype":"api_retry"}"#,
            br#"{"type":"active_goal","goal":"secret"}"#,
            br#"{"type":"rate_limit_event"}"#,
            br#"[1,2]"#,
            br#"{"no":"type"}"#,
            br#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"t"},{"type":"tool_use","name":"Read"},{"type":"tool_use","name":"mcp__a__b"},{"type":"tool_use","name":"bad name"},{"type":"tool_use"}]}}"#,
            br#"{"type":"assistant","error":"rate_limit","message":"not an object"}"#,
            b"not json",
            br#"{"type":"result""#,
        ];
        for line in lines {
            assert!(note_line(line, &mut stats).is_none());
        }
        assert_eq!(
            stats,
            TurnStats {
                init_seen: true,
                assistant_messages: 2,
                tool_calls: vec![
                    "Read".into(),
                    "<other>".into(),
                    "<other>".into(),
                    "<other>".into()
                ],
                api_error: Some("rate_limit".into()),
                ignored_lines: 5,
                malformed_lines: 2,
                duration: Duration::ZERO,
            }
        );
    }

    #[test]
    fn codes_are_short_plain_strings() {
        let value = |text: &str| Value::String(text.to_owned());
        assert_eq!(
            code(Some(&value("api_error"))).as_deref(),
            Some("api_error")
        );
        assert_eq!(code(Some(&value("error_2"))).as_deref(), Some("error_2"));
        assert_eq!(code(Some(&value("a.b-c:d"))), None);
        assert_eq!(code(Some(&value("Success"))), None);
        assert_eq!(code(Some(&value("sk-ant-api03-x"))), None);
        assert_eq!(code(Some(&value(""))), None);
        assert_eq!(code(Some(&value("with space"))), None);
        assert_eq!(code(Some(&value(&"a".repeat(65)))), None);
        assert_eq!(
            code(Some(&value(&"a".repeat(64)))).map(|c| c.len()),
            Some(64)
        );
        assert_eq!(code(Some(&Value::Bool(true))), None);
        assert_eq!(code(None), None);
    }

    #[test]
    fn only_the_given_tools_are_named() {
        let value = |text: &str| Value::String(text.to_owned());
        for tool in ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "Skill"] {
            assert_eq!(tool_name(Some(&value(tool))), tool);
        }
        for other in ["bash", "Task", "sk-ant-api03-secret", "", "Bash,Read"] {
            assert_eq!(tool_name(Some(&value(other))), "<other>", "{other}");
        }
        assert_eq!(tool_name(Some(&Value::Null)), "<other>");
        assert_eq!(tool_name(None), "<other>");
    }

    #[tokio::test]
    async fn read_line_caps_long_lines_and_skips_blank_ones() {
        let input = b"\n\nabc\n0123456789\nxyz\n\ntail".to_vec();
        let mut reader = tokio::io::BufReader::with_capacity(4, input.as_slice());
        let mut buf = Vec::new();
        let mut next = async || {
            let raw = read_line(&mut reader, &mut buf, 5).await.unwrap();
            (raw, String::from_utf8(buf.clone()).unwrap())
        };
        assert_eq!(next().await, (RawLine::Line, "abc".into()));
        assert_eq!(next().await, (RawLine::TooLong(10), String::new()));
        assert_eq!(next().await, (RawLine::Line, "xyz".into()));
        assert_eq!(next().await, (RawLine::Line, "tail".into()));
        assert_eq!(next().await, (RawLine::Eof, String::new()));
    }

    #[tokio::test]
    async fn an_overlong_line_is_skipped_and_the_turn_goes_on() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 1];
        input.extend_from_slice(b"\n{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\"}\n");
        let mut reader = input.as_slice();
        let mut stats = TurnStats::default();
        let result = read_turn(&mut reader, &mut Vec::new(), &mut stats)
            .await
            .unwrap()
            .into_result(stats, &mut 0.0);
        assert_eq!(result.result.as_deref(), Some("ok"));
        assert_eq!(result.stats.malformed_lines, 1);
    }

    #[tokio::test]
    async fn a_read_error_ends_the_turn() {
        struct Failing;
        impl tokio::io::AsyncRead for Failing {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("broken")))
            }
        }
        let mut reader = tokio::io::BufReader::new(Failing);
        let mut stats = TurnStats::default();
        assert!(
            read_turn(&mut reader, &mut Vec::new(), &mut stats)
                .await
                .is_none()
        );
    }

    #[test]
    fn a_result_line_debug_hides_the_reply_text() {
        let mut stats = TurnStats::default();
        let line = br#"{"type":"result","is_error":false,"result":"the password is hunter2"}"#;
        let result = note_line(line, &mut stats).unwrap();
        let debug = format!("{result:?} {result:#?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("result_len: Some(23)"), "{debug}");
    }

    #[test]
    fn debug_hides_the_reply_text() {
        let mut stats = TurnStats::default();
        let line = br#"{"type":"result","is_error":false,"result":"the password is hunter2"}"#;
        let result = note_line(line, &mut stats)
            .unwrap()
            .into_result(stats, &mut 0.0);
        let outcome = TurnOutcome::Finished(result);
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("result_len: Some(23)"), "{debug}");
        assert!(outcome.is_success());
        let crashed = TurnOutcome::Crashed {
            exit_code: Some(1),
            stats: TurnStats::default(),
        };
        assert!(!crashed.is_success());
        assert_eq!(crashed.stats(), &TurnStats::default());
        let timed_out = TurnOutcome::TimedOut {
            stats: TurnStats {
                init_seen: true,
                ..TurnStats::default()
            },
        };
        assert!(timed_out.stats().init_seen);
        assert!(!timed_out.is_success());
    }
}
