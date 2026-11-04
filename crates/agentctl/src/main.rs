//! In-sandbox CLI that agents use to call back into agentd.
//!
//! It reads `AGENTCTL_URL` (default `http://agentctl.internal:8081`) and
//! `AGENTCTL_TOKEN` from the environment, sends one request to agentd's ctl
//! API per command (several for `lock`), and prints the result as plain
//! text for the model. On a refusal or failure it prints one line,
//! `agentctl: <reason>`, to standard error and exits with status 1. Usage
//! errors exit with status 2. `lock` exits with its command's status.

mod client;
mod lock;
mod output;

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use core_types::{AskAgentRequest, HistoryRequest, PostRequest, PrivateRequest, ReactRequest};

use client::{Client, Failure};

/// Where agentd's ctl API is when `AGENTCTL_URL` is not set.
pub const DEFAULT_URL: &str = "http://agentctl.internal:8081";
/// How long `lock` waits for the lock by default: under the two minutes
/// Claude Code's Bash tool allows a command by default, so the model sees
/// why it failed instead of a killed command.
pub const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 100;

/// Calls back into agentd from inside the sandbox. Every command works only
/// during a turn.
#[derive(Parser)]
#[command(name = "agentctl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stage a file to upload with this turn's reply.
    Attach {
        /// The file to attach.
        path: PathBuf,
    },
    /// Post a message somewhere else the agent may post. It is sent after
    /// the turn.
    Post {
        /// Where: `here` (this thread), a conversation id, or
        /// `<conversation id>/<message id>` for a thread. A channel turn may
        /// post only in its own conversation.
        #[arg(long, value_name = "TARGET")]
        to: String,
        /// The Markdown text. Several words are joined with spaces.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Add a reaction to a message in this conversation, after the turn.
    React {
        /// The emoji's short name, such as `eyes`.
        emoji: String,
        /// The message to react to. Without it, the message that started
        /// the turn.
        message: Option<String>,
    },
    /// Read more of this thread than the turn included, oldest first.
    History {
        /// Only messages older than this message id.
        #[arg(long, value_name = "ID")]
        before: Option<String>,
        /// The most messages to print (1 to 200, default 50).
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },
    /// Run a command while holding this scope's `shared/` lock, for writes
    /// to `shared/`. Waits while another command holds it.
    Lock {
        /// Give up after waiting this many seconds for the lock or for
        /// agentd to answer, at most a day. A request sent near the end
        /// still gets seven seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_LOCK_TIMEOUT_SECS)]
        timeout: u64,
        /// The command and its arguments, after `--`. It is run directly,
        /// not through a shell: use `sh -c '…'` for pipes.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Hand a task to another agent: it is posted in this thread after this
    /// turn, mentioning that agent. The hop is billed to this turn's
    /// requester.
    AskAgent {
        /// The other agent's name, or its bot's handle as a mention
        /// (`@handle`).
        agent: String,
        /// The task. Several words are joined with spaces.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        task: Vec<String>,
    },
    /// Ask for a task on the owner's private resources. Returns a consent id
    /// at once; the result is posted to this thread when the task finishes.
    Private {
        /// A file in this session's directory to hand to the task. Repeat
        /// for several.
        #[arg(long = "file", value_name = "PATH")]
        files: Vec<String>,
        /// The task, shown to the owner exactly as given. Several words are
        /// joined with spaces.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        task: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("can't start: {err}")),
    };
    let env = |name: &str| std::env::var(name).ok();
    match runtime.block_on(run(cli.command, &env)) {
        Ok(code) => code,
        Err(message) => fail(&message),
    }
}

/// Prints `agentctl: <message>` on one line and returns status 1.
fn fail(message: &str) -> ExitCode {
    eprintln!("agentctl: {}", output::one_line(message));
    ExitCode::FAILURE
}

/// Runs `command` against the API that `env` names.
async fn run(command: Command, env: &dyn Fn(&str) -> Option<String>) -> Result<ExitCode, String> {
    let client = Client::from_env(env)?;
    let text = match command {
        Command::Attach { path } => {
            let response = client.attach(&path).await?;
            output::attached(&response)
        }
        Command::Post { to, text } => {
            client
                .send(&PostRequest {
                    to,
                    text: text.join(" "),
                })
                .await?;
            output::POSTED.to_owned()
        }
        Command::React { emoji, message } => {
            let request = ReactRequest { emoji, message };
            client.send(&request).await?;
            output::reacted(&request.emoji)
        }
        Command::History { before, limit } => {
            let response = client.send(&HistoryRequest { before, limit }).await?;
            output::history(&response.messages)
        }
        Command::Lock { timeout, command } => {
            return lock::run(&client, Duration::from_secs(timeout), &command).await;
        }
        Command::AskAgent { agent, task } => {
            client
                .send(&AskAgentRequest {
                    agent,
                    task: task.join(" "),
                })
                .await?;
            output::ASKED.to_owned()
        }
        Command::Private { files, task } => {
            let files = if files.is_empty() {
                files
            } else {
                let cwd = std::env::current_dir()
                    .map_err(|err| format!("can't read the working directory: {err}"))?;
                let config_dir =
                    env(CONFIG_DIR_VAR).ok_or_else(|| format!("{CONFIG_DIR_VAR} is not set"))?;
                let session_dir = Path::new(&config_dir)
                    .parent()
                    .ok_or_else(|| format!("{CONFIG_DIR_VAR} is not in a session directory"))?;
                files
                    .iter()
                    .map(|file| in_session(file, &cwd, session_dir))
                    .collect::<Result<_, _>>()?
            };
            let response = client
                .send(&PrivateRequest {
                    task: task.join(" "),
                    files,
                })
                .await?;
            output::private(&response)
        }
    };
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

/// The variable naming the session's Claude config directory, whose parent
/// is the session's directory.
const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// `file`, relative to `cwd` unless absolute, as a path relative to
/// `session_dir`, with `.` and `..` resolved as written. agentd checks the
/// path again and opens it without following symlinks.
fn in_session(file: &str, cwd: &Path, session_dir: &Path) -> Result<String, String> {
    let outside = || format!("{file} is not in this session's directory");
    let path = normalized(&cwd.join(file)).ok_or_else(outside)?;
    let session_dir = normalized(session_dir).ok_or_else(outside)?;
    let relative = path.strip_prefix(&session_dir).map_err(|_| outside())?;
    if relative.as_os_str().is_empty() {
        return Err(format!("{file} is a directory, not a file"));
    }
    relative
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{file} isn't a valid UTF-8 path"))
}

/// `path` with `.` dropped and each `..` taking the component before it
/// away; `None` if a `..` would climb above the root.
fn normalized(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

impl From<Failure> for String {
    fn from(failure: Failure) -> Self {
        failure.to_string()
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::*;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("agentctl").chain(args.iter().copied()))
            .map(|cli| cli.command)
    }

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn agentctl_has_no_cloud_command() {
        let cli = Cli::command();
        let names: Vec<&str> = cli.get_subcommands().map(clap::Command::get_name).collect();
        assert!(
            names.iter().all(|name| !name.contains("cloud")),
            "{names:?}"
        );
        for args in [
            ["cloud", "run", "agent-core", "fix it"].as_slice(),
            ["cloud", "add", "agent-core", "https://x", "sk-ant-oat01-x"].as_slice(),
            ["cloud-run", "agent-core", "fix it"].as_slice(),
        ] {
            let Err(err) = parse(args) else {
                panic!("{args:?} parsed as a command");
            };
            assert_eq!(err.kind(), ErrorKind::InvalidSubcommand, "{args:?}");
        }
        let help = cli.clone().render_long_help().to_string();
        assert!(!help.to_lowercase().contains("cloud"), "{help}");
    }

    #[test]
    fn prints_version() {
        let Err(err) = Cli::try_parse_from(["agentctl", "--version"]) else {
            panic!("--version parsed as a command");
        };
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn text_arguments_are_joined_and_may_start_with_a_hyphen() {
        match parse(&["post", "--to", "here", "hello", "-", "world"]).unwrap() {
            Command::Post { to, text } => {
                assert_eq!(to, "here");
                assert_eq!(text.join(" "), "hello - world");
            }
            _ => panic!("parsed as another command"),
        }
        match parse(&["private", "--file", "a", "--file", "b", "do", "it"]).unwrap() {
            Command::Private { files, task } => {
                assert_eq!(files, ["a", "b"]);
                assert_eq!(task, ["do", "it"]);
            }
            _ => panic!("parsed as another command"),
        }
        assert!(parse(&["post", "--to", "here"]).is_err());
        assert!(parse(&["ask-agent", "reviewer"]).is_err());
    }

    #[test]
    fn lock_needs_a_command_after_a_double_dash() {
        match parse(&["lock", "--", "git", "commit", "-m", "x"]).unwrap() {
            Command::Lock { timeout, command } => {
                assert_eq!(timeout, DEFAULT_LOCK_TIMEOUT_SECS);
                assert_eq!(command, ["git", "commit", "-m", "x"]);
            }
            _ => panic!("parsed as another command"),
        }
        match parse(&["lock", "--timeout", "5", "--", "true"]).unwrap() {
            Command::Lock { timeout, .. } => assert_eq!(timeout, 5),
            _ => panic!("parsed as another command"),
        }
        assert!(parse(&["lock"]).is_err());
        assert!(parse(&["lock", "git"]).is_err());
    }

    #[test]
    fn react_and_history_take_optional_arguments() {
        assert!(matches!(
            parse(&["react", "eyes"]).unwrap(),
            Command::React { message: None, .. }
        ));
        assert!(matches!(
            parse(&["history", "--before", "3", "--limit", "10"]).unwrap(),
            Command::History {
                before: Some(_),
                limit: Some(10)
            }
        ));
    }

    #[test]
    fn private_files_are_named_relative_to_the_session_directory() {
        let session = Path::new("/volume/sessions/s1");
        let cwd = session.join("work");
        for (file, relative) in [
            ("in.txt", "work/in.txt"),
            ("./out/../in.txt", "work/in.txt"),
            ("../tmp/x.csv", "tmp/x.csv"),
            ("/volume/sessions/s1/home/notes.md", "home/notes.md"),
        ] {
            assert_eq!(in_session(file, &cwd, session).unwrap(), relative, "{file}");
        }
        for (file, reason) in [
            ("../../s2/work/secret", "is not in this session's directory"),
            ("/volume/shared/x", "is not in this session's directory"),
            ("/../../etc/passwd", "is not in this session's directory"),
            ("..", "is a directory, not a file"),
        ] {
            let err = in_session(file, &cwd, session).unwrap_err();
            assert!(err.ends_with(reason), "{file}: {err}");
        }
    }

    #[tokio::test]
    async fn private_files_need_the_session_directory() {
        let env = |name: &str| (name == "AGENTCTL_TOKEN").then(|| "t".to_owned());
        let err = run(
            Command::Private {
                files: vec!["a.txt".to_owned()],
                task: vec!["x".to_owned()],
            },
            &env,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "CLAUDE_CONFIG_DIR is not set");
    }

    #[tokio::test]
    async fn a_missing_token_is_reported_before_any_request() {
        let err = run(
            Command::History {
                before: None,
                limit: None,
            },
            &|_| None,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "AGENTCTL_TOKEN is not set");
    }
}
