//! Agents' Slack apps against a wiremock Slack: `/agent create` creating an
//! app from a manifest (answering Slack's challenge while the binding is
//! still `creating`), the install through the OAuth callback, the install
//! reminder, `/agent delete` with and without a configuration token, and a
//! channel message to an agent's app answered by a turn, posted with the
//! agent's bot token.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::pipeline::{Pipeline, TurnSettings, Turns};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use core_types::{BindingId, MemberId, MemberKey, SurfaceKind, TeamId, UserId};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{
    AgentCreation, BindingState, NewAgent, NewClaudeLink, NewSlackApp, NewSlackConfigToken, Store,
    Visibility,
};
use testkit::slack as fixtures;
use testkit::{Turn, agentctl_path, fake_anthropic, fake_claude_path};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use common::{Response, TempDir, env};

const PUBLIC_URL: &str = "https://agentd.example.com";
const MANAGER_SECRET: &str = "manager-signing-SECRET";
const MANAGER_TOKEN: &str = "xoxb-manager-SECRET";
const CONFIG_TOKEN: &str = "xoxe.xoxp-1-config-SECRET";
const APP_ID: &str = "A0HELPER1";
const CLIENT_ID: &str = "1111.2222";
const CLIENT_SECRET: &str = "client-SECRET-helper";
const SIGNING_SECRET: &str = "signing-SECRET-helper";
const AGENT_TOKEN: &str = "xoxb-helper-SECRET";
const AGENT_BOT: &str = "U0HELPER1";
const CODE: &str = "oauth-code-SECRET";

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

fn ada() -> MemberKey {
    MemberKey {
        surface: SurfaceKind::Slack,
        team: TeamId::new(fixtures::TEAM),
        user: UserId::new(fixtures::USER),
    }
}

async fn mount(slack: &MockServer, name: &str, token: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{name}")))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ok(body))
        .mount(slack)
        .await;
}

/// A Slack Web API that knows the manager app's and the agent's bot
/// tokens, and the member's configuration token.
async fn fake_slack() -> MockServer {
    let slack = MockServer::start().await;
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
        (
            "users.info",
            json!({"user": {"id": fixtures::USER, "name": "ada", "profile": {"display_name": "Ada"}}}),
        ),
    ] {
        mount(&slack, name, MANAGER_TOKEN, body).await;
    }
    for (name, body) in [
        ("chat.postMessage", json!({"ts": "1727700001.000200"})),
        ("reactions.add", json!({})),
        ("reactions.remove", json!({})),
        ("conversations.replies", json!({"messages": []})),
        ("conversations.history", json!({"messages": []})),
        ("users.list", json!({"members": []})),
    ] {
        mount(&slack, name, AGENT_TOKEN, body).await;
    }
    mount(&slack, "apps.manifest.delete", CONFIG_TOKEN, json!({})).await;
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(header("authorization", basic.as_str()))
        .respond_with(ok(json!({
            "app_id": APP_ID,
            "authed_user": {"id": fixtures::USER},
            "token_type": "bot",
            "access_token": AGENT_TOKEN,
            "bot_user_id": AGENT_BOT,
            "team": {"id": fixtures::TEAM, "name": "Example"},
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/hooks/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&slack)
        .await;
    slack
}

/// `apps.manifest.create`: before answering, sends Slack's
/// `url_verification` challenge to the events URL the manifest names, as
/// Slack does while the call runs, and keeps agentd's answer.
struct CreateApp {
    public: SocketAddr,
    manifests: Arc<Mutex<Vec<Value>>>,
    challenges: Arc<Mutex<Vec<Response>>>,
}

impl Respond for CreateApp {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form: HashMap<String, String> = serde_urlencoded::from_bytes(&request.body).unwrap();
        let manifest: Value = serde_json::from_str(&form["manifest"]).unwrap();
        let url = manifest["settings"]["event_subscriptions"]["request_url"]
            .as_str()
            .unwrap()
            .to_owned();
        let path = url.strip_prefix(PUBLIC_URL).unwrap();
        let answer = common::post(
            self.public,
            path,
            &[("content-type", "application/json".to_owned())],
            fixtures::URL_VERIFICATION,
        )
        .expect("agentd answers the challenge");
        self.challenges.lock().unwrap().push(answer);
        self.manifests.lock().unwrap().push(manifest);
        ok(json!({
            "app_id": APP_ID,
            "team_id": fixtures::TEAM,
            "credentials": {
                "client_id": CLIENT_ID,
                "client_secret": CLIENT_SECRET,
                "verification_token": "legacy",
                "signing_secret": SIGNING_SECRET,
            },
        }))
    }
}

fn config_text(slack: &MockServer, extra: &str) -> String {
    format!(
        "{}\n[slack]\napi_url = \"{}/api/\"\npublic_url = \"{PUBLIC_URL}\"\n{extra}",
        common::CONFIG,
        slack.uri()
    )
}

fn slack_env() -> Vec<(String, String)> {
    let mut env = env();
    env.push((
        "AGENTD_SLACK_MANAGER_SIGNING_SECRET".to_owned(),
        MANAGER_SECRET.to_owned(),
    ));
    env.push((
        "AGENTD_SLACK_MANAGER_BOT_TOKEN".to_owned(),
        MANAGER_TOKEN.to_owned(),
    ));
    env
}

async fn link(store: &Store, key: &MemberKey) -> MemberId {
    let now = OffsetDateTime::now_utc();
    let member = store
        .ensure_member(key, key.user.as_str(), now)
        .await
        .unwrap();
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from("claude-access"),
                refresh_token: SecretString::from("claude-refresh"),
                expires_at: now + Duration::from_secs(24 * 60 * 60),
                plan: None,
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    member
}

async fn register_config_token(store: &Store, member: MemberId) {
    let now = OffsetDateTime::now_utc();
    store
        .put_slack_config_token(
            member,
            &TeamId::new(fixtures::TEAM),
            &NewSlackConfigToken {
                token: SecretString::from(CONFIG_TOKEN),
                refresh_token: SecretString::from("xoxe-1-refresh-SECRET"),
                expires_at: now + Duration::from_secs(12 * 60 * 60),
            },
            now,
        )
        .await
        .unwrap();
}

struct Harness {
    slack: MockServer,
    app: App,
    public: SocketAddr,
    member: MemberId,
    manifests: Arc<Mutex<Vec<Value>>>,
    challenges: Arc<Mutex<Vec<Response>>>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    hooks: usize,
}

impl Harness {
    /// agentd serving Slack, with ada linked and her configuration token
    /// registered, and `apps.manifest.create` answering as `create` says.
    async fn start(extra: &str) -> Self {
        let slack = fake_slack().await;
        let config = Config::parse(&config_text(&slack, extra), slack_env()).unwrap();
        let app = App::open(config).await.unwrap();
        let member = link(app.store(), &ada()).await;
        register_config_token(app.store(), member).await;
        let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
            .await
            .unwrap();
        let public = server.addrs().public;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(
            async {
                let _ = stopped.await;
            },
            std::future::pending(),
        ));
        let manifests = Arc::default();
        let challenges = Arc::default();
        Self {
            slack,
            app,
            public,
            member,
            manifests,
            challenges,
            stop,
            task,
            hooks: 0,
        }
    }

    async fn creates_apps(&self) {
        Mock::given(method("POST"))
            .and(path("/api/apps.manifest.create"))
            .and(header(
                "authorization",
                format!("Bearer {CONFIG_TOKEN}").as_str(),
            ))
            .respond_with(CreateApp {
                public: self.public,
                manifests: Arc::clone(&self.manifests),
                challenges: Arc::clone(&self.challenges),
            })
            .mount(&self.slack)
            .await;
    }

    fn store(&self) -> &Store {
        self.app.store()
    }

    async fn requests(&self, name: &str) -> Vec<Request> {
        let wanted = format!("/api/{name}");
        self.slack
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.url.path() == wanted)
            .collect()
    }

    /// Runs `/agent <text>` as ada and returns the reply.
    async fn slash(&mut self, text: &str) -> String {
        self.hooks += 1;
        let hook = format!("/hooks/{}", self.hooks);
        let url = format!("{}{hook}", self.slack.uri())
            .replace(':', "%3A")
            .replace('/', "%2F");
        let body = fixtures::SLASH_COMMAND
            .replace(
                "https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT0TEAM001%2F7800000000001%2FfakeResponseUrlToken",
                &url,
            )
            .replace(
                "text=create+helper+You+are+terse.",
                &format!("text={}", text.replace(' ', "+")),
            );
        let response = self
            .post("/slack/b/manager/commands", MANAGER_SECRET, &body)
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let replies: Vec<String> = self
                .slack
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|request| request.url.path() == hook)
                .map(|request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    body["text"].as_str().unwrap().to_owned()
                })
                .collect();
            if let Some(reply) = replies.into_iter().next() {
                return reply;
            }
            assert!(Instant::now() < deadline, "no reply to /agent {text}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn post(&self, path: &str, secret: &str, body: &str) -> Response {
        let headers = fixtures::signed_headers(secret, fixtures::now(), body.as_bytes()).to_vec();
        let (addr, path, body) = (self.public, path.to_owned(), body.to_owned());
        tokio::task::spawn_blocking(move || common::post(addr, &path, &headers, &body))
            .await
            .unwrap()
            .expect("no response")
    }

    async fn get(&self, path: &str) -> Response {
        let (addr, path) = (self.public, path.to_owned());
        tokio::task::spawn_blocking(move || common::get(addr, &path))
            .await
            .unwrap()
            .expect("no response")
    }

    /// The texts the manager app posted.
    async fn manager_posts(&self) -> Vec<String> {
        self.requests("chat.postMessage")
            .await
            .into_iter()
            .filter(|request| {
                request.headers.get("authorization").unwrap()
                    == format!("Bearer {MANAGER_TOKEN}").as_str()
            })
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["text"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    /// The only agent binding ada has.
    async fn binding(&self) -> BindingId {
        let agent = self
            .store()
            .agent_by_name(self.member, "helper")
            .await
            .unwrap()
            .expect("the agent exists");
        let bindings = self.store().bindings_of(agent.id).await.unwrap();
        assert_eq!(bindings.len(), 1);
        bindings[0].id
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

/// The install link's `state` in `text`.
fn state_in(text: &str) -> String {
    let start = text.find("state=").expect("an install link") + "state=".len();
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || "-_.%".contains(*c))
        .collect()
}

/// Creates `helper` and returns the create reply and the install link's
/// `state` from the DM.
async fn create(harness: &mut Harness) -> (String, String) {
    harness.creates_apps().await;
    let reply = harness.slash("create helper").await;
    let dm = harness
        .manager_posts()
        .await
        .into_iter()
        .find(|text| text.contains("Install `helper`"))
        .expect("an install DM");
    assert!(
        dm.contains("https://slack.com/oauth/v2/authorize?client_id=1111.2222"),
        "{dm}"
    );
    (reply, state_in(&dm))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_install_and_callback_make_an_active_agent_app() {
    let mut harness = Harness::start("").await;
    let (reply, state) = create(&mut harness).await;
    assert!(
        reply.starts_with("Created `helper` as a Slack app."),
        "{reply}"
    );
    assert!(reply.contains("/invite @helper"), "{reply}");

    let challenges: Vec<(u16, String)> = harness
        .challenges
        .lock()
        .unwrap()
        .iter()
        .map(|answer| (answer.status, answer.body.clone()))
        .collect();
    assert_eq!(
        challenges,
        [(200, fixtures::CHALLENGE.to_owned())],
        "answered while creating"
    );
    let binding = harness.binding().await;
    let manifest = harness.manifests.lock().unwrap()[0].clone();
    assert_eq!(manifest["display_information"]["name"], "helper");
    assert_eq!(
        manifest["settings"]["event_subscriptions"]["request_url"],
        format!("{PUBLIC_URL}/slack/b/{binding}/events")
    );
    assert_eq!(
        manifest["oauth_config"]["redirect_urls"][0],
        format!("{PUBLIC_URL}/slack/oauth/callback")
    );
    let created = &harness.requests("apps.manifest.create").await[0];
    assert!(!String::from_utf8_lossy(&created.body).contains(CONFIG_TOKEN));
    let app = harness.store().slack_app(binding).await.unwrap().unwrap();
    assert_eq!(app.state, BindingState::PendingInstall);
    assert_eq!(app.app_id.as_deref(), Some(APP_ID));

    let before_install = fixtures::MESSAGE_MENTION.replace("U0BOT0001", AGENT_BOT);
    let events = format!("/slack/b/{binding}/events");
    assert_eq!(
        harness
            .post(&events, SIGNING_SECRET, &before_install)
            .await
            .status,
        200,
        "verified with the app's own signing secret"
    );
    assert_eq!(
        harness
            .post(&events, MANAGER_SECRET, &before_install)
            .await
            .status,
        401,
        "another app's secret doesn't verify"
    );

    let callback = format!("/slack/oauth/callback?code={CODE}&state={state}");
    let installed = harness.get(&callback).await;
    assert_eq!(installed.status, 200, "{installed:?}");
    assert!(installed.body.starts_with("Installed."), "{installed:?}");
    let exchange = &harness.requests("oauth.v2.access").await[0];
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&exchange.body).unwrap();
    assert_eq!(form["code"], CODE);
    assert_eq!(
        form["redirect_uri"],
        format!("{PUBLIC_URL}/slack/oauth/callback")
    );
    let app = harness.store().slack_app(binding).await.unwrap().unwrap();
    assert_eq!(app.state, BindingState::Active);
    assert_eq!(app.bot_user, Some(UserId::new(AGENT_BOT)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !harness
        .manager_posts()
        .await
        .iter()
        .any(|text| text.contains("`helper` is installed"))
    {
        assert!(Instant::now() < deadline, "the owner wasn't told");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let replayed = harness.get(&callback).await;
    assert_eq!(
        replayed.status, 409,
        "a replayed state is refused: {replayed:?}"
    );
    let (key, sealed) = state.split_once('.').unwrap();
    for forged in [
        format!("{}.{sealed}", BindingId::new_v4()),
        format!("{key}.{}", sealed.replace('A', "B")),
        String::new(),
    ] {
        let refused = harness
            .get(&format!("/slack/oauth/callback?code={CODE}&state={forged}"))
            .await;
        assert_eq!(refused.status, 400, "{forged}: {refused:?}");
    }
    assert_eq!(harness.requests("oauth.v2.access").await.len(), 1);
    assert_eq!(
        harness.get("/slack/oauth/callback").await.status,
        400,
        "no state at all"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_install_for_another_workspace_or_a_cancelled_one_is_not_stored() {
    let mut harness = Harness::start("").await;
    let (_, state) = create(&mut harness).await;
    let binding = harness.binding().await;

    let cancelled = harness
        .get(&format!(
            "/slack/oauth/callback?error=access_denied&state={state}"
        ))
        .await;
    assert_eq!(cancelled.status, 200);
    assert!(cancelled.body.contains("cancelled"), "{cancelled:?}");
    assert!(harness.requests("oauth.v2.access").await.is_empty());

    harness.slack.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ok(json!({
            "app_id": APP_ID,
            "token_type": "bot",
            "access_token": AGENT_TOKEN,
            "bot_user_id": AGENT_BOT,
            "team": {"id": "T0OTHER01"},
        })))
        .mount(&harness.slack)
        .await;
    let elsewhere = harness
        .get(&format!("/slack/oauth/callback?code={CODE}&state={state}"))
        .await;
    assert_eq!(elsewhere.status, 400, "{elsewhere:?}");
    let app = harness.store().slack_app(binding).await.unwrap().unwrap();
    assert_eq!(app.state, BindingState::PendingInstall);
    assert!(harness.store().bot_token(binding).await.unwrap().is_none());
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_creation_frees_the_name_and_says_why() {
    let mut harness = Harness::start("").await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.create"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "invalid_manifest"})),
        )
        .mount(&harness.slack)
        .await;
    let reply = harness.slash("create helper").await;
    assert!(reply.contains("`invalid_manifest`"), "{reply}");
    assert!(
        harness
            .store()
            .agent_by_name(harness.member, "helper")
            .await
            .unwrap()
            .is_none(),
        "the name is free again"
    );

    harness
        .store()
        .delete_slack_config_tokens(harness.member)
        .await
        .unwrap();
    let reply = harness.slash("create helper").await;
    assert!(reply.contains("/agent slack-token"), "{reply}");
    assert_eq!(harness.requests("apps.manifest.create").await.len(), 1);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_is_reminded_once_when_the_app_waits_for_its_install() {
    let mut harness = Harness::start("install_reminder_secs = 60\n").await;
    let (_, first_state) = create(&mut harness).await;
    let agents = harness.app.slack_agents().unwrap().clone();
    let now = OffsetDateTime::now_utc();

    assert_eq!(agents.pass_at(|| now).await.unwrap().reminded, 0);
    let later = now + Duration::from_secs(120);
    assert_eq!(agents.pass_at(|| later).await.unwrap().reminded, 1);
    assert_eq!(agents.pass_at(|| later).await.unwrap().reminded, 0, "once");
    let reminder = harness
        .manager_posts()
        .await
        .into_iter()
        .find(|text| text.starts_with("`helper` still isn't installed"))
        .expect("a reminder");
    let state = state_in(&reminder);
    assert_ne!(state, first_state);
    let installed = harness
        .get(&format!("/slack/oauth/callback?code={CODE}&state={state}"))
        .await;
    assert_eq!(installed.status, 200, "the reminder's link installs");
    harness.stop().await;
}

/// Creates and installs `helper`, and returns its binding.
async fn installed(harness: &mut Harness) -> BindingId {
    let (_, state) = create(harness).await;
    let installed = harness
        .get(&format!("/slack/oauth/callback?code={CODE}&state={state}"))
        .await;
    assert_eq!(installed.status, 200);
    harness.binding().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_with_a_configuration_token_deletes_the_app() {
    let mut harness = Harness::start("").await;
    let binding = installed(&mut harness).await;
    let reply = harness.slash("delete helper").await;
    assert_eq!(reply, "Deleted `helper` and its Slack app.");
    let deleted = &harness.requests("apps.manifest.delete").await[0];
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&deleted.body).unwrap();
    assert_eq!(form["app_id"], APP_ID);
    let row = harness.store().binding(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Disabled);
    assert!(row.retired_at.is_some());
    let event = fixtures::MESSAGE_MENTION.replace("U0BOT0001", AGENT_BOT);
    let response = harness
        .post(
            &format!("/slack/b/{binding}/events"),
            SIGNING_SECRET,
            &event,
        )
        .await;
    assert_eq!(response.status, 404, "its events aren't handled any more");
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_without_a_configuration_token_disables_the_binding_and_says_what_to_do() {
    let mut harness = Harness::start("").await;
    let binding = installed(&mut harness).await;
    harness
        .store()
        .delete_slack_config_tokens(harness.member)
        .await
        .unwrap();
    let reply = harness.slash("delete helper").await;
    assert!(
        reply.starts_with("Deleted `helper`: it no longer answers"),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("https://api.slack.com/apps/{APP_ID}")),
        "{reply}"
    );
    assert!(harness.requests("apps.manifest.delete").await.is_empty());
    let row = harness.store().binding(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Disabled);
    assert!(row.retired_at.is_none());
    assert!(harness.store().bot_token(binding).await.unwrap().is_none());
    let event = fixtures::MESSAGE_MENTION.replace("U0BOT0001", AGENT_BOT);
    let response = harness
        .post(
            &format!("/slack/b/{binding}/events"),
            SIGNING_SECRET,
            &event,
        )
        .await;
    assert_eq!(response.status, 404);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_mention_of_an_agent_is_answered_with_its_bot_token() {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let slack = fake_slack().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n",
        config_text(&slack, "")
            .replace("/nonexistent/agentd", &dir.path().display().to_string())
            .replace(
                "sqlite::memory:",
                &format!("sqlite://{}", dir.path().join("agentd.db").display())
            ),
        fake.uri()
    );
    let config = Config::parse(&text, slack_env()).unwrap();
    let app = App::open(config).await.unwrap();
    let store = app.store().clone();
    let ada = link(&store, &ada()).await;
    let team = TeamId::new(fixtures::TEAM);
    let AgentCreation::Created(_, binding) = store
        .create_agent(
            &NewAgent {
                owner: ada,
                name: "helper",
                persona: "You are helper.",
                visibility: Visibility::Public,
                surface: SurfaceKind::Slack,
                team: &team,
            },
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    else {
        panic!("created");
    };
    assert!(
        store
            .set_slack_app(
                binding,
                &NewSlackApp {
                    app_id: APP_ID.to_owned(),
                    client_id: CLIENT_ID.to_owned(),
                    client_secret: SecretString::from(CLIENT_SECRET),
                    signing_secret: SecretString::from(SIGNING_SECRET),
                    scopes: "chat:write".to_owned(),
                },
                "helper",
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .install_slack_app(
                binding,
                APP_ID,
                &UserId::new(AGENT_BOT),
                &SecretString::from(AGENT_TOKEN),
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
    );

    let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
        .await
        .unwrap();
    let addrs = server.addrs();
    let script = dir.path().join("script.json");
    testkit::write_script(&script, &vec![Turn::reply("Hello from helper."); 4]).unwrap();
    let path = format!("{}:/usr/bin:/bin", agentctl.parent().unwrap().display());
    let mut vars = BTreeMap::from([
        (
            testkit::claude::SCRIPT_ENV.to_owned(),
            script.display().to_string(),
        ),
        ("PATH".to_owned(), path),
    ]);
    for name in ["NO_PROXY", "no_proxy"] {
        vars.insert(name.to_owned(), addrs.proxy.ip().to_string());
    }
    let settings = TurnSettings {
        process: ProcessConfig {
            claude_bin: claude.display().to_string(),
            anthropic_base_url: format!("http://{}", addrs.proxy),
            turn_timeout_secs: 60,
        },
        pool: PoolConfig {
            global_container_cap: 1,
            ..PoolConfig::default()
        },
        image: "unused".to_owned(),
        data_dir: dir.path().to_owned(),
        agentctl_url: format!("http://{}", addrs.ctl),
        env: vars,
    };
    let sandbox = ProcessSandbox::new(store.clone(), dir.path()).unwrap();
    let turns = Turns::start(&app, Arc::new(sandbox), settings).unwrap();
    let server = server.with_pipeline(Pipeline::for_app(&app, turns));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));

    let events = format!("/slack/b/{binding}/events");
    let post = |body: String| {
        let headers =
            fixtures::signed_headers(SIGNING_SECRET, fixtures::now(), body.as_bytes()).to_vec();
        let (addr, events) = (addrs.public, events.clone());
        tokio::task::spawn_blocking(move || common::post(addr, &events, &headers, &body).unwrap())
    };
    let elsewhere = fixtures::MESSAGE_MENTION
        .replace("U0BOT0001", AGENT_BOT)
        .replace("T0TEAM001", "T0OTHER01")
        .replace("Ev0MENTION1", "Ev0ELSEWHERE")
        .replace("1727697600.000100", "1727697500.000100");
    assert_eq!(post(elsewhere).await.unwrap().status, 200);
    let mention = fixtures::MESSAGE_MENTION.replace("U0BOT0001", AGENT_BOT);
    assert_eq!(post(mention).await.unwrap().status, 200);

    let agent_posts = || async {
        slack
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| {
                request.url.path() == "/api/chat.postMessage"
                    && request.headers.get("authorization").unwrap()
                        == format!("Bearer {AGENT_TOKEN}").as_str()
            })
            .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
            .collect::<Vec<_>>()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let posts = loop {
        let posts = agent_posts().await;
        if !posts.is_empty() {
            break posts;
        }
        assert!(Instant::now() < deadline, "the agent never replied");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["text"], "Hello from helper.");
    assert_eq!(posts[0]["channel"], fixtures::CHANNEL);
    assert_eq!(posts[0]["thread_ts"], "1727697600.000100");
    let reply = core_types::MsgRef {
        conv: core_types::ConvRef {
            surface: SurfaceKind::Slack,
            team: team.clone(),
            conversation: fixtures::CHANNEL.into(),
        },
        id: "1727700001.000200".into(),
    };
    let posted = store.posted_message_ref(&reply).await.unwrap().unwrap();
    assert_eq!(posted.requester.member, Some(ada));
    assert!(posted.agent.is_some());
    assert_eq!(
        fake.message_requests().await.len(),
        1,
        "one turn: the message from another workspace was dropped"
    );

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
