//! Claude Code stream-json driver and session runner for agent-core.
//!
//! [`SessionManager`] runs turns on sessions: it looks sessions up or makes
//! them (the store's `sessions` table), queues each session's turns, and
//! keeps a warm container and `claude` process per active session, with a
//! per-scope and a global container cap and an idle reaper. It never calls
//! agentd: placeholders and agentctl tokens reach it through the
//! [`TurnHooks`] agentd implements, around every process and every turn.
//!
//! [`ClaudeProcess`] drives one `claude` process in a sandbox container, in
//! the design's stream-json mode:
//!
//! 1. [`ClaudeProcess::start`] execs `claude` in a [`Container`] with the
//!    design's launch flags and the credential proxy's environment, built
//!    from a [`LaunchSpec`] and a [`ProcessConfig`]. Every path it uses
//!    comes from [`Container::paths`].
//! 2. [`ClaudeProcess::send_turn`] writes one stream-json user line and
//!    reads lines until the `result` line, giving a [`TurnOutcome`]: a
//!    [`TurnResult`], a [`Crashed`](TurnOutcome::Crashed) process, or a
//!    [`TimedOut`](TurnOutcome::TimedOut) turn whose process was killed.
//!    A result's [`cost_usd`](TurnResult::cost_usd) is the turn's own,
//!    although the CLI reports a running total for its process, except on
//!    the first turn of a process started with `--resume`, whose running
//!    total the CLI starts from the session's saved total.
//! 3. [`ClaudeProcess::stop`] closes stdin and waits for the process.
//!
//! A kill doesn't always end a process: it can fail, and under Docker
//! signal nothing. Once a process is gone,
//! [`ClaudeProcess::may_be_alive`] says whether its exit was seen; if not,
//! stop the container before starting another process in it.
//!
//! # What is kept and logged
//!
//! Message bodies, tool inputs and tool output can hold file contents and
//! secrets that no field-name redaction can catch, so the driver never
//! keeps or logs them. From each turn it keeps only what the plan lists:
//! the result line's fields and [`TurnStats`], which counts messages and
//! lines and names the tools called. The reply text is kept, since it is
//! delivered, but never logged, and the `Debug` forms of [`TurnResult`] and
//! [`LaunchSpec`] show only its length and the environment's names.
//!
//! # Parsing
//!
//! Lines are parsed leniently, as the plan's "Claude Code CLI" section asks:
//! unknown line types and fields are skipped, and a line that isn't JSON is
//! skipped with a log of its length and the parse error, never its text.
//! Lines are parsed as plain JSON values first and fields are then read one
//! by one, so no typed deserialization error can quote a value from the
//! line.
//!
//! # Persona
//!
//! The persona file is `<data>/agents/<agent>/persona.md`
//! ([`persona_dir`]), which agentd writes with [`write_persona`] when the
//! persona changes. The sandbox mounts the directory, and the process gets
//! the file's path from [`Container::paths`]. A persona edit takes effect
//! when the process next starts.
//!
//! # Skills
//!
//! An agent's skills are the directories in `<data>/skills/<agent>`
//! ([`skills_dir`]), which agentd writes. Every session of the agent mounts
//! that directory read-only as its `$CLAUDE_CONFIG_DIR/skills`, when it
//! exists as the session's container starts.
//!
//! [`Container`]: sandbox::Container
//! [`Container::paths`]: sandbox::Container::paths

#![warn(missing_docs)]

mod config;
mod hooks;
mod launch;
mod persona;
mod process;
mod sessions;
mod stream;

pub use config::{
    ConfigError, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_CLAUDE_BIN, DEFAULT_GLOBAL_CONTAINER_CAP,
    DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_SCOPE_CONTAINER_CAP, DEFAULT_TURN_TIMEOUT_SECS, PoolConfig,
    ProcessConfig,
};
pub use hooks::{HookError, ProcessEnv, TurnHooks, TurnRequest};
pub use launch::{LaunchSpec, SessionStart};
pub use persona::{
    AGENTS_DIR, SKILLS_DIR, persona_dir, skills_dir, write_if_changed, write_persona,
};
pub use process::ClaudeProcess;
pub use sessions::{SessionConfig, SessionManager, TurnReport};
pub use store::{Session, SessionKind};
pub use stream::{ErrorKind, MAX_LINE_BYTES, TurnOutcome, TurnResult, TurnStats, Usage};

/// The error returned by [`ClaudeProcess`] and the other runner functions.
///
/// No variant carries an environment value, a message body or a line of
/// the process's output.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The [`ProcessConfig`] is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The [`LaunchSpec`] breaks one of its rules.
    #[error("invalid launch spec: {0}")]
    InvalidSpec(&'static str),
    /// The sandbox failed to start the process.
    #[error(transparent)]
    Sandbox(#[from] sandbox::SandboxError),
    /// The process has ended, after a crash, a timeout or a cancelled
    /// turn. Start a new one with [`SessionStart::Resume`].
    #[error("the claude process is not running")]
    NotRunning,
    /// A filesystem operation failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        source: std::io::Error,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] store::StoreError),
    /// A [`TurnHooks`] call failed.
    #[error("the {hook} hook failed: {source}")]
    Hook {
        /// The hook, such as `process_starting`.
        hook: &'static str,
        /// What it returned.
        source: HookError,
    },
    /// No session has this id.
    #[error("no such session")]
    UnknownSession,
    /// The session was reset, and takes no more turns.
    #[error("the session was reset")]
    SessionReset,
    /// A [`TurnRequest`] doesn't fit its session.
    #[error("invalid turn request: {0}")]
    InvalidRequest(&'static str),
    /// The task running the turn panicked.
    #[error("the turn's task failed")]
    TurnTask,
}

/// A `Result` whose error is [`RunnerError`].
pub type Result<T, E = RunnerError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_without_values() {
        assert_eq!(
            RunnerError::InvalidSpec("x").to_string(),
            "invalid launch spec: x"
        );
        assert_eq!(
            RunnerError::NotRunning.to_string(),
            "the claude process is not running"
        );
        let io = RunnerError::Io {
            what: "writing the persona",
            source: std::io::Error::other("disk full"),
        };
        assert_eq!(io.to_string(), "writing the persona: disk full");
        let sandbox: RunnerError = sandbox::SandboxError::NotFound.into();
        assert_eq!(sandbox.to_string(), "no such container");
        let hook = RunnerError::Hook {
            hook: "turn_starting",
            source: "refused".into(),
        };
        assert_eq!(hook.to_string(), "the turn_starting hook failed: refused");
        assert_eq!(RunnerError::UnknownSession.to_string(), "no such session");
        assert_eq!(
            RunnerError::SessionReset.to_string(),
            "the session was reset"
        );
        assert_eq!(
            RunnerError::InvalidRequest("x").to_string(),
            "invalid turn request: x"
        );
        assert_eq!(RunnerError::TurnTask.to_string(), "the turn's task failed");
    }
}
