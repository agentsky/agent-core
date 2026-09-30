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
use surface_slack::{SlackClient, SlackInbound};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::commands::intake::CommandIntake;
use crate::commands::{Commands, Replies};
use crate::slack::manager::ManagerIdentity;
use crate::slack::{Inbound, Messages};
use crate::telemetry::tests::Captured;
use crate::telemetry::{LogFormat, subscriber};

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
    let captured = Captured::default();
    let logs = subscriber(
        LogFormat::Json,
        tracing_subscriber::EnvFilter::new("trace"),
        captured.clone(),
    );
    let _guard = tracing::subscriber::set_default(logs);

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
        h.agents.delete_apps(h.owner, &bindings).await.unwrap(),
        [AppDeletion::Deleted]
    );

    let text = captured.text();
    assert!(text.contains("installed an agent's Slack app"), "{text}");
    for secret in SECRETS {
        assert!(!text.contains(secret), "{secret} was logged");
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
    let auth = Arc::new(auth::Auth::new(auth::OAuthConfig::default(), h.store.clone()).unwrap());
    let commands = Commands::new(h.store.clone(), auth, Replies::default(), None, None);
    let (_intake, submitter) = CommandIntake::new(commands);
    let messages = Messages::default();
    let inbound = Inbound::new(
        h.store.clone(),
        Some(h.agents.inner.manager.identity().clone()),
        submitter,
    )
    .with_agents(h.agents.bots().clone(), messages.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    messages.connect(Sender::new(Collect(tx)));

    inbound
        .send(SlackInbound::Message(Box::new(bot_message(binding, TEAM))))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err(), "not installed yet: dropped");

    let query = format!("code={CODE}&state={}", state_of(&install_url));
    assert_eq!(
        h.agents.callback(Some(&query)).await.status(),
        StatusCode::OK
    );
    inbound
        .send(SlackInbound::Message(Box::new(bot_message(
            binding,
            "T0OTHER01",
        ))))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err(), "another workspace: dropped");
    inbound
        .send(SlackInbound::Message(Box::new(bot_message(
            BindingId::new_v4(),
            TEAM,
        ))))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err(), "an unknown binding: dropped");

    inbound
        .send(SlackInbound::Message(Box::new(bot_message(binding, TEAM))))
        .await
        .unwrap();
    let event = rx.try_recv().expect("handed on");
    assert_eq!(event.sender.user, UserId::new("U0OTHERBOT"));
    assert_eq!(event.sender_bot_user, Some(UserId::new("U0OTHERBOT")));
}
