//! [`ClaudeProcess`]: one `claude` process in stream-json mode.

use std::pin::Pin;
use std::time::Duration;

use core_types::{CredentialKind, SessionId};
use sandbox::{ChildHandle, Container, ContainerId, Sandbox};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use crate::stream::{self, TurnOutcome, TurnStats};
use crate::{LaunchSpec, ProcessConfig, Result, RunnerError, SessionStart, launch};

/// How long a process that closed its stdout, or was killed, gets to exit
/// before it is killed or given up on.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The stdout buffer size.
const READ_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between turns.
    Idle,
    /// A turn is being read. Still set when `send_turn` is next called if
    /// the previous call was cancelled, which leaves the stream mid-turn.
    InTurn,
    /// The process has ended or was killed.
    Dead,
}

/// One `claude` process in stream-json mode, in a sandbox container.
///
/// Turns on one process are serialized by `&mut self`. A process whose turn
/// crashed or timed out is gone; start a new one with
/// [`SessionStart::Resume`]. [`stop`](Self::stop) ends a process
/// gracefully. Dropping a `ClaudeProcess` instead kills its process group
/// under a process sandbox, but under Docker the process may run on until
/// the container stops, so call `stop` or stop the container.
pub struct ClaudeProcess {
    session: SessionId,
    container: ContainerId,
    credential: CredentialKind,
    model: Option<String>,
    turn_timeout: Duration,
    stdin: Pin<Box<dyn AsyncWrite + Send>>,
    stdout: BufReader<Pin<Box<dyn AsyncRead + Send>>>,
    child: ChildHandle,
    state: State,
    line: Vec<u8>,
}

impl std::fmt::Debug for ClaudeProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeProcess")
            .field("session", &self.session)
            .field("container", &self.container)
            .field("credential", &self.credential)
            .field("model", &self.model)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ClaudeProcess {
    /// Starts `claude` in `container` for the container's session.
    ///
    /// The argv is the design's launch flags: `-p`, stream-json input and
    /// output, `--verbose`, `--tools "Bash,Read,Edit,Write,Glob,Grep"`,
    /// `--strict-mcp-config`, `--setting-sources user`, `--permission-mode
    /// bypassPermissions`, `--append-system-prompt-file` with the persona
    /// file from [`Container::paths`], `--model` when `spec` names one, and
    /// `--session-id <id>` or `--resume <id>` as [`LaunchSpec::start`] says.
    ///
    /// The environment is `spec.env`, then the design's credential proxy
    /// block: the placeholder in `CLAUDE_CODE_OAUTH_TOKEN` or
    /// `ANTHROPIC_API_KEY` (never both), `ANTHROPIC_BASE_URL`,
    /// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `DISABLE_AUTOUPDATER=1`,
    /// `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_PROJECT_DIR_NAME` (the session
    /// id), and `HOME` and `TMPDIR`, all paths from [`Container::paths`].
    ///
    /// The CLI reads nothing until the first turn, so a process the CLI
    /// refuses (a `--session-id` whose transcript exists, say) shows up as
    /// the first turn's [`TurnOutcome::Crashed`].
    ///
    /// # Errors
    ///
    /// [`RunnerError::Config`] for an invalid `config`,
    /// [`RunnerError::InvalidSpec`] for a spec that breaks a rule on
    /// [`LaunchSpec`], and [`RunnerError::Sandbox`] if the sandbox can't
    /// start it.
    pub async fn start(
        sandbox: &dyn Sandbox,
        container: &Container,
        config: &ProcessConfig,
        spec: LaunchSpec,
    ) -> Result<Self> {
        config.validate()?;
        let session = container.session();
        let paths = container.paths();
        let argv = launch::argv(config, paths, session, &spec)?;
        let env = launch::env(config, paths, session, &spec)?;
        let io = sandbox.exec(container, &argv, &env).await?;
        tracing::info!(
            %session,
            container = %container.id(),
            resume = spec.start == SessionStart::Resume,
            credential = ?spec.credential,
            model = spec.model.as_deref().unwrap_or("<default>"),
            "started claude"
        );
        Ok(Self {
            session,
            container: container.id().clone(),
            credential: spec.credential,
            model: spec.model,
            turn_timeout: config.turn_timeout(),
            stdin: io.stdin,
            stdout: BufReader::with_capacity(READ_BUFFER, io.stdout),
            child: io.child,
            state: State::Idle,
            line: Vec::new(),
        })
    }

    /// The session the process runs.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The container it runs in.
    pub fn container(&self) -> &ContainerId {
        &self.container
    }

    /// The credential kind it was started with. A turn on another kind
    /// needs a new process.
    pub fn credential(&self) -> CredentialKind {
        self.credential
    }

    /// The `--model` it was started with, if any. A turn on another model
    /// needs a new process.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether the process can take another turn: it hasn't crashed, timed
    /// out or had a turn cancelled. A process that exited between turns is
    /// found out by the next turn, which then crashes.
    pub fn is_running(&self) -> bool {
        self.state == State::Idle
    }

    /// Runs one turn: writes `message` as a stream-json user line and reads
    /// lines until the `result` line.
    ///
    /// - A `result` line gives [`TurnOutcome::Finished`], and the process
    ///   takes the next turn, even after an error result.
    /// - The end of stdout, or a failed write, before a result gives
    ///   [`TurnOutcome::Crashed`] once the process exits (it is killed if it
    ///   doesn't within a few seconds).
    /// - The configured turn timeout, counted from the call, kills the
    ///   process and gives [`TurnOutcome::TimedOut`].
    ///
    /// After a crash or a timeout the process is gone. Cancelling this
    /// future (dropping it before it completes) leaves the stream in the
    /// middle of a turn, so the next call kills the process and returns
    /// [`RunnerError::NotRunning`].
    ///
    /// Neither the message nor any line of output is logged. One log line
    /// per turn gives its outcome, [`TurnStats`] and the result's codes.
    ///
    /// # Errors
    ///
    /// [`RunnerError::NotRunning`] if the process has already ended.
    pub async fn send_turn(&mut self, message: &str) -> Result<TurnOutcome> {
        match self.state {
            State::Idle => {}
            State::Dead => return Err(RunnerError::NotRunning),
            State::InTurn => {
                tracing::warn!(session = %self.session, "a cancelled turn left the process mid-turn; killing it");
                self.kill_and_reap().await;
                return Err(RunnerError::NotRunning);
            }
        }
        self.state = State::InTurn;
        let started = Instant::now();
        let mut stats = TurnStats::default();
        let line = user_line(message);
        let exchanged =
            tokio::time::timeout(self.turn_timeout, self.exchange(&line, &mut stats)).await;
        let outcome = match exchanged {
            Ok(Some(result)) => {
                self.state = State::Idle;
                stats.duration = started.elapsed();
                TurnOutcome::Finished(result.into_result(stats))
            }
            Ok(None) => {
                let exit_code = self.reap().await;
                stats.duration = started.elapsed();
                TurnOutcome::Crashed { exit_code, stats }
            }
            Err(_elapsed) => {
                self.kill_and_reap().await;
                stats.duration = started.elapsed();
                TurnOutcome::TimedOut { stats }
            }
        };
        self.log_outcome(&outcome, message.len());
        Ok(outcome)
    }

    /// Closes the process's stdin, which ends the CLI, and waits for it to
    /// exit, killing it if it doesn't within a few seconds.
    pub async fn stop(mut self) {
        if self.state != State::Dead {
            let _ = self.stdin.shutdown().await;
            let exit_code = self.reap().await;
            tracing::info!(session = %self.session, ?exit_code, "stopped claude");
        }
    }

    /// Writes the line and reads the turn. `None` means the process is gone.
    ///
    /// A failed write still reads what the process printed before it went:
    /// the CLI can print a `result` and exit without reading its input, as
    /// it does for a `--resume` with no transcript.
    async fn exchange(&mut self, line: &[u8], stats: &mut TurnStats) -> Option<stream::ResultLine> {
        let written = async {
            self.stdin.write_all(line).await?;
            self.stdin.flush().await
        }
        .await;
        if let Err(error) = written {
            tracing::warn!(session = %self.session, %error, "writing to claude's stdin failed");
            let _ = self.stdin.shutdown().await;
        }
        stream::read_turn(&mut self.stdout, &mut self.line, stats).await
    }

    /// Waits for a process that is ending, killing it if it takes longer
    /// than [`EXIT_GRACE`]. Returns its exit code if it could be read.
    async fn reap(&mut self) -> Option<i32> {
        self.state = State::Dead;
        match tokio::time::timeout(EXIT_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => status.code,
            Ok(Err(error)) => {
                tracing::warn!(session = %self.session, %error, "reading claude's exit status failed");
                None
            }
            Err(_) => {
                tracing::warn!(session = %self.session, "claude didn't exit; killing it");
                self.kill_and_reap().await
            }
        }
    }

    /// Kills the process and waits up to [`EXIT_GRACE`] for it.
    async fn kill_and_reap(&mut self) -> Option<i32> {
        self.state = State::Dead;
        if let Err(error) = self.child.kill().await {
            tracing::warn!(session = %self.session, %error, "killing claude failed");
        }
        match tokio::time::timeout(EXIT_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => status.code,
            _ => None,
        }
    }

    fn log_outcome(&self, outcome: &TurnOutcome, message_len: usize) {
        let stats = outcome.stats();
        let (kind, result) = match outcome {
            TurnOutcome::Finished(result) => ("finished", Some(result)),
            TurnOutcome::Crashed { .. } => ("crashed", None),
            TurnOutcome::TimedOut { .. } => ("timed_out", None),
        };
        let exit_code = match outcome {
            TurnOutcome::Crashed { exit_code, .. } => *exit_code,
            _ => None,
        };
        tracing::info!(
            session = %self.session,
            outcome = kind,
            message_len,
            is_error = result.map(|r| r.is_error),
            error_kind = ?result.and_then(|r| r.error_kind),
            subtype = result.and_then(|r| r.subtype.as_deref()),
            terminal_reason = result.and_then(|r| r.terminal_reason.as_deref()),
            api_error_status = result.and_then(|r| r.api_error_status),
            api_error = stats.api_error.as_deref(),
            result_len = result.and_then(|r| r.result.as_ref()).map(String::len),
            exit_code,
            init_seen = stats.init_seen,
            assistant_messages = stats.assistant_messages,
            tool_calls = ?stats.tool_calls,
            ignored_lines = stats.ignored_lines,
            malformed_lines = stats.malformed_lines,
            duration_ms = u64::try_from(stats.duration.as_millis()).unwrap_or(u64::MAX),
            "claude turn ended"
        );
    }
}

/// A stream-json user line for `message`, with its newline.
fn user_line(message: &str) -> Vec<u8> {
    let mut line = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": message},
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_line_is_one_json_line() {
        let line = user_line("two\nlines \"quoted\"");
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(line.last(), Some(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "two\nlines \"quoted\""},
            })
        );
    }
}
