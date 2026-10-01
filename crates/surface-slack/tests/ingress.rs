//! The ingress end to end, in-process: request URLs, verification, the
//! ack, the queue, deduplication and normalization.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use core_types::{BindingId, ConvKind, MemberId, Outside, SendError, Sender, Sink, TeamId, UserId};
use futures::StreamExt as _;
use secrecy::{ExposeSecret as _, SecretString};
use surface_slack::ingress::{
    AGENT_BURST, AGENT_REQUESTS_PER_SECOND, MAX_IN_FLIGHT_PER_AGENT, MAX_IN_FLIGHT_PER_OWNER,
    OWNER_BURST, OWNER_REQUESTS_PER_SECOND, PRE_ACK_TIMEOUT,
};
use surface_slack::normalize::{MAX_ID_TAIL, MAX_TEXT_BYTES};
use surface_slack::{
    AgentApp, BindingRef, BoxError, Dedup, SigningSecrets, SlackApp, SlackInbound, ingress,
};
use testkit::Logs;
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
    manager: Option<SlackApp>,
    agents: HashMap<BindingId, AgentApp>,
    failing: bool,
    delay: Duration,
}

impl Secrets {
    async fn looked_up(&self) -> Result<(), BoxError> {
        tokio::time::sleep(self.delay).await;
        if self.failing {
            return Err("the store is down".into());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SigningSecrets for Secrets {
    async fn manager(&self) -> Result<Option<SlackApp>, BoxError> {
        self.looked_up().await?;
        Ok(self.manager.clone())
    }

    async fn agent(&self, binding: BindingId) -> Result<Option<AgentApp>, BoxError> {
        self.looked_up().await?;
        Ok(self.agents.get(&binding).cloned())
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
    }
}

fn agent_app(secret: &str, bot_user: &str, owner: MemberId) -> AgentApp {
    AgentApp {
        app: app(Some(secret), Some(bot_user)),
        owner,
    }
}

/// The manager app, and four agents' apps whose owners are `owners`, in
/// order, besides one being created.
fn secrets_owned_by(owners: [MemberId; 4]) -> Secrets {
    let [first, second, third, fourth] = owners;
    Secrets {
        manager: Some(app(Some(MANAGER_SECRET), None)),
        agents: HashMap::from([
            (agent(), agent_app(AGENT_SECRET, BOT_USER, first)),
            (other_agent(), agent_app(OTHER_SECRET, "U0BOT0002", second)),
            (third_agent(), agent_app(THIRD_SECRET, "U0BOT0003", third)),
            (
                fourth_agent(),
                agent_app(FOURTH_SECRET, "U0BOT0004", fourth),
            ),
            (
                creating_agent(),
                AgentApp {
                    app: app(None, None),
                    owner: first,
                },
            ),
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
        Self::serving(secrets, dedup, capacity, None)
    }

    /// A harness whose queue serves `workspace`, when given.
    fn serving(
        secrets: Secrets,
        dedup: MemoryDedup,
        capacity: usize,
        workspace: Option<&str>,
    ) -> Self {
        let (router, queue) = ingress(Arc::new(secrets), capacity);
        let queue = match workspace {
            Some(workspace) => queue.with_workspace(workspace.into(), None),
            None => queue,
        };
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

    /// Sends the `n`th marker to the fourth agent's app, and returns the
    /// messages delivered before it, since the queue keeps order.
    async fn delivered_before_marker(&mut self, n: usize) -> Vec<core_types::InboundEvent> {
        let (status, _) = self
            .send(signed_events(fourth_agent(), FOURTH_SECRET, &nth_dm(n)))
            .await;
        assert_eq!(status, StatusCode::OK, "marker {n}");
        let mut delivered = Vec::new();
        loop {
            let event = self.message().await;
            if event.binding == fourth_agent() {
                return delivered;
            }
            delivered.push(event);
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

/// `body`, made [`fresh`], signed at `timestamp`.
fn signed_at(uri: &str, secret: &str, body: &str, timestamp: i64) -> Request<Body> {
    let body = fresh(body);
    request(
        uri,
        &body,
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
    assert_eq!(event.message.id.as_str(), fresh("1727697600.000100"));
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
    let body = fresh(fixtures::MESSAGE_MENTION);
    let mut headers =
        fixtures::signed_headers(AGENT_SECRET, fixtures::now(), body.as_bytes()).to_vec();
    headers.push(("x-slack-retry-num", "1".to_owned()));
    headers.push(("x-slack-retry-reason", "http_timeout".to_owned()));
    let (status, _) = harness.send(request(&uri, &body, &headers)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.message().await.event_id, "Ev0MENTION1");
    harness.assert_nothing_delivered().await;
    assert!(harness.recorded(&format!("slack:{}", agent())).is_empty());
    let messages = harness.recorded(&format!("slack:{}:message", agent()));
    assert!(
        messages.contains(&fresh(&format!("{}:1727697600.000100", fixtures::CHANNEL))),
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

    let for_other = edited(fixtures::MESSAGE_THREAD_REPLY, |body| {
        body["event"]
            .as_object_mut()
            .unwrap()
            .remove("parent_user_id");
    });
    let for_other = for_other.as_str();
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
    let send = |body: &str| signed_events(agent(), AGENT_SECRET, body);

    for dropped in [
        fixtures::MESSAGE_PLAIN,
        fixtures::MESSAGE_CHANGED,
        fixtures::MESSAGE_MPIM,
    ] {
        assert_eq!(harness.send(send(dropped)).await.0, StatusCode::OK);
    }
    harness.assert_nothing_delivered().await;

    harness.send(send(fixtures::MESSAGE_THREAD_REPLY)).await;
    let reply = harness.message().await;
    assert_eq!(reply.conv_kind, ConvKind::Channel);
    assert_eq!(
        reply.thread_root.as_ref().map(|root| root.as_str()),
        Some(fresh("1727697650.000150").as_str())
    );
    assert_eq!(
        reply.reply_to.as_ref().map(|to| to.id.as_str()),
        Some(fresh("1727697650.000150").as_str())
    );
    assert!(reply.mentions.is_empty());

    harness.send(send(fixtures::MESSAGE_THREAD_BROADCAST)).await;
    let broadcast = harness.message().await;
    assert_eq!(
        broadcast.thread_root.as_ref().map(|root| root.as_str()),
        Some(fresh("1727697650.000150").as_str())
    );
    assert_eq!(broadcast.message.id.as_str(), fresh("1727697800.000400"));

    harness.send(send(fixtures::MESSAGE_IM)).await;
    let dm = harness.message().await;
    assert_eq!(dm.conv_kind, ConvKind::Dm);
    assert!(dm.is_dm());
    assert!(dm.mentions.is_empty());

    let group_dm_mention = edited(fixtures::MESSAGE_MPIM, |body| {
        body["event"]["text"] = format!("<@{BOT_USER}> can you check the build?").into();
    });
    harness.send(send(&group_dm_mention)).await;
    let group_dm = harness.message().await;
    assert_eq!(group_dm.conv_kind, ConvKind::GroupDm);
    assert_eq!(group_dm.mentions, [UserId::from(BOT_USER)]);

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
            assert_eq!(event.team.as_str(), TEAM);
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
        format!("Ev{}", "A".repeat(MAX_ID_TAIL + 1)),
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
        format!("C{}", "A".repeat(MAX_ID_TAIL + 1)),
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
        "01727697900.000500".to_owned(),
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

/// `body` with every `ts`-shaped value (10 digits of seconds and a dot)
/// from 2024 moved by one offset, so that the earliest fixture's is a
/// minute old: Slack's fixtures date from 2024, and an agent's app ignores
/// a message older than the confirmation window. Later values are left as
/// they are, so a fresh body stays as it is.
fn fresh(body: &str) -> String {
    static OFFSET: std::sync::LazyLock<i64> =
        std::sync::LazyLock::new(|| fixtures::now() - 60 - 1_727_697_600);
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut at = 0;
    while at < bytes.len() {
        let starts = !at
            .checked_sub(1)
            .is_some_and(|before| bytes[before].is_ascii_digit());
        let seconds = bytes.get(at..at + 10);
        if starts
            && seconds.is_some_and(|digits| digits.iter().all(u8::is_ascii_digit))
            && bytes.get(at + 10) == Some(&b'.')
        {
            let seconds: i64 = body[at..at + 10].parse().unwrap();
            let moved = if seconds < 1_735_689_600 {
                seconds + *OFFSET
            } else {
                seconds
            };
            out.push_str(&moved.to_string());
            at += 10;
            continue;
        }
        let ch = body[at..].chars().next().unwrap();
        out.push(ch);
        at += ch.len_utf8();
    }
    out
}

/// A fresh DM to an agent's app, changed by `edit`.
fn fresh_dm(event_id: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    edited(
        &fresh(&fixtures::with_event_id(fixtures::MESSAGE_IM, event_id)),
        edit,
    )
}

#[tokio::test]
async fn a_sender_not_shaped_like_slacks_is_acked_and_dropped_with_nothing_written() {
    let logs = Logs::global();
    let mut harness = Harness::start();
    let huge = "A".repeat(900_000);
    let mut bodies = Vec::new();
    for bot_id in [
        format!("B{huge}"),
        format!("B{}", "A".repeat(MAX_ID_TAIL + 1)),
        "B".to_owned(),
        "b0lower".to_owned(),
        "U0HUMAN01".to_owned(),
    ] {
        bodies.push(fresh_dm("Ev0BADBOT", |body| {
            let event = body["event"].as_object_mut().unwrap();
            event.remove("user");
            event.insert("bot_id".into(), bot_id.clone().into());
        }));
        bodies.push(fresh_dm("Ev0BADBOT", |body| {
            body["event"]["bot_id"] = bot_id.into();
        }));
    }
    for user in [
        format!("U{huge}"),
        format!("W{}", "A".repeat(MAX_ID_TAIL + 1)),
        "U".to_owned(),
        "u0lower".to_owned(),
        "B0LEGACY1".to_owned(),
    ] {
        bodies.push(fresh_dm("Ev0BADUSER", |body| {
            body["event"]["user"] = user.into();
        }));
    }
    for body in &bodies {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, body))
            .await;
        assert_eq!(status, StatusCode::OK, "{:.200}", body);
        let (status, _) = harness
            .send(signed(
                &path(BindingRef::Manager, "events"),
                MANAGER_SECRET,
                body,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "manager: {:.200}", body);
    }
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
    let malformed = logs
        .snapshot()
        .matching("dropped a Slack message not shaped like Slack's")
        .matching(&format!("binding={}", agent()));
    malformed.assert_has("WARN");
    malformed.assert_has("DEBUG");

    let longer = format!("B0{}", "A".repeat(MAX_ID_TAIL - 1));
    let bot = fresh_dm("Ev0GOODBOT", |body| {
        let event = body["event"].as_object_mut().unwrap();
        event.remove("user");
        event.insert("bot_id".into(), longer.clone().into());
        event.insert("text".into(), format!("<@{BOT_USER}> done").into());
    });
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &bot))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.message().await.sender.user.as_str(), longer);
}

#[tokio::test]
async fn a_message_keeps_ten_files_160_kb_of_text_and_short_mention_ids() {
    let mut harness = Harness::start();
    let long_user = format!("U{}", "A".repeat(MAX_ID_TAIL + 1));
    let body = fresh_dm("Ev0BIG", |body| {
        let files: Vec<serde_json::Value> = (0..5_000)
            .map(|n| {
                serde_json::json!({
                    "id": format!("F0FILE{n}"),
                    "url_private": format!("https://files.slack.com/F0FILE{n}"),
                })
            })
            .collect();
        body["event"]["files"] = files.into();
        body["event"]["subtype"] = "file_share".into();
        body["event"]["text"] =
            format!("<@{long_user}> <@U0SHORT1> {}", "é".repeat(100_000)).into();
    });
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &body))
        .await;
    assert_eq!(status, StatusCode::OK);
    let event = harness.message().await;
    assert_eq!(event.files.len(), 10);
    assert_eq!(event.files[9].id, "F0FILE9");
    assert!(event.text.len() <= MAX_TEXT_BYTES);
    assert!(event.text.len() > MAX_TEXT_BYTES - 2);
    assert!(event.text.ends_with('é'));
    assert_eq!(event.mentions, [UserId::from("U0SHORT1")]);
}

#[tokio::test]
async fn an_agents_apps_other_requests_are_acked_and_write_nothing() {
    let mut harness = Harness::start();
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, fixtures::USER_CHANGE))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .send(signed(
            &path(agent(), "commands"),
            AGENT_SECRET,
            fixtures::SLASH_COMMAND,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .send(signed(
            &path(agent(), "interactivity"),
            AGENT_SECRET,
            &fixtures::interactivity_body(fixtures::BLOCK_ACTIONS),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
}

#[tokio::test]
async fn an_agents_message_older_than_the_window_is_acked_writes_nothing_and_is_warned_of() {
    let logs = Logs::global();
    let mut harness = Harness::start();
    for n in 0..3 {
        let stale = format!("{}.{n:06}", fixtures::now() - 16 * 60);
        let body = fresh_dm(&format!("Ev0STALE{n}"), |body| {
            body["event"]["ts"] = stale.into();
        });
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, &body))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
    let stale = logs
        .snapshot()
        .matching("older than the confirmation window")
        .matching(&format!("binding={}", agent()));
    stale.assert_has("WARN");
    let lines = stale.to_string();
    let count = |level: &str| lines.lines().filter(|line| line.contains(level)).count();
    assert_eq!(
        (count("WARN"), count("DEBUG")),
        (1, 2),
        "one warning, the rest throttled:\n{lines}"
    );

    let recent = format!("{}.000100", fixtures::now() - 10 * 60);
    let body = fresh_dm("Ev0RECENT", |body| {
        body["event"]["ts"] = recent.into();
    });
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &body))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.message().await.event_id, "Ev0RECENT");
}

#[tokio::test]
async fn one_owners_agents_in_busy_channels_miss_no_mention() {
    let ada = owner(1);
    let mut agents = vec![(agent(), AGENT_SECRET.to_owned())];
    let mut secrets = Secrets::default();
    secrets
        .agents
        .insert(agent(), agent_app(AGENT_SECRET, BOT_USER, ada));
    for n in 1..10_u128 {
        let binding = BindingId::from_uuid(uuid::Uuid::from_u128(0xB05E_0000 + n));
        let secret = format!("busy-agent-signing-secret-{n}");
        secrets
            .agents
            .insert(binding, agent_app(&secret, &format!("U0BUSY{n:03}"), ada));
        agents.push((binding, secret));
    }
    let mut harness = Harness::with(secrets, MemoryDedup::default());
    let rounds = 40;
    let mut markers = 0;
    for round in 0..rounds {
        for (binding, secret) in &agents {
            let (status, _) = harness
                .send(signed_events(*binding, secret, fixtures::MESSAGE_PLAIN))
                .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "round {round}: {binding}'s unaddressed channel message"
            );
        }
        if round % 5 == 4 {
            harness.assert_nothing_delivered().await;
            markers += 1;
        }
    }
    assert!(agents.len() * rounds >= 2 * usize::try_from(OWNER_BURST).unwrap());
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "the mention");
    assert_eq!(harness.message().await.event_id, "Ev0MENTION1");
    assert_eq!(harness.recorded_anywhere(), markers + 1);
}

/// A thread reply in a channel, the `n`th of its kind, under a root that
/// `parent` posted: its own event id, `ts` and thread.
fn nth_thread_reply(n: usize, parent: &str) -> String {
    edited(fixtures::MESSAGE_THREAD_REPLY, |body| {
        body["event_id"] = format!("Ev0REPLY{n}").into();
        let event = &mut body["event"];
        event["ts"] = format!("1727697700.{n:06}").into();
        event["thread_ts"] = format!("1727697650.{n:06}").into();
        event["parent_user_id"] = parent.into();
    })
}

#[tokio::test]
async fn one_owners_agents_in_busy_threads_miss_no_mention() {
    let ada = owner(1);
    let mut agents = vec![(agent(), AGENT_SECRET.to_owned(), BOT_USER.to_owned())];
    let mut secrets = Secrets::default();
    secrets
        .agents
        .insert(agent(), agent_app(AGENT_SECRET, BOT_USER, ada));
    for n in 1..10_u128 {
        let binding = BindingId::from_uuid(uuid::Uuid::from_u128(0xB05E_0000 + n));
        let secret = format!("busy-agent-signing-secret-{n}");
        let bot = format!("U0BUSY{n:03}");
        secrets
            .agents
            .insert(binding, agent_app(&secret, &bot, ada));
        agents.push((binding, secret, bot));
    }
    secrets.agents.insert(
        fourth_agent(),
        agent_app(FOURTH_SECRET, "U0BOT0004", owner(2)),
    );
    let mut harness = Harness::with(secrets, MemoryDedup::default());
    let rounds = 40;
    let mut kept = Vec::new();
    let mut expected = Vec::new();
    for round in 0..rounds {
        let root_by = if round % 2 == 0 {
            fixtures::OTHER_USER
        } else {
            let (binding, _, bot) = &agents[(round / 2) % agents.len()];
            expected.push((*binding, fresh(&format!("1727697700.{round:06}"))));
            bot.as_str()
        };
        let reply = nth_thread_reply(round, root_by);
        for (binding, secret, _) in &agents {
            let (status, _) = harness.send(signed_events(*binding, secret, &reply)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "round {round}: {binding}'s thread reply"
            );
        }
        kept.extend(
            harness
                .delivered_before_marker(round)
                .await
                .into_iter()
                .map(|event| (event.binding, event.message.id.as_str().to_owned())),
        );
    }
    assert!(agents.len() * rounds >= 2 * usize::try_from(OWNER_BURST).unwrap());
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "the mention");
    let after = harness.delivered_before_marker(rounds).await;
    assert!(
        after.iter().any(|event| event.event_id == "Ev0MENTION1"),
        "the mention was dropped after {} thread replies were kept",
        kept.len()
    );
    assert_eq!(
        kept, expected,
        "only a reply under an agent's own root is kept, by that agent's app"
    );
    let rows: usize = agents
        .iter()
        .map(|(binding, _, _)| harness.recorded(&format!("slack:{binding}:message")).len())
        .sum();
    assert_eq!(rows, expected.len() + 1);
}

#[tokio::test]
async fn the_agents_own_posts_and_bots_not_mentioning_it_are_dropped_without_a_row() {
    let mut harness = Harness::start();
    let own = |event_id: &str, fixture: &str, extra: serde_json::Value| {
        edited(fixture, |body| {
            body["event_id"] = event_id.into();
            let event = body["event"].as_object_mut().unwrap();
            event.insert("user".into(), BOT_USER.into());
            event.insert("bot_id".into(), "B0AGENT01".into());
            event.insert("text".into(), format!("<@{BOT_USER}> done").into());
            for (key, value) in extra.as_object().unwrap() {
                event.insert(key.clone(), value.clone());
            }
        })
    };
    let other_bot = |event_id: &str, fixture: &str| {
        edited(fixture, |body| {
            body["event_id"] = event_id.into();
            let event = body["event"].as_object_mut().unwrap();
            event.insert("user".into(), "U0BOT0002".into());
            event.insert("bot_id".into(), "B0OTHER01".into());
            event.insert("text".into(), "deploy finished".into());
            event.remove("blocks");
        })
    };
    let dropped = [
        own(
            "Ev0OWNTHREAD",
            fixtures::MESSAGE_THREAD_REPLY,
            serde_json::json!({}),
        ),
        own("Ev0OWNDM", fixtures::MESSAGE_IM, serde_json::json!({})),
        own(
            "Ev0OWNROOT",
            fixtures::MESSAGE_MENTION,
            serde_json::json!({"thread_ts": "1727697600.000100"}),
        ),
        other_bot("Ev0BOTTHREAD", fixtures::MESSAGE_THREAD_REPLY),
        other_bot("Ev0BOTDM", fixtures::MESSAGE_IM),
        other_bot("Ev0BOTMPIM", fixtures::MESSAGE_MPIM),
    ];
    for body in &dropped {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, body))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
}

#[tokio::test]
async fn a_thread_reply_under_the_agents_own_root_is_kept() {
    let mut harness = Harness::start();
    let root = fresh("1727697650.000150");
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_THREAD_REPLY,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let reply = harness.message().await;
    assert_eq!(reply.event_id, "Ev0THREAD01");
    assert!(reply.mentions.is_empty());
    assert_eq!(
        reply.thread_root.as_ref().map(|root| root.as_str()),
        Some(root.as_str())
    );
    assert_eq!(
        reply.reply_to.as_ref().map(|to| to.id.as_str()),
        Some(root.as_str())
    );

    let (status, _) = harness
        .send(signed_events(
            other_agent(),
            OTHER_SECRET,
            fixtures::MESSAGE_THREAD_REPLY,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "another agent's app");
    harness.assert_nothing_delivered().await;
    assert!(
        harness
            .recorded(&format!("slack:{}:message", other_agent()))
            .is_empty()
    );
}

#[tokio::test]
async fn messages_one_owner_keeps_past_their_rate_are_acked_and_dropped_without_a_row() {
    let logs = Logs::global();
    let ada = owner(1);
    let secrets = secrets_owned_by([ada, ada, ada, owner(2)]);
    let mut harness = Harness::with_capacity(secrets, MemoryDedup::default(), 1024);
    let adas = [
        (agent(), AGENT_SECRET),
        (other_agent(), OTHER_SECRET),
        (third_agent(), THIRD_SECRET),
    ];
    let started = Instant::now();
    let (mut sent, mut kept, mut rounds) = (0, 0, 0);
    while kept == sent {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "all {sent} messages were kept"
        );
        for (binding, secret) in adas {
            let (status, _) = harness
                .send(signed_events(binding, secret, &nth_dm(rounds)))
                .await;
            assert_eq!(status, StatusCode::OK, "{binding}'s message {rounds}");
        }
        sent += adas.len();
        kept += harness.delivered_before_marker(rounds).await.len();
        rounds += 1;
    }
    let refilled = started.elapsed().as_secs_f64() * f64::from(OWNER_REQUESTS_PER_SECOND);
    assert!(kept >= usize::try_from(OWNER_BURST).unwrap(), "{kept}");
    assert!(
        (kept as f64) <= f64::from(OWNER_BURST) + refilled + 1.0,
        "{kept}"
    );
    let rows: usize = adas
        .iter()
        .map(|(binding, _)| harness.recorded(&format!("slack:{binding}:message")).len())
        .sum();
    assert_eq!(
        rows, kept,
        "a message dropped for its owner's rate writes no row"
    );
    assert_eq!(
        harness
            .recorded(&format!("slack:{}:message", fourth_agent()))
            .len(),
        rounds,
        "another owner's messages are all kept"
    );
    logs.snapshot()
        .matching("keeping events faster than their rate")
        .assert_has("WARN");
}

#[tokio::test]
async fn the_workspace_is_the_installation_not_the_envelope_team() {
    let mut harness = Harness::start();
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_CONNECT_THEIR_TEAM,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let event = harness.message().await;
    assert_eq!(event.sender.team.as_str(), TEAM);
    assert_eq!(event.sender.user.as_str(), fixtures::OUTSIDE_USER);
    assert_eq!(event.conv.team.as_str(), TEAM);
    assert_eq!(event.message.conv.team.as_str(), TEAM);
    assert_eq!(
        event.outside,
        Some(Outside {
            team: Some(fixtures::OUTSIDE_TEAM.into())
        })
    );

    let elsewhere = edited(fixtures::USER_CHANGE, |body| {
        body["team_id"] = fixtures::OUTSIDE_TEAM.into();
        body["authorizations"][0]["team_id"] = "T0ELSE001".into();
    });
    let (status, _) = harness
        .send(signed(
            &path(BindingRef::Manager, "events"),
            MANAGER_SECRET,
            &elsewhere,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    match harness.next().await {
        SlackInbound::Event(event) => assert_eq!(event.team.as_str(), "T0ELSE001"),
        other => panic!("expected an event, got {other:?}"),
    }
}

#[tokio::test]
async fn an_event_without_an_installation_team_is_dropped() {
    let logs = Logs::global();
    let mut harness = Harness::start();
    let uninstalled = |n: usize, authorizations: Option<serde_json::Value>| {
        edited(
            &fixtures::with_event_id(fixtures::MESSAGE_MENTION, &format!("Ev0NOINST{n}")),
            |body| match authorizations {
                Some(authorizations) => body["authorizations"] = authorizations,
                None => {
                    body.as_object_mut().unwrap().remove("authorizations");
                }
            },
        )
    };
    let cases = [
        None,
        Some(serde_json::Value::Null),
        Some(serde_json::json!([])),
        Some(serde_json::json!([{}])),
        Some(serde_json::json!([{"team_id": null}])),
        Some(serde_json::json!([{"team_id": "not-a-team"}])),
        Some(serde_json::json!([{"team_id": 7}])),
        Some(serde_json::json!([null, {"team_id": TEAM}])),
        Some(serde_json::json!({"team_id": TEAM})),
        Some(serde_json::json!("T0TEAM001")),
    ];
    let count = cases.len();
    for (n, authorizations) in cases.into_iter().enumerate() {
        let body = uninstalled(n, authorizations);
        let (status, body) = harness
            .send(signed_events(agent(), AGENT_SECRET, &body))
            .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, ""), "case {n}");
    }
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_WITHOUT_AUTHORIZATIONS,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let manager = edited(fixtures::USER_CHANGE, |body| {
        body.as_object_mut().unwrap().remove("authorizations");
    });
    let (status, _) = harness
        .send(signed(
            &path(BindingRef::Manager, "events"),
            MANAGER_SECRET,
            &manager,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
    assert!(harness.recorded("slack:manager").is_empty());

    let dropped = logs
        .snapshot()
        .matching("name no installation team")
        .matching(&format!("binding={}", agent()));
    let lines = dropped.to_string();
    let level = |level: &str| lines.lines().filter(|line| line.contains(level)).count();
    assert_eq!(
        (level("WARN"), level("DEBUG")),
        (1, count),
        "one warning per binding, the rest throttled:\n{lines}"
    );
    logs.snapshot()
        .matching("name no installation team")
        .matching("binding=manager")
        .assert_has("WARN");

    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            &edited(fixtures::MESSAGE_MENTION, |body| {
                body["authorizations"][0]["team_id"] = TEAM.into();
                body["authorizations"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"team_id": "nonsense", "extra": [1, 2]}));
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        harness.message().await.event_id,
        "Ev0MENTION1",
        "only the first authorization is read"
    );
}

#[tokio::test]
async fn deduplication_keys_are_unchanged_in_shared_channels() {
    let mut harness = Harness::start();
    let theirs = edited(
        &fixtures::with_event_id(fixtures::MESSAGE_CONNECT_NO_ACTOR_TEAM, "Ev0CONNECTX"),
        |body| {
            body["team_id"] = fixtures::OUTSIDE_TEAM.into();
            body["context_team_id"] = fixtures::OUTSIDE_TEAM.into();
            body["event"]["team"] = fixtures::OUTSIDE_TEAM.into();
        },
    );
    for body in [fixtures::MESSAGE_CONNECT_NO_ACTOR_TEAM, theirs.as_str()] {
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, body))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(harness.message().await.event_id, "Ev0CONNECT1");
    harness.assert_nothing_delivered().await;
    let messages = harness.recorded(&format!("slack:{}:message", agent()));
    assert!(
        messages.contains(&fresh(&format!(
            "{}:1727697800.000100",
            fixtures::SHARED_CHANNEL
        ))),
        "keyed by channel and ts only: {messages:?}"
    );
    assert!(harness.recorded(&format!("slack:{}", agent())).is_empty());

    let manager_events = path(BindingRef::Manager, "events");
    for (event_id, team) in [
        ("Ev0SHAREDEV", TEAM),
        ("Ev0SHAREDEV", fixtures::OUTSIDE_TEAM),
    ] {
        let body = edited(
            &fixtures::with_event_id(fixtures::USER_CHANGE, event_id),
            |body| {
                body["team_id"] = team.into();
            },
        );
        let (status, _) = harness
            .send(signed(&manager_events, MANAGER_SECRET, &body))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert!(matches!(harness.next().await, SlackInbound::Event(_)));
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded("slack:manager"), ["Ev0SHAREDEV"]);
}

#[tokio::test]
async fn an_interactions_sender_team_is_its_users_team_id() {
    let mut harness = Harness::start();
    let uri = path(BindingRef::Manager, "interactivity");
    for (fixture, sender_team) in [
        (fixtures::BLOCK_ACTIONS, Some(TEAM)),
        (fixtures::BLOCK_ACTIONS_WITHOUT_USER_TEAM, None),
        (
            fixtures::BLOCK_ACTIONS_OUTSIDE,
            Some(fixtures::OUTSIDE_TEAM),
        ),
    ] {
        let body = fixtures::interactivity_body(fixture);
        let (status, _) = harness.send(signed(&uri, MANAGER_SECRET, &body)).await;
        assert_eq!(status, StatusCode::OK);
        let SlackInbound::Interaction(interaction) = harness.next().await else {
            panic!("expected an interaction");
        };
        assert_eq!(
            interaction.sender_team.as_ref().map(TeamId::as_str),
            sender_team
        );
        assert_eq!(
            interaction
                .sender
                .as_ref()
                .map(|sender| sender.team.as_str()),
            Some(TEAM),
            "the payload's team.id keys the clicker"
        );
    }
    let malformed = edited(fixtures::BLOCK_ACTIONS, |payload| {
        payload["user"]["team_id"] = "not a team".into();
    });
    let (status, _) = harness
        .send(signed(
            &uri,
            MANAGER_SECRET,
            &fixtures::interactivity_body(&malformed),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let SlackInbound::Interaction(interaction) = harness.next().await else {
        panic!("expected an interaction");
    };
    assert_eq!(interaction.sender_team, None);
}

#[tokio::test]
async fn an_event_installed_elsewhere_takes_no_deduplication_key() {
    let logs = Logs::global();
    let mut harness = Harness::serving(secrets(), MemoryDedup::default(), 64, Some(TEAM));
    let elsewhere = edited(
        &fixtures::with_event_id(fixtures::MESSAGE_MENTION, "Ev0ELSEWHR"),
        |body| body["authorizations"][0]["team_id"] = "T0ELSE001".into(),
    );
    let (status, _) = harness
        .send(signed_events(agent(), AGENT_SECRET, &elsewhere))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::MESSAGE_MENTION,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let event = harness.message().await;
    assert_eq!(
        event.event_id, "Ev0MENTION1",
        "the home installation's delivery of the same message is kept"
    );
    assert_eq!(event.sender.team.as_str(), TEAM);
    harness.assert_nothing_delivered().await;
    logs.snapshot()
        .matching("installed in another workspace")
        .matching(&format!("binding={}", agent()))
        .assert_has("WARN");
}

#[tokio::test]
async fn an_agents_channel_id_change_is_queued_once_by_event_id() {
    let mut harness = Harness::start();
    let send = |body: String| harness.send(signed_events(agent(), AGENT_SECRET, &body));
    let (status, body) = send(fixtures::CHANNEL_ID_CHANGED.to_owned()).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    match harness.next().await {
        SlackInbound::ChannelIdChanged(changed) => {
            assert_eq!(changed.binding, agent());
            assert_eq!(changed.team.as_str(), TEAM);
            assert_eq!(changed.event_id, "Ev0CHANID1");
            assert_eq!(changed.old.as_str(), fixtures::PRIVATE_CHANNEL);
            assert_eq!(changed.new.as_str(), fixtures::PRIVATE_CHANNEL_SHARED);
        }
        other => panic!("expected the channel id change, got {other:?}"),
    }
    assert_eq!(
        harness.recorded(&format!("slack:{}", agent())),
        ["Ev0CHANID1"]
    );
    let (status, _) = harness
        .send(signed_events(
            agent(),
            AGENT_SECRET,
            fixtures::CHANNEL_ID_CHANGED,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "Slack's retry");
    harness.assert_nothing_delivered().await;

    let renamed = fixtures::with_event_id(fixtures::CHANNEL_ID_CHANGED, "Ev0CHANID2");
    let (status, _) = harness
        .send(signed_events(other_agent(), OTHER_SECRET, &renamed))
        .await;
    assert_eq!(status, StatusCode::OK);
    match harness.next().await {
        SlackInbound::ChannelIdChanged(changed) => assert_eq!(changed.binding, other_agent()),
        other => panic!("expected the other agent's change, got {other:?}"),
    }
}

#[tokio::test]
async fn a_channel_id_change_without_two_channel_ids_is_refused_and_writes_nothing() {
    let mut harness = Harness::start();
    let bad: [fn(&mut serde_json::Value); 7] = [
        |body| body["event"]["old_channel_id"] = "U0HUMAN01".into(),
        |body| body["event"]["new_channel_id"] = "c0lower01".into(),
        |body| body["event"]["new_channel_id"] = format!("C{}", "A".repeat(MAX_ID_TAIL + 1)).into(),
        |body| {
            body["event"].as_object_mut().unwrap().remove("old_channel_id");
        },
        |body| {
            body["event"].as_object_mut().unwrap().remove("new_channel_id");
        },
        |body| body["event"]["new_channel_id"] = serde_json::Value::Null,
        |body| body["event"]["old_channel_id"] = 7.into(),
    ];
    for (n, edit) in bad.into_iter().enumerate() {
        let body = edited(fixtures::CHANNEL_ID_CHANGED, edit);
        let (status, _) = harness
            .send(signed_events(agent(), AGENT_SECRET, &body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {n}: {body}");
    }
    harness.assert_nothing_delivered().await;
    assert_eq!(harness.recorded_anywhere(), 1, "only the marker");
}

