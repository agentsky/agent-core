//! The Web API client against a wiremock Slack.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use core_types::{ConversationId, MessageId, OutFile, SurfaceError, UserId};
use secrecy::SecretString;
use serde_json::{Value, json};
use surface_slack::web::{PageRequest, map_error};
use surface_slack::{SlackClient, WebApi};
use testkit::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "xoxb-test-0000-secret";

async fn server() -> (MockServer, WebApi) {
    let server = MockServer::start().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri()))
        .unwrap()
        .with_max_retry_wait(Duration::from_secs(5));
    let api = client.bot(SecretString::from(TOKEN));
    (server, api)
}

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

fn failed(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": code}))
}

async fn mount(server: &MockServer, name: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{name}")))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn form(request: &Request) -> HashMap<String, String> {
    assert_eq!(
        request.headers.get("content-type").unwrap(),
        "application/x-www-form-urlencoded"
    );
    serde_urlencoded::from_bytes(&request.body).unwrap()
}

fn json_body(request: &Request) -> Value {
    assert_eq!(
        request.headers.get("content-type").unwrap(),
        "application/json; charset=utf-8"
    );
    serde_json::from_slice(&request.body).unwrap()
}

/// The token goes only in the `Authorization` header.
fn assert_token_only_in_header(request: &Request) {
    assert!(!request.url.as_str().contains(TOKEN), "token in the URL");
    assert!(
        !String::from_utf8_lossy(&request.body).contains(TOKEN),
        "token in the body"
    );
}

fn channel() -> ConversationId {
    "C0CHAN001".into()
}

#[tokio::test]
async fn auth_test_reads_the_bot_identity() {
    let (server, api) = server().await;
    mount(
        &server,
        "auth.test",
        ok(json!({
            "url": "https://example.slack.com/",
            "team": "Example",
            "user": "helper",
            "team_id": "T0TEAM001",
            "user_id": "U0BOT0001",
            "bot_id": "B0BOT0001",
            "is_enterprise_install": false,
        })),
    )
    .await;
    let auth = api.auth_test().await.unwrap();
    assert_eq!(auth.team_id.as_str(), "T0TEAM001");
    assert_eq!(auth.user_id.as_str(), "U0BOT0001");
    assert_eq!(auth.bot_id.as_deref(), Some("B0BOT0001"));
    assert_eq!(auth.team.as_deref(), Some("Example"));
    let sent = requests(&server).await;
    assert_eq!(sent.len(), 1);
    assert_token_only_in_header(&sent[0]);
}

#[tokio::test]
async fn post_message_threads_without_unfurls_link_names_or_parse() {
    let (server, api) = server().await;
    mount(
        &server,
        "chat.postMessage",
        ok(json!({"channel": "C0CHAN001", "ts": "1727697700.000200", "message": {}})),
    )
    .await;
    let root = MessageId::from("1727697600.000100");
    let ts = api
        .post_message(&channel(), Some(&root), "*hi* <@U0HUMAN01>")
        .await
        .unwrap();
    assert_eq!(ts.as_str(), "1727697700.000200");
    api.post_message(&channel(), None, "top level")
        .await
        .unwrap();

    let sent = requests(&server).await;
    let threaded = json_body(&sent[0]);
    assert_eq!(
        threaded,
        json!({
            "channel": "C0CHAN001",
            "text": "*hi* <@U0HUMAN01>",
            "thread_ts": "1727697600.000100",
            "mrkdwn": true,
            "unfurl_links": false,
        })
    );
    let top = json_body(&sent[1]);
    assert!(top.get("thread_ts").is_none());
    for body in [&threaded, &top] {
        assert!(body.get("link_names").is_none(), "{body}");
        assert!(body.get("parse").is_none(), "{body}");
    }
    assert_token_only_in_header(&sent[0]);
}

#[tokio::test]
async fn update_message_sends_neither_link_names_nor_parse() {
    let (server, api) = server().await;
    mount(
        &server,
        "chat.update",
        ok(json!({"channel": "C0CHAN001", "ts": "1.2", "text": "new"})),
    )
    .await;
    api.update_message(&channel(), &"1.2".into(), "new")
        .await
        .unwrap();
    let body = json_body(&requests(&server).await[0]);
    assert_eq!(
        body,
        json!({"channel": "C0CHAN001", "ts": "1.2", "text": "new"})
    );
    assert!(body.get("link_names").is_none());
    assert!(body.get("parse").is_none());
}

#[tokio::test]
async fn post_ephemeral_targets_one_user() {
    let (server, api) = server().await;
    mount(
        &server,
        "chat.postEphemeral",
        ok(json!({"message_ts": "1727697800.000300"})),
    )
    .await;
    let ts = api
        .post_ephemeral(
            &channel(),
            &UserId::from("U0HUMAN01"),
            Some(&"1.1".into()),
            "only you",
        )
        .await
        .unwrap();
    assert_eq!(ts.as_str(), "1727697800.000300");
    let body = json_body(&requests(&server).await[0]);
    assert_eq!(
        body,
        json!({"channel": "C0CHAN001", "user": "U0HUMAN01", "thread_ts": "1.1", "text": "only you"})
    );
}

#[tokio::test]
async fn reactions_are_idempotent() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/reactions.add"))
        .respond_with(ok(json!({})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, "reactions.add", failed("already_reacted")).await;
    mount(&server, "reactions.remove", failed("no_reaction")).await;

    let ts = MessageId::from("1.2");
    api.add_reaction(&channel(), &ts, ":eyes:").await.unwrap();
    api.add_reaction(&channel(), &ts, "eyes").await.unwrap();
    api.remove_reaction(&channel(), &ts, "eyes").await.unwrap();

    let sent = requests(&server).await;
    assert_eq!(sent.len(), 3);
    for request in &sent {
        let form = form(request);
        assert_eq!(form["channel"], "C0CHAN001");
        assert_eq!(form["timestamp"], "1.2");
        assert_eq!(form["name"], "eyes");
    }
}

#[tokio::test]
async fn a_reaction_on_a_missing_message_still_fails() {
    let (server, api) = server().await;
    mount(&server, "reactions.add", failed("message_not_found")).await;
    let err = api
        .add_reaction(&channel(), &"1.2".into(), "eyes")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::NotFound("message_not_found".into()));
}

#[tokio::test]
async fn replies_and_history_read_pages() {
    let (server, api) = server().await;
    mount(
        &server,
        "conversations.replies",
        ok(json!({
            "messages": [
                {"type": "message", "user": "U0HUMAN01", "text": "root", "ts": "1.1", "thread_ts": "1.1"},
                {"type": "message", "bot_id": "B1", "text": "bot", "ts": "1.2", "thread_ts": "1.1",
                 "files": [{"id": "F1", "name": "a.txt", "url_private": "https://files.slack.com/F1"}]},
            ],
            "has_more": true,
            "response_metadata": {"next_cursor": "bmV4dA=="},
        })),
    )
    .await;
    mount(
        &server,
        "conversations.history",
        ok(json!({
            "messages": [{"type": "message", "subtype": "bot_message", "text": "legacy", "ts": "2.1"}],
            "has_more": false,
            "response_metadata": {"next_cursor": ""},
        })),
    )
    .await;

    let root = MessageId::from("1.1");
    let page = api
        .replies(
            &channel(),
            &root,
            PageRequest {
                latest: Some("1.9"),
                cursor: Some("abc"),
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.next_cursor.as_deref(), Some("bmV4dA=="));
    assert_eq!(page.messages.len(), 2);
    assert_eq!(
        page.messages[0].user.as_ref().unwrap().as_str(),
        "U0HUMAN01"
    );
    assert!(!page.messages[0].is_bot);
    assert_eq!(page.messages[1].bot_id.as_deref(), Some("B1"));
    assert!(page.messages[1].is_bot);
    assert_eq!(page.messages[1].files[0].id, "F1");

    let history = api
        .history(&channel(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(history.next_cursor, None);
    assert!(history.messages[0].is_bot);
    assert_eq!(history.messages[0].subtype.as_deref(), Some("bot_message"));

    let sent = requests(&server).await;
    let replies = form(&sent[0]);
    assert_eq!(replies["channel"], "C0CHAN001");
    assert_eq!(replies["ts"], "1.1");
    assert_eq!(replies["latest"], "1.9");
    assert_eq!(replies["inclusive"], "false");
    assert_eq!(replies["cursor"], "abc");
    assert_eq!(replies["limit"], "50");
    let history = form(&sent[1]);
    assert_eq!(history["channel"], "C0CHAN001");
    assert_eq!(history["limit"], "200");
    assert!(!history.contains_key("latest"));
}

#[tokio::test]
async fn conversation_info_and_join() {
    let (server, api) = server().await;
    let channel_json = json!({"channel": {
        "id": "C0CHAN001", "name": "general", "is_channel": true, "is_private": false,
        "is_member": true, "is_archived": false,
    }});
    mount(&server, "conversations.info", ok(channel_json.clone())).await;
    mount(&server, "conversations.join", ok(channel_json)).await;
    let info = api.conversation_info(&channel()).await.unwrap();
    assert_eq!(info.name.as_deref(), Some("general"));
    assert!(info.is_channel && info.is_member && !info.is_im);
    let joined = api.join(&channel()).await.unwrap();
    assert_eq!(joined.id, channel());
    for request in requests(&server).await {
        assert_eq!(form(&request)["channel"], "C0CHAN001");
    }
}

#[tokio::test]
async fn join_of_a_private_channel_is_refused() {
    let (server, api) = server().await;
    mount(
        &server,
        "conversations.join",
        failed("method_not_supported_for_channel_type"),
    )
    .await;
    let err = api.join(&"G0PRIV001".into()).await.unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Forbidden("method_not_supported_for_channel_type".into())
    );
}

#[tokio::test]
async fn users_info_and_every_page_of_users_list() {
    let (server, api) = server().await;
    mount(
        &server,
        "users.info",
        ok(json!({"user": {
            "id": "U0HUMAN01", "team_id": "T0TEAM001", "name": "ada", "real_name": "Ada Lovelace",
            "deleted": false, "is_bot": false,
            "profile": {"display_name": "Ada", "real_name": "Ada Lovelace", "email": "ada@example.com"},
        }})),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(wiremock::matchers::body_string_contains("cursor=page2"))
        .respond_with(ok(json!({
            "members": [{"id": "U2", "name": "grace"}],
            "response_metadata": {"next_cursor": ""},
        })))
        .mount(&server)
        .await;
    mount(
        &server,
        "users.list",
        ok(json!({
            "members": [{"id": "U1", "name": "ada", "profile": {"display_name": "Ada"}}],
            "response_metadata": {"next_cursor": "page2"},
        })),
    )
    .await;

    let user = api.user_info(&"U0HUMAN01".into()).await.unwrap();
    assert_eq!(user.profile.display_name.as_deref(), Some("Ada"));
    assert_eq!(user.team_id.unwrap().as_str(), "T0TEAM001");

    let users = api.all_users().await.unwrap();
    let ids: Vec<&str> = users.iter().map(|user| user.id.as_str()).collect();
    assert_eq!(ids, ["U1", "U2"]);

    let sent = requests(&server).await;
    assert_eq!(form(&sent[0])["user"], "U0HUMAN01");
    let first = form(&sent[1]);
    assert_eq!(first["limit"], "999");
    assert!(!first.contains_key("cursor"));
    assert_eq!(form(&sent[2])["cursor"], "page2");
}

#[tokio::test]
async fn bots_info_reads_the_bot_user() {
    let (server, api) = server().await;
    mount(
        &server,
        "bots.info",
        ok(json!({"bot": {
            "id": "B0OTHER01", "deleted": false, "name": "other", "updated": 1727600000,
            "app_id": "A0OTHER01", "user_id": "U0OTHER01", "icons": {},
        }})),
    )
    .await;
    let bot = api.bot_info("B0OTHER01").await.unwrap();
    assert_eq!(bot.user_id, Some(UserId::from("U0OTHER01")));
    assert_eq!(bot.app_id.as_deref(), Some("A0OTHER01"));
    assert_eq!(form(&requests(&server).await[0])["bot"], "B0OTHER01");
}

fn staged(dir: &TempDir, name: &str, contents: &str) -> OutFile {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    OutFile {
        name: name.into(),
        path,
    }
}

async fn mount_upload_flow(server: &MockServer) {
    for (id, upload) in [("F0FILE001", "one"), ("F0FILE002", "two")] {
        Mock::given(method("POST"))
            .and(path("/api/files.getUploadURLExternal"))
            .respond_with(ok(json!({
                "upload_url": format!("{}/upload/v1/{upload}", server.uri()),
                "file_id": id,
            })))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/files.completeUploadExternal"))
        .respond_with(ok(
            json!({"files": [{"id": "F0FILE001"}, {"id": "F0FILE002"}]}),
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn upload_runs_the_external_flow_in_order() {
    let (server, api) = server().await;
    mount_upload_flow(&server).await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex("^/upload/v1/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK - 5"))
        .mount(&server)
        .await;
    let dir = TempDir::new("surface-slack");
    let files = [
        staged(&dir, "report.txt", "hello"),
        staged(&dir, "data.csv", "a,b\n1,2\n"),
    ];
    let ids = api
        .upload_files(&channel(), Some(&"1.1".into()), &files)
        .await
        .unwrap();
    assert_eq!(ids, ["F0FILE001", "F0FILE002"]);

    let sent = requests(&server).await;
    let paths: Vec<&str> = sent.iter().map(|r| r.url.path()).collect();
    assert_eq!(
        paths,
        [
            "/api/files.getUploadURLExternal",
            "/upload/v1/one",
            "/api/files.getUploadURLExternal",
            "/upload/v1/two",
            "/api/files.completeUploadExternal",
        ]
    );
    let first = form(&sent[0]);
    assert_eq!(first["filename"], "report.txt");
    assert_eq!(first["length"], "5");
    assert_eq!(form(&sent[2])["length"], "8");

    let upload = &sent[1];
    assert!(upload.headers.get("authorization").is_none());
    assert_eq!(
        upload.headers.get("content-type").unwrap(),
        "application/octet-stream"
    );
    assert_eq!(upload.body, b"hello");
    assert_eq!(sent[3].body, b"a,b\n1,2\n");

    let complete = form(&sent[4]);
    assert_eq!(complete["channel_id"], "C0CHAN001");
    assert_eq!(complete["thread_ts"], "1.1");
    let shared: Value = serde_json::from_str(&complete["files"]).unwrap();
    assert_eq!(
        shared,
        json!([
            {"id": "F0FILE001", "title": "report.txt"},
            {"id": "F0FILE002", "title": "data.csv"},
        ])
    );
    for request in &sent {
        assert_token_only_in_header(request);
    }
}

#[tokio::test]
async fn a_refused_upload_shares_nothing() {
    let (server, api) = server().await;
    mount_upload_flow(&server).await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex("^/upload/v1/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = TempDir::new("surface-slack");
    let err = api
        .upload_files(&channel(), None, &[staged(&dir, "a.txt", "x")])
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Api("the file upload was refused (HTTP 500)".into())
    );
    let sent = requests(&server).await;
    assert!(
        sent.iter()
            .all(|r| r.url.path() != "/api/files.completeUploadExternal")
    );
}

#[tokio::test]
async fn an_upload_of_nothing_or_of_a_missing_file_sends_nothing() {
    let (server, api) = server().await;
    assert_eq!(
        api.upload_files(&channel(), None, &[]).await.unwrap(),
        Vec::<String>::new()
    );
    let dir = TempDir::new("surface-slack");
    let missing = OutFile {
        name: "gone.txt".into(),
        path: dir.join("gone.txt"),
    };
    let err = api
        .upload_files(&channel(), None, &[missing])
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::NotFound("file to upload".into()));
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn a_429_is_retried_after_retry_after() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(json!({"ok": false, "error": "ratelimited"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, "users.info", ok(json!({"user": {"id": "U1"}}))).await;
    let started = Instant::now();
    let user = api.user_info(&"U1".into()).await.unwrap();
    assert_eq!(user.id.as_str(), "U1");
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test]
async fn a_long_retry_after_fails_at_once_and_holds_later_calls() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&server)
        .await;
    let started = Instant::now();
    let err = api
        .update_message(&channel(), &"1.2".into(), "x")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::RateLimited {
            retry_after: Duration::from_secs(120)
        }
    );
    let again = api
        .update_message(&channel(), &"1.2".into(), "x")
        .await
        .unwrap_err();
    assert!(
        matches!(again, SurfaceError::RateLimited { retry_after } if retry_after > Duration::from_secs(100)),
        "{again:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        requests(&server).await.len(),
        1,
        "the held call wasn't sent"
    );
}

#[tokio::test]
async fn rate_limits_are_retried_a_bounded_number_of_times() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("retry-after", "0")
                .set_body_json(json!({"ok": false, "error": "ratelimited"})),
        )
        .mount(&server)
        .await;
    let err = api.conversation_info(&channel()).await.unwrap_err();
    assert_eq!(
        err,
        SurfaceError::RateLimited {
            retry_after: Duration::ZERO
        }
    );
    let tries = requests(&server).await.len();
    assert_eq!(tries, 1 + surface_slack::web::MAX_RETRIES as usize);
}

#[tokio::test]
async fn ok_false_with_http_200_maps_to_surface_errors() {
    let cases = [
        ("invalid_auth", SurfaceError::Unauthorized),
        ("not_authed", SurfaceError::Unauthorized),
        ("token_revoked", SurfaceError::Unauthorized),
        (
            "not_in_channel",
            SurfaceError::Forbidden("not_in_channel".into()),
        ),
        ("is_archived", SurfaceError::Forbidden("is_archived".into())),
        (
            "channel_not_found",
            SurfaceError::NotFound("channel_not_found".into()),
        ),
        ("msg_too_long", SurfaceError::Api("msg_too_long".into())),
        ("<b>html</b>", SurfaceError::Api("unknown_error".into())),
    ];
    for (code, expected) in cases {
        let (server, api) = server().await;
        mount(&server, "chat.postMessage", failed(code)).await;
        let err = api.post_message(&channel(), None, "x").await.unwrap_err();
        assert_eq!(err, expected, "{code}");
        assert!(!err.to_string().contains(TOKEN));
    }
    let (server, api) = server().await;
    mount(
        &server,
        "chat.postMessage",
        ResponseTemplate::new(200).set_body_json(json!({
            "ok": false, "error": "missing_scope", "needed": "chat:write", "provided": "users:read",
        })),
    )
    .await;
    let err = api.post_message(&channel(), None, "x").await.unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Forbidden("missing_scope (needs chat:write)".into())
    );
    assert_eq!(err, map_error("missing_scope", Some("chat:write")));
}

#[tokio::test]
async fn http_failures_and_unreadable_bodies_are_errors() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "https://evil.example/"))
        .mount(&server)
        .await;
    mount(&server, "conversations.info", ok(json!({"channel": 7}))).await;

    assert_eq!(
        api.auth_test().await.unwrap_err(),
        SurfaceError::Api("HTTP 503".into())
    );
    assert!(matches!(
        api.user_info(&"U1".into()).await.unwrap_err(),
        SurfaceError::Transport(text) if text.starts_with("unreadable response")
    ));
    assert_eq!(
        api.bot_info("B1").await.unwrap_err(),
        SurfaceError::Api("HTTP 302".into())
    );
    let err = api.conversation_info(&channel()).await.unwrap_err();
    assert!(
        matches!(&err, SurfaceError::Transport(text) if text.starts_with("unexpected response from conversations.info")),
        "{err:?}"
    );
    assert_eq!(requests(&server).await.len(), 4, "no redirect was followed");
}

#[tokio::test]
async fn an_unreachable_slack_is_a_transport_error_without_the_token() {
    let client = SlackClient::new("http://127.0.0.1:9/api/").unwrap();
    let err = client
        .bot(SecretString::from(TOKEN))
        .auth_test()
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::Transport(_)), "{err:?}");
    assert!(!err.to_string().contains(TOKEN));
    assert!(!err.to_string().contains("127.0.0.1"), "{err}");
}

#[tokio::test]
async fn a_token_with_a_line_break_is_refused_without_repeating_it() {
    let (server, _) = server().await;
    let client = SlackClient::new(&format!("{}/api/", server.uri())).unwrap();
    let err = client
        .bot(SecretString::from("xoxb-bad\nsecret"))
        .auth_test()
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::Api(_)));
    assert!(!err.to_string().contains("secret"));
    assert!(requests(&server).await.is_empty());
}

mod response_url {
    use super::*;

    async fn client_and_url(server: &MockServer) -> (SlackClient, SecretString) {
        let client = SlackClient::new("https://slack.com/api/").unwrap();
        let url = SecretString::from(format!("{}/commands/T1/123/secret-hook", server.uri()));
        (client, url)
    }

    #[tokio::test]
    async fn replies_ephemerally_without_a_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/commands/T1/123/secret-hook"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let (client, url) = client_and_url(&server).await;
        client.respond_ephemeral(&url, "*done*").await.unwrap();
        let sent = requests(&server).await;
        assert!(sent[0].headers.get("authorization").is_none());
        let body = json_body(&sent[0]);
        assert_eq!(
            body,
            json!({"response_type": "ephemeral", "text": "*done*"})
        );
    }

    #[tokio::test]
    async fn a_plain_ok_body_is_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let (client, url) = client_and_url(&server).await;
        client.respond_ephemeral(&url, "x").await.unwrap();
    }

    #[tokio::test]
    async fn failures_map_and_never_show_the_url() {
        let cases = [
            (
                ResponseTemplate::new(404).set_body_string("expired_url"),
                SurfaceError::NotFound("expired_url".into()),
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "used_url"})),
                SurfaceError::NotFound("used_url".into()),
            ),
            (
                ResponseTemplate::new(410),
                SurfaceError::NotFound("HTTP 410".into()),
            ),
            (
                ResponseTemplate::new(400).set_body_string("no_text"),
                SurfaceError::Api("no_text".into()),
            ),
            (
                ResponseTemplate::new(500).set_body_string("Internal <b>error</b>"),
                SurfaceError::Api("unknown_error".into()),
            ),
            (
                ResponseTemplate::new(502),
                SurfaceError::Api("HTTP 502".into()),
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"ok": false})),
                SurfaceError::Api("unknown_error".into()),
            ),
            (
                ResponseTemplate::new(429).insert_header("retry-after", "7"),
                SurfaceError::RateLimited {
                    retry_after: Duration::from_secs(7),
                },
            ),
        ];
        for (response, expected) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            let (client, url) = client_and_url(&server).await;
            let err = client.respond_ephemeral(&url, "x").await.unwrap_err();
            assert_eq!(err, expected);
            assert!(!err.to_string().contains("secret-hook"));
        }
    }

    #[tokio::test]
    async fn a_malformed_url_is_refused_without_repeating_it() {
        let client = SlackClient::new("https://slack.com/api/").unwrap();
        for url in ["not a url secret-hook", "file:///etc/secret-hook"] {
            let err = client
                .respond_ephemeral(&SecretString::from(url), "x")
                .await
                .unwrap_err();
            assert!(matches!(err, SurfaceError::Api(_)));
            assert!(!err.to_string().contains("secret-hook"));
        }
        let unreachable = SecretString::from("http://127.0.0.1:9/secret-hook");
        let err = client
            .respond_ephemeral(&unreachable, "x")
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Transport(_)));
        assert!(!err.to_string().contains("secret-hook"));
    }
}

mod config_tokens {
    use super::*;

    const REFRESH: &str = "xoxe-1-REFRESH-secret-0001";
    const NEW_TOKEN: &str = "xoxe.xoxp-1-NEWTOKEN-secret-0002";
    const NEW_REFRESH: &str = "xoxe-1-NEWREFRESH-secret-0003";

    async fn client(server: &MockServer) -> SlackClient {
        SlackClient::new(&format!("{}/api/", server.uri()))
            .unwrap()
            .with_max_retry_wait(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn rotate_sends_the_refresh_token_in_the_body_only() {
        use secrecy::ExposeSecret as _;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/tooling.tokens.rotate"))
            .respond_with(ok(json!({
                "token": NEW_TOKEN,
                "refresh_token": NEW_REFRESH,
                "team_id": "T0TEAM001",
                "user_id": "U0HUMAN01",
                "iat": 1_727_700_000,
                "exp": 1_727_743_200,
            })))
            .mount(&server)
            .await;
        let rotated = client(&server)
            .await
            .rotate_config_token(&SecretString::from(REFRESH))
            .await
            .unwrap();
        assert_eq!(rotated.token.expose_secret(), NEW_TOKEN);
        assert_eq!(rotated.refresh_token.expose_secret(), NEW_REFRESH);
        assert_eq!(rotated.team.as_str(), "T0TEAM001");
        assert_eq!(rotated.user.as_str(), "U0HUMAN01");
        assert_eq!(rotated.expires_at.unix_timestamp(), 1_727_743_200);
        let debug = format!("{rotated:?}");
        for secret in [NEW_TOKEN, NEW_REFRESH] {
            assert!(!debug.contains(secret), "{debug}");
        }

        let sent = requests(&server).await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].headers.get("authorization").is_none());
        assert!(!sent[0].url.as_str().contains(REFRESH));
        assert_eq!(form(&sent[0])["refresh_token"], REFRESH);
    }

    #[tokio::test]
    async fn a_refused_refresh_token_is_unauthorized_without_repeating_it() {
        for code in ["invalid_refresh_token", "token_revoked", "invalid_auth"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(failed(code))
                .mount(&server)
                .await;
            let err = client(&server)
                .await
                .rotate_config_token(&SecretString::from(REFRESH))
                .await
                .unwrap_err();
            assert_eq!(err, SurfaceError::Unauthorized, "{code}");
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ok(json!({"token": NEW_TOKEN})))
            .mount(&server)
            .await;
        let err = client(&server)
            .await
            .rotate_config_token(&SecretString::from(REFRESH))
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Transport(_)), "{err:?}");
        let text = format!("{err} {err:?}");
        for secret in [REFRESH, NEW_TOKEN] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_slack_never_shows_the_refresh_token() {
        let client = SlackClient::new("http://127.0.0.1:9/api/").unwrap();
        let err = client
            .rotate_config_token(&SecretString::from(REFRESH))
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Transport(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains(REFRESH));
    }
}

#[tokio::test]
async fn open_dm_returns_the_channel() {
    let (opened, api) = server().await;
    mount(
        &opened,
        "conversations.open",
        ok(json!({"channel": {"id": "D0DM00001"}})),
    )
    .await;
    let channel = api.open_dm(&UserId::new("U0HUMAN01")).await.unwrap();
    assert_eq!(channel.as_str(), "D0DM00001");
    let sent = requests(&opened).await;
    assert_eq!(form(&sent[0])["users"], "U0HUMAN01");
    assert_token_only_in_header(&sent[0]);

    let (missing, api) = server().await;
    mount(&missing, "conversations.open", failed("user_not_found")).await;
    let err = api.open_dm(&UserId::new("U0GONE")).await.unwrap_err();
    assert_eq!(err, SurfaceError::NotFound("user_not_found".into()));
}

mod downloads {
    use core_types::InFile;

    use super::*;

    fn file(url: String, size: Option<u64>) -> InFile {
        InFile {
            id: "F0FILE001".into(),
            name: "persona.md".into(),
            mime_type: Some("text/markdown".into()),
            size,
            url,
        }
    }

    async fn serve(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/files-pri/T0-F0FILE001/download/persona.md"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn url(server: &MockServer) -> String {
        format!(
            "{}/files-pri/T0-F0FILE001/download/persona.md",
            server.uri()
        )
    }

    #[tokio::test]
    async fn a_file_is_downloaded_with_the_bot_token() {
        let (server, api) = server().await;
        serve(
            &server,
            ResponseTemplate::new(200).set_body_string("You are terse."),
        )
        .await;
        let data = api
            .download_file(&file(url(&server), Some(14)), 1024)
            .await
            .unwrap();
        assert_eq!(data, b"You are terse.");
        let sent = requests(&server).await;
        assert_eq!(
            sent[0].headers.get("authorization").unwrap(),
            format!("Bearer {TOKEN}").as_str()
        );
    }

    #[tokio::test]
    async fn a_file_over_the_limit_is_refused() {
        let (server, api) = server().await;
        serve(
            &server,
            ResponseTemplate::new(200).set_body_string("x".repeat(100)),
        )
        .await;
        let err = api
            .download_file(&file(url(&server), Some(100)), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Api(_)), "{err:?}");
        assert!(
            requests(&server).await.is_empty(),
            "the declared size is checked first"
        );

        let err = api
            .download_file(&file(url(&server), None), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Api(_)), "{err:?}");
        assert!(!err.to_string().contains("files-pri"));
    }

    #[tokio::test]
    async fn a_refused_download_is_an_error_without_the_url() {
        let (server, api) = server().await;
        serve(
            &server,
            ResponseTemplate::new(302).insert_header("location", "https://example.slack.com/"),
        )
        .await;
        let err = api
            .download_file(&file(url(&server), None), 1024)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SurfaceError::Api("the file download was refused (HTTP 302)".into())
        );
    }

    #[tokio::test]
    async fn the_token_goes_only_to_slack() {
        let (server, api) = server().await;
        for url in [
            "https://files.slack.com.evil.example/x".to_owned(),
            "https://evilslack.com/x".to_owned(),
            "http://files.slack.com/x".to_owned(),
            "https://files.slack.com:8443/x".to_owned(),
            "not a url".to_owned(),
            format!("{}/x", server.uri().replace("127.0.0.1", "localhost")),
        ] {
            let err = api
                .download_file(&file(url.clone(), None), 1024)
                .await
                .unwrap_err();
            assert_eq!(
                err,
                SurfaceError::Api("the file's URL is not a Slack URL".into()),
                "{url}"
            );
        }
        assert!(requests(&server).await.is_empty());
    }
}
