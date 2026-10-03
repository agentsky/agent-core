//! The `fake-claude` binary's script format, and [`fake_claude_path`] to
//! find the binary.
//!
//! `fake-claude` stands in for the Claude Code CLI in tests. It takes the
//! design's launch flags and speaks stream-json like the real CLI 2.1.285:
//!
//! ```text
//! fake-claude -p --input-format stream-json --output-format stream-json --verbose \
//!   --tools "Bash,Read,Edit,Write,Glob,Grep,Skill" --strict-mcp-config \
//!   --setting-sources user --permission-mode bypassPermissions \
//!   --append-system-prompt-file /agent/persona.md [--model <model>] \
//!   --session-id <uuid> | --resume <uuid>
//! ```
//!
//! # Checks
//!
//! - An unknown flag, a bad value, or both or neither of `--session-id` and
//!   `--resume` exits with status 2. So does a missing `-p`, `--verbose`
//!   or stream-json format, which the real CLI needs in this mode too.
//!   `--append-system-prompt-file` must name a readable file.
//! - The transcript is
//!   `$CLAUDE_CONFIG_DIR/projects/$CLAUDE_CODE_PROJECT_DIR_NAME/<id>.jsonl`.
//!   Both variables must be set. With `--session-id`, an existing
//!   transcript exits with status 1 ("Session ID … is already in use"). With
//!   `--resume`, a missing one prints an `error_during_execution` result
//!   and exits with status 1. Unlike the real CLI, `--resume` doesn't
//!   search other project directories. Like the real CLI, the transcript is
//!   created by the first user message, not at start.
//! - `ANTHROPIC_BASE_URL` must be set. The fake never falls back to
//!   `api.anthropic.com`.
//!
//! # Turns
//!
//! Each stream-json user line on stdin
//! (`{"type":"user","message":{"role":"user","content":"…"}}`) is one turn.
//! Other line types are ignored, and a line that isn't JSON exits with
//! status 1. For each turn the fake:
//!
//! 1. Prints a `system`/`init` line, as the real CLI does at the start of
//!    every turn, with `session_id`, `model` (from `--model`, default
//!    `claude-sonnet-5-5`), `tools` (from `--tools`), `permissionMode` and
//!    `apiKeySource`.
//! 2. Appends the user message to the transcript.
//! 3. Sends `POST $ANTHROPIC_BASE_URL/v1/messages?beta=true` with
//!    `"stream": true` and the message content. The credential is
//!    `x-api-key: $ANTHROPIC_API_KEY` when that is set, and otherwise
//!    `Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN`. The real CLI also
//!    prefers the API key when both are set. `anthropic-beta` is the real
//!    CLI's list for that credential, [`API_KEY_BETA`] or [`OAUTH_BETA`],
//!    and `anthropic-version`, `x-app: cli` and `x-claude-code-session-id`
//!    are sent as it sends them. If neither credential is set, or the answer
//!    isn't a 200 with a complete body, the turn ends with an `is_error`
//!    result carrying `api_error_status`, like the real CLI's, and the
//!    script turn is used up without running.
//! 4. Plays the next [`Turn`] of the script in `FAKE_CLAUDE_SCRIPT`: waits
//!    [`Turn::delay_ms`], exits with [`CRASH_EXIT_CODE`] if
//!    [`Turn::crash`], runs [`Turn::commands`], prints
//!    [`Turn::extra_lines`], then prints the reply as an `assistant` line
//!    and a `result` line, and appends it to the transcript.
//!
//! A result's `usage` is the turn's own, and its `total_cost_usd` is the
//! process's running total, as the real CLI reports them: each reply adds
//! [`REPLY_COST_USD`] and an error result nothing. A process started with
//! `--session-id` counts from 0, and one started with `--resume` from the
//! transcript's last `{"type":"cost-state","sessionId":…,"totalCostUSD":…}`
//! line of the session, or 0 without one. Like the real CLI, the fake
//! appends that line, with its running total and the other fields the CLI
//! 2.1.285 writes, when stdin ends and the session has a transcript, and a
//! crash writes none.
//!
//! With the OAuth token, the first successful turn of each process also
//! prints a `rate_limit_event` line right after its first `assistant` line,
//! shaped like the one in [`fixtures::TOOL_TURNS`](crate::fixtures::TOOL_TURNS).
//! The real CLI prints one when a subscription's rate-limit status changes,
//! which against a local server is once per process, and never with an API
//! key. It is a line the runner must skip.
//!
//! The script is read again for every turn, and turn *n* of a session plays
//! script turn *n*: the count comes from the user messages already in the
//! transcript, so a resumed process carries on where the last one stopped.
//! A turn past the end of the script ends with an `is_error` result. At the
//! end of stdin the fake exits with status 1 if the last result was an
//! error and 0 otherwise, as the real CLI does.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The environment variable naming the script file.
pub const SCRIPT_ENV: &str = "FAKE_CLAUDE_SCRIPT";

/// What each reply adds to the running `total_cost_usd` of its process's
/// results. A power of two, so the totals are exact.
pub const REPLY_COST_USD: f64 = 0.0009765625;

/// The exit status of a turn with [`Turn::crash`] set.
pub const CRASH_EXIT_CODE: i32 = 70;

/// The model `fake-claude` reports without `--model`: the real CLI's default
/// when this was written.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The `anthropic-beta` header `fake-claude` sends with the OAuth token:
/// what Claude Code 2.1.285 sent with `CLAUDE_CODE_OAUTH_TOKEN`.
pub const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,effort-2025-11-24,extended-cache-ttl-2025-04-11";

/// The `anthropic-beta` header `fake-claude` sends with an API key: what
/// Claude Code 2.1.285 sent with `ANTHROPIC_API_KEY`.
pub const API_KEY_BETA: &str = "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,effort-2025-11-24";

/// One turn of a `fake-claude` script. A script is a JSON list of turns; see
/// [`write_script`].
///
/// Unknown fields are rejected, so a typo fails the turn instead of being
/// ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// The reply text: the `assistant` message and the result's `result`.
    #[serde(default)]
    pub reply: String,
    /// Whether the result is an error, as an API error would make it: the
    /// assistant message is synthetic, `is_error` is true and
    /// `terminal_reason` is `api_error`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// The `api_error_status` of an error result, such as 429 or 401.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<u16>,
    /// How long to wait, after the API request, before running the
    /// commands and replying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Exit with [`CRASH_EXIT_CODE`] after the API request and the delay,
    /// without a result.
    #[serde(default, skip_serializing_if = "is_false")]
    pub crash: bool,
    /// Commands to run, in order, before replying, as the model would run
    /// them with its Bash tool: each is an argv whose program is found on
    /// `PATH`, such as `["agentctl", "react", "eyes"]`. They inherit the
    /// fake's environment and working directory. Each prints an `assistant`
    /// line with a `Bash` `tool_use` and a `user` line with its
    /// `tool_result`: the combined output, and `is_error` when the command
    /// failed. A failed command doesn't fail the turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Vec<String>>,
    /// Lines to print verbatim after the commands and before the reply,
    /// such as line types a parser must skip (`{"type":"active_goal"}`) or
    /// lines that aren't JSON at all. They are printed for an
    /// [`is_error`](Self::is_error) turn too, and not written to the
    /// transcript.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_lines: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Turn {
    /// A turn that replies with `text`.
    pub fn reply(text: impl Into<String>) -> Self {
        Self {
            reply: text.into(),
            ..Self::default()
        }
    }

    /// A turn whose result is an API error with `status`.
    pub fn api_error(status: u16, text: impl Into<String>) -> Self {
        Self {
            reply: text.into(),
            is_error: true,
            api_error_status: Some(status),
            ..Self::default()
        }
    }

    /// A turn that exits with [`CRASH_EXIT_CODE`] instead of replying.
    pub fn crash() -> Self {
        Self {
            crash: true,
            ..Self::default()
        }
    }

    /// Waits `delay` before replying.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay_ms = Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
        self
    }

    /// Runs `argv` before replying.
    pub fn with_command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.commands
            .push(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Prints `line` verbatim before the reply; see
    /// [`extra_lines`](Self::extra_lines).
    pub fn with_extra_line(mut self, line: impl Into<String>) -> Self {
        self.extra_lines.push(line.into());
        self
    }
}

/// Writes `turns` to `path` as a `fake-claude` script: the file to name in
/// `FAKE_CLAUDE_SCRIPT`.
pub fn write_script(path: &Path, turns: &[Turn]) -> io::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(turns)?)
}

/// The path of the `fake-claude` binary, built on first use.
///
/// Cargo sets `CARGO_BIN_EXE_<name>` only for a package's own integration
/// tests, so other crates call this instead. The first call in a process
/// runs `$CARGO build --locked -p testkit --bin fake-claude
/// --message-format=json` and reads the executable's path from the artifact
/// message. It builds in the target directory the running test executable
/// was built in, and with the environment cargo gave the test, so under
/// `cargo llvm-cov` the binary is instrumented and built in its target
/// directory. Later calls return the same path.
///
/// The first call blocks the calling thread until the build ends. After a
/// workspace `cargo test` that is a fraction of a second, but it can take
/// tens of seconds when the binary isn't built yet, for example when the
/// calling crate's tests resolved testkit's dependencies with other
/// features. Call it before starting any timeout, such as a turn timeout or
/// a `tokio::time::timeout` around a spawn, and outside code that measures
/// elapsed time.
///
/// Code that starts `fake-claude` with a cleared environment should pass
/// `LLVM_PROFILE_FILE` through when it is set. Under `cargo llvm-cov` the
/// binary is instrumented, and without it the binary writes
/// `default.profraw` into its working directory and its coverage is lost.
///
/// # Panics
///
/// If the build fails or reports no executable.
pub fn fake_claude_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| build_bin("testkit", "fake-claude"))
}

/// The path of the `agentctl` binary, built on first use, for tests that
/// let `fake-claude` run agentctl commands: put its directory on the
/// script's `PATH`.
///
/// It is built the way [`fake_claude_path`] builds `fake-claude`, with the
/// same blocking first call, so call it before starting any timeout too.
///
/// # Panics
///
/// If the build fails or reports no executable.
pub fn agentctl_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| build_bin("agentctl", "agentctl"))
}

fn build_bin(package: &str, name: &str) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .args(["--locked", "-p", package, "--bin", name])
        .arg("--message-format=json");
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = target_dir(&exe)
    {
        command.arg("--target-dir").arg(dir);
    }
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("running {} build: {err}", cargo.to_string_lossy()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "building {name} failed ({}):\n{stderr}",
        output.status
    );
    executable(&stdout, name)
        .unwrap_or_else(|| panic!("cargo reported no executable for {name}:\n{stderr}"))
}

/// The target directory `exe` was built in: its nearest ancestor holding
/// the `CACHEDIR.TAG` file cargo writes there.
///
/// Cargo passes a test the environment it was run with, but `cargo llvm-cov`
/// names its target directory with `--target-dir`, not `CARGO_TARGET_DIR`.
/// Without this the build would go to the default directory, with
/// `cargo llvm-cov`'s compiler wrapper still in the environment.
fn target_dir(exe: &Path) -> Option<&Path> {
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())
}

/// Finds the executable of bin target `name` in cargo's JSON messages.
fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<Artifact>(line).ok())
        .filter(|artifact| {
            artifact.reason == "compiler-artifact"
                && artifact.target.name == name
                && artifact.target.kind.iter().any(|kind| kind == "bin")
        })
        .find_map(|artifact| artifact.executable)
}

#[derive(Deserialize)]
struct Artifact {
    reason: String,
    target: Target,
    executable: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_serialize_without_defaults_and_reject_unknown_fields() {
        let turns = [
            Turn::reply("hi"),
            Turn::api_error(429, "limit")
                .with_delay(Duration::from_millis(5))
                .with_command(["agentctl", "react", "eyes"]),
            Turn::crash(),
            Turn::reply("raw")
                .with_extra_line(r#"{"type":"active_goal"}"#)
                .with_extra_line("not json"),
        ];
        let json = serde_json::to_value(&turns).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"reply": "hi"},
                {
                    "reply": "limit",
                    "is_error": true,
                    "api_error_status": 429,
                    "delay_ms": 5,
                    "commands": [["agentctl", "react", "eyes"]],
                },
                {"reply": "", "crash": true},
                {"reply": "raw", "extra_lines": [r#"{"type":"active_goal"}"#, "not json"]},
            ])
        );
        let back: Vec<Turn> = serde_json::from_value(json).unwrap();
        assert_eq!(back, turns);
        let typo = serde_json::from_str::<Turn>(r#"{"reply":"x","crashh":true}"#);
        assert!(typo.unwrap_err().to_string().contains("crashh"));
        assert_eq!(
            Turn::reply("x").with_delay(Duration::MAX).delay_ms,
            Some(u64::MAX)
        );
    }

    #[test]
    fn executable_is_read_from_the_bin_artifact_message() {
        let messages = [
            r#"{"reason":"compiler-artifact","target":{"name":"testkit","kind":["lib"]},"executable":null}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"fake-claude","kind":["test"]},"executable":"/t/deps/fake_claude-1"}"#,
            "not json",
            r#"{"reason":"compiler-artifact","target":{"name":"fake-claude","kind":["bin"]},"executable":"/t/debug/fake-claude"}"#,
            r#"{"reason":"build-finished","success":true}"#,
        ]
        .join("\n");
        assert_eq!(
            executable(&messages, "fake-claude"),
            Some(PathBuf::from("/t/debug/fake-claude"))
        );
        assert_eq!(executable(&messages, "other"), None);
    }

    #[test]
    fn target_dir_is_the_nearest_ancestor_with_a_cachedir_tag() {
        let root = std::env::temp_dir().join(format!("testkit-target-{}", uuid::Uuid::new_v4()));
        let deps = root.join("target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(root.join("target/CACHEDIR.TAG"), "").unwrap();
        let exe = deps.join("runner-0123");
        assert_eq!(target_dir(&exe), Some(root.join("target").as_path()));
        assert_eq!(target_dir(&root.join("elsewhere/exe")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[should_panic(expected = "building no-such-bin failed")]
    fn a_failed_build_panics_with_cargo_output() {
        build_bin("testkit", "no-such-bin");
    }
}
