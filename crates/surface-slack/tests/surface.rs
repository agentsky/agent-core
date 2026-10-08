//! `SlackSurface` against a wiremock Slack.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use core_types::{
    Binding, BindingId, ConvKind, ConvRef, Cursor, InboundEvent, LengthUnit, MemberKey, MsgRef,
    OutFile, Outside, ReplyTarget, SendError, Sender, Sharing, Sink, Surface, SurfaceError,
    SurfaceKind, TeamId, ThreadKey, UserId,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use surface_slack::directory::{ConvInfo, Membership};
use surface_slack::normalize::{self, Context, MAX_ID_TAIL};
use surface_slack::surface::CAPS;
use surface_slack::web::MAX_CONNECTED_TEAMS;
use surface_slack::{SlackClient, SlackSurface, TeamDirectory};
use testkit::slack::{BOT_USER, CHANNEL, HOME_ORG, OUTSIDE_TEAM, SHARED_CHANNEL, TEAM, USER};
use testkit::{Held, TempDir};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "xoxb-surface-test";

async fn setup() -> (MockServer, SlackSurface) {
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let directory = Arc::new(TeamDirectory::new(TEAM.into()));
    let surface = SlackSurface::new(client.bot(SecretString::from(TOKEN)), directory);
    (server, surface)
}

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

async fn mount(server: &MockServer, name: &str, response: impl Respond + 'static) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{name}")))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn form(request: &Request) -> HashMap<String, String> {
    serde_urlencoded::from_bytes(&request.body).unwrap()
}

fn conv() -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: TEAM.into(),
        conversation: CHANNEL.into(),
    }
}

fn thread(root: &str) -> ReplyTarget {
    ReplyTarget {
        conv: conv(),
        thread_root: Some(root.into()),
    }
}

#[tokio::test]
async fn caps_match_the_plan() {
    let (_server, surface) = setup().await;
    let caps = surface.caps();
    assert_eq!(caps, CAPS);
    assert_eq!(caps.message_limit.max, 3000);
    assert_eq!(caps.message_limit.unit, LengthUnit::Chars);
    assert!(caps.supports_edit);
    assert!(caps.supports_buttons);
    assert!(caps.supports_threads);
    assert!(caps.per_binding_delivery);
}

#[tokio::test]
async fn render_converts_to_mrkdwn_and_splits_at_the_slack_limit() {
    let (_server, surface) = setup().await;
    let chunks = surface.render("**Done** & <!here>");
    assert_eq!(chunks, ["*Done* &amp; @\u{200B}here"]);

    let paragraph = format!("{}\n\n", "word ".repeat(200));
    let long = paragraph.repeat(8);
    let chunks = surface.render(&long);
    assert!(chunks.len() > 1, "{}", chunks.len());
    for chunk in &chunks {
        assert!(chunk.chars().count() <= 3000);
    }
    assert!(surface.render("").is_empty());
}

#[tokio::test]
async fn render_resolves_mentions_from_the_member_cache() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "users.list",
        ok(json!({
            "members": [
                {"id": USER, "name": "ada", "profile": {"display_name": "Ada", "real_name": "Ada Lovelace"}},
                {"id": BOT_USER, "name": "helper", "is_bot": true, "profile": {"real_name": "helper"}},
                {"id": "U0GONE001", "name": "gone", "deleted": true, "profile": {"display_name": "Gone"}},
            ],
            "response_metadata": {"next_cursor": ""},
        })),
    )
    .await;
    assert_eq!(surface.render("hi @Ada"), ["hi @Ada"]);
    let members = surface.refresh_members().await.unwrap();
    assert_eq!(members.lookup("ada lovelace"), Some(&UserId::from(USER)));
    assert_eq!(
        surface.render("hi @Ada, ask @helper, not @Gone"),
        [format!("hi <@{USER}>, ask <@{BOT_USER}>, not @Gone")]
    );
    surface.refresh_members().await.unwrap();
    assert_eq!(requests(&server).await.len(), 1, "the TTL holds");
}

#[tokio::test]
async fn render_refreshes_a_stale_member_cache_in_the_background() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(ok(
            json!({"members": [{"id": USER, "name": "ada", "profile": {"display_name": "Ada"}}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(surface.render("@ada"), ["@ada"]);
    let mut rendered = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        rendered = surface.render("@ada");
        if rendered != ["@ada"] {
            break;
        }
    }
    assert_eq!(rendered, [format!("<@{USER}>")]);
}

#[tokio::test]
async fn the_member_list_is_read_with_the_members_api_token_only() {
    let (server, agent) = setup().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let agent = agent.with_members_api(client.bot(SecretString::from("xoxb-manager")));
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(header("authorization", "Bearer xoxb-manager"))
        .respond_with(ok(
            json!({"members": [{"id": USER, "name": "ada", "profile": {"display_name": "Ada"}}]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "token_revoked"})),
        )
        .mount(&server)
        .await;
    let members = agent.refresh_members().await.unwrap();
    assert_eq!(members.lookup("ada"), Some(&UserId::from(USER)));
    let seen = requests(&server).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].headers["authorization"], "Bearer xoxb-manager");
}

#[test]
fn render_outside_a_runtime_uses_the_cache_as_is() {
    let client = SlackClient::new("http://127.0.0.1:9/api/").unwrap();
    let directory = Arc::new(TeamDirectory::new(TEAM.into()));
    let surface = SlackSurface::new(client.bot(SecretString::from(TOKEN)), directory);
    assert_eq!(surface.render("@ada"), ["@ada"]);
}

#[tokio::test]
async fn members_refresh_after_the_ttl_and_survive_a_failed_refresh() {
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let directory = Arc::new(TeamDirectory::new(TEAM.into()).with_ttl(Duration::ZERO));
    let surface = SlackSurface::new(client.bot(SecretString::from(TOKEN)), directory);
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(ok(
            json!({"members": [{"id": USER, "name": "ada", "profile": {"display_name": "Ada"}}]}),
        ))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    mount(
        &server,
        "users.list",
        ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "fatal_error"})),
    )
    .await;
    surface.refresh_members().await.unwrap();
    surface.refresh_members().await.unwrap();
    assert_eq!(requests(&server).await.len(), 2);
    let stale = surface.refresh_members().await.unwrap();
    assert_eq!(stale.lookup("ada"), Some(&UserId::from(USER)));
    assert_eq!(requests(&server).await.len(), 3);
    assert_eq!(surface.render("@ada"), [format!("<@{USER}>")]);
}

#[tokio::test]
async fn concurrent_member_refreshes_share_one_users_list() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(
            ok(json!({"members": [{"id": USER, "name": "ada", "profile": {"display_name": "Ada"}}]}))
                .set_delay(Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (a, b) = tokio::join!(surface.refresh_members(), surface.refresh_members());
    assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
}

#[tokio::test]
async fn a_first_member_refresh_that_fails_is_an_error() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "users.list",
        ResponseTemplate::new(200)
            .set_body_json(json!({"ok": false, "error": "missing_scope", "needed": "users:read"})),
    )
    .await;
    let err = surface.refresh_members().await.unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Forbidden("missing_scope (needs users:read)".into())
    );
    assert!(surface.directory().members().is_empty());
    assert_eq!(surface.refresh_members().await.unwrap_err(), err);
    assert_eq!(requests(&server).await.len(), 1, "the retry waits");
}

#[tokio::test]
async fn renders_after_a_failed_first_member_refresh_wait_to_retry() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "users.list",
        ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "fatal_error"})),
    )
    .await;
    for _ in 0..8 {
        assert_eq!(surface.render("@ada"), ["@ada"]);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(requests(&server).await.len(), 1);
}

fn members_with_bot() -> ResponseTemplate {
    ok(json!({
        "members": [
            {"id": USER, "name": "ada", "profile": {"display_name": "Ada"}},
            {"id": BOT_USER, "name": "helper", "is_bot": true, "profile": {"real_name": "helper"}},
        ],
    }))
}

async fn users_list_calls(server: &MockServer) -> usize {
    requests(server)
        .await
        .iter()
        .filter(|request| request.url.path() == "/api/users.list")
        .count()
}

#[tokio::test]
async fn a_managed_bot_the_member_list_lacks_makes_it_stale() {
    let (server, surface) = setup().await;
    mount(&server, "users.list", members_with_bot()).await;
    surface.refresh_members().await.unwrap();
    assert_eq!(users_list_calls(&server).await, 1);

    surface
        .directory()
        .set_managed_bots([UserId::from(BOT_USER)]);
    surface.refresh_members().await.unwrap();
    assert_eq!(
        users_list_calls(&server).await,
        1,
        "a known bot keeps the list"
    );

    surface
        .directory()
        .set_managed_bots([UserId::from(BOT_USER), UserId::from("U0NEWBOT1")]);
    surface.refresh_members().await.unwrap();
    assert_eq!(
        users_list_calls(&server).await,
        2,
        "a new bot reads it again"
    );
    surface.refresh_members().await.unwrap();
    assert_eq!(
        users_list_calls(&server).await,
        2,
        "once, even if still absent"
    );

    surface
        .directory()
        .set_managed_bots([UserId::from("U0NEWBOT2")]);
    assert_eq!(surface.render("@ada"), [format!("<@{USER}>")]);
    for _ in 0..100 {
        if users_list_calls(&server).await == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(users_list_calls(&server).await, 3, "render refreshes too");
}

#[tokio::test]
async fn a_new_managed_bot_waits_out_a_failed_refresh() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(members_with_bot())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        "users.list",
        ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "fatal_error"})),
    )
    .await;
    surface.refresh_members().await.unwrap();
    surface
        .directory()
        .set_managed_bots([UserId::from("U0NEWBOT1")]);
    let stale = surface.refresh_members().await.unwrap();
    assert_eq!(stale.lookup("ada"), Some(&UserId::from(USER)));
    assert_eq!(users_list_calls(&server).await, 2);
    surface
        .directory()
        .set_managed_bots([UserId::from("U0NEWBOT2")]);
    surface.refresh_members().await.unwrap();
    assert_eq!(users_list_calls(&server).await, 2, "the retry wait holds");
}

#[tokio::test]
async fn a_managed_bot_set_during_a_refresh_leaves_the_result_stale() {
    let (server, surface) = setup().await;
    let (held, mut hold) = Held::new(members_with_bot());
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(held)
        .mount(&server)
        .await;
    let refreshing = tokio::spawn({
        let surface = surface.clone();
        async move { surface.refresh_members().await.map(drop) }
    });
    hold.arrived().await;
    surface
        .directory()
        .set_managed_bots([UserId::from("U0NEWBOT1")]);
    hold.release();
    refreshing.await.unwrap().unwrap();
    surface.refresh_members().await.unwrap();
    assert_eq!(users_list_calls(&server).await, 2);
}

#[tokio::test]
async fn managed_agents_keep_names_humans_share() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "users.list",
        ok(json!({
            "members": [
                {"id": USER, "name": "helper", "profile": {"display_name": "Ada", "real_name": "Ada Lovelace"}},
                {"id": "U0HUMAN02", "name": "grace", "profile": {"display_name": "Scout", "real_name": "Grace Hopper"}},
                {"id": BOT_USER, "name": "helper", "is_bot": true, "profile": {"real_name": "helper"}},
                {"id": "U0SCOUT01", "name": "scout", "is_bot": true, "profile": {"real_name": "scout"}},
            ],
        })),
    )
    .await;
    surface.refresh_members().await.unwrap();
    assert_eq!(
        surface.render("@helper and @scout"),
        [format!("<@{BOT_USER}> and @scout")]
    );
    surface
        .directory()
        .set_managed_bots([UserId::from(BOT_USER), UserId::from("U0SCOUT01")]);
    assert_eq!(
        surface.render("@helper and @scout, cc @Ada"),
        [format!("<@{BOT_USER}> and <@U0SCOUT01>, cc <@{USER}>")]
    );
    let debug = format!("{surface:?}");
    assert!(debug.contains(TEAM), "{debug}");
    assert!(!debug.contains(USER) && !debug.contains("Ada"), "{debug}");
    assert!(!debug.contains(TOKEN), "{debug}");
}

#[tokio::test]
async fn post_sends_one_chunk_in_the_thread() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "chat.postMessage",
        ok(json!({"channel": CHANNEL, "ts": "1727697700.000200"})),
    )
    .await;
    let chunks = surface.render("Hello **world**");
    assert_eq!(chunks.len(), 1);
    let posted = surface
        .post(&thread("1727697600.000100"), &chunks[0])
        .await
        .unwrap();
    assert_eq!(
        posted.msg,
        MsgRef {
            conv: conv(),
            id: "1727697700.000200".into()
        }
    );
    assert!(posted.mentions.is_empty());
    let sent = requests(&server).await;
    let post = sent
        .iter()
        .find(|request| request.url.path() == "/api/chat.postMessage")
        .unwrap();
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["text"], "Hello *world*");
    assert_eq!(body["thread_ts"], "1727697600.000100");
    assert_eq!(body["channel"], CHANNEL);
    assert_eq!(body["unfurl_links"], false);
    assert!(body.get("link_names").is_none());
    assert!(body.get("parse").is_none());
    let mentioning = surface
        .post(
            &thread("1727697600.000100"),
            "<@U0HELPER> and <@U0HELPER|helper> <!channel>",
        )
        .await
        .unwrap();
    assert_eq!(mentioning.mentions, [UserId::from("U0HELPER")]);
    let in_code = surface
        .post(
            &thread("1727697600.000100"),
            "`a <@U0HELPER>` and `x`y` <@U0OTHER> `z`, as a cut can leave it",
        )
        .await
        .unwrap();
    assert!(
        in_code.mentions.is_empty(),
        "Slack shows these as code, so they hand off to no one"
    );
}

#[tokio::test]
async fn edit_react_and_ephemeral_use_the_message_conversation() {
    let (server, surface) = setup().await;
    mount(&server, "chat.update", ok(json!({}))).await;
    mount(&server, "reactions.add", ok(json!({}))).await;
    mount(&server, "reactions.remove", ok(json!({}))).await;
    mount(
        &server,
        "chat.postEphemeral",
        ok(json!({"message_ts": "9.9"})),
    )
    .await;
    let msg = MsgRef {
        conv: conv(),
        id: "1.2".into(),
    };
    surface.edit(&msg, "edited").await.unwrap();
    surface.react(&msg, "eyes").await.unwrap();
    surface
        .post_ephemeral(&thread("1.1"), &UserId::from(USER), "psst")
        .await
        .unwrap();
    surface.unreact(&msg, "eyes").await.unwrap();
    let sent = requests(&server).await;
    let update: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        update,
        json!({"channel": CHANNEL, "ts": "1.2", "text": "edited"})
    );
    let reaction = form(&sent[1]);
    assert_eq!(reaction["name"], "eyes");
    assert_eq!(reaction["timestamp"], "1.2");
    assert!(sent[3].url.path().ends_with("reactions.remove"));
    assert_eq!(form(&sent[3])["name"], "eyes");
    let ephemeral: Value = serde_json::from_slice(&sent[2].body).unwrap();
    assert_eq!(ephemeral["user"], USER);
    assert_eq!(ephemeral["thread_ts"], "1.1");
}

#[tokio::test]
async fn can_post_only_where_the_bot_is_a_member_and_trusts_a_yes_until_asked_now() {
    let (server, surface) = setup().await;
    let elsewhere = ConvRef {
        team: "T0OTHER01".into(),
        ..conv()
    };
    assert!(surface.can_post(&elsewhere).await.is_err());
    mount(
        &server,
        "conversations.info",
        ok(json!({"channel": {"id": CHANNEL, "is_channel": true, "is_member": false}})),
    )
    .await;
    assert!(!surface.can_post(&conv()).await.unwrap());
    assert!(!surface.can_post(&conv()).await.unwrap());
    server.reset().await;
    mount(
        &server,
        "conversations.info",
        ok(json!({"channel": {"id": CHANNEL, "is_channel": true, "is_member": true}})),
    )
    .await;
    assert!(surface.can_post(&conv()).await.unwrap());
    assert!(surface.can_post(&conv()).await.unwrap());
    let asked = |requests: Vec<Request>| {
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/conversations.info")
            .count()
    };
    assert_eq!(
        asked(requests(&server).await),
        1,
        "a yes is trusted for a while"
    );
    assert!(surface.can_post_now(&conv()).await.unwrap());
    assert_eq!(
        asked(requests(&server).await),
        2,
        "asked now, Slack is asked whatever it said lately"
    );

    for channel in [
        json!({"id": CHANNEL, "is_channel": true, "is_member": true, "is_archived": true}),
        json!({"id": "C0OTHER01", "is_channel": true, "is_member": true}),
    ] {
        server.reset().await;
        mount(
            &server,
            "conversations.info",
            ok(json!({ "channel": channel })),
        )
        .await;
        assert!(
            !surface.can_post_now(&conv()).await.unwrap(),
            "{channel}: not a conversation the bot can post in"
        );
        assert!(
            !surface.can_post(&conv()).await.unwrap(),
            "{channel}: the no dropped the yes kept before"
        );
    }

    for error in ["channel_not_found", "invalid_auth", "missing_scope"] {
        server.reset().await;
        mount(
            &server,
            "conversations.info",
            ok(json!({"channel": {"id": CHANNEL, "is_channel": true, "is_member": true}})),
        )
        .await;
        assert!(surface.can_post_now(&conv()).await.unwrap());
        server.reset().await;
        mount(
            &server,
            "conversations.info",
            ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": error})),
        )
        .await;
        assert!(
            !surface.can_post_now(&conv()).await.unwrap(),
            "{error}: Slack won't let the bot post there"
        );
        assert!(
            !surface.can_post(&conv()).await.unwrap(),
            "{error}: the no dropped the yes kept before"
        );
    }
    server.reset().await;
    mount(
        &server,
        "conversations.info",
        ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "fatal_error"})),
    )
    .await;
    assert!(
        surface.can_post_now(&conv()).await.is_err(),
        "a failure that may pass stays an error"
    );
}

#[tokio::test]
async fn upload_shares_into_the_thread() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "files.getUploadURLExternal",
        ok(json!({"upload_url": format!("{}/upload/v1/x", server.uri()), "file_id": "F1"})),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/upload/v1/x"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    mount(
        &server,
        "files.completeUploadExternal",
        ok(json!({"files": []})),
    )
    .await;
    let dir = TempDir::new("surface-slack");
    std::fs::write(dir.join("out.txt"), "data").unwrap();
    let file = OutFile {
        name: "out.txt".into(),
        path: dir.join("out.txt"),
    };
    surface.upload(&thread("1.1"), &[file]).await.unwrap();
    surface.upload(&thread("1.1"), &[]).await.unwrap();
    let sent = requests(&server).await;
    assert_eq!(sent.len(), 3);
    let complete = form(&sent[2]);
    assert_eq!(complete["channel_id"], CHANNEL);
    assert_eq!(complete["thread_ts"], "1.1");
}

#[tokio::test]
async fn a_conversation_in_another_workspace_is_refused_before_sending() {
    let (server, surface) = setup().await;
    let elsewhere = ConvRef {
        surface: SurfaceKind::Slack,
        team: "T0OTHER01".into(),
        conversation: CHANNEL.into(),
    };
    let rocket = ConvRef {
        surface: SurfaceKind::RocketChat,
        ..conv()
    };
    for conv in [elsewhere, rocket] {
        let to = ReplyTarget {
            conv: conv.clone(),
            thread_root: None,
        };
        let msg = MsgRef {
            conv: conv.clone(),
            id: "1.1".into(),
        };
        assert!(matches!(
            surface.post(&to, "x").await,
            Err(SurfaceError::Api(_))
        ));
        assert!(matches!(
            surface.edit(&msg, "x").await,
            Err(SurfaceError::Api(_))
        ));
        assert!(matches!(
            surface.react(&msg, "x").await,
            Err(SurfaceError::Api(_))
        ));
        assert!(matches!(
            surface.unreact(&msg, "x").await,
            Err(SurfaceError::Api(_))
        ));
        assert!(matches!(
            surface.can_post(&conv).await,
            Err(SurfaceError::Api(_))
        ));
        assert!(matches!(
            surface.upload(&to, &[]).await,
            Err(SurfaceError::Api(_))
        ));
        let key = ThreadKey { conv, root: None };
        assert!(matches!(
            surface.history(&key, None, 5).await,
            Err(SurfaceError::Api(_))
        ));
    }
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn events_come_through_the_ingress_not_the_surface() {
    struct Drop;
    #[async_trait::async_trait]
    impl Sink<InboundEvent> for Drop {
        async fn send(&self, _: InboundEvent) -> Result<(), SendError> {
            Ok(())
        }
    }
    let (_server, surface) = setup().await;
    let binding = Binding {
        id: BindingId::new_v4(),
        agent: None,
        bot: MemberKey {
            surface: SurfaceKind::Slack,
            team: TEAM.into(),
            user: BOT_USER.into(),
        },
    };
    let err = surface
        .events(&binding, Sender::new(Drop))
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Unsupported("events"));
}

fn message(ts: &str, extra: Value) -> Value {
    let mut message = json!({"type": "message", "ts": ts, "text": format!("m{ts}")});
    for (key, value) in extra.as_object().unwrap() {
        message[key] = value.clone();
    }
    message
}

#[tokio::test]
async fn thread_history_keeps_the_newest_messages_before_the_cursor_oldest_first() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.replies"))
        .and(body_string_contains("cursor=p2"))
        .respond_with(ok(json!({
            "messages": [
                message("10.4", json!({"bot_id": "B0OTHER01"})),
                message("10.5", json!({"user": USER})),
                message("10.9", json!({"user": USER})),
            ],
            "response_metadata": {"next_cursor": ""},
        })))
        .mount(&server)
        .await;
    mount(
        &server,
        "conversations.replies",
        ok(json!({
            "messages": [
                message("10.0", json!({"user": USER})),
                message("10.1", json!({"user": USER, "subtype": "channel_join"})),
                message("10.2", json!({"bot_id": "B0OTHER01", "bot_profile": {"id": "B0OTHER01"}})),
                message("10.3", json!({"subtype": "tombstone"})),
            ],
            "has_more": true,
            "response_metadata": {"next_cursor": "p2"},
        })),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(ok(
            json!({"bot": {"id": "B0OTHER01", "user_id": "U0OTHER01"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let key = ThreadKey {
        conv: conv(),
        root: Some("10.0".into()),
    };
    let msgs = surface
        .history(&key, Some(Cursor::new("10.9")), 3)
        .await
        .unwrap();
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["10.2", "10.4", "10.5"]);
    assert_eq!(msgs[0].sender.user.as_str(), "U0OTHER01");
    assert!(msgs[0].sender_is_bot);
    assert_eq!(msgs[1].sender.user.as_str(), "U0OTHER01");
    assert_eq!(msgs[2].sender.user.as_str(), USER);
    assert!(!msgs[2].sender_is_bot);
    assert_eq!(msgs[2].sender.team.as_str(), TEAM);
    assert_eq!(msgs[2].text, "m10.5");
    assert_eq!(msgs[2].sent_at.unix_timestamp(), 10);

    let sent = requests(&server).await;
    let first = form(&sent[0]);
    assert_eq!(first["ts"], "10.0");
    assert_eq!(first["latest"], "10.9");
    assert_eq!(first["inclusive"], "false");
    assert_eq!(surface.history(&key, None, 0).await.unwrap(), []);
}

#[tokio::test]
async fn channel_history_pages_back_until_the_limit() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .and(body_string_contains("cursor=older"))
        .respond_with(ok(json!({
            "messages": [message("5.0", json!({"user": USER})), message("4.0", json!({"user": USER}))],
            "response_metadata": {"next_cursor": "oldest"},
        })))
        .mount(&server)
        .await;
    mount(
        &server,
        "conversations.history",
        ok(json!({
            "messages": [
                message("7.0", json!({"user": USER})),
                message("6.0", json!({"subtype": "channel_leave", "user": USER})),
            ],
            "has_more": true,
            "response_metadata": {"next_cursor": "older"},
        })),
    )
    .await;
    let key = ThreadKey {
        conv: conv(),
        root: None,
    };
    let msgs = surface.history(&key, None, 2).await.unwrap();
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["5.0", "7.0"]);
    let sent = requests(&server).await;
    assert_eq!(sent.len(), 2, "stopped once the limit was reached");
    assert_eq!(form(&sent[0])["limit"], "200");
    assert_eq!(form(&sent[1])["limit"], "200");
}

struct PagedHistory(Vec<Value>);

impl wiremock::Respond for PagedHistory {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form = form(request);
        let limit: usize = form["limit"].parse().unwrap();
        let start: usize = form
            .get("cursor")
            .map_or(0, |cursor| cursor.parse().unwrap());
        let end = (start + limit).min(self.0.len());
        let next = if end < self.0.len() {
            end.to_string()
        } else {
            String::new()
        };
        ok(json!({
            "messages": self.0[start..end],
            "has_more": end < self.0.len(),
            "response_metadata": {"next_cursor": next},
        }))
    }
}

#[tokio::test]
async fn channel_history_asks_for_full_pages_however_many_messages_are_skipped() {
    let (server, surface) = setup().await;
    let mut newest_first: Vec<Value> = (0..30)
        .map(|i| {
            message(
                &format!("{}.0", 100 - i),
                json!({"subtype": "channel_join", "user": USER}),
            )
        })
        .collect();
    newest_first.push(message("50.0", json!({"user": USER})));
    newest_first.push(message("40.0", json!({"user": USER})));
    newest_first.push(message("30.0", json!({"user": USER})));
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(PagedHistory(newest_first))
        .mount(&server)
        .await;
    let key = ThreadKey {
        conv: conv(),
        root: None,
    };
    let msgs = surface.history(&key, None, 2).await.unwrap();
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["40.0", "50.0"]);
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn thread_history_skips_a_root_repeated_on_a_later_page() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.replies"))
        .and(body_string_contains("cursor=p2"))
        .respond_with(ok(json!({
            "messages": [
                message("10.0", json!({"user": USER})),
                message("10.2", json!({"user": USER})),
                message("10.1", json!({"user": USER})),
            ],
            "response_metadata": {"next_cursor": ""},
        })))
        .mount(&server)
        .await;
    mount(
        &server,
        "conversations.replies",
        ok(json!({
            "messages": [
                message("10.0", json!({"user": USER})),
                message("10.1", json!({"user": USER})),
            ],
            "has_more": true,
            "response_metadata": {"next_cursor": "p2"},
        })),
    )
    .await;
    let key = ThreadKey {
        conv: conv(),
        root: Some("10.0".into()),
    };
    let msgs = surface.history(&key, None, 10).await.unwrap();
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["10.0", "10.1", "10.2"]);
}

fn bot_event() -> InboundEvent {
    let envelope: Value = serde_json::from_str(testkit::slack::MESSAGE_BOT_WITHOUT_USER).unwrap();
    let bot = UserId::from(BOT_USER);
    let team = TEAM.into();
    let context = Context {
        binding: BindingId::new_v4(),
        bot_user: Some(&bot),
        team: &team,
        home_org: None,
        event_id: "Ev0BOTNOUSR",
        received_at: time::OffsetDateTime::now_utc(),
    };
    normalize::message(&context, &envelope["event"]).unwrap()
}

#[tokio::test]
async fn fill_bot_sender_names_a_bot_by_its_user_and_caches_it() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(ok(
            json!({"bot": {"id": "B0LEGACY1", "user_id": "U0DEPLOY1", "app_id": "A0LEGACY1"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    for _ in 0..2 {
        let mut event = bot_event();
        assert_eq!(event.sender.user.as_str(), "B0LEGACY1");
        assert_eq!(event.sender_bot_user, None);
        surface.fill_bot_sender(&mut event).await.unwrap();
        assert_eq!(event.sender.user.as_str(), "U0DEPLOY1");
        assert_eq!(event.sender_bot_user, Some(UserId::from("U0DEPLOY1")));
    }
    assert_eq!(form(&requests(&server).await[0])["bot"], "B0LEGACY1");
}

#[tokio::test]
async fn a_bot_without_a_user_keeps_its_bot_id() {
    for response in [
        ok(json!({"bot": {"id": "B0LEGACY1", "name": "deploys"}})),
        ok(json!({"bot": {"id": "B0LEGACY1", "user_id": "not-a-user"}})),
        ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "bot_not_found"})),
    ] {
        let (server, surface) = setup().await;
        Mock::given(method("POST"))
            .and(path("/api/bots.info"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        for _ in 0..2 {
            let mut event = bot_event();
            surface.fill_bot_sender(&mut event).await.unwrap();
            assert_eq!(event.sender.user.as_str(), "B0LEGACY1");
            assert_eq!(event.sender_bot_user, None);
        }
    }
}

#[tokio::test]
async fn a_failed_bot_lookup_leaves_the_event_alone_and_is_not_cached() {
    let (server, surface) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "invalid_auth"})),
        )
        .expect(2)
        .mount(&server)
        .await;
    for _ in 0..2 {
        let mut event = bot_event();
        let err = surface.fill_bot_sender(&mut event).await.unwrap_err();
        assert_eq!(err, SurfaceError::Unauthorized);
        assert_eq!(event.sender.user.as_str(), "B0LEGACY1");
    }
}

#[tokio::test]
async fn a_bot_answer_about_another_bot_or_not_saying_bot_not_found_is_no_answer() {
    for (answer, err) in [
        (
            ok(json!({"bot": {"id": "B0SOMEONE", "user_id": "U0DEPLOY1"}})),
            SurfaceError::Api("bots.info answered for another bot".into()),
        ),
        (
            refused("user_not_found"),
            SurfaceError::NotFound("user_not_found".into()),
        ),
    ] {
        let (server, surface) = setup().await;
        Mock::given(method("POST"))
            .and(path("/api/bots.info"))
            .respond_with(answer)
            .expect(2)
            .mount(&server)
            .await;
        for _ in 0..2 {
            let mut event = bot_event();
            assert_eq!(surface.fill_bot_sender(&mut event).await.unwrap_err(), err);
            assert_eq!(event.sender.user.as_str(), "B0LEGACY1");
            assert_eq!(event.sender_bot_user, None);
        }
    }
}

#[tokio::test]
async fn fill_bot_sender_leaves_humans_known_bots_and_other_teams_alone() {
    let (server, surface) = setup().await;
    let mut human = bot_event();
    human.sender_is_bot = false;
    let mut known = bot_event();
    known.sender_bot_user = Some("U0KNOWN01".into());
    let mut elsewhere = bot_event();
    elsewhere.sender.team = "T0OTHER01".into();
    for mut event in [human, known, elsewhere] {
        let before = event.clone();
        surface.fill_bot_sender(&mut event).await.unwrap();
        assert_eq!(event, before);
    }
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn a_bot_lookup_past_the_quota_fails_at_once_without_a_call() {
    let (server, surface) = setup().await;
    mount(&server, "bots.info", |request: &Request| {
        let bot = form(request).remove("bot").unwrap_or_default();
        ok(json!({"bot": {"id": bot, "user_id": "U0MADEUP"}}))
    })
    .await;
    for n in 0..50 {
        let mut event = bot_event();
        event.sender.user = format!("B0MADEUP{n}").into();
        surface.fill_bot_sender(&mut event).await.unwrap();
    }
    let mut event = bot_event();
    let before = event.clone();
    let started = std::time::Instant::now();
    let err = surface.fill_bot_sender(&mut event).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(err, SurfaceError::RateLimited { .. }), "{err:?}");
    assert_eq!(event, before, "the message goes on from an unmanaged bot");
    assert_eq!(lookups(&server, "bots.info").await.len(), 50);
}

/// `event`, as if it arrived a second after it was posted.
fn arrived_now(mut event: InboundEvent) -> InboundEvent {
    let (seconds, _) = event.message.id.as_str().split_once('.').unwrap();
    let posted = time::OffsetDateTime::from_unix_timestamp(seconds.parse().unwrap()).unwrap();
    event.received_at = posted + Duration::from_secs(1);
    event
}

fn event_from(fixture: &str) -> InboundEvent {
    let envelope: Value = serde_json::from_str(fixture).unwrap();
    let bot = UserId::from(BOT_USER);
    let team = TEAM.into();
    let context = Context {
        binding: BindingId::new_v4(),
        bot_user: Some(&bot),
        team: &team,
        home_org: None,
        event_id: "Ev0CONFIRM",
        received_at: time::OffsetDateTime::now_utc(),
    };
    arrived_now(normalize::message(&context, &envelope["event"]).unwrap())
}

async fn confirming_setup(kind: Value) -> (MockServer, SlackSurface) {
    let (server, surface) = setup().await;
    let mut channel = kind;
    channel["id"] = json!(CHANNEL);
    mount(
        &server,
        "conversations.info",
        ok(json!({"channel": channel})),
    )
    .await;
    (server, surface.with_bot_user(Some(BOT_USER.into())))
}

/// [`confirming_setup`], with `users.info` answering that the senders
/// these tests read back, `USER` and `U0HUMAN02`, are home.
async fn confirming_home_setup(kind: Value) -> (MockServer, SlackSurface) {
    let (server, surface) = confirming_setup(kind).await;
    for user in [USER, "U0HUMAN02"] {
        mount_user(&server, user, user_in(user, Some(TEAM))).await;
    }
    (server, surface)
}

fn public_channel() -> Value {
    json!({"is_channel": true, "is_member": true})
}

async fn lookups(server: &MockServer, name: &str) -> Vec<Request> {
    requests(server)
        .await
        .into_iter()
        .filter(|request| request.url.path() == format!("/api/{name}"))
        .collect()
}

#[tokio::test]
async fn a_top_level_message_is_confirmed_as_slack_has_it() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let ts = event.message.id.as_str().to_owned();
    let mention = format!("<@{BOT_USER}> hi");
    let cases = [
        (
            json!([{"ts": ts, "user": USER, "text": event.text}]),
            Some((USER, event.text.as_str())),
        ),
        (
            json!([{"ts": ts, "user": "U0HUMAN02", "text": mention}]),
            Some(("U0HUMAN02", mention.as_str())),
        ),
        (
            json!([{"ts": ts, "user": USER, "text": "no mention"}]),
            None,
        ),
        (
            json!([{"ts": "1727697500.000100", "user": USER, "text": event.text}]),
            None,
        ),
        (
            json!([{"ts": ts, "user": USER, "text": event.text, "subtype": "channel_join"}]),
            None,
        ),
        (
            json!([{"ts": ts, "subtype": "tombstone", "text": "This message was deleted."}]),
            None,
        ),
        (json!([]), None),
    ];
    for (messages, expected) in cases {
        let (server, surface) = confirming_home_setup(public_channel()).await;
        mount(
            &server,
            "conversations.history",
            ok(json!({"messages": messages})),
        )
        .await;
        let copy = surface.confirm(&event).await.unwrap();
        match expected {
            Some((user, text)) => {
                let copy = copy.unwrap();
                assert_eq!(copy.sender.user.as_str(), user, "{messages}");
                assert_eq!(copy.text, text);
                assert_eq!(copy.message, event.message);
                assert_eq!(copy.event_id, event.event_id);
                assert_eq!(copy.binding, event.binding);
                assert_eq!(copy.received_at, event.received_at);
            }
            None => assert_eq!(copy, None, "{messages}"),
        }
        let sent = lookups(&server, "conversations.history").await;
        assert_eq!(sent.len(), 1);
        let form = form(&sent[0]);
        assert_eq!(form["channel"], CHANNEL);
        assert_eq!(form["oldest"], ts);
        assert_eq!(form["latest"], ts);
        assert_eq!(form["inclusive"], "true");
        assert!(!form.contains_key("ts"));
    }
}

#[tokio::test]
async fn nothing_but_slacks_copy_decides_what_the_event_is() {
    let mut forged = event_from(testkit::slack::MESSAGE_MENTION);
    forged.conv_kind = ConvKind::Dm;
    forged.files = vec![];
    let (server, surface) = confirming_home_setup(public_channel()).await;
    let ts = forged.message.id.as_str();
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{
            "ts": ts,
            "user": USER,
            "text": format!("<@{BOT_USER}> see attached"),
            "subtype": "file_share",
            "files": [{"id": "F1", "name": "a.txt", "url_private": "https://files.slack.com/F1"}],
            "channel_type": "im",
        }]})),
    )
    .await;
    let copy = surface.confirm(&forged).await.unwrap().unwrap();
    assert_eq!(copy.conv_kind, ConvKind::Channel);
    assert_eq!(copy.mentions, [UserId::from(BOT_USER)]);
    assert_eq!(copy.files.len(), 1);
    assert_eq!(copy.files[0].id, "F1");
    assert_eq!(copy.text, format!("<@{BOT_USER}> see attached"));
}

#[tokio::test]
async fn blocks_count_only_as_slack_has_them() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let ts = event.message.id.as_str();
    let blocks = json!([{"type": "rich_text", "elements": [
        {"type": "rich_text_section", "elements": [{"type": "user", "user_id": BOT_USER}]},
    ]}]);
    let (server, surface) = confirming_home_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": "hi", "blocks": blocks}]})),
    )
    .await;
    let copy = surface.confirm(&event).await.unwrap().unwrap();
    assert_eq!(copy.mentions, [UserId::from(BOT_USER)]);

    let (server, surface) = confirming_home_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": "hi"}]})),
    )
    .await;
    assert_eq!(
        surface.confirm(&event).await,
        Ok(None),
        "the event's blocks don't count"
    );
}

#[tokio::test]
async fn the_conversation_kind_comes_from_conversations_info_once_an_hour() {
    let mut event = event_from(testkit::slack::MESSAGE_MENTION);
    event.conv_kind = ConvKind::Channel;
    let ts = event.message.id.as_str();
    let (server, surface) = confirming_home_setup(json!({"is_im": true, "user": USER})).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": "no mention"}]})),
    )
    .await;
    for _ in 0..2 {
        let copy = surface.confirm(&event).await.unwrap().unwrap();
        assert_eq!(copy.conv_kind, ConvKind::Dm);
    }
    assert_eq!(lookups(&server, "conversations.info").await.len(), 1);

    let (server, surface) = confirming_home_setup(json!({"is_mpim": true, "is_group": true})).await;
    let mention = format!("<@{BOT_USER}> in a group DM");
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": mention}]})),
    )
    .await;
    assert_eq!(
        surface.confirm(&event).await.unwrap().unwrap().conv_kind,
        ConvKind::GroupDm
    );
    let form = form(&lookups(&server, "conversations.info").await[0]);
    assert_eq!(form["channel"], CHANNEL);
}

#[tokio::test]
async fn a_message_older_than_the_window_is_not_read_back() {
    let mut event = event_from(testkit::slack::MESSAGE_MENTION);
    let (server, surface) = confirming_setup(public_channel()).await;
    event.received_at += surface_slack::surface::CONFIRM_WINDOW + Duration::from_secs(1);
    assert_eq!(surface.confirm(&event).await, Ok(None));
    event.message.id = "not a ts".into();
    assert_eq!(surface.confirm(&event).await, Ok(None));
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn a_thread_reply_is_confirmed_only_under_a_root_the_bot_may_have_posted() {
    let event = event_from(testkit::slack::MESSAGE_THREAD_REPLY);
    let root = event.thread_root.clone().unwrap();
    for (parent, confirmed) in [
        (json!(BOT_USER), true),
        (Value::Null, true),
        (json!("not a user"), true),
        (json!(USER), false),
    ] {
        let (server, surface) = confirming_home_setup(public_channel()).await;
        mount(
            &server,
            "conversations.replies",
            ok(json!({"messages": [{
                "ts": event.message.id.as_str(),
                "user": USER,
                "text": event.text,
                "thread_ts": root.as_str(),
                "parent_user_id": parent,
            }]})),
        )
        .await;
        let copy = surface.confirm(&event).await.unwrap();
        assert_eq!(copy.is_some(), confirmed, "{parent}");
    }
}

#[tokio::test]
async fn a_thread_reply_is_read_back_in_the_thread_the_event_names() {
    let event = event_from(testkit::slack::MESSAGE_THREAD_REPLY);
    let root = event.thread_root.clone().unwrap();
    let (server, surface) = confirming_home_setup(public_channel()).await;
    mount(
        &server,
        "conversations.replies",
        ok(json!({"messages": [
            {"ts": root.as_str(), "user": BOT_USER, "text": "the root", "thread_ts": root.as_str()},
            {"ts": event.message.id.as_str(), "user": USER, "text": event.text, "thread_ts": "1727697600.000001"},
        ]})),
    )
    .await;
    let copy = surface.confirm(&event).await.unwrap().unwrap();
    assert_eq!(copy.thread_root, Some("1727697600.000001".into()));
    assert_eq!(copy.reply_to.unwrap().id.as_str(), "1727697600.000001");
    let form = form(&lookups(&server, "conversations.replies").await[0]);
    assert_eq!(form["ts"], root.as_str());
    assert_eq!(form["oldest"], event.message.id.as_str());
}

#[tokio::test]
async fn a_bot_known_by_its_bot_id_is_named_by_its_user_and_an_edited_bot_post_is_refused() {
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "bots.info",
        ok(json!({"bot": {"id": "B0LEGACY1", "user_id": "U0DEPLOY1"}})),
    )
    .await;
    let event = arrived_now(bot_event());
    let ts = event.message.id.as_str();
    let root = event.thread_root.clone().unwrap();
    let post =
        json!({"ts": ts, "bot_id": "B0LEGACY1", "text": event.text, "thread_ts": root.as_str()});
    Mock::given(method("POST"))
        .and(path("/api/conversations.replies"))
        .respond_with(ok(json!({"messages": [post]})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let copy = surface.confirm(&event).await.unwrap().unwrap();
    assert_eq!(copy.sender.user.as_str(), "U0DEPLOY1");
    assert_eq!(copy.sender_bot_user, Some("U0DEPLOY1".into()));
    assert!(copy.sender_is_bot);

    let mut edited = post;
    edited["edited"] = json!({"user": "U0DEPLOY1", "ts": "1727698400.000000"});
    mount(
        &server,
        "conversations.replies",
        ok(json!({"messages": [edited]})),
    )
    .await;
    assert_eq!(surface.confirm(&event).await, Ok(None));
}

#[tokio::test]
async fn a_message_that_cannot_be_read_back_is_an_error() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ResponseTemplate::new(200)
            .set_body_json(json!({"ok": false, "error": "channel_not_found"})),
    )
    .await;
    assert_eq!(
        surface.confirm(&event).await,
        Err(SurfaceError::NotFound("channel_not_found".into()))
    );
    let mut elsewhere = event.clone();
    elsewhere.conv.team = "T0OTHER01".into();
    assert!(matches!(
        surface.confirm(&elsewhere).await,
        Err(SurfaceError::Api(_))
    ));

    let (server, surface) = setup().await;
    mount(&server, "conversations.info", ResponseTemplate::new(503)).await;
    assert!(matches!(
        surface.confirm(&event).await,
        Err(SurfaceError::Transport(_))
    ));
    assert!(lookups(&server, "conversations.history").await.is_empty());
}

#[tokio::test]
async fn confirming_never_waits_out_a_rate_limit() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
    )
    .await;
    let started = std::time::Instant::now();
    for _ in 0..2 {
        assert!(matches!(
            surface.confirm(&event).await,
            Err(SurfaceError::RateLimited { .. })
        ));
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        lookups(&server, "conversations.history").await.len(),
        1,
        "not retried, and the next lookup isn't sent while the 429 holds"
    );
}

#[tokio::test]
async fn a_channel_id_slack_spells_otherwise_is_refused() {
    let mut event = event_from(testkit::slack::MESSAGE_MENTION);
    let lower = CHANNEL.to_lowercase();
    event.conv.conversation = lower.as_str().into();
    event.message.conv.conversation = lower.as_str().into();
    let (server, surface) = confirming_setup(public_channel()).await;
    assert!(matches!(
        surface.confirm(&event).await,
        Err(SurfaceError::NotFound(_))
    ));
    assert!(lookups(&server, "conversations.history").await.is_empty());
}

#[tokio::test]
async fn a_bot_id_not_shaped_like_slacks_is_never_looked_up() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "bots.info",
        ok(json!({"bot": {"id": "B0MADEUP", "user_id": "U0MADEUP"}})),
    )
    .await;
    for bot_id in [
        format!("B{}", "A".repeat(900_000)),
        format!("B{}", "A".repeat(MAX_ID_TAIL + 1)),
        "B".to_owned(),
        "b0lower".to_owned(),
        "U0HUMAN01".to_owned(),
    ] {
        let mut event = bot_event();
        event.sender.user = bot_id.as_str().into();
        let before = event.clone();
        surface.fill_bot_sender(&mut event).await.unwrap();
        assert_eq!(event, before);
    }
    assert!(lookups(&server, "bots.info").await.is_empty());
}

async fn mount_user(server: &MockServer, user: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(body_string_contains(format!("user={user}").as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

fn user_in(user: &str, team: Option<&str>) -> ResponseTemplate {
    let mut answer = json!({"id": user, "name": user.to_lowercase()});
    if let Some(team) = team {
        answer["team_id"] = json!(team);
    }
    ok(json!({"user": answer}))
}

fn refused(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": code}))
}

/// A surface whose member list has `listed` with their teams.
async fn listing(listed: &[(&str, &str)]) -> (MockServer, SlackSurface) {
    let (server, surface) = setup().await;
    let members: Vec<Value> = listed
        .iter()
        .map(|(user, team)| json!({"id": user, "team_id": team, "name": user.to_lowercase()}))
        .collect();
    mount(&server, "users.list", ok(json!({"members": members}))).await;
    surface.refresh_members().await.unwrap();
    (server, surface)
}

fn home_event_from(user: &str) -> InboundEvent {
    let mut event = event_from(testkit::slack::MESSAGE_CONNECT_HOME);
    event.sender.user = user.into();
    event
}

fn outside_of(team: &str) -> Option<Outside> {
    Some(Outside { team: team.into() })
}

#[tokio::test]
async fn a_sender_is_home_only_when_the_home_check_agrees() {
    let (server, surface) = listing(&[(USER, TEAM), ("U0HUMAN02", OUTSIDE_TEAM)]).await;
    mount_user(
        &server,
        "U0HUMAN02",
        user_in("U0HUMAN02", Some(OUTSIDE_TEAM)),
    )
    .await;
    mount_user(&server, "U0LOOKUP1", user_in("U0LOOKUP1", Some(TEAM))).await;
    mount_user(&server, "U0NOTEAM1", user_in("U0NOTEAM1", None)).await;
    mount_user(
        &server,
        "U0THEIRS1",
        user_in("U0THEIRS1", Some(OUTSIDE_TEAM)),
    )
    .await;
    mount_user(&server, "U0ORGWIDE", user_in("U0ORGWIDE", Some(HOME_ORG))).await;
    mount_user(&server, "U0GONE001", refused("user_not_found")).await;
    for (user, home) in [
        (USER, true),
        ("U0HUMAN02", false),
        ("U0LOOKUP1", true),
        ("U0NOTEAM1", false),
        ("U0THEIRS1", false),
        ("U0ORGWIDE", false),
        ("U0GONE001", false),
    ] {
        let event = home_event_from(user);
        assert_eq!(event.outside, None, "the fields all name home");
        assert_eq!(
            surface.copy_sender_is_home(&event).await.unwrap(),
            home,
            "{user}"
        );
    }
    let looked_up = lookups(&server, "users.info").await;
    assert_eq!(looked_up.len(), 6, "the listed home member costs no lookup");

    for user in ["U0HUMAN02", "U0LOOKUP1", "U0GONE001"] {
        surface
            .copy_sender_is_home(&home_event_from(user))
            .await
            .unwrap();
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        6,
        "both answers are kept"
    );

    let mut theirs = home_event_from(USER);
    theirs.outside = outside_of(OUTSIDE_TEAM);
    assert!(
        !surface.copy_sender_is_home(&theirs).await.unwrap(),
        "fields that say outside are never answered home"
    );
    assert_eq!(lookups(&server, "users.info").await.len(), 6);
}

#[tokio::test]
async fn a_home_lookup_naming_no_team_is_not_home() {
    let (server, surface) = setup().await;
    mount_user(&server, "U0NOTEAM1", user_in("U0NOTEAM1", None)).await;
    assert_eq!(
        surface
            .directory()
            .membership(surface.api(), &"U0NOTEAM1".into())
            .await,
        Ok(Membership::Outside(None))
    );
    assert!(
        !surface
            .copy_sender_is_home(&home_event_from("U0NOTEAM1"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn user_not_found_and_not_visible_are_cached_outside_verdicts() {
    let (server, surface) = setup().await;
    for (user, code) in [
        ("U0GONE001", "user_not_found"),
        ("U0HIDDEN1", "user_not_visible"),
    ] {
        mount_user(&server, user, refused(code)).await;
        for _ in 0..2 {
            assert_eq!(
                surface
                    .directory()
                    .membership(surface.api(), &user.into())
                    .await,
                Ok(Membership::Outside(None)),
                "{code}"
            );
        }
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        2,
        "each verdict is kept"
    );
}

#[tokio::test]
async fn a_sender_of_another_workspace_or_surface_is_not_home_without_a_lookup() {
    let (server, surface) = listing(&[(USER, TEAM)]).await;
    let mut elsewhere = home_event_from(USER);
    elsewhere.sender.team = OUTSIDE_TEAM.into();
    let mut org = home_event_from(USER);
    org.sender.team = HOME_ORG.into();
    let mut other_surface = home_event_from(USER);
    other_surface.sender.surface = SurfaceKind::RocketChat;
    for (event, what) in [
        (elsewhere, "another workspace"),
        (org, "the organization itself"),
        (other_surface, "another surface"),
    ] {
        assert!(
            !surface.copy_sender_is_home(&event).await.unwrap(),
            "{what}"
        );
    }
    assert!(lookups(&server, "users.info").await.is_empty());
}

#[tokio::test]
async fn a_home_member_in_a_shared_channel_is_home() {
    let (server, surface) = listing(&[(USER, TEAM)]).await;
    let event = event_from(testkit::slack::MESSAGE_CONNECT_HOME);
    assert_eq!(event.conv.conversation.as_str(), SHARED_CHANNEL);
    assert!(surface.copy_sender_is_home(&event).await.unwrap());
    assert!(lookups(&server, "users.info").await.is_empty());
}

#[tokio::test]
async fn a_home_organization_field_with_a_home_lookup_is_home() {
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let directory = Arc::new(TeamDirectory::new(TEAM.into()).with_home_org(Some(HOME_ORG.into())));
    let surface = SlackSurface::new(client.bot(SecretString::from(TOKEN)), directory);
    mount_user(&server, USER, user_in(USER, Some(TEAM))).await;
    mount_user(
        &server,
        "U0ELSEWHR",
        user_in("U0ELSEWHR", Some("T0SIBLING")),
    )
    .await;
    let envelope: Value = serde_json::from_str(testkit::slack::MESSAGE_HOME_ORG).unwrap();
    let bot = UserId::from(BOT_USER);
    let team = TEAM.into();
    let org = HOME_ORG.into();
    let context = Context {
        binding: BindingId::new_v4(),
        bot_user: Some(&bot),
        team: &team,
        home_org: Some(&org),
        event_id: "Ev0HOMEORG",
        received_at: time::OffsetDateTime::now_utc(),
    };
    let event = normalize::message(&context, &envelope["event"]).unwrap();
    assert_eq!(event.outside, None);
    assert!(surface.copy_sender_is_home(&event).await.unwrap());
    let mut sibling = event.clone();
    sibling.sender.user = "U0ELSEWHR".into();
    assert!(
        !surface.copy_sender_is_home(&sibling).await.unwrap(),
        "another workspace of the organization isn't home"
    );

    mount_user(
        &server,
        "U0GRIDMEM",
        ok(json!({"user": {
            "id": "U0GRIDMEM",
            "team_id": "T0SIBLING",
            "enterprise_user": {"enterprise_id": HOME_ORG, "teams": ["T0SIBLING", TEAM]},
        }})),
    )
    .await;
    let mut member_of_both = event;
    member_of_both.sender.user = "U0GRIDMEM".into();
    assert!(
        surface.copy_sender_is_home(&member_of_both).await.unwrap(),
        "a Grid member whose workspaces include this one is home"
    );
}

#[tokio::test]
async fn a_home_lookup_slack_refuses_is_not_home() {
    let (server, surface) = setup().await;
    for (user, code) in [
        ("U0SCOPE01", "missing_scope"),
        ("U0AUTH001", "invalid_auth"),
    ] {
        mount_user(&server, user, refused(code)).await;
        for _ in 0..2 {
            assert!(
                !surface
                    .copy_sender_is_home(&home_event_from(user))
                    .await
                    .unwrap(),
                "{code}"
            );
        }
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        4,
        "a refusal is no answer and isn't kept"
    );
}

#[tokio::test]
async fn user_not_visible_is_kept_and_noted_once_a_minute() {
    const HIDING: &str = "T0HIDING1";
    let logs = testkit::Logs::global();
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let surface = SlackSurface::new(
        client.bot(SecretString::from(TOKEN)),
        Arc::new(TeamDirectory::new(HIDING.into())),
    );
    for user in ["U0HIDDEN1", "U0HIDDEN2", "U0HIDDEN1"] {
        mount_user(&server, user, refused("user_not_visible")).await;
        let mut event = home_event_from(user);
        event.sender.team = HIDING.into();
        assert!(
            !surface.copy_sender_is_home(&event).await.unwrap(),
            "{user}"
        );
    }
    assert_eq!(lookups(&server, "users.info").await.len(), 2, "kept");
    let noted = logs
        .snapshot()
        .matching("user_not_visible")
        .matching(HIDING)
        .to_string();
    assert_eq!(
        noted.lines().filter(|line| line.contains("INFO")).count(),
        1,
        "{noted}"
    );
}

#[tokio::test]
async fn an_answer_about_someone_else_or_that_doesnt_read_is_no_answer_and_warned_of() {
    let logs = testkit::Logs::global();
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    mount_user(&server, "U0ASKED01", user_in("U0SOMEONE", Some(TEAM))).await;
    mount_user(&server, "U0GARBLED", ok(json!({"user": "garbled"}))).await;
    for (team, user, answer, warning) in [
        ("T0WARNED2", "U0ASKED01", Some(false), "another user"),
        ("T0WARNED3", "U0GARBLED", None, "unexpected response"),
    ] {
        let surface = SlackSurface::new(
            client.bot(SecretString::from(TOKEN)),
            Arc::new(TeamDirectory::new(team.into())),
        );
        for _ in 0..2 {
            let mut event = home_event_from(user);
            event.sender.team = team.into();
            let home = surface.copy_sender_is_home(&event).await;
            assert_eq!(home.clone().ok(), answer, "{user}: {home:?}");
        }
        let warned = logs
            .snapshot()
            .matching("couldn't ask Slack whether a user is home")
            .matching(team)
            .to_string();
        assert_eq!(
            warned.lines().filter(|line| line.contains("WARN")).count(),
            1,
            "{warned}"
        );
        assert!(warned.contains(warning), "{warned}");
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        4,
        "nothing is kept"
    );
}

#[tokio::test]
async fn a_grid_member_while_auth_test_named_no_organization_is_warned_of_once() {
    const LONE: &str = "T0NOORG01";
    let logs = testkit::Logs::global();
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let surface = SlackSurface::new(
        client.bot(SecretString::from(TOKEN)),
        Arc::new(TeamDirectory::new(LONE.into())),
    );
    let grid_member = |user: &str, team: &str, org: &str| {
        ok(json!({"user": {
            "id": user,
            "team_id": team,
            "enterprise_user": {"enterprise_id": org, "teams": [team]},
        }}))
    };
    let warnings = || {
        let warned = logs
            .snapshot()
            .matching("auth.test gave the workspace none")
            .matching(LONE)
            .to_string();
        warned.lines().filter(|line| line.contains("WARN")).count()
    };
    let look_up = async |user: &str| {
        let mut event = home_event_from(user);
        event.sender.team = LONE.into();
        assert!(
            !surface.copy_sender_is_home(&event).await.unwrap(),
            "{user}"
        );
    };
    mount_user(
        &server,
        "U0THEIRS1",
        grid_member("U0THEIRS1", OUTSIDE_TEAM, "E0THEIRS1"),
    )
    .await;
    look_up("U0THEIRS1").await;
    assert_eq!(
        warnings(),
        0,
        "another organization's Grid member says nothing of this workspace"
    );
    for user in ["U0GRIDMEM", "U0GRIDME2"] {
        mount_user(&server, user, grid_member(user, LONE, HOME_ORG)).await;
        look_up(user).await;
    }
    assert_eq!(warnings(), 1);

    for (lone, user, team_id) in [
        ("T0NOORG02", "U0SIBLING", "T0SIBLING"),
        ("T0NOORG03", "U0ORGWIDE", HOME_ORG),
    ] {
        let surface = SlackSurface::new(
            client.bot(SecretString::from(TOKEN)),
            Arc::new(TeamDirectory::new(lone.into())),
        );
        mount_user(
            &server,
            user,
            ok(json!({"user": {
                "id": user,
                "team_id": team_id,
                "enterprise_user": {"enterprise_id": HOME_ORG, "teams": ["T0SIBLING", lone]},
            }})),
        )
        .await;
        let mut event = home_event_from(user);
        event.sender.team = lone.into();
        assert!(
            !surface.copy_sender_is_home(&event).await.unwrap(),
            "{user}"
        );
        let warned = logs
            .snapshot()
            .matching("auth.test gave the workspace none")
            .matching(lone)
            .to_string();
        assert_eq!(
            warned.lines().filter(|line| line.contains("WARN")).count(),
            1,
            "a member the organization lists in {lone}, whose team_id is {team_id}: {warned}"
        );
    }
}

#[tokio::test]
async fn slacks_passing_failures_ask_to_try_again_rather_than_say_not_home() {
    let (server, surface) = setup().await;
    for (user, code) in [
        ("U0FATAL01", "fatal_error"),
        ("U0INTERN1", "internal_error"),
        ("U0UNAVAIL", "service_unavailable"),
        ("U0TIMEOUT", "request_timeout"),
    ] {
        mount_user(&server, user, refused(code)).await;
        for _ in 0..2 {
            let err = surface
                .copy_sender_is_home(&home_event_from(user))
                .await
                .unwrap_err();
            assert_eq!(err, SurfaceError::Transport(code.to_owned()), "{code}");
        }
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        8,
        "nothing is kept"
    );
}

#[tokio::test]
async fn a_lookup_that_wont_pass_on_its_own_is_warned_of_once_a_minute() {
    const WARNED: &str = "T0WARNED1";
    let logs = testkit::Logs::global();
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let surface = SlackSurface::new(
        client.bot(SecretString::from(TOKEN)),
        Arc::new(TeamDirectory::new(WARNED.into())),
    );
    let event_from = |user: &str| {
        let mut event = home_event_from(user);
        event.sender.team = WARNED.into();
        event
    };
    for user in ["U0SCOPE01", "U0SCOPE02", "U0SCOPE03"] {
        mount_user(&server, user, refused("missing_scope")).await;
        assert!(
            !surface
                .copy_sender_is_home(&event_from(user))
                .await
                .unwrap()
        );
    }
    mount_user(&server, "U0DOWN001", ResponseTemplate::new(503)).await;
    surface
        .copy_sender_is_home(&event_from("U0DOWN001"))
        .await
        .unwrap_err();
    let warned = logs
        .snapshot()
        .matching("couldn't ask Slack whether a user is home")
        .matching(WARNED);
    let lines = warned.to_string();
    assert_eq!(
        lines.lines().filter(|line| line.contains("WARN")).count(),
        1,
        "{lines}"
    );
    assert!(lines.contains("missing_scope"), "{lines}");
}

#[tokio::test]
async fn deactivated_members_are_never_home() {
    let (server, surface) = setup().await;
    mount(
        &server,
        "users.list",
        ok(json!({"members": [
            {"id": USER, "team_id": TEAM, "name": "ada", "deleted": true},
        ]})),
    )
    .await;
    surface.refresh_members().await.unwrap();
    mount_user(
        &server,
        USER,
        ok(json!({"user": {"id": USER, "team_id": TEAM, "deleted": true}})),
    )
    .await;
    for _ in 0..2 {
        assert!(
            !surface
                .copy_sender_is_home(&home_event_from(USER))
                .await
                .unwrap()
        );
    }
    assert_eq!(
        lookups(&server, "users.info").await.len(),
        1,
        "the list doesn't vouch for them, and Slack's answer is kept"
    );
}

#[tokio::test]
async fn a_failed_home_lookup_is_an_error_not_a_verdict_and_is_not_cached() {
    let (server, surface) = setup().await;
    mount_user(&server, "U0DOWN001", ResponseTemplate::new(503)).await;
    mount_user(
        &server,
        "U0BUSY001",
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
    )
    .await;
    for _ in 0..2 {
        let err = surface
            .copy_sender_is_home(&home_event_from("U0DOWN001"))
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Transport(_)), "{err:?}");
    }
    assert_eq!(lookups(&server, "users.info").await.len(), 2);

    let started = std::time::Instant::now();
    let err = surface
        .copy_sender_is_home(&home_event_from("U0BUSY001"))
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::RateLimited { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "never waits");

    let (server, surface) = setup().await;
    mount_user(&server, "U0SCOPE01", refused("missing_scope")).await;
    let directory = surface.directory();
    for _ in 0..2 {
        let err = directory
            .home_user(surface.api(), &"U0SCOPE01".into())
            .await
            .unwrap_err();
        assert!(!matches!(err, SurfaceError::NotFound(_)), "{err:?}");
    }
    assert_eq!(lookups(&server, "users.info").await.len(), 2);
}

#[tokio::test]
async fn a_bots_post_is_never_looked_up() {
    let (server, surface) = setup().await;
    let mut bot = bot_event();
    bot.sender_bot_user = Some("U0OTHERBT".into());
    let mut classic = bot_event();
    classic.sender_bot_user = None;
    for event in [bot, classic] {
        assert!(surface.copy_sender_is_home(&event).await.unwrap());
    }

    let (server2, surface) = confirming_setup(public_channel()).await;
    let event = event_from(testkit::slack::MESSAGE_BOT);
    let ts = event.message.id.as_str();
    mount(
        &server2,
        "conversations.history",
        ok(json!({"messages": [{
            "ts": ts,
            "user": "U0OTHERBT",
            "bot_id": "B0OTHER01",
            "bot_profile": {"id": "B0OTHER01"},
            "text": format!("<@{BOT_USER}> hello"),
            "user_team": OUTSIDE_TEAM,
        }]})),
    )
    .await;
    mount(
        &server2,
        "bots.info",
        ok(json!({"bot": {"id": "B0OTHER01", "user_id": "U0OTHERBT"}})),
    )
    .await;
    let copy = surface.confirm(&event).await.unwrap().unwrap();
    assert!(copy.sender_is_bot);
    assert!(lookups(&server, "users.info").await.is_empty());
    assert!(lookups(&server2, "users.info").await.is_empty());
}

#[tokio::test]
async fn a_made_up_sender_costs_no_home_lookup() {
    let mut forged = event_from(testkit::slack::MESSAGE_MENTION);
    forged.sender.user = "U0MADEUP1".into();
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": []})),
    )
    .await;
    assert_eq!(surface.confirm(&forged).await.unwrap(), None);
    assert!(lookups(&server, "users.info").await.is_empty());

    let (server, surface) = confirming_setup(public_channel()).await;
    let ts = forged.message.id.as_str();
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": forged.text}]})),
    )
    .await;
    mount_user(&server, USER, user_in(USER, Some(TEAM))).await;
    let copy = surface.confirm(&forged).await.unwrap().unwrap();
    assert_eq!(copy.sender.user.as_str(), USER);
    assert_eq!(copy.outside, None);
    let looked_up = lookups(&server, "users.info").await;
    assert_eq!(looked_up.len(), 1);
    assert_eq!(
        form(&looked_up[0])["user"],
        USER,
        "Slack's sender, not the event's"
    );
}

#[tokio::test]
async fn confirm_reads_who_is_outside_from_slacks_copy() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let ts = event.message.id.as_str().to_owned();
    for (copy, info, outside) in [
        (
            json!({"ts": ts, "user": USER, "text": event.text, "user_team": OUTSIDE_TEAM}),
            None,
            Some(outside_of(OUTSIDE_TEAM)),
        ),
        (
            json!({"ts": ts, "user": USER, "text": event.text, "team": TEAM}),
            Some(user_in(USER, Some(OUTSIDE_TEAM))),
            None,
        ),
        (
            json!({"ts": ts, "user": USER, "text": event.text, "team": TEAM}),
            Some(user_in(USER, Some(TEAM))),
            Some(None),
        ),
    ] {
        let (server, surface) = confirming_setup(public_channel()).await;
        mount(
            &server,
            "conversations.history",
            ok(json!({"messages": [copy]})),
        )
        .await;
        let asks = info.is_some();
        if let Some(info) = info {
            mount_user(&server, USER, info).await;
        }
        let confirmed = surface.confirm(&event).await.unwrap();
        assert_eq!(
            confirmed.as_ref().map(|copy| copy.outside.clone()),
            outside,
            "a copy its fields leave home is dropped unless the lookup says home"
        );
        assert!(confirmed.is_none_or(|copy| copy.sender.team.as_str() == TEAM));
        assert_eq!(lookups(&server, "users.info").await.is_empty(), !asks);
    }
}

#[tokio::test]
async fn an_event_naming_the_organization_only_the_lookup_gives_is_dropped() {
    let mut envelope: Value = serde_json::from_str(testkit::slack::MESSAGE_MENTION).unwrap();
    envelope["event"]["user_team"] = json!(OUTSIDE_TEAM);
    let forged = event_from(&envelope.to_string());
    assert_eq!(
        forged.outside,
        outside_of(OUTSIDE_TEAM),
        "its fields name X"
    );
    let ts = forged.message.id.as_str();
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": forged.text, "team": TEAM}]})),
    )
    .await;
    mount_user(&server, USER, user_in(USER, Some(OUTSIDE_TEAM))).await;
    let logs = testkit::Logs::global();
    assert_eq!(surface.confirm(&forged).await, Ok(None));
    assert_eq!(lookups(&server, "users.info").await.len(), 1);
    let warned = logs
        .snapshot()
        .matching("isn't one of the workspace's members by Slack's lookup")
        .matching(forged.binding.to_string().as_str())
        .to_string();
    assert!(warned.contains("WARN"), "{warned}");
    assert!(warned.contains(OUTSIDE_TEAM), "{warned}");
    assert!(!warned.contains(forged.text.as_str()), "{warned}");
}

#[tokio::test]
async fn a_refused_home_lookup_drops_the_message_and_is_warned_of() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let ts = event.message.id.as_str();
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": event.text}]})),
    )
    .await;
    mount_user(&server, USER, refused("missing_scope")).await;
    let logs = testkit::Logs::global();
    for _ in 0..2 {
        assert_eq!(surface.confirm(&event).await, Ok(None));
    }
    assert_eq!(lookups(&server, "users.info").await.len(), 2, "not kept");
    let warned = logs
        .snapshot()
        .matching("wouldn't say whether the sender is one of the workspace's members")
        .matching(event.binding.to_string().as_str())
        .to_string();
    assert_eq!(
        warned.lines().filter(|line| line.contains("WARN")).count(),
        1,
        "once a minute: {warned}"
    );
    assert!(warned.contains("missing_scope"), "{warned}");
}

#[tokio::test]
async fn a_rate_limited_home_lookup_fails_the_confirmation_at_once() {
    let event = event_from(testkit::slack::MESSAGE_MENTION);
    let ts = event.message.id.as_str();
    let (server, surface) = confirming_setup(public_channel()).await;
    mount(
        &server,
        "conversations.history",
        ok(json!({"messages": [{"ts": ts, "user": USER, "text": event.text}]})),
    )
    .await;
    mount_user(
        &server,
        USER,
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
    )
    .await;
    let started = std::time::Instant::now();
    let err = surface.confirm(&event).await.unwrap_err();
    assert!(matches!(err, SurfaceError::RateLimited { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn conversation(extra: Value) -> ResponseTemplate {
    let mut channel = json!({"id": SHARED_CHANNEL, "is_channel": true, "is_member": true});
    for (key, value) in extra.as_object().unwrap() {
        channel[key] = value.clone();
    }
    ok(json!({"channel": channel}))
}

async fn info_of(extra: Value) -> ConvInfo {
    let (server, surface) = setup().await;
    mount(&server, "conversations.info", conversation(extra)).await;
    surface
        .directory()
        .conv_info(surface.api(), &SHARED_CHANNEL.into())
        .await
        .unwrap()
}

fn external(teams: Option<&[&str]>) -> Sharing {
    Sharing::External {
        teams: teams.map(|teams| teams.iter().map(|team| TeamId::from(*team)).collect()),
    }
}

#[tokio::test]
async fn conv_info_reads_sharing_and_connected_teams() {
    let both = [TEAM, OUTSIDE_TEAM];
    for (extra, kind, sharing) in [
        (json!({}), ConvKind::Channel, Sharing::None),
        (
            json!({"is_shared": false, "is_org_shared": false, "is_ext_shared": false}),
            ConvKind::Channel,
            Sharing::None,
        ),
        (
            json!({"is_shared": true, "is_org_shared": true}),
            ConvKind::Channel,
            Sharing::Org,
        ),
        (
            json!({"is_org_shared": true}),
            ConvKind::Channel,
            Sharing::Org,
        ),
        (
            json!({"is_shared": true, "is_ext_shared": true, "connected_team_ids": both}),
            ConvKind::Channel,
            external(Some(&both)),
        ),
        (
            json!({"is_ext_shared": true, "is_org_shared": true, "connected_team_ids": [HOME_ORG]}),
            ConvKind::Channel,
            external(Some(&[HOME_ORG])),
        ),
        (
            json!({"is_shared": true}),
            ConvKind::Channel,
            external(None),
        ),
        (
            json!({"is_ext_shared": true, "connected_team_ids": []}),
            ConvKind::Channel,
            external(Some(&[])),
        ),
        (
            json!({"is_ext_shared": "yes", "is_org_shared": null}),
            ConvKind::Channel,
            external(None),
        ),
        (json!({"is_shared": 1}), ConvKind::Channel, external(None)),
        (
            json!({"is_shared": null, "is_ext_shared": null, "is_org_shared": "yes"}),
            ConvKind::Channel,
            external(None),
        ),
        (
            json!({"is_shared": false, "is_ext_shared": false, "is_org_shared": null}),
            ConvKind::Channel,
            Sharing::None,
        ),
        (
            json!({"is_shared": "yes", "is_org_shared": true}),
            ConvKind::Channel,
            Sharing::Org,
        ),
        (
            json!({"is_channel": false, "is_mpim": true, "is_ext_shared": true}),
            ConvKind::GroupDm,
            external(None),
        ),
        (
            json!({"is_channel": false, "is_im": true}),
            ConvKind::Dm,
            Sharing::None,
        ),
    ] {
        let info = info_of(extra.clone()).await;
        assert_eq!(info, ConvInfo { kind, sharing }, "{extra}");
    }
}

#[tokio::test]
async fn a_malformed_or_overflowing_team_list_is_unknown() {
    let most: Vec<String> = (0..MAX_CONNECTED_TEAMS)
        .map(|n| format!("T0MANY{n:03}"))
        .collect();
    let mut too_many = most.clone();
    too_many.push("T0ONEMORE".to_owned());
    for teams in [
        json!([TEAM, "not a team"]),
        json!([TEAM, 7]),
        json!([TEAM, null]),
        json!([[TEAM]]),
        json!(TEAM),
        json!({"id": TEAM}),
        json!(null),
        json!(too_many),
    ] {
        let info = info_of(json!({"is_ext_shared": true, "connected_team_ids": teams})).await;
        assert_eq!(info.sharing, external(None), "{teams}");
        assert_eq!(info.kind, ConvKind::Channel);
    }
    let info = info_of(json!({"is_ext_shared": true, "connected_team_ids": most})).await;
    let Sharing::External { teams: Some(teams) } = info.sharing else {
        panic!("{:?}", info.sharing);
    };
    assert_eq!(teams.len(), MAX_CONNECTED_TEAMS);
}

#[tokio::test]
async fn conv_info_fresh_refreshes_the_cache() {
    let (server, surface) = setup().await;
    let directory = surface.directory();
    let channel = SHARED_CHANNEL.into();
    mount(&server, "conversations.info", conversation(json!({}))).await;
    let first = directory.conv_info(surface.api(), &channel).await.unwrap();
    assert_eq!(first.sharing, Sharing::None);

    server.reset().await;
    mount(
        &server,
        "conversations.info",
        conversation(json!({"is_ext_shared": true, "connected_team_ids": [TEAM, OUTSIDE_TEAM]})),
    )
    .await;
    assert_eq!(
        directory.conv_info(surface.api(), &channel).await.unwrap(),
        first,
        "kept until it is read fresh"
    );
    assert_eq!(
        directory.conv_kind(surface.api(), &channel).await.unwrap(),
        ConvKind::Channel
    );
    assert!(requests(&server).await.is_empty());
    let shared = external(Some(&[TEAM, OUTSIDE_TEAM]));
    let fresh = directory
        .conv_info_fresh(surface.api(), &channel)
        .await
        .unwrap();
    assert_eq!(fresh.sharing, shared);
    assert_eq!(
        directory
            .conv_info(surface.api(), &channel)
            .await
            .unwrap()
            .sharing,
        shared,
        "the fresh answer replaced the cached one"
    );
    assert_eq!(requests(&server).await.len(), 1);

    server.reset().await;
    mount(&server, "conversations.info", ResponseTemplate::new(503)).await;
    assert!(
        directory
            .conv_info_fresh(surface.api(), &channel)
            .await
            .is_err()
    );
    assert_eq!(
        directory
            .conv_info(surface.api(), &channel)
            .await
            .unwrap()
            .sharing,
        shared,
        "a failed fresh read leaves what was kept"
    );

    server.reset().await;
    mount(
        &server,
        "conversations.info",
        ok(json!({"channel": {"id": SHARED_CHANNEL.to_lowercase(), "is_channel": true}})),
    )
    .await;
    let err = directory
        .conv_info_fresh(surface.api(), &channel)
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::NotFound(_)), "{err:?}");
}
