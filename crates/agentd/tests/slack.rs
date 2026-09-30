//! The Slack request URLs on agentd's public listener, with the store for
//! deduplication.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use agentd::server::{Routers, Server, Worker, public_router};
use agentd::{App, Config, slack};
use core_types::{SendError, Sender, Sink};
use surface_slack::{BindingRef, SlackInbound};
use testkit::slack as fixtures;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use common::{CONFIG, Response, env};

const SECRET: &str = "manager-signing-secret";

struct Collect(mpsc::UnboundedSender<SlackInbound>);

#[async_trait::async_trait]
impl Sink<SlackInbound> for Collect {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        self.0.send(item).map_err(|_| SendError)
    }
}

fn config(manager_secret: Option<&str>) -> Config {
    let mut env = env();
    if let Some(secret) = manager_secret {
        env.push((
            "AGENTD_SLACK_MANAGER_SIGNING_SECRET".to_owned(),
            secret.to_owned(),
        ));
    }
    Config::parse(CONFIG, env).unwrap()
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
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
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
    let mut routers = Routers::new(app);
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
    let running = Running::start(app.clone(), Routers::new(&app)).await;
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
    let app = App::open(config(Some(SECRET))).await.unwrap();
    let running = Running::start(app.clone(), Routers::new(&app)).await;
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

    let unsigned = running
        .post(
            "/slack/b/manager/commands",
            Vec::new(),
            fixtures::SLASH_COMMAND,
        )
        .await;
    assert_eq!(unsigned.status, 401);
    let signed = running
        .signed("/slack/b/manager/commands", fixtures::SLASH_COMMAND)
        .await;
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
    let app = App::open(config(Some(SECRET))).await.unwrap();
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
            .signed("/slack/b/manager/commands", fixtures::SLASH_COMMAND)
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
            .mark_event_processed("slack:manager", "Ev0USERCHG1")
            .await
            .unwrap(),
        "the event id is not in processed_events"
    );
    running.stop().await;
}
