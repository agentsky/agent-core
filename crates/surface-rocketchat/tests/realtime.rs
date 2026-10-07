//! Tests of the realtime client and `RocketChatSurface::events` against
//! `testkit::rocketchat::FakeDdp` and `FakeRest`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_types::{
    AgentId, Binding, BindingId, ConvKind, InboundEvent, MemberKey, SendError, Sender, Sink,
    Surface, SurfaceError, SurfaceKind, UserId,
};
use futures::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use store::{Sealer, Store};
use surface_rocketchat::realtime::{RealtimeClient, RealtimeOptions};
use surface_rocketchat::rest::{Credentials, NewBotUser, RestClient};
use surface_rocketchat::{BotRoles, DEDUP_SOURCE, Dedup, RocketChatConfig, RocketChatSurface};
use testkit::Held;
use testkit::rocketchat::{FakeDdp, FakeRest, NOTIFY_USER, realtime_message, subscription_doc};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as Frame;
use wiremock::matchers::path;
use wiremock::{Mock, ResponseTemplate};

const TEAM: &str = "chat.example";
const WAIT: Duration = Duration::from_secs(10);

/// Records each decision in the shared store, and reports it with the name
/// of the bot whose connection asked.
struct StoreDedup {
    store: Store,
    bot: String,
    decisions: mpsc::UnboundedSender<(String, String, bool)>,
}

#[async_trait::async_trait]
impl Dedup for StoreDedup {
    async fn mark_event_processed(
        &self,
        source: &str,
        event_id: &str,
    ) -> Result<bool, SurfaceError> {
        let first = self
            .store
            .mark_event_processed(source, event_id, time::OffsetDateTime::now_utc())
            .await
            .map_err(|err| SurfaceError::Api(err.to_string()))?;
        let _ = self
            .decisions
            .send((self.bot.clone(), event_id.to_owned(), first));
        Ok(first)
    }
}

struct Channel(mpsc::UnboundedSender<InboundEvent>);

#[async_trait::async_trait]
impl Sink<InboundEvent> for Channel {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        self.0.send(event).map_err(|_| SendError)
    }
}

struct Harness {
    rest: FakeRest,
    ddp: FakeDdp,
    manager: RestClient,
    bots: BotRoles,
    store: Store,
    decisions_tx: mpsc::UnboundedSender<(String, String, bool)>,
    decisions: mpsc::UnboundedReceiver<(String, String, bool)>,
    events_tx: mpsc::UnboundedSender<InboundEvent>,
    events: mpsc::UnboundedReceiver<InboundEvent>,
}

struct Bot {
    id: String,
    username: String,
    token: SecretString,
    binding: Binding,
}

fn options() -> RealtimeOptions {
    RealtimeOptions {
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_millis(100),
        heartbeat: Duration::from_secs(5),
        setup_timeout: Duration::from_secs(5),
    }
}

async fn harness() -> Harness {
    let rest = FakeRest::start().await;
    let ddp = FakeDdp::start().await;
    let manager = RestClient::new(
        &rest.uri(),
        Credentials {
            user_id: FakeRest::MANAGER_ID.into(),
            token: SecretString::from(FakeRest::MANAGER_TOKEN),
        },
    )
    .unwrap();
    let bots = BotRoles::new(manager.clone());
    let key = Sealer::generate_key().unwrap();
    let store = Store::open_in_memory(Sealer::from_base64(&key).unwrap())
        .await
        .unwrap();
    let (decisions_tx, decisions) = mpsc::unbounded_channel();
    let (events_tx, events) = mpsc::unbounded_channel();
    Harness {
        rest,
        ddp,
        manager,
        bots,
        store,
        decisions_tx,
        decisions,
        events_tx,
        events,
    }
}

impl Harness {
    /// Creates a bot user through the REST API and lets the fake realtime
    /// server accept its token.
    async fn bot(&self, username: &str) -> Bot {
        let new = NewBotUser {
            username,
            name: username,
            email: "bot@bots.invalid",
        };
        let (user, password) = self.manager.create_bot_user(&new).await.unwrap();
        let creds = self
            .manager
            .issue_bot_token(&user.username, password, "agentd")
            .await
            .unwrap();
        self.ddp
            .add_token(creds.token.expose_secret(), user.id.as_str());
        Bot {
            id: user.id.to_string(),
            username: username.into(),
            token: creds.token,
            binding: Binding {
                id: BindingId::new_v4(),
                agent: Some(AgentId::new_v4()),
                bot: MemberKey {
                    surface: SurfaceKind::RocketChat,
                    team: TEAM.into(),
                    user: user.id,
                },
            },
        }
    }

    fn surface(&self, bot: &Bot, options: RealtimeOptions) -> Arc<RocketChatSurface> {
        let mut config = RocketChatConfig::new(
            self.rest.uri(),
            TEAM.into(),
            Credentials {
                user_id: bot.id.as_str().into(),
                token: bot.token.clone(),
            },
        );
        config.websocket_url = Some(self.ddp.url());
        config.realtime = options;
        let dedup = StoreDedup {
            store: self.store.clone(),
            bot: bot.username.clone(),
            decisions: self.decisions_tx.clone(),
        };
        Arc::new(RocketChatSurface::new(config, Arc::new(dedup), self.bots.clone()).unwrap())
    }

    /// Runs `bot`'s event loop, delivering into the harness's event channel.
    fn listen(&self, bot: &Bot) -> JoinHandle<Result<(), SurfaceError>> {
        self.listen_with(bot, options())
    }

    fn listen_with(
        &self,
        bot: &Bot,
        options: RealtimeOptions,
    ) -> JoinHandle<Result<(), SurfaceError>> {
        let surface = self.surface(bot, options);
        let binding = bot.binding.clone();
        let tx = Sender::new(Channel(self.events_tx.clone()));
        tokio::spawn(async move { surface.events(&binding, tx).await })
    }

    async fn next_event(&mut self) -> InboundEvent {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    async fn next_decision(&mut self) -> (String, String, bool) {
        tokio::time::timeout(WAIT, self.decisions.recv())
            .await
            .expect("timed out waiting for a dedup decision")
            .expect("decision channel closed")
    }

    fn no_more_events(&mut self) {
        if let Ok(event) = self.events.try_recv() {
            panic!("unexpected event {}", event.event_id);
        }
    }

    /// A channel with `members`, as the manager created it.
    fn room(&self, id: &str, members: &[&str]) {
        self.rest.add_room(id, "c", &id.to_lowercase());
        for member in members {
            self.rest.add_member(id, member);
        }
    }
}

/// Waits until `bot` is logged in and listening in `room`.
async fn ready(h: &Harness, bot: &Bot, room: &str) {
    h.ddp.wait_for_logins(&bot.id, 1).await;
    h.ddp.wait_for_room(&bot.id, room).await;
}

/// The `sub` frames asking for `room`'s messages.
fn room_subs(h: &Harness, room: &str) -> Vec<Value> {
    h.ddp
        .client_frames()
        .into_iter()
        .map(|(_, frame)| frame)
        .filter(|frame| frame["msg"] == "sub" && frame["params"][0] == room)
        .collect()
}

/// Waits until `bot`'s connection has handled every frame the server sent
/// it so far: the pong to a new ping comes after them.
async fn settled(h: &Harness, bot: &Bot) {
    let id = h.ddp.ping(&bot.id);
    h.ddp.wait_for_pong(&id).await;
}

fn mention(message: &mut Value, bot: &Bot) {
    if let Some(mentions) = message["mentions"].as_array_mut() {
        mentions.push(json!({ "_id": bot.id, "username": bot.username, "type": "user" }));
    }
}

#[tokio::test]
async fn login_sends_connect_then_the_token_as_a_resume_token() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    let _task = h.listen(&bot);
    h.ddp.wait_for_logins(&bot.id, 1).await;
    let logins = h.ddp.logins();
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0].token, bot.token.expose_secret());
    assert_eq!(logins[0].user.as_deref(), Some(bot.id.as_str()));
    let frames = h.ddp.client_frames();
    assert_eq!(frames[0].1["msg"], "connect");
    assert_eq!(frames[0].1["version"], "1");
    assert_eq!(frames[1].1["msg"], "method");
    assert_eq!(frames[1].1["method"], "login");
    assert_eq!(
        frames[1].1["params"][0]["resume"],
        bot.token.expose_secret()
    );
}

#[tokio::test]
async fn a_rejected_token_ends_events_with_unauthorized() {
    let h = harness().await;
    let bot = Bot {
        token: SecretString::from("revoked"),
        ..h.bot("helper").await
    };
    let task = h.listen(&bot);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(result, Err(SurfaceError::Unauthorized));
    assert_eq!(h.ddp.logins()[0].user, None);
}

#[tokio::test]
async fn a_binding_for_another_user_is_refused() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    let other = h.bot("other").await;
    let surface = h.surface(&bot, options());
    let tx = Sender::new(Channel(h.events_tx.clone()));
    let result = surface.events(&other.binding, tx).await;
    assert!(matches!(result, Err(SurfaceError::Api(_))));
    assert_eq!(h.ddp.connections(), 0);
}

#[tokio::test]
async fn it_subscribes_to_room_changes_and_to_every_room_it_is_in() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    h.rest.add_room("PRIVATE", "p", "private");
    h.rest.add_member("PRIVATE", &bot.id);
    h.room("ELSEWHERE", &[]);
    let _task = h.listen(&bot);
    let notices = format!("{}/subscriptions-changed", bot.id);
    h.ddp
        .wait_for_subscription(&bot.id, NOTIFY_USER, &notices)
        .await;
    h.ddp.wait_for_room(&bot.id, "GENERAL").await;
    h.ddp.wait_for_room(&bot.id, "PRIVATE").await;
    assert!(
        !h.ddp
            .subscribed(&bot.id, "stream-room-messages", "ELSEWHERE")
    );
    assert!(
        !h.ddp
            .subscribed(&bot.id, "stream-room-messages", "__my_messages__")
    );
    let subs: Vec<Value> = h
        .ddp
        .client_frames()
        .into_iter()
        .map(|(_, frame)| frame)
        .filter(|frame| frame["msg"] == "sub")
        .collect();
    assert!(subs.iter().all(|sub| sub["params"][1] == false));
}

#[tokio::test]
async fn a_mention_produces_the_inbound_event() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let root = h.rest.seed_message("GENERAL", &alice, "a question", None);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let mut message = realtime_message("m-1", "GENERAL", (&alice, "alice"), "@helper look");
    mention(&mut message, &bot);
    message["tmid"] = json!(root);
    message["files"] = json!([{ "_id": "f1", "name": "log.txt", "type": "text/plain", "size": 5 }]);
    assert_eq!(h.ddp.send_message(&message), 1);
    let event = h.next_event().await;
    assert_eq!(event.event_id, "m-1");
    assert_eq!(event.binding, bot.binding.id);
    assert_eq!(event.sender.surface, SurfaceKind::RocketChat);
    assert_eq!(event.sender.team.as_str(), TEAM);
    assert_eq!(event.sender.user.as_str(), alice);
    assert!(!event.sender_is_bot);
    assert_eq!(event.sender_bot_user, None);
    assert_eq!(event.conv.conversation.as_str(), "GENERAL");
    assert_eq!(event.conv.team.as_str(), TEAM);
    assert_eq!(event.conv_kind, ConvKind::Channel);
    assert_eq!(event.message.id.as_str(), "m-1");
    assert_eq!(event.text, "@helper look");
    assert_eq!(event.mentions, [UserId::from(bot.id.as_str())]);
    assert_eq!(
        event.thread_root.as_ref().map(|t| t.as_str()),
        Some(root.as_str())
    );
    assert_eq!(
        event.reply_to.as_ref().map(|r| r.id.as_str()),
        Some(root.as_str())
    );
    assert_eq!(event.files.len(), 1);
    assert_eq!(
        event.files[0].url,
        format!("{}/file-upload/f1/log.txt", h.rest.uri())
    );
}

#[tokio::test]
async fn a_dm_and_a_group_dm_get_their_conv_kinds() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    let bob = h.rest.add_user("bob");
    let surface = h.surface(&bot, options());
    let dm = surface.rest().create_dm("alice").await.unwrap();
    h.rest.add_room("GROUP", "d", "");
    for member in [&bot.id, &alice, &bob] {
        h.rest.add_member("GROUP", member);
    }
    let _task = h.listen(&bot);
    ready(&h, &bot, dm.as_str()).await;
    h.ddp.wait_for_room(&bot.id, "GROUP").await;
    h.ddp.send_message(&realtime_message(
        "m-dm",
        dm.as_str(),
        (&alice, "alice"),
        "hi",
    ));
    let event = h.next_event().await;
    assert_eq!(event.conv_kind, ConvKind::Dm);
    assert!(event.is_dm());
    h.ddp.send_message(&realtime_message(
        "m-group",
        "GROUP",
        (&bob, "bob"),
        "hi all",
    ));
    let event = h.next_event().await;
    assert_eq!(event.conv_kind, ConvKind::GroupDm);
}

#[tokio::test]
async fn system_messages_edits_and_old_updates_are_ignored() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let sender = (alice.as_str(), "alice");
    let mut joined = realtime_message("m-join", "GENERAL", sender, "alice");
    joined["t"] = json!("uj");
    let mut edited = realtime_message("m-edit", "GENERAL", sender, "fixed typo");
    edited["editedAt"] = edited["ts"].clone();
    let mut reacted = realtime_message("m-old", "GENERAL", sender, "old news");
    reacted["ts"] = json!({ "$date": 1_700_000_000_000_i64 });
    let fresh = realtime_message("m-new", "GENERAL", sender, "new");
    for message in [&joined, &edited, &reacted, &fresh] {
        h.ddp.send_message(message);
    }
    assert_eq!(h.next_event().await.event_id, "m-new");
    let (_, id, first) = h.next_decision().await;
    assert_eq!((id.as_str(), first), ("m-new", true));
    h.no_more_events();
}

#[tokio::test]
async fn a_dropped_connection_reconnects_and_resubscribes() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp.drop_connections();
    h.ddp.wait_for_logins(&bot.id, 2).await;
    h.ddp.wait_for_room(&bot.id, "GENERAL").await;
    let notices = format!("{}/subscriptions-changed", bot.id);
    assert!(h.ddp.subscribed(&bot.id, NOTIFY_USER, &notices));
    h.ddp.send_message(&realtime_message(
        "m-after",
        "GENERAL",
        (&alice, "alice"),
        "back?",
    ));
    assert_eq!(h.next_event().await.event_id, "m-after");
}

#[tokio::test]
async fn rooms_joined_while_connected_are_subscribed_again_after_a_drop() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.room("NEW", &[&bot.id, &alice]);
    h.ddp.notify_subscription(
        &bot.id,
        "inserted",
        &subscription_doc(&bot.id, "NEW", "c", "new"),
    );
    h.ddp.wait_for_room(&bot.id, "NEW").await;
    h.ddp.drop_connections();
    h.ddp.wait_for_logins(&bot.id, 2).await;
    h.ddp.wait_for_room(&bot.id, "NEW").await;
    h.ddp
        .send_message(&realtime_message("m-new", "NEW", (&alice, "alice"), "hi"));
    assert_eq!(h.next_event().await.conv.conversation.as_str(), "NEW");
}

#[tokio::test]
async fn a_subscriptions_changed_notice_subscribes_to_the_new_room() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.room("INVITED", &[&bot.id, &alice]);
    let delivered = h.ddp.notify_subscription(
        &bot.id,
        "inserted",
        &subscription_doc(&bot.id, "INVITED", "c", "invited"),
    );
    assert_eq!(delivered, 1);
    h.ddp.wait_for_room(&bot.id, "INVITED").await;
    let mut message = realtime_message("m-inv", "INVITED", (&alice, "alice"), "welcome @helper");
    mention(&mut message, &bot);
    h.ddp.send_message(&message);
    let event = h.next_event().await;
    assert_eq!(event.conv.conversation.as_str(), "INVITED");
    assert_eq!(event.mentions, [UserId::from(bot.id.as_str())]);
    assert_eq!(h.ddp.logins().len(), 1);
}

#[tokio::test]
async fn a_removed_notice_unsubscribes_and_other_room_types_are_ignored() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let livechat = subscription_doc(&bot.id, "LIVE", "l", "visitor");
    h.ddp.notify_subscription(&bot.id, "inserted", &livechat);
    h.ddp
        .notify_subscription(&bot.id, "removed", &json!({ "_id": "x", "rid": "GENERAL" }));
    h.ddp.wait_for_room_unsubscribed(&bot.id, "GENERAL").await;
    assert!(!h.ddp.subscribed(&bot.id, "stream-room-messages", "LIVE"));
    h.ddp
        .wait_for_frame("an unsub", |frame| frame["msg"] == "unsub")
        .await;
}

#[tokio::test]
async fn a_refused_room_subscription_leaves_the_connection_up() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    h.room("SECRET", &[&bot.id]);
    h.ddp.forbid_room("SECRET");
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp
        .wait_for_frame("the SECRET subscription", |frame| {
            frame["msg"] == "sub" && frame["params"][0] == "SECRET"
        })
        .await;
    h.ddp.send_message(&realtime_message(
        "m-ok",
        "GENERAL",
        (&alice, "alice"),
        "still here",
    ));
    assert_eq!(h.next_event().await.event_id, "m-ok");
    assert!(!h.ddp.subscribed(&bot.id, "stream-room-messages", "SECRET"));
    assert_eq!(h.ddp.logins().len(), 1);
}

#[tokio::test]
async fn server_pings_are_answered_with_their_id() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let id = h.ddp.ping(&bot.id);
    h.ddp.wait_for_pong(&id).await;
}

#[tokio::test]
async fn a_silent_server_is_pinged_then_replaced() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    let quick = RealtimeOptions {
        heartbeat: Duration::from_millis(100),
        ..options()
    };
    let _task = h.listen_with(&bot, quick);
    ready(&h, &bot, "GENERAL").await;
    h.ddp
        .wait_for_frame("a client ping", |frame| frame["msg"] == "ping")
        .await;
    h.ddp.mute_connections();
    h.ddp.wait_for_logins(&bot.id, 2).await;
    h.ddp.wait_for_room(&bot.id, "GENERAL").await;
}

#[tokio::test]
async fn a_slow_consumer_is_not_taken_for_a_silent_server() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let rest = RestClient::new(
        &h.rest.uri(),
        Credentials {
            user_id: bot.id.as_str().into(),
            token: bot.token.clone(),
        },
    )
    .unwrap();
    let heartbeat = Duration::from_millis(100);
    let quick = RealtimeOptions {
        heartbeat,
        ..options()
    };
    let client = RealtimeClient::new(&h.ddp.url(), rest, quick).unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    let _run = tokio::spawn(async move { client.run(tx).await });
    ready(&h, &bot, "GENERAL").await;

    for id in ["m-1", "m-2"] {
        h.ddp
            .send_message(&realtime_message(id, "GENERAL", (&alice, "alice"), "hi"));
    }
    tokio::time::sleep(heartbeat * 5).await;
    for _ in 0..2 {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    h.ddp
        .send_message(&realtime_message("m-3", "GENERAL", (&alice, "alice"), "hi"));
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(heartbeat * 3).await;
    assert_eq!(h.ddp.logins().len(), 1, "the connection was replaced");
}

#[tokio::test]
async fn closing_the_receiver_ends_events_with_closed() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let surface = h.surface(&bot, options());
    let (tx, rx) = mpsc::unbounded_channel();
    let binding = bot.binding.clone();
    let task =
        tokio::spawn(async move { surface.events(&binding, Sender::new(Channel(tx))).await });
    ready(&h, &bot, "GENERAL").await;
    drop(rx);
    h.ddp.send_message(&realtime_message(
        "m-late",
        "GENERAL",
        (&alice, "alice"),
        "anyone?",
    ));
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(result, Err(SurfaceError::Closed));
    let (_, id, first) = h.next_decision().await;
    assert_eq!((id.as_str(), first), ("m-late", true));
}

#[tokio::test]
async fn two_bots_in_one_room_deliver_each_message_once() {
    let mut h = harness().await;
    let a = h.bot("alpha").await;
    let b = h.bot("beta").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&a.id, &b.id, &alice]);
    let _a = h.listen(&a);
    let _b = h.listen(&b);
    ready(&h, &a, "GENERAL").await;
    ready(&h, &b, "GENERAL").await;
    let mut message = realtime_message("m-both", "GENERAL", (&alice, "alice"), "@alpha @beta hi");
    mention(&mut message, &a);
    mention(&mut message, &b);
    assert_eq!(h.ddp.send_message(&message), 2);
    let mut decisions = [h.next_decision().await, h.next_decision().await];
    decisions.sort();
    assert_eq!(decisions[0].0, "alpha");
    assert_eq!(decisions[1].0, "beta");
    assert_eq!(decisions.iter().filter(|(_, _, first)| *first).count(), 1);
    assert!(decisions.iter().all(|(_, id, _)| id == "m-both"));
    let event = h.next_event().await;
    assert_eq!(
        event.mentions,
        [UserId::from(a.id.as_str()), UserId::from(b.id.as_str())]
    );
    h.no_more_events();
    let processed_again = h
        .store
        .mark_event_processed(DEDUP_SOURCE, "m-both", time::OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(!processed_again);
}

/// A posts mentioning B and C. The copy is delivered to `first`'s connection
/// and recorded before the other connection sees it.
async fn a_mentions_b_recorded_by(first: &str) {
    let mut h = harness().await;
    let a = h.bot("alpha").await;
    let b = h.bot("beta").await;
    let c = h.bot("gamma").await;
    h.room("GENERAL", &[&a.id, &b.id, &c.id]);
    let _a = h.listen(&a);
    let _b = h.listen(&b);
    ready(&h, &a, "GENERAL").await;
    ready(&h, &b, "GENERAL").await;
    let (winner, loser) = if first == "alpha" { (&a, &b) } else { (&b, &a) };
    let mut message = realtime_message(
        "m-a2b",
        "GENERAL",
        (&a.id, "alpha"),
        "@beta @gamma over to you",
    );
    mention(&mut message, &b);
    mention(&mut message, &c);

    assert_eq!(h.ddp.send_message_to(&winner.id, &message), 1);
    let event = h.next_event().await;
    assert_eq!(event.binding, winner.binding.id);
    assert_eq!(
        h.next_decision().await,
        (winner.username.clone(), "m-a2b".into(), true)
    );

    assert_eq!(h.ddp.send_message_to(&loser.id, &message), 1);
    assert_eq!(
        h.next_decision().await,
        (loser.username.clone(), "m-a2b".into(), false)
    );
    h.no_more_events();

    assert_eq!(event.event_id, "m-a2b");
    assert_eq!(event.sender.user.as_str(), a.id);
    assert!(event.sender_is_bot);
    assert_eq!(event.sender_bot_user.as_ref(), Some(&event.sender.user));
    assert_eq!(
        event.mentions,
        [UserId::from(b.id.as_str()), UserId::from(c.id.as_str())]
    );
}

#[tokio::test]
async fn a_post_by_a_mentioning_b_survives_when_a_records_it_first() {
    a_mentions_b_recorded_by("alpha").await;
}

#[tokio::test]
async fn a_post_by_a_mentioning_b_survives_when_b_records_it_first() {
    a_mentions_b_recorded_by("beta").await;
}

#[tokio::test]
async fn the_bot_field_marks_a_sender_as_a_bot_without_a_role() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let hook = h.rest.add_user("webhook");
    h.room("GENERAL", &[&bot.id, &hook]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let mut message = realtime_message("m-hook", "GENERAL", (&hook, "webhook"), "build failed");
    message["bot"] = json!({ "i": "integration" });
    h.ddp.send_message(&message);
    let event = h.next_event().await;
    assert!(event.sender_is_bot);
    assert_eq!(
        event.sender_bot_user.as_ref().map(UserId::as_str),
        Some(hook.as_str())
    );
}

#[tokio::test]
async fn a_message_in_an_unreadable_room_is_left_for_another_connection() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    h.rest.add_room("GHOST", "p", "ghost");
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp.notify_subscription(
        &bot.id,
        "inserted",
        &subscription_doc(&bot.id, "GHOST", "p", "ghost"),
    );
    h.ddp.wait_for_room(&bot.id, "GHOST").await;
    h.ddp.send_message(&realtime_message(
        "m-ghost",
        "GHOST",
        (&alice, "alice"),
        "boo",
    ));
    h.ddp.send_message(&realtime_message(
        "m-real",
        "GENERAL",
        (&alice, "alice"),
        "hi",
    ));
    assert_eq!(h.next_event().await.event_id, "m-real");
    assert_eq!(h.next_decision().await.1, "m-real");
    let unclaimed = h
        .store
        .mark_event_processed(DEDUP_SOURCE, "m-ghost", time::OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(unclaimed);
}

#[tokio::test]
async fn a_tls_endpoint_that_fails_the_handshake_is_retried() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, mut accepted) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
            let _ = accepted_tx.send(());
        }
    });
    let rest = RestClient::new(
        &format!("https://127.0.0.1:{port}"),
        Credentials {
            user_id: "u1".into(),
            token: SecretString::from("t"),
        },
    )
    .unwrap();
    let url =
        surface_rocketchat::realtime::websocket_url(&format!("https://127.0.0.1:{port}")).unwrap();
    let client = RealtimeClient::new(&url, rest, options()).unwrap();
    let (tx, rx) = mpsc::channel(1);
    let run = tokio::spawn(async move { client.run(tx).await });
    for _ in 0..2 {
        tokio::time::timeout(WAIT, accepted.recv())
            .await
            .unwrap()
            .unwrap();
    }
    drop(rx);
    let result = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(result, Ok(()));
}

/// A realtime endpoint that accepts any `connect` and answers every method
/// call with `result`. It reports each call it answers.
async fn answering_login_with(result: Value) -> (String, mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/websocket", listener.local_addr().unwrap());
    let (called_tx, called) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (called_tx, result) = (called_tx.clone(), result.clone());
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                    return;
                };
                while let Some(Ok(frame)) = ws.next().await {
                    let Ok(sent) = serde_json::from_slice::<Value>(&frame.into_data()) else {
                        continue;
                    };
                    let reply = match sent["msg"].as_str() {
                        Some("connect") => json!({ "msg": "connected", "session": "s1" }),
                        Some("method") => {
                            let _ = called_tx.send(());
                            json!({ "msg": "result", "id": sent["id"], "result": result })
                        }
                        _ => continue,
                    };
                    if ws.send(Frame::text(reply.to_string())).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, called)
}

fn client_for(url: &str) -> RealtimeClient {
    let rest = RestClient::new(
        "http://127.0.0.1:9",
        Credentials {
            user_id: "u1".into(),
            token: SecretString::from("t"),
        },
    )
    .unwrap();
    RealtimeClient::new(url, rest, options()).unwrap()
}

#[tokio::test]
async fn a_login_result_without_an_id_is_retried() {
    let (url, mut logins) = answering_login_with(json!({ "token": "t" })).await;
    let client = client_for(&url);
    let (tx, rx) = mpsc::channel(1);
    let run = tokio::spawn(async move { client.run(tx).await });
    for _ in 0..2 {
        tokio::time::timeout(WAIT, logins.recv())
            .await
            .unwrap()
            .unwrap();
    }
    drop(rx);
    let result = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn a_login_result_for_another_user_is_fatal() {
    let (url, _logins) = answering_login_with(json!({ "id": "u2", "token": "t" })).await;
    let client = client_for(&url);
    let (tx, _rx) = mpsc::channel(1);
    let result = tokio::time::timeout(WAIT, client.run(tx)).await.unwrap();
    assert!(matches!(result, Err(SurfaceError::Api(_))), "{result:?}");
}

#[tokio::test]
async fn a_server_that_drops_after_setup_is_retried_less_and_less_often() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    let flapping = RealtimeOptions {
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_secs(1),
        ..options()
    };
    let _task = h.listen_with(&bot, flapping);
    let mut gaps = Vec::new();
    for logins in 1..=7 {
        h.ddp.wait_for_logins(&bot.id, logins).await;
        h.ddp.wait_for_room(&bot.id, "GENERAL").await;
        let dropped = Instant::now();
        h.ddp.drop_connections();
        h.ddp.wait_for_logins(&bot.id, logins + 1).await;
        gaps.push(dropped.elapsed());
    }
    assert!(gaps[5] >= Duration::from_millis(150), "{gaps:?}");
    assert!(gaps[6] >= Duration::from_millis(300), "{gaps:?}");
}

#[tokio::test]
async fn a_removal_without_rid_is_resolved_and_a_re_add_subscribes_again() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id]);
    h.room("TEAM", &[&bot.id, &alice]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "TEAM").await;
    h.ddp.notify_subscription(
        &bot.id,
        "removed",
        &json!({ "_id": format!("TEAM{}", bot.id) }),
    );
    h.ddp.wait_for_room_unsubscribed(&bot.id, "TEAM").await;
    assert!(h.ddp.subscribed(&bot.id, "stream-room-messages", "GENERAL"));
    let mut again = subscription_doc(&bot.id, "TEAM", "c", "team");
    again["_id"] = json!("a-new-subscription");
    h.ddp.notify_subscription(&bot.id, "inserted", &again);
    h.ddp.wait_for_room(&bot.id, "TEAM").await;
    h.ddp
        .send_message(&realtime_message("m-back", "TEAM", (&alice, "alice"), "hi"));
    assert_eq!(h.next_event().await.event_id, "m-back");
}

#[tokio::test]
async fn an_inserted_notice_for_a_subscribed_room_subscribes_to_it_again() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    let first = room_subs(&h, "GENERAL")[0]["id"].clone();
    h.ddp
        .notify_subscription(&bot.id, "removed", &json!({ "_id": "unknown" }));
    h.ddp.notify_subscription(
        &bot.id,
        "inserted",
        &subscription_doc(&bot.id, "GENERAL", "c", "general"),
    );
    h.ddp
        .wait_for_frame("the old subscription's unsub", |frame| {
            frame["msg"] == "unsub" && frame["id"] == first
        })
        .await;
    h.ddp.wait_for_room(&bot.id, "GENERAL").await;
    assert_eq!(room_subs(&h, "GENERAL").len(), 2);
    h.ddp.send_message(&realtime_message(
        "m-again",
        "GENERAL",
        (&alice, "alice"),
        "hi",
    ));
    assert_eq!(h.next_event().await.event_id, "m-again");
    h.no_more_events();
}

#[tokio::test]
async fn room_changes_while_the_rooms_are_listed_apply_on_top_of_the_list() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    let listed = |room: &str| json!({ "_id": format!("{room}-doc"), "rid": room, "t": "c", "name": room.to_lowercase() });
    let stale = json!({
        "success": true,
        "update": [listed("GENERAL"), listed("GONE"), listed("LEFT")],
        "remove": [],
    });
    let (held, mut hold) = Held::new(ResponseTemplate::new(200).set_body_json(stale));
    Mock::given(path("/api/v1/subscriptions.get"))
        .respond_with(held)
        .with_priority(1)
        .mount(h.rest.server())
        .await;
    let _task = h.listen(&bot);
    let notices = format!("{}/subscriptions-changed", bot.id);
    h.ddp
        .wait_for_subscription(&bot.id, NOTIFY_USER, &notices)
        .await;
    hold.arrived().await;
    h.ddp.notify_subscription(
        &bot.id,
        "removed",
        &json!({ "_id": "GONE-doc", "rid": "GONE" }),
    );
    h.ddp
        .notify_subscription(&bot.id, "removed", &json!({ "_id": "LEFT-doc" }));
    h.ddp.notify_subscription(
        &bot.id,
        "inserted",
        &subscription_doc(&bot.id, "NEW", "c", "new"),
    );
    settled(&h, &bot).await;
    hold.release();
    h.ddp.wait_for_room(&bot.id, "GENERAL").await;
    h.ddp.wait_for_room(&bot.id, "NEW").await;
    settled(&h, &bot).await;
    assert!(room_subs(&h, "GONE").is_empty());
    assert!(room_subs(&h, "LEFT").is_empty());
    assert_eq!(room_subs(&h, "NEW").len(), 1);
}

#[tokio::test]
async fn a_failed_role_lookup_delivers_the_message_as_a_persons() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &alice]);
    h.rest.fail("users.info", 500, "unavailable", None).await;
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp.send_message(&realtime_message(
        "m-roles",
        "GENERAL",
        (&alice, "alice"),
        "hello",
    ));
    let event = h.next_event().await;
    assert_eq!(event.event_id, "m-roles");
    assert!(!event.sender_is_bot);
    assert_eq!(
        h.next_decision().await,
        (bot.username.clone(), "m-roles".into(), true)
    );
}

#[tokio::test]
async fn a_manager_that_cannot_see_roles_still_gets_messages_delivered() {
    let mut h = harness().await;
    let bot = h.bot("helper").await;
    let peer = h.bot("peer").await;
    let alice = h.rest.add_user("alice");
    h.room("GENERAL", &[&bot.id, &peer.id, &alice]);
    let peer_rest = RestClient::new(
        &h.rest.uri(),
        Credentials {
            user_id: peer.id.as_str().into(),
            token: peer.token.clone(),
        },
    )
    .unwrap();
    h.bots = BotRoles::new(peer_rest);
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp.send_message(&realtime_message(
        "m-hidden",
        "GENERAL",
        (&alice, "alice"),
        "hello",
    ));
    let event = h.next_event().await;
    assert_eq!(event.event_id, "m-hidden");
    assert!(!event.sender_is_bot);
}

#[tokio::test]
async fn a_refused_room_is_not_asked_for_again_until_it_is_inserted() {
    let h = harness().await;
    let bot = h.bot("helper").await;
    h.room("GENERAL", &[&bot.id]);
    h.room("SECRET", &[&bot.id]);
    h.ddp.forbid_room("SECRET");
    let _task = h.listen(&bot);
    ready(&h, &bot, "GENERAL").await;
    h.ddp
        .wait_for_frame("the SECRET subscription", |frame| {
            frame["msg"] == "sub" && frame["params"][0] == "SECRET"
        })
        .await;
    let secret = subscription_doc(&bot.id, "SECRET", "p", "secret");
    for _ in 0..3 {
        h.ddp.notify_subscription(&bot.id, "updated", &secret);
    }
    settled(&h, &bot).await;
    assert_eq!(room_subs(&h, "SECRET").len(), 1);
    h.ddp.notify_subscription(&bot.id, "inserted", &secret);
    h.ddp
        .wait_for_frames("SECRET asked for again", 2, |frame| {
            frame["msg"] == "sub" && frame["params"][0] == "SECRET"
        })
        .await;
    assert_eq!(h.ddp.logins().len(), 1);
}
