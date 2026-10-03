//! `SlackSurface` against a wiremock Slack.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use core_types::{
    Binding, BindingId, ConvRef, Cursor, InboundEvent, LengthUnit, MemberKey, MsgRef, OutFile,
    ReplyTarget, SendError, Sender, Sink, Surface, SurfaceError, SurfaceKind, ThreadKey, UserId,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use surface_slack::normalize::{self, Context};
use surface_slack::surface::CAPS;
use surface_slack::{SlackClient, SlackSurface, TeamDirectory};
use testkit::Held;
use testkit::slack::{BOT_USER, CHANNEL, TEAM, USER};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

async fn mount(server: &MockServer, name: &str, response: ResponseTemplate) {
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
        posted,
        MsgRef {
            conv: conv(),
            id: "1727697700.000200".into()
        }
    );
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
}

#[tokio::test]
async fn edit_react_and_ephemeral_use_the_message_conversation() {
    let (server, surface) = setup().await;
    mount(&server, "chat.update", ok(json!({}))).await;
    mount(&server, "reactions.add", ok(json!({}))).await;
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
    let sent = requests(&server).await;
    let update: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        update,
        json!({"channel": CHANNEL, "ts": "1.2", "text": "edited"})
    );
    let reaction = form(&sent[1]);
    assert_eq!(reaction["name"], "eyes");
    assert_eq!(reaction["timestamp"], "1.2");
    let ephemeral: Value = serde_json::from_slice(&sent[2].body).unwrap();
    assert_eq!(ephemeral["user"], USER);
    assert_eq!(ephemeral["thread_ts"], "1.1");
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
    let dir = std::env::temp_dir().join(format!("surface-slack-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
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
