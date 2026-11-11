//! The Slack request URLs on agentd's public listener, with the store for
//! deduplication, and the manager app against a wiremock Slack.

mod common;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd::server::{Routers, Server, Worker, public_router};
use agentd::{App, Config, slack};
use core_types::{SendError, Sender, Sink};
use serde_json::{Value, json};
use surface_slack::{BindingRef, SlackInbound};
use testkit::slack as fixtures;
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tower::util::MapRequestLayer;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use common::{CONFIG, Response, env};

const SECRET: &str = "manager-signing-secret";
const BOT_TOKEN: &str = "xoxb-manager-test-token";

struct Collect(mpsc::UnboundedSender<SlackInbound>);

#[async_trait::async_trait]
impl Sink<SlackInbound> for Collect {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        self.0.send(item).map_err(|_| SendError)
    }
}

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

/// A Slack Web API that knows the manager app's bot token, and takes
/// posts, DMs and `response_url` replies.
async fn fake_slack() -> MockServer {
    let slack = MockServer::start().await;
    let bearer = format!("Bearer {BOT_TOKEN}");
    for (name, body) in [
        (
            "auth.test",
            json!({"team_id": fixtures::TEAM, "user_id": "U0MANAGER", "bot_id": "B0MANAGER"}),
        ),
        (
            "bots.info",
            json!({"bot": {"id": "B0MANAGER", "app_id": "A0MANAGER", "name": "agent-core", "user_id": "U0MANAGER"}}),
        ),
        ("chat.postMessage", json!({"ts": "1727700000.000100"})),
        (
            "conversations.open",
            json!({"channel": {"id": "D0DM00001"}}),
        ),
        ("users.list", json!({"members": []})),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/api/{name}")))
            .and(header("authorization", bearer.as_str()))
            .respond_with(ok(body))
            .mount(&slack)
            .await;
    }
    Mock::given(method("POST"))
        .and(path_regex("^/hooks/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&slack)
        .await;
    slack
}

/// The configuration, with the manager app's secrets and its Web API at
/// `slack` when given.
fn config(slack: Option<&MockServer>) -> Config {
    let mut env = env();
    let mut text = CONFIG.to_owned();
    if let Some(slack) = slack {
        env.push((
            "AGENTD_SLACK_MANAGER_SIGNING_SECRET".to_owned(),
            SECRET.to_owned(),
        ));
        env.push((
            "AGENTD_SLACK_MANAGER_BOT_TOKEN".to_owned(),
            BOT_TOKEN.to_owned(),
        ));
        text.push_str(&format!("\n[slack]\napi_url = \"{}/api/\"\n", slack.uri()));
    }
    Config::parse(&text, env).unwrap()
}

/// The slash command fixture with `text`, answered at `slack`'s
/// `/hooks/<name>` rather than at hooks.slack.com.
fn slash_command(slack: &MockServer, text: &str, name: &str) -> String {
    let hook = format!("{}/hooks/{name}", slack.uri())
        .replace(':', "%3A")
        .replace('/', "%2F");
    fixtures::SLASH_COMMAND
        .replace(
            "https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT0TEAM001%2F7800000000001%2FfakeResponseUrlToken",
            &hook,
        )
        .replace("text=create+helper+You+are+terse.", &format!("text={text}"))
}

async fn wait_for_request(slack: &MockServer, wanted: &str) -> Request {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = slack
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|request| request.url.path() == wanted);
        if let Some(request) = found {
            return request;
        }
        assert!(Instant::now() < deadline, "nothing reached {wanted}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Running {
    app: App,
    public: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn start(app: App, routers: Routers) -> Self {
        let server = Server::bind(app.clone(), routers).await.unwrap();
        let public = server.addrs().public;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(
            async {
                let _ = stopped.await;
            },
            std::future::pending(),
        ));
        Self {
            app,
            public,
            stop,
            task,
        }
    }

    async fn post(&self, path: &str, headers: Vec<(&'static str, String)>, body: &str) -> Response {
        let (addr, path, body) = (self.public, path.to_owned(), body.to_owned());
        tokio::task::spawn_blocking(move || common::post(addr, &path, &headers, &body))
            .await
            .unwrap()
            .expect("no response")
    }

    async fn signed(&self, path: &str, body: &str) -> Response {
        let headers = fixtures::signed_headers(SECRET, fixtures::now(), body.as_bytes()).to_vec();
        self.post(path, headers, body).await
    }

    async fn stop(self) -> App {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.app
    }
}

/// Routers like [`Routers::new`], but with the Slack queue feeding `out`.
fn routers_into(app: &App, out: mpsc::UnboundedSender<SlackInbound>) -> Routers {
    let mut routers = Routers::new(app).unwrap();
    let (slack_routes, queue) = slack::routes(app);
    routers.public = public_router(app.clone()).merge(slack_routes);
    routers.workers = vec![Worker::new(
        "Slack queue",
        slack::run_queue(queue, app.store().clone(), Sender::new(Collect(out))),
    )];
    routers
}

async fn next(out: &mut mpsc::UnboundedReceiver<SlackInbound>) -> SlackInbound {
    tokio::time::timeout(Duration::from_secs(5), out.recv())
        .await
        .expect("nothing was delivered")
        .expect("the queue stopped")
}

#[tokio::test]
async fn without_a_signing_secret_the_manager_urls_are_404() {
    let app = App::open(config(None)).await.unwrap();
    let running = Running::start(app.clone(), Routers::new(&app).unwrap()).await;
    for kind in ["events", "commands", "interactivity"] {
        let path = format!("/slack/b/manager/{kind}");
        let response = running
            .post(&path, Vec::new(), fixtures::URL_VERIFICATION)
            .await;
        assert_eq!(response.status, 404, "{kind}");
    }
    running.stop().await;
}

#[tokio::test]
async fn the_manager_answers_the_challenge_and_verifies_everything_else() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let running = Running::start(app.clone(), Routers::new(&app).unwrap()).await;
    let challenge = running
        .post(
            "/slack/b/manager/events",
            Vec::new(),
            fixtures::URL_VERIFICATION,
        )
        .await;
    assert_eq!(
        (challenge.status, challenge.body.as_str()),
        (200, fixtures::CHALLENGE)
    );

    let command = slash_command(&slack, "me", "challenge");
    let unsigned = running
        .post("/slack/b/manager/commands", Vec::new(), &command)
        .await;
    assert_eq!(unsigned.status, 401);
    let signed = running.signed("/slack/b/manager/commands", &command).await;
    assert_eq!((signed.status, signed.body.as_str()), (200, ""));

    let agent = format!("/slack/b/{}/events", core_types::BindingId::new_v4());
    let unknown = running
        .post(&agent, Vec::new(), fixtures::URL_VERIFICATION)
        .await;
    assert_eq!(unknown.status, 404);
    running.stop().await;
}

#[tokio::test]
async fn retries_are_dropped_through_the_store() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let (tx, mut out) = mpsc::unbounded_channel();
    let running = Running::start(app.clone(), routers_into(&app, tx)).await;

    let events = "/slack/b/manager/events";
    assert_eq!(
        running.signed(events, fixtures::USER_CHANGE).await.status,
        200
    );
    match next(&mut out).await {
        SlackInbound::Event(event) => {
            assert_eq!(event.binding, BindingRef::MANAGER_ID);
            assert_eq!(event.event_type, "user_change");
        }
        other => panic!("expected an event, got {other:?}"),
    }
    let mut retry =
        fixtures::signed_headers(SECRET, fixtures::now(), fixtures::USER_CHANGE.as_bytes())
            .to_vec();
    retry.push(("x-slack-retry-num", "1".to_owned()));
    assert_eq!(
        running
            .post(events, retry, fixtures::USER_CHANGE)
            .await
            .status,
        200
    );
    assert_eq!(
        running
            .signed(
                "/slack/b/manager/commands",
                &slash_command(&slack, "me", "unused")
            )
            .await
            .status,
        200
    );
    assert!(
        matches!(next(&mut out).await, SlackInbound::Command(_)),
        "the retry was delivered"
    );

    assert!(
        !app.store()
            .mark_event_processed("slack:manager", "Ev0USERCHG1", OffsetDateTime::now_utc())
            .await
            .unwrap(),
        "the event id is not in processed_events"
    );
    running.stop().await;
}

#[tokio::test]
async fn only_kept_messages_reach_processed_events_and_only_by_channel_and_ts() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let (tx, mut out) = mpsc::unbounded_channel();
    let running = Running::start(app.clone(), routers_into(&app, tx)).await;

    let events = "/slack/b/manager/events";
    assert_eq!(
        running.signed(events, fixtures::MESSAGE_PLAIN).await.status,
        200
    );
    assert_eq!(
        running.signed(events, fixtures::MESSAGE_IM).await.status,
        200
    );
    let mut retry =
        fixtures::signed_headers(SECRET, fixtures::now(), fixtures::MESSAGE_IM.as_bytes()).to_vec();
    retry.push(("x-slack-retry-num", "1".to_owned()));
    assert_eq!(
        running
            .post(events, retry, fixtures::MESSAGE_IM)
            .await
            .status,
        200
    );
    assert_eq!(
        running
            .signed(
                "/slack/b/manager/commands",
                &slash_command(&slack, "me", "unused")
            )
            .await
            .status,
        200
    );
    match next(&mut out).await {
        SlackInbound::Message(message) => assert_eq!(message.event_id, "Ev0IM000001"),
        other => panic!("expected the DM, got {other:?}"),
    }
    assert!(
        matches!(next(&mut out).await, SlackInbound::Command(_)),
        "the unaddressed message or the retried DM was delivered"
    );

    let now = OffsetDateTime::now_utc();
    let store = app.store();
    for (source, key) in [
        ("slack:manager", "Ev0PLAIN001"),
        ("slack:manager", "Ev0IM000001"),
        ("slack:manager:message", "C0CHAN001:1727697610.000200"),
    ] {
        assert!(
            store.mark_event_processed(source, key, now).await.unwrap(),
            "{source} {key} is in processed_events"
        );
    }
    assert!(
        !store
            .mark_event_processed("slack:manager:message", "D0DM00001:1727697900.000500", now)
            .await
            .unwrap(),
        "the DM is not in processed_events"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_stalled_body_does_not_hold_up_shutdown() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let arrived = Arc::new(Notify::new());
    let mut routers = Routers::new(&app).unwrap();
    routers.public = routers.public.layer(MapRequestLayer::new({
        let arrived = Arc::clone(&arrived);
        move |request: axum::extract::Request| {
            arrived.notify_one();
            request
        }
    }));
    let running = Running::start(app.clone(), routers).await;
    let addr = running.public;
    let client = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(
                b"POST /slack/b/manager/events HTTP/1.1\r\nHost: agentd\r\n\
                  Content-Length: 1000\r\n\r\n{\"type\":",
            )
            .unwrap();
        common::read_response(&mut stream)
    });
    tokio::time::timeout(Duration::from_secs(30), arrived.notified())
        .await
        .expect("the request never reached agentd's routes");

    let begun = Instant::now();
    running.stop().await;
    let took = begun.elapsed();
    assert!(took < Duration::from_secs(4), "shutdown took {took:?}");
    let response = client.await.unwrap().expect("no response");
    assert_eq!(response.status, 408);
}

#[tokio::test]
async fn a_signed_slash_command_is_answered_through_its_response_url() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let running = Running::start(app.clone(), Routers::new(&app).unwrap()).await;
    let command = slash_command(&slack, "me", "me-reply");
    assert_eq!(
        running
            .signed("/slack/b/manager/commands", &command)
            .await
            .status,
        200
    );
    let reply = wait_for_request(&slack, "/hooks/me-reply").await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["response_type"], "ephemeral");
    let text = body["text"].as_str().unwrap();
    assert!(text.starts_with("Claude account: not linked."), "{text}");
    assert!(text.contains("`agent-core` (`A0MANAGER`)"), "{text}");
    assert!(reply.headers.get("authorization").is_none());
    running.stop().await;
}

#[tokio::test]
async fn a_dm_to_the_manager_app_is_answered_in_the_dm() {
    let slack = fake_slack().await;
    let app = App::open(config(Some(&slack))).await.unwrap();
    let running = Running::start(app.clone(), Routers::new(&app).unwrap()).await;
    let dm = fixtures::MESSAGE_IM.replace("what did we decide yesterday?", "me");
    assert_eq!(
        running.signed("/slack/b/manager/events", &dm).await.status,
        200
    );
    let posted = wait_for_request(&slack, "/api/chat.postMessage").await;
    let body: Value = serde_json::from_slice(&posted.body).unwrap();
    assert_eq!(body["channel"], "D0DM00001");
    let text = body["text"].as_str().unwrap();
    assert!(
        text.starts_with("Claude account: not linked. Send `login`"),
        "{text}"
    );
    running.stop().await;
}

#[tokio::test]
async fn agentd_does_not_start_when_slack_refuses_the_manager_token() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "invalid_auth"})),
        )
        .mount(&slack)
        .await;
    let err = App::open(config(Some(&slack))).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("AGENTD_SLACK_MANAGER_BOT_TOKEN"), "{text}");
    assert!(!text.contains(BOT_TOKEN), "{text}");
}
