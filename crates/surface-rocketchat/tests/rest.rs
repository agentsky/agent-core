//! Tests of the REST client against `testkit::rocketchat::FakeRest`.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use core_types::{ConversationId, MessageId, OutFile, SurfaceError, UserId};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use surface_rocketchat::rest::{Credentials, Message, NewBotUser, RestClient, RoomType};
use testkit::TempDir;
use testkit::rocketchat::FakeRest;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn manager(fake: &FakeRest) -> RestClient {
    RestClient::new(
        &fake.uri(),
        Credentials {
            user_id: FakeRest::MANAGER_ID.into(),
            token: SecretString::from(FakeRest::MANAGER_TOKEN),
        },
    )
    .unwrap()
}

fn body(request: &wiremock::Request) -> Value {
    request.body_json().unwrap()
}

fn header<'a>(request: &'a wiremock::Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

fn bot_user<'a>(username: &'a str) -> NewBotUser<'a> {
    NewBotUser {
        username,
        name: "Helper",
        email: "helper@bots.invalid",
    }
}

/// Creates a bot and returns a client acting as it.
async fn bot(client: &RestClient, username: &str) -> RestClient {
    let (user, password) = client.create_bot_user(&bot_user(username)).await.unwrap();
    let creds = client
        .issue_bot_token(&user.username, password, "agentd")
        .await
        .unwrap();
    client.with_credentials(creds)
}

fn conv(id: &str) -> ConversationId {
    id.into()
}

#[tokio::test]
async fn me_sends_the_auth_headers_and_reads_the_user() {
    let fake = FakeRest::start().await;
    let me = manager(&fake).me().await.unwrap();
    assert_eq!(me.id.as_str(), FakeRest::MANAGER_ID);
    assert_eq!(me.username, FakeRest::MANAGER_USERNAME);
    assert_eq!(me.active, Some(true));
    let requests = fake.requests("me").await;
    assert_eq!(
        header(&requests[0], "x-user-id"),
        Some(FakeRest::MANAGER_ID)
    );
    assert_eq!(
        header(&requests[0], "x-auth-token"),
        Some(FakeRest::MANAGER_TOKEN)
    );
}

#[tokio::test]
async fn a_rejected_token_is_unauthorized() {
    let fake = FakeRest::start().await;
    let client = manager(&fake).with_credentials(Credentials {
        user_id: FakeRest::MANAGER_ID.into(),
        token: SecretString::from("wrong"),
    });
    assert_eq!(client.me().await, Err(SurfaceError::Unauthorized));
}

#[tokio::test]
async fn create_bot_user_sends_the_bot_role_and_a_random_password() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let (user, _password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    assert_eq!(user.username, "helper");
    assert_eq!(user.roles, ["bot"]);
    let (_, _other) = client.create_bot_user(&bot_user("helper2")).await.unwrap();

    let requests = fake.requests("users.create").await;
    let first = body(&requests[0]);
    assert_eq!(first["roles"], serde_json::json!(["bot"]));
    assert_eq!(first["verified"], false);
    assert_eq!(first["joinDefaultChannels"], false);
    assert_eq!(first["requirePasswordChange"], false);
    assert_eq!(first["sendWelcomeEmail"], false);
    assert_eq!(first["email"], "helper@bots.invalid");
    assert!(
        first.get("active").is_none(),
        "active needs another permission"
    );
    let password = first["password"].as_str().unwrap();
    assert_eq!(password.len(), 48);
    assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(password, body(&requests[1])["password"].as_str().unwrap());
}

#[tokio::test]
async fn a_taken_username_is_an_api_error_naming_the_code() {
    let fake = FakeRest::start().await;
    fake.add_user("helper");
    let err = manager(&fake)
        .create_bot_user(&bot_user("helper"))
        .await
        .unwrap_err();
    let SurfaceError::Api(text) = err else {
        panic!("expected an API error, got {err:?}");
    };
    assert!(text.contains("error-field-unavailable"), "{text}");
}

#[tokio::test]
async fn create_bot_user_without_permission_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.fail(
        "users.create",
        400,
        "Adding user is not allowed [error-action-not-allowed]",
        Some("error-action-not-allowed"),
    )
    .await;
    let err = manager(&fake)
        .create_bot_user(&bot_user("helper"))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Forbidden("error-action-not-allowed".into())
    );
}

#[tokio::test]
async fn issue_bot_token_logs_in_generates_a_token_and_logs_out() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let (user, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    let creds = client
        .issue_bot_token("helper", password, "agentd")
        .await
        .unwrap();
    assert_eq!(creds.user_id, user.id);
    assert_eq!(
        fake.token_user(creds.token.expose_secret()),
        Some(user.id.to_string())
    );

    let created = body(&fake.requests("users.create").await[0]);
    let login = &fake.requests("login").await[0];
    assert!(header(login, "x-auth-token").is_none());
    assert_eq!(body(login)["user"], "helper");
    assert_eq!(body(login)["password"], created["password"]);

    let generate = &fake.requests("users.generatePersonalAccessToken").await[0];
    assert_eq!(body(generate)["tokenName"], "agentd");
    assert_eq!(body(generate)["bypassTwoFactor"], true);
    assert_eq!(header(generate, "x-user-id"), Some(user.id.as_str()));
    let session = header(generate, "x-auth-token").unwrap().to_owned();
    assert!(session.starts_with("login-"));
    assert_eq!(header(generate, "x-2fa-method"), Some("password"));
    let code = header(generate, "x-2fa-code").unwrap();
    assert_eq!(code.len(), 64);
    assert!(
        code.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    let logout = &fake.requests("logout").await[0];
    assert_eq!(header(logout, "x-auth-token"), Some(session.as_str()));
    assert_eq!(fake.token_user(&session), None, "login session is revoked");

    let as_bot = client.with_credentials(creds);
    assert_eq!(as_bot.me().await.unwrap().username, "helper");
    assert_eq!(as_bot.user_id(), &user.id);
}

#[tokio::test]
async fn the_2fa_code_is_the_sha256_of_the_password() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let (_, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    client
        .issue_bot_token("helper", password, "agentd")
        .await
        .unwrap();
    let sent = body(&fake.requests("users.create").await[0])["password"]
        .as_str()
        .unwrap()
        .to_owned();
    let generate = &fake.requests("users.generatePersonalAccessToken").await[0];
    use sha2::Digest;
    let expected: String = sha2::Sha256::digest(sent.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(header(generate, "x-2fa-code"), Some(expected.as_str()));
}

#[tokio::test]
async fn a_failed_login_is_unauthorized() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let (_, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    let err = client
        .issue_bot_token("nobody", password, "agentd")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Unauthorized);
    assert!(
        fake.requests("users.generatePersonalAccessToken")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_bot_role_without_token_permission_is_forbidden_and_still_logs_out() {
    let fake = FakeRest::start().await;
    fake.fail(
        "users.generatePersonalAccessToken",
        400,
        "Not Authorized [not-authorized]",
        Some("not-authorized"),
    )
    .await;
    let client = manager(&fake);
    let (_, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    let err = client
        .issue_bot_token("helper", password, "agentd")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Forbidden("not-authorized".into()));
    let logout = &fake.requests("logout").await[0];
    let session = header(logout, "x-auth-token").unwrap();
    assert_eq!(fake.token_user(session), None);
}

#[tokio::test]
async fn a_failed_logout_does_not_lose_the_token() {
    let fake = FakeRest::start().await;
    fake.fail("logout", 500, "Internal server error", None)
        .await;
    let client = manager(&fake);
    let (_, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    let creds = client
        .issue_bot_token("helper", password, "agentd")
        .await
        .unwrap();
    assert!(fake.token_user(creds.token.expose_secret()).is_some());
}

#[tokio::test]
async fn set_avatar_sends_the_url() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let id = fake.add_user("helper");
    client
        .set_avatar(&id.as_str().into(), "https://example.com/a.png")
        .await
        .unwrap();
    assert_eq!(
        fake.user("helper").unwrap().avatar_url.as_deref(),
        Some("https://example.com/a.png")
    );
    let err = client
        .set_avatar(&"missing".into(), "https://example.com/a.png")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::NotFound("error-invalid-user".into()));
}

#[tokio::test]
async fn set_avatar_without_permission_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.fail("users.setAvatar", 403, "unauthorized", None)
        .await;
    let err = manager(&fake)
        .set_avatar(&"u1".into(), "https://example.com/a.png")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Forbidden("unauthorized".into()));
}

#[tokio::test]
async fn set_name_updates_the_display_name() {
    let fake = FakeRest::start().await;
    let id = fake.add_user("helper");
    manager(&fake)
        .set_name(&id.as_str().into(), "Helpful Bot")
        .await
        .unwrap();
    assert_eq!(fake.user("helper").unwrap().name, "Helpful Bot");
    let request = &fake.requests("users.update").await[0];
    assert_eq!(body(request)["data"]["name"], "Helpful Bot");
}

#[tokio::test]
async fn set_name_needing_two_factor_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.fail(
        "users.update",
        400,
        "TOTP Required [totp-required]",
        Some("totp-required"),
    )
    .await;
    let err = manager(&fake)
        .set_name(&"u1".into(), "x")
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Forbidden("totp-required".into()));
}

#[tokio::test]
async fn set_active_deactivates_and_the_bot_token_stops_working() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let as_bot = bot(&client, "helper").await;
    client.set_active(as_bot.user_id(), false).await.unwrap();
    assert!(!fake.user("helper").unwrap().active);
    assert_eq!(
        body(&fake.requests("users.setActiveStatus").await[0])["activeStatus"],
        false
    );
    assert_eq!(as_bot.me().await, Err(SurfaceError::Unauthorized));
    client.set_active(as_bot.user_id(), true).await.unwrap();
    assert!(as_bot.me().await.is_ok());
}

#[tokio::test]
async fn set_active_without_permission_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.fail(
        "users.setActiveStatus",
        403,
        "User does not have the permissions required for this action [error-unauthorized]",
        None,
    )
    .await;
    let err = manager(&fake)
        .set_active(&"u1".into(), false)
        .await
        .unwrap_err();
    assert_eq!(err, SurfaceError::Forbidden("error-unauthorized".into()));
}

#[tokio::test]
async fn invite_uses_the_endpoint_for_the_room_type() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    fake.add_room("G1", "p", "secret");
    let user = fake.add_user("helper");
    let client = manager(&fake);
    let user_id: UserId = user.as_str().into();
    client
        .invite(&conv("C1"), &RoomType::Channel, &user_id)
        .await
        .unwrap();
    client
        .invite(&conv("G1"), &RoomType::Group, &user_id)
        .await
        .unwrap();
    assert!(fake.members("C1").contains(&user));
    assert!(fake.members("G1").contains(&user));
    assert_eq!(
        body(&fake.requests("channels.invite").await[0])["roomId"],
        "C1"
    );
    assert_eq!(
        body(&fake.requests("groups.invite").await[0])["userId"],
        user
    );

    let err = client
        .invite(&conv("D1"), &RoomType::Direct, &user_id)
        .await;
    assert_eq!(
        err,
        Err(SurfaceError::Unsupported("inviting into this room type"))
    );
    let err = client.invite(&conv("C1"), &RoomType::Group, &user_id).await;
    assert_eq!(
        err,
        Err(SurfaceError::NotFound("error-room-not-found".into()))
    );
}

#[tokio::test]
async fn room_info_reads_the_type_and_members() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let info = client.room_info(&conv("C1")).await.unwrap();
    assert_eq!(info.id.as_str(), "C1");
    assert_eq!(info.room_type, RoomType::Channel);
    assert_eq!(info.name.as_deref(), Some("general"));
    assert_eq!(info.users_count, Some(1));
    assert_eq!(
        client.room_info(&conv("nope")).await,
        Err(SurfaceError::NotFound("error-room-not-found".into()))
    );
}

#[tokio::test]
async fn a_room_is_found_by_its_name() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    fake.add_room("G1", "p", "secret");
    let client = manager(&fake);
    let info = client.room_by_name("general").await.unwrap();
    assert_eq!(info.id.as_str(), "C1");
    assert_eq!(info.room_type, RoomType::Channel);
    assert_eq!(
        client.room_by_name("nope").await,
        Err(SurfaceError::NotFound("error-room-not-found".into()))
    );
    let as_bot = bot(&client, "helper").await;
    assert!(matches!(
        as_bot.room_by_name("secret").await,
        Err(SurfaceError::Forbidden(_))
    ));
    assert_eq!(
        as_bot.room_by_name("general").await.unwrap().id.as_str(),
        "C1"
    );
}

#[tokio::test]
async fn room_info_not_allowed_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.add_room("G1", "p", "secret");
    let client = manager(&fake);
    let as_bot = bot(&client, "helper").await;
    assert_eq!(
        as_bot.room_info(&conv("G1")).await,
        Err(SurfaceError::Forbidden("not-allowed".into()))
    );
}

#[tokio::test]
async fn room_info_with_a_null_room_is_not_found() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/rooms.info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "room": null, "success": true })),
        )
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).room_info(&conv("C1")).await,
        Err(SurfaceError::NotFound("room".into()))
    );
}

#[tokio::test]
async fn user_by_username_finds_the_user_or_fails() {
    let fake = FakeRest::start().await;
    let alice = fake.add_user("alice");
    let client = manager(&fake);
    let found = client.user_by_username("alice").await.unwrap().unwrap();
    assert_eq!(found.id.as_str(), alice);
    assert_eq!(found.roles, ["user"]);
    let requests = fake.requests("users.info").await;
    assert_eq!(requests[0].url.query(), Some("username=alice"));
    assert_eq!(client.user_by_username("nobody").await, Ok(None));
}

#[tokio::test]
async fn user_by_username_reads_an_older_servers_user_codes_as_no_user() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    for code in ["error-user-not-found", "error-invalid-user"] {
        Mock::given(path("/api/v1/users.info"))
            .and(query_param("username", code))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "success": false,
                "error": format!("User not found [{code}]"),
                "errorType": code,
            })))
            .with_priority(1)
            .mount(fake.server())
            .await;
        assert_eq!(client.user_by_username(code).await, Ok(None), "{code}");
    }
}

#[tokio::test]
async fn user_by_username_fails_when_the_server_does() {
    let fake = FakeRest::start().await;
    fake.add_user("alice");
    Mock::given(path("/api/v1/users.info"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert!(matches!(
        manager(&fake).user_by_username("alice").await,
        Err(SurfaceError::Api(_))
    ));
}

#[tokio::test]
async fn download_reads_a_file_with_the_auth_headers() {
    let fake = FakeRest::start().await;
    let id = fake.add_file("persona.md", b"You are terse.");
    let client = manager(&fake);
    let data = client.download(&id, "persona.md", 1024).await.unwrap();
    assert_eq!(&data[..], b"You are terse.");
    let requests = fake.server().received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|r| r.url.path().starts_with("/file-upload/"))
        .unwrap();
    assert_eq!(request.url.path(), format!("/file-upload/{id}/persona.md"));
    assert_eq!(request.url.query(), None, "no token in the URL");
    assert_eq!(header(request, "x-user-id"), Some(FakeRest::MANAGER_ID));
    assert_eq!(
        header(request, "x-auth-token"),
        Some(FakeRest::MANAGER_TOKEN)
    );
}

#[tokio::test]
async fn download_refuses_a_file_over_the_limit() {
    let fake = FakeRest::start().await;
    let id = fake.add_file("big.md", &[b'x'; 100]);
    let client = manager(&fake);
    let err = client.download(&id, "big.md", 99).await.unwrap_err();
    assert_eq!(
        err,
        SurfaceError::TooLarge("the file is larger than the 99-byte limit".into())
    );
    assert_eq!(
        client.download(&id, "big.md", 100).await.unwrap().len(),
        100
    );
}

#[tokio::test]
async fn download_refuses_a_body_longer_than_the_limit_without_a_length() {
    let fake = FakeRest::start().await;
    Mock::given(path("/file-upload/f1/chunked.md"))
        .respond_with(ChunkedBody)
        .with_priority(1)
        .mount(fake.server())
        .await;
    let err = manager(&fake)
        .download("f1", "chunked.md", 10)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::TooLarge("the file is larger than the 10-byte limit".into())
    );
}

/// A body whose length the response doesn't declare.
struct ChunkedBody;

impl Respond for ChunkedBody {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("transfer-encoding", "chunked")
            .set_body_bytes(vec![b'y'; 64])
    }
}

#[tokio::test]
async fn download_errors_map_by_status() {
    let fake = FakeRest::start().await;
    let id = fake.add_file("persona.md", b"x");
    let client = manager(&fake);
    assert_eq!(
        client.download("missing", "persona.md", 10).await,
        Err(SurfaceError::NotFound("file".into()))
    );
    let stranger = client.with_credentials(Credentials {
        user_id: "someone".into(),
        token: SecretString::from("wrong"),
    });
    assert_eq!(
        stranger.download(&id, "persona.md", 10).await,
        Err(SurfaceError::Forbidden("file download".into()))
    );
    Mock::given(path("/file-upload/f2/broken.md"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        client.download("f2", "broken.md", 10).await,
        Err(SurfaceError::Api("file download failed (HTTP 500)".into()))
    );
}

fn redirect_to(location: &str) -> ResponseTemplate {
    ResponseTemplate::new(302).insert_header("location", location)
}

#[tokio::test]
async fn download_follows_a_same_origin_redirect_with_the_auth_headers() {
    let fake = FakeRest::start().await;
    let id = fake.add_file("persona.md", b"moved");
    Mock::given(path("/file-upload/old/persona.md"))
        .respond_with(redirect_to(&format!("/file-upload/{id}/persona.md")))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let data = manager(&fake)
        .download("old", "persona.md", 1024)
        .await
        .unwrap();
    assert_eq!(&data[..], b"moved");
    let requests = fake.server().received_requests().await.unwrap();
    let followed = requests
        .iter()
        .find(|r| r.url.path() == format!("/file-upload/{id}/persona.md"))
        .unwrap();
    assert_eq!(header(followed, "x-user-id"), Some(FakeRest::MANAGER_ID));
    assert_eq!(
        header(followed, "x-auth-token"),
        Some(FakeRest::MANAGER_TOKEN)
    );
}

#[tokio::test]
async fn download_fetches_a_presigned_url_elsewhere_without_the_auth_headers() {
    let fake = FakeRest::start().await;
    let store = MockServer::start().await;
    Mock::given(path("/bucket/persona.md"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"from the bucket".to_vec()))
        .mount(&store)
        .await;
    let presigned = format!("{}/bucket/persona.md?X-Amz-Signature=abc", store.uri());
    Mock::given(path("/file-upload/f1/persona.md"))
        .respond_with(redirect_to(&presigned))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let client = manager(&fake);
    let data = client.download("f1", "persona.md", 1024).await.unwrap();
    assert_eq!(&data[..], b"from the bucket");
    let requests = store.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.query(), Some("X-Amz-Signature=abc"));
    for name in ["x-user-id", "x-auth-token", "cookie", "authorization"] {
        assert_eq!(header(&requests[0], name), None, "{name}");
    }
    assert_eq!(
        client.download("f1", "persona.md", 3).await,
        Err(SurfaceError::TooLarge(
            "the file is larger than the 3-byte limit".into()
        ))
    );
}

#[tokio::test]
async fn download_follows_no_redirect_from_the_presigned_url() {
    let fake = FakeRest::start().await;
    let store = MockServer::start().await;
    let third = MockServer::start().await;
    Mock::given(path("/bucket/persona.md"))
        .respond_with(redirect_to(&format!("{}/again", third.uri())))
        .mount(&store)
        .await;
    Mock::given(path("/file-upload/f1/persona.md"))
        .respond_with(redirect_to(&format!("{}/bucket/persona.md", store.uri())))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).download("f1", "persona.md", 1024).await,
        Err(SurfaceError::Api("file download failed (HTTP 302)".into()))
    );
    assert!(third.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_call_redirected_to_another_origin_stops_there() {
    let fake = FakeRest::start().await;
    let elsewhere = MockServer::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(redirect_to(&format!("{}/api/v1/me", elsewhere.uri())))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let err = manager(&fake).me().await.unwrap_err();
    assert_eq!(err, SurfaceError::Api("HTTP 302".into()));
    assert!(elsewhere.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_redirect_from_http_to_https_on_the_same_host_is_not_followed() {
    let fake = FakeRest::start().await;
    let https = fake.uri().replacen("http://", "https://", 1);
    Mock::given(path("/api/v1/me"))
        .respond_with(redirect_to(&format!("{https}/api/v1/me")))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let err = manager(&fake).me().await.unwrap_err();
    assert_eq!(err, SurfaceError::Api("HTTP 302".into()));
    let requests = fake.server().received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "only the first request reached the server"
    );
}

#[tokio::test]
async fn create_dm_returns_the_room_id() {
    let fake = FakeRest::start().await;
    let member = fake.add_user("alice");
    let client = manager(&fake);
    let room = client.create_dm("alice").await.unwrap();
    assert!(fake.members(room.as_str()).contains(&member));
    let info = client.room_info(&room).await.unwrap();
    assert_eq!(info.room_type, RoomType::Direct);
    assert_eq!(info.uids.len(), 2);
    assert_eq!(
        client.create_dm("nobody").await,
        Err(SurfaceError::NotFound("error-invalid-user".into()))
    );
}

#[tokio::test]
async fn create_dm_refuses_the_self_dm_rocket_chat_returns_for_an_unknown_name() {
    let fake = FakeRest::start().await;
    fake.add_user("alice");
    let client = manager(&fake);
    assert_eq!(
        client.create_dm("Alice").await,
        Err(SurfaceError::NotFound("error-invalid-user".into()))
    );
    let own = client.create_dm(FakeRest::MANAGER_USERNAME).await.unwrap();
    assert_eq!(
        fake.members(own.as_str()),
        [FakeRest::MANAGER_ID.to_owned()]
    );
}

#[tokio::test]
async fn post_message_top_level_and_in_a_thread() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let root = client
        .post_message(&conv("C1"), "hello", None)
        .await
        .unwrap();
    assert_eq!(root.room.as_str(), "C1");
    assert_eq!(root.text, "hello");
    assert_eq!(root.sender.id.as_str(), FakeRest::MANAGER_ID);
    assert_eq!(root.thread_root, None);
    let reply = client
        .post_message(&conv("C1"), "in thread", Some(&root.id))
        .await
        .unwrap();
    assert_eq!(reply.thread_root.as_ref(), Some(&root.id));
    let requests = fake.requests("chat.postMessage").await;
    assert!(body(&requests[0]).get("tmid").is_none());
    assert_eq!(body(&requests[1])["tmid"], root.id.as_str());
    assert_eq!(body(&requests[1])["roomId"], "C1");
    assert_eq!(
        fake.message(reply.id.as_str()).unwrap().tmid.as_deref(),
        Some(root.id.as_str())
    );
}

#[tokio::test]
async fn post_message_errors_map_by_code() {
    let fake = FakeRest::start().await;
    fake.add_room("G1", "p", "secret");
    let client = manager(&fake);
    assert_eq!(
        client.post_message(&conv("nope"), "x", None).await,
        Err(SurfaceError::NotFound("invalid-channel".into()))
    );
    let as_bot = bot(&client, "helper").await;
    assert_eq!(
        as_bot.post_message(&conv("G1"), "x", None).await,
        Err(SurfaceError::Forbidden("error-not-allowed".into()))
    );
}

#[tokio::test]
async fn post_message_too_long_is_an_api_error() {
    let fake = FakeRest::start().await;
    fake.fail("chat.postMessage", 400, "error-message-size-exceeded", None)
        .await;
    assert_eq!(
        manager(&fake).post_message(&conv("C1"), "x", None).await,
        Err(SurfaceError::Api("error-message-size-exceeded".into()))
    );
}

#[tokio::test]
async fn update_message_replaces_the_text() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let posted = client
        .post_message(&conv("C1"), "draft", None)
        .await
        .unwrap();
    client
        .update_message(&conv("C1"), &posted.id, "final")
        .await
        .unwrap();
    let stored = fake.message(posted.id.as_str()).unwrap();
    assert_eq!(stored.text, "final");
    assert!(stored.edited);
    let err = client
        .update_message(&conv("C1"), &"missing".into(), "x")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::Api("No message found with the id of \"missing\".".into())
    );
}

#[tokio::test]
async fn update_message_of_someone_else_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let posted = client
        .post_message(&conv("C1"), "mine", None)
        .await
        .unwrap();
    let as_bot = bot(&client, "helper").await;
    assert_eq!(
        as_bot.update_message(&conv("C1"), &posted.id, "x").await,
        Err(SurfaceError::Forbidden("error-action-not-allowed".into()))
    );
}

#[tokio::test]
async fn react_never_toggles_an_existing_reaction_off() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let posted = client.post_message(&conv("C1"), "hi", None).await.unwrap();
    client.react(&posted.id, "eyes").await.unwrap();
    client.react(&posted.id, ":eyes:").await.unwrap();
    let stored = fake.message(posted.id.as_str()).unwrap();
    assert_eq!(
        stored.reactions,
        [(":eyes:".to_owned(), FakeRest::MANAGER_ID.to_owned())]
    );
    let request = &fake.requests("chat.react").await[0];
    assert_eq!(body(request)["shouldReact"], true);
    assert_eq!(body(request)["emoji"], "eyes");
    assert_eq!(
        client.react(&"missing".into(), "eyes").await,
        Err(SurfaceError::NotFound("error-message-not-found".into()))
    );
}

#[tokio::test]
async fn get_message_reads_one_message() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let id = fake.seed_message("C1", FakeRest::MANAGER_ID, "hello", None);
    let client = manager(&fake);
    let message = client.get_message(&id.as_str().into()).await.unwrap();
    assert_eq!(message.text, "hello");
    assert_eq!(message.sender.username, FakeRest::MANAGER_USERNAME);
    assert_eq!(
        client.get_message(&"missing".into()).await,
        Err(SurfaceError::NotFound("message".into()))
    );
}

#[tokio::test]
async fn get_message_reads_only_a_bare_json_failure_as_not_found() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    for body in ["", "<html>Bad Request</html>", r#"{"success":false,"x":1}"#] {
        let mock = Mock::given(path("/api/v1/chat.getMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_string(body))
            .with_priority(1)
            .mount_as_scoped(fake.server())
            .await;
        assert_eq!(
            client.get_message(&"m1".into()).await,
            Err(SurfaceError::Api("HTTP 400".into())),
            "{body}"
        );
        drop(mock);
    }
}

#[tokio::test]
async fn create_dm_fails_to_decode_an_answer_without_usernames() {
    let fake = FakeRest::start().await;
    fake.add_user("alice");
    Mock::given(path("/api/v1/im.create"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "success": true, "room": { "_id": "D1" } })),
        )
        .with_priority(1)
        .mount(fake.server())
        .await;
    let err = manager(&fake).create_dm("alice").await.unwrap_err();
    assert!(
        matches!(&err, SurfaceError::Transport(text) if text.contains("im.create")),
        "{err:?}"
    );
}

fn temp_file(name: &str, contents: &[u8]) -> (TempDir, OutFile) {
    let dir = TempDir::new("rc-rest");
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    let file = OutFile {
        name: name.into(),
        path,
    };
    (dir, file)
}

#[tokio::test]
async fn upload_sends_multipart_then_confirms_in_the_thread() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake);
    let root = client
        .post_message(&conv("C1"), "root", None)
        .await
        .unwrap();
    let (_dir, file) = temp_file("report.png", b"\x89PNG data");
    let message = client
        .upload(&conv("C1"), Some(&root.id), &file)
        .await
        .unwrap();
    assert_eq!(message.thread_root.as_ref(), Some(&root.id));
    assert_eq!(message.files.len(), 1);
    assert_eq!(message.files[0].name, "report.png");

    let media = &fake.requests("rooms.media").await[0];
    assert_eq!(media.url.path(), "/api/v1/rooms.media/C1");
    assert!(
        header(media, "content-type")
            .unwrap()
            .starts_with("multipart/form-data")
    );
    let raw = String::from_utf8_lossy(&media.body);
    assert!(
        raw.contains("name=\"file\"; filename=\"report.png\""),
        "{raw}"
    );
    assert!(raw.contains("Content-Type: image/png"), "{raw}");
    assert!(raw.contains("PNG data"));

    let confirm = &fake.requests("rooms.mediaConfirm").await[0];
    assert!(
        confirm
            .url
            .path()
            .starts_with("/api/v1/rooms.mediaConfirm/C1/file-")
    );
    assert_eq!(
        body(confirm),
        serde_json::json!({ "tmid": root.id.as_str() })
    );
}

#[tokio::test]
async fn upload_top_level_has_no_tmid() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let (_dir, file) = temp_file("notes.bin", b"\x00\x01");
    let message = manager(&fake)
        .upload(&conv("C1"), None, &file)
        .await
        .unwrap();
    assert_eq!(message.thread_root, None);
    let confirm = &fake.requests("rooms.mediaConfirm").await[0];
    assert_eq!(body(confirm), serde_json::json!({}));
    let raw = String::from_utf8_lossy(&fake.requests("rooms.media").await[0].body).into_owned();
    assert!(
        raw.contains("Content-Type: application/octet-stream"),
        "{raw}"
    );
}

#[tokio::test]
async fn upload_of_a_missing_file_is_not_found_and_sends_nothing() {
    let fake = FakeRest::start().await;
    let file = OutFile {
        name: "gone.txt".into(),
        path: PathBuf::from("/nonexistent/rc-rest/gone.txt"),
    };
    assert_eq!(
        manager(&fake).upload(&conv("C1"), None, &file).await,
        Err(SurfaceError::NotFound("file to upload".into()))
    );
    assert!(fake.requests("rooms.media").await.is_empty());
}

#[tokio::test]
async fn upload_errors_map_at_each_step() {
    let fake = FakeRest::start().await;
    fake.add_room("G1", "p", "secret");
    let client = manager(&fake);
    let (_dir, file) = temp_file("a.txt", b"a");
    let as_bot = bot(&client, "helper").await;
    assert_eq!(
        as_bot.upload(&conv("G1"), None, &file).await,
        Err(SurfaceError::Forbidden("forbidden".into()))
    );
    fake.fail(
        "rooms.mediaConfirm",
        400,
        "[invalid-file]",
        Some("invalid-file"),
    )
    .await;
    assert_eq!(
        client.upload(&conv("G1"), None, &file).await,
        Err(SurfaceError::NotFound("invalid-file".into()))
    );
}

#[tokio::test]
async fn upload_larger_than_the_limit_fails_and_sends_nothing() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let client = manager(&fake).with_max_upload_size(4);
    let (_dir, file) = temp_file("five.txt", b"12345");
    assert_eq!(
        client.upload(&conv("C1"), None, &file).await,
        Err(SurfaceError::TooLarge(
            "the file to upload is 5 bytes, more than the 4-byte limit".into()
        ))
    );
    assert!(fake.requests("rooms.media").await.is_empty());
    let (_dir, file) = temp_file("four.txt", b"1234");
    assert!(client.upload(&conv("C1"), None, &file).await.is_ok());
}

#[tokio::test]
async fn upload_limit_defaults_to_100_mib_and_is_checked_before_reading() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let (_dir, file) = temp_file("sparse.bin", b"");
    std::fs::File::options()
        .write(true)
        .open(&file.path)
        .unwrap()
        .set_len(100 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        manager(&fake).upload(&conv("C1"), None, &file).await,
        Err(SurfaceError::TooLarge(
            "the file to upload is 104857601 bytes, more than the 104857600-byte limit".into()
        ))
    );
    assert!(fake.requests("rooms.media").await.is_empty());
}

#[tokio::test]
async fn upload_of_a_directory_is_refused() {
    let fake = FakeRest::start().await;
    let file = OutFile {
        name: "dir".into(),
        path: std::env::temp_dir(),
    };
    assert_eq!(
        manager(&fake).upload(&conv("C1"), None, &file).await,
        Err(SurfaceError::Api(
            "the file to upload is not a regular file".into()
        ))
    );
    assert!(fake.requests("rooms.media").await.is_empty());
}

/// Answers the first request with a 429 after deleting the file being
/// uploaded, so a retry that read the file again would fail.
struct DeleteThenRateLimit(PathBuf);

impl Respond for DeleteThenRateLimit {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        std::fs::remove_file(&self.0).unwrap();
        ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "success": false,
            "error": "Error, too many requests. [error-too-many-requests]",
        }))
    }
}

#[tokio::test]
async fn a_429_on_upload_resends_the_bytes_read_the_first_time() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let (_dir, file) = temp_file("once.txt", b"read once");
    Mock::given(path("/api/v1/rooms.media/C1"))
        .respond_with(DeleteThenRateLimit(file.path.clone()))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(fake.server())
        .await;
    let client = manager(&fake).with_max_retry_wait(Duration::from_secs(2));
    let message = client.upload(&conv("C1"), None, &file).await.unwrap();
    assert_eq!(message.files[0].name, "once.txt");
    assert!(!file.path.exists());
    let media = fake.requests("rooms.media").await;
    assert_eq!(media.len(), 2);
    for request in media {
        assert!(String::from_utf8_lossy(&request.body).contains("read once"));
    }
}

#[tokio::test]
async fn room_history_uses_the_endpoint_for_the_room_type() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    for (id, t, room_type, endpoint) in [
        ("C1", "c", RoomType::Channel, "channels.history"),
        ("G1", "p", RoomType::Group, "groups.history"),
        ("D1", "d", RoomType::Direct, "im.history"),
    ] {
        fake.add_room(id, t, id);
        let root = fake.seed_message(id, FakeRest::MANAGER_ID, "one", None);
        fake.seed_message(id, FakeRest::MANAGER_ID, "reply", Some(&root));
        fake.seed_message(id, FakeRest::MANAGER_ID, "two", None);
        let history = client
            .room_history(&conv(id), &room_type, None, 10)
            .await
            .unwrap();
        let texts: Vec<&str> = history.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["two", "one"], "{endpoint}");
        let request = &fake.requests(endpoint).await[0];
        let query: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
        assert!(query.contains(&("roomId".into(), id.into())));
        assert!(query.contains(&("count".into(), "10".into())));
        assert!(query.contains(&("showThreadMessages".into(), "false".into())));
        assert!(query.contains(&("inclusive".into(), "false".into())));
    }
    assert_eq!(
        client
            .room_history(&conv("L1"), &RoomType::Livechat, None, 10)
            .await,
        Err(SurfaceError::Unsupported("history of this room type"))
    );
}

#[tokio::test]
async fn room_history_before_a_time_is_exclusive() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    for text in ["a", "b", "c", "d"] {
        fake.seed_message("C1", FakeRest::MANAGER_ID, text, None);
    }
    let client = manager(&fake);
    let newest = client
        .room_history(&conv("C1"), &RoomType::Channel, None, 1)
        .await
        .unwrap();
    assert_eq!(newest[0].text, "d");
    let older = client
        .room_history(&conv("C1"), &RoomType::Channel, Some(newest[0].sent_at), 2)
        .await
        .unwrap();
    let texts: Vec<&str> = older.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["c", "b"]);
    let request = &fake.requests("channels.history").await[1];
    let latest = request
        .url
        .query_pairs()
        .find(|(k, _)| k == "latest")
        .map(|(_, v)| v.into_owned());
    assert_eq!(latest.as_deref(), Some("2026-09-30T00:00:04.000Z"));
}

#[tokio::test]
async fn room_history_of_a_room_the_bot_cannot_read_is_forbidden() {
    let fake = FakeRest::start().await;
    fake.add_room("G1", "p", "secret");
    let client = manager(&fake);
    let as_bot = bot(&client, "helper").await;
    assert_eq!(
        as_bot
            .room_history(&conv("G1"), &RoomType::Group, None, 5)
            .await,
        Err(SurfaceError::Forbidden("unauthorized".into()))
    );
}

#[tokio::test]
async fn thread_messages_page_newest_first() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    let root = fake.seed_message("C1", FakeRest::MANAGER_ID, "root", None);
    for text in ["r1", "r2", "r3"] {
        fake.seed_message("C1", FakeRest::MANAGER_ID, text, Some(&root));
    }
    let client = manager(&fake);
    let root_id: MessageId = root.as_str().into();
    let page = client.thread_messages(&root_id, 0, 2).await.unwrap();
    let texts: Vec<&str> = page.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["r3", "r2"]);
    let page = client.thread_messages(&root_id, 2, 2).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].text, "r1");
    assert!(
        page.iter()
            .all(|m| m.thread_root.as_ref() == Some(&root_id))
    );
    let request = &fake.requests("chat.getThreadMessages").await[0];
    let query: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
    assert!(query.contains(&("sort".into(), r#"{"ts":-1}"#.into())));
    assert!(query.contains(&("tmid".into(), root.clone())));
    assert_eq!(
        client.thread_messages(&"missing".into(), 0, 2).await,
        Err(SurfaceError::NotFound("error-invalid-message".into()))
    );
}

#[tokio::test]
async fn a_429_is_retried_once_after_the_reset() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    fake.rate_limit("chat.postMessage", 1, Duration::from_millis(300))
        .await;
    let started = Instant::now();
    let posted = manager(&fake)
        .post_message(&conv("C1"), "hello", None)
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(posted.text, "hello");
    assert_eq!(fake.requests("chat.postMessage").await.len(), 2);
}

#[tokio::test]
async fn a_second_429_is_not_retried() {
    let fake = FakeRest::start().await;
    fake.rate_limit("me", 5, Duration::from_millis(20)).await;
    let err = manager(&fake).me().await.unwrap_err();
    let SurfaceError::RateLimited { retry_after } = err else {
        panic!("expected rate limited, got {err:?}");
    };
    assert!(retry_after <= Duration::from_millis(20));
    assert_eq!(fake.requests("me").await.len(), 2);
}

#[tokio::test]
async fn a_429_resetting_later_than_the_max_wait_fails_at_once() {
    let fake = FakeRest::start().await;
    fake.rate_limit("me", 5, Duration::from_secs(600)).await;
    let client = manager(&fake).with_max_retry_wait(Duration::from_secs(1));
    let started = Instant::now();
    let err = client.me().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    let SurfaceError::RateLimited { retry_after } = err else {
        panic!("expected rate limited, got {err:?}");
    };
    assert!(retry_after > Duration::from_secs(590), "{retry_after:?}");
    assert_eq!(fake.requests("me").await.len(), 1);
}

#[tokio::test]
async fn a_429_from_a_server_clock_an_hour_ahead_is_retried_after_the_reset() {
    let fake = FakeRest::start().await;
    let server_now = SystemTime::now() + Duration::from_secs(3600);
    fake.rate_limit_at("me", 1, Duration::from_millis(300), server_now)
        .await;
    let started = Instant::now();
    manager(&fake).me().await.unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    assert_eq!(fake.requests("me").await.len(), 2);
}

#[tokio::test]
async fn a_429_from_a_server_clock_an_hour_behind_waits_for_the_reset() {
    let fake = FakeRest::start().await;
    let server_now = SystemTime::now() - Duration::from_secs(3600);
    fake.rate_limit_at("me", 1, Duration::from_millis(1200), server_now)
        .await;
    let started = Instant::now();
    manager(&fake).me().await.unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1200), "{elapsed:?}");
    assert_eq!(fake.requests("me").await.len(), 2);
}

#[tokio::test]
async fn a_skewed_server_reports_the_reset_by_its_own_clock() {
    let fake = FakeRest::start().await;
    let behind = SystemTime::now() - Duration::from_secs(3600);
    fake.rate_limit_at("me", 5, Duration::from_millis(20), behind)
        .await;
    assert_eq!(
        manager(&fake).me().await,
        Err(SurfaceError::RateLimited {
            retry_after: Duration::from_millis(20)
        })
    );
    assert_eq!(fake.requests("me").await.len(), 2);

    let fake = FakeRest::start().await;
    let ahead = SystemTime::now() + Duration::from_secs(3600);
    fake.rate_limit_at("me", 5, Duration::from_secs(600), ahead)
        .await;
    let client = manager(&fake).with_max_retry_wait(Duration::from_secs(1));
    assert_eq!(
        client.me().await,
        Err(SurfaceError::RateLimited {
            retry_after: Duration::from_secs(600)
        })
    );
    assert_eq!(fake.requests("me").await.len(), 1);
}

#[tokio::test]
async fn a_429_on_upload_resends_the_file() {
    let fake = FakeRest::start().await;
    fake.add_room("C1", "c", "general");
    fake.rate_limit("rooms.media", 1, Duration::from_millis(10))
        .await;
    let (_dir, file) = temp_file("retry.txt", b"payload");
    manager(&fake)
        .upload(&conv("C1"), None, &file)
        .await
        .unwrap();
    let media = fake.requests("rooms.media").await;
    assert_eq!(media.len(), 2);
    assert!(String::from_utf8_lossy(&media[1].body).contains("payload"));
}

#[tokio::test]
async fn a_rate_limit_error_code_without_429_is_also_retried() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "success": false,
            "error": "Error, too many requests. [error-too-many-requests]",
            "errorType": "error-too-many-requests",
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(fake.server())
        .await;
    let client = manager(&fake).with_max_retry_wait(Duration::from_secs(2));
    assert!(client.me().await.is_ok());
    assert_eq!(fake.requests("me").await.len(), 2);
}

#[tokio::test]
async fn a_rate_limited_login_is_rate_limited_not_unauthorized() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/login"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "success": false,
            "status": "error",
            "error": "too-many-requests",
            "message": "Error, too many requests. Please slow down.",
        })))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let client = manager(&fake).with_max_retry_wait(Duration::ZERO);
    let (_, password) = client.create_bot_user(&bot_user("helper")).await.unwrap();
    let err = client
        .issue_bot_token("helper", password, "agentd")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SurfaceError::RateLimited {
            retry_after: Duration::from_secs(1)
        }
    );
    assert_eq!(fake.requests("login").await.len(), 1);
}

#[tokio::test]
async fn an_unknown_endpoint_is_not_found() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).me().await,
        Err(SurfaceError::NotFound("404".into()))
    );
}

#[tokio::test]
async fn an_html_error_page_is_an_api_error_with_the_status() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).me().await,
        Err(SurfaceError::Api("HTTP 502".into()))
    );
}

#[tokio::test]
async fn an_unreadable_success_is_a_transport_error() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).me().await,
        Err(SurfaceError::Transport(
            "unreadable response (HTTP 200)".into()
        ))
    );
}

#[tokio::test]
async fn a_success_with_the_wrong_shape_is_a_transport_error_without_content() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_id": 7,
            "username": "secret-looking-value",
            "success": true,
        })))
        .with_priority(1)
        .mount(fake.server())
        .await;
    let err = manager(&fake).me().await.unwrap_err();
    let SurfaceError::Transport(text) = err else {
        panic!("expected transport, got {err:?}");
    };
    assert!(text.starts_with("unexpected response from me"), "{text}");
    assert!(!text.contains("secret-looking-value"));
}

#[tokio::test]
async fn success_false_on_http_200_is_an_error() {
    let fake = FakeRest::start().await;
    Mock::given(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false,
            "error": "Not Allowed [error-not-allowed]",
        })))
        .with_priority(1)
        .mount(fake.server())
        .await;
    assert_eq!(
        manager(&fake).me().await,
        Err(SurfaceError::Forbidden("error-not-allowed".into()))
    );
}

#[tokio::test]
async fn an_unreachable_server_is_a_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let client = RestClient::new(
        &format!("http://127.0.0.1:{port}"),
        Credentials {
            user_id: "u".into(),
            token: SecretString::from("t"),
        },
    )
    .unwrap();
    let err = client.me().await.unwrap_err();
    assert!(matches!(err, SurfaceError::Transport(_)), "{err:?}");
}

#[tokio::test]
async fn a_base_url_with_a_path_prefix_is_kept() {
    let fake = FakeRest::start().await;
    Mock::given(path("/chat/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_id": "u1",
            "username": "prefixed",
            "success": true,
        })))
        .mount(fake.server())
        .await;
    let creds = Credentials {
        user_id: "u1".into(),
        token: SecretString::from("t"),
    };
    for base in [
        format!("{}/chat", fake.uri()),
        format!("{}/chat/", fake.uri()),
    ] {
        let client = RestClient::new(&base, creds.clone()).unwrap();
        assert_eq!(client.me().await.unwrap().username, "prefixed");
    }
}

#[test]
fn bad_base_urls_are_refused() {
    let creds = Credentials {
        user_id: "u".into(),
        token: SecretString::from("t"),
    };
    for base in [
        "not a url",
        "ftp://chat.example.com",
        "https://user:pass@chat.example.com",
        "https://chat.example.com/?x=1",
        "https://chat.example.com/#frag",
    ] {
        let err = RestClient::new(base, creds.clone()).unwrap_err();
        assert!(matches!(err, SurfaceError::Api(_)), "{base}: {err:?}");
    }
}

#[test]
fn debug_output_never_shows_secrets() {
    let creds = Credentials {
        user_id: "u1".into(),
        token: SecretString::from("super-secret-token"),
    };
    let client = RestClient::new("https://chat.example.com", creds.clone()).unwrap();
    assert!(!format!("{creds:?}").contains("super-secret-token"));
    assert!(!format!("{client:?}").contains("super-secret-token"));
}

#[test]
fn message_debug_shows_the_text_length_not_the_text() {
    let message: Message = serde_json::from_value(serde_json::json!({
        "_id": "m1",
        "rid": "r1",
        "msg": "MESSAGE-TEXT",
        "ts": "2026-10-07T08:00:00.000Z",
        "u": { "_id": "u1", "username": "alice" },
    }))
    .unwrap();
    let debug = format!("{message:?} {message:#?}");
    assert!(!debug.contains("MESSAGE-TEXT"), "{debug}");
    assert!(debug.contains("text_len: 12"), "{debug}");
    assert!(debug.contains("m1"), "{debug}");
}

#[tokio::test]
async fn bot_password_debug_is_redacted() {
    let fake = FakeRest::start().await;
    let (_, password) = manager(&fake)
        .create_bot_user(&bot_user("helper"))
        .await
        .unwrap();
    let sent = body(&fake.requests("users.create").await[0])["password"]
        .as_str()
        .unwrap()
        .to_owned();
    let printed = format!("{password:?}");
    assert_eq!(printed, "BotPassword([REDACTED])");
    assert!(!printed.contains(&sent));
}

#[tokio::test]
async fn subscriptions_list_the_rooms_the_user_is_in() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let helper = bot(&client, "helper").await;
    fake.add_room("GENERAL", "c", "general");
    fake.add_room("SECRET", "p", "secret");
    fake.add_room("OTHER", "c", "other");
    fake.add_member("GENERAL", helper.user_id().as_str());
    fake.add_member("SECRET", helper.user_id().as_str());
    let mut rooms = helper.subscriptions().await.unwrap();
    rooms.sort_by(|a, b| a.room.cmp(&b.room));
    let listed: Vec<(&str, RoomType)> = rooms
        .iter()
        .map(|s| (s.room.as_str(), s.room_type.clone()))
        .collect();
    assert_eq!(
        listed,
        [("GENERAL", RoomType::Channel), ("SECRET", RoomType::Group)]
    );
    assert_eq!(rooms[0].name.as_deref(), Some("general"));
    assert_eq!(rooms[0].id, format!("GENERAL{}", helper.user_id()));
    let requests = fake.requests("subscriptions.get").await;
    assert_eq!(
        header(&requests[0], "x-user-id"),
        Some(helper.user_id().as_str())
    );
}

#[tokio::test]
async fn one_subscription_is_the_users_own_or_none() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let helper = bot(&client, "helper").await;
    fake.add_room("GENERAL", "c", "general");
    fake.add_room("OTHER", "c", "other");
    fake.add_member("GENERAL", helper.user_id().as_str());
    let found = helper
        .subscription(&"GENERAL".into())
        .await
        .unwrap()
        .expect("the helper is in GENERAL");
    assert_eq!(found.room.as_str(), "GENERAL");
    assert_eq!(found.room_type, RoomType::Channel);
    assert_eq!(found.id, format!("GENERAL{}", helper.user_id()));
    assert_eq!(helper.subscription(&"OTHER".into()).await.unwrap(), None);
    assert_eq!(helper.subscription(&"MISSING".into()).await.unwrap(), None);
    let requests = fake.requests("subscriptions.getOne").await;
    assert_eq!(requests[0].url.query(), Some("roomId=GENERAL"));
    assert_eq!(
        header(&requests[0], "x-user-id"),
        Some(helper.user_id().as_str())
    );
}

#[tokio::test]
async fn user_info_shows_roles_to_the_manager_and_to_the_user_itself() {
    let fake = FakeRest::start().await;
    let client = manager(&fake);
    let helper = bot(&client, "helper").await;
    let other = bot(&client, "other").await;
    let seen_by_manager = client.user_info(other.user_id()).await.unwrap();
    assert_eq!(seen_by_manager.username, "other");
    assert_eq!(seen_by_manager.roles, ["bot"]);
    let seen_by_itself = other.user_info(other.user_id()).await.unwrap();
    assert_eq!(seen_by_itself.roles, ["bot"]);
    let seen_by_a_peer = helper.user_info(other.user_id()).await.unwrap();
    assert!(seen_by_a_peer.roles.is_empty());
    let missing = client.user_info(&UserId::from("nobody")).await.unwrap_err();
    assert_eq!(missing, SurfaceError::Api("User not found.".into()));
    let requests = fake.requests("users.info").await;
    assert_eq!(
        requests[0].url.query(),
        Some(format!("userId={}", other.user_id()).as_str())
    );
}

#[tokio::test]
async fn file_urls_sit_under_the_base_path() {
    let creds = Credentials {
        user_id: "u1".into(),
        token: SecretString::from("t"),
    };
    let client = RestClient::new("https://chat.example.com/rc/", creds).unwrap();
    let file = surface_rocketchat::rest::FileRef {
        id: "f1".into(),
        name: "a b/c.png".into(),
        mime_type: None,
        size: None,
    };
    assert_eq!(
        client.file_url(&file),
        "https://chat.example.com/rc/file-upload/f1/a%20b%2Fc.png"
    );
    assert_eq!(client.credentials().user_id.as_str(), "u1");
}
