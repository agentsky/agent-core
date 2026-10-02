//! [`ClaudeProcess`]: one `claude` process in stream-json mode.

use std::pin::Pin;
use std::time::Duration;

use core_types::{CredentialKind, SessionId};
use sandbox::{ChildHandle, Container, ContainerId, ExitStatus, Sandbox};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use crate::stream::{self, TurnOutcome, TurnStats};
use crate::{LaunchSpec, ProcessConfig, Result, RunnerError, SessionStart, launch};

/// How long a process that closed its stdout, or was killed, gets to exit
/// before it is killed or given up on.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The stdout buffer size, and the capacity the line buffer is shrunk back
/// to after each turn.
const READ_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between turns.
    Idle,
    /// A turn is being read. Still set when `send_turn` is next called if
    /// the previous call was cancelled, which leaves the stream mid-turn.
    InTurn,
    /// The process has ended or was killed. `exited` says whether its exit
    /// was seen; if not, it may still be running.
    Dead {
        /// Whether a wait returned after the process ended.
        exited: bool,
    },
}

/// How ending a process went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Exit {
    /// Whether the process was seen to exit.
    exited: bool,
    /// Its exit code, when it exited and the code could be read.
    code: Option<i32>,
}

impl Exit {
    const UNCONFIRMED: Self = Self {
        exited: false,
        code: None,
    };

    fn from_status(status: ExitStatus) -> Self {
        Self {
            exited: true,
            code: status.code,
        }
    }
}

/// What ending a process takes: a [`ChildHandle`], or a double in tests.
trait Child {
    async fn wait(&mut self) -> sandbox::Result<ExitStatus>;
    async fn kill(&mut self) -> sandbox::Result<()>;
}

impl Child for ChildHandle {
    async fn wait(&mut self) -> sandbox::Result<ExitStatus> {
        ChildHandle::wait(self).await
    }

    async fn kill(&mut self) -> sandbox::Result<()> {
        ChildHandle::kill(self).await
    }
}

/// Waits up to `grace` for a process that is ending, and kills it if it
/// doesn't exit by then or its status can't be read.
async fn wait_or_kill(child: &mut impl Child, session: SessionId, grace: Duration) -> Exit {
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(Ok(status)) => Exit::from_status(status),
        Ok(Err(error)) => {
            tracing::warn!(%session, %error, "reading claude's exit status failed; killing it");
            kill_and_wait(child, session, grace).await
        }
        Err(_) => {
            tracing::warn!(%session, "claude didn't exit; killing it");
            kill_and_wait(child, session, grace).await
        }
    }
}

/// Kills the process and waits up to `grace` for it. The exit is confirmed
/// only by a wait that returns in that time: a kill can fail, or, under
/// Docker, signal nothing.
async fn kill_and_wait(child: &mut impl Child, session: SessionId, grace: Duration) -> Exit {
    if let Err(error) = child.kill().await {
        tracing::warn!(%session, %error, "killing claude failed");
    }
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(Ok(status)) => Exit::from_status(status),
        Ok(Err(error)) => {
            tracing::warn!(%session, %error, "reading claude's exit status failed; it may still be running");
            Exit::UNCONFIRMED
        }
        Err(_) => {
            tracing::warn!(%session, "claude didn't exit after the kill; it may still be running");
            Exit::UNCONFIRMED
        }
    }
}

/// One `claude` process in stream-json mode, in a sandbox container.
///
/// Turns on one process are serialized by `&mut self`. A process whose turn
/// crashed or timed out is gone; start a new one with
/// [`SessionStart::Resume`]. [`stop`](Self::stop) ends a process
/// gracefully. Dropping a `ClaudeProcess` instead kills its process group
/// under a process sandbox, but under Docker the process may run on until
/// the container stops, so call `stop` or stop the container.
///
/// Killing a process doesn't always end it: the kill can fail, and under
/// Docker it can signal nothing. Once [`is_running`](Self::is_running) is
/// false, [`may_be_alive`](Self::may_be_alive) says whether the process was
/// seen to exit. If it wasn't, stop the container before starting another
/// process in it, or two processes may write one transcript.
pub struct ClaudeProcess {
    session: SessionId,
    container: ContainerId,
    credential: CredentialKind,
    model: Option<String>,
    turn_timeout: Duration,
    /// Declared before `stdin`, so a drop under a process sandbox kills a
    /// process still waiting for input instead of closing its input first
    /// and killing it in the middle of exiting.
    child: ChildHandle,
    stdin: Pin<Box<dyn AsyncWrite + Send>>,
    stdout: BufReader<Pin<Box<dyn AsyncRead + Send>>>,
    state: State,
    line: Vec<u8>,
    process_total_cost_usd: f64,
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
            process_total_cost_usd: 0.0,
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
    /// out, had a turn cancelled, refused its `--resume`, stopped reading
    /// its input or been stopped. A process that exited between turns is
    /// found out by the next turn, which then crashes.
    pub fn is_running(&self) -> bool {
        self.state == State::Idle
    }

    /// Whether the process may still be running: true until it has been
    /// seen to exit. After a turn that leaves
    /// [`is_running`](Self::is_running) false, or after
    /// [`stop`](Self::stop), true means the process was killed or given up
    /// on without its exit being confirmed, so stop the container before
    /// starting another process in it.
    pub fn may_be_alive(&self) -> bool {
        self.state != State::Dead { exited: true }
    }

    /// Runs one turn: writes `message` as a stream-json user line and reads
    /// lines until the `result` line.
    ///
    /// - A `result` line gives [`TurnOutcome::Finished`], and the process
    ///   takes the next turn, even after an error result, unless the write
    ///   failed or the result is shaped like a refused `--resume`
    ///   ([`TurnOutcome::resume_refused`]), after which the CLI exits: the
    ///   process is ending then, and is reaped before the result is
    ///   returned.
    /// - The end of stdout, or a failed write, before a result gives
    ///   [`TurnOutcome::Crashed`] once the process exits (it is killed if it
    ///   doesn't within a few seconds).
    /// - The configured turn timeout, counted from the call, kills the
    ///   process and gives [`TurnOutcome::TimedOut`].
    ///
    /// After a crash or a timeout the process is gone. Cancelling this
    /// future (dropping it before it completes) leaves the stream in the
    /// middle of a turn, so the next call kills the process and returns
    /// [`RunnerError::NotRunning`]. Whenever the process is gone,
    /// [`may_be_alive`](Self::may_be_alive) says whether its exit was
    /// confirmed.
    ///
    /// The result's [`cost_usd`](crate::TurnResult::cost_usd) is the turn's
    /// own: the CLI reports a running total for the process, and the
    /// process keeps the previous total to take it off.
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
            State::Dead { .. } => return Err(RunnerError::NotRunning),
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
            Ok((Some(result), write_failed)) => {
                stats.duration = started.elapsed();
                let outcome = TurnOutcome::Finished(
                    result.into_result(stats, &mut self.process_total_cost_usd),
                );
                if write_failed || outcome.resume_refused() {
                    self.reap().await;
                } else {
                    self.state = State::Idle;
                }
                outcome
            }
            Ok((None, _)) => {
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
        self.line.clear();
        self.line.shrink_to(READ_BUFFER);
        self.log_outcome(&outcome, message.len());
        Ok(outcome)
    }

    /// Closes the process's stdin, which ends the CLI, and waits for it to
    /// exit, killing it if it doesn't within a few seconds. It does nothing
    /// to a process that is already gone. Afterwards
    /// [`may_be_alive`](Self::may_be_alive) says whether the exit was
    /// confirmed.
    pub async fn stop(&mut self) {
        if !matches!(self.state, State::Dead { .. }) {
            let _ = self.stdin.shutdown().await;
            let exit_code = self.reap().await;
            tracing::info!(
                session = %self.session,
                ?exit_code,
                may_be_alive = self.may_be_alive(),
                "stopped claude"
            );
        }
    }

    /// Writes the line and reads the turn. The result is `None` when the
    /// process is gone, and the flag says whether the write failed.
    ///
    /// A failed write still reads what the process printed before it went:
    /// the CLI can print a `result` and exit without reading its input, as
    /// it does for a `--resume` with no transcript.
    async fn exchange(
        &mut self,
        line: &[u8],
        stats: &mut TurnStats,
    ) -> (Option<stream::ResultLine>, bool) {
        let written = async {
            self.stdin.write_all(line).await?;
            self.stdin.flush().await
        }
        .await;
        let write_failed = written.is_err();
        if let Err(error) = written {
            tracing::warn!(session = %self.session, %error, "writing to claude's stdin failed");
            let _ = self.stdin.shutdown().await;
        }
        let result = stream::read_turn(&mut self.stdout, &mut self.line, stats).await;
        (result, write_failed)
    }

    /// Waits for a process that is ending, killing it if it takes longer
    /// than [`EXIT_GRACE`]. Returns its exit code if it could be read.
    async fn reap(&mut self) -> Option<i32> {
        self.state = State::Dead { exited: false };
        let exit = wait_or_kill(&mut self.child, self.session, EXIT_GRACE).await;
        self.state = State::Dead {
            exited: exit.exited,
        };
        exit.code
    }

    /// Kills the process and waits up to [`EXIT_GRACE`] for it.
    async fn kill_and_reap(&mut self) {
        self.state = State::Dead { exited: false };
        let exit = kill_and_wait(&mut self.child, self.session, EXIT_GRACE).await;
        self.state = State::Dead {
            exited: exit.exited,
        };
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
            running = self.is_running(),
            may_be_alive = self.may_be_alive(),
            cost_usd = result.and_then(|r| r.cost_usd),
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

    const GRACE: Duration = Duration::from_millis(20);

    /// A process double: whether it has exited, whether a kill fails,
    /// whether a kill ends it, and whether its status can be read.
    #[derive(Default)]
    struct FakeChild {
        exited: bool,
        kill_fails: bool,
        ignores_kill: bool,
        wait_fails: bool,
        kills: u32,
    }

    impl Child for FakeChild {
        async fn wait(&mut self) -> sandbox::Result<ExitStatus> {
            if self.wait_fails {
                return Err(sandbox::SandboxError::NotFound);
            }
            if !self.exited {
                std::future::pending::<()>().await;
            }
            Ok(ExitStatus {
                code: Some(if self.kills > 0 { 137 } else { 0 }),
            })
        }

        async fn kill(&mut self) -> sandbox::Result<()> {
            self.kills += 1;
            if self.kill_fails {
                return Err(sandbox::SandboxError::Docker {
                    op: "exec kill",
                    status: None,
                    message: None,
                });
            }
            if !self.ignores_kill {
                self.exited = true;
            }
            Ok(())
        }
    }

    fn session() -> SessionId {
        SessionId::new_v4()
    }

    #[tokio::test]
    async fn dropping_a_process_kills_it_before_closing_its_stdin() {
        let dir = crate::test_util::TempDir::new();
        let sealer = store::Sealer::from_base64(&store::Sealer::generate_key().unwrap()).unwrap();
        let store = store::Store::open_in_memory(sealer).await.unwrap();
        let sandbox = sandbox::ProcessSandbox::new(store, dir.0.clone()).unwrap();
        let volume = sandbox
            .ensure_volume(&core_types::VolumeKey {
                agent: core_types::AgentId::new_v4(),
                scope: core_types::ScopeKey::Private,
            })
            .await
            .unwrap();
        let spec = sandbox::SessionSpec::new(session(), volume, "unused", dir.0.join("persona"));
        let container = sandbox.start(&spec).await.unwrap();
        let pid_file = dir.0.join("pid");
        let script = format!("echo $$ > '{}'; exec cat >/dev/null", pid_file.display());
        let argv = ["/bin/sh", "-c", &script].map(str::to_owned);
        let io = sandbox
            .exec(&container, &argv, &std::collections::BTreeMap::new())
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&pid_file).is_ok_and(|pid| pid.ends_with('\n')) {
            assert!(
                std::time::Instant::now() < deadline,
                "the process never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (stdin, killed_first) = testkit::child::NotingStdin::new(io.stdin, pid_file);
        let process = ClaudeProcess {
            session: container.session(),
            container: container.id().clone(),
            credential: CredentialKind::Subscription,
            model: None,
            turn_timeout: Duration::from_secs(60),
            child: io.child,
            stdin: Box::pin(stdin),
            stdout: BufReader::new(io.stdout),
            state: State::Idle,
            line: Vec::new(),
            process_total_cost_usd: 0.0,
        };
        drop(process);
        assert!(
            killed_first.load(std::sync::atomic::Ordering::SeqCst),
            "the process's stdin closed before it was killed, so it could start exiting"
        );
    }

    #[tokio::test]
    async fn a_process_that_exits_is_reaped_without_a_kill() {
        let mut child = FakeChild {
            exited: true,
            ..FakeChild::default()
        };
        let exit = wait_or_kill(&mut child, session(), GRACE).await;
        assert_eq!(
            exit,
            Exit {
                exited: true,
                code: Some(0)
            }
        );
        assert_eq!(child.kills, 0);
    }

    #[tokio::test]
    async fn a_process_that_lingers_is_killed_and_reaped() {
        let mut child = FakeChild::default();
        let exit = wait_or_kill(&mut child, session(), GRACE).await;
        assert_eq!(
            exit,
            Exit {
                exited: true,
                code: Some(137)
            }
        );
        assert_eq!(child.kills, 1);
    }

    #[tokio::test]
    async fn a_kill_that_signals_nothing_leaves_the_exit_unconfirmed() {
        let mut child = FakeChild {
            ignores_kill: true,
            ..FakeChild::default()
        };
        assert_eq!(
            kill_and_wait(&mut child, session(), GRACE).await,
            Exit::UNCONFIRMED
        );
        let mut child = FakeChild {
            ignores_kill: true,
            ..FakeChild::default()
        };
        assert_eq!(
            wait_or_kill(&mut child, session(), GRACE).await,
            Exit::UNCONFIRMED
        );
        assert_eq!(child.kills, 1);
    }

    #[tokio::test]
    async fn a_failed_kill_leaves_the_exit_unconfirmed() {
        let mut child = FakeChild {
            kill_fails: true,
            ..FakeChild::default()
        };
        assert_eq!(
            kill_and_wait(&mut child, session(), GRACE).await,
            Exit::UNCONFIRMED
        );
        let mut exited = FakeChild {
            kill_fails: true,
            exited: true,
            ..FakeChild::default()
        };
        assert!(
            kill_and_wait(&mut exited, session(), GRACE).await.exited,
            "a failed kill of a process that had exited is still an exit"
        );
    }

    #[tokio::test]
    async fn an_unreadable_status_is_killed_and_left_unconfirmed() {
        let mut child = FakeChild {
            wait_fails: true,
            ..FakeChild::default()
        };
        assert_eq!(
            wait_or_kill(&mut child, session(), GRACE).await,
            Exit::UNCONFIRMED
        );
        assert_eq!(child.kills, 1);
    }

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
