//! [`RocketChatAgents`], the [`Supervisor`] and [`Acknowledge`] against
//! `testkit`'s fake Rocket.Chat and an in-memory store.

use std::sync::Arc;
use std::time::Duration;

use core_types::{
    BindingId, ConvKind, ConvRef, InboundEvent, MemberId, MemberKey, MsgRef, Sender, Sink,
    SurfaceKind, TeamId, UserId,
};
use secrecy::SecretString;
use store::{AgentState, BindingState, NewAgent, Sealer, Store, Visibility};
use surface_rocketchat::rest::{Credentials, RestClient};
use surface_rocketchat::{BotRoles, RocketChatConfig};
use testkit::rocketchat::{FakeDdp, FakeRest};
use time::OffsetDateTime;
use tokio::sync::watch;

use super::*;
use crate::commands::rocketchat::{CommandIntake, StoreDedup};
use crate::commands::{Commands, Replies};

const TEAM: &str = "chat.example";

fn manager_credentials() -> Credentials {
    Credentials {
        user_id: FakeRest::MANAGER_ID.into(),
        token: SecretString::from(FakeRest::MANAGER_TOKEN),
    }
}

struct Harness {
    fake: FakeRest,
    store: Store,
    agents: RocketChatAgents,
    owner: MemberId,
}

async fn harness() -> Harness {
    let fake = FakeRest::start().await;
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let rest = RestClient::new(&fake.uri(), manager_credentials()).unwrap();
    let agents = RocketChatAgents::new(store.clone(), rest, TEAM.into(), None);
    let owner = store
        .ensure_member(&key("owner"), "owner", OffsetDateTime::now_utc())
        .await
        .unwrap();
    Harness {
        fake,
        store,
        agents,
        owner,
    }
}

fn key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        user: user.into(),
    }
}

impl Harness {
    /// Stores a new agent `name` created at `at`, and returns its binding.
    async fn creating(&self, name: &str, at: OffsetDateTime) -> BindingId {
        let team = TeamId::new(TEAM);
        let new = NewAgent {
            owner: self.owner,
            name,
            persona: "p",
            visibility: Visibility::Public,
            surface: SurfaceKind::RocketChat,
            team: &team,
        };
        self.store.create_agent(&new, at).await.unwrap().unwrap().1
    }

    /// An agent `name` whose bot exists and is active; returns its binding
    /// and bot user id.
    async fn created(&self, name: &str) -> (BindingId, UserId) {
        let binding = self.creating(name, OffsetDateTime::now_utc()).await;
        let bot = self
            .agents
            .create_bot(binding, name, "owner")
            .await
            .unwrap();
        (binding, bot.user)
    }

    async fn state(&self, binding: BindingId) -> (BindingState, AgentState) {
        let row = self.store.binding(binding).await.unwrap().unwrap();
        let agent = self.store.agent(row.agent).await.unwrap().unwrap();
        (row.state, agent.state)
    }
}

#[tokio::test]
async fn a_failure_after_the_bot_exists_abandons_and_retires_it() {
    let h = harness().await;
    h.fake
        .fail(
            "users.generatePersonalAccessToken",
            400,
            "error-not-allowed",
            None,
        )
        .await;
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let err = h
        .agents
        .create_bot(binding, "helper", "owner")
        .await
        .unwrap_err();
    assert!(matches!(err, CreateError::Surface(_)), "{err:?}");
    assert_eq!(
        h.state(binding).await,
        (BindingState::Disabled, AgentState::Deleted)
    );
    assert!(!h.fake.user("helper").unwrap().active);
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert!(row.retired_at.is_some());
}

#[tokio::test]
async fn a_creation_abandoned_meanwhile_deactivates_its_new_bot() {
    let h = harness().await;
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let now = OffsetDateTime::now_utc();
    assert!(h.store.abandon_creation(binding, now, now).await.unwrap());
    let err = h
        .agents
        .create_bot(binding, "helper", "owner")
        .await
        .unwrap_err();
    assert!(matches!(err, CreateError::Abandoned), "{err:?}");
    assert!(!h.fake.user("helper").unwrap().active);
}

#[tokio::test]
async fn a_new_bot_sets_the_configured_avatar_as_itself() {
    let h = harness().await;
    let agents = RocketChatAgents::new(
        h.store.clone(),
        h.agents.rest().clone(),
        TEAM.into(),
        Some("https://img.example/bot.png".into()),
    );
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let bot = agents.create_bot(binding, "helper", "owner").await.unwrap();
    let user = h.fake.user("helper").unwrap();
    assert_eq!(
        user.avatar_url.as_deref(),
        Some("https://img.example/bot.png")
    );
    let requests = h.fake.requests("users.setAvatar").await;
    let sender = requests[0]
        .headers
        .get("x-user-id")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(sender, bot.user.as_str());
}

#[tokio::test]
async fn stale_creations_are_abandoned_and_their_bots_retired() {
    let h = harness().await;
    let old = OffsetDateTime::now_utc() - CREATION_LEASE - time::Duration::seconds(5);
    let stale = h.creating("stale", old).await;
    let bot = h.fake.add_user("stale-bot");
    assert!(
        h.store
            .set_binding_bot_user(stale, &UserId::new(bot.as_str()), "stale-bot")
            .await
            .unwrap()
    );
    let fresh = h.creating("fresh", OffsetDateTime::now_utc()).await;

    assert_eq!(h.agents.abandon_stale().await.unwrap(), 1);
    assert_eq!(
        h.state(stale).await,
        (BindingState::Disabled, AgentState::Deleted)
    );
    assert_eq!(
        h.state(fresh).await,
        (BindingState::Creating, AgentState::Active)
    );
    assert_eq!(h.agents.retire_pending().await.unwrap(), 1);
    assert!(!h.fake.user("stale-bot").unwrap().active);
    assert_eq!(h.agents.retire_pending().await.unwrap(), 0);
}

#[tokio::test]
async fn a_refused_retirement_waits_for_its_backoff() {
    let h = harness().await;
    let (binding, _) = h.created("helper").await;
    let agent = h.store.binding(binding).await.unwrap().unwrap().agent;
    assert!(
        h.store
            .delete_agent(agent, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    h.fake
        .fail("users.setActiveStatus", 403, "unauthorized", None)
        .await;
    assert!(!h.agents.retire(binding).await.unwrap());
    assert_eq!(h.agents.retire_pending().await.unwrap(), 0, "backing off");
    let pending = h
        .store
        .pending_retirements(
            SurfaceKind::RocketChat,
            &TEAM.into(),
            OffsetDateTime::now_utc() + RETIRE_BACKOFF_INITIAL,
            RETIRE_MAX_ATTEMPTS,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "due again after the backoff");
    assert!(h.fake.user("helper").unwrap().active);
}

#[tokio::test]
async fn a_bot_user_that_is_gone_counts_as_retired() {
    let h = harness().await;
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    assert!(
        h.store
            .set_binding_bot_user(binding, &UserId::new("gone"), "helper")
            .await
            .unwrap()
    );
    let now = OffsetDateTime::now_utc();
    assert!(h.store.abandon_creation(binding, now, now).await.unwrap());
    assert!(h.agents.retire(binding).await.unwrap());
    assert!(!h.agents.retire(binding).await.unwrap(), "nothing left");
}

#[test]
fn the_backoff_doubles_up_to_its_cap() {
    assert_eq!(backoff(1), RETIRE_BACKOFF_INITIAL);
    assert_eq!(backoff(2), RETIRE_BACKOFF_INITIAL * 2);
    assert_eq!(backoff(40), RETIRE_BACKOFF_MAX);
}

#[tokio::test]
async fn usernames_rooms_and_files_go_through_the_manager() {
    let h = harness().await;
    let alice = h.fake.add_user("alice");
    assert_eq!(
        h.agents
            .username(&UserId::new(alice.as_str()))
            .await
            .unwrap(),
        "alice"
    );
    assert_eq!(
        h.agents.user_named("alice").await.unwrap(),
        Some(UserId::new(alice.as_str()))
    );
    assert_eq!(h.agents.user_named("nobody").await.unwrap(), None);
    h.fake.add_room("DM", "d", "");
    let err = h
        .agents
        .invite(&"DM".into(), &UserId::new(alice.as_str()))
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::Unsupported(_)), "{err:?}");
    h.fake.add_room("PRIVATE", "p", "private");
    h.agents
        .invite(&"PRIVATE".into(), &UserId::new(alice.as_str()))
        .await
        .unwrap();
    assert!(h.fake.members("PRIVATE").contains(&alice));
    let file = h.fake.add_file("persona.md", b"hello");
    let data = h.agents.download(&file, "persona.md", 10).await.unwrap();
    assert_eq!(&data[..], b"hello");
}

fn event(
    sender: &str,
    binding: BindingId,
    kind: ConvKind,
    id: &str,
    mentions: &[&str],
) -> InboundEvent {
    let conv = ConvRef {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        conversation: "ROOM".into(),
    };
    InboundEvent {
        event_id: id.to_owned(),
        binding,
        sender: key(sender),
        sender_is_bot: false,
        sender_bot_user: None,
        conv: conv.clone(),
        conv_kind: kind,
        thread_root: None,
        message: MsgRef {
            conv,
            id: id.into(),
        },
        text: "hi".to_owned(),
        mentions: mentions.iter().map(|m| UserId::new(*m)).collect(),
        reply_to: None,
        files: Vec::new(),
        received_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn a_dm_with_an_agent_is_acknowledged_by_that_agent_only() {
    let h = harness().await;
    let (helper_binding, helper) = h.created("helper").await;
    let (_, writer) = h.created("writer").await;
    let alice = h.fake.add_user("alice");
    h.fake.add_room("ROOM", "d", "");
    h.fake.add_member("ROOM", helper.as_str());
    h.fake.add_member("ROOM", &alice);
    let ack = Acknowledge::new(h.agents.clone(), key(FakeRest::MANAGER_ID));

    let dm = h.fake.seed_message("ROOM", &alice, "hi", None);
    ack.send(event(
        &alice,
        helper_binding,
        ConvKind::Dm,
        &dm,
        &[helper.as_str()],
    ))
    .await
    .unwrap();
    let reactions = h.fake.message(&dm).unwrap().reactions;
    assert_eq!(reactions, [(":eyes:".to_owned(), helper.to_string())]);

    let from_manager = h
        .fake
        .seed_message("ROOM", FakeRest::MANAGER_ID, "hi", None);
    ack.send(event(
        FakeRest::MANAGER_ID,
        helper_binding,
        ConvKind::Dm,
        &from_manager,
        &[writer.as_str()],
    ))
    .await
    .unwrap();
    assert!(h.fake.message(&from_manager).unwrap().reactions.is_empty());

    let from_agent = h.fake.seed_message("ROOM", writer.as_str(), "hi", None);
    ack.send(event(
        writer.as_str(),
        helper_binding,
        ConvKind::Dm,
        &from_agent,
        &[],
    ))
    .await
    .unwrap();
    assert!(h.fake.message(&from_agent).unwrap().reactions.is_empty());
}

async fn wait_for_logins(ddp: &FakeDdp, user: &str, count: usize) {
    ddp.wait_for_logins(user, count).await;
}

#[tokio::test]
async fn the_supervisor_follows_the_store_and_restarts_ended_connections() {
    let h = harness().await;
    let ddp = FakeDdp::start().await;
    ddp.accept_tokens_of(&h.fake);
    let (binding, helper) = h.created("helper").await;

    let auth = Arc::new(auth::Auth::new(auth::OAuthConfig::default(), h.store.clone()).unwrap());
    let commands = Commands::new(h.store.clone(), auth, Replies::default(), None);
    let manager = core_types::Binding {
        id: BindingId::new_v4(),
        agent: None,
        bot: key(FakeRest::MANAGER_ID),
    };
    let (intake, feed) = CommandIntake::new(commands, manager);
    let mut template = RocketChatConfig::new(h.fake.uri(), TEAM.into(), manager_credentials());
    template.websocket_url = Some(ddp.url());
    let bots = BotRoles::new(h.agents.rest().clone());
    let supervisor = Supervisor::new(
        h.agents.clone(),
        template,
        Arc::new(StoreDedup(h.store.clone())),
        bots,
        feed,
        None::<Sender<InboundEvent>>,
    )
    .every(Duration::from_millis(100));
    let (stop, stopping) = watch::channel(false);
    let intake = tokio::spawn(intake.run());
    let supervising = tokio::spawn(supervisor.run(stopping));

    wait_for_logins(&ddp, helper.as_str(), 1).await;
    h.agents.rest().set_active(&helper, false).await.unwrap();
    ddp.drop_connections();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ddp.logins().iter().any(|l| l.user.is_none()) {
        assert!(tokio::time::Instant::now() < deadline, "no refused login");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.agents.rest().set_active(&helper, true).await.unwrap();
    wait_for_logins(&ddp, helper.as_str(), 2).await;

    let agent = h.store.binding(binding).await.unwrap().unwrap().agent;
    assert!(
        h.store
            .delete_agent(agent, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    h.agents.poke();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while ddp.connections() > 0 {
        assert!(tokio::time::Instant::now() < deadline, "still connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let retired = h.store.binding(binding).await.unwrap().unwrap();
    assert!(retired.retired_at.is_some(), "the pass retired the bot");

    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), supervising)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), intake)
        .await
        .unwrap()
        .unwrap();
}
