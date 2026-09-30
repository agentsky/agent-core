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
use time::OffsetDateTime;
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::commands::intake::CommandIntake;
use crate::commands::{Commands, Replies};
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
    ] {
        mount(&slack, name, MANAGER_TOKEN, ok(body)).await;
    }
    mount(
        &slack,
        "apps.manifest.create",
        CONFIG_TOKEN,
        ok(json!({
            "app_id": "A0HELPER1",
            "credentials": {
                "client_id": "1111.2222",
                "client_secret": CLIENT_SECRET,
                "signing_secret": SIGNING_SECRET,
            },
        }))
        .set_delay(Duration::from_millis(300)),
    )
    .await;
    mount(&slack, "apps.manifest.delete", CONFIG_TOKEN, ok(json!({}))).await;
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("1111.2222:{CLIENT_SECRET}"))
    );
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(header("authorization", basic.as_str()))
        .respond_with(ok(json!({
            "app_id": "A0HELPER1",
            "token_type": "bot",
            "access_token": AGENT_TOKEN,
            "bot_user_id": "U0HELPER1",
            "team": {"id": TEAM},
        })))
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
        },
    );
    let bots = SlackBots::new(
        store.clone(),
        client,
        Arc::clone(manager.surface().directory()),
    );
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
    let binding = loop {
        if let Some(binding) = h.binding().await {
            break binding;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let now = OffsetDateTime::now_utc();
    assert!(h.store.abandon_creation(binding, now, now).await.unwrap());
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
    let pass = h.agents.pass().await.unwrap();
    assert_eq!(
        pass,
        SweepPass {
            abandoned: 1,
            reminded: 0
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
    let commands = Commands::new(h.store.clone(), auth, Replies::default(), None, None, skills);
    let (_intake, submitter) = CommandIntake::new(commands);
    let (messages, worker) = Messages::new(h.agents.bots().clone());
    let inbound = Inbound::new(
        h.store.clone(),
        Some(h.agents.inner.manager.identity().clone()),
        submitter,
    )
    .with_agents(messages.clone());
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
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(
            ok(json!({
                "app_id": "A0HELPER1",
                "token_type": "bot",
                "access_token": AGENT_TOKEN,
                "bot_user_id": "U0HELPER1",
                "team": {"id": TEAM},
            }))
            .set_delay(Duration::from_millis(500)),
        )
        .with_priority(1)
        .mount(&h.slack)
        .await;
    let binding = h.binding().await.unwrap();
    let installing = {
        let agents = h.agents.clone();
        let query = format!("code={CODE}&state={}", state_of(&install_url));
        tokio::spawn(async move { agents.callback(Some(&query)).await.status() })
    };
    while h.calls("oauth.v2.access").await == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    deleted_bindings(&h).await;
    assert_eq!(installing.await.unwrap(), StatusCode::CONFLICT);
    assert!(h.store.bot_token(binding).await.unwrap().is_none());
}
