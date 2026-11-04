//! A stand-in for the Claude Code CLI in tests. The `testkit::claude` module
//! documents its flags, checks and output.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::{ArgGroup, CommandFactory, Parser, error::ErrorKind};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use testkit::claude::{API_KEY_BETA, CRASH_EXIT_CODE, DEFAULT_MODEL, OAUTH_BETA, SCRIPT_ENV, Turn};
use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};
use uuid::Uuid;

const VERSION: &str = "2.1.285-fake";
const DEFAULT_TOOLS: &str = "Bash,Edit,Glob,Grep,Read,Write";
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(name = "claude", about = "A stand-in for the Claude Code CLI.")]
#[command(group(ArgGroup::new("session").required(true).args(["session_id", "resume"])))]
struct Args {
    #[arg(short = 'p', long = "print")]
    print: bool,
    #[arg(long, value_parser = ["stream-json"])]
    input_format: Option<String>,
    #[arg(long, value_parser = ["stream-json"])]
    output_format: Option<String>,
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    tools: Option<String>,
    #[arg(long)]
    strict_mcp_config: bool,
    #[arg(long, value_parser = parse_setting_sources)]
    setting_sources: Option<String>,
    #[arg(long, value_parser = ["acceptEdits", "auto", "bypassPermissions", "default", "dontAsk", "plan"])]
    permission_mode: Option<String>,
    #[arg(long)]
    append_system_prompt_file: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    session_id: Option<Uuid>,
    #[arg(long)]
    resume: Option<Uuid>,
}

fn parse_setting_sources(value: &str) -> Result<String, String> {
    let known = ["user", "project", "local"];
    match value
        .split(',')
        .all(|source| known.contains(&source.trim()))
    {
        true => Ok(value.to_owned()),
        false => Err(format!("expected a comma-separated list of {known:?}")),
    }
}

impl Args {
    fn check(&self) {
        let usage = |message: &str| {
            Args::command()
                .error(ErrorKind::MissingRequiredArgument, message)
                .exit()
        };
        if !self.print {
            usage("-p is required in this mode");
        }
        if self.input_format.is_none() || self.output_format.is_none() {
            usage("--input-format stream-json and --output-format stream-json are required");
        }
        if !self.verbose {
            usage("--output-format=stream-json requires --verbose");
        }
        if let Some(path) = &self.append_system_prompt_file
            && let Err(err) = std::fs::read(path)
        {
            Args::command()
                .error(
                    ErrorKind::InvalidValue,
                    format!(
                        "cannot read --append-system-prompt-file {}: {err}",
                        path.display()
                    ),
                )
                .exit();
        }
    }
}

enum Credential {
    ApiKey(SecretString),
    OAuth(SecretString),
    Missing,
}

impl Credential {
    fn from_env() -> Self {
        let var = |name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(SecretString::from)
        };
        match (var("ANTHROPIC_API_KEY"), var("CLAUDE_CODE_OAUTH_TOKEN")) {
            (Some(key), _) => Self::ApiKey(key),
            (None, Some(token)) => Self::OAuth(token),
            (None, None) => Self::Missing,
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "ANTHROPIC_API_KEY",
            Self::OAuth(_) | Self::Missing => "none",
        }
    }
}

struct Session {
    id: Uuid,
    model: String,
    tools: Vec<String>,
    permission_mode: String,
    cwd: String,
    transcript: PathBuf,
    script: PathBuf,
    base_url: String,
    credential: Credential,
    http: reqwest::Client,
    rate_limit_reported: Cell<bool>,
}

struct ApiError {
    status: Option<u16>,
    kind: &'static str,
    message: String,
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    args.check();
    match run(args).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("fake-claude: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode, String> {
    let resuming = args.resume.is_some();
    let Some(id) = args.session_id.or(args.resume) else {
        return Err("no session id".into());
    };
    let config_dir = PathBuf::from(required_env("CLAUDE_CONFIG_DIR")?);
    let project = required_env("CLAUDE_CODE_PROJECT_DIR_NAME")?;
    let transcript = config_dir
        .join("projects")
        .join(&project)
        .join(format!("{id}.jsonl"));
    match (resuming, transcript.exists()) {
        (false, true) => {
            eprintln!("Error: Session ID {id} is already in use.");
            return Ok(ExitCode::FAILURE);
        }
        (true, false) => {
            eprintln!("No conversation found with session ID: {id}");
            emit(&json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "num_turns": 0,
                "session_id": id,
                "total_cost_usd": 0,
                "usage": usage(0, 0),
                "errors": [format!("No conversation found with session ID: {id}")],
                "uuid": Uuid::new_v4(),
            }))?;
            return Ok(ExitCode::FAILURE);
        }
        _ => {}
    }
    let mut tools: Vec<String> = args
        .tools
        .as_deref()
        .unwrap_or(DEFAULT_TOOLS)
        .split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(str::to_owned)
        .collect();
    tools.sort();
    let session = Session {
        id,
        model: args.model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        tools,
        permission_mode: args.permission_mode.unwrap_or_else(|| "default".to_owned()),
        cwd: std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
        transcript,
        script: PathBuf::from(required_env(SCRIPT_ENV)?),
        base_url: required_env("ANTHROPIC_BASE_URL")?,
        credential: Credential::from_env(),
        http: reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|err| format!("building the HTTP client: {err}"))?,
        rate_limit_reported: Cell::new(false),
    };

    let mut lines = AsyncBufReader::new(tokio::io::stdin()).lines();
    let mut failed = false;
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|err| format!("reading stdin: {err}"))?
    {
        if line.trim().is_empty() {
            continue;
        }
        let input: Value =
            serde_json::from_str(&line).map_err(|err| format!("stdin line is not JSON: {err}"))?;
        if input["type"] == "user" {
            failed = session.turn(input["message"]["content"].clone()).await?;
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

impl Session {
    /// Plays one turn and returns whether its result was an error.
    async fn turn(&self, content: Value) -> Result<bool, String> {
        let started = Instant::now();
        emit(&json!({
            "type": "system",
            "subtype": "init",
            "cwd": self.cwd,
            "session_id": self.id,
            "tools": self.tools,
            "mcp_servers": [],
            "model": self.model,
            "permissionMode": self.permission_mode,
            "apiKeySource": self.credential.source(),
            "claude_code_version": VERSION,
            "uuid": Uuid::new_v4(),
        }))?;
        let index = self.user_turns()?;
        self.record(json!({
            "type": "user",
            "message": {"role": "user", "content": content},
        }))?;

        if let Err(err) = self.call_api(&content).await {
            let text = format!("API Error: {}", err.message);
            return self.fail(&text, err.kind, err.status, started);
        }
        let turns: Vec<Turn> = std::fs::read(&self.script)
            .map_err(|err| err.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|err| err.to_string()))
            .map_err(|err| format!("reading the script {}: {err}", self.script.display()))?;
        let Some(turn) = turns.get(index) else {
            let text = format!("fake-claude: the script has no turn {index}");
            return self.fail(&text, "invalid_request", None, started);
        };
        if let Some(ms) = turn.delay_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if turn.crash {
            std::process::exit(CRASH_EXIT_CODE);
        }
        for argv in &turn.commands {
            self.run_command(argv).await?;
        }
        for line in &turn.extra_lines {
            emit_raw(line)?;
        }
        if turn.is_error {
            let kind = match turn.api_error_status {
                Some(401 | 403) => "authentication_failed",
                Some(429) => "rate_limit",
                _ => "server_error",
            };
            return self.fail(&turn.reply, kind, turn.api_error_status, started);
        }
        let message = json!({
            "id": format!("msg_fake_{}", Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": [{"type": "text", "text": turn.reply}],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": usage(10, 1),
        });
        self.record(json!({"type": "assistant", "message": message}))?;
        self.emit_reply(message)?;
        self.emit_result(&turn.reply, false, None, "end_turn", started)?;
        Ok(false)
    }

    fn user_turns(&self) -> Result<usize, String> {
        let file = match File::open(&self.transcript) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(format!("reading the transcript: {err}")),
        };
        let mut count = 0;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|err| format!("reading the transcript: {err}"))?;
            if serde_json::from_str::<Value>(&line).is_ok_and(|entry| entry["type"] == "user") {
                count += 1;
            }
        }
        Ok(count)
    }

    fn record(&self, mut entry: Value) -> Result<(), String> {
        entry["sessionId"] = json!(self.id);
        entry["uuid"] = json!(Uuid::new_v4());
        entry["timestamp"] = json!(now());
        let write = || -> io::Result<()> {
            if let Some(dir) = self.transcript.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.transcript)?;
            writeln!(file, "{entry}")
        };
        write().map_err(|err| format!("writing the transcript: {err}"))
    }

    async fn call_api(&self, content: &Value) -> Result<(), ApiError> {
        let url = format!(
            "{}/v1/messages?beta=true",
            self.base_url.trim_end_matches('/')
        );
        let request = self
            .http
            .post(url)
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", format!("claude-cli/{VERSION} (fake-claude)"))
            .header("x-app", "cli")
            .header("x-claude-code-session-id", self.id.to_string())
            .json(&json!({
                "model": self.model,
                "max_tokens": 1024,
                "stream": true,
                "messages": [{"role": "user", "content": content}],
            }));
        let request = match &self.credential {
            Credential::ApiKey(key) => request
                .header("x-api-key", key.expose_secret())
                .header("anthropic-beta", API_KEY_BETA),
            Credential::OAuth(token) => request
                .bearer_auth(token.expose_secret())
                .header("anthropic-beta", OAUTH_BETA),
            Credential::Missing => {
                return Err(ApiError {
                    status: None,
                    kind: "authentication_failed",
                    message: "Not logged in: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN"
                        .into(),
                });
            }
        };
        let transport = |err: reqwest::Error| ApiError {
            status: None,
            kind: "server_error",
            message: format!("request failed: {err}"),
        };
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let sse = response
            .headers()
            .get("content-type")
            .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"));
        let body = response.text().await.map_err(transport)?;
        if status != reqwest::StatusCode::OK {
            return Err(ApiError {
                status: Some(status.as_u16()),
                kind: match status.as_u16() {
                    401 | 403 => "authentication_failed",
                    429 => "rate_limit",
                    _ => "server_error",
                },
                message: format!("{}", status.as_u16()),
            });
        }
        if sse && !body.contains("event: message_stop") {
            return Err(ApiError {
                status: None,
                kind: "server_error",
                message: "the stream ended before message_stop".into(),
            });
        }
        Ok(())
    }

    async fn run_command(&self, argv: &[String]) -> Result<(), String> {
        let tool_id = format!("toolu_fake_{}", Uuid::new_v4().simple());
        self.emit_reply(json!({
            "id": format!("msg_fake_{}", Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": [{
                "type": "tool_use",
                "id": tool_id,
                "name": "Bash",
                "input": {"command": argv.join(" "), "description": "Run a scripted command"},
            }],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": usage(10, 1),
        }))?;
        let (stdout, stderr, code) = match argv.split_first() {
            None => (String::new(), "empty command".to_owned(), None),
            Some((program, rest)) => match tokio::process::Command::new(program)
                .args(rest)
                .stdin(Stdio::null())
                .output()
                .await
            {
                Ok(output) => (
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                    output.status.code(),
                ),
                Err(err) => (String::new(), format!("{program}: {err}"), None),
            },
        };
        let is_error = code != Some(0);
        let combined = format!("{stdout}{stderr}");
        let combined = combined.trim_end();
        let content = match (is_error, code) {
            (false, _) => combined.to_owned(),
            (true, Some(code)) => format!("Exit code {code}\n{combined}"),
            (true, None) => combined.to_owned(),
        };
        self.emit_message(
            "user",
            json!({
                "role": "user",
                "content": [{
                    "tool_use_id": tool_id,
                    "type": "tool_result",
                    "content": content,
                    "is_error": is_error,
                }],
            }),
            json!({
                "tool_use_result": {
                    "stdout": stdout,
                    "stderr": stderr,
                    "interrupted": false,
                    "isImage": false,
                },
            }),
        )
    }

    fn fail(
        &self,
        text: &str,
        kind: &str,
        status: Option<u16>,
        started: Instant,
    ) -> Result<bool, String> {
        let message = json!({
            "id": Uuid::new_v4(),
            "type": "message",
            "role": "assistant",
            "model": "<synthetic>",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "stop_sequence",
            "stop_sequence": "",
            "usage": usage(0, 0),
        });
        self.record(json!({"type": "assistant", "message": message}))?;
        self.emit_message(
            "assistant",
            message,
            json!({"error": kind, "is_api_error_message": true}),
        )?;
        self.emit_result(text, true, status, "stop_sequence", started)?;
        Ok(true)
    }

    /// Prints an `assistant` line of a successful API answer, and after the
    /// first one in the process a `rate_limit_event` if the real CLI would.
    fn emit_reply(&self, message: Value) -> Result<(), String> {
        self.emit_message("assistant", message, json!({}))?;
        if matches!(self.credential, Credential::OAuth(_))
            && !self.rate_limit_reported.replace(true)
        {
            emit(&json!({
                "type": "rate_limit_event",
                "rate_limit_info": {"status": "allowed", "isUsingOverage": false},
                "uuid": Uuid::new_v4(),
                "session_id": self.id,
            }))?;
        }
        Ok(())
    }

    fn emit_message(&self, kind: &str, message: Value, extra: Value) -> Result<(), String> {
        let mut line = json!({
            "type": kind,
            "message": message,
            "parent_tool_use_id": null,
            "session_id": self.id,
            "uuid": Uuid::new_v4(),
        });
        if let (Some(line), Value::Object(extra)) = (line.as_object_mut(), extra) {
            line.extend(extra);
        }
        emit(&line)
    }

    fn emit_result(
        &self,
        text: &str,
        is_error: bool,
        status: Option<u16>,
        stop_reason: &str,
        started: Instant,
    ) -> Result<(), String> {
        let (input, output) = if is_error { (0, 0) } else { (10, 1) };
        emit(&json!({
            "type": "result",
            "subtype": "success",
            "is_error": is_error,
            "result": text,
            "session_id": self.id,
            "total_cost_usd": 0,
            "usage": usage(input, output),
            "terminal_reason": if is_error { "api_error" } else { "completed" },
            "api_error_status": status,
            "stop_reason": stop_reason,
            "num_turns": 1,
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "uuid": Uuid::new_v4(),
        }))
    }
}

fn usage(input: u64, output: u64) -> Value {
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn emit(line: &Value) -> Result<(), String> {
    emit_raw(&line.to_string())
}

fn emit_raw(line: &str) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")
        .and_then(|()| stdout.flush())
        .map_err(|err| format!("writing stdout: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_sources_accept_known_lists_only() {
        assert!(parse_setting_sources("user").is_ok());
        assert!(parse_setting_sources("user,project, local").is_ok());
        assert!(parse_setting_sources("user,cloud").is_err());
    }

    #[test]
    fn the_command_definition_is_valid() {
        Args::command().debug_assert();
    }
}
