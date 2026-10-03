use std::time::Duration;

use core_types::{CloudRoutineId, MemberKey, RoutineToken, SurfaceKind, TeamId, UserId};
use secrecy::SecretString;
use serde_json::json;
use store::{CloudHandoffState, CloudOrigin, NewCloudHandoff, Sealer, Store};
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
        None,
    )
    .unwrap()
}

/// A client whose request times out in two seconds, for a fake that holds
/// its answer back for ten.
fn quick(base: &str) -> FireClient {
    FireClient::build(
        &config(base),
        Duration::from_secs(2),
        Duration::from_secs(1),
        None,
    )
    .unwrap()
}

/// Routine [`ROUTINE`] as the store gives it, registered for `base`'s
/// origin.
fn routine(base: &str) -> CloudRoutineToken {
    CloudRoutineToken {
        id: CloudRoutineId::new_v4(),
        routine_id: ROUTINE.parse().unwrap(),
        url_origin: Url::parse(base).unwrap().origin().ascii_serialization(),
        token: RoutineToken::parse(SecretString::from(TOKEN)).unwrap(),
    }
}

async fn fire(base: &str) -> FireOutcome {
    client(base).fire(&routine(base), TASK).await
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

fn rejected(status: u16, error_type: Option<&str>, retry_after_secs: Option<u32>) -> FireOutcome {
    FireOutcome::Rejected {
        status: Some(status),
        error_type: error_type.map(str::to_owned),
        retry_after_secs,
    }
}

fn not_sent() -> FireOutcome {
    FireOutcome::Rejected {
        status: None,
        error_type: None,
        retry_after_secs: None,
    }
}

fn unknown_with(status: Option<u16>, reason: CloudUnknownReason) -> FireOutcome {
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
    assert_eq!(outcome.state(), CloudHandoffState::Fired);

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
    assert_eq!(header("accept-encoding"), None);
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
        .fire(&routine(&server.uri()), TASK)
        .await;
    assert_eq!(outcome.state(), CloudHandoffState::Fired);
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
        assert_eq!(outcome.state(), CloudHandoffState::Rejected);
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
        ("86400", Some(86_400)),
        ("86401", Some(MAX_RETRY_AFTER_SECS)),
        ("4294967295", Some(MAX_RETRY_AFTER_SECS)),
        ("4294967296", Some(MAX_RETRY_AFTER_SECS)),
        ("99999999999999999999999", Some(MAX_RETRY_AFTER_SECS)),
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
    assert_eq!(MAX_RETRY_AFTER_SECS, 24 * 60 * 60);
    let outcome = classify(answer(503, Some("10"), Ok(Vec::new())));
    assert_eq!(
        outcome,
        unknown_with(Some(503), CloudUnknownReason::ServerError)
    );
}

#[tokio::test]
async fn server_errors_and_other_statuses_are_unknown() {
    for status in [500, 502, 503, 504, 529] {
        let outcome =
            answered(ResponseTemplate::new(status).set_body_json(envelope("api_error"))).await;
        assert_eq!(
            outcome,
            unknown_with(Some(status), CloudUnknownReason::ServerError),
            "{status}"
        );
        assert_eq!(outcome.state(), CloudHandoffState::Unknown);
    }
    for status in [201, 202, 204, 402, 405, 409, 413, 418, 422, 451] {
        let outcome =
            answered(ResponseTemplate::new(status).set_body_json(session_body(SESSION, None)))
                .await;
        assert_eq!(
            outcome,
            unknown_with(Some(status), CloudUnknownReason::OtherStatus),
            "{status}"
        );
    }
    for status in [100, 199, 600, 999] {
        assert_eq!(
            classify(answer(status, None, Ok(Vec::new()))),
            unknown_with(Some(status), CloudUnknownReason::OtherStatus),
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
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;
    let outcome = quick(&server.uri())
        .fire(&routine(&server.uri()), TASK)
        .await;
    assert_eq!(outcome, unknown_with(None, CloudUnknownReason::Timeout));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    assert_eq!(
        classify(answer(200, None, Err(BodyError::TimedOut))),
        unknown_with(Some(200), CloudUnknownReason::Timeout)
    );
    assert_eq!(
        classify(Exchange::TimedOut),
        unknown_with(None, CloudUnknownReason::Timeout)
    );
}

/// What [`hand_written`]'s server does with a request once it has read it
/// whole.
#[derive(Clone)]
enum Then {
    HangUp,
    Answer(Vec<u8>),
    /// Answers, then holds the connection open for ten seconds or until
    /// the client hangs up.
    AnswerAndStall(Vec<u8>),
    /// Answers every request the connection carries.
    KeepAlive(Vec<u8>),
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
                    let mut rest = [0; 1];
                    let _ =
                        tokio::time::timeout(Duration::from_secs(10), stream.read(&mut rest)).await;
                }
                Then::KeepAlive(bytes) => {
                    stream.write_all(bytes).await.unwrap();
                    while read_request(&mut stream).await {
                        stream.write_all(bytes).await.unwrap();
                    }
                }
            }
        }
        connections
    });
    (base, task)
}

/// Reads one request, and says whether it came whole.
async fn read_request(stream: &mut TcpStream) -> bool {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
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
            return true;
        }
    }
}

#[tokio::test]
async fn a_connection_lost_after_sending_is_unknown() {
    let (base, server) = hand_written(Then::HangUp).await;
    let outcome = fire(&base).await;
    assert_eq!(
        outcome,
        unknown_with(None, CloudUnknownReason::ConnectionLost)
    );
    assert_eq!(server.await.unwrap(), 1, "the request was sent again");

    let cut = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"claude_code".to_vec();
    let (base, server) = hand_written(Then::Answer(cut)).await;
    let outcome = fire(&base).await;
    assert_eq!(
        outcome,
        unknown_with(Some(200), CloudUnknownReason::ConnectionLost)
    );
    assert_eq!(server.await.unwrap(), 1);

    assert_eq!(
        classify(answer(200, None, Err(BodyError::Lost))),
        unknown_with(Some(200), CloudUnknownReason::ConnectionLost)
    );
}

#[tokio::test]
async fn a_success_whose_body_stalls_or_runs_past_the_limit_is_unknown() {
    let head = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"claude_code".to_vec();
    let (base, server) = hand_written(Then::AnswerAndStall(head)).await;
    let outcome = quick(&base).fire(&routine(&base), TASK).await;
    assert_eq!(
        outcome,
        unknown_with(Some(200), CloudUnknownReason::Timeout)
    );
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
    assert_eq!(
        outcome,
        unknown_with(Some(200), CloudUnknownReason::UnreadableAnswer)
    );
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
    assert_eq!(outcome, not_sent());
    assert_eq!(outcome.state(), CloudHandoffState::Rejected);
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
            unknown_with(Some(status), CloudUnknownReason::Redirect),
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
            unknown_with(Some(200), CloudUnknownReason::UnreadableAnswer),
            "{shown}"
        );
    }
    assert_eq!(
        classify(answer(200, None, Err(BodyError::TooLarge))),
        unknown_with(Some(200), CloudUnknownReason::UnreadableAnswer)
    );

    let outcome = answered(ResponseTemplate::new(200).set_body_string("<html>ok</html>")).await;
    assert_eq!(
        outcome,
        unknown_with(Some(200), CloudUnknownReason::UnreadableAnswer)
    );

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
    assert_eq!(outcome.state(), CloudHandoffState::Fired);
    let outcome =
        answered(ResponseTemplate::new(200).set_body_string(padded(MAX_BODY_BYTES + 1))).await;
    assert_eq!(
        outcome,
        unknown_with(Some(200), CloudUnknownReason::UnreadableAnswer)
    );
}

#[tokio::test]
async fn fire_never_retries() {
    for status in [500, 503] {
        let outcome = answered(ResponseTemplate::new(status)).await;
        assert_eq!(outcome.state(), CloudHandoffState::Unknown);
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(FIRE_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = fire(&server.uri()).await;
    assert_eq!(
        outcome,
        unknown_with(Some(500), CloudUnknownReason::ServerError)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn each_fire_opens_a_connection_of_its_own() {
    let mut answer = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        session_body(SESSION, None).to_string().len()
    )
    .into_bytes();
    answer.extend_from_slice(session_body(SESSION, None).to_string().as_bytes());
    let (base, server) = hand_written(Then::KeepAlive(answer)).await;
    let reused = client(&base);
    for _ in 0..2 {
        let outcome = reused.fire(&routine(&base), TASK).await;
        assert_eq!(outcome.state(), CloudHandoffState::Fired);
    }
    drop(reused);
    let connections = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server is still serving a connection")
        .unwrap();
    assert_eq!(connections, 2, "a connection was reused");
}

#[tokio::test]
async fn a_loopback_base_url_is_called_without_a_proxy() {
    testkit::proxy::assert_proxied_only_elsewhere(
        |base, proxy| {
            FireClient::build(
                &config(base),
                Duration::from_secs(10),
                Duration::from_secs(5),
                Some(proxy),
            )
            .unwrap()
            .http
        },
        &["https://127.0.0.1:9", "https://[::1]:9"],
    )
    .await;
}

#[tokio::test]
async fn nothing_is_sent_for_another_origin_or_a_bad_task() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(0)
        .mount(&server)
        .await;
    let refusing = client(&server.uri());
    assert_eq!(
        refusing.origin(),
        Url::parse(&server.uri())
            .unwrap()
            .origin()
            .ascii_serialization()
    );
    let logs = global_logs().tag();
    let port = Url::parse(&server.uri()).unwrap().port().unwrap();
    for elsewhere in [
        "https://api.anthropic.com".to_owned(),
        format!("http://127.0.0.1:{}", port + 1),
        format!("https://127.0.0.1:{port}"),
        format!("http://127.0.0.2:{port}"),
        format!("http://127.0.0.1:{port}/"),
        String::new(),
    ] {
        let routine = CloudRoutineToken {
            url_origin: elsewhere.clone(),
            ..routine(&server.uri())
        };
        let outcome = refusing.fire(&routine, TASK).await;
        assert_eq!(outcome, not_sent(), "{elsewhere}");
    }
    let too_long = "x".repeat(MAX_TASK_BYTES + 1);
    for task in ["", too_long.as_str()] {
        assert_eq!(check_task(task), Err(TaskError), "{}", task.len());
        let outcome = refusing.fire(&routine(&server.uri()), task).await;
        assert_eq!(outcome, not_sent(), "{}", task.len());
    }
    server.verify().await;
    logs.snapshot()
        .assert_has("wasn't sent")
        .assert_has("another origin")
        .assert_has("the task is empty or longer than 65536 bytes");

    let longest_id: RoutineId = format!("trig_{}", "Z9".repeat(32)).parse().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/claude_code/routines/{longest_id}/fire")))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body(SESSION, None)))
        .expect(1)
        .mount(&server)
        .await;
    let longest_task = "é".repeat(MAX_TASK_BYTES / 2);
    assert_eq!(check_task(&longest_task), Ok(()));
    let routine = CloudRoutineToken {
        routine_id: longest_id,
        ..routine(&server.uri())
    };
    let outcome = client(&server.uri()).fire(&routine, &longest_task).await;
    assert_eq!(outcome.state(), CloudHandoffState::Fired);
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
            FireClientError::Config(err) => assert_eq!(err.key(), Some(key)),
            other => panic!("{key}: {other}"),
        }
    }
}

#[test]
fn an_answer_shows_its_body_only_by_length() {
    let shown = format!(
        "{:?}",
        answer(401, Some("5"), Ok(b"BODY-MARKER-echoed".to_vec()))
    );
    assert!(!shown.contains("BODY-MARKER"), "{shown}");
    assert!(shown.contains("401") && shown.contains("Ok(18)"), "{shown}");
}

/// Every shape of outcome the classifier gives, recorded on a hand-off of
/// its own and read back: the store holds each one as it is.
#[tokio::test]
async fn every_outcome_is_recorded_as_it_is() {
    let longest_session = format!("session_{}", "a".repeat(128));
    let longest_type = "a_1".repeat(21) + "z";
    let outcomes = vec![
        classify(answer(
            200,
            None,
            Ok(session_body(
                &longest_session,
                Some(&format!("{SESSION_URL_PREFIX}{longest_session}")),
            )
            .to_string()
            .into_bytes()),
        )),
        classify(answer(
            200,
            None,
            Ok(session_body(SESSION, None).to_string().into_bytes()),
        )),
        classify(answer(
            429,
            Some("99999999999"),
            Ok(envelope(&longest_type).to_string().into_bytes()),
        )),
        classify(answer(401, Some("0"), Ok(Vec::new()))),
        classify(Exchange::NotSent),
        classify(answer(503, None, Ok(Vec::new()))),
        classify(answer(308, None, Ok(Vec::new()))),
        classify(answer(100, None, Ok(Vec::new()))),
        classify(answer(999, None, Ok(Vec::new()))),
        classify(answer(200, None, Err(BodyError::TimedOut))),
        classify(answer(200, None, Ok(b"<html>".to_vec()))),
        classify(Exchange::TimedOut),
        classify(Exchange::Lost),
    ];
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let key = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new("chat.example.org"),
        user: UserId::new("ada"),
    };
    let now = time::OffsetDateTime::now_utc();
    let member = store.ensure_member(&key, "Ada", now).await.unwrap();
    let routine_id: RoutineId = ROUTINE.parse().unwrap();
    for outcome in &outcomes {
        let id = store
            .begin_cloud_handoff(
                &NewCloudHandoff {
                    member,
                    routine_label: "agent-core",
                    routine_id: &routine_id,
                    requested_by: &key,
                    origin: CloudOrigin::RocketChatDm,
                    task: TASK,
                },
                now,
            )
            .await
            .unwrap();
        assert!(
            store.finish_cloud_handoff(id, outcome, now).await.unwrap(),
            "{outcome:?}"
        );
        let recent = store.recent_cloud_handoffs(member, 1).await.unwrap();
        let stored = &recent[0].handoff;
        assert_eq!(stored.id, id);
        let read_back = match stored.state {
            CloudHandoffState::Fired => CloudOutcome::Fired {
                session_id: stored.session_id.clone().unwrap(),
                session_url: stored.session_url.clone(),
            },
            CloudHandoffState::Rejected => CloudOutcome::Rejected {
                status: stored.http_status,
                error_type: stored.error_type.clone(),
                retry_after_secs: stored.retry_after_secs,
            },
            CloudHandoffState::Unknown => CloudOutcome::Unknown {
                status: stored.http_status,
                reason: stored.unknown_reason.unwrap(),
            },
            CloudHandoffState::Sending => panic!("{outcome:?} left the hand-off sending"),
        };
        assert_eq!(&read_back, outcome);
    }
    let reasons: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            CloudOutcome::Unknown { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    for reason in [
        CloudUnknownReason::ServerError,
        CloudUnknownReason::Redirect,
        CloudUnknownReason::OtherStatus,
        CloudUnknownReason::Timeout,
        CloudUnknownReason::ConnectionLost,
        CloudUnknownReason::UnreadableAnswer,
    ] {
        assert!(reasons.contains(&reason), "{reason:?}");
    }
    assert!(!reasons.contains(&CloudUnknownReason::NoAnswer));
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
    let states: Vec<_> = outcomes.iter().map(CloudOutcome::state).collect();
    assert_eq!(
        states,
        [
            CloudHandoffState::Fired,
            CloudHandoffState::Rejected,
            CloudHandoffState::Unknown,
            CloudHandoffState::Unknown,
            CloudHandoffState::Rejected,
        ]
    );

    let mine = logs.snapshot();
    mine.assert_has("fired a cloud routine")
        .assert_has(r#""outcome":"fired""#)
        .assert_has("wasn't sent")
        .assert_has("tcp connect error")
        .assert_has(ROUTINE)
        .assert_has(SESSION)
        .assert_has("authentication_error")
        .assert_has("server_error")
        .assert_has("unreadable_answer");
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
