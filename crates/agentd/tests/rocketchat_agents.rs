//! The agent lifecycle over Rocket.Chat end to end: agentd serving a fake
//! Rocket.Chat, with the manager bot's connection and every agent's
//! connection started as `serve` starts them.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{ConvKind, ConvRef, MemberId, MemberKey, ScopeKey, SurfaceKind, ThreadKey};
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{AgentState, BindingState, NewClaudeLink};
use testkit::rocketchat::{FakeDdp, FakeRest, realtime_message, subscription_doc};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use common::{CONFIG, master_key};

const TEAM: &str = "chat.example";
const WAIT: Duration = Duration::from_secs(10);

/// A fake Rocket.Chat with the manager, alice and bob, each with a DM
/// with the manager, and a channel all three are in.
struct Chat {
    fake: FakeRest,
    ddp: FakeDdp,
    alice: String,
    bob: String,
    master_key: String,
}

impl Chat {
    async fn start() -> Self {
        let fake = FakeRest::start().await;
        let ddp = FakeDdp::start().await;
        ddp.add_token(FakeRest::MANAGER_TOKEN, FakeRest::MANAGER_ID);
        ddp.accept_tokens_of(&fake);
        let alice = fake.add_user("alice");
        let bob = fake.add_user("bob");
        for (room, user) in [("DM-ALICE", &alice), ("DM-BOB", &bob)] {
            fake.add_room(room, "d", "");
            fake.add_member(room, user);
        }
        fake.add_room("GENERAL", "c", "general");
        fake.add_member("GENERAL", &alice);
        fake.add_member("GENERAL", &bob);
        Self {
            fake,
            ddp,
            alice,
            bob,
            master_key: master_key(),
        }
    }

    fn config(&self, db_url: &str) -> Config {
        self.config_with(db_url, "")
    }

    /// The configuration, with the sections `extra` added.
    fn config_with(&self, db_url: &str, extra: &str) -> Config {
        let text = format!(
            "{}{extra}\n[rocketchat]\nbase_url = \"{}\"\nwebsocket_url = \"{}\"\nteam = \"{TEAM}\"\n\
             manager_user_id = \"{}\"\n",
            CONFIG.replace("sqlite::memory:", db_url),
            self.fake.uri(),
            self.ddp.url(),
            FakeRest::MANAGER_ID,
        );
        let env = vec![
            ("AGENTD_MASTER_KEY".to_owned(), self.master_key.clone()),
            (
                "AGENTD_RC_MANAGER_TOKEN".to_owned(),
                FakeRest::MANAGER_TOKEN.to_owned(),
            ),
        ];
        Config::parse(&text, env).unwrap()
    }

    fn user(&self, name: &str) -> &str {
        match name {
            "alice" => &self.alice,
            "bob" => &self.bob,
            _ => panic!("no user {name}"),
        }
    }

    fn dm_of(name: &str) -> &'static str {
        match name {
            "alice" => "DM-ALICE",
            "bob" => "DM-BOB",
            _ => panic!("no DM for {name}"),
        }
    }

    /// Stores a message from `name` in `room` and delivers it to every
    /// connection there, with `extra` fields merged in. Returns its id.
    fn say(&self, name: &str, room: &str, text: &str, extra: Value) -> String {
        let id = self.fake.seed_message(room, self.user(name), text, None);
        let mut message = realtime_message(&id, room, (self.user(name), name), text);
        if let (Some(message), Some(extra)) = (message.as_object_mut(), extra.as_object()) {
            message.extend(extra.clone());
        }
        self.ddp.send_message(&message);
        id
    }

    /// Sends `text` to the manager bot in `name`'s DM and returns its reply.
    async fn command(&self, name: &str, text: &str) -> String {
        let room = Self::dm_of(name);
        let before = self.posted(room).await.len();
        self.say(name, room, text, json!({}));
        self.wait_for_posts(room, before + 1).await.remove(before)
    }

    async fn posted(&self, room: &str) -> Vec<String> {
        self.fake
            .requests("chat.postMessage")
            .await
            .into_iter()
            .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
            .filter(|body| body["roomId"] == room)
            .filter_map(|body| body["text"].as_str().map(str::to_owned))
            .collect()
    }

    async fn wait_for_posts(&self, room: &str, count: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let texts = self.posted(room).await;
            if texts.len() >= count {
                return texts;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{room} has {texts:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn reactors(&self, message: &str) -> Vec<String> {
        self.fake
            .message(message)
            .map(|m| m.reactions.into_iter().map(|(_, user)| user).collect())
            .unwrap_or_default()
    }

    async fn wait_for_reaction(&self, message: &str, user: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self.reactors(message).iter().any(|u| u == user) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no reaction from {user} on {message}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_connections(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.ddp.connections() != count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} connections, not {count}",
                self.ddp.connections()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Creates the agent `name` for `owner` from their DM and waits for its
    /// bot to connect and hear `GENERAL`, which it is added to as the
    /// owner would. Returns the bot's user id.
    async fn create(&self, running: &Running, owner: &str, name: &str) -> String {
        let reply = self.command(owner, &format!("create {name}")).await;
        assert!(reply.starts_with(&format!("Created `{name}`.")), "{reply}");
        let bot = running
            .bot_of(self.user(owner), name)
            .await
            .expect("the agent has a bot");
        let logins = self
            .ddp
            .logins()
            .iter()
            .filter(|l| l.user.as_deref() == Some(bot.as_str()))
            .count();
        self.ddp.wait_for_logins(&bot, logins.max(1)).await;
        self.join(&bot, "GENERAL").await;
        bot
    }

    /// Adds `bot` to `room` and tells its connection, as Rocket.Chat does.
    async fn join(&self, bot: &str, room: &str) {
        self.fake.add_member(room, bot);
        self.ddp
            .wait_for_subscription(
                bot,
                testkit::rocketchat::NOTIFY_USER,
                &format!("{bot}/subscriptions-changed"),
            )
            .await;
        self.ddp
            .notify_subscription(bot, "inserted", &subscription_doc(bot, room, "c", room));
        self.ddp.wait_for_room(bot, room).await;
    }
}

/// agentd running against a [`Chat`].
struct Running {
    app: App,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn start(chat: &Chat, db_url: &str) -> Self {
        Self::start_with(chat, chat.config(db_url)).await
    }

    async fn start_with(chat: &Chat, config: Config) -> Self {
        let app = App::open(config).await.unwrap();
        let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
            .await
            .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(
            async {
                let _ = stopped.await;
            },
            std::future::pending(),
        ));
        chat.ddp
            .wait_for_room(FakeRest::MANAGER_ID, "DM-ALICE")
            .await;
        chat.ddp
            .wait_for_room(FakeRest::MANAGER_ID, "GENERAL")
            .await;
        Self {
            app,
            stop: Some(stop),
            task,
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(WAIT, &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    async fn member(&self, user: &str) -> MemberId {
        self.app
            .store()
            .ensure_member(&key(user), user, OffsetDateTime::now_utc())
            .await
            .unwrap()
    }

    /// Links `user`'s Claude account directly in the store.
    async fn link(&self, user: &str) {
        let member = self.member(user).await;
        let link = NewClaudeLink {
            access_token: SecretString::from("access"),
            refresh_token: SecretString::from("refresh"),
            expires_at: OffsetDateTime::now_utc() + time::Duration::hours(8),
            plan: None,
            rate_limit_tier: None,
        };
        self.app
            .store()
            .put_claude_link(member, &link, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    async fn agent(&self, owner: &str, name: &str) -> Option<store::Agent> {
        let member = self.member(owner).await;
        self.app.store().agent_by_name(member, name).await.unwrap()
    }

    async fn bot_of(&self, owner: &str, name: &str) -> Option<String> {
        let agent = self.agent(owner, name).await?;
        let bindings = self.app.store().bindings_of(agent.id).await.unwrap();
        bindings
            .into_iter()
            .find_map(|b| b.bot_user.map(|u| u.to_string()))
    }
}

fn key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        user: user.into(),
    }
}

fn mention(users: &[&str]) -> Value {
    let mentions: Vec<Value> = users.iter().map(|u| json!({ "_id": u })).collect();
    json!({ "mentions": mentions })
}

/// A directory for a database file, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("agentd-agents-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn db_url(&self) -> String {
        format!("sqlite://{}", self.0.join("agentd.db").display())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn create_makes_a_bot_user_and_starts_its_connection() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;

    let unlinked = chat.command("alice", "create helper").await;
    assert_eq!(
        unlinked,
        "Link your Claude account first: send `login`. Your agents run on it."
    );
    assert!(chat.fake.user("helper").is_none());

    running.link(&chat.alice).await;
    let reply = chat.command("alice", "create helper").await;
    assert!(
        reply.starts_with("Created `helper`. Its bot user is @helper. To use it in a room, invite"),
        "{reply}"
    );
    let bot = chat.fake.user("helper").unwrap();
    assert_eq!(bot.roles, ["bot"]);
    assert!(bot.active);
    assert!(!bot.verified);
    assert_eq!(bot.name, "helper");
    chat.ddp.wait_for_logins(&bot.id, 1).await;

    let agent = running.agent(&chat.alice, "helper").await.unwrap();
    assert_eq!(agent.state, AgentState::Active);
    assert!(
        agent
            .persona
            .starts_with("You are helper, an agent that alice created."),
        "{}",
        agent.persona
    );
    let bindings = running.app.store().bindings_of(agent.id).await.unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].state, BindingState::Active);
    assert_eq!(bindings[0].bot_username.as_deref(), Some("helper"));

    let again = chat.command("alice", "create helper").await;
    assert_eq!(again, "You already have an agent named `helper`.");
    let listed = chat.command("bob", "list").await;
    assert_eq!(listed, "Agents:\n- `helper` (@helper), owned by alice");
    running.stop().await;
}

#[tokio::test]
async fn a_member_has_at_most_max_per_owner_agents() {
    let chat = Chat::start().await;
    let config = chat.config_with("sqlite::memory:", "\n[agents]\nmax_per_owner = 1\n");
    let running = Running::start_with(&chat, config).await;
    running.link(&chat.alice).await;
    chat.create(&running, "alice", "helper").await;

    let refused = chat.command("alice", "create writer").await;
    assert_eq!(
        refused,
        "You already have as many agents as one member may have (1), so I didn't create \
         `writer`. Delete one first with `delete <name>`."
    );
    assert!(chat.fake.user("writer").is_none());
    assert!(running.agent(&chat.alice, "writer").await.is_none());

    chat.command("alice", "delete helper").await;
    let created = chat.command("alice", "create writer").await;
    assert!(created.starts_with("Created `writer`."), "{created}");
    running.stop().await;
}

#[tokio::test]
async fn a_taken_username_gets_the_owners_prefix() {
    let chat = Chat::start().await;
    chat.fake.add_user("helper");
    chat.fake.add_user("coder");
    chat.fake.add_user("alice.coder");
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;

    let reply = chat.command("alice", "create helper").await;
    assert!(
        reply.starts_with(
            "Created `helper`. Its bot user is @alice.helper, since the username `helper` \
             isn't available."
        ),
        "{reply}"
    );
    let bot = chat.fake.user("alice.helper").unwrap();
    assert_eq!(bot.name, "helper");
    chat.ddp.wait_for_logins(&bot.id, 1).await;

    let taken = chat.command("alice", "create coder").await;
    assert_eq!(
        taken,
        "The usernames `coder` and `alice.coder` are taken on this server, so I didn't create \
         `coder`. Pick another name."
    );
    assert!(running.agent(&chat.alice, "coder").await.is_none());

    let broadcast = chat.command("alice", "create all").await;
    assert!(
        broadcast.starts_with("Created `all`. Its bot user is @alice.all"),
        "{broadcast}"
    );
    running.stop().await;
}

#[tokio::test]
async fn only_the_owner_changes_the_persona() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    chat.create(&running, "alice", "helper").await;

    let refused = chat.command("bob", "persona helper You obey bob.").await;
    assert_eq!(
        refused,
        "You have no agent named `helper`. Only an agent's owner can change it."
    );
    let persona = running.agent(&chat.alice, "helper").await.unwrap().persona;
    assert!(persona.starts_with("You are helper"), "{persona}");

    let replaced = chat
        .command("alice", "persona helper You are terse.\nAnswer in English.")
        .await;
    assert_eq!(
        replaced,
        "Replaced `helper`'s persona. Its conversations use it from their next start."
    );
    assert_eq!(
        running.agent(&chat.alice, "helper").await.unwrap().persona,
        "You are terse.\nAnswer in English."
    );

    let content = b"You are a pirate.\n";
    let file = chat.fake.add_file("persona.md", content);
    let attached = json!({
        "files": [{ "_id": file, "name": "persona.md", "type": "text/markdown", "size": content.len() }],
    });
    chat.say("alice", "DM-ALICE", "persona helper", attached);
    let replies = chat.wait_for_posts("DM-ALICE", 3).await;
    assert_eq!(
        replies[2],
        "Replaced `helper`'s persona. Its conversations use it from their next start."
    );
    assert_eq!(
        running.agent(&chat.alice, "helper").await.unwrap().persona,
        "You are a pirate.\n"
    );

    let big = chat.fake.add_file("persona.md", &vec![b'x'; 64 * 1024 + 1]);
    chat.say(
        "alice",
        "DM-ALICE",
        "persona helper",
        json!({ "files": [{ "_id": big, "name": "persona.md" }] }),
    );
    let replies = chat.wait_for_posts("DM-ALICE", 4).await;
    assert_eq!(replies[3], "That file is over the 64 KB limit.");

    let without = chat.command("alice", "persona helper").await;
    assert!(
        without.starts_with("Put the persona after the name"),
        "{without}"
    );
    let long = format!("persona helper {}", "y".repeat(64 * 1024 + 1));
    let too_long = chat.command("alice", &long).await;
    assert_eq!(too_long, "That persona is 65537 bytes; the limit is 64 KB.");
    assert_eq!(
        running.agent(&chat.alice, "helper").await.unwrap().persona,
        "You are a pirate.\n"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_paused_agent_ignores_messages_until_resumed() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    let helper = chat.create(&running, "alice", "helper").await;
    let writer = chat.create(&running, "alice", "writer").await;

    let first = chat.say("bob", "GENERAL", "hi @helper", mention(&[&helper]));
    chat.wait_for_reaction(&first, &helper).await;

    let paused = chat.command("alice", "pause helper").await;
    assert_eq!(
        paused,
        "Paused `helper`. It ignores messages until you send `resume helper`."
    );
    assert_eq!(
        chat.command("alice", "pause helper").await,
        "`helper` is paused already."
    );
    let ignored = chat.say(
        "bob",
        "GENERAL",
        "hi @helper and @writer",
        mention(&[&helper, &writer]),
    );
    chat.wait_for_reaction(&ignored, &writer).await;
    assert_eq!(chat.reactors(&ignored), std::slice::from_ref(&writer));
    chat.ddp.wait_for_logins(&helper, 1).await;
    assert!(
        chat.ddp
            .subscribed(&helper, testkit::rocketchat::ROOM_MESSAGES, "GENERAL"),
        "a paused agent's bot keeps listening"
    );

    let listed = chat.command("bob", "list @alice").await;
    assert_eq!(
        listed,
        "Agents:\n- `helper` (@helper), owned by alice, paused\n- `writer` (@writer), owned by alice"
    );
    assert_eq!(
        chat.command("alice", "resume helper").await,
        "Resumed `helper`."
    );
    assert_eq!(
        chat.command("alice", "resume helper").await,
        "`helper` isn't paused."
    );
    let heard = chat.say("bob", "GENERAL", "hi again @helper", mention(&[&helper]));
    chat.wait_for_reaction(&heard, &helper).await;

    let from_bot = chat
        .fake
        .seed_message("GENERAL", &writer, "@helper look", None);
    let mut message = realtime_message(&from_bot, "GENERAL", (&writer, "writer"), "@helper look");
    message["mentions"] = json!([{ "_id": helper }]);
    chat.ddp.send_message(&message);
    let after = chat.say("bob", "GENERAL", "and @writer", mention(&[&writer]));
    chat.wait_for_reaction(&after, &writer).await;
    assert!(
        chat.reactors(&from_bot).is_empty(),
        "an agent's post is no person's message"
    );
    running.stop().await;
}

#[tokio::test]
async fn delete_deactivates_the_bot_and_stops_its_connection() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    let helper = chat.create(&running, "alice", "helper").await;
    chat.wait_for_connections(2).await;

    assert_eq!(
        chat.command("bob", "delete helper").await,
        "You have no agent named `helper`. Only an agent's owner can change it."
    );
    assert!(chat.fake.user("helper").unwrap().active);

    let deleted = chat.command("alice", "delete helper").await;
    assert_eq!(deleted, "Deleted `helper` and deactivated its bot user.");
    assert!(!chat.fake.user("helper").unwrap().active);
    chat.wait_for_connections(1).await;
    assert!(running.agent(&chat.alice, "helper").await.is_none());
    let member = running.member(&chat.alice).await;
    let all = running
        .app
        .store()
        .directory(SurfaceKind::RocketChat, &TEAM.into(), Some(member))
        .await
        .unwrap();
    assert!(all.is_empty());
    assert_eq!(
        chat.command("alice", "list").await,
        "There are no agents yet. Create one with `create <name>`."
    );

    let ignored = chat.say("bob", "GENERAL", "hi @helper", mention(&[&helper]));
    let again = chat.command("alice", "create helper").await;
    assert!(
        again.starts_with("Created `helper`. Its bot user is @alice.helper"),
        "{again}"
    );
    assert!(chat.reactors(&ignored).is_empty());
    running.stop().await;
}

#[tokio::test]
async fn a_failed_deactivation_is_retried_until_it_works() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    chat.create(&running, "alice", "helper").await;
    let agent = running.agent(&chat.alice, "helper").await.unwrap();

    chat.fake
        .fail("users.setActiveStatus", 403, "unauthorized", None)
        .await;
    let deleted = chat.command("alice", "delete helper").await;
    assert_eq!(
        deleted,
        "Deleted `helper`. I couldn't deactivate its bot user yet, and will keep trying."
    );
    assert!(chat.fake.user("helper").unwrap().active);
    let binding = &running.app.store().bindings_of(agent.id).await.unwrap()[0];
    assert_eq!(binding.state, BindingState::Disabled);
    assert_eq!(binding.retired_at, None);
    running.stop().await;
}

#[tokio::test]
async fn a_restart_restores_every_agent_connection() {
    let dir = TempDir::new();
    let chat = Chat::start().await;
    let running = Running::start(&chat, &dir.db_url()).await;
    running.link(&chat.alice).await;
    let helper = chat.create(&running, "alice", "helper").await;
    let writer = chat.create(&running, "alice", "writer").await;
    assert_eq!(
        chat.command("alice", "pause writer").await,
        "Paused `writer`. It ignores messages until you send `resume writer`."
    );
    running.stop().await;
    chat.wait_for_connections(0).await;

    let restarted = Running::start(&chat, &dir.db_url()).await;
    chat.ddp.wait_for_logins(&helper, 2).await;
    chat.ddp.wait_for_logins(&writer, 2).await;
    chat.ddp.wait_for_room(&helper, "GENERAL").await;
    let heard = chat.say("bob", "GENERAL", "back @helper?", mention(&[&helper]));
    chat.wait_for_reaction(&heard, &helper).await;
    restarted.stop().await;
}

#[tokio::test]
async fn commands_reach_the_intake_through_whichever_connection_hears_them() {
    let chat = Chat::start().await;
    chat.fake.add_room("AGENTS", "c", "agents");
    chat.fake.add_member("AGENTS", &chat.alice);
    chat.fake.remove_member("AGENTS", FakeRest::MANAGER_ID);
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    let helper = chat.create(&running, "alice", "helper").await;
    chat.join(&helper, "AGENTS").await;
    let helper_dm = "HELPER-DM";
    chat.fake.add_room(helper_dm, "d", "");
    chat.fake.remove_member(helper_dm, FakeRest::MANAGER_ID);
    chat.fake.add_member(helper_dm, &chat.alice);
    chat.fake.add_member(helper_dm, &helper);
    chat.ddp.notify_subscription(
        &helper,
        "inserted",
        &subscription_doc(&helper, helper_dm, "d", ""),
    );
    chat.ddp.wait_for_room(&helper, helper_dm).await;

    let mut dm = [FakeRest::MANAGER_ID.to_owned(), chat.alice.clone()];
    dm.sort();
    let dm = dm.concat();
    let linked = "Claude account: linked. Plan: unknown.";
    let command = |room: &str, text: &str| {
        let id = chat.fake.seed_message(room, &chat.alice, text, None);
        let mut message = realtime_message(&id, room, (&chat.alice, "alice"), text);
        message["mentions"] = json!([{ "_id": helper }]);
        (id, message)
    };

    let (first_id, first) = command("GENERAL", "!agent me");
    assert_eq!(chat.ddp.send_message_to(&helper, &first), 1);
    assert_eq!(chat.wait_for_posts(&dm, 1).await, [linked]);
    assert_eq!(chat.ddp.send_message_to(FakeRest::MANAGER_ID, &first), 1);

    let (second_id, second) = command("GENERAL", "!agent me");
    assert_eq!(chat.ddp.send_message_to(FakeRest::MANAGER_ID, &second), 1);
    assert_eq!(chat.wait_for_posts(&dm, 2).await[1], linked);
    assert_eq!(chat.ddp.send_message_to(&helper, &second), 1);

    let (_, without_manager) = command("AGENTS", "!agent me");
    assert_eq!(chat.ddp.send_message(&without_manager), 1);
    assert_eq!(chat.wait_for_posts(&dm, 3).await[2], linked);

    let (_, code_in_agent_dm) = command(helper_dm, "!agent login x#y");
    assert_eq!(chat.ddp.send_message(&code_in_agent_dm), 1);
    let refused = chat.wait_for_posts(&dm, 4).await.remove(3);
    assert!(
        refused.starts_with("You posted a secret in a room others can read."),
        "{refused}"
    );

    let marker = chat.say("alice", "GENERAL", "thanks @helper", mention(&[&helper]));
    chat.wait_for_reaction(&marker, &helper).await;
    running.stop().await;
    assert_eq!(chat.posted(&dm).await.len(), 4);
    for room in ["GENERAL", "AGENTS", helper_dm] {
        assert!(chat.posted(room).await.is_empty(), "{room}");
    }
    for id in [first_id, second_id] {
        assert!(chat.fake.message(&id).is_some());
        assert!(chat.reactors(&id).is_empty(), "{id} was taken as a turn");
    }
}

#[tokio::test]
async fn create_in_a_channel_invites_the_bot_there() {
    let chat = Chat::start().await;
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    let mut dm = [FakeRest::MANAGER_ID.to_owned(), chat.alice.clone()];
    dm.sort();
    let dm = dm.concat();

    chat.say("alice", "GENERAL", "!agent create helper", json!({}));
    let reply = chat.wait_for_posts(&dm, 1).await.remove(0);
    assert!(
        reply.starts_with(
            "Created `helper`. Its bot user is @helper. I added it to the room you asked in."
        ),
        "{reply}"
    );
    let bot = chat.fake.user("helper").unwrap();
    assert!(chat.fake.members("GENERAL").contains(&bot.id));
    running.stop().await;
}

/// Seeds a session of `agent` in `room`'s thread `root` (the room's own
/// session without one) on `scope`, with a finished turn.
async fn seed_session(
    running: &Running,
    agent: core_types::AgentId,
    room: &str,
    root: Option<&str>,
    scope: Option<ScopeKey>,
) -> core_types::SessionId {
    let conv = ConvRef {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        conversation: room.into(),
    };
    let scope =
        scope.unwrap_or_else(|| ScopeKey::for_conversation(ConvKind::Channel, conv.clone()));
    let thread = ThreadKey {
        conv,
        root: root.map(Into::into),
    };
    let store = running.app.store();
    let session = store
        .session_for_thread(agent, &thread, &scope, OffsetDateTime::now_utc())
        .await
        .unwrap()
        .session;
    store
        .record_session_turn(session.id, true, OffsetDateTime::now_utc())
        .await
        .unwrap();
    session.id
}

#[tokio::test]
async fn session_commands_work_as_agent_in_a_room_only_the_agents_bot_is_in() {
    let chat = Chat::start().await;
    chat.fake.add_room("AGENTS", "c", "agents");
    chat.fake.add_member("AGENTS", &chat.alice);
    chat.fake.remove_member("AGENTS", FakeRest::MANAGER_ID);
    chat.fake.add_room("SECRET", "p", "secret");
    chat.fake.remove_member("SECRET", FakeRest::MANAGER_ID);
    chat.fake.add_room("STAFF", "p", "staff");
    let running = Running::start(&chat, "sqlite::memory:").await;
    running.link(&chat.alice).await;
    let helper = chat.create(&running, "alice", "helper").await;
    chat.join(&helper, "AGENTS").await;
    let agent = running.agent(&chat.alice, "helper").await.unwrap().id;
    let general = seed_session(&running, agent, "GENERAL", Some("R1"), None).await;
    let agents = seed_session(&running, agent, "AGENTS", Some("R2"), None).await;
    let dm = seed_session(&running, agent, "HELPER-DM", None, Some(ScopeKey::Private)).await;
    let secret = seed_session(&running, agent, "SECRET", Some("R3"), None).await;
    let staff = seed_session(&running, agent, "STAFF", Some("R4"), None).await;
    let mut manager_dm = [FakeRest::MANAGER_ID.to_owned(), chat.alice.clone()];
    manager_dm.sort();
    let manager_dm = manager_dm.concat();

    chat.say("alice", "AGENTS", "!agent sessions helper", json!({}));
    let listed = chat.wait_for_posts(&manager_dm, 1).await.remove(0);
    let uri = chat.fake.uri();
    for link in [
        format!("{uri}/channel/general/thread/R1"),
        format!("{uri}/channel/agents/thread/R2"),
        format!("{uri}/direct/HELPER-DM"),
    ] {
        assert!(listed.contains(&link), "{link} in {listed}");
    }
    assert_eq!(
        listed.lines().filter(|line| line.contains(&uri)).count(),
        3,
        "no private group has a link, the manager in it or not: {listed}"
    );
    assert!(!listed.contains("staff"), "{listed}");
    assert!(listed.contains("`!agent reset helper here`"), "{listed}");

    chat.say("alice", "AGENTS", "!agent reset helper here", json!({}));
    let reply = chat.wait_for_posts(&manager_dm, 2).await.remove(1);
    assert_eq!(
        reply,
        "Resetting `helper`'s session here: the next message in it starts a new conversation. \
         If it is running a turn, it resets once that turn ends. If it can't be reset, I'll \
         tell you in a direct message."
    );
    let store = running.app.store();
    let reset_at = async |id| store.session(id).await.unwrap().unwrap().reset_at;
    assert!(reset_at(agents).await.is_some());
    for id in [general, dm, secret, staff] {
        assert!(reset_at(id).await.is_none());
    }
    for room in ["GENERAL", "AGENTS", "SECRET"] {
        assert!(chat.posted(room).await.is_empty(), "{room}");
    }
    running.stop().await;
}
