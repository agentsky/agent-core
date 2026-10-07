//! [`RocketChatAgents`], the [`Supervisor`] and [`Acknowledge`] against
//! `testkit`'s fake Rocket.Chat and an in-memory store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use core_types::{
    BindingId, ConvKind, ConvRef, InboundEvent, MemberId, MemberKey, MsgRef, SendError, Sender,
    Sink, SurfaceKind, TeamId, UserId,
};
use secrecy::SecretString;
use serde_json::json;
use store::{AgentCreation, AgentState, BindingState, NewAgent, Sealer, Store, Visibility};
use surface_rocketchat::rest::{Credentials, NewBotUser, RestClient};
use surface_rocketchat::{BotRoles, RocketChatConfig};
use testkit::Held;
use testkit::rocketchat::{FakeDdp, FakeRest, realtime_message};
use time::OffsetDateTime;
use tokio::sync::watch;
use wiremock::matchers::path;
use wiremock::{Mock, ResponseTemplate};

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

async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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
        match self.store.create_agent(&new, 100, at).await.unwrap() {
            AgentCreation::Created(_, binding) => binding,
            other => panic!("couldn't store {name}: {other:?}"),
        }
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
async fn a_creation_abandoned_before_it_starts_makes_no_bot() {
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
    assert_eq!(h.fake.user("helper"), None);
    assert!(h.fake.requests("users.create").await.is_empty());
}

#[tokio::test]
async fn a_bot_made_after_its_creation_was_abandoned_owes_retirement() {
    let h = harness().await;
    let made = h.fake.add_user("helper");
    let (held, mut hold) =
        Held::new(ResponseTemplate::new(200).set_body_json(
            json!({ "success": true, "user": { "_id": made, "username": "helper" } }),
        ));
    Mock::given(path("/api/v1/users.create"))
        .respond_with(held)
        .up_to_n_times(1)
        .with_priority(1)
        .mount(h.fake.server())
        .await;
    Mock::given(path("/api/v1/users.setActiveStatus"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(h.fake.server())
        .await;
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let agents = h.agents.clone();
    let creating = tokio::spawn(async move { agents.create_bot(binding, "helper", "owner").await });
    hold.arrived().await;
    let now = OffsetDateTime::now_utc();
    assert!(h.store.abandon_creation(binding, now, now).await.unwrap());
    hold.release();
    let err = creating.await.unwrap().unwrap_err();
    assert!(matches!(err, CreateError::Abandoned), "{err:?}");

    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.bot_user, Some(UserId::new(made.as_str())));
    assert!(
        h.fake.user("helper").unwrap().active,
        "the first attempt failed"
    );
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
    assert_eq!(pending.len(), 1, "retried after the backoff");
}

#[tokio::test]
async fn a_store_failure_after_users_create_deactivates_the_new_bot() {
    let h = harness().await;
    let probe = h.fake.add_user("probe");
    let sequence: u64 = probe.trim_start_matches("user-").parse().unwrap();
    let next = UserId::new(format!("user-{}", sequence + 1));
    let squatter = h.creating("squatter", OffsetDateTime::now_utc()).await;
    assert_eq!(
        h.store
            .set_binding_bot_user(squatter, &next, "squatter")
            .await
            .unwrap(),
        Some(true)
    );
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let err = h
        .agents
        .create_bot(binding, "helper", "owner")
        .await
        .unwrap_err();
    assert!(matches!(err, CreateError::Store(_)), "{err:?}");
    let bot = h.fake.user("helper").unwrap();
    assert_eq!(bot.id, next.as_str());
    assert!(!bot.active);
    assert_eq!(
        h.state(binding).await,
        (BindingState::Disabled, AgentState::Deleted)
    );
}

#[tokio::test]
async fn an_abandoned_creations_unrecorded_bot_is_found_by_its_email_and_retired() {
    let h = harness().await;
    let old = OffsetDateTime::now_utc() - CREATION_LEASE - time::Duration::seconds(5);
    let binding = h.creating("helper", old).await;
    assert!(
        h.store
            .set_binding_bot_username(binding, "helper")
            .await
            .unwrap()
    );
    let email = bot_email(binding);
    let (made, _) = h
        .agents
        .rest()
        .create_bot_user(&NewBotUser {
            username: "helper",
            name: "helper",
            email: &email,
        })
        .await
        .unwrap();
    let other = h.creating("writer", old).await;
    h.fake.add_user("writer");
    assert!(
        h.store
            .set_binding_bot_username(other, "writer")
            .await
            .unwrap()
    );

    assert_eq!(h.agents.abandon_stale().await.unwrap(), 2);
    assert_eq!(h.agents.retire_pending().await.unwrap(), 1);
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.bot_user, Some(made.id));
    let other_row = h.store.binding(other).await.unwrap().unwrap();
    assert_eq!(other_row.bot_user, None, "someone else's user isn't taken");
    assert_eq!(other_row.bot_username, None, "and is looked up only once");
    assert!(!h.fake.user("helper").unwrap().active);
    assert!(h.fake.user("writer").unwrap().active);
    assert_eq!(h.agents.retire_pending().await.unwrap(), 0);
}

#[tokio::test]
async fn an_unrecorded_bot_whose_lookup_failed_is_looked_up_again_and_retired() {
    let h = harness().await;
    let old = OffsetDateTime::now_utc() - CREATION_LEASE - time::Duration::seconds(5);
    let binding = h.creating("helper", old).await;
    assert!(
        h.store
            .set_binding_bot_username(binding, "helper")
            .await
            .unwrap()
    );
    let email = bot_email(binding);
    let (made, _) = h
        .agents
        .rest()
        .create_bot_user(&NewBotUser {
            username: "helper",
            name: "helper",
            email: &email,
        })
        .await
        .unwrap();
    Mock::given(path("/api/v1/users.info"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(h.fake.server())
        .await;

    assert_eq!(h.agents.abandon_stale().await.unwrap(), 1);
    assert_eq!(h.agents.retire_pending().await.unwrap(), 0);
    assert_eq!(h.fake.requests("users.info").await.len(), 1);
    assert!(h.fake.user("helper").unwrap().active);
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

    assert!(
        h.store
            .defer_retirement(binding, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    assert_eq!(h.agents.retire_pending().await.unwrap(), 1);
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.bot_user, Some(made.id));
    assert!(!h.fake.user("helper").unwrap().active);
}

#[tokio::test]
async fn a_noted_username_no_user_has_is_forgotten_at_once() {
    let h = harness().await;
    let old = OffsetDateTime::now_utc() - CREATION_LEASE - time::Duration::seconds(5);
    let binding = h.creating("helper", old).await;
    assert!(
        h.store
            .set_binding_bot_username(binding, "helper")
            .await
            .unwrap()
    );

    assert_eq!(h.agents.abandon_stale().await.unwrap(), 1);
    assert_eq!(h.agents.retire_pending().await.unwrap(), 0);
    assert_eq!(h.fake.requests("users.info").await.len(), 1);
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.bot_user, None);
    assert_eq!(row.bot_username, None, "forgotten, not deferred");
    let pending = h
        .store
        .pending_retirements(
            SurfaceKind::RocketChat,
            &TEAM.into(),
            OffsetDateTime::now_utc() + RETIRE_BACKOFF_MAX,
            RETIRE_MAX_ATTEMPTS,
        )
        .await
        .unwrap();
    assert!(pending.is_empty(), "{pending:?}");
}

#[tokio::test]
async fn the_fallback_username_joins_owner_and_name_with_a_dot() {
    let h = harness().await;
    h.fake.add_user("helper");
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    let bot = h
        .agents
        .create_bot(binding, "helper", "owner")
        .await
        .unwrap();
    assert_eq!(bot.username, "owner.helper");
    let binding = h.creating("all", OffsetDateTime::now_utc()).await;
    let bot = h.agents.create_bot(binding, "all", "owner").await.unwrap();
    assert_eq!(bot.username, "owner.all");
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
    assert_eq!(
        h.store
            .set_binding_bot_user(stale, &UserId::new(bot.as_str()), "stale-bot")
            .await
            .unwrap(),
        Some(true)
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
async fn a_bare_404_does_not_count_as_the_bot_user_being_gone() {
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
        .fail("users.setActiveStatus", 404, "Not Found", None)
        .await;
    assert!(!h.agents.retire(binding).await.unwrap());
    let row = h.store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.retired_at, None);
    assert!(h.fake.user("helper").unwrap().active);
}

#[tokio::test]
async fn a_bot_user_that_is_gone_counts_as_retired() {
    let h = harness().await;
    let binding = h.creating("helper", OffsetDateTime::now_utc()).await;
    assert_eq!(
        h.store
            .set_binding_bot_user(binding, &UserId::new("gone"), "helper")
            .await
            .unwrap(),
        Some(true)
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
    Mock::given(path("/api/v1/users.info"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(h.fake.server())
        .await;
    assert!(h.agents.user_named("alice").await.is_err());
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

/// A supervisor over `h`'s agents that passes what isn't a command to
/// `onward`, running every 100 ms, with its command intake.
fn new_supervisor(
    h: &Harness,
    ddp: &FakeDdp,
    onward: Option<Sender<InboundEvent>>,
) -> (Supervisor, CommandIntake) {
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
        onward,
    )
    .every(Duration::from_millis(100));
    (supervisor, intake)
}

/// Panics on the first message, as a bug in a connection would.
struct PanicOnce(AtomicBool);

#[async_trait]
impl Sink<InboundEvent> for PanicOnce {
    async fn send(&self, _: InboundEvent) -> Result<(), SendError> {
        assert!(self.0.swap(true, Ordering::SeqCst), "a connection bug");
        Ok(())
    }
}

#[tokio::test]
async fn a_connection_that_panics_is_started_again() {
    let h = harness().await;
    let ddp = FakeDdp::start().await;
    ddp.accept_tokens_of(&h.fake);
    let (_, helper) = h.created("helper").await;
    let alice = h.fake.add_user("alice");
    h.fake.add_room("ROOM", "c", "general");
    h.fake.add_member("ROOM", helper.as_str());
    h.fake.add_member("ROOM", &alice);
    let onward = Sender::new(PanicOnce(AtomicBool::new(false)));
    let (supervisor, intake) = new_supervisor(&h, &ddp, Some(onward));
    let (stop, stopping) = watch::channel(false);
    let intake = tokio::spawn(intake.run());
    let supervising = tokio::spawn(supervisor.run(stopping));

    ddp.wait_for_room(helper.as_str(), "ROOM").await;
    let message = realtime_message("M1", "ROOM", (&alice, "alice"), "hello");
    assert_eq!(ddp.send_message_to(helper.as_str(), &message), 1);
    wait_for_logins(&ddp, helper.as_str(), 2).await;

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

#[test]
fn a_connection_that_keeps_ending_waits_longer_each_time() {
    let every = Duration::from_secs(60);
    assert_eq!(supervisor::restart_delay(1, every), Duration::ZERO);
    assert_eq!(supervisor::restart_delay(2, every), every);
    assert_eq!(supervisor::restart_delay(3, every), every * 2);
    assert_eq!(supervisor::restart_delay(100, every), every * 32);
}

#[tokio::test]
async fn the_supervisor_follows_the_store_and_restarts_ended_connections() {
    let h = harness().await;
    let ddp = FakeDdp::start().await;
    ddp.accept_tokens_of(&h.fake);
    let (binding, helper) = h.created("helper").await;
    let (supervisor, intake) = new_supervisor(&h, &ddp, None);
    let (stop, stopping) = watch::channel(false);
    let intake = tokio::spawn(intake.run());
    let supervising = tokio::spawn(supervisor.run(stopping));

    wait_for_logins(&ddp, helper.as_str(), 1).await;
    h.agents.rest().set_active(&helper, false).await.unwrap();
    ddp.drop_connections();
    eventually("a refused login", async || {
        ddp.logins().iter().any(|l| l.user.is_none())
    })
    .await;
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
    eventually("the bot to be retired and disconnected", async || {
        let row = h.store.binding(binding).await.unwrap().unwrap();
        row.retired_at.is_some() && !h.fake.user("helper").unwrap().active && ddp.connections() == 0
    })
    .await;

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
