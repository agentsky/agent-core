//! The ingress end to end, in-process: request URLs, verification, the
//! ack, the queue, deduplication and normalization.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use core_types::{BindingId, ConvKind, MemberId, SendError, Sender, Sink, UserId};
use futures::StreamExt as _;
use secrecy::{ExposeSecret as _, SecretString};
use surface_slack::ingress::{
    AGENT_BURST, AGENT_REQUESTS_PER_SECOND, MAX_IN_FLIGHT_PER_AGENT, MAX_IN_FLIGHT_PER_OWNER,
    PRE_ACK_TIMEOUT,
};
use surface_slack::{BindingRef, BoxError, Dedup, SigningSecrets, SlackApp, SlackInbound, ingress};
use testkit::slack::{self as fixtures, BOT_USER, CHALLENGE, TEAM};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tower::ServiceExt as _;

const MANAGER_SECRET: &str = "manager-signing-secret";
const AGENT_SECRET: &str = "agent-signing-secret";
const OTHER_SECRET: &str = "other-agent-signing-secret";
const THIRD_SECRET: &str = "third-agent-signing-secret";
const FOURTH_SECRET: &str = "fourth-agent-signing-secret";

#[derive(Default)]
struct Secrets {
    apps: HashMap<BindingRef, SlackApp>,
    failing: bool,
    delay: Duration,
}

#[async_trait::async_trait]
impl SigningSecrets for Secrets {
    async fn lookup(&self, binding: BindingRef) -> Result<Option<SlackApp>, BoxError> {
        tokio::time::sleep(self.delay).await;
        if self.failing {
            return Err("the store is down".into());
        }
        Ok(self.apps.get(&binding).cloned())
    }
}

#[derive(Default)]
struct MemoryDedup {
    seen: Mutex<HashSet<(String, String)>>,
    failing: bool,
}

#[async_trait::async_trait]
impl Dedup for MemoryDedup {
    async fn first_time(&self, source: &str, key: &str) -> Result<bool, BoxError> {
        if self.failing {
            return Err("the store is down".into());
        }
        Ok(self
            .seen
            .lock()
            .unwrap()
            .insert((source.to_owned(), key.to_owned())))
    }
}

struct Collect(mpsc::UnboundedSender<SlackInbound>);

#[async_trait::async_trait]
impl Sink<SlackInbound> for Collect {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        self.0.send(item).map_err(|_| SendError)
    }
}

/// A sink that takes a long time for every item, like a slow handler.
struct Slow {
    delay: Duration,
    inner: Collect,
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl Sink<SlackInbound> for Slow {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        self.started.notify_one();
        tokio::time::sleep(self.delay).await;
        self.inner.send(item).await
    }
}

fn agent() -> BindingId {
    "0b9f6f3e-7c4f-4c55-9d7b-7f2a6c1e5d10".parse().unwrap()
}

fn other_agent() -> BindingId {
    "5f0c1d2e-3a4b-4c5d-8e6f-708192a3b4c5".parse().unwrap()
}

fn creating_agent() -> BindingId {
    "9a8b7c6d-5e4f-4a3b-9c2d-1e0f2a3b4c5d".parse().unwrap()
}

fn third_agent() -> BindingId {
    "3c1d2e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f".parse().unwrap()
}

fn fourth_agent() -> BindingId {
    "7e6d5c4b-3a29-4180-9f8e-7d6c5b4a3928".parse().unwrap()
}

fn owner(n: u128) -> MemberId {
    MemberId::from_uuid(uuid::Uuid::from_u128(n))
}

fn app(secret: Option<&str>, bot_user: Option<&str>) -> SlackApp {
    SlackApp {
        signing_secret: secret.map(SecretString::from),
        bot_user: bot_user.map(UserId::from),
        owner: None,
    }
}

fn agent_app(secret: &str, bot_user: &str, owner: MemberId) -> SlackApp {
    SlackApp {
        owner: Some(owner),
        ..app(Some(secret), Some(bot_user))
    }
}

/// The manager app, and four agents' apps whose owners are `owners`, in
/// order, besides one being created.
fn secrets_owned_by(owners: [MemberId; 4]) -> Secrets {
    let [first, second, third, fourth] = owners;
    Secrets {
        apps: HashMap::from([
            (BindingRef::Manager, app(Some(MANAGER_SECRET), None)),
            (
                BindingRef::Agent(agent()),
                agent_app(AGENT_SECRET, BOT_USER, first),
            ),
            (
                BindingRef::Agent(other_agent()),
                agent_app(OTHER_SECRET, "U0BOT0002", second),
            ),
            (
                BindingRef::Agent(third_agent()),
                agent_app(THIRD_SECRET, "U0BOT0003", third),
            ),
            (
                BindingRef::Agent(fourth_agent()),
                agent_app(FOURTH_SECRET, "U0BOT0004", fourth),
            ),
            (BindingRef::Agent(creating_agent()), app(None, None)),
        ]),
        ..Secrets::default()
    }
}

fn secrets() -> Secrets {
    secrets_owned_by([owner(1), owner(2), owner(3), owner(4)])
}

struct Harness {
    router: Router,
    out: mpsc::UnboundedReceiver<SlackInbound>,
    worker: JoinHandle<()>,
    dedup: Arc<MemoryDedup>,
}

impl Harness {
    fn start() -> Self {
        Self::with(secrets(), MemoryDedup::default())
    }

    fn with(secrets: Secrets, dedup: MemoryDedup) -> Self {
        Self::with_capacity(secrets, dedup, 64)
    }

    fn with_capacity(secrets: Secrets, dedup: MemoryDedup, capacity: usize) -> Self {
        let (router, queue) = ingress(Arc::new(secrets), capacity);
        let (tx, out) = mpsc::unbounded_channel();
        let dedup = Arc::new(dedup);
        let worker = tokio::spawn(queue.run(dedup.clone(), Sender::new(Collect(tx))));
        Self {
            router,
            out,
            worker,
            dedup,
        }
    }

    /// How many keys are recorded, under any source.
    fn recorded_anywhere(&self) -> usize {
        self.dedup.seen.lock().unwrap().len()
    }

    /// The keys recorded under `source`.
    fn recorded(&self, source: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .dedup
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(recorded, _)| recorded == source)
            .map(|(_, key)| key.clone())
            .collect();
        keys.sort();
        keys
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn next(&mut self) -> SlackInbound {
        tokio::time::timeout(Duration::from_secs(5), self.out.recv())
            .await
            .expect("nothing was delivered")
            .expect("the queue stopped")
    }

    /// Sends a message that is always delivered, and checks that it is the
    /// next thing delivered: whatever was sent before it and not delivered
    /// yet was dropped, since the queue keeps order.
    async fn assert_nothing_delivered(&mut self) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let event_id = format!("Ev0MARKER{n}");
        let marker = fixtures::with_event_id(fixtures::MESSAGE_IM, &event_id)
            .replace("1727697900.000500", &format!("1727699999.{n:06}"));
        let (status, _) = self
            .send(signed_events(agent(), AGENT_SECRET, &marker))
            .await;
        assert_eq!(status, StatusCode::OK);
        match self.next().await {
            SlackInbound::Message(event, _) => assert_eq!(event.event_id, event_id),
            other => panic!("expected the marker, got {other:?}"),
        }
    }

    async fn message(&mut self) -> core_types::InboundEvent {
        match self.next().await {
            SlackInbound::Message(event, _) => *event,
            other => panic!("expected a message, got {other:?}"),
        }
    }
}

fn path(binding: impl std::fmt::Display, kind: &str) -> String {
    format!("/slack/b/{binding}/{kind}")
}

fn request(uri: &str, body: &str, headers: &[(&str, String)]) -> Request<Body> {
    let mut builder = Request::post(uri);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

fn signed(uri: &str, secret: &str, body: &str) -> Request<Body> {
    signed_at(uri, secret, body, fixtures::now())
}

fn signed_at(uri: &str, secret: &str, body: &str, timestamp: i64) -> Request<Body> {
    request(
        uri,
        body,
        &fixtures::signed_headers(secret, timestamp, body.as_bytes()),
    )
}

fn signed_events(binding: BindingId, secret: &str, body: &str) -> Request<Body> {
    signed(&path(binding, "events"), secret, body)
}

#[tokio::test]
async fn a_validly_signed_mention_is_acked_and_normalized() {
    let mut harness = Harness::start();
    let (status, body) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    let event = harness.message().await;
    assert_eq!(event.binding, agent());
    assert_eq!(event.event_id, "Ev0MENTION1");
    assert_eq!(event.sender.user.as_str(), fixtures::USER);
    assert_eq!(event.sender.team.as_str(), TEAM);
    assert_eq!(event.conv.conversation.as_str(), fixtures::CHANNEL);
    assert_eq!(event.conv_kind, ConvKind::Channel);
    assert_eq!(event.message.id.as_str(), "1727697600.000100");
    let mentions: Vec<&str> = event.mentions.iter().map(UserId::as_str).collect();
    assert_eq!(mentions, [BOT_USER, fixtures::OTHER_USER]);
    assert!(!event.sender_is_bot);
    assert!(event.text.starts_with("<@U0BOT0001> can you ask"));
}

#[tokio::test]
async fn a_bad_signature_is_refused() {
    let mut harness = Harness::start();
    let mut headers = fixtures::signed_headers(
        AGENT_SECRET,
        fixtures::now(),
        fixtures::MESSAGE_MENTION.as_bytes(),
    );
    headers[1].1 = format!("v0={}", "0".repeat(64));
    let uri = path(agent(), "events");
    let (status, body) = harness
        .send(request(&uri, fixtures::MESSAGE_MENTION, &headers))
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, ""));

    let tampered = fixtures::MESSAGE_MENTION.replace("release notes", "release notez");
    let headers = fixtures::signed_headers(
        AGENT_SECRET,
        fixtures::now(),
        fixtures::MESSAGE_MENTION.as_bytes(),
    );
    let (status, _) = harness.send(request(&uri, &tampered, &headers)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    harness.assert_nothing_delivered().await;
}

#[tokio::test]
async fn a_stale_or_future_timestamp_is_refused() {
    let mut harness = Harness::start();
    let uri = path(agent(), "events");
    for offset in [-301, 301, -86_400] {
        let request = signed_at(
            &uri,
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
            fixtures::now() + offset,
        );
        let (status, _) = harness.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{offset}");
    }
    let request = signed_at(
        &uri,
        AGENT_SECRET,
        fixtures::MESSAGE_MENTION,
        fixtures::now() - 240,
    );
    assert_eq!(harness.send(request).await.0, StatusCode::OK);
    assert_eq!(harness.message().await.event_id, "Ev0MENTION1");
}

#[tokio::test]
async fn the_wrong_apps_secret_is_refused() {
    let mut harness = Harness::start();
    for (binding, secret) in [
        (path(agent(), "events"), OTHER_SECRET),
        (path(other_agent(), "events"), AGENT_SECRET),
        (path(BindingRef::Manager, "events"), AGENT_SECRET),
        (path(agent(), "events"), MANAGER_SECRET),
    ] {
        let (status, _) = harness
            .send(signed(&binding, secret, fixtures::MESSAGE_MENTION))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{binding}");
    }
    harness.assert_nothing_delivered().await;
}

#[tokio::test]
async fn missing_repeated_and_malformed_headers_are_refused() {
    let harness = Harness::start();
    let uri = path(agent(), "events");
    let body = fixtures::MESSAGE_MENTION;
    let [timestamp, signature] =
        fixtures::signed_headers(AGENT_SECRET, fixtures::now(), body.as_bytes());
    let cases: Vec<Vec<(&str, String)>> = vec![
        vec![],
        vec![timestamp.clone()],
        vec![signature.clone()],
        vec![timestamp.clone(), signature.clone(), signature.clone()],
        vec![timestamp.clone(), timestamp.clone(), signature.clone()],
        vec![
            ("x-slack-request-timestamp", format!("{}.0", timestamp.1)),
            signature.clone(),
        ],
        vec![
            ("x-slack-request-timestamp", format!("+{}", timestamp.1)),
            signature.clone(),
        ],
        vec![
            timestamp.clone(),
            ("x-slack-signature", signature.1.replacen("v0=", "v1=", 1)),
        ],
    ];
    for headers in cases {
        let (status, _) = harness.send(request(&uri, body, &headers)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
    }
}

#[tokio::test]
async fn the_challenge_is_echoed_unsigned_only_while_a_binding_has_no_secret() {
    let mut harness = Harness::start();
    let uri = path(BindingRef::Agent(creating_agent()), "events");
    let response = harness
        .router
        .clone()
        .oneshot(request(&uri, fixtures::URL_VERIFICATION, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, CHALLENGE);

    for (binding, secret) in [
        (BindingRef::Manager, MANAGER_SECRET),
        (BindingRef::Agent(agent()), AGENT_SECRET),
    ] {
        let uri = path(binding, "events");
        let (status, body) = harness
            .send(request(&uri, fixtures::URL_VERIFICATION, &[]))
            .await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::UNAUTHORIZED, ""),
            "{binding} has a secret"
        );
        assert_eq!(
            harness
                .send(signed(&uri, secret, fixtures::URL_VERIFICATION))
                .await,
            (StatusCode::OK, CHALLENGE.to_owned()),
            "{binding}"
        );
    }

    for unknown in [
        BindingId::new_v4().to_string(),
        agent().to_string().to_uppercase(),
        agent().to_string().replace('-', ""),
        BindingRef::MANAGER_ID.to_string(),
        "Manager".to_owned(),
        "..".to_owned(),
        "%2e%2e".to_owned(),
    ] {
        let uri = path(&unknown, "events");
        let (status, body) = harness
            .send(request(&uri, fixtures::URL_VERIFICATION, &[]))
            .await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::NOT_FOUND, ""),
            "{unknown}"
        );
    }
    harness.assert_nothing_delivered().await;
}

#[tokio::test]
async fn the_challenge_is_answered_only_on_events_and_only_when_well_formed() {
    let harness = Harness::start();
    for kind in ["commands", "interactivity"] {
        let uri = path(agent(), kind);
        let (status, _) = harness
            .send(request(&uri, fixtures::URL_VERIFICATION, &[]))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{kind}");
    }
    let uri = path(creating_agent(), "events");
    for bad in [
        r#"{"type":"url_verification"}"#.to_owned(),
        r#"{"type":"url_verification","challenge":""}"#.to_owned(),
        r#"{"type":"url_verification","challenge":42}"#.to_owned(),
        r#"{"type":"url_verification","challenge":"<script>alert(1)</script> x"}"#.to_owned(),
        format!(
            r#"{{"type":"url_verification","challenge":"{}"}}"#,
            "a".repeat(257)
        ),
    ] {
        let (status, body) = harness.send(request(&uri, &bad, &[])).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::BAD_REQUEST, ""),
            "{bad}"
        );
    }
    let longest = format!(
        r#"{{"type":"url_verification","challenge":"{}"}}"#,
        "a".repeat(256)
    );
    assert_eq!(
        harness.send(request(&uri, &longest, &[])).await.0,
        StatusCode::OK
    );
    for not_a_challenge in [
        r#"{"type":"event_callback","challenge":"x"}"#,
        "not json",
        "",
    ] {
        let (status, _) = harness.send(request(&uri, not_a_challenge, &[])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{not_a_challenge}");
    }
}

#[tokio::test]
async fn ssl_check_is_answered_on_the_command_url_without_verification() {
    let mut harness = Harness::start();
    let body = "ssl_check=1&token=legacy-verification-token";
    for binding in [
        BindingRef::Manager,
        BindingRef::Agent(agent()),
        BindingRef::Agent(creating_agent()),
    ] {
        let uri = path(binding, "commands");
        assert_eq!(
            harness.send(request(&uri, body, &[])).await,
            (StatusCode::OK, String::new()),
            "{binding}"
        );
    }
    let uri = path(BindingRef::Manager, "commands");
    assert_eq!(
        harness.send(signed(&uri, MANAGER_SECRET, body)).await,
        (StatusCode::OK, String::new())
    );

    for kind in ["events", "interactivity"] {
        let uri = path(BindingRef::Manager, kind);
        let (status, _) = harness.send(request(&uri, body, &[])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{kind}");
    }
    for not_a_check in [
        "ssl_check=0&token=x",
        "ssl_check=true",
        "ssl_check=1&ssl_check=1",
    ] {
        let (status, _) = harness.send(request(&uri, not_a_check, &[])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{not_a_check}");
    }
    let unknown = path(BindingId::new_v4(), "commands");
    assert_eq!(
        harness.send(request(&unknown, body, &[])).await.0,
        StatusCode::NOT_FOUND
    );
    harness.assert_nothing_delivered().await;
    assert!(harness.recorded("slack:manager:request").is_empty());
}

/// A request whose body sends its first bytes and then nothing more.
fn stalled(uri: &str) -> Request<Body> {
    let body = fixtures::MESSAGE_MENTION;
    let headers = fixtures::signed_headers(AGENT_SECRET, fixtures::now(), body.as_bytes());
    let first = Bytes::copy_from_slice(&body.as_bytes()[..10]);
    let chunks =
        futures::stream::iter([Ok::<_, std::io::Error>(first)]).chain(futures::stream::pending());
    let mut builder = Request::post(uri).header("content-length", body.len());
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from_stream(chunks)).unwrap()
}

#[tokio::test]
async fn a_stalled_body_gets_408_after_the_pre_ack_timeout() {
    let harness = Harness::start();
    let begun = Instant::now();
    let (status, body) = tokio::time::timeout(
        PRE_ACK_TIMEOUT + Duration::from_secs(3),
        harness.send(stalled(&path(agent(), "events"))),
    )
    .await
    .expect("the stalled request was never answered");
    let took = begun.elapsed();
    assert_eq!((status, body.as_str()), (StatusCode::REQUEST_TIMEOUT, ""));
    assert!(took >= PRE_ACK_TIMEOUT, "{took:?}");
    assert!(took < PRE_ACK_TIMEOUT + Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn a_slow_lookup_gets_503_and_shares_the_timeout_with_the_body() {
    let hung = Secrets {
        delay: Duration::from_secs(3600),
        ..secrets()
    };
    let harness = Harness::with(hung, MemoryDedup::default());
    let begun = Instant::now();
    let (status, _) = tokio::time::timeout(
        PRE_ACK_TIMEOUT + Duration::from_secs(3),
        harness.send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        )),
    )
    .await
    .expect("the request was never answered");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        begun.elapsed() < PRE_ACK_TIMEOUT + Duration::from_secs(1),
        "{:?}",
        begun.elapsed()
    );

    let slow = Secrets {
        delay: PRE_ACK_TIMEOUT * 3 / 4,
        ..secrets()
    };
    let harness = Harness::with(slow, MemoryDedup::default());
    let begun = Instant::now();
    let (status, _) = harness.send(stalled(&path(agent(), "events"))).await;
    let took = begun.elapsed();
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert!(
        took < PRE_ACK_TIMEOUT + Duration::from_millis(900),
        "{took:?}"
    );
}

#[tokio::test]
async fn a_binding_without_a_secret_verifies_nothing() {
    let harness = Harness::start();
    let uri = path(creating_agent(), "events");
    for secret in [AGENT_SECRET, ""] {
        let (status, _) = harness
            .send(signed(&uri, secret, fixtures::MESSAGE_MENTION))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn a_retried_event_is_delivered_once() {
    let mut harness = Harness::start();
    let uri = path(agent(), "events");
    let (status, _) = harness
        .send(signed(&uri, AGENT_SECRET, fixtures::MESSAGE_MENTION))
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = fixtures::MESSAGE_MENTION;
    let mut headers =
        fixtures::signed_headers(AGENT_SECRET, fixtures::now(), body.as_bytes()).to_vec();
    headers.push(("x-slack-retry-num", "1".to_owned()));
    headers.push(("x-slack-retry-reason", "http_timeout".to_owned()));
    let (status, _) = harness.send(request(&uri, body, &headers)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.message().await.event_id, "Ev0MENTION1");
    harness.assert_nothing_delivered().await;
    assert!(harness.recorded(&format!("slack:{}", agent())).is_empty());
    let messages = harness.recorded(&format!("slack:{}:message", agent()));
    assert!(
        messages.contains(&format!("{}:1727697600.000100", fixtures::CHANNEL)),
        "{messages:?}"
    );
}

#[tokio::test]
async fn unaddressed_messages_are_dropped_without_a_dedup_write() {
    let mut harness = Harness::start();
    for body in [fixtures::MESSAGE_PLAIN, fixtures::MESSAGE_CHANGED] {
        for _ in 0..2 {
            let (status, _) = harness
                .send(signed_events(agent(), AGENT_SECRET, body))
                .await;
            assert_eq!(status, StatusCode::OK);
        }
    }
    harness.assert_nothing_delivered().await;
    assert!(harness.recorded(&format!("slack:{}", agent())).is_empty());
    let messages = harness.recorded(&format!("slack:{}:message", agent()));
    assert!(
        messages
            .iter()
            .all(|key| !key.starts_with(fixtures::CHANNEL)),
        "{messages:?}"
    );
}

#[tokio::test]
async fn one_message_under_two_event_ids_is_delivered_once_per_binding() {
    let mut harness = Harness::start();
    let second = fixtures::with_event_id(fixtures::MESSAGE_MENTION, "Ev0MENTION2");
    for body in [fixtures::MESSAGE_MENTION, second.as_str()] {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, body))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(harness.message().await.event_id, "Ev0MENTION1");
    harness.assert_nothing_delivered().await;

    let for_other = fixtures::MESSAGE_THREAD_REPLY;
    for binding in [agent(), other_agent()] {
        let secret = if binding == agent() {
            AGENT_SECRET
        } else {
            OTHER_SECRET
        };
        let (status, _) = harness
            .send(signed_events(binding, secret, for_other))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(harness.message().await.binding, agent());
    assert_eq!(harness.message().await.binding, other_agent());
}

#[tokio::test]
async fn each_message_fixture_normalizes_as_the_plan_says() {
    let mut harness = Harness::start();
    let send = |body: &'static str| signed_events(agent(), AGENT_SECRET, body);

    assert_eq!(
        harness.send(send(fixtures::MESSAGE_PLAIN)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        harness.send(send(fixtures::MESSAGE_CHANGED)).await.0,
        StatusCode::OK
    );
    harness.assert_nothing_delivered().await;

    harness.send(send(fixtures::MESSAGE_THREAD_REPLY)).await;
    let reply = harness.message().await;
    assert_eq!(reply.conv_kind, ConvKind::Channel);
    assert_eq!(
        reply.thread_root.as_ref().map(|root| root.as_str()),
        Some("1727697650.000150")
    );
    assert_eq!(
        reply.reply_to.as_ref().map(|to| to.id.as_str()),
        Some("1727697650.000150")
    );
    assert!(reply.mentions.is_empty());

    harness.send(send(fixtures::MESSAGE_THREAD_BROADCAST)).await;
    let broadcast = harness.message().await;
    assert_eq!(
        broadcast.thread_root.as_ref().map(|root| root.as_str()),
        Some("1727697650.000150")
    );
    assert_eq!(broadcast.message.id.as_str(), "1727697800.000400");

    harness.send(send(fixtures::MESSAGE_IM)).await;
    let dm = harness.message().await;
    assert_eq!(dm.conv_kind, ConvKind::Dm);
    assert!(dm.is_dm());
    assert!(dm.mentions.is_empty());

    harness.send(send(fixtures::MESSAGE_MPIM)).await;
    assert_eq!(harness.message().await.conv_kind, ConvKind::GroupDm);

    harness.send(send(fixtures::MESSAGE_GROUP)).await;
    let group = harness.message().await;
    assert_eq!(group.conv_kind, ConvKind::Channel);
    assert_eq!(group.mentions, [UserId::from(BOT_USER)]);

    harness.send(send(fixtures::MESSAGE_BOT)).await;
    let bot = harness.message().await;
    assert!(bot.sender_is_bot);
    assert_eq!(bot.sender.user.as_str(), "U0BOT0002");
    assert_eq!(bot.sender_bot_user, Some(UserId::from("U0BOT0002")));
    assert_eq!(bot.mentions, [UserId::from(BOT_USER)]);

    harness.send(send(fixtures::MESSAGE_BOT_WITHOUT_USER)).await;
    let legacy = harness.message().await;
    assert!(legacy.sender_is_bot);
    assert_eq!(legacy.sender.user.as_str(), "B0LEGACY1");
    assert_eq!(legacy.sender_bot_user, None);

    harness.send(send(fixtures::MESSAGE_FILE_SHARE)).await;
    let file = harness.message().await;
    assert_eq!(file.conv_kind, ConvKind::Dm);
    assert_eq!(file.files.len(), 1);
    assert_eq!(file.files[0].id, "F0FILE0001");
    assert_eq!(file.files[0].name, "build.log");
    assert_eq!(file.files[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(file.files[0].size, Some(2048));
    assert!(file.files[0].url.ends_with("/download/build.log"));
}

#[tokio::test]
async fn other_events_are_handed_on_and_rate_limit_notices_are_acked() {
    let mut harness = Harness::start();
    let manager_events = path(BindingRef::Manager, "events");
    let (status, _) = harness
        .send(signed(
            &manager_events,
            MANAGER_SECRET,
            fixtures::USER_CHANGE,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    match harness.next().await {
        SlackInbound::Event(event) => {
            assert_eq!(event.binding, BindingRef::MANAGER_ID);
            assert_eq!(event.event_type, "user_change");
            assert_eq!(event.event_id, "Ev0USERCHG1");
            assert_eq!(event.team.as_ref().map(|t| t.as_str()), Some(TEAM));
            assert_eq!(event.event["user"]["deleted"], true);
        }
        other => panic!("expected an event, got {other:?}"),
    }
    let (status, body) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::APP_RATE_LIMITED,
        ))
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    assert_eq!(harness.recorded("slack:manager"), ["Ev0USERCHG1"]);
    let unknown = r#"{"type":"something_new","token":"x"}"#;
    assert_eq!(
        harness
            .send(signed_events(agent(), AGENT_SECRET, unknown))
            .await
            .0,
        StatusCode::OK
    );
    harness.assert_nothing_delivered().await;
}

#[tokio::test]
async fn a_slash_command_is_acked_empty_and_handed_on() {
    let mut harness = Harness::start();
    let uri = path(BindingRef::Manager, "commands");
    let (status, body) = harness
        .send(signed(&uri, MANAGER_SECRET, fixtures::SLASH_COMMAND))
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    let SlackInbound::Command(command) = harness.next().await else {
        panic!("expected a command");
    };
    assert_eq!(command.binding, BindingRef::MANAGER_ID);
    assert_eq!(command.command, "/agent");
    assert_eq!(command.text, "create helper You are terse.");
    assert_eq!(command.sender.user.as_str(), fixtures::USER);
    assert_eq!(command.sender.team.as_str(), TEAM);
    assert_eq!(command.conv.conversation.as_str(), fixtures::CHANNEL);
    assert!(
        command
            .response_url
            .expose_secret()
            .starts_with("https://hooks.slack.com/commands/")
    );
    assert!(command.trigger_id.is_some());
    let debug = format!("{command:?}");
    assert!(!debug.contains("terse"), "{debug}");
    assert!(!debug.contains("hooks.slack.com"), "{debug}");
}

#[tokio::test]
async fn a_replayed_command_or_interaction_is_dropped() {
    let mut harness = Harness::start();
    let uri = path(BindingRef::Manager, "commands");
    let timestamp = fixtures::now();
    let body = fixtures::SLASH_COMMAND;
    let headers = fixtures::signed_headers(MANAGER_SECRET, timestamp, body.as_bytes());
    let mut upper = headers.clone();
    upper[1].1 = format!("v0={}", upper[1].1[3..].to_uppercase());
    for headers in [&headers, &headers, &upper] {
        assert_eq!(
            harness.send(request(&uri, body, headers)).await.0,
            StatusCode::OK
        );
    }
    assert!(matches!(harness.next().await, SlackInbound::Command(_)));
    harness.assert_nothing_delivered().await;

    let interactivity = path(BindingRef::Manager, "interactivity");
    let body = fixtures::interactivity_body(fixtures::BLOCK_ACTIONS);
    let headers = fixtures::signed_headers(MANAGER_SECRET, timestamp, body.as_bytes());
    for _ in 0..2 {
        let (status, _) = harness.send(request(&interactivity, &body, &headers)).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert!(matches!(harness.next().await, SlackInbound::Interaction(_)));
    harness.assert_nothing_delivered().await;
}

#[tokio::test]
async fn an_interaction_is_acked_empty_and_handed_on_without_its_token() {
    let mut harness = Harness::start();
    let uri = path(BindingRef::Manager, "interactivity");
    let body = fixtures::interactivity_body(fixtures::BLOCK_ACTIONS);
    let (status, response) = harness.send(signed(&uri, MANAGER_SECRET, &body)).await;
    assert_eq!((status, response.as_str()), (StatusCode::OK, ""));
    let SlackInbound::Interaction(interaction) = harness.next().await else {
        panic!("expected an interaction");
    };
    assert_eq!(interaction.binding, BindingRef::MANAGER_ID);
    assert_eq!(interaction.kind, "block_actions");
    let sender = interaction.sender.as_ref().unwrap();
    assert_eq!(sender.user.as_str(), fixtures::USER);
    assert_eq!(sender.team.as_str(), TEAM);
    assert!(
        interaction
            .response_url
            .as_ref()
            .unwrap()
            .expose_secret()
            .starts_with("https://hooks.slack.com/actions/")
    );
    assert!(!interaction.payload.contains_key("token"));
    assert!(!interaction.payload.contains_key("response_url"));
    assert_eq!(
        interaction.payload["actions"][0]["action_id"],
        "consent_approve"
    );
    let debug = format!("{interaction:?}");
    assert!(!debug.contains("consent_approve"), "{debug}");
    assert!(!debug.contains("hooks.slack.com"), "{debug}");
}

#[tokio::test]
async fn verified_bodies_that_dont_parse_are_bad_requests() {
    let harness = Harness::start();
    let cases = [
        ("events", "not json".to_owned()),
        (
            "events",
            r#"{"type":"event_callback","event":{}}"#.to_owned(),
        ),
        (
            "events",
            r#"{"type":"event_callback","event_id":"Ev1","event":"x"}"#.to_owned(),
        ),
        ("commands", "team_id=T1".to_owned()),
        (
            "commands",
            "team_id=T1&channel_id=C1&user_id=&command=%2Fagent&response_url=https%3A%2F%2Fx"
                .to_owned(),
        ),
        ("interactivity", "payload=not-json".to_owned()),
        ("interactivity", "payload=%5B1%5D".to_owned()),
        ("interactivity", "nothing=here".to_owned()),
    ];
    for (kind, body) in cases {
        let uri = path(BindingRef::Manager, kind);
        let (status, _) = harness.send(signed(&uri, MANAGER_SECRET, &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}: {body}");
    }
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_verification() {
    let harness = Harness::start();
    let body = "x".repeat(surface_slack::ingress::MAX_BODY_BYTES + 1);
    let uri = path(agent(), "events");
    let (status, _) = harness.send(signed(&uri, AGENT_SECRET, &body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_failed_lookup_is_503_and_a_failed_dedup_drops_the_request() {
    let failing = Secrets {
        failing: true,
        ..Secrets::default()
    };
    let harness = Harness::with(failing, MemoryDedup::default());
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let dedup = MemoryDedup {
        failing: true,
        ..MemoryDedup::default()
    };
    let mut harness = Harness::with(secrets(), dedup);
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let nothing = tokio::time::timeout(Duration::from_millis(200), harness.out.recv()).await;
    assert!(nothing.is_err(), "a request was delivered without dedup");
}

#[tokio::test]
async fn only_post_is_served() {
    let harness = Harness::start();
    let uri = path(agent(), "events");
    let get = Request::get(&uri).body(Body::empty()).unwrap();
    assert_eq!(harness.send(get).await.0, StatusCode::METHOD_NOT_ALLOWED);
    let other = path(agent(), "oauth");
    assert_eq!(
        harness.send(signed(&other, AGENT_SECRET, "{}")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_slash_command_is_acked_within_three_seconds_while_the_handler_is_slow() {
    let (router, queue) = ingress(Arc::new(secrets()), 64);
    let (tx, mut out) = mpsc::unbounded_channel();
    let started = Arc::new(Notify::new());
    let slow = Slow {
        delay: Duration::from_secs(5),
        inner: Collect(tx),
        started: started.clone(),
    };
    tokio::spawn(queue.run(Arc::new(MemoryDedup::default()), Sender::new(slow)));

    let uri = path(BindingRef::Manager, "commands");
    let begun = Instant::now();
    let response = router
        .clone()
        .oneshot(signed(&uri, MANAGER_SECRET, fixtures::SLASH_COMMAND))
        .await
        .unwrap();
    let took = begun.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(took < Duration::from_secs(3), "the ack took {took:?}");
    assert!(took < Duration::from_secs(1), "the ack took {took:?}");

    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the handler never started");
    assert!(out.try_recv().is_err(), "the slow handler finished early");
    let handled = tokio::time::timeout(Duration::from_secs(10), out.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(handled, SlackInbound::Command(_)));
    assert!(begun.elapsed() >= Duration::from_secs(5));
}

#[tokio::test]
async fn the_ack_never_waits_for_the_queue() {
    let (router, queue) = ingress(Arc::new(secrets()), 1);
    let uri = path(agent(), "events");
    let first = fixtures::MESSAGE_MENTION.to_owned();
    let second = fixtures::with_event_id(fixtures::MESSAGE_IM, "Ev0SECOND");

    let begun = Instant::now();
    let response = router
        .clone()
        .oneshot(signed(&uri, AGENT_SECRET, &first))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .clone()
        .oneshot(signed(&uri, AGENT_SECRET, &second))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "{:?}",
        begun.elapsed()
    );

    let (tx, mut out) = mpsc::unbounded_channel();
    let worker =
        tokio::spawn(queue.run(Arc::new(MemoryDedup::default()), Sender::new(Collect(tx))));
    let SlackInbound::Message(event, _) = out.recv().await.unwrap() else {
        panic!("expected the first message");
    };
    assert_eq!(event.event_id, "Ev0MENTION1");
    let response = router
        .clone()
        .oneshot(signed(&uri, AGENT_SECRET, &second))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let SlackInbound::Message(event, _) = out.recv().await.unwrap() else {
        panic!("expected the retried message");
    };
    assert_eq!(event.event_id, "Ev0SECOND");

    drop(router);
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("the queue didn't stop when the router was dropped")
        .unwrap();
}

#[tokio::test]
async fn the_queue_stops_when_its_receiver_is_gone() {
    let mut harness = Harness::start();
    harness.out.close();
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, fixtures::MESSAGE_IM))
        .await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), &mut harness.worker)
        .await
        .expect("the queue kept running")
        .unwrap();
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A DM to an agent's app, the `n`th of its kind: its own event id and
/// `ts`.
fn nth_dm(n: usize) -> String {
    fixtures::with_event_id(fixtures::MESSAGE_IM, &format!("Ev0HELD{n}"))
        .replace("1727697900.000500", &format!("1727698000.{n:06}"))
}

#[tokio::test]
async fn each_binding_has_its_own_places_in_flight() {
    let mut harness = Harness::start();
    for n in 0..MAX_IN_FLIGHT_PER_AGENT {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, &nth_dm(n)))
            .await;
        assert_eq!(status, StatusCode::OK, "message {n}");
    }
    let past = nth_dm(MAX_IN_FLIGHT_PER_AGENT);
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &past))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    for n in 0..MAX_IN_FLIGHT_PER_AGENT {
        let (status, _) = harness
            .send(signed_events(other_agent(), OTHER_SECRET, &nth_dm(n)))
            .await;
        assert_eq!(status, StatusCode::OK, "the other agent's message {n}");
    }
    let (status, _) = harness
        .send(signed_events(third_agent(), THIRD_SECRET, &nth_dm(0)))
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the agents' apps hold every place agents' apps may"
    );
    let (status, _) = harness
        .send(signed(
            &path(BindingRef::Manager, "commands"),
            MANAGER_SECRET,
            fixtures::SLASH_COMMAND,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the agents' apps took all of theirs, and none of the manager's"
    );

    let mut held = Vec::new();
    for _ in 0..2 * MAX_IN_FLIGHT_PER_AGENT {
        let SlackInbound::Message(event, place) = harness.next().await else {
            panic!("expected the agents' messages first");
        };
        held.push((event, place));
    }
    assert!(matches!(harness.next().await, SlackInbound::Command(_)));
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &past))
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a message handed on keeps its place until it is dropped"
    );
    held.truncate(MAX_IN_FLIGHT_PER_AGENT - 1);
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &past))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        harness.message().await.event_id,
        format!("Ev0HELD{MAX_IN_FLIGHT_PER_AGENT}"),
        "the refused message wasn't recorded as handled"
    );
}

#[tokio::test]
async fn one_owners_agents_share_their_places_in_flight() {
    let ada = owner(1);
    let secrets = secrets_owned_by([ada, ada, ada, owner(2)]);
    let harness = Harness::with_capacity(secrets, MemoryDedup::default(), 1024);
    let adas = [(agent(), AGENT_SECRET), (other_agent(), OTHER_SECRET)];
    for (binding, secret) in adas {
        for n in 0..MAX_IN_FLIGHT_PER_AGENT {
            let (status, _) = harness
                .send(signed_events(binding, secret, &nth_dm(n)))
                .await;
            assert_eq!(status, StatusCode::OK, "{binding}'s message {n}");
        }
    }
    assert_eq!(
        MAX_IN_FLIGHT_PER_OWNER,
        adas.len() * MAX_IN_FLIGHT_PER_AGENT
    );
    let (status, _) = harness
        .send(signed_events(third_agent(), THIRD_SECRET, &nth_dm(0)))
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "ada's agents hold every place one owner's may"
    );
    let (status, _) = harness
        .send(signed_events(fourth_agent(), FOURTH_SECRET, &nth_dm(0)))
        .await;
    assert_eq!(status, StatusCode::OK, "another owner's agent has its own");
}

#[tokio::test]
async fn an_agents_burst_past_its_rate_gets_503_and_other_apps_are_answered() {
    let mut harness = Harness::start();
    let started = Instant::now();
    let limit = usize::try_from(AGENT_BURST).unwrap() * 2;
    let mut taken = 0;
    for n in 0..limit {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, &nth_dm(n)))
            .await;
        if status == StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        assert_eq!(status, StatusCode::OK, "message {n}");
        assert_eq!(harness.message().await.event_id, format!("Ev0HELD{n}"));
        taken += 1;
    }
    let refilled = started.elapsed().as_secs_f64() * f64::from(AGENT_REQUESTS_PER_SECOND);
    assert!(taken < limit, "none of {limit} messages was refused");
    assert!(
        taken >= usize::try_from(AGENT_BURST).unwrap(),
        "only {taken} were taken"
    );
    assert!(
        (taken as f64) <= f64::from(AGENT_BURST) + refilled + 1.0,
        "{taken} were taken"
    );
    assert_eq!(
        harness
            .recorded(&format!("slack:{}:message", agent()))
            .len(),
        taken,
        "the refused message wasn't recorded"
    );

    let (status, _) = harness
        .send(signed_events(other_agent(), OTHER_SECRET, &nth_dm(0)))
        .await;
    assert_eq!(status, StatusCode::OK, "another agent's app is answered");
    let (status, _) = harness
        .send(signed(
            &path(BindingRef::Manager, "commands"),
            MANAGER_SECRET,
            fixtures::SLASH_COMMAND,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "the manager app is answered");
}

/// A signed body that changes `fixture` with `edit`.
fn edited(fixture: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut value: serde_json::Value = serde_json::from_str(fixture).unwrap();
    edit(&mut value);
    value.to_string()
}

#[tokio::test]
async fn ids_not_shaped_like_slacks_are_refused_and_nothing_is_written() {
    let mut harness = Harness::start();
    let huge = "A".repeat(900_000);
    let mut bodies = Vec::new();
    for event_id in [
        format!("Ev{huge}"),
        format!("Ev{}", "A".repeat(33)),
        "Ev".to_owned(),
        "Ev0lower".to_owned(),
        "EV0UPPER".to_owned(),
        "0EvFIRST".to_owned(),
    ] {
        bodies.push(fixtures::with_event_id(fixtures::MESSAGE_IM, &event_id));
        bodies.push(fixtures::with_event_id(fixtures::USER_CHANGE, &event_id));
    }
    bodies.push(edited(fixtures::MESSAGE_IM, |body| {
        body.as_object_mut().unwrap().remove("event_id");
    }));
    for channel in [
        format!("D{huge}"),
        format!("C{}", "A".repeat(21)),
        "C".to_owned(),
        "c0chan001".to_owned(),
        "X0CHAN001".to_owned(),
        "C0CHAN 01".to_owned(),
    ] {
        bodies.push(edited(fixtures::MESSAGE_IM, |body| {
            body["event"]["channel"] = channel.into();
        }));
    }
    for ts in [
        format!("1727697900.{}", "0".repeat(900_000)),
        "1727697900.0005001".to_owned(),
        "172769790.000500".to_owned(),
        "1727697900000500".to_owned(),
        "1727697900.00050a".to_owned(),
        "1.2".to_owned(),
    ] {
        for field in ["ts", "thread_ts"] {
            bodies.push(edited(fixtures::MESSAGE_IM, |body| {
                body["event"][field] = ts.clone().into();
            }));
        }
    }
    bodies.push(edited(fixtures::MESSAGE_IM, |body| {
        body["event"]["channel"] = serde_json::json!({"id": "D0DM00001"});
    }));
    for team in [format!("T{huge}"), "t0team001".to_owned(), String::new()] {
        bodies.push(edited(fixtures::MESSAGE_IM, |body| {
            body["team_id"] = team.into();
        }));
    }
    for body in &bodies {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{:.200}", body);
        let (status, _) = harness
            .send(signed(
                &path(BindingRef::Manager, "events"),
                MANAGER_SECRET,
                body,
            ))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "manager: {:.200}", body);
    }
    harness.assert_nothing_delivered().await;
    assert_eq!(
        harness.recorded_anywhere(),
        1,
        "only the marker was recorded"
    );

    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_THREAD_REPLY,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "Slack's own shapes pass");
    let (status, _) = harness
        .send(signed(
            &path(BindingRef::Manager, "events"),
            MANAGER_SECRET,
            &edited(fixtures::USER_CHANGE, |body| {
                body["event"]["channel"] = serde_json::json!({"id": "not checked"});
            }),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "only a message's channel is a key, so only a message's is checked"
    );
}
