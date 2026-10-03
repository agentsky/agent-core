mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Harness, PLACEHOLDER};
use core_types::CredentialKind;
use runner::{ClaudeProcess, ErrorKind, ProcessConfig, RunnerError, SessionStart, TurnOutcome};
use sandbox::Sandbox;
use secrecy::SecretString;
use testkit::Turn;
use testkit::claude::{CRASH_EXIT_CODE, REPLY_COST_USD};

fn finished(outcome: TurnOutcome) -> runner::TurnResult {
    match outcome {
        TurnOutcome::Finished(result) => result,
        other => panic!("expected a result, got {other:?}"),
    }
}

#[tokio::test]
async fn first_start_uses_session_id_and_later_starts_resume() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two")]).await;
    let mut first = h.start(h.launch(SessionStart::New)).await;
    let result = finished(first.send_turn("hello").await.unwrap());
    assert_eq!(result.result.as_deref(), Some("one"));
    assert_eq!(result.session_id, Some(h.container.session()));
    assert!(result.stats.init_seen);
    first.stop().await;

    let mut refused = h.start(h.launch(SessionStart::New)).await;
    let outcome = refused.send_turn("again").await.unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Crashed { exit_code: Some(1), ref stats } if !stats.init_seen),
        "--session-id on a started session is refused: {outcome:?}"
    );

    let mut resumed = h.start(h.launch(SessionStart::Resume)).await;
    let result = finished(resumed.send_turn("back").await.unwrap());
    assert_eq!(result.result.as_deref(), Some("two"));
    assert_eq!(h.transcript_user_messages(), ["hello", "back"]);
    resumed.stop().await;
}

#[tokio::test]
async fn resuming_a_session_that_never_started_is_refused_and_ends_the_process() {
    let h = Harness::new(&[Turn::reply("fresh")]).await;
    let mut process = h.start(h.launch(SessionStart::Resume)).await;
    let outcome = process.send_turn("hello").await.unwrap();
    assert!(outcome.resume_refused(), "{outcome:?}");
    let result = finished(outcome);
    assert!(result.is_error);
    assert_eq!(result.subtype.as_deref(), Some("error_during_execution"));
    assert!(!result.stats.init_seen);
    assert_eq!(result.error_kind, Some(ErrorKind::Other));
    assert!(!process.is_running(), "the CLI exits after refusing");
    assert!(!process.may_be_alive());
    assert!(matches!(
        process.send_turn("again").await,
        Err(RunnerError::NotRunning)
    ));

    let mut fresh = h.start(h.launch(SessionStart::New)).await;
    let outcome = fresh.send_turn("hello").await.unwrap();
    assert!(!outcome.resume_refused());
    assert_eq!(finished(outcome).result.as_deref(), Some("fresh"));
    assert_eq!(h.transcript_user_messages(), ["hello"]);
    fresh.stop().await;
}

#[tokio::test]
async fn a_result_after_a_failed_write_ends_the_process() {
    let mut h = Harness::new(&[]).await;
    let bin = h._dir.join("closes-stdin");
    let script = format!(
        "#!/bin/sh\nexec 0<&-\nprintf '%s\\n' '{}'\n: > \"$TMPDIR/stdin-closed\"\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"early"}"#
    );
    std::fs::write(&bin, script).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    h.config.claude_bin = bin.to_str().unwrap().to_owned();
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let marker = h.container.paths().tmp.join("stdin-closed");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "the script never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let outcome = process.send_turn("hello").await.unwrap();
    let result = finished(outcome);
    assert_eq!(result.result.as_deref(), Some("early"));
    assert!(
        !process.is_running(),
        "a process that stopped reading is gone"
    );
    assert!(!process.may_be_alive());
    assert!(matches!(
        process.send_turn("again").await,
        Err(RunnerError::NotRunning)
    ));
}

#[tokio::test]
async fn two_turns_on_one_warm_process() {
    let h = Harness::new(&[Turn::reply("first reply"), Turn::reply("second reply")]).await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    assert_eq!(process.session(), h.container.session());
    assert_eq!(process.container(), h.container.id());
    assert_eq!(process.credential(), CredentialKind::Subscription);
    assert_eq!(process.model(), None);
    let one = finished(process.send_turn("one").await.unwrap());
    assert!(process.is_running());
    let two = finished(process.send_turn("two").await.unwrap());
    assert_eq!(one.result.as_deref(), Some("first reply"));
    assert_eq!(two.result.as_deref(), Some("second reply"));
    assert!(!one.is_error && !two.is_error);
    assert_eq!(one.terminal_reason.as_deref(), Some("completed"));
    assert_eq!(one.usage.unwrap().input_tokens, 10);
    assert_eq!(two.usage.unwrap().input_tokens, 10, "usage is per turn");
    assert_eq!(one.cost_usd, Ok(REPLY_COST_USD));
    assert_eq!(
        two.cost_usd,
        Ok(REPLY_COST_USD),
        "the second turn is not billed for the first"
    );
    assert_eq!(two.process_total_cost_usd, Some(2.0 * REPLY_COST_USD));
    assert_eq!(
        one.stats.ignored_lines, 1,
        "the OAuth rate_limit_event of the first turn is skipped"
    );
    assert_eq!(two.stats.ignored_lines, 0);
    assert_eq!(h.anthropic.message_requests().await.len(), 2);
    assert_eq!(h.transcript_user_messages(), ["one", "two"]);
    process.stop().await;
}

#[tokio::test]
async fn a_crash_then_a_resume() {
    let h = Harness::new(&[Turn::reply("before"), Turn::crash(), Turn::reply("after")]).await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    finished(process.send_turn("one").await.unwrap());
    let outcome = process.send_turn("two").await.unwrap();
    let TurnOutcome::Crashed { exit_code, stats } = outcome else {
        panic!("expected a crash, got {outcome:?}");
    };
    assert_eq!(exit_code, Some(CRASH_EXIT_CODE));
    assert!(stats.init_seen, "the CLI read the message before crashing");
    assert!(!process.is_running());
    assert!(matches!(
        process.send_turn("three").await,
        Err(RunnerError::NotRunning)
    ));
    process.stop().await;

    let mut resumed = h.start(h.launch(SessionStart::Resume)).await;
    let result = finished(resumed.send_turn("three").await.unwrap());
    assert_eq!(result.result.as_deref(), Some("after"));
    assert_eq!(
        result.cost_usd,
        Ok(REPLY_COST_USD),
        "a new process counts its cost from 0"
    );
    assert_eq!(h.transcript_user_messages(), ["one", "two", "three"]);
    resumed.stop().await;
}

#[tokio::test]
async fn a_process_that_exits_before_reading_crashes_the_turn() {
    let mut h = Harness::new(&[]).await;
    h.config.claude_bin = "/bin/true".into();
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let outcome = process.send_turn("hello").await.unwrap();
    let TurnOutcome::Crashed { exit_code, stats } = outcome else {
        panic!("expected a crash, got {outcome:?}");
    };
    assert_eq!(exit_code, Some(0));
    assert!(!stats.init_seen);
    process.stop().await;
}

#[tokio::test]
async fn a_timeout_kills_the_process_and_fails_the_turn() {
    let mut h = Harness::new(&[
        Turn::reply("fast"),
        Turn::reply("too late").with_delay(Duration::from_secs(60)),
        Turn::reply("resumed"),
    ])
    .await;
    h.config.turn_timeout_secs = 1;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    finished(process.send_turn("one").await.unwrap());
    let started = std::time::Instant::now();
    let outcome = process.send_turn("two").await.unwrap();
    let elapsed = started.elapsed();
    let TurnOutcome::TimedOut { stats } = outcome else {
        panic!("expected a timeout, got {outcome:?}");
    };
    assert!(stats.init_seen);
    assert!(stats.duration >= Duration::from_secs(1), "{stats:?}");
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
    assert!(!process.is_running());
    assert!(!process.may_be_alive(), "the killed process was reaped");
    assert!(matches!(
        process.send_turn("three").await,
        Err(RunnerError::NotRunning)
    ));
    process.stop().await;

    h.config.turn_timeout_secs = 60;
    let mut resumed = h.start(h.launch(SessionStart::Resume)).await;
    let result = finished(resumed.send_turn("three").await.unwrap());
    assert_eq!(result.result.as_deref(), Some("resumed"));
    resumed.stop().await;
}

#[tokio::test]
async fn a_cancelled_turn_leaves_the_process_unusable() {
    let h = Harness::new(&[Turn::reply("slow").with_delay(Duration::from_secs(60))]).await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), process.send_turn("one")).await;
    assert!(cancelled.is_err());
    assert!(!process.is_running());
    assert!(process.may_be_alive());
    assert!(matches!(
        process.send_turn("two").await,
        Err(RunnerError::NotRunning)
    ));
    assert!(!process.may_be_alive(), "the killed process was reaped");
    assert!(matches!(
        process.send_turn("three").await,
        Err(RunnerError::NotRunning)
    ));
}

#[tokio::test]
async fn is_error_results_are_classified_and_the_process_goes_on() {
    let h = Harness::new(&[
        Turn::api_error(429, "API Error: 429 rate limited"),
        Turn::api_error(401, "Failed to authenticate. API Error: 401"),
        Turn::api_error(403, "API Error: 403 forbidden"),
        Turn::api_error(500, "API Error: 500 internal"),
        Turn::api_error(400, "Credit balance is too low"),
        Turn::reply("fine again"),
    ])
    .await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let expected = [
        (Some(429), Some("rate_limit"), ErrorKind::UsageLimit),
        (Some(401), Some("authentication_failed"), ErrorKind::Auth),
        (Some(403), Some("authentication_failed"), ErrorKind::Auth),
        (Some(500), Some("server_error"), ErrorKind::Other),
        (Some(400), Some("server_error"), ErrorKind::UsageLimit),
    ];
    for (status, code, kind) in expected {
        let result = finished(process.send_turn("go").await.unwrap());
        assert!(result.is_error, "{status:?}");
        assert_eq!(result.api_error_status, status);
        assert_eq!(result.stats.api_error.as_deref(), code);
        assert_eq!(result.terminal_reason.as_deref(), Some("api_error"));
        assert_eq!(result.error_kind, Some(kind), "{status:?}");
        assert!(process.is_running());
    }
    let result = finished(process.send_turn("go").await.unwrap());
    assert!(!result.is_error);
    assert_eq!(result.error_kind, None);
    process.stop().await;
}

#[tokio::test]
async fn an_unreachable_proxy_is_an_error_result() {
    let mut h = Harness::new(&[Turn::reply("unused")]).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    h.config.anthropic_base_url = format!("http://{}", socket.local_addr().unwrap());
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let result = finished(process.send_turn("hello").await.unwrap());
    assert!(result.is_error);
    assert_eq!(result.api_error_status, None);
    assert_eq!(result.error_kind, Some(ErrorKind::Other));
    process.stop().await;
}

#[tokio::test]
async fn unknown_line_types_are_ignored() {
    let h = Harness::new(&[Turn::reply("still here")
        .with_command(["/bin/echo", "tool ran"])
        .with_extra_line(r#"{"type":"active_goal","goal":{"text":"x"}}"#)
        .with_extra_line(r#"{"type":"system","subtype":"commands_changed"}"#)
        .with_extra_line(r#"{"type":"autocompact_state","new_field":[1,2,3]}"#)
        .with_extra_line("[1,2,3]")
        .with_extra_line("this is not json")
        .with_extra_line(r#"{"type":"result","#)])
    .await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let result = finished(process.send_turn("hello").await.unwrap());
    assert_eq!(result.result.as_deref(), Some("still here"));
    assert!(!result.is_error);
    assert_eq!(result.stats.tool_calls, ["Bash"]);
    assert_eq!(result.stats.assistant_messages, 2);
    assert_eq!(
        result.stats.ignored_lines, 5,
        "four extra lines and the rate_limit_event"
    );
    assert_eq!(result.stats.malformed_lines, 2);
    process.stop().await;
}

#[tokio::test]
async fn the_environment_holds_only_the_placeholder() {
    let dump = r#"/usr/bin/env > "$TMPDIR/env.txt""#;
    for credential in [CredentialKind::Subscription, CredentialKind::ApiKey] {
        let h = Harness::new(&[Turn::reply("ok").with_command(["/bin/sh", "-c", dump])]).await;
        let mut spec = h.launch_with(SessionStart::New, credential);
        spec.model = Some("claude-test-model".into());
        let mut process = h.start(spec).await;
        assert_eq!(process.model(), Some("claude-test-model"));
        assert!(process.send_turn("hello").await.unwrap().is_success());
        process.stop().await;

        let paths = h.container.paths();
        let dumped = std::fs::read_to_string(paths.tmp.join("env.txt")).unwrap();
        let env: BTreeMap<&str, &str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let (present, absent) = match credential {
            CredentialKind::Subscription => ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"),
            CredentialKind::ApiKey => ("ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"),
        };
        assert_eq!(env.get(present), Some(&PLACEHOLDER));
        assert!(!env.contains_key(absent), "{credential:?}");
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        let holding: Vec<_> = env
            .iter()
            .filter(|(_, v)| v.contains(PLACEHOLDER))
            .collect();
        assert_eq!(holding.len(), 1, "{holding:?}");
        let session = h.container.session().to_string();
        let anthropic = h.anthropic.uri();
        let expected = [
            ("ANTHROPIC_BASE_URL", anthropic.as_str()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("DISABLE_AUTOUPDATER", "1"),
            ("CLAUDE_CONFIG_DIR", paths.claude_config.to_str().unwrap()),
            ("CLAUDE_CODE_PROJECT_DIR_NAME", session.as_str()),
            ("HOME", paths.home.to_str().unwrap()),
            ("TMPDIR", paths.tmp.to_str().unwrap()),
            ("AGENTCTL_TOKEN", "ctl-token-for-test"),
        ];
        for (key, value) in expected {
            assert_eq!(env.get(key), Some(&value), "{key}");
        }
        for leaked in ["CARGO", "CARGO_MANIFEST_DIR", "RUST_TEST_THREADS", "USER"] {
            assert!(!env.contains_key(leaked), "{leaked} leaked from agentd");
        }
        assert!(!dumped.contains("sk-ant-"));

        let requests = h.anthropic.message_requests().await;
        let [request] = requests.as_slice() else {
            panic!("{} requests", requests.len());
        };
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        match credential {
            CredentialKind::Subscription => {
                assert_eq!(
                    header("authorization"),
                    Some(format!("Bearer {PLACEHOLDER}"))
                );
                assert_eq!(header("x-api-key"), None);
            }
            CredentialKind::ApiKey => {
                assert_eq!(header("x-api-key").as_deref(), Some(PLACEHOLDER));
                assert_eq!(header("authorization"), None);
            }
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "claude-test-model");
    }
}

#[tokio::test]
async fn a_real_credential_cannot_be_passed_in_the_environment() {
    let h = Harness::new(&[]).await;
    for key in [
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        let mut spec = h.launch(SessionStart::New);
        spec.env.insert(
            key.into(),
            SecretString::from("sk-ant-oat01-real-credential"),
        );
        let err = ClaudeProcess::start(&h.sandbox, &h.container, &h.config, spec)
            .await
            .unwrap_err();
        assert!(matches!(err, RunnerError::InvalidSpec(_)), "{key}");
    }
}

#[tokio::test]
async fn start_checks_the_config_and_the_container() {
    let h = Harness::new(&[]).await;
    let bad = ProcessConfig {
        turn_timeout_secs: 0,
        ..h.config.clone()
    };
    let err = ClaudeProcess::start(&h.sandbox, &h.container, &bad, h.launch(SessionStart::New))
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::Config(_)), "{err:?}");

    h.sandbox.stop(h.container.id()).await.unwrap();
    let err = ClaudeProcess::start(
        &h.sandbox,
        &h.container,
        &h.config,
        h.launch(SessionStart::New),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, RunnerError::Sandbox(sandbox::SandboxError::NotFound)),
        "{err:?}"
    );
}

#[tokio::test]
async fn stopping_ends_the_process_and_is_quiet_after_a_crash() {
    let h = Harness::new(&[Turn::reply("one"), Turn::crash()]).await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    finished(process.send_turn("one").await.unwrap());
    let debug = format!("{process:?}");
    assert!(debug.contains("Idle"), "{debug}");
    assert!(process.may_be_alive());
    process.stop().await;
    assert!(!process.is_running());
    assert!(!process.may_be_alive());
    process.stop().await;
    assert!(matches!(
        process.send_turn("two").await,
        Err(RunnerError::NotRunning)
    ));

    let mut crashed = h.start(h.launch(SessionStart::Resume)).await;
    assert!(matches!(
        crashed.send_turn("two").await.unwrap(),
        TurnOutcome::Crashed { .. }
    ));
    crashed.stop().await;
}

#[test]
fn a_process_can_move_between_tasks() {
    fn assert_send<T: Send + 'static>() {}
    assert_send::<ClaudeProcess>();
    assert_send::<TurnOutcome>();
    fn assert_send_future<F: std::future::Future + Send>(_: F) {}
    let _ = |process: &mut ClaudeProcess| assert_send_future(process.send_turn("x"));
    let _ = |process: &mut ClaudeProcess| assert_send_future(process.stop());
}
