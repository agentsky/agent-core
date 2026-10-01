use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::DEFAULT_CLOUD_BETA;
use crate::telemetry::tests::global_logs;

const ROUTINE: &str = "trig_01AbCdEfGh";
const FIRE_PATH: &str = "/v1/claude_code/routines/trig_01AbCdEfGh/fire";
const TOKEN: &str = "sk-ant-oat01-routine-token-7Qz9xMarker";
const TASK: &str = "Fix the flaky test in \"runner\" and open a draft PR.\n\n  - keep it ünïcode";
const SESSION: &str = "session_01HJKmNpQrStUv";

fn config(base: &str) -> CloudConfig {
    CloudConfig {
        base_url: base.to_owned(),
        ..CloudConfig::default()
    }
}

fn client(base: &str) -> FireClient {
    FireClient::build(
        &config(base),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .unwrap()
}

fn routine() -> RoutineId {
    ROUTINE.parse().unwrap()
}

fn token() -> SecretString {
    SecretString::from(TOKEN)
}

async fn fire(base: &str) -> FireOutcome {
    client(base).fire(&routine(), &token(), TASK).await.unwrap()
}

/// Fires once at a fresh fake that answers with `template`, and checks it
/// got exactly one request.
async fn answered(template: ResponseTemplate) -> FireOutcome {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .respond_with(template)
        .expect(1)
        .mount(&server)
        .await;
    let outcome = fire(&server.uri()).await;
    server.verify().await;
    outcome
}

fn envelope(kind: &str) -> serde_json::Value {
    json!({
        "type": "error",
        "error": {"type": kind, "message": "the body's message, never logged"},
    })
}

fn session_body(id: &str, url: Option<&str>) -> serde_json::Value {
    let mut body = json!({"type": "routine_fire", "claude_code_session_id": id});
    if let Some(url) = url {
        body["claude_code_session_url"] = json!(url);
    }
    body
}

fn rejected(status: u16, error_type: Option<&str>, retry_after: Option<u32>) -> FireOutcome {
    FireOutcome::Rejected {
        status: Some(status),
        error_type: error_type.map(str::to_owned),
        retry_after,
    }
}

fn unknown_with(status: Option<u16>, reason: UnknownReason) -> FireOutcome {
    FireOutcome::Unknown { status, reason }
}

fn answer(status: u16, retry_after: Option<&str>, body: Result<Vec<u8>, BodyError>) -> Exchange {
    Exchange::Answered(Answer {
        status,
        retry_after: retry_after.map(str::to_owned),
        body,
    })
}

#[tokio::test]
async fn fire_sends_the_documented_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = fire(&server.uri()).await;
    assert_eq!(outcome.kind(), "fired");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url.path(), FIRE_PATH);
    assert_eq!(request.url.query(), None);
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    assert_eq!(header("authorization"), Some(format!("Bearer {TOKEN}")));
    assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_eq!(
        header("anthropic-beta").as_deref(),
        Some("experimental-cc-routine-2026-04-01")
    );
    assert_eq!(DEFAULT_CLOUD_BETA, "experimental-cc-routine-2026-04-01");
    assert_eq!(header("content-type").as_deref(), Some("application/json"));
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body, json!({"text": TASK}));

    assert!(!request.url.as_str().contains(TOKEN));
    assert!(!String::from_utf8_lossy(&request.body).contains(TOKEN));
    for (name, value) in &request.headers {
        if name != "authorization" {
            assert!(
                !String::from_utf8_lossy(value.as_bytes()).contains(TOKEN),
                "{name}"
            );
        }
    }
}

#[tokio::test]
async fn a_custom_beta_header_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .and(wiremock::matchers::header(
            "anthropic-beta",
            "experimental-cc-routine-2027-01-01",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(1)
        .mount(&server)
        .await;
    let config = CloudConfig {
        beta: "experimental-cc-routine-2027-01-01".to_owned(),
        ..config(&server.uri())
    };
    let outcome = FireClient::new(&config)
        .unwrap()
        .fire(&routine(), &token(), TASK)
        .await
        .unwrap();
    assert_eq!(outcome.kind(), "fired");
    server.verify().await;
}

#[tokio::test]
async fn fire_reads_the_session_id_and_url() {
    let url = format!("{SESSION_URL_PREFIX}{SESSION}");
    let outcome =
        answered(ResponseTemplate::new(200).set_body_json(session_body(SESSION, Some(&url)))).await;
    assert_eq!(
        outcome,
        FireOutcome::Fired {
            session_id: SESSION.to_owned(),
            session_url: Some(format!("https://claude.ai/code/{SESSION}")),
        }
    );
    assert_eq!(outcome.status(), Some(200));

    let longest = format!("session_{}", "a".repeat(128));
    let outcome = classify(answer(
        200,
        None,
        Ok(session_body(&longest, None).to_string().into_bytes()),
    ));
    assert_eq!(
        outcome,
        FireOutcome::Fired {
            session_id: longest,
            session_url: None,
        }
    );
}

#[tokio::test]
async fn a_session_url_elsewhere_falls_back_to_the_id() {
    for url in [
        json!(format!("https://evil.example/code/{SESSION}")),
        json!(format!("https://claude.ai/code/{SESSION}/extra")),
        json!(format!("https://claude.ai/code/{SESSION}?x=1")),
        json!("https://claude.ai/code/session_SomeoneElse"),
        json!(format!("http://claude.ai/code/{SESSION}")),
        json!(format!("https://claude.ai.evil.example/code/{SESSION}")),
        json!(format!("https://CLAUDE.ai/code/{SESSION}")),
        json!(""),
        json!(7),
        json!(null),
    ] {
        let mut body = session_body(SESSION, None);
        body["claude_code_session_url"] = url.clone();
        let outcome = classify(answer(200, None, Ok(body.to_string().into_bytes())));
        assert_eq!(
            outcome,
            FireOutcome::Fired {
                session_id: SESSION.to_owned(),
                session_url: None,
            },
            "{url}"
        );
    }
    let outcome = answered(
        ResponseTemplate::new(200)
            .set_body_json(session_body(SESSION, Some("https://evil.example/phish"))),
    )
    .await;
    assert_eq!(
        outcome,
        FireOutcome::Fired {
            session_id: SESSION.to_owned(),
            session_url: None,
        }
    );
}

#[tokio::test]
async fn each_documented_4xx_is_rejected_with_its_type() {
    for (status, kind) in [
        (400, "invalid_request_error"),
        (401, "authentication_error"),
        (403, "permission_error"),
        (404, "not_found_error"),
        (429, "rate_limit_error"),
    ] {
        let outcome = answered(ResponseTemplate::new(status).set_body_json(envelope(kind))).await;
        assert_eq!(outcome, rejected(status, Some(kind), None), "{status}");
        assert_eq!(outcome.kind(), "rejected");
        assert_eq!(outcome.status(), Some(status));
    }

    let lenient = [
        (400, b"not json at all".to_vec()),
        (401, Vec::new()),
        (403, br#"{"error": "permission_error"}"#.to_vec()),
        (404, br#"{"error": {"type": "Not Found!"}}"#.to_vec()),
        (429, br#"{"error": {"type": 7}}"#.to_vec()),
        (400, br#"{"type": "invalid_request_error"}"#.to_vec()),
        (401, br#"{"error": {"type": ""}}"#.to_vec()),
        (
            403,
            format!(r#"{{"error": {{"type": "{}"}}}}"#, "a".repeat(65)).into_bytes(),
        ),
    ];
    for (status, body) in lenient {
        let shown = String::from_utf8_lossy(&body).into_owned();
        let outcome = classify(answer(status, None, Ok(body)));
        assert_eq!(outcome, rejected(status, None, None), "{status} {shown}");
    }
    for error in [BodyError::TooLarge, BodyError::TimedOut, BodyError::Lost] {
        let outcome = classify(answer(401, None, Err(error)));
        assert_eq!(outcome, rejected(401, None, None), "{error:?}");
    }
    let longest = "a_1".repeat(21) + "z";
    let body = envelope(&longest).to_string().into_bytes();
    assert_eq!(
        classify(answer(400, None, Ok(body))),
        rejected(400, Some(&longest), None)
    );

    let unbreakable = answered(ResponseTemplate::new(400).set_body_string("<html>no</html>")).await;
    assert_eq!(unbreakable, rejected(400, None, None));
}

#[tokio::test]
async fn retry_after_is_kept_in_seconds_and_a_date_is_ignored() {
    let outcome = answered(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "120")
            .set_body_json(envelope("rate_limit_error")),
    )
    .await;
    assert_eq!(outcome, rejected(429, Some("rate_limit_error"), Some(120)));

    let outcome = answered(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")
            .set_body_json(envelope("rate_limit_error")),
    )
    .await;
    assert_eq!(outcome, rejected(429, Some("rate_limit_error"), None));

    for (value, seconds) in [
        ("0", Some(0)),
        ("3600", Some(3600)),
        (" 30 ", Some(30)),
        ("4294967295", Some(u32::MAX)),
        ("4294967296", None),
        ("99999999999999999999999", None),
        ("+5", None),
        ("-1", None),
        ("1.5", None),
        ("", None),
        ("5s", None),
        ("Sun, 06 Nov 1994 08:49:37 GMT", None),
    ] {
        let outcome = classify(answer(429, Some(value), Ok(Vec::new())));
        assert_eq!(outcome, rejected(429, None, seconds), "{value:?}");
    }
    let outcome = classify(answer(503, Some("10"), Ok(Vec::new())));
    assert_eq!(outcome, unknown_with(Some(503), UnknownReason::ServerError));
}

#[tokio::test]
async fn server_errors_and_other_statuses_are_unknown() {
    for status in [500, 502, 503, 504, 529] {
        let outcome =
            answered(ResponseTemplate::new(status).set_body_json(envelope("api_error"))).await;
        assert_eq!(
            outcome,
            unknown_with(Some(status), UnknownReason::ServerError),
            "{status}"
        );
        assert_eq!(outcome.kind(), "unknown");
        assert_eq!(outcome.status(), Some(status));
    }
    for status in [201, 202, 204, 402, 405, 409, 413, 418, 422, 451] {
        let outcome =
            answered(ResponseTemplate::new(status).set_body_json(session_body(SESSION, None)))
                .await;
        assert_eq!(
            outcome,
            unknown_with(Some(status), UnknownReason::OtherStatus),
            "{status}"
        );
    }
    for status in [100, 199, 600, 999] {
        assert_eq!(
            classify(answer(status, None, Ok(Vec::new()))),
            unknown_with(Some(status), UnknownReason::OtherStatus),
            "{status}"
        );
    }
}

#[tokio::test]
async fn a_timeout_after_sending_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(session_body(SESSION, None))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let outcome = quick(&server.uri())
        .fire(&routine(), &token(), TASK)
        .await
        .unwrap();
    assert_eq!(outcome, unknown_with(None, UnknownReason::Timeout));
    assert_eq!(outcome.status(), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    assert_eq!(
        classify(answer(200, None, Err(BodyError::TimedOut))),
        unknown_with(Some(200), UnknownReason::Timeout)
    );
    assert_eq!(
        classify(Exchange::TimedOut),
        unknown_with(None, UnknownReason::Timeout)
    );
}

/// What [`hand_written`]'s server does with a request once it has read it
/// whole.
#[derive(Clone)]
enum Then {
    HangUp,
    Answer(Vec<u8>),
    AnswerAndStall(Vec<u8>),
}

/// A server on a local port that does `then` with every request, until a
/// second passes without a new connection; its task returns how many
/// connections it took.
async fn hand_written(then: Then) -> (String, tokio::task::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut connections = 0;
        while let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            connections += 1;
            read_request(&mut stream).await;
            match &then {
                Then::HangUp => {}
                Then::Answer(bytes) => {
                    stream.write_all(bytes).await.unwrap();
                }
                Then::AnswerAndStall(bytes) => {
                    stream.write_all(bytes).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        connections
    });
    (base, task)
}

async fn read_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return;
        }
    }
}

fn quick(base: &str) -> FireClient {
    FireClient::build(
        &config(base),
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .unwrap()
}

#[tokio::test]
async fn a_connection_lost_after_sending_is_unknown() {
    let (base, server) = hand_written(Then::HangUp).await;
    let outcome = fire(&base).await;
    assert_eq!(outcome, unknown_with(None, UnknownReason::ConnectionLost));
    assert_eq!(server.await.unwrap(), 1, "the request was sent again");

    let cut = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"claude_code".to_vec();
    let (base, server) = hand_written(Then::Answer(cut)).await;
    let outcome = fire(&base).await;
    assert_eq!(
        outcome,
        unknown_with(Some(200), UnknownReason::ConnectionLost)
    );
    assert_eq!(server.await.unwrap(), 1);

    assert_eq!(
        classify(answer(200, None, Err(BodyError::Lost))),
        unknown_with(Some(200), UnknownReason::ConnectionLost)
    );
}

#[tokio::test]
async fn a_success_whose_body_stalls_or_runs_past_the_limit_is_unknown() {
    let head = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"claude_code".to_vec();
    let (base, server) = hand_written(Then::AnswerAndStall(head)).await;
    let outcome = quick(&base).fire(&routine(), &token(), TASK).await.unwrap();
    assert_eq!(outcome, unknown_with(Some(200), UnknownReason::Timeout));
    assert_eq!(server.await.unwrap(), 1);

    let mut body = session_body(SESSION, None).to_string();
    body.pop();
    body.push_str(",\"pad\":\"");
    body.push_str(&" ".repeat(MAX_BODY_BYTES + 1 - body.len() - 2));
    body.push_str("\"}");
    assert_eq!(body.len(), MAX_BODY_BYTES + 1);
    assert!(session(body.as_bytes()).is_some());
    let mut chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
    for piece in body.as_bytes().chunks(8192) {
        chunked.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        chunked.extend_from_slice(piece);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let (base, server) = hand_written(Then::Answer(chunked)).await;
    let outcome = fire(&base).await;
    assert_eq!(outcome, unknown_with(Some(200), UnknownReason::Unreadable));
    assert_eq!(server.await.unwrap(), 1);

    let refused = b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 100\r\n\r\n{\"error".to_vec();
    let (base, server) = hand_written(Then::Answer(refused)).await;
    assert_eq!(fire(&base).await, rejected(401, None, None));
    assert_eq!(server.await.unwrap(), 1);
}

#[tokio::test]
async fn a_refused_connection_is_rejected() {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let outcome = fire(&base).await;
    assert_eq!(
        outcome,
        FireOutcome::Rejected {
            status: None,
            error_type: None,
            retry_after: None,
        }
    );
    assert_eq!(outcome.kind(), "rejected");
    assert_eq!(outcome.status(), None);
    drop(socket);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let server = MockServer::start().await;
    for status in [301, 302, 303, 307, 308] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path(FIRE_PATH))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", format!("{}/elsewhere", server.uri()).as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .and(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
            .expect(0)
            .mount(&server)
            .await;
        let outcome = fire(&server.uri()).await;
        assert_eq!(
            outcome,
            unknown_with(Some(status), UnknownReason::Redirect),
            "{status}"
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn an_unreadable_success_is_unknown() {
    let unreadable = [
        b"not json".to_vec(),
        b"{}".to_vec(),
        br#"["session_01HJK"]"#.to_vec(),
        br#"{"claude_code_session_id": 7}"#.to_vec(),
        br#"{"claude_code_session_id": "sess_01HJK"}"#.to_vec(),
        br#"{"claude_code_session_id": "session_"}"#.to_vec(),
        br#"{"claude_code_session_id": "session_01-HJK"}"#.to_vec(),
        br#"{"claude_code_session_id": "session_01HJK/../x"}"#.to_vec(),
        "{\"claude_code_session_id\": \"session_01é\"}"
            .as_bytes()
            .to_vec(),
        br#"{"claude_code_session_id": " session_01HJK"}"#.to_vec(),
        br#"{"session_id": "session_01HJK"}"#.to_vec(),
        session_body(&format!("session_{}", "a".repeat(129)), None)
            .to_string()
            .into_bytes(),
    ];
    for body in unreadable {
        let shown = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(
            classify(answer(200, None, Ok(body))),
            unknown_with(Some(200), UnknownReason::Unreadable),
            "{shown}"
        );
    }
    assert_eq!(
        classify(answer(200, None, Err(BodyError::TooLarge))),
        unknown_with(Some(200), UnknownReason::Unreadable)
    );

    let outcome = answered(ResponseTemplate::new(200).set_body_string("<html>ok</html>")).await;
    assert_eq!(outcome, unknown_with(Some(200), UnknownReason::Unreadable));

    let padded = |len: usize| {
        let mut body = session_body(SESSION, None).to_string();
        body.pop();
        body.push_str(",\"pad\":\"");
        let pad = len - body.len() - 2;
        body.push_str(&"x".repeat(pad));
        body.push_str("\"}");
        assert_eq!(body.len(), len);
        body
    };
    let outcome =
        answered(ResponseTemplate::new(200).set_body_string(padded(MAX_BODY_BYTES))).await;
    assert_eq!(outcome.kind(), "fired");
    let outcome =
        answered(ResponseTemplate::new(200).set_body_string(padded(MAX_BODY_BYTES + 1))).await;
    assert_eq!(outcome, unknown_with(Some(200), UnknownReason::Unreadable));
}

#[tokio::test]
async fn fire_never_retries() {
    for status in [500, 503] {
        let outcome = answered(ResponseTemplate::new(status)).await;
        assert_eq!(outcome.kind(), "unknown");
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = fire(&server.uri()).await;
    assert_eq!(outcome, unknown_with(Some(500), UnknownReason::ServerError));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn nothing_is_sent_for_a_bad_token_or_task() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(0)
        .mount(&server)
        .await;
    let refusing = client(&server.uri());
    for bad in [
        "",
        "sk-ant oat01",
        "sk-ant-oat01\r\nx-other: 1",
        "sk-ant-\u{e9}",
        "tab\there",
    ] {
        let err = refusing
            .fire(&routine(), &SecretString::from(bad), TASK)
            .await
            .unwrap_err();
        assert!(matches!(err, FireError::Token), "{bad:?}: {err}");
        assert!(bad.is_empty() || !err.to_string().contains(bad));
        assert!(bad.is_empty() || !format!("{err:?}").contains(bad));
    }
    let too_long = "x".repeat(MAX_TASK_BYTES + 1);
    for task in ["", too_long.as_str()] {
        let err = refusing.fire(&routine(), &token(), task).await.unwrap_err();
        assert!(matches!(err, FireError::Task), "{}", task.len());
    }
    server.verify().await;

    let longest_id: RoutineId = format!("trig_{}", "Z9".repeat(32)).parse().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/claude_code/routines/{longest_id}/fire")))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(1)
        .mount(&server)
        .await;
    let longest_task = "é".repeat(MAX_TASK_BYTES / 2);
    let outcome = client(&server.uri())
        .fire(&longest_id, &token(), &longest_task)
        .await
        .unwrap();
    assert_eq!(outcome.kind(), "fired");
    server.verify().await;
}

#[test]
fn a_client_is_built_only_from_a_valid_config() {
    let built = FireClient::new(&CloudConfig::default()).unwrap();
    let shown = format!("{built:?}");
    assert!(shown.contains("https://api.anthropic.com/"), "{shown}");
    assert!(shown.contains(DEFAULT_CLOUD_BETA), "{shown}");
    for (config, key) in [
        (
            CloudConfig {
                base_url: "https://api.anthropic.com/v1".to_owned(),
                ..CloudConfig::default()
            },
            "cloud.base_url",
        ),
        (
            CloudConfig {
                timeout_secs: 1,
                ..CloudConfig::default()
            },
            "cloud.timeout_secs",
        ),
        (
            CloudConfig {
                beta: "two words".to_owned(),
                ..CloudConfig::default()
            },
            "cloud.beta",
        ),
    ] {
        match FireClient::new(&config).unwrap_err() {
            FireError::Config(err) => assert_eq!(err.key(), Some(key)),
            other => panic!("{key}: {other}"),
        }
    }
}

#[test]
fn outcome_names_are_the_handoff_states() {
    assert_eq!(classify(Exchange::NotSent).kind(), "rejected");
    assert_eq!(classify(Exchange::Lost).kind(), "unknown");
    for (reason, name) in [
        (UnknownReason::ServerError, "server_error"),
        (UnknownReason::Redirect, "redirect"),
        (UnknownReason::OtherStatus, "other_status"),
        (UnknownReason::Timeout, "timeout"),
        (UnknownReason::ConnectionLost, "connection_lost"),
        (UnknownReason::Unreadable, "unreadable"),
    ] {
        assert_eq!(reason.as_str(), name);
    }
}

#[tokio::test]
async fn token_and_task_never_reach_the_log() {
    let logs = global_logs().tag();
    let marker = "BODY-MARKER-5c1f";
    let mut fired = session_body(SESSION, Some(&format!("{SESSION_URL_PREFIX}{SESSION}")));
    fired["note"] = json!(marker);
    let mut refused = envelope("authentication_error");
    refused["error"]["message"] = json!(marker);
    let templates = [
        ResponseTemplate::new(200).set_body_json(fired),
        ResponseTemplate::new(401).set_body_json(refused),
        ResponseTemplate::new(500).set_body_string(marker),
        ResponseTemplate::new(200).set_body_string(marker),
    ];
    let mut outcomes = Vec::new();
    for template in templates {
        outcomes.push(answered(template).await);
    }
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    outcomes.push(fire(&format!("http://{}", socket.local_addr().unwrap())).await);
    let kinds: Vec<_> = outcomes.iter().map(FireOutcome::kind).collect();
    assert_eq!(
        kinds,
        ["fired", "rejected", "unknown", "unknown", "rejected"]
    );

    let mine = logs.snapshot();
    mine.assert_has("fired a cloud routine")
        .assert_has(ROUTINE)
        .assert_has(SESSION)
        .assert_has("authentication_error")
        .assert_has("server_error")
        .assert_has("unreadable");
    for outcome in &outcomes {
        assert!(!format!("{outcome:?}").contains(TOKEN));
    }
    global_logs()
        .snapshot()
        .assert_lacks(TOKEN)
        .assert_lacks("routine-token")
        .assert_lacks("Fix the flaky test")
        .assert_lacks("ünïcode")
        .assert_lacks(marker);
}
