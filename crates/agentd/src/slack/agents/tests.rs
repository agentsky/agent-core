//! [`SlackAgents`] and the Slack queue's handling of agents' messages
//! against a wiremock Slack, on an in-memory store.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use core_types::{
    BindingId, ConvKind, ConvRef, InboundEvent, MemberKey, MsgRef, SendError, Sender, Sink,
    SurfaceKind, TeamId, UserId,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{NewClaudeLink, NewSlackConfigToken, Sealer, Store};
use surface_slack::{InFlight, SlackClient, SlackInbound};
use testkit::Held;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::commands::intake::CommandIntake;
use crate::commands::{Commands, Replies};
use crate::policy::Rules;
use crate::slack::manager::ManagerIdentity;
use crate::slack::{Inbound, Messages};
use crate::telemetry::tests::global_logs;

const TEAM: &str = "T0TEAM001";
const MANAGER_TOKEN: &str = "xoxb-manager-SECRET";
const CONFIG_TOKEN: &str = "xoxe.xoxp-1-config-SECRET";
const CLIENT_SECRET: &str = "client-SECRET-helper";
const SIGNING_SECRET: &str = "signing-SECRET-helper";
const AGENT_TOKEN: &str = "xoxb-helper-SECRET";
const CODE: &str = "oauth-code-SECRET";
const SECRETS: [&str; 6] = [
    MANAGER_TOKEN,
    CONFIG_TOKEN,
    CLIENT_SECRET,
    SIGNING_SECRET,
    AGENT_TOKEN,
    CODE,
];

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

fn ada() -> MemberKey {
    MemberKey {
        surface: SurfaceKind::Slack,
        team: TeamId::new(TEAM),
        user: UserId::new("U0ADA0001"),
    }
}

struct Harness {
    store: Store,
    slack: MockServer,
    agents: SlackAgents,
    owner: MemberId,
}

async fn mount(slack: &MockServer, name: &str, token: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{name}")))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(response)
        .mount(slack)
        .await;
}

fn app_created() -> ResponseTemplate {
    ok(json!({
        "app_id": "A0HELPER1",
        "credentials": {
            "client_id": "1111.2222",
            "client_secret": CLIENT_SECRET,
            "signing_secret": SIGNING_SECRET,
        },
    }))
}

fn app_installed() -> ResponseTemplate {
    ok(json!({
        "app_id": "A0HELPER1",
        "token_type": "bot",
        "access_token": AGENT_TOKEN,
        "bot_user_id": "U0HELPER1",
        "team": {"id": TEAM},
    }))
}

async fn harness() -> Harness {
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let slack = MockServer::start().await;
    for (name, body) in [
        ("chat.postMessage", json!({"ts": "1727700000.000100"})),
        (
            "conversations.open",
            json!({"channel": {"id": "D0DM00001"}}),
        ),
        ("users.list", json!({"members": []})),
        (
            "users.info",
            json!({"user": {"id": "U0ADA0001", "team_id": TEAM}}),
        ),
    ] {
        mount(&slack, name, MANAGER_TOKEN, ok(body)).await;
    }
    mount(&slack, "apps.manifest.create", CONFIG_TOKEN, app_created()).await;
    mount(&slack, "apps.manifest.delete", CONFIG_TOKEN, ok(json!({}))).await;
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("1111.2222:{CLIENT_SECRET}"))
    );
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(header("authorization", basic.as_str()))
        .respond_with(app_installed())
        .mount(&slack)
        .await;
    let client = SlackClient::new(&format!("{}/api/", slack.uri()))
        .unwrap()
        .with_max_retry_wait(Duration::from_secs(5));
    let manager = SlackManager::with_identity(
        client.clone(),
        client.bot(SecretString::from(MANAGER_TOKEN)),
        ManagerIdentity {
            team: TeamId::new(TEAM),
            bot_user: UserId::new("U0MANAGER"),
            bot_id: "B0MANAGER".to_owned(),
            app_id: "A0MANAGER".to_owned(),
            app_name: None,
            enterprise: None,
        },
    );
    let bots = SlackBots::new(store.clone(), client, manager.surface());
    let agents = SlackAgents::new(
        store.clone(),
        manager,
        bots,
        AgentAppSettings {
            public_url: Some("https://agentd.example.com".to_owned()),
            public_posting: false,
            reminder_after: Duration::from_secs(3600),
            max_per_owner: 10,
        },
    );
    let now = OffsetDateTime::now_utc();
    let owner = store.ensure_member(&ada(), "ada", now).await.unwrap();
    store
        .put_claude_link(
            owner,
            &NewClaudeLink {
                access_token: SecretString::from("access"),
                refresh_token: SecretString::from("refresh"),
                expires_at: now + Duration::from_secs(3600),
                plan: None,
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    store
        .put_slack_config_token(
            owner,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from(CONFIG_TOKEN),
                refresh_token: SecretString::from("refresh-SECRET"),
                expires_at: now + Duration::from_secs(3600),
            },
            now,
        )
        .await
        .unwrap();
    Harness {
        store,
        slack,
        agents,
        owner,
    }
}

impl Harness {
    async fn create(&self) -> Creation {
        self.agents
            .create(self.owner, &ada(), "helper", "You help.")
            .await
            .unwrap()
    }

    async fn binding(&self) -> Option<BindingId> {
        let agent = self
            .store
            .agent_by_name(self.owner, "helper")
            .await
            .unwrap()?;
        Some(self.store.bindings_of(agent.id).await.unwrap()[0].id)
    }

    async fn calls(&self, name: &str) -> usize {
        let wanted = format!("/api/{name}");
        self.slack
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == wanted)
            .count()
    }
}

fn state_of(url: &str) -> String {
    let (_, query) = url.split_once('?').unwrap();
    serde_urlencoded::from_str::<Vec<(String, String)>>(query)
        .unwrap()
        .into_iter()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
}

#[tokio::test]
async fn creating_installing_and_deleting_an_app_never_logs_a_secret() {
    let h = harness().await;
    let logs = global_logs().tag();

    let Creation::Created {
        install_url,
        dm_sent: true,
    } = h.create().await
    else {
        panic!("created");
    };
    let query = format!("code={CODE}&state={}", state_of(&install_url));
    let response = h.agents.callback(Some(&query)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    let agent = h
        .store
        .agent_by_name(h.owner, "helper")
        .await
        .unwrap()
        .unwrap()
        .id;
    assert!(
        h.store
            .delete_agent(agent, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    let bindings = h.store.bindings_of(agent).await.unwrap();
    assert_eq!(
        h.agents.delete_apps(h.owner, &bindings).await,
        [AppDeletion::Deleted]
    );

    let logged = logs.snapshot();
    logged
        .assert_has("created an agent's Slack app")
        .assert_has("installed an agent's Slack app")
        .assert_has("deleted a deleted agent's Slack app")
        .assert_has("\"target\":\"sqlx::query\"")
        .assert_has("\"target\":\"hyper_util::client::legacy::pool\"")
        .assert_has("\"level\":\"TRACE\"");
    for secret in SECRETS {
        logged.assert_lacks(secret);
    }
}

#[tokio::test]
async fn a_creation_abandoned_while_slack_creates_the_app_deletes_the_app_again() {
    let h = harness().await;
    let (held, mut hold) = Held::new(app_created());
    let bearer = format!("Bearer {CONFIG_TOKEN}");
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.create"))
        .and(header("authorization", bearer.as_str()))
        .respond_with(held)
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let creating = {
        let agents = h.agents.clone();
        let owner = h.owner;
        tokio::spawn(async move {
            agents
                .create(owner, &ada(), "helper", "You help.")
                .await
                .unwrap()
        })
    };
    hold.arrived().await;
    let binding = h.binding().await.unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(h.store.abandon_creation(binding, now, now).await.unwrap());
    hold.release();
    assert_eq!(creating.await.unwrap(), Creation::Failed);
    assert_eq!(h.calls("apps.manifest.delete").await, 1);
    assert_eq!(h.calls("chat.postMessage").await, 0, "no install link");
}

#[tokio::test]
async fn the_sweeper_abandons_a_creation_that_stopped_halfway() {
    let h = harness().await;
    let team = TeamId::new(TEAM);
    let long_ago = OffsetDateTime::now_utc() - Duration::from_secs(3600);
    let store::AgentCreation::Created(_, binding) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner: h.owner,
                name: "helper",
                persona: "You help.",
                visibility: Visibility::Public,
                surface: SurfaceKind::Slack,
                team: &team,
            },
            10,
            long_ago,
        )
        .await
        .unwrap()
    else {
        panic!("created");
    };
    let pass = h.agents.pass().await;
    assert_eq!(
        pass,
        SweepPass {
            abandoned: 1,
            ..SweepPass::default()
        }
    );
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Disabled);
    assert!(h.binding().await.is_none(), "the name is free again");
}

#[tokio::test]
async fn nothing_is_created_without_a_usable_configuration_token() {
    let h = harness().await;
    h.store.delete_slack_config_tokens(h.owner).await.unwrap();
    assert_eq!(h.create().await, Creation::NoConfigToken);
    assert!(h.binding().await.is_none());
    assert_eq!(h.calls("apps.manifest.create").await, 0);
}

struct Collect(mpsc::UnboundedSender<InboundEvent>);

#[async_trait::async_trait]
impl Sink<InboundEvent> for Collect {
    async fn send(&self, item: InboundEvent) -> Result<(), SendError> {
        self.0.send(item).map_err(|_| SendError)
    }
}

fn bot_message(binding: BindingId, team: &str) -> InboundEvent {
    let conv = ConvRef {
        surface: SurfaceKind::Slack,
        team: TeamId::new(team),
        conversation: "C0CHAN001".into(),
    };
    InboundEvent {
        event_id: "Ev1".to_owned(),
        binding,
        sender: MemberKey {
            surface: SurfaceKind::Slack,
            team: TeamId::new(team),
            user: UserId::new("B0OTHERBOT"),
        },
        sender_is_bot: true,
        sender_bot_user: None,
        conv: conv.clone(),
        conv_kind: ConvKind::Channel,
        thread_root: None,
        message: MsgRef {
            conv,
            id: "1727697600.000100".into(),
        },
        text: "<@U0HELPER1> hi".to_owned(),
        mentions: vec![UserId::new("U0HELPER1")],
        reply_to: None,
        files: vec![],
        received_at: OffsetDateTime::now_utc(),
        outside: None,
    }
}

#[tokio::test]
async fn an_agents_message_reaches_the_pipeline_with_its_bot_sender_looked_up() {
    let h = harness().await;
    mount(
        &h.slack,
        "bots.info",
        AGENT_TOKEN,
        ok(json!({"bot": {"id": "B0OTHERBOT", "user_id": "U0OTHERBOT"}})),
    )
    .await;
    let Creation::Created { install_url, .. } = h.create().await else {
        panic!("created");
    };
    let binding = h.binding().await.unwrap();
    let Creation::Created { .. } = h
        .agents
        .create(h.owner, &ada(), "waiting", "You wait.")
        .await
        .unwrap()
    else {
        panic!("created");
    };
    let waiting = h
        .store
        .bindings_of(
            h.store
                .agent_by_name(h.owner, "waiting")
                .await
                .unwrap()
                .unwrap()
                .id,
        )
        .await
        .unwrap()[0]
        .id;
    let query = format!("code={CODE}&state={}", state_of(&install_url));
    assert_eq!(
        h.agents.callback(Some(&query)).await.status(),
        StatusCode::OK
    );
    let auth = Arc::new(auth::Auth::new(auth::OAuthConfig::default(), h.store.clone()).unwrap());
    let git = crate::skills::Git::new(cred_proxy::EgressPolicy::new(Vec::new(), Vec::new()));
    let skills = crate::skills::Skills::new(h.store.clone(), "/nonexistent/agentd".into(), git);
    let commands = Commands::new(
        h.store.clone(),
        auth,
        Replies::default(),
        None,
        None,
        skills,
    );
    let (_intake, submitter) = CommandIntake::new(commands);
    let (messages, worker) = Messages::new(h.agents.bots().clone());
    let inbound = Inbound::new(
        h.store.clone(),
        Some(h.agents.inner.manager.identity().clone()),
        submitter,
    )
    .with_agents(messages.clone(), h.agents.clone());
    inbound
        .send(SlackInbound::Message(
            Box::new(bot_message(binding, TEAM)),
            InFlight::untracked(),
        ))
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    messages.connect(Sender::new(Collect(tx)));
    let worker = tokio::spawn(worker);

    for (event_id, event) in [
        ("Ev1", bot_message(waiting, TEAM)),
        ("Ev2", bot_message(binding, "T0OTHER01")),
        ("Ev3", bot_message(BindingId::new_v4(), TEAM)),
        ("Ev4", bot_message(binding, TEAM)),
    ] {
        let event = InboundEvent {
            event_id: event_id.to_owned(),
            ..event
        };
        inbound
            .send(SlackInbound::Message(
                Box::new(event),
                InFlight::untracked(),
            ))
            .await
            .unwrap();
    }
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("handed on")
        .unwrap();
    assert_eq!(
        event.event_id, "Ev4",
        "dropped: before the pipeline was connected, not installed yet, another workspace, an \
         unknown binding"
    );
    assert_eq!(event.sender.user, UserId::new("U0OTHERBOT"));
    assert_eq!(event.sender_bot_user, Some(UserId::new("U0OTHERBOT")));

    drop((inbound, messages));
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("the worker ends with its queue")
        .unwrap();
    assert!(rx.try_recv().is_err());
}

struct Closed;

#[async_trait::async_trait]
impl Sink<InboundEvent> for Closed {
    async fn send(&self, _: InboundEvent) -> Result<(), SendError> {
        Err(SendError)
    }

    fn is_closed(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn a_closed_pipeline_gets_no_lookups_for_the_messages_left() {
    let h = harness().await;
    mount(
        &h.slack,
        "bots.info",
        AGENT_TOKEN,
        ok(json!({"bot": {"id": "B0OTHERBOT", "user_id": "U0OTHERBOT"}})),
    )
    .await;
    let binding = installed(&h).await;
    let (messages, worker) = Messages::new(h.agents.bots().clone());
    messages.connect(Sender::new(Closed));
    let worker = tokio::spawn(worker);
    for _ in 0..3 {
        messages.hand(bot_message(binding, TEAM), InFlight::untracked());
    }
    drop(messages);
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("the worker ends with its queue")
        .unwrap();
    let lookups = h
        .slack
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/api/bots.info")
        .count();
    assert_eq!(lookups, 0);
}

async fn installed(h: &Harness) -> BindingId {
    let Creation::Created { install_url, .. } = h.create().await else {
        panic!("created");
    };
    let query = format!("code={CODE}&state={}", state_of(&install_url));
    assert_eq!(
        h.agents.callback(Some(&query)).await.status(),
        StatusCode::OK
    );
    h.binding().await.unwrap()
}

async fn deleted_bindings(h: &Harness) -> Vec<store::AgentBinding> {
    let agent = h
        .store
        .agent_by_name(h.owner, "helper")
        .await
        .unwrap()
        .unwrap()
        .id;
    let bindings = h.store.bindings_of(agent).await.unwrap();
    assert!(
        h.store
            .delete_agent(agent, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    bindings
}

#[tokio::test]
async fn an_installed_bot_is_named_as_a_managed_agent() {
    let h = harness().await;
    let directory = Arc::clone(h.agents.inner.manager.surface().directory());
    assert!(directory.members().lookup("U0HELPER1").is_none());
    installed(&h).await;
    assert_eq!(
        directory.members().lookup("U0HELPER1"),
        Some(&UserId::new("U0HELPER1"))
    );
    let bindings = deleted_bindings(&h).await;
    h.agents.delete_apps(h.owner, &bindings).await;
    assert!(directory.members().lookup("U0HELPER1").is_none());
}

#[tokio::test]
async fn an_app_slack_says_is_gone_counts_as_deleted() {
    let h = harness().await;
    let binding = installed(&h).await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.delete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "app_not_found"})),
        )
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let bindings = deleted_bindings(&h).await;
    assert_eq!(
        h.agents.delete_apps(h.owner, &bindings).await,
        [AppDeletion::Deleted]
    );
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert!(row.retired_at.is_some());
}

#[tokio::test]
async fn a_configuration_token_slack_refuses_is_marked_broken() {
    let h = harness().await;
    let binding = installed(&h).await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.delete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "token_revoked"})),
        )
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let bindings = deleted_bindings(&h).await;
    assert_eq!(
        h.agents.delete_apps(h.owner, &bindings).await,
        [AppDeletion::NoToken("A0HELPER1".to_owned())]
    );
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert!(row.retired_at.is_none());
    let status = h
        .store
        .slack_config_token_status(h.owner, &TeamId::new(TEAM))
        .await
        .unwrap()
        .unwrap();
    assert!(status.broken);
}

#[tokio::test]
async fn a_refused_token_at_creation_is_marked_broken_and_an_expired_one_waits_for_renewal() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.create"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "invalid_auth"})),
        )
        .with_priority(1)
        .mount(&h.slack)
        .await;
    assert_eq!(h.create().await, Creation::TokenRefused);
    assert_eq!(h.create().await, Creation::NoConfigToken, "broken now");

    let now = OffsetDateTime::now_utc();
    h.store
        .put_slack_config_token(
            h.owner,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from(CONFIG_TOKEN),
                refresh_token: SecretString::from("refresh-SECRET"),
                expires_at: now - Duration::from_secs(60),
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(h.create().await, Creation::TokenRenewing);
    assert!(h.binding().await.is_none());
}

#[tokio::test]
async fn an_install_granting_a_scope_the_app_does_not_ask_for_is_refused() {
    let h = harness().await;
    let Creation::Created { install_url, .. } = h.create().await else {
        panic!("created");
    };
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ok(json!({
            "app_id": "A0HELPER1",
            "token_type": "bot",
            "scope": "chat:write,admin",
            "access_token": AGENT_TOKEN,
            "bot_user_id": "U0HELPER1",
            "team": {"id": TEAM},
        })))
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let query = format!("code={CODE}&state={}", state_of(&install_url));
    let response = h.agents.callback(Some(&query)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let binding = h.binding().await.unwrap();
    assert!(h.store.bot_token(binding).await.unwrap().is_none());
}

#[tokio::test]
async fn an_install_that_finishes_after_the_agent_was_deleted_is_refused() {
    let h = harness().await;
    let Creation::Created { install_url, .. } = h.create().await else {
        panic!("created");
    };
    let (held, mut hold) = Held::new(app_installed());
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(held)
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let binding = h.binding().await.unwrap();
    let installing = {
        let agents = h.agents.clone();
        let query = format!("code={CODE}&state={}", state_of(&install_url));
        tokio::spawn(async move { agents.callback(Some(&query)).await.status() })
    };
    hold.arrived().await;
    deleted_bindings(&h).await;
    hold.release();
    assert_eq!(installing.await.unwrap(), StatusCode::CONFLICT);
    assert!(h.store.bot_token(binding).await.unwrap().is_none());
}

const OLD: &str = "G0PRIVAT1";
const NEW: &str = "C0PRIVAT1";
const NEWER: &str = "C0PRIVAT2";
const WRITER_TOKEN: &str = "xoxb-writer-SECRET";

fn room(id: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: TeamId::new(TEAM),
        conversation: id.into(),
    }
}

fn room_rule(id: &str) -> crate::policy::Rule {
    crate::policy::Rule::Room {
        conv: room(id),
        label: "#secret".to_owned(),
    }
}

/// `conversations.info` on `token` answering for `channel` with `answer`.
async fn channel_info(h: &Harness, token: &str, channel: &str, answer: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(wiremock::matchers::body_string_contains(format!(
            "channel={channel}"
        )))
        .respond_with(answer)
        .mount(&h.slack)
        .await;
}

fn member_of(channel: &str) -> ResponseTemplate {
    ok(
        json!({"channel": {"id": channel, "is_channel": true, "is_private": true, "is_member": true}}),
    )
}

fn refused(error: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": error}))
}

async fn installed_as(h: &Harness, name: &str, token: &str, version: u32) -> BindingId {
    let team = TeamId::new(TEAM);
    let now = OffsetDateTime::now_utc();
    let store::AgentCreation::Created(_, binding) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner: h.owner,
                name,
                persona: "You help.",
                visibility: Visibility::Public,
                surface: SurfaceKind::Slack,
                team: &team,
            },
            10,
            now,
        )
        .await
        .unwrap()
    else {
        panic!("created");
    };
    let app = AgentApp {
        name,
        public_url: "https://agentd.example.com",
        binding,
        public_posting: false,
    };
    let app_id = format!("A0{}", name.to_uppercase());
    assert!(
        h.store
            .set_slack_app(
                binding,
                &NewSlackApp {
                    app_id: app_id.clone(),
                    client_id: "3333.4444".to_owned(),
                    client_secret: SecretString::from("client-SECRET"),
                    signing_secret: SecretString::from("signing-SECRET"),
                    scopes: app.scopes().join(","),
                    redirect_url: app.redirect_url(),
                    manifest_version: version,
                },
                name,
                now,
            )
            .await
            .unwrap()
    );
    assert!(
        h.store
            .install_slack_app(
                binding,
                &app_id,
                &UserId::new(format!("U0{}", name.to_uppercase())),
                &SecretString::from(token),
                now,
            )
            .await
            .unwrap()
    );
    binding
}

async fn agent_of(h: &Harness, binding: BindingId) -> core_types::AgentId {
    h.store.binding(binding).await.unwrap().unwrap().agent
}

async fn set_rules(h: &Harness, binding: BindingId, rules: &Rules) {
    h.store
        .update_agent_settings(agent_of(h, binding).await, |settings| rules.write(settings))
        .await
        .unwrap();
}

async fn rules_of(h: &Harness, binding: BindingId) -> Rules {
    let settings = h
        .store
        .agent_settings(agent_of(h, binding).await)
        .await
        .unwrap();
    Rules::read(&settings).unwrap()
}

fn denying(ids: &[&str]) -> Rules {
    Rules {
        allow: Vec::new(),
        deny: ids.iter().map(|id| room_rule(id)).collect(),
    }
}

fn change_of(binding: BindingId, old: &str, new: &str) -> ChannelIdChange {
    ChannelIdChange {
        binding,
        old: old.into(),
        new: new.into(),
        received_at: OffsetDateTime::now_utc(),
    }
}

fn change(binding: BindingId) -> ChannelIdChange {
    change_of(binding, OLD, NEW)
}

/// Records `change` as the ingress's receiver does, without the task that
/// settles it at once.
async fn recorded(h: &Harness, change: &ChannelIdChange) {
    assert_eq!(
        h.store
            .record_channel_id_change(change, MAX_CHANNEL_CHANGES)
            .await
            .unwrap(),
        ChannelIdChangeRecord::Recorded
    );
}

/// `binding`'s recorded changes, each with whether it still waits.
async fn changes_of(h: &Harness, binding: BindingId) -> Vec<(String, String, bool)> {
    h.store
        .channel_id_changes_of_agent(agent_of(h, binding).await)
        .await
        .unwrap()
        .into_iter()
        .filter(|known| known.change.binding == binding)
        .map(|known| {
            (
                known.change.old.as_str().to_owned(),
                known.change.new.as_str().to_owned(),
                known.waiting,
            )
        })
        .collect()
}

fn the_event(binding: BindingId) -> ChannelIdChanged {
    ChannelIdChanged {
        binding,
        team: TeamId::new(TEAM),
        event_id: "Ev0CHANID1".to_owned(),
        old: OLD.into(),
        new: NEW.into(),
        received_at: OffsetDateTime::now_utc(),
    }
}

/// Waits until `binding`'s change from [`OLD`] to [`NEW`] is settled.
async fn settled(h: &Harness, binding: BindingId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while changes_of(h, binding).await != [(OLD.to_owned(), NEW.to_owned(), false)] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the change never settled"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether the router lets a member who isn't the owner use `binding`'s
/// agent in `channel`, by the agent's rules and its waiting changes, as
/// the router's view reads them.
async fn permits(h: &Harness, binding: BindingId, channel: &str) -> bool {
    let agent = agent_of(h, binding).await;
    let settings = h.store.agent_settings(agent).await.unwrap();
    let changes = h.store.channel_id_changes_of_agent(agent).await.unwrap();
    let pending = crate::policy::pending_denials(&changes);
    crate::policy::agent_policy(&settings, &crate::policy::Limits::default(), 0, &pending)
        .unwrap()
        .permits(
            &core_types::Requester {
                member: None,
                key: MemberKey {
                    user: UserId::new("U0BOB0001"),
                    ..ada()
                },
                outside: None,
            },
            &room(channel),
        )
}

#[tokio::test]
async fn only_the_receiving_agents_rules_are_rewritten() {
    let h = harness().await;
    let helper = installed(&h).await;
    let writer = installed_as(&h, "writer", WRITER_TOKEN, MANIFEST_VERSION).await;
    channel_info(&h, AGENT_TOKEN, NEW, member_of(NEW)).await;
    channel_info(&h, WRITER_TOKEN, NEW, member_of(NEW)).await;
    for binding in [helper, writer] {
        set_rules(&h, binding, &denying(&[OLD])).await;
    }
    h.agents.channel_id_changed(the_event(helper)).await;
    settled(&h, helper).await;
    assert_eq!(rules_of(&h, helper).await, denying(&[NEW]));
    assert_eq!(
        rules_of(&h, writer).await,
        denying(&[OLD]),
        "the other agent's event hasn't come"
    );
    assert!(changes_of(&h, writer).await.is_empty());
}

#[tokio::test]
async fn an_unconfirmed_new_channel_rewrites_nothing() {
    let h = harness().await;
    let helper = installed(&h).await;
    set_rules(&h, helper, &denying(&[OLD])).await;
    let answers = [
        ok(json!({"channel": {"id": NEW, "is_channel": true, "is_member": false}})),
        ok(json!({"channel": {"id": "c0privat1", "is_channel": true, "is_member": true}})),
        refused("channel_not_found"),
        refused("team_access_not_granted"),
        refused("missing_scope"),
        refused("invalid_auth"),
        ResponseTemplate::new(503),
        refused("ratelimited"),
    ];
    let changed = change(helper);
    recorded(&h, &changed).await;
    let mut at = OffsetDateTime::now_utc();
    for answer in answers {
        h.slack.reset().await;
        channel_info(&h, AGENT_TOKEN, NEW, answer).await;
        assert_eq!(
            h.agents
                .settle_channel_change(&changed, &|| at)
                .await
                .unwrap(),
            ChannelChange::Waiting
        );
        assert_eq!(h.calls("conversations.info").await, 1, "one ask each");
        assert_eq!(rules_of(&h, helper).await, denying(&[OLD]));
        assert_eq!(
            changes_of(&h, helper).await,
            [(OLD.to_owned(), NEW.to_owned(), true)],
            "it waits for its next try"
        );
        assert!(
            !permits(&h, helper, NEW).await,
            "the deny applies meanwhile"
        );
        at += CHANNEL_CHANGE_RETRY;
    }
}

#[tokio::test]
async fn a_channel_slack_hasnt_caught_up_with_settles_once_it_has() {
    let h = harness().await;
    let helper = installed(&h).await;
    set_rules(&h, helper, &denying(&[OLD])).await;
    channel_info(&h, AGENT_TOKEN, NEW, refused("channel_not_found")).await;
    h.agents.channel_id_changed(the_event(helper)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while h.calls("conversations.info").await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "never asked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let start = OffsetDateTime::now_utc();
    assert_eq!(h.agents.pass_at(|| start).await.settled, 0, "not due yet");
    assert_eq!(rules_of(&h, helper).await, denying(&[OLD]));

    h.slack.reset().await;
    channel_info(&h, AGENT_TOKEN, NEW, member_of(NEW)).await;
    let retry = start + CHANNEL_CHANGE_RETRY;
    assert_eq!(h.agents.pass_at(|| retry).await.settled, 1);
    assert_eq!(rules_of(&h, helper).await, denying(&[NEW]));
    assert!(!permits(&h, helper, NEW).await);
    assert!(permits(&h, helper, OLD).await, "the old id's deny moved");
}

#[tokio::test]
async fn a_chain_of_changes_lands_on_the_last_id_in_either_order() {
    let h = harness().await;
    let helper = installed(&h).await;
    channel_info(&h, AGENT_TOKEN, NEWER, member_of(NEWER)).await;
    channel_info(&h, AGENT_TOKEN, NEW, refused("channel_not_found")).await;
    let first = change_of(helper, OLD, NEW);
    let second = change_of(helper, NEW, NEWER);
    let now = OffsetDateTime::now_utc;
    for order in [[&first, &second], [&second, &first]] {
        set_rules(&h, helper, &denying(&[OLD])).await;
        for changed in [&first, &second] {
            h.store.delete_channel_id_change(changed).await.unwrap();
            recorded(&h, changed).await;
        }
        for changed in order {
            assert!(matches!(
                h.agents.settle_channel_change(changed, &now).await.unwrap(),
                ChannelChange::Moved { to, .. } if to.as_str() == NEWER
            ));
        }
        assert_eq!(rules_of(&h, helper).await, denying(&[NEWER]));
    }

    let redirected = installed_as(&h, "writer", WRITER_TOKEN, MANIFEST_VERSION).await;
    set_rules(&h, redirected, &denying(&[OLD])).await;
    channel_info(&h, WRITER_TOKEN, NEW, member_of(NEWER)).await;
    let changed = change_of(redirected, OLD, NEW);
    recorded(&h, &changed).await;
    assert_eq!(
        h.agents
            .settle_channel_change(&changed, &now)
            .await
            .unwrap(),
        ChannelChange::Moved {
            rules: true,
            to: NEWER.into()
        },
        "Slack names the id the channel has since"
    );
    assert_eq!(rules_of(&h, redirected).await, denying(&[NEWER]));
}

#[tokio::test]
async fn a_change_slack_never_confirms_is_given_up_with_its_denies_copied() {
    let h = harness().await;
    let helper = installed(&h).await;
    let rules = Rules {
        allow: vec![room_rule(OLD)],
        deny: vec![room_rule(OLD)],
    };
    set_rules(&h, helper, &rules).await;
    channel_info(&h, AGENT_TOKEN, NEW, ResponseTemplate::new(503)).await;
    let changed = change(helper);
    recorded(&h, &changed).await;
    let late = changed.received_at + CHANNEL_CHANGE_TTL + Duration::from_secs(1);
    assert_eq!(h.agents.pass_at(|| changed.received_at).await.settled, 0);
    let pass = h.agents.pass_at(|| late).await;
    assert_eq!((pass.given_up, pass.settled), (1, 0));
    assert_eq!(
        rules_of(&h, helper).await,
        Rules {
            allow: vec![room_rule(OLD)],
            deny: vec![room_rule(OLD), room_rule(NEW)],
        },
        "the deny applies to the new id for good, and the allow didn't move"
    );
    assert!(changes_of(&h, helper).await.is_empty());
    assert!(!permits(&h, helper, NEW).await);
    assert_eq!(h.calls("conversations.info").await, 1);

    let settled_long_ago = change_of(helper, "G0PRIVAT3", "C0PRIVAT3");
    recorded(&h, &settled_long_ago).await;
    h.store
        .settle_channel_id_change(&settled_long_ago, settled_long_ago.received_at)
        .await
        .unwrap();
    let later = settled_long_ago.received_at + CHANNEL_CHANGE_TTL + Duration::from_secs(1);
    assert_eq!(h.agents.pass_at(|| later).await.given_up, 0);
    assert!(changes_of(&h, helper).await.is_empty(), "purged");
}

#[tokio::test]
async fn a_replayed_channel_id_change_is_dropped() {
    let h = harness().await;
    let helper = installed(&h).await;
    set_rules(&h, helper, &denying(&[OLD])).await;
    channel_info(
        &h,
        AGENT_TOKEN,
        NEW,
        member_of(NEW).set_delay(Duration::from_millis(300)),
    )
    .await;
    let changed = change(helper);
    recorded(&h, &changed).await;
    let now = OffsetDateTime::now_utc;
    let (first, second) = tokio::join!(
        h.agents.settle_channel_change(&changed, &now),
        h.agents.settle_channel_change(&changed, &now)
    );
    let mut outcomes = [first.unwrap(), second.unwrap()];
    outcomes.sort_by_key(|outcome| *outcome == ChannelChange::Waiting);
    assert_eq!(
        outcomes,
        [
            ChannelChange::Moved {
                rules: true,
                to: NEW.into()
            },
            ChannelChange::Waiting
        ]
    );
    assert_eq!(
        h.store
            .record_channel_id_change(&changed, MAX_CHANNEL_CHANGES)
            .await
            .unwrap(),
        ChannelIdChangeRecord::Known,
        "a settled change is known"
    );
    h.agents.channel_id_changed(the_event(helper)).await;
    assert_eq!(h.agents.pass().await.settled, 0);
    assert_eq!(h.calls("conversations.info").await, 1);
    assert_eq!(rules_of(&h, helper).await, denying(&[NEW]));
}

#[tokio::test]
async fn a_full_binding_records_no_more_and_an_inactive_binding_settles_none() {
    let h = harness().await;
    let helper = installed(&h).await;
    for n in 0..MAX_CHANNEL_CHANGES {
        recorded(&h, &change_of(helper, OLD, &format!("C0FULL{n:03}"))).await;
    }
    h.agents.channel_id_changed(the_event(helper)).await;
    let recorded = changes_of(&h, helper).await;
    assert_eq!(recorded.len(), MAX_CHANNEL_CHANGES as usize);
    assert!(recorded.iter().all(|(_, new, _)| new != NEW));

    let bindings = deleted_bindings(&h).await;
    assert_eq!(bindings[0].id, helper);
    let pass = h.agents.pass().await;
    assert_eq!(
        pass.settled,
        recorded.len(),
        "an inactive binding's are dropped"
    );
    assert_eq!(h.calls("conversations.info").await, 0);
}

#[tokio::test]
async fn sessions_stay_under_the_old_id() {
    let h = harness().await;
    let helper = installed(&h).await;
    let agent = agent_of(&h, helper).await;
    set_rules(&h, helper, &denying(&[OLD])).await;
    channel_info(&h, AGENT_TOKEN, NEW, member_of(NEW)).await;
    let now = OffsetDateTime::now_utc();
    let thread = |id: &str| core_types::ThreadKey {
        conv: room(id),
        root: Some("1727698800.000100".into()),
    };
    let scope = |id: &str| core_types::ScopeKey::for_conversation(ConvKind::Channel, room(id));
    let old = h
        .store
        .session_for_thread(agent, &thread(OLD), &scope(OLD), now)
        .await
        .unwrap()
        .session;
    let requester = core_types::Requester {
        member: Some(h.owner),
        key: ada(),
        outside: None,
    };
    let msg = MsgRef {
        conv: room(OLD),
        id: "1727698800.000200".into(),
    };
    h.store
        .record_message_ref(
            &store::NewMessageRef {
                session: old.id,
                msg: &msg,
                thread_root: thread(OLD).root.as_ref(),
                agent: Some(agent),
                turn: None,
                requester: &requester,
                hop: core_types::Hop(0),
                consent: None,
                hands_off: true,
            },
            now,
        )
        .await
        .unwrap();
    h.store
        .record_turn_usage(
            h.owner,
            agent,
            &thread(OLD),
            store::TurnUsage {
                input_tokens: 10,
                output_tokens: 5,
                cost: Ok(0.01),
            },
            true,
            now,
        )
        .await
        .unwrap();
    let window = store::LimitWindow::Hour;
    assert!(
        h.store
            .claim_limit_notice(agent, &thread(OLD), "turns", window, now)
            .await
            .unwrap()
    );

    h.agents.channel_id_changed(the_event(helper)).await;
    settled(&h, helper).await;
    assert_eq!(rules_of(&h, helper).await, denying(&[NEW]));

    let again = h
        .store
        .session_for_thread(agent, &thread(OLD), &scope(OLD), now)
        .await
        .unwrap();
    assert_eq!((again.session.id, again.replaced), (old.id, None));
    let moved = h
        .store
        .session_for_thread(agent, &thread(NEW), &scope(NEW), now)
        .await
        .unwrap();
    assert_ne!(moved.session.id, old.id, "the new id starts afresh");
    assert_eq!(
        h.store
            .posted_message_ref(&msg)
            .await
            .unwrap()
            .map(|row| row.session),
        Some(old.id)
    );
    assert_eq!(
        h.store
            .thread_spend(&thread(OLD), now)
            .await
            .unwrap()
            .tokens_today,
        15
    );
    assert_eq!(
        h.store
            .thread_spend(&thread(NEW), now)
            .await
            .unwrap()
            .tokens_today,
        0
    );
    assert!(
        !h.store
            .claim_limit_notice(agent, &thread(OLD), "turns", window, now)
            .await
            .unwrap(),
        "the old id's notice stays claimed"
    );
}

/// An agent app's manifest as `apps.manifest.export` gives it: `binding`'s
/// app, with the bot events of an app made before `channel_id_changed`, as
/// its owner edited its description.
fn exported(binding: BindingId) -> Value {
    let mut manifest = agent_manifest(&AgentApp {
        name: "writer",
        public_url: "https://agentd.example.com",
        binding,
        public_posting: false,
    });
    manifest["settings"]["event_subscriptions"]["bot_events"] = json!([
        "message.channels",
        "message.groups",
        "message.im",
        "message.mpim"
    ]);
    manifest["display_information"]["description"] = json!("The owner's words.");
    manifest
}

async fn sent_manifests(h: &Harness) -> Vec<(String, Value)> {
    h.slack
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/api/apps.manifest.update")
        .map(|request| {
            let form: std::collections::HashMap<String, String> =
                serde_urlencoded::from_bytes(&request.body).unwrap();
            (
                form["app_id"].clone(),
                serde_json::from_str(&form["manifest"]).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn older_agent_apps_get_the_event_through_a_manifest_update() {
    let h = harness().await;
    let current = installed(&h).await;
    let old = installed_as(&h, "writer", WRITER_TOKEN, 0).await;
    let manifest = exported(old);
    mount(
        &h.slack,
        "apps.manifest.export",
        CONFIG_TOKEN,
        ok(json!({"manifest": manifest})),
    )
    .await;
    mount(
        &h.slack,
        "apps.manifest.update",
        CONFIG_TOKEN,
        ok(json!({"app_id": "A0WRITER", "permissions_updated": false})),
    )
    .await;
    assert_eq!(h.agents.pass().await.updated, 1);
    let mut expected = manifest;
    expected["settings"]["event_subscriptions"]["bot_events"]
        .as_array_mut()
        .unwrap()
        .push(json!("channel_id_changed"));
    assert_eq!(
        sent_manifests(&h).await,
        [("A0WRITER".to_owned(), expected)],
        "only the older app, with only the event added"
    );
    assert!(
        h.store
            .outdated_slack_apps(h.owner, &TeamId::new(TEAM), MANIFEST_VERSION)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(h.agents.pass().await.updated, 0);
    assert_eq!(h.calls("apps.manifest.export").await, 1);
    assert_ne!(current, old);

    let h = harness().await;
    let has_it = installed_as(&h, "writer", WRITER_TOKEN, 0).await;
    let manifest = agent_manifest(&AgentApp {
        name: "writer",
        public_url: "https://agentd.example.com",
        binding: has_it,
        public_posting: false,
    });
    mount(
        &h.slack,
        "apps.manifest.export",
        CONFIG_TOKEN,
        ok(json!({"manifest": manifest})),
    )
    .await;
    assert_eq!(h.agents.pass().await.updated, 1);
    assert_eq!(
        h.calls("apps.manifest.update").await,
        0,
        "an app with every event already is only recorded"
    );
}

#[tokio::test]
async fn a_manifest_update_that_cant_succeed_stops_and_a_refused_token_breaks() {
    let h = harness().await;
    let gone = installed_as(&h, "gone", WRITER_TOKEN, 0).await;
    let eventless = installed_as(&h, "eventless", "xoxb-eventless", 0).await;
    let refusing = installed_as(&h, "refusing", "xoxb-refusing", 0).await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.export"))
        .and(wiremock::matchers::body_string_contains("app_id=A0GONE"))
        .respond_with(refused("app_not_found"))
        .mount(&h.slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.export"))
        .and(wiremock::matchers::body_string_contains(
            "app_id=A0EVENTLESS",
        ))
        .respond_with(ok(json!({"manifest": {"settings": {}}})))
        .mount(&h.slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/apps.manifest.export"))
        .and(wiremock::matchers::body_string_contains(
            "app_id=A0REFUSING",
        ))
        .respond_with(refused("invalid_auth"))
        .mount(&h.slack)
        .await;
    let start = OffsetDateTime::now_utc();
    assert_eq!(h.agents.pass_at(|| start).await.updated, 0);
    assert_eq!(h.calls("apps.manifest.export").await, 3);
    let status = h
        .store
        .slack_config_token_status(h.owner, &TeamId::new(TEAM))
        .await
        .unwrap()
        .unwrap();
    assert!(status.broken, "Slack refused the token");
    let outdated = h
        .store
        .outdated_slack_apps(h.owner, &TeamId::new(TEAM), MANIFEST_VERSION)
        .await
        .unwrap();
    let blocked: Vec<_> = outdated
        .iter()
        .map(|app| (app.agent_name.as_str(), app.blocked))
        .collect();
    assert_eq!(
        blocked,
        [("eventless", true), ("gone", true), ("refusing", false)]
    );

    register_token(&h, start).await;
    let later = start + MANIFEST_UPDATE_LEASE + Duration::from_secs(1);
    assert_eq!(h.agents.pass_at(|| later).await.updated, 0);
    assert_eq!(
        h.calls("apps.manifest.export").await,
        4,
        "only the app whose token was refused is tried again"
    );
    assert_eq!(h.calls("apps.manifest.update").await, 0);
    let _ = (gone, eventless, refusing);
}

#[tokio::test]
async fn a_token_that_expires_before_a_call_could_end_is_not_used() {
    let h = harness().await;
    installed_as(&h, "writer", WRITER_TOKEN, 0).await;
    let start = OffsetDateTime::now_utc();
    let expiry = h
        .store
        .slack_config_token_status(h.owner, &TeamId::new(TEAM))
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    let close = expiry - APP_CALL_TIMEOUT + Duration::from_secs(1);
    assert!(close > start);
    assert_eq!(h.agents.pass_at(|| close).await.updated, 0);
    assert_eq!(h.calls("apps.manifest.export").await, 0);
    assert!(
        !h.store
            .slack_config_token_status(h.owner, &TeamId::new(TEAM))
            .await
            .unwrap()
            .unwrap()
            .broken
    );
}

#[tokio::test]
async fn a_pass_slack_holds_up_starts_no_call_past_its_budget() {
    let h = harness().await;
    for name in ["first", "second"] {
        installed_as(&h, name, &format!("xoxb-{name}"), 0).await;
    }
    mount(
        &h.slack,
        "apps.manifest.export",
        CONFIG_TOKEN,
        refused("internal_error").set_delay(Duration::from_millis(400)),
    )
    .await;
    let pass = h
        .agents
        .pass_within(OffsetDateTime::now_utc, Duration::from_millis(100))
        .await;
    assert_eq!(pass.updated, 0);
    assert_eq!(
        h.calls("apps.manifest.export").await,
        1,
        "the second update waits for the next pass"
    );
}

/// Registers a new configuration token for the owner at `at`, good for
/// three hours.
async fn register_token(h: &Harness, at: OffsetDateTime) {
    h.store
        .put_slack_config_token(
            h.owner,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from(CONFIG_TOKEN),
                refresh_token: SecretString::from("refresh-SECRET"),
                expires_at: at + Duration::from_secs(3 * 3600),
            },
            at,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_busy_day_of_shares_records_every_change() {
    let h = harness().await;
    let helper = installed(&h).await;
    let count = 20;
    let olds: Vec<String> = (0..count).map(|n| format!("G0BUSY{n:03}")).collect();
    let news: Vec<String> = (0..count).map(|n| format!("C0BUSY{n:03}")).collect();
    set_rules(&h, helper, &denying(&ids(&olds))).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .and(header(
            "authorization",
            format!("Bearer {AGENT_TOKEN}").as_str(),
        ))
        .respond_with(|request: &wiremock::Request| {
            let form: std::collections::HashMap<String, String> =
                serde_urlencoded::from_bytes(&request.body).unwrap_or_default();
            member_of(form.get("channel").map_or("", String::as_str))
        })
        .mount(&h.slack)
        .await;
    for (old, new) in olds.iter().zip(&news) {
        h.agents
            .channel_id_changed(ChannelIdChanged {
                old: old.as_str().into(),
                new: new.as_str().into(),
                ..the_event(helper)
            })
            .await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rules_of(&h, helper).await != denying(&ids(&news)) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not every change moved its deny: {:?}",
            changes_of(&h, helper).await
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(changes_of(&h, helper).await.len(), count);
}

fn ids(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
}

#[tokio::test]
async fn a_change_the_binding_has_no_room_for_copies_its_denies_at_once() {
    let h = harness().await;
    let helper = installed(&h).await;
    let room_for = MAX_CHANNEL_CHANGES as usize;
    let count = room_for + 3;
    let olds: Vec<String> = (0..count).map(|n| format!("G0FULL{n:03}")).collect();
    let news: Vec<String> = (0..count).map(|n| format!("C0FULL{n:03}")).collect();
    set_rules(&h, helper, &denying(&ids(&olds))).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.slack)
        .await;
    for (old, new) in olds.iter().zip(&news) {
        h.agents
            .channel_id_changed(ChannelIdChanged {
                old: old.as_str().into(),
                new: new.as_str().into(),
                ..the_event(helper)
            })
            .await;
    }
    let waiting = changes_of(&h, helper).await;
    assert_eq!(waiting.len(), room_for);
    assert!(waiting.iter().all(|(_, _, waiting)| *waiting));
    let mut expected = ids(&olds);
    expected.extend(ids(&news[room_for..]));
    assert_eq!(
        rules_of(&h, helper).await,
        denying(&expected),
        "the denies of the changes without room apply to their new ids for good"
    );
    for new in &news {
        assert!(!permits(&h, helper, new).await, "{new}");
    }
}

#[tokio::test]
async fn a_token_too_close_to_expiry_by_the_time_its_update_comes_burns_no_lease() {
    let h = harness().await;
    installed_as(&h, "writer", WRITER_TOKEN, 0).await;
    let team = TeamId::new(TEAM);
    let start = OffsetDateTime::now_utc();
    let due = h
        .store
        .due_manifest_updates(&team, MANIFEST_VERSION, start, start + APP_CALL_TIMEOUT, 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let expiry = h
        .store
        .slack_config_token_status(h.owner, &team)
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    let close = expiry - APP_CALL_TIMEOUT + Duration::from_secs(1);
    assert!(!h.agents.update_manifest(&due[0], &|| close).await.unwrap());
    assert_eq!(h.calls("apps.manifest.export").await, 0);
    assert_eq!(
        h.store
            .due_manifest_updates(&team, MANIFEST_VERSION, start, start + APP_CALL_TIMEOUT, 10)
            .await
            .unwrap(),
        due,
        "not leased"
    );
}

#[tokio::test]
async fn a_flood_of_changes_a_full_binding_has_no_room_for_keeps_the_deny_list_bounded() {
    let h = harness().await;
    let helper = installed(&h).await;
    for n in 0..MAX_CHANNEL_CHANGES {
        recorded(
            &h,
            &change_of(helper, &format!("G0WAIT{n:03}"), &format!("C0WAIT{n:03}")),
        )
        .await;
    }
    set_rules(&h, helper, &denying(&[OLD])).await;
    let flood = crate::policy::MAX_DENIES + 50;
    for n in 0..flood {
        h.agents
            .channel_id_changed(ChannelIdChanged {
                event_id: format!("Ev0FLOOD{n:04}"),
                new: format!("C0FLOOD{n:04}").as_str().into(),
                ..the_event(helper)
            })
            .await;
    }
    let rules = rules_of(&h, helper).await;
    assert!(rules.denies_everyone(), "the flood ends in deny everyone");
    assert_eq!(rules.deny.len(), crate::policy::MAX_DENIES + 1);
    assert!(!permits(&h, helper, &format!("C0FLOOD{:04}", flood - 1)).await);
    assert_eq!(
        changes_of(&h, helper).await.len(),
        MAX_CHANNEL_CHANGES as usize
    );
}

#[tokio::test]
async fn a_change_with_no_room_carries_the_denies_a_waiting_change_brings_to_its_old_id() {
    let h = harness().await;
    let helper = installed(&h).await;
    recorded(&h, &change_of(helper, OLD, NEW)).await;
    for n in 1..MAX_CHANNEL_CHANGES {
        recorded(
            &h,
            &change_of(helper, &format!("G0WAIT{n:03}"), &format!("C0WAIT{n:03}")),
        )
        .await;
    }
    set_rules(&h, helper, &denying(&[OLD])).await;
    h.agents
        .channel_id_changed(ChannelIdChanged {
            old: NEW.into(),
            new: "C0LATER01".into(),
            ..the_event(helper)
        })
        .await;
    assert_eq!(
        rules_of(&h, helper).await,
        denying(&[OLD, "C0LATER01"]),
        "the deny on the waiting change's old id reaches the id after its new one"
    );
    assert!(!permits(&h, helper, "C0LATER01").await);
}
