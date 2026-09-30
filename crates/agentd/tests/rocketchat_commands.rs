//! Commands over Rocket.Chat end to end: `serve` with the manager bot's
//! connection to a fake Rocket.Chat, and wiremock OAuth endpoints.

mod common;

use std::time::Duration;

use agentd::server::{Routers, Server};
use agentd::{App, Config};
use serde_json::{Value, json};
use testkit::rocketchat::{FakeDdp, FakeRest, realtime_message};
use tokio::sync::oneshot;
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
    let server = Server::bind(app.clone(), Routers::new(&app)).await.unwrap();
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
