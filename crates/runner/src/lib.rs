//! Claude Code stream-json driver and session runner for agent-core.
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
//!    although the CLI reports a running total for its process.
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
//! [`Container`]: sandbox::Container
//! [`Container::paths`]: sandbox::Container::paths

#![warn(missing_docs)]

mod config;
mod launch;
mod persona;
mod process;
mod stream;

pub use config::{
    ConfigError, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_CLAUDE_BIN, DEFAULT_TURN_TIMEOUT_SECS,
    ProcessConfig,
};
pub use launch::{LaunchSpec, SessionStart};
pub use persona::{AGENTS_DIR, persona_dir, write_persona};
pub use process::ClaudeProcess;
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
}

/// A `Result` whose error is [`RunnerError`].
pub type Result<T, E = RunnerError> = std::result::Result<T, E>;

#[cfg(test)]
mod test_util;

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
    }
}
