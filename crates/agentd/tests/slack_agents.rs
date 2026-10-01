//! Agents' Slack apps against a wiremock Slack: `/agent create` creating an
//! app from a manifest (answering Slack's challenge while the binding is
//! still `creating`), the install through the OAuth callback, the install
//! reminder, `/agent delete` with and without a configuration token, a
//! channel message to an agent's app answered by a turn, posted with the
//! agent's bot token, and an owner's forged events billing, resuming and
//! prompting no one, the owner included.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::pipeline::{Pipeline, TurnSettings, Turns, UNCONFIRMED_TEXT};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use core_types::{
    BindingId, Hop, MemberId, MemberKey, Requester, SessionId, SurfaceKind, TeamId, UserId,
};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{
    AgentCreation, BindingState, NewAgent, NewClaudeLink, NewMessageRef, NewSlackApp,
    NewSlackConfigToken, Store, Visibility,
};
use testkit::slack as fixtures;
use testkit::{Turn, agentctl_path, fake_anthropic, fake_claude_path};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
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

/// `users.info`'s answer that whoever was asked about is a member of the
/// workspace named Ada.
fn home_member(request: &Request) -> ResponseTemplate {
    let form: HashMap<String, String> =
        serde_urlencoded::from_bytes(&request.body).unwrap_or_default();
    let user = form.get("user").cloned().unwrap_or_default();
    ok(
        json!({"user": {"id": user, "team_id": fixtures::TEAM, "name": "ada", "profile": {"display_name": "Ada"}}}),
    )
}

/// `conversations.info`'s answer for the public channel the fixtures are in.
fn public_channel() -> Value {
    json!({"channel": {"id": fixtures::CHANNEL, "is_channel": true, "is_member": true}})
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
    ] {
        mount(&slack, name, MANAGER_TOKEN, body).await;
    }
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(header(
            "authorization",
            format!("Bearer {MANAGER_TOKEN}").as_str(),
        ))
        .respond_with(home_member)
        .mount(&slack)
        .await;
    for (name, body) in [
        ("chat.postMessage", json!({"ts": "1727700001.000200"})),
        ("reactions.add", json!({})),
        ("reactions.remove", json!({})),
        ("conversations.replies", json!({"messages": []})),
        ("conversations.history", json!({"messages": []})),
        ("conversations.info", public_channel()),
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

/// The install link's `state` in `text`: the first query parameter of the
/// `<url|label>` link the manager app posted.
fn state_in(text: &str) -> String {
    let start = text.find("?state=").expect("an install link") + "?state=".len();
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
        .find(|text| text.contains("|Install helper>"))
        .expect("an install DM");
    assert!(
        dm.starts_with("<https://slack.com/oauth/v2/authorize?state="),
        "{dm}"
    );
    assert!(dm.contains("&amp;client_id=1111.2222&amp;"), "{dm}");
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
    assert!(reply.contains("I'll tell you how to invite it"), "{reply}");

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
    let row = harness.store().binding(binding).await.unwrap().unwrap();
    assert_eq!(row.bot_user, Some(UserId::new(AGENT_BOT)));
    let deadline = Instant::now() + Duration::from_secs(10);
    let told = loop {
        if let Some(told) = harness
            .manager_posts()
            .await
            .into_iter()
            .find(|text| text.contains("`helper` is installed"))
        {
            break told;
        }
        assert!(Instant::now() < deadline, "the owner wasn't told");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        told.contains(&format!("installed as <@{AGENT_BOT}>")),
        "the bot is mentioned, since several agents may share a name: {told}"
    );

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
    assert!(reply.contains("`[slack] public_url`"), "{reply}");
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
async fn an_agent_is_never_named_after_a_broadcast() {
    let mut harness = Harness::start("").await;
    harness.creates_apps().await;
    for name in ["here", "channel", "everyone"] {
        let reply = harness.slash(&format!("create {name}")).await;
        assert!(reply.contains("message to everyone"), "{name}: {reply}");
    }
    assert!(harness.requests("apps.manifest.create").await.is_empty());
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

/// An agent's installed app, for [`Turned`].
struct AgentSpec {
    name: &'static str,
    app_id: &'static str,
    bot: &'static str,
    token: &'static str,
    secret: &'static str,
    posted_ts: &'static str,
}

const HELPER: AgentSpec = AgentSpec {
    name: "helper",
    app_id: APP_ID,
    bot: AGENT_BOT,
    token: AGENT_TOKEN,
    secret: SIGNING_SECRET,
    posted_ts: "1727700001.000200",
};

const SCOUT: AgentSpec = AgentSpec {
    name: "scout",
    app_id: "A0SCOUT01",
    bot: "U0SCOUT01",
    token: "xoxb-scout-SECRET",
    secret: "signing-SECRET-scout",
    posted_ts: "1727700002.000300",
};

/// agentd running turns, with ada's installed agents and bob linked too.
struct Turned {
    slack: MockServer,
    fake: testkit::FakeAnthropic,
    store: Store,
    public: SocketAddr,
    bindings: Vec<BindingId>,
    ada: MemberId,
    bob: MemberId,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    _dir: TempDir,
}

fn bob() -> MemberKey {
    MemberKey {
        user: UserId::new(fixtures::OTHER_USER),
        ..ada()
    }
}

/// A channel message from `user` at `ts` with `text`, as an agent's app
/// receives it.
fn channel_message(user: &str, ts: &str, event_id: &str, text: &str) -> String {
    message_event(user, ts, event_id, text, json!({}))
}

/// [`channel_message`], with `extra`'s fields set in the event.
fn message_event(user: &str, ts: &str, event_id: &str, text: &str, extra: Value) -> String {
    let mut envelope: Value = serde_json::from_str(fixtures::MESSAGE_MENTION).unwrap();
    envelope["event_id"] = json!(event_id);
    let event = &mut envelope["event"];
    event["user"] = json!(user);
    event["ts"] = json!(ts);
    event["event_ts"] = json!(ts);
    event["text"] = json!(text);
    event.as_object_mut().unwrap().remove("blocks");
    for (key, value) in extra.as_object().unwrap() {
        event[key] = value.clone();
    }
    envelope.to_string()
}

/// A `ts` from `seconds_ago` seconds ago, with `micros` after the point.
fn recent_ts(seconds_ago: i64, micros: u32) -> String {
    let seconds = OffsetDateTime::now_utc().unix_timestamp() - seconds_ago;
    format!("{seconds}.{micros:06}")
}

/// A `rich_text` block that mentions `user`, as a client writes one.
fn mention_block(user: &str) -> Value {
    json!([{"type": "rich_text", "elements": [
        {"type": "rich_text_section", "elements": [
            {"type": "user", "user_id": user},
            {"type": "text", "text": " "},
        ]},
    ]}])
}

impl Turned {
    async fn start(agents: &[AgentSpec]) -> Self {
        let claude = fake_claude_path();
        let agentctl = agentctl_path();
        let dir = TempDir::new();
        let fake = fake_anthropic().await;
        let slack = fake_slack().await;
        for agent in agents.iter().skip(1) {
            for (name, body) in [
                ("chat.postMessage", json!({"ts": agent.posted_ts})),
                ("reactions.add", json!({})),
                ("reactions.remove", json!({})),
                ("conversations.replies", json!({"messages": []})),
                ("conversations.history", json!({"messages": []})),
                ("conversations.info", public_channel()),
            ] {
                mount(&slack, name, agent.token, body).await;
            }
        }
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
        let bob = link(&store, &bob()).await;
        let team = TeamId::new(fixtures::TEAM);
        let mut bindings = Vec::new();
        for agent in agents {
            let AgentCreation::Created(_, binding) = store
                .create_agent(
                    &NewAgent {
                        owner: ada,
                        name: agent.name,
                        persona: "You help.",
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
                            app_id: agent.app_id.to_owned(),
                            client_id: CLIENT_ID.to_owned(),
                            client_secret: SecretString::from(CLIENT_SECRET),
                            signing_secret: SecretString::from(agent.secret),
                            scopes: "chat:write".to_owned(),
                            redirect_url: format!("{PUBLIC_URL}/slack/oauth/callback"),
                            manifest_version: surface_slack::manifest::MANIFEST_VERSION,
                        },
                        agent.name,
                        OffsetDateTime::now_utc(),
                    )
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .install_slack_app(
                        binding,
                        agent.app_id,
                        &UserId::new(agent.bot),
                        &SecretString::from(agent.token),
                        OffsetDateTime::now_utc(),
                    )
                    .await
                    .unwrap()
            );
            bindings.push(binding);
        }

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
        Self {
            slack,
            fake,
            store,
            public: addrs.public,
            bindings,
            ada,
            bob,
            stop,
            task,
            _dir: dir,
        }
    }

    /// Posts `body` to the events URL of the agent at `index`, signed with
    /// its secret.
    async fn post(&self, index: usize, secret: &str, body: String) -> u16 {
        let events = format!("/slack/b/{}/events", self.bindings[index]);
        let headers = fixtures::signed_headers(secret, fixtures::now(), body.as_bytes()).to_vec();
        let addr = self.public;
        tokio::task::spawn_blocking(move || common::post(addr, &events, &headers, &body).unwrap())
            .await
            .unwrap()
            .status
    }

    async fn requests(&self, name: &str, token: &str) -> Vec<Request> {
        let wanted = format!("/api/{name}");
        self.slack
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| {
                request.url.path() == wanted
                    && request.headers.get("authorization").unwrap()
                        == format!("Bearer {token}").as_str()
            })
            .collect()
    }

    /// What the agent whose bot token is `token` posted.
    async fn posts(&self, token: &str) -> Vec<Value> {
        self.requests("chat.postMessage", token)
            .await
            .into_iter()
            .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
            .collect()
    }

    /// Waits until the agent whose bot token is `token` posted `count`
    /// messages.
    async fn wait_for_posts(&self, token: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let posts = self.posts(token).await;
            if posts.len() >= count {
                return posts;
            }
            assert!(Instant::now() < deadline, "the agent never replied");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The record of the agent's post `ts`, once the pipeline made it.
    async fn posted(&self, ts: &str) -> store::MessageRef {
        self.posted_in(fixtures::CHANNEL, ts).await
    }

    /// The record of the agent's post `ts` in `channel`, once the pipeline
    /// made it.
    async fn posted_in(&self, channel: &str, ts: &str) -> store::MessageRef {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(posted) = self
                .store
                .posted_message_ref(&msg_in(channel, ts))
                .await
                .unwrap()
            {
                return posted;
            }
            assert!(Instant::now() < deadline, "the post was never recorded");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The lookups that confirmed a message with Slack, with `token`.
    async fn confirmations(&self, token: &str) -> Vec<Request> {
        let mut found = self.requests("conversations.history", token).await;
        found.extend(self.requests("conversations.replies", token).await);
        found.retain(|request| String::from_utf8_lossy(&request.body).contains("oldest="));
        found
    }

    /// Waits until `count` messages were looked up with `token`, and a
    /// moment more for what would follow.
    async fn wait_for_confirmations(&self, token: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.confirmations(token).await.len() < count {
            assert!(Instant::now() < deadline, "the message was never looked up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    async fn wait_for_confirmation(&self, token: &str) {
        self.wait_for_confirmations(token, 1).await;
    }

    /// Makes Slack answer a lookup of the message at `ts`, at the top level
    /// or in a thread, with `response`.
    async fn answer_lookup(&self, ts: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path_regex("^/api/conversations\\.(history|replies)$"))
            .and(body_string_contains(format!("oldest={ts}").as_str()))
            .respond_with(response)
            .with_priority(1)
            .mount(&self.slack)
            .await;
    }

    /// Makes Slack's copy of the message at `ts` be `message`.
    async fn slack_has(&self, ts: &str, message: Value) {
        self.answer_lookup(ts, ok(json!({"messages": [message]})))
            .await;
    }

    /// Makes `conversations.info` answer `channel` for every bot, with
    /// `kind`'s fields.
    async fn conversation_is(&self, channel: &str, kind: Value) {
        let mut info = kind;
        info["id"] = json!(channel);
        Mock::given(method("POST"))
            .and(path("/api/conversations.info"))
            .and(body_string_contains(format!("channel={channel}").as_str()))
            .respond_with(ok(json!({"channel": info})))
            .with_priority(1)
            .mount(&self.slack)
            .await;
    }

    /// Makes the agent whose bot token is `token` post its next messages
    /// at `ts`.
    async fn posts_at(&self, token: &str, ts: &str) {
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ok(json!({"ts": ts})))
            .with_priority(1)
            .mount(&self.slack)
            .await;
    }

    /// Asserts that no turn ran and nothing was recorded as asked for by
    /// bob.
    async fn nothing_billed_to_bob(&self) {
        assert!(self.fake.message_requests().await.is_empty(), "no turn");
        self.no_turn_for_bob().await;
    }

    /// Asserts that nothing the agents posted was asked for by bob.
    async fn no_turn_for_bob(&self) {
        for ts in [HELPER.posted_ts, SCOUT.posted_ts] {
            let posted = self.store.posted_message_ref(&reply_ref(ts)).await.unwrap();
            assert!(
                posted.is_none_or(|posted| posted.requester.member != Some(self.bob)),
                "a reply at {ts} was billed to bob"
            );
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn reply_ref(ts: &str) -> core_types::MsgRef {
    msg_in(fixtures::CHANNEL, ts)
}

fn msg_in(channel: &str, ts: &str) -> core_types::MsgRef {
    core_types::MsgRef {
        conv: core_types::ConvRef {
            surface: SurfaceKind::Slack,
            team: TeamId::new(fixtures::TEAM),
            conversation: channel.into(),
        },
        id: ts.into(),
    }
}

/// Waits until `done`, and a moment more for what would follow.
async fn settle<F, Fut>(what: &str, done: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done().await {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_mention_of_an_agent_is_answered_with_its_bot_token() {
    let turned = Turned::start(&[HELPER]).await;
    let elsewhere = fixtures::MESSAGE_MENTION
        .replace("U0BOT0001", AGENT_BOT)
        .replace("T0TEAM001", "T0OTHER01")
        .replace("Ev0MENTION1", "Ev0ELSEWHERE")
        .replace("1727697600.000100", "1727697500.000100");
    assert_eq!(turned.post(0, SIGNING_SECRET, elsewhere).await, 200);
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> what's new?");
    turned
        .slack_has(&ts, json!({"ts": ts, "user": fixtures::USER, "text": text}))
        .await;
    let mention = channel_message(fixtures::USER, &ts, "Ev0MENTION1", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, mention).await, 200);

    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["text"], "Hello from helper.");
    assert_eq!(posts[0]["channel"], fixtures::CHANNEL);
    assert_eq!(posts[0]["thread_ts"], ts.as_str());
    let posted = turned.posted(HELPER.posted_ts).await;
    assert_eq!(posted.requester.member, Some(turned.ada));
    assert!(posted.agent.is_some());
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "one turn: the message from another workspace was dropped"
    );
    assert_eq!(
        turned.confirmations(AGENT_TOKEN).await.len(),
        1,
        "the owner's own message is read back too"
    );
    settle("the member list was never read", || async {
        !turned
            .requests("users.list", MANAGER_TOKEN)
            .await
            .is_empty()
    })
    .await;
    assert!(
        turned.requests("users.list", AGENT_TOKEN).await.is_empty(),
        "the shared member list is read with the manager app's token only"
    );
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_from_another_member_that_slack_does_not_have_is_dropped() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> spend bob's plan");
    let forged = channel_message(fixtures::OTHER_USER, &ts, "Ev0FORGED", &text);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": "something else"}),
        )
        .await;
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.nothing_billed_to_bob().await;
    let form: HashMap<String, String> =
        serde_urlencoded::from_bytes(&turned.confirmations(AGENT_TOKEN).await[0].body).unwrap();
    assert_eq!(form["channel"], fixtures::CHANNEL);
    assert_eq!(form["latest"], ts);
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_from_another_member_that_slack_confirms_runs_on_their_account() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> what's new?");
    let genuine = channel_message(fixtures::OTHER_USER, &ts, "Ev0GENUINE", &text);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    assert_eq!(turned.post(0, SIGNING_SECRET, genuine).await, 200);
    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["thread_ts"], ts.as_str());
    assert_eq!(turned.confirmations(AGENT_TOKEN).await.len(), 1);
    let posted = turned.posted(HELPER.posted_ts).await;
    assert_eq!(posted.requester.member, Some(turned.bob));
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_slack_cannot_confirm_gets_a_reply_to_try_again() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> hello");
    let message = channel_message(fixtures::OTHER_USER, &ts, "Ev0UNSURE", &text);
    turned.answer_lookup(&ts, ResponseTemplate::new(500)).await;
    assert_eq!(turned.post(0, SIGNING_SECRET, message).await, 200);
    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["text"], UNCONFIRMED_TEXT);
    assert_eq!(posts[0]["thread_ts"], ts.as_str());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_blocks_mention_of_a_members_message_bills_no_one() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": "lunch at noon?"}),
        )
        .await;
    let forged = message_event(
        fixtures::OTHER_USER,
        &ts,
        "Ev0BLOCKS",
        "lunch at noon?",
        json!({"blocks": mention_block(AGENT_BOT)}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_dm_channel_type_bills_no_one() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": "lunch at noon?"}),
        )
        .await;
    let forged = message_event(
        fixtures::OTHER_USER,
        &ts,
        "Ev0DM",
        "lunch at noon?",
        json!({"channel_type": "im"}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_files_on_a_copy_of_a_members_message_bill_no_one() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": "notes attached"}),
        )
        .await;
    let forged = message_event(
        fixtures::OTHER_USER,
        &ts,
        "Ev0FILES",
        "notes attached",
        json!({
            "subtype": "file_share",
            "files": [{
                "id": "F0OWNER01",
                "name": "instructions.txt",
                "url_private": format!("{}/files/F0OWNER01", turned.slack.uri()),
            }],
            "blocks": mention_block(AGENT_BOT),
        }),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.nothing_billed_to_bob().await;
    let fetched = turned.slack.received_requests().await.unwrap_or_default();
    assert!(
        fetched
            .iter()
            .all(|request| !request.url.path().contains("F0OWNER01")),
        "the forged file was never fetched"
    );
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_mention_to_a_second_agent_bills_no_one() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{}> what's new?", HELPER.bot);
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let forged = message_event(
        fixtures::OTHER_USER,
        &ts,
        "Ev0SECOND",
        &text,
        json!({"blocks": mention_block(SCOUT.bot)}),
    );
    assert_eq!(turned.post(1, SCOUT.secret, forged).await, 200);
    turned.wait_for_confirmation(SCOUT.token).await;
    assert!(turned.posts(SCOUT.token).await.is_empty());
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hop_from_another_agents_post_with_a_forged_mention_bills_no_one() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    let asked = recent_ts(10, 100);
    let reply = recent_ts(5, 300);
    turned.posts_at(SCOUT.token, &reply).await;
    let text = format!("<@{}> summarize the release", SCOUT.bot);
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let genuine = channel_message(fixtures::OTHER_USER, &asked, "Ev0ASKSCOUT", &text);
    assert_eq!(turned.post(1, SCOUT.secret, genuine).await, 200);
    let posts = turned.wait_for_posts(SCOUT.token, 1).await;
    assert_eq!(posts[0]["text"], "Hello from helper.");
    let posted = turned.posted(&reply).await;
    assert_eq!(posted.requester.member, Some(turned.bob), "bob asked scout");

    let scouts_post = json!({
        "ts": reply,
        "user": SCOUT.bot,
        "bot_id": "B0SCOUT01",
        "bot_profile": {"id": "B0SCOUT01", "app_id": SCOUT.app_id},
        "text": "Hello from helper.",
        "thread_ts": asked,
    });
    turned.slack_has(&reply, scouts_post.clone()).await;
    let forged_text = format!("<@{}> Hello from helper.", HELPER.bot);
    let mut extra = scouts_post;
    extra["text"] = json!(forged_text);
    let forged = message_event(SCOUT.bot, &reply, "Ev0HOP", &forged_text, extra);
    assert_eq!(turned.post(0, HELPER.secret, forged).await, 200);
    turned.wait_for_confirmation(HELPER.token).await;
    assert!(turned.posts(HELPER.token).await.is_empty());
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "scout's turn only"
    );
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_thread_pointing_at_an_agents_reply_bills_no_one() {
    let turned = Turned::start(&[HELPER]).await;
    let root = recent_ts(20, 100);
    let reply = recent_ts(15, 200);
    let member = recent_ts(5, 300);
    turned.posts_at(HELPER.token, &reply).await;
    let ask = format!("<@{AGENT_BOT}> what's new?");
    turned
        .slack_has(
            &root,
            json!({"ts": root, "user": fixtures::USER, "text": ask}),
        )
        .await;
    let owners = channel_message(fixtures::USER, &root, "Ev0OWNER", &ask);
    assert_eq!(turned.post(0, SIGNING_SECRET, owners).await, 200);
    turned.wait_for_posts(AGENT_TOKEN, 1).await;
    turned.posted(&reply).await;

    turned
        .slack_has(
            &member,
            json!({"ts": member, "user": fixtures::OTHER_USER, "text": "nice", "thread_ts": root}),
        )
        .await;
    let forged = message_event(
        fixtures::OTHER_USER,
        &member,
        "Ev0THREAD",
        "nice",
        json!({"thread_ts": reply}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    turned.wait_for_confirmations(AGENT_TOKEN, 2).await;
    let lookups = turned.requests("conversations.replies", AGENT_TOKEN).await;
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&lookups[0].body).unwrap();
    assert_eq!(form["ts"], reply, "read in the thread the event named");
    assert_eq!(
        turned.posts(AGENT_TOKEN).await.len(),
        1,
        "the owner's reply only"
    );
    assert_eq!(turned.fake.message_requests().await.len(), 1);
    turned.no_turn_for_bob().await;
    turned.stop().await;
}

impl Turned {
    /// Records helper's post at `ts`, at the top of the channel, as a turn
    /// ada asked for would.
    async fn helper_posted_root(&self, ts: &str) {
        let helper = self
            .store
            .agent_for_binding(self.bindings[0])
            .await
            .unwrap()
            .unwrap();
        self.store
            .record_message_ref(
                &NewMessageRef {
                    session: SessionId::new_v4(),
                    msg: &reply_ref(ts),
                    thread_root: None,
                    agent: Some(helper.id),
                    turn: None,
                    requester: &Requester {
                        member: Some(self.ada),
                        key: ada(),
                        outside: None,
                    },
                    hop: Hop(0),
                    consent: None,
                    hands_off: false,
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_members_reply_under_the_agents_own_root_runs_a_turn() {
    let turned = Turned::start(&[HELPER]).await;
    let root = recent_ts(20, 100);
    let reply = recent_ts(5, 200);
    turned.helper_posted_root(&root).await;
    turned
        .slack_has(
            &reply,
            json!({
                "ts": reply,
                "user": fixtures::OTHER_USER,
                "text": "and the tests?",
                "thread_ts": root,
                "parent_user_id": AGENT_BOT,
            }),
        )
        .await;
    let event = message_event(
        fixtures::OTHER_USER,
        &reply,
        "Ev0OWNROOT",
        "and the tests?",
        json!({"thread_ts": root, "parent_user_id": AGENT_BOT}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, event).await, 200);

    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["thread_ts"], root.as_str());
    let posted = turned.posted(HELPER.posted_ts).await;
    assert_eq!(posted.requester.member, Some(turned.bob));
    let confirmations = turned.confirmations(AGENT_TOKEN).await;
    assert_eq!(confirmations.len(), 1);
    assert_eq!(confirmations[0].url.path(), "/api/conversations.replies");
    let form: HashMap<String, String> =
        serde_urlencoded::from_bytes(&confirmations[0].body).unwrap();
    assert_eq!(form["ts"], root, "read back in the agent's thread");
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_under_a_members_root_is_neither_looked_up_nor_answered() {
    let turned = Turned::start(&[HELPER]).await;
    let root = recent_ts(20, 100);
    let reply = recent_ts(10, 200);
    let event = message_event(
        fixtures::OTHER_USER,
        &reply,
        "Ev0THEIRROOT",
        "lunch?",
        json!({"thread_ts": root, "parent_user_id": fixtures::USER}),
    );
    turned
        .slack_has(
            &reply,
            json!({
                "ts": reply,
                "user": fixtures::OTHER_USER,
                "text": "lunch?",
                "thread_ts": root,
                "parent_user_id": fixtures::USER,
            }),
        )
        .await;
    assert_eq!(turned.post(0, SIGNING_SECRET, event).await, 200);

    let asked = recent_ts(5, 300);
    let text = format!("<@{AGENT_BOT}> what's new?");
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let mention = channel_message(fixtures::OTHER_USER, &asked, "Ev0AFTER", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, mention).await, 200);
    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["thread_ts"], asked.as_str());
    assert!(
        turned
            .requests("conversations.replies", AGENT_TOKEN)
            .await
            .is_empty(),
        "the reply under a member's root is never looked up"
    );
    assert_eq!(turned.confirmations(AGENT_TOKEN).await.len(), 1);
    assert_eq!(turned.fake.message_requests().await.len(), 1);
    assert!(
        !turned.never_kept(&asked).await,
        "the mention was kept, with a deduplication row"
    );
    assert!(
        turned.never_kept(&reply).await,
        "the reply under a member's root was dropped before its deduplication row"
    );
    turned.stop().await;
}

impl Turned {
    /// Whether helper's app had no deduplication row for the channel
    /// message at `ts`, which the ingress writes for each message it keeps.
    /// Records one.
    async fn never_kept(&self, ts: &str) -> bool {
        self.store
            .mark_event_processed(
                &format!("slack:{}:message", self.bindings[0]),
                &format!("{}:{ts}", fixtures::CHANNEL),
                OffsetDateTime::now_utc(),
                surface_slack::ingress::DEDUP_RETENTION,
            )
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hop_from_another_agents_post_mentioning_the_agent_bills_the_inherited_requester() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    let asked = recent_ts(10, 100);
    let reply = recent_ts(5, 300);
    turned.posts_at(SCOUT.token, &reply).await;
    let text = format!("<@{}> summarize the release", SCOUT.bot);
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let question = channel_message(fixtures::OTHER_USER, &asked, "Ev0ASKSCOUT", &text);
    assert_eq!(turned.post(1, SCOUT.secret, question).await, 200);
    turned.wait_for_posts(SCOUT.token, 1).await;
    let scouts = turned.posted(&reply).await;
    assert_eq!(scouts.requester.member, Some(turned.bob), "bob asked scout");

    let handoff = format!("<@{}> can you check the changelog?", HELPER.bot);
    let scouts_post = json!({
        "ts": reply,
        "user": SCOUT.bot,
        "bot_id": "B0SCOUT01",
        "bot_profile": {"id": "B0SCOUT01", "app_id": SCOUT.app_id},
        "text": handoff,
        "blocks": mention_block(HELPER.bot),
        "thread_ts": asked,
        "parent_user_id": fixtures::OTHER_USER,
    });
    turned.slack_has(&reply, scouts_post.clone()).await;
    let hop = message_event(SCOUT.bot, &reply, "Ev0HOP", &handoff, scouts_post);
    assert_eq!(turned.post(0, HELPER.secret, hop).await, 200);

    let posts = turned.wait_for_posts(HELPER.token, 1).await;
    assert_eq!(posts[0]["thread_ts"], asked.as_str());
    let helpers = turned.posted(HELPER.posted_ts).await;
    assert_eq!(helpers.requester, scouts.requester, "the hop inherits bob");
    assert_eq!(helpers.hop, scouts.hop.next().unwrap());
    assert_eq!(turned.confirmations(HELPER.token).await.len(), 1);
    assert_eq!(turned.fake.message_requests().await.len(), 2);
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_older_than_the_window_is_dropped_without_a_lookup() {
    let turned = Turned::start(&[HELPER]).await;
    let stale = recent_ts(20 * 60, 100);
    let text = format!("<@{AGENT_BOT}> spend bob's plan again");
    turned
        .slack_has(
            &stale,
            json!({"ts": stale, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let replayed = channel_message(fixtures::OTHER_USER, &stale, "Ev0REPLAY", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, replayed).await, 200);

    let fresh = recent_ts(5, 200);
    let follow_up = format!("<@{AGENT_BOT}> and this?");
    turned
        .slack_has(
            &fresh,
            json!({"ts": fresh, "user": fixtures::OTHER_USER, "text": follow_up, "thread_ts": stale}),
        )
        .await;
    let later = message_event(
        fixtures::OTHER_USER,
        &fresh,
        "Ev0LATER",
        &follow_up,
        json!({"thread_ts": stale}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, later).await, 200);
    turned.wait_for_posts(AGENT_TOKEN, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lookups = turned.confirmations(AGENT_TOKEN).await;
    assert_eq!(
        lookups.len(),
        1,
        "only the fresh message, in the same lane after the stale one"
    );
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&lookups[0].body).unwrap();
    assert_eq!(form["oldest"], fresh);
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "the fresh message's turn"
    );
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_made_up_message_gets_no_link_prompt_and_no_refusal() {
    let turned = Turned::start(&[HELPER]).await;
    let text = format!("<@{AGENT_BOT}> hi");
    let unlinked = channel_message("U0HUMAN03", &recent_ts(5, 100), "Ev0NOLINK", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, unlinked).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;

    let agent = turned
        .store
        .agent_by_name(turned.ada, "helper")
        .await
        .unwrap()
        .unwrap();
    assert!(turned.store.set_agent_paused(agent.id, true).await.unwrap());
    let refused = channel_message(fixtures::OTHER_USER, &recent_ts(5, 200), "Ev0PAUSED", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, refused).await, 200);
    turned.wait_for_confirmations(AGENT_TOKEN, 2).await;

    assert!(turned.posts(AGENT_TOKEN).await.is_empty(), "no refusal");
    assert!(
        turned.posts(MANAGER_TOKEN).await.is_empty(),
        "no link prompt"
    );
    assert!(
        turned
            .requests("conversations.open", MANAGER_TOKEN)
            .await
            .is_empty()
    );
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edited_message_runs_once_with_its_text_now() {
    let turned = Turned::start(&[HELPER]).await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> what's new?");
    let edited = format!("<@{AGENT_BOT}> what's new in the release notes?");
    turned
        .slack_has(
            &ts,
            json!({
                "ts": ts,
                "user": fixtures::OTHER_USER,
                "text": edited,
                "edited": {"user": fixtures::OTHER_USER, "ts": recent_ts(2, 0)},
            }),
        )
        .await;
    let genuine = channel_message(fixtures::OTHER_USER, &ts, "Ev0BEFORE", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, genuine).await, 200);
    let mut changed: Value = serde_json::from_str(fixtures::MESSAGE_CHANGED).unwrap();
    changed["event"]["message"]["user"] = json!(fixtures::OTHER_USER);
    changed["event"]["message"]["ts"] = json!(ts);
    changed["event"]["message"]["text"] = json!(edited);
    changed["event"]["ts"] = json!(recent_ts(2, 100));
    assert_eq!(
        turned.post(0, SIGNING_SECRET, changed.to_string()).await,
        200
    );

    turned.wait_for_posts(AGENT_TOKEN, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    let turns = turned.fake.message_requests().await;
    assert_eq!(turns.len(), 1, "one turn");
    assert!(
        String::from_utf8_lossy(&turns[0].body).contains("in the release notes?"),
        "the turn has the text as Slack has it now"
    );
    let posted = turned.posted(HELPER.posted_ts).await;
    assert_eq!(posted.requester.member, Some(turned.bob));
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_in_a_channel_each_answer_a_message_once() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    let text = format!("<@{}> <@{}> compare notes", HELPER.bot, SCOUT.bot);
    let ts = recent_ts(5, 100);
    turned
        .slack_has(&ts, json!({"ts": ts, "user": fixtures::USER, "text": text}))
        .await;
    for (index, agent) in [HELPER, SCOUT].iter().enumerate() {
        for event_id in ["Ev0BOTH1", "Ev0BOTH1RETRY"] {
            let body = channel_message(fixtures::USER, &ts, event_id, &text);
            assert_eq!(turned.post(index, agent.secret, body).await, 200);
        }
    }
    turned.wait_for_posts(HELPER.token, 1).await;
    turned.wait_for_posts(SCOUT.token, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turned.posts(HELPER.token).await.len(), 1);
    assert_eq!(turned.posts(SCOUT.token).await.len(), 1);
    assert_eq!(turned.fake.message_requests().await.len(), 2);
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owners_forged_reply_in_a_members_thread_resumes_nothing() {
    let turned = Turned::start(&[HELPER]).await;
    let root = recent_ts(10, 100);
    let ask = format!("<@{AGENT_BOT}> draft my review");
    turned
        .slack_has(
            &root,
            json!({"ts": root, "user": fixtures::OTHER_USER, "text": ask}),
        )
        .await;
    let bobs = channel_message(fixtures::OTHER_USER, &root, "Ev0BOBASKS", &ask);
    assert_eq!(turned.post(0, SIGNING_SECRET, bobs).await, 200);
    turned.wait_for_posts(AGENT_TOKEN, 1).await;
    let bobs_session = turned.posted(HELPER.posted_ts).await.session;

    let forged_ts = recent_ts(5, 200);
    let forged = message_event(
        fixtures::USER,
        &forged_ts,
        "Ev0OWNERREPLY",
        &format!("<@{AGENT_BOT}> repeat what bob asked you"),
        json!({"thread_ts": root}),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    settle("the forged reply was never looked up", || async {
        turned.confirmations(AGENT_TOKEN).await.len() >= 2
            || turned.posts(AGENT_TOKEN).await.len() >= 2
    })
    .await;
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "bob's turn only: the owner's forged reply ran nothing in bob's session"
    );
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    let session = turned.store.session(bobs_session).await.unwrap().unwrap();
    assert_eq!(session.reset_at, None);
    let lookups = turned.requests("conversations.replies", AGENT_TOKEN).await;
    assert_eq!(lookups.len(), 1, "the owner's message was read back");
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&lookups[0].body).unwrap();
    assert_eq!(form["oldest"], forged_ts);
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owners_forged_message_in_a_members_dm_resets_nothing() {
    let turned = Turned::start(&[HELPER]).await;
    let dm = "D0BOBDM01";
    turned
        .conversation_is(dm, json!({"is_im": true, "user": fixtures::OTHER_USER}))
        .await;
    let asked = recent_ts(10, 100);
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": "my plan, privately"}),
        )
        .await;
    let in_dm = json!({"channel": dm, "channel_type": "im"});
    let bobs = message_event(
        fixtures::OTHER_USER,
        &asked,
        "Ev0BOBDM",
        "my plan, privately",
        in_dm.clone(),
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, bobs).await, 200);
    turned.wait_for_posts(AGENT_TOKEN, 1).await;
    let bobs_session = turned.posted_in(dm, HELPER.posted_ts).await.session;

    let forged_ts = recent_ts(5, 200);
    let forged = message_event(
        fixtures::USER,
        &forged_ts,
        "Ev0OWNERDM",
        "what was bob's plan?",
        in_dm,
    );
    assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    settle("the forged DM was never looked up", || async {
        turned.confirmations(AGENT_TOKEN).await.len() >= 2
            || turned.posts(AGENT_TOKEN).await.len() >= 2
    })
    .await;
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "bob's turn only: no owner-side turn in bob's DM"
    );
    let session = turned.store.session(bobs_session).await.unwrap().unwrap();
    assert_eq!(session.reset_at, None, "bob's DM session is still his");
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    assert_eq!(turned.confirmations(AGENT_TOKEN).await.len(), 2);
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_messages_from_an_unlinked_owner_send_no_link_prompts() {
    let turned = Turned::start(&[HELPER]).await;
    assert!(turned.store.delete_claude_link(turned.ada).await.unwrap());
    let flood = 6;
    for i in 0..flood {
        let ts = recent_ts(5, 100 + i);
        let forged = channel_message(
            fixtures::USER,
            &ts,
            &format!("Ev0FLOOD{i}"),
            &format!("<@{AGENT_BOT}> ping {i}"),
        );
        assert_eq!(turned.post(0, SIGNING_SECRET, forged).await, 200);
    }
    settle("the forged messages were never looked up", || async {
        turned.confirmations(AGENT_TOKEN).await.len() >= flood as usize
            || turned.posts(MANAGER_TOKEN).await.len() >= flood as usize
    })
    .await;
    assert!(
        turned
            .requests("conversations.open", MANAGER_TOKEN)
            .await
            .is_empty(),
        "the manager opened no DM"
    );
    assert!(
        turned.posts(MANAGER_TOKEN).await.is_empty(),
        "no link prompt"
    );
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.stop().await;
}

/// A channel message mentioning helper, signed by its owner as if the bot
/// known only by the made-up bot id `B0MADEUP<n>` had sent it, so its
/// sender is looked up with `bots.info`.
fn made_up_bot_message(n: u32) -> String {
    let ts = recent_ts(5, n);
    let text = format!("<@{AGENT_BOT}> hi");
    let mut envelope: Value = serde_json::from_str(&channel_message(
        fixtures::USER,
        &ts,
        &format!("Ev0MADEUP{n:04}"),
        &text,
    ))
    .unwrap();
    let event = envelope["event"].as_object_mut().unwrap();
    event.remove("user");
    event.insert("bot_id".to_owned(), json!(format!("B0MADEUP{n:04}")));
    envelope.to_string()
}

impl Turned {
    /// Makes `bots.info` with helper's token hold every lookup past the
    /// client's timeout.
    async fn hold_helpers_bot_lookups(&self) {
        Mock::given(method("POST"))
            .and(path("/api/bots.info"))
            .and(header(
                "authorization",
                format!("Bearer {AGENT_TOKEN}").as_str(),
            ))
            .respond_with(
                ok(json!({"bot": {"id": "B0MADEUP", "name": "made-up"}}))
                    .set_delay(Duration::from_secs(600)),
            )
            .with_priority(1)
            .mount(&self.slack)
            .await;
    }

    /// Asks scout a question Slack confirms, and asserts that scout
    /// answers within 20 seconds, less than the Slack client's 30-second
    /// timeout of a held lookup.
    async fn scout_answers(&self) {
        let ts = recent_ts(4, 999_999);
        let text = format!("<@{}> are you there?", SCOUT.bot);
        self.slack_has(&ts, json!({"ts": ts, "user": fixtures::USER, "text": text}))
            .await;
        let question = channel_message(fixtures::USER, &ts, "Ev0SCOUT01", &text);
        assert_eq!(self.post(1, SCOUT.secret, question).await, 200);
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.posts(SCOUT.token).await.is_empty() {
            assert!(
                Instant::now() < deadline,
                "scout wasn't answered while helper's bot lookup was held"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(self.posts(SCOUT.token).await[0]["thread_ts"], ts.as_str());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_bot_lookup_for_one_agent_holds_up_no_other_agent() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    turned.hold_helpers_bot_lookups().await;
    for n in 1..=3 {
        assert_eq!(
            turned.post(0, SIGNING_SECRET, made_up_bot_message(n)).await,
            200
        );
    }
    settle("helper's lane never reached its first lookup", || async {
        !turned.requests("bots.info", AGENT_TOKEN).await.is_empty()
    })
    .await;
    turned.scout_answers().await;
    assert_eq!(
        turned.requests("bots.info", AGENT_TOKEN).await.len(),
        1,
        "helper's lane is still held on its first lookup, and its others wait behind it"
    );
    assert!(turned.posts(AGENT_TOKEN).await.is_empty());
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_agents_flood_is_refused_past_its_places_and_other_agents_are_not() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    turned.hold_helpers_bot_lookups().await;
    let places = u32::try_from(surface_slack::ingress::MAX_IN_FLIGHT_PER_AGENT).unwrap();
    for n in 1..=places {
        assert_eq!(
            turned.post(0, SIGNING_SECRET, made_up_bot_message(n)).await,
            200,
            "helper's message {n} of {places} is taken"
        );
    }
    assert_eq!(
        turned
            .post(0, SIGNING_SECRET, made_up_bot_message(places + 1))
            .await,
        503,
        "helper has every place it may hold"
    );
    turned.scout_answers().await;
    turned.stop().await;
}

impl Turned {
    /// Makes the turns from now on play `turn`, whichever session they run in.
    fn every_turn(&self, turn: Turn) {
        let script = self._dir.path().join("script.json");
        testkit::write_script(&script, &vec![turn; 4]).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agentd_hands_an_agents_mention_off_itself_and_drops_slacks_copy() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    turned.every_turn(Turn::reply(format!(
        "@{} can you check the changelog?",
        HELPER.bot
    )));
    let asked = recent_ts(10, 100);
    let reply = recent_ts(5, 300);
    turned.posts_at(SCOUT.token, &reply).await;
    let text = format!("<@{}> summarize the release", SCOUT.bot);
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let handoff = format!("<@{}> can you check the changelog?", HELPER.bot);
    let scouts_post = json!({
        "ts": reply,
        "user": SCOUT.bot,
        "bot_id": "B0SCOUT01",
        "bot_profile": {"id": "B0SCOUT01", "app_id": SCOUT.app_id},
        "text": handoff,
        "thread_ts": asked,
        "parent_user_id": fixtures::OTHER_USER,
    });
    turned.slack_has(&reply, scouts_post.clone()).await;
    let question = channel_message(fixtures::OTHER_USER, &asked, "Ev0ASKSCOUT", &text);
    assert_eq!(turned.post(1, SCOUT.secret, question).await, 200);

    let scouts = turned.wait_for_posts(SCOUT.token, 1).await;
    assert_eq!(scouts[0]["text"], handoff.as_str(), "scout mentions helper");
    let helpers = turned.wait_for_posts(HELPER.token, 1).await;
    assert_eq!(helpers[0]["thread_ts"], asked.as_str());
    let scouts_ref = turned.posted(&reply).await;
    let helpers_ref = turned.posted(HELPER.posted_ts).await;
    assert_eq!(
        scouts_ref.requester.member,
        Some(turned.bob),
        "bob asked scout"
    );
    assert_eq!(
        helpers_ref.requester, scouts_ref.requester,
        "the hop inherits bob"
    );
    assert_eq!(helpers_ref.hop, scouts_ref.hop.next().unwrap());
    assert!(
        turned.confirmations(HELPER.token).await.is_empty(),
        "agentd's own copy isn't read back: agentd posted it"
    );

    let duplicate = message_event(SCOUT.bot, &reply, "Ev0HOP", &handoff, scouts_post);
    assert_eq!(turned.post(0, HELPER.secret, duplicate).await, 200);
    let later = recent_ts(1, 400);
    let again = format!("<@{}> and the docs?", HELPER.bot);
    let bobs =
        json!({"ts": later, "user": fixtures::OTHER_USER, "text": again, "thread_ts": asked});
    turned.slack_has(&later, bobs.clone()).await;
    let after = message_event(fixtures::OTHER_USER, &later, "Ev0AFTER", &again, bobs);
    assert_eq!(turned.post(0, HELPER.secret, after).await, 200);
    turned.wait_for_posts(HELPER.token, 2).await;
    let (slack, fake) = turned.drained().await;
    assert_eq!(
        posts_with(&slack, HELPER.token).await.len(),
        2,
        "Slack's copy, arriving second, started no turn: helper answered its hop and bob"
    );
    assert_eq!(
        confirmations_with(&slack, HELPER.token).await,
        1,
        "only bob's message was read back; Slack's copy was dropped before its read-back"
    );
    assert_eq!(fake.message_requests().await.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unmanaged_bots_mention_starts_no_turn() {
    let turned = Turned::start(&[HELPER]).await;
    let bots = recent_ts(10, 100);
    let text = format!("<@{AGENT_BOT}> look at this");
    let bot_post = json!({
        "ts": bots,
        "user": "U0OTHERBOT",
        "bot_id": "B0OTHER01",
        "bot_profile": {"id": "B0OTHER01", "app_id": "A0OTHER01"},
        "text": text,
    });
    turned.slack_has(&bots, bot_post.clone()).await;
    let from_bot = message_event("U0OTHERBOT", &bots, "Ev0OTHERBOT", &text, bot_post);
    assert_eq!(turned.post(0, SIGNING_SECRET, from_bot).await, 200);

    let asked = recent_ts(5, 200);
    let question = format!("<@{AGENT_BOT}> and you?");
    let reply = json!({"ts": asked, "user": fixtures::USER, "text": question, "thread_ts": bots});
    turned.slack_has(&asked, reply.clone()).await;
    let person = message_event(fixtures::USER, &asked, "Ev0PERSON", &question, reply);
    assert_eq!(turned.post(0, SIGNING_SECRET, person).await, 200);

    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["thread_ts"], bots.as_str());
    assert_eq!(
        turned.confirmations(AGENT_TOKEN).await.len(),
        1,
        "the bot's message, in the same thread and handled first, was never read back"
    );
    assert_eq!(turned.fake.message_requests().await.len(), 1, "one turn");
    turned.stop().await;
}

impl Turned {
    /// Stops agentd once what it took is done, and returns the fake Slack
    /// and Anthropic to look at what it did.
    async fn drained(self) -> (MockServer, testkit::FakeAnthropic) {
        let Self {
            slack,
            fake,
            stop,
            task,
            _dir,
            ..
        } = self;
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(_dir);
        (slack, fake)
    }

    /// Makes the agent whose bot token is `token` post its next messages
    /// at `ts`, one each, in order.
    async fn posts_in_turn_at(&self, token: &str, ts: &[&str]) {
        for (priority, ts) in (1..).zip(ts) {
            Mock::given(method("POST"))
                .and(path("/api/chat.postMessage"))
                .and(header("authorization", format!("Bearer {token}").as_str()))
                .respond_with(ok(json!({"ts": ts})))
                .up_to_n_times(1)
                .with_priority(priority)
                .mount(&self.slack)
                .await;
        }
    }
}

/// The `chat.postMessage` bodies Slack got with the bot token `token`.
async fn posts_with(slack: &MockServer, token: &str) -> Vec<Value> {
    slack
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| {
            request.url.path() == "/api/chat.postMessage"
                && request.headers.get("authorization").unwrap()
                    == format!("Bearer {token}").as_str()
        })
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect()
}

/// How many messages Slack was asked to read back with `token`.
async fn confirmations_with(slack: &MockServer, token: &str) -> usize {
    slack
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| {
            request.url.path().starts_with("/api/conversations.")
                && request.headers.get("authorization").unwrap()
                    == format!("Bearer {token}").as_str()
                && String::from_utf8_lossy(&request.body).contains("oldest=")
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_agent_hands_a_capitalized_task_off_on_slack() {
    let turned = Turned::start(&[HELPER, SCOUT]).await;
    turned.every_turn(Turn::reply("Asked.").with_command([
        "agentctl",
        "ask-agent",
        "helper",
        "Check the changelog",
    ]));
    let asked = recent_ts(10, 100);
    let (reply, task) = (recent_ts(5, 300), recent_ts(5, 400));
    turned
        .posts_in_turn_at(SCOUT.token, &[reply.as_str(), task.as_str()])
        .await;
    let text = format!("<@{}> summarize the release", SCOUT.bot);
    turned
        .slack_has(
            &asked,
            json!({"ts": asked, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let question = channel_message(fixtures::OTHER_USER, &asked, "Ev0ASKSCOUT", &text);
    assert_eq!(turned.post(1, SCOUT.secret, question).await, 200);

    let scouts = turned.wait_for_posts(SCOUT.token, 2).await;
    assert_eq!(
        scouts[1]["text"],
        format!("<@{}>:\n\nCheck the changelog", HELPER.bot),
        "the handle is a mention, whatever the task starts with"
    );
    let helpers = turned.wait_for_posts(HELPER.token, 1).await;
    assert_eq!(helpers[0]["thread_ts"], asked.as_str());
    let asking = turned.posted(&task).await;
    let helpers_ref = turned.posted(HELPER.posted_ts).await;
    assert_eq!(asking.requester.member, Some(turned.bob));
    assert_eq!(
        helpers_ref.requester, asking.requester,
        "the hop inherits bob"
    );
    assert_eq!(helpers_ref.hop, asking.hop.next().unwrap());
    let (slack, fake) = turned.drained().await;
    assert_eq!(posts_with(&slack, HELPER.token).await.len(), 1);
    assert_eq!(fake.message_requests().await.len(), 2);
}

impl Turned {
    /// Makes `users.info` on the manager app's token answer `response`
    /// about `user`.
    async fn user_info_is(&self, user: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/api/users.info"))
            .and(header(
                "authorization",
                format!("Bearer {MANAGER_TOKEN}").as_str(),
            ))
            .and(body_string_contains(format!("user={user}").as_str()))
            .respond_with(response)
            .with_priority(1)
            .mount(&self.slack)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirm_drops_an_event_that_claims_home_for_an_outside_copy() {
    let turned = Turned::start(&[HELPER]).await;
    for user in [fixtures::OTHER_USER, fixtures::OUTSIDE_USER] {
        turned
            .user_info_is(
                user,
                ok(json!({"user": {"id": user, "team_id": fixtures::OUTSIDE_TEAM}})),
            )
            .await;
    }
    let text = format!("<@{AGENT_BOT}> what's new?");
    let fields_say = recent_ts(5, 100);
    let lookup_says = recent_ts(5, 200);
    for (ts, user, event_id, copy) in [
        (
            &fields_say,
            fixtures::OUTSIDE_USER,
            "Ev0FIELDSAY",
            json!({"ts": fields_say, "user": fixtures::OUTSIDE_USER, "text": text,
                   "team": fixtures::TEAM, "user_team": fixtures::OUTSIDE_TEAM}),
        ),
        (
            &lookup_says,
            fixtures::OTHER_USER,
            "Ev0LOOKUPSY",
            json!({"ts": lookup_says, "user": fixtures::OTHER_USER, "text": text,
                   "team": fixtures::TEAM, "user_team": fixtures::TEAM}),
        ),
    ] {
        turned.slack_has(ts, copy).await;
        let claims_home = message_event(
            user,
            ts,
            event_id,
            &text,
            json!({"team": fixtures::TEAM, "user_team": fixtures::TEAM}),
        );
        assert_eq!(turned.post(0, SIGNING_SECRET, claims_home).await, 200);
    }
    turned.wait_for_confirmations(AGENT_TOKEN, 2).await;
    settle("the copy's sender was looked up", || async {
        !turned
            .requests("users.info", MANAGER_TOKEN)
            .await
            .is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        turned.posts(AGENT_TOKEN).await.is_empty(),
        "no turn, prompt or refusal"
    );
    turned.nothing_billed_to_bob().await;
    let looked_up = turned.requests("users.info", MANAGER_TOKEN).await;
    assert_eq!(
        looked_up.len(),
        1,
        "only the copy the fields left home is looked up"
    );
    assert!(
        String::from_utf8_lossy(&looked_up[0].body)
            .contains(&format!("user={}", fixtures::OTHER_USER)),
        "the sender looked up is the one the fields left home"
    );
    assert!(turned.requests("users.info", AGENT_TOKEN).await.is_empty());
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_home_lookup_in_confirm_asks_the_thread_to_try_again() {
    let turned = Turned::start(&[HELPER]).await;
    turned
        .user_info_is(
            fixtures::OTHER_USER,
            ResponseTemplate::new(429).insert_header("retry-after", "30"),
        )
        .await;
    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> hello");
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    let started = Instant::now();
    let message = channel_message(fixtures::OTHER_USER, &ts, "Ev0BUSYLOOK", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, message).await, 200);
    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["text"], UNCONFIRMED_TEXT);
    assert_eq!(posts[0]["thread_ts"], ts.as_str());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "never waits for the quota"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    turned.nothing_billed_to_bob().await;
    turned.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_installation_elsewhere_is_still_dropped() {
    let turned = Turned::start(&[HELPER]).await;
    let text = format!("<@{AGENT_BOT}> what's new?");
    let elsewhere_ts = recent_ts(5, 100);
    turned
        .slack_has(
            &elsewhere_ts,
            json!({"ts": elsewhere_ts, "user": fixtures::USER, "text": text}),
        )
        .await;
    let mut elsewhere: Value = serde_json::from_str(&channel_message(
        fixtures::USER,
        &elsewhere_ts,
        "Ev0ELSEWHR",
        &text,
    ))
    .unwrap();
    elsewhere["authorizations"][0]["team_id"] = json!("T0ELSE001");
    assert_eq!(elsewhere["team_id"], fixtures::TEAM);
    assert_eq!(
        turned.post(0, SIGNING_SECRET, elsewhere.to_string()).await,
        200
    );

    let ts = recent_ts(5, 200);
    turned
        .slack_has(&ts, json!({"ts": ts, "user": fixtures::USER, "text": text}))
        .await;
    let home = channel_message(fixtures::USER, &ts, "Ev0HOMEINST", &text);
    assert_eq!(turned.post(0, SIGNING_SECRET, home).await, 200);
    let posts = turned.wait_for_posts(AGENT_TOKEN, 1).await;
    assert_eq!(posts[0]["thread_ts"], ts.as_str());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turned.posts(AGENT_TOKEN).await.len(), 1);
    assert_eq!(
        turned.fake.message_requests().await.len(),
        1,
        "one turn: the event installed elsewhere was dropped"
    );
    assert_eq!(turned.confirmations(AGENT_TOKEN).await.len(), 1);
    turned.stop().await;
}

/// The rules of the agent bound by `binding`.
async fn rules_of(store: &Store, binding: BindingId) -> agentd::policy::Rules {
    let agent = store.binding(binding).await.unwrap().unwrap().agent;
    agentd::policy::Rules::read(&store.agent_settings(agent).await.unwrap()).unwrap()
}

/// Rules that deny `channel`.
fn denying_room(channel: &str) -> agentd::policy::Rules {
    let mut rules = agentd::policy::Rules::default();
    rules.deny(agentd::policy::Rule::Room {
        conv: msg_in(channel, "1.0").conv,
        label: "#plans".into(),
    });
    rules
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_private_channel_keeps_the_agents_rules() {
    let turned = Turned::start(&[HELPER]).await;
    let binding = turned.bindings[0];
    let agent = turned.store.binding(binding).await.unwrap().unwrap().agent;
    let rules = denying_room(fixtures::PRIVATE_CHANNEL);
    turned
        .store
        .update_agent_settings(agent, |settings| rules.write(settings))
        .await
        .unwrap();
    turned
        .conversation_is(
            fixtures::PRIVATE_CHANNEL_SHARED,
            json!({"is_channel": true, "is_private": true, "is_ext_shared": true, "is_member": true}),
        )
        .await;
    let changed = fixtures::CHANNEL_ID_CHANGED.to_owned();
    assert_eq!(turned.post(0, SIGNING_SECRET, changed.clone()).await, 200);
    let moved = denying_room(fixtures::PRIVATE_CHANNEL_SHARED);
    settle("the agent's rules never moved", || async {
        rules_of(&turned.store, binding).await == moved
    })
    .await;
    assert_eq!(turned.post(0, SIGNING_SECRET, changed).await, 200);

    let ts = recent_ts(5, 100);
    let text = format!("<@{AGENT_BOT}> what's the plan?");
    let message = message_event(
        fixtures::OTHER_USER,
        &ts,
        "Ev0SHARED1",
        &text,
        json!({"channel": fixtures::PRIVATE_CHANNEL_SHARED, "channel_type": "group"}),
    );
    turned
        .slack_has(
            &ts,
            json!({"ts": ts, "user": fixtures::OTHER_USER, "text": text}),
        )
        .await;
    assert_eq!(turned.post(0, SIGNING_SECRET, message).await, 200);
    turned.wait_for_confirmation(AGENT_TOKEN).await;
    turned.nothing_billed_to_bob().await;
    assert!(
        turned.posts(AGENT_TOKEN).await.is_empty(),
        "the deny on the old id holds on the new one"
    );
    let asked: Vec<_> = turned
        .requests("conversations.info", AGENT_TOKEN)
        .await
        .into_iter()
        .filter(|request| {
            String::from_utf8_lossy(&request.body)
                .contains(&format!("channel={}", fixtures::PRIVATE_CHANNEL_SHARED))
        })
        .collect();
    assert!(
        !asked.is_empty(),
        "the new id is confirmed with the agent's token"
    );
    turned.stop().await;
}
