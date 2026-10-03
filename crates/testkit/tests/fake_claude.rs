//! Runs `fake-claude` as a child process, found through
//! `testkit::fake_claude_path()`, and checks each of its checks.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use testkit::claude::{
    API_KEY_BETA, CRASH_EXIT_CODE, DEFAULT_MODEL, OAUTH_BETA, REPLY_COST_USD, SCRIPT_ENV,
};
use testkit::{
    FakeAnthropic, TempDir, Turn, fake_anthropic, fake_claude_path, fixtures, write_script,
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const WAIT: Duration = Duration::from_secs(60);

/// Held around every spawn in this binary, and while a test writes an
/// executable that `fake-claude` will run.
///
/// The tests run in parallel. A child starts with a copy of each descriptor
/// open in the test process and holds it until its exec closes it, so a
/// spawn on another thread while a script was open for writing could leave
/// a child holding it, and the script would fail to start with `ETXTBSY`. A
/// spawn returns only once its child has exec'd, so under the lock no
/// pre-exec child is left holding another test's pipes or files. The
/// process sandbox has the same lock for the same reason.
static SPAWNING: Mutex<()> = Mutex::new(());

fn spawning() -> MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One session's directories and environment, as the runner would set them
/// up.
struct Setup {
    dir: TempDir,
    id: Uuid,
    base_url: String,
    env: Vec<(String, String)>,
}

impl Setup {
    fn new(base_url: &str, turns: &[Turn]) -> Self {
        let dir = TempDir::new("testkit-fake-claude");
        for sub in ["claude", "work", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("persona.md"), "You are a test agent.\n").unwrap();
        let setup = Self {
            dir,
            id: Uuid::new_v4(),
            base_url: base_url.to_owned(),
            env: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sub-placeholder".into())],
        };
        setup.script(turns);
        setup
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn script(&self, turns: &[Turn]) {
        write_script(&self.path("script.json"), turns).unwrap();
    }

    fn transcript(&self) -> PathBuf {
        self.path("claude")
            .join("projects")
            .join(self.id.to_string())
            .join(format!("{}.jsonl", self.id))
    }

    fn with_env(mut self, name: &str, value: &str) -> Self {
        self.env.retain(|(key, _)| key != name);
        self.env.push((name.into(), value.into()));
        self
    }

    fn without_env(mut self, name: &str) -> Self {
        self.env.retain(|(key, _)| key != name);
        self
    }

    /// The design's launch flags for this session.
    fn flags(&self, session: &str) -> Vec<String> {
        let persona = self.path("persona.md");
        let mut flags: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "Bash,Read,Edit,Write,Glob,Grep,Skill",
            "--strict-mcp-config",
            "--setting-sources",
            "user",
            "--permission-mode",
            "bypassPermissions",
            "--append-system-prompt-file",
        ]
        .map(String::from)
        .to_vec();
        flags.push(persona.display().to_string());
        flags.push(session.to_owned());
        flags.push(self.id.to_string());
        flags
    }

    fn command(&self, args: &[String]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(fake_claude_path());
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .args(args)
            .current_dir(self.path("work"))
            .env_clear()
            .env("PATH", path)
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("CLAUDE_CODE_PROJECT_DIR_NAME", self.id.to_string())
            .env("ANTHROPIC_BASE_URL", &self.base_url)
            .env(SCRIPT_ENV, self.path("script.json"))
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    async fn run_args(&self, args: &[String], lines: &[&str]) -> Run {
        Run::new(self.output(args, lines).await)
    }

    async fn output(&self, args: &[String], lines: &[&str]) -> Output {
        let mut child = {
            let _spawning = spawning();
            self.command(args).spawn().unwrap()
        };
        let mut stdin = child.stdin.take().unwrap();
        let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let _ = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
        tokio::time::timeout(WAIT, child.wait_with_output())
            .await
            .expect("fake-claude timed out")
            .unwrap()
    }

    async fn run(&self, session: &str, messages: &[&str]) -> Run {
        let lines: Vec<String> = messages.iter().map(|text| user_line(text)).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.run_args(&self.flags(session), &lines).await
    }
}

fn user_line(text: &str) -> String {
    serde_json::json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
}

struct Run {
    code: Option<i32>,
    raw: Vec<String>,
    lines: Vec<Value>,
    stderr: String,
}

impl Run {
    /// Every stdout line must be JSON.
    fn new(output: Output) -> Self {
        let run = Self::lenient(output);
        assert_eq!(run.lines.len(), run.raw.len(), "a stdout line is not JSON");
        run
    }

    /// Keeps every stdout line in `raw`, and the JSON ones in `lines`.
    fn lenient(output: Output) -> Self {
        Self {
            code: output.status.code(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            ..Self::stdout(&String::from_utf8(output.stdout).unwrap())
        }
    }

    /// The lines of `stdout`, such as a capture, without a process.
    fn stdout(stdout: &str) -> Self {
        let raw: Vec<String> = stdout.lines().map(str::to_owned).collect();
        let lines = raw
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Self {
            code: None,
            raw,
            lines,
            stderr: String::new(),
        }
    }

    fn of_type(&self, kind: &str) -> Vec<&Value> {
        self.lines.iter().filter(|l| l["type"] == kind).collect()
    }

    fn results(&self) -> Vec<&Value> {
        self.of_type("result")
    }

    fn kinds(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| match line["subtype"].as_str() {
                Some(subtype) if line["type"] == "system" => format!("system/{subtype}"),
                _ => line["type"].as_str().unwrap().to_owned(),
            })
            .collect()
    }
}

fn transcript_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn anthropic() -> FakeAnthropic {
    fake_anthropic().await
}

#[tokio::test]
async fn plays_a_turn_with_the_design_flags() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("Hello, world.")]);
    let mut args = setup.flags("--session-id");
    args.extend(["--model".to_owned(), "claude-opus-test".to_owned()]);
    let run = setup.run_args(&args, &[&user_line("Say hello.")]).await;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(
        run.kinds(),
        ["system/init", "assistant", "rate_limit_event", "result"]
    );
    let init = &run.lines[0];
    assert_eq!(init["session_id"], setup.id.to_string());
    assert_eq!(init["model"], "claude-opus-test");
    assert_eq!(
        init["tools"],
        serde_json::json!(["Bash", "Edit", "Glob", "Grep", "Read", "Skill", "Write"])
    );
    assert_eq!(init["permissionMode"], "bypassPermissions");
    assert_eq!(init["apiKeySource"], "none");
    assert_eq!(
        init["cwd"],
        setup
            .path("work")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    let assistant = &run.lines[1];
    assert_eq!(assistant["message"]["content"][0]["text"], "Hello, world.");
    let rate_limit = &run.lines[2];
    assert_eq!(rate_limit["session_id"], setup.id.to_string());
    assert_eq!(rate_limit["rate_limit_info"]["status"], "allowed");
    let result = &run.lines[3];
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "Hello, world.");
    assert_eq!(result["terminal_reason"], "completed");
    assert!(result["api_error_status"].is_null());
    assert_eq!(result["session_id"], setup.id.to_string());

    let requests = api.message_requests().await;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url.query(), Some("beta=true"));
    assert_eq!(request.headers["authorization"], "Bearer sub-placeholder");
    assert_eq!(request.headers["anthropic-beta"], OAUTH_BETA);
    assert_eq!(request.headers["anthropic-version"], "2023-06-01");
    assert_eq!(request.headers["x-app"], "cli");
    assert!(request.headers.get("x-api-key").is_none());
    assert_eq!(
        request.headers["x-claude-code-session-id"],
        setup.id.to_string().as_str()
    );
    let body: Value = request.body_json().unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "claude-opus-test");
    assert_eq!(body["messages"][0]["content"], "Say hello.");

    let entries = transcript_entries(&setup.transcript());
    let types: Vec<&str> = entries
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["user", "assistant"]);
    assert_eq!(entries[0]["message"]["content"], "Say hello.");
    assert_eq!(entries[0]["sessionId"], setup.id.to_string());
    assert_eq!(entries[1]["message"]["content"][0]["text"], "Hello, world.");
}

#[tokio::test]
async fn sends_the_api_key_as_x_api_key_and_prefers_it_like_the_real_cli() {
    let api = anthropic().await;
    for with_token in [false, true] {
        let mut setup = Setup::new(&api.uri(), &[Turn::reply("ok")])
            .with_env("ANTHROPIC_API_KEY", "key-placeholder");
        if !with_token {
            setup = setup.without_env("CLAUDE_CODE_OAUTH_TOKEN");
        }
        let run = setup.run("--session-id", &["hi"]).await;
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert_eq!(run.kinds(), ["system/init", "assistant", "result"]);
        assert_eq!(run.lines[0]["apiKeySource"], "ANTHROPIC_API_KEY");
        assert_eq!(run.lines[0]["model"], DEFAULT_MODEL);
    }
    let requests = api.message_requests().await;
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.headers["x-api-key"], "key-placeholder");
        assert!(request.headers.get("authorization").is_none());
        assert_eq!(request.headers["anthropic-beta"], API_KEY_BETA);
    }
}

#[tokio::test]
async fn usage_errors_exit_with_status_2() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("unused")]);
    let id = setup.id.to_string();
    let base = setup.flags("--session-id");
    let without = |flag: &str, values: usize| {
        let mut args = base.clone();
        let at = args.iter().position(|a| a == flag).unwrap();
        args.drain(at..at + 1 + values);
        args
    };
    let with = |extra: &[&str]| {
        let mut args = base.clone();
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    };
    let replace = |flag: &str, value: &str| {
        let mut args = base.clone();
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1] = value.to_owned();
        args
    };
    let cases = [
        ("unknown flag", with(&["--bogus"])),
        ("both session flags", with(&["--resume", &id])),
        ("neither session flag", without("--session-id", 1)),
        ("no -p", without("-p", 0)),
        ("no --verbose", without("--verbose", 0)),
        ("no input format", without("--input-format", 1)),
        ("text output format", replace("--output-format", "text")),
        ("bad session id", replace("--session-id", "not-a-uuid")),
        ("bad permission mode", replace("--permission-mode", "yolo")),
        (
            "bad setting source",
            replace("--setting-sources", "user,cloud"),
        ),
        (
            "missing persona file",
            replace("--append-system-prompt-file", "/nonexistent/persona.md"),
        ),
    ];
    for (name, args) in cases {
        let run = setup.run_args(&args, &[&user_line("hi")]).await;
        assert_eq!(run.code, Some(2), "{name}: {}", run.stderr);
        assert!(run.lines.is_empty(), "{name}");
        assert!(!run.stderr.is_empty(), "{name}");
    }
    assert!(api.requests().await.is_empty());
    assert!(!setup.transcript().exists());
}

#[tokio::test]
async fn session_id_refuses_an_existing_transcript() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("unused")]);
    std::fs::create_dir_all(setup.transcript().parent().unwrap()).unwrap();
    std::fs::write(setup.transcript(), "").unwrap();
    let run = setup.run("--session-id", &["hi"]).await;
    assert_eq!(run.code, Some(1));
    assert!(run.lines.is_empty());
    assert_eq!(
        run.stderr.trim(),
        format!("Error: Session ID {} is already in use.", setup.id)
    );
    assert!(api.requests().await.is_empty());
}

#[tokio::test]
async fn resume_refuses_a_missing_transcript_like_the_real_cli() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("unused")]);
    let run = setup.run("--resume", &["hi"]).await;
    assert_eq!(run.code, Some(1));
    assert_eq!(
        run.stderr.trim(),
        format!("No conversation found with session ID: {}", setup.id)
    );
    assert_eq!(run.kinds(), ["result"]);
    assert_eq!(run.lines[0]["subtype"], "error_during_execution");
    assert_eq!(run.lines[0]["is_error"], true);
    assert!(api.requests().await.is_empty());
    assert!(!setup.transcript().exists());
}

#[tokio::test]
async fn a_resumed_session_continues_the_script_and_the_transcript() {
    let api = anthropic().await;
    let setup = Setup::new(
        &api.uri(),
        &[
            Turn::reply("first"),
            Turn::reply("second"),
            Turn::reply("third"),
        ],
    );
    let run = setup.run("--session-id", &["one"]).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.results()[0]["result"], "first");

    let run = setup.run("--resume", &["two", "three"]).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(
        run.kinds(),
        [
            "system/init",
            "assistant",
            "rate_limit_event",
            "result",
            "system/init",
            "assistant",
            "result"
        ]
    );
    let replies: Vec<&Value> = run.results().iter().map(|r| &r["result"]).collect();
    assert_eq!(replies, ["second", "third"]);
    let totals: Vec<f64> = run
        .results()
        .iter()
        .map(|r| r["total_cost_usd"].as_f64().unwrap())
        .collect();
    assert_eq!(
        totals,
        [REPLY_COST_USD, 2.0 * REPLY_COST_USD],
        "the cost is the process's running total, from 0 on a resumed process"
    );

    let entries = transcript_entries(&setup.transcript());
    let users: Vec<&Value> = entries
        .iter()
        .filter(|e| e["type"] == "user")
        .map(|e| &e["message"]["content"])
        .collect();
    assert_eq!(users, ["one", "two", "three"]);
    assert_eq!(api.message_requests().await.len(), 3);
}

#[tokio::test]
async fn http_failures_become_error_results_and_use_up_the_turn() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("event: message_start\ndata: {}\n\n", "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&upstream)
        .await;
    let turns: Vec<Turn> = ["a", "b", "c", "d"].map(Turn::reply).to_vec();
    let setup = Setup::new(&upstream.uri(), &turns);
    let run = setup.run("--session-id", &["1", "2", "3", "4"]).await;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let results = run.results();
    assert_eq!(results.len(), 4);
    for (result, status) in results.iter().zip([Some(429_u64), Some(401), None]) {
        assert_eq!(result["is_error"], true);
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["terminal_reason"], "api_error");
        assert_eq!(result["api_error_status"].as_u64(), status);
    }
    assert!(
        results[2]["result"]
            .as_str()
            .unwrap()
            .contains("before message_stop")
    );
    assert_eq!(results[3]["is_error"], false);
    assert_eq!(results[3]["result"], "d");
    let errors: Vec<&Value> = run
        .of_type("assistant")
        .into_iter()
        .filter(|a| a["is_api_error_message"] == true)
        .collect();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0]["error"], "rate_limit");
    assert_eq!(errors[1]["error"], "authentication_failed");
    assert_eq!(errors[0]["message"]["model"], "<synthetic>");
}

#[tokio::test]
async fn an_unreachable_base_url_or_no_credential_is_an_error_result() {
    // Bound but never listening: the port stays taken until `held` is
    // dropped, so no server started by a parallel test can get it, and a
    // connection to it is refused at once.
    let held = tokio::net::TcpSocket::new_v4().unwrap();
    held.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let closed = format!("http://{}", held.local_addr().unwrap());
    let setup = Setup::new(&closed, &[Turn::reply("unused")]);
    let run = setup.run("--session-id", &["hi"]).await;
    drop(held);
    assert_eq!(run.code, Some(1));
    let result = run.results()[0];
    assert_eq!(result["is_error"], true);
    assert!(result["api_error_status"].is_null());
    let text = result["result"].as_str().unwrap();
    assert!(text.starts_with("API Error: request failed"), "{text}");

    let api = anthropic().await;
    let setup =
        Setup::new(&api.uri(), &[Turn::reply("unused")]).without_env("CLAUDE_CODE_OAUTH_TOKEN");
    let run = setup.run("--session-id", &["hi"]).await;
    assert_eq!(run.code, Some(1));
    assert!(
        run.results()[0]["result"]
            .as_str()
            .unwrap()
            .contains("Not logged in")
    );
    assert!(api.requests().await.is_empty());
}

#[tokio::test]
async fn scripted_errors_delays_and_the_end_of_the_script() {
    let api = anthropic().await;
    let setup = Setup::new(
        &api.uri(),
        &[
            Turn::api_error(429, "usage limit reached"),
            Turn::reply("late").with_delay(Duration::from_millis(300)),
        ],
    );
    let started = Instant::now();
    let run = setup.run("--session-id", &["1", "2", "3"]).await;
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let results = run.results();
    assert_eq!(results[0]["is_error"], true);
    assert_eq!(results[0]["api_error_status"], 429);
    assert_eq!(results[0]["result"], "usage limit reached");
    assert_eq!(results[1]["is_error"], false);
    assert_eq!(results[1]["result"], "late");
    assert_eq!(results[2]["is_error"], true);
    assert!(results[2]["result"].as_str().unwrap().contains("no turn 2"));
    assert_eq!(api.message_requests().await.len(), 3);
}

#[tokio::test]
async fn a_crash_exits_mid_turn_without_a_result() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("before"), Turn::crash()]);
    let run = setup.run("--session-id", &["1", "2"]).await;
    assert_eq!(run.code, Some(CRASH_EXIT_CODE));
    assert_eq!(
        run.kinds(),
        [
            "system/init",
            "assistant",
            "rate_limit_event",
            "result",
            "system/init"
        ]
    );
    assert_eq!(api.message_requests().await.len(), 2);
}

#[tokio::test]
async fn scripted_commands_run_on_path_with_the_fake_environment() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[]).with_env("AGENTCTL_TOKEN", "ctl-token");
    let agentctl = setup.path("bin").join("agentctl");
    let log = setup.path("agentctl.log");
    {
        let _spawning = spawning();
        std::fs::write(
            &agentctl,
            format!(
                "#!/bin/sh\necho \"$AGENTCTL_TOKEN $*\" >> '{}'\necho reacted\necho note >&2\n",
                log.display()
            ),
        )
        .unwrap();
        make_executable(&agentctl);
    }
    setup.script(&[Turn::reply("done")
        .with_command(["agentctl", "react", "eyes"])
        .with_command(["sh", "-c", "echo failing; exit 3"])
        .with_command(["no-such-program-for-fake-claude"])
        .with_command(Vec::<String>::new())]);

    let run = setup.run("--session-id", &["go"]).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "ctl-token react eyes\n"
    );
    assert_eq!(
        run.kinds(),
        [
            "system/init",
            "assistant",
            "rate_limit_event",
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "result",
        ]
    );
    let uses = run.of_type("assistant");
    let tool_use = &uses[0]["message"]["content"][0];
    assert_eq!(tool_use["type"], "tool_use");
    assert_eq!(tool_use["name"], "Bash");
    assert_eq!(tool_use["input"]["command"], "agentctl react eyes");
    let results: Vec<&Value> = run
        .of_type("user")
        .into_iter()
        .map(|u| &u["message"]["content"][0])
        .collect();
    assert_eq!(results[0]["tool_use_id"], tool_use["id"]);
    assert_eq!(results[0]["type"], "tool_result");
    assert_eq!(results[0]["content"], "reacted\nnote");
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[1]["content"], "Exit code 3\nfailing");
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(results[2]["is_error"], true);
    assert!(
        results[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("no-such-program-for-fake-claude:")
    );
    assert_eq!(results[3]["content"], "empty command");
    assert_eq!(
        run.of_type("user")[0]["tool_use_result"]["stdout"],
        "reacted\n"
    );
    assert_eq!(run.results()[0]["result"], "done");
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn other_line_types_are_ignored_and_bad_input_is_fatal() {
    let api = anthropic().await;
    let setup = Setup::new(&api.uri(), &[Turn::reply("only")]);
    let control = r#"{"type":"control_request","request":{}}"#;
    let run = setup
        .run_args(
            &setup.flags("--session-id"),
            &[control, "", &user_line("hi"), "{not json"],
        )
        .await;
    assert_eq!(run.code, Some(1));
    assert_eq!(
        run.kinds(),
        ["system/init", "assistant", "rate_limit_event", "result"]
    );
    assert!(
        run.stderr.contains("stdin line is not JSON"),
        "{}",
        run.stderr
    );
}

#[tokio::test]
async fn missing_environment_and_a_bad_script_are_fatal() {
    let api = anthropic().await;
    for name in [
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_PROJECT_DIR_NAME",
        "ANTHROPIC_BASE_URL",
        SCRIPT_ENV,
    ] {
        let setup = Setup::new(&api.uri(), &[Turn::reply("unused")]);
        let mut command = setup.command(&setup.flags("--session-id"));
        command.env_remove(name);
        let output = tokio::time::timeout(WAIT, command.output())
            .await
            .unwrap()
            .unwrap();
        let run = Run::new(output);
        assert_eq!(run.code, Some(1), "{name}");
        assert_eq!(run.stderr.trim(), format!("fake-claude: {name} is not set"));
    }

    let setup = Setup::new(&api.uri(), &[]);
    std::fs::write(setup.path("script.json"), r#"[{"reply":"x","typo":1}]"#).unwrap();
    let run = setup.run("--session-id", &["hi"]).await;
    assert_eq!(run.code, Some(1));
    assert_eq!(run.kinds(), ["system/init"]);
    assert!(run.stderr.contains("reading the script"), "{}", run.stderr);
}

#[tokio::test]
async fn a_tool_turn_then_a_reply_prints_the_captured_line_sequence() {
    let api = anthropic().await;
    let setup = Setup::new(
        &api.uri(),
        &[
            Turn::reply("Hello from the fake.").with_command(["echo", "hi"]),
            Turn::reply("Again."),
        ],
    );
    let run = setup.run("--session-id", &["one", "two"]).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    let captured = Run::stdout(fixtures::TOOL_TURNS);
    assert_eq!(run.kinds(), captured.kinds());
    let keys = |run: &Run| -> Vec<String> {
        let line = run.of_type("rate_limit_event")[0];
        let mut keys: Vec<String> = line.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&run), keys(&captured));
    assert_eq!(
        run.of_type("rate_limit_event")[0]["rate_limit_info"],
        captured.of_type("rate_limit_event")[0]["rate_limit_info"]
    );
}

#[tokio::test]
async fn extra_lines_are_printed_verbatim_before_the_reply() {
    let api = anthropic().await;
    let goal = r#"{"type":"active_goal","goal":null}"#;
    let setup = Setup::new(
        &api.uri(),
        &[
            Turn::reply("ok")
                .with_extra_line(goal)
                .with_extra_line("{not json"),
            Turn::api_error(429, "limit").with_extra_line("plain text"),
        ],
    );
    let output = setup
        .output(
            &setup.flags("--session-id"),
            &[&user_line("one"), &user_line("two")],
        )
        .await;
    let run = Run::lenient(output);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let shape: Vec<String> = run
        .raw
        .iter()
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(value) if value["type"] == "active_goal" => line.clone(),
            Ok(value) => value["type"].as_str().unwrap().to_owned(),
            Err(_) => line.clone(),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "system",
            goal,
            "{not json",
            "assistant",
            "rate_limit_event",
            "result",
            "system",
            "plain text",
            "assistant",
            "result",
        ]
    );
    let entries = transcript_entries(&setup.transcript());
    assert!(entries.iter().all(|e| e["type"] != "active_goal"));
    assert_eq!(entries.len(), 4);
}
