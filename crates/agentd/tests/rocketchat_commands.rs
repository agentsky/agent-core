//! Commands over Rocket.Chat end to end: the manager bot's and an agent
//! bot's connections to a fake Rocket.Chat feeding the command intake, and
//! wiremock OAuth endpoints.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agentd::commands::intake::CommandIntake;
use agentd::commands::rocketchat::{CommandFeed, RocketChatDms, StoreDedup, listen};
use agentd::commands::{Commands, ManagerBot, Replies};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use async_trait::async_trait;
use auth::{Auth, OAuthConfig};
use core_types::{
    AgentId, Binding, BindingId, InboundEvent, MemberKey, SendError, Sender, Sink, SurfaceKind,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use store::{Sealer, Store};
use surface_rocketchat::rest::{Credentials, NewBotUser, RestClient};
use surface_rocketchat::{BotRoles, RocketChatConfig, RocketChatSurface};
use testkit::rocketchat::{FakeDdp, FakeRest, realtime_message};
use tokio::sync::{mpsc, oneshot, watch};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{CONFIG, master_key};

const CODE: &str = "E2E-SECRET-CODE-91c3";

/// The text of every `chat.postMessage` to `room`, oldest first.
async fn posted(fake: &FakeRest, room: &str) -> Vec<String> {
    fake.requests("chat.postMessage")
        .await
        .into_iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["roomId"] == room)
        .filter_map(|body| body["text"].as_str().map(str::to_owned))
        .collect()
}

async fn wait_for_posts(fake: &FakeRest, room: &str, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let texts = posted(fake, room).await;
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

#[tokio::test]
async fn the_manager_bot_runs_dm_and_channel_commands() {
    let fake = FakeRest::start().await;
    let ddp = FakeDdp::start().await;
    ddp.add_token(FakeRest::MANAGER_TOKEN, FakeRest::MANAGER_ID);
    let alice = fake.add_user("alice");
    fake.add_room("DM1", "d", "");
    fake.add_member("DM1", &alice);
    fake.add_room("GENERAL", "c", "general");
    fake.add_member("GENERAL", &alice);
    let oauth = MockServer::start().await;

    let text = format!(
        "{CONFIG}\n[claude_oauth]\ntoken_url = \"{0}/token\"\nprofile_url = \"{0}/profile\"\n\
         revoke_url = \"{0}/revoke\"\n\n[rocketchat]\nbase_url = \"{1}\"\n\
         websocket_url = \"{2}\"\nteam = \"chat.example\"\nmanager_user_id = \"{3}\"\n",
        oauth.uri(),
        fake.uri(),
        ddp.url(),
        FakeRest::MANAGER_ID,
    );
    let env = vec![
        ("AGENTD_MASTER_KEY".to_owned(), master_key()),
        (
            "AGENTD_RC_MANAGER_TOKEN".to_owned(),
            FakeRest::MANAGER_TOKEN.to_owned(),
        ),
    ];
    let app = App::open(Config::parse(&text, env).unwrap()).await.unwrap();
    assert!(app.rocketchat().is_some());
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
    ddp.wait_for_room(FakeRest::MANAGER_ID, "DM1").await;
    ddp.wait_for_room(FakeRest::MANAGER_ID, "GENERAL").await;

    ddp.send_message(&realtime_message("m-1", "DM1", (&alice, "alice"), "login"));
    let started = wait_for_posts(&fake, "DM1", 1).await.remove(0);
    let at = started.find("state=").unwrap() + "state=".len();
    let state: String = started[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_partial_json(json!({"code": CODE, "state": state})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a",
            "refresh_token": "r",
            "expires_in": 28800,
        })))
        .mount(&oauth)
        .await;
    Mock::given(method("GET"))
        .and(path("/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "organization": {"organization_type": "claude_team"},
        })))
        .mount(&oauth)
        .await;
    let pasted = format!("login {CODE}#{state}");
    ddp.send_message(&realtime_message("m-2", "DM1", (&alice, "alice"), &pasted));
    let replies = wait_for_posts(&fake, "DM1", 2).await;
    assert_eq!(
        replies[1],
        "Your Claude account is linked. Plan: Claude Team."
    );

    ddp.send_message(&realtime_message(
        "m-3",
        "GENERAL",
        (&alice, "alice"),
        "!agent me",
    ));
    ddp.send_message(&realtime_message(
        "m-3",
        "GENERAL",
        (&alice, "alice"),
        "!agent me",
    ));
    let mut dm = [FakeRest::MANAGER_ID.to_owned(), alice.clone()];
    dm.sort();
    let dm = dm.concat();
    let replies = wait_for_posts(&fake, &dm, 1).await;
    assert_eq!(replies, ["Claude account: linked. Plan: Claude Team."]);
    assert!(posted(&fake, "GENERAL").await.is_empty());

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(posted(&fake, &dm).await.len(), 1);
}

struct Onward(mpsc::UnboundedSender<InboundEvent>);

#[async_trait]
impl Sink<InboundEvent> for Onward {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        self.0.send(event).map_err(|_| SendError)
    }
}

fn binding(user: &str, agent: Option<AgentId>) -> Binding {
    Binding {
        id: BindingId::new_v4(),
        agent,
        bot: MemberKey {
            surface: SurfaceKind::RocketChat,
            team: "chat.example".into(),
            user: user.into(),
        },
    }
}

fn surface(
    fake: &FakeRest,
    ddp: &FakeDdp,
    credentials: Credentials,
    dedup: &Arc<StoreDedup>,
    bots: &BotRoles,
) -> Arc<RocketChatSurface> {
    let mut config = RocketChatConfig::new(fake.uri(), "chat.example".into(), credentials);
    config.websocket_url = Some(ddp.url());
    Arc::new(RocketChatSurface::new(config, dedup.clone(), bots.clone()).unwrap())
}

#[tokio::test]
async fn every_bot_connection_feeds_commands_to_the_one_intake() {
    let fake = FakeRest::start().await;
    let ddp = FakeDdp::start().await;
    ddp.add_token(FakeRest::MANAGER_TOKEN, FakeRest::MANAGER_ID);
    let manager_credentials = Credentials {
        user_id: FakeRest::MANAGER_ID.into(),
        token: SecretString::from(FakeRest::MANAGER_TOKEN),
    };
    let manager_rest = RestClient::new(&fake.uri(), manager_credentials.clone()).unwrap();
    let new = NewBotUser {
        username: "helper",
        name: "helper",
        email: "bot@bots.invalid",
    };
    let (helper, password) = manager_rest.create_bot_user(&new).await.unwrap();
    let helper_credentials = manager_rest
        .issue_bot_token(&helper.username, password, "agentd")
        .await
        .unwrap();
    ddp.add_token(helper_credentials.token.expose_secret(), helper.id.as_str());
    let helper = helper.id.to_string();
    let alice = fake.add_user("alice");
    fake.add_room("SHARED", "c", "shared");
    fake.add_room("AGENTS", "c", "agents");
    fake.add_room("HELPER-DM", "d", "");
    for room in ["SHARED", "AGENTS", "HELPER-DM"] {
        fake.add_member(room, &alice);
        fake.add_member(room, &helper);
    }
    fake.remove_member("AGENTS", FakeRest::MANAGER_ID);
    fake.remove_member("HELPER-DM", FakeRest::MANAGER_ID);

    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let auth = Arc::new(Auth::new(OAuthConfig::default(), store.clone()).unwrap());
    let dedup = Arc::new(StoreDedup(store.clone()));
    let bots = BotRoles::new(manager_rest.clone());
    let manager_surface = surface(&fake, &ddp, manager_credentials, &dedup, &bots);
    let helper_surface = surface(&fake, &ddp, helper_credentials, &dedup, &bots);
    let manager = binding(FakeRest::MANAGER_ID, None);
    let bot = Arc::new(ManagerBot::new(
        manager.bot.clone(),
        manager_surface.clone(),
        Arc::new(RocketChatDms(manager_rest)),
    ));
    let commands = Commands::new(store, auth, Replies::new(Some(bot)), None, None);
    let (intake, submitter) = CommandIntake::new(commands);
    let feed = CommandFeed::new(submitter, manager.clone());
    let (onward_tx, mut onward) = mpsc::unbounded_channel();
    let (stop, stopping) = watch::channel(false);
    let manager_connection = tokio::spawn(listen(
        manager_surface,
        manager.clone(),
        feed.clone().into_sender(None),
        stopping.clone(),
    ));
    let helper_connection = tokio::spawn(listen(
        helper_surface,
        binding(&helper, Some(AgentId::new_v4())),
        feed.into_sender(Some(Sender::new(Onward(onward_tx)))),
        stopping,
    ));
    let intake = tokio::spawn(intake.run());
    ddp.wait_for_room(FakeRest::MANAGER_ID, "SHARED").await;
    for room in ["SHARED", "AGENTS", "HELPER-DM"] {
        ddp.wait_for_room(&helper, room).await;
    }
    let mut dm = [FakeRest::MANAGER_ID.to_owned(), alice.clone()];
    dm.sort();
    let dm = dm.concat();
    let from_alice =
        |id: &str, room: &str, text: &str| realtime_message(id, room, (&alice, "alice"), text);
    let not_linked = "Claude account: not linked. Send `login` to link one.";

    let first = from_alice("m-1", "SHARED", "!agent me");
    assert_eq!(ddp.send_message_to(&helper, &first), 1);
    assert_eq!(wait_for_posts(&fake, &dm, 1).await, [not_linked]);
    assert_eq!(ddp.send_message_to(FakeRest::MANAGER_ID, &first), 1);

    let second = from_alice("m-2", "SHARED", "!agent me");
    assert_eq!(ddp.send_message_to(FakeRest::MANAGER_ID, &second), 1);
    assert_eq!(wait_for_posts(&fake, &dm, 2).await[1], not_linked);
    assert_eq!(ddp.send_message_to(&helper, &second), 1);

    let without_manager = from_alice("m-3", "AGENTS", "!agent me");
    assert_eq!(ddp.send_message(&without_manager), 1);
    assert_eq!(wait_for_posts(&fake, &dm, 3).await[2], not_linked);

    let code_in_agent_dm = from_alice("m-4", "HELPER-DM", "!agent login x#y");
    assert_eq!(ddp.send_message(&code_in_agent_dm), 1);
    let refused = wait_for_posts(&fake, &dm, 4).await.remove(3);
    assert!(
        refused.starts_with("You posted a secret in a room others can read."),
        "{refused}"
    );

    let chat = from_alice("m-5", "SHARED", "hello helper");
    assert_eq!(ddp.send_message_to(&helper, &chat), 1);
    let passed_on = tokio::time::timeout(Duration::from_secs(10), onward.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(passed_on.text, "hello helper");

    stop.send_replace(true);
    for connection in [manager_connection, helper_connection] {
        tokio::time::timeout(Duration::from_secs(10), connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), intake)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(posted(&fake, &dm).await.len(), 4);
    for room in ["SHARED", "AGENTS", "HELPER-DM"] {
        assert!(posted(&fake, room).await.is_empty(), "{room}");
    }
    assert!(onward.try_recv().is_err());
}
