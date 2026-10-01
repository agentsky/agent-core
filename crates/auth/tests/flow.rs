//! The login, refresh and logout flows against wiremock peers.

use std::sync::Arc;
use std::time::Duration;

use auth::{Auth, AuthError, Endpoint, LinkStatus, OAuthConfig, Plan, PlanInfo, TokenSource};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use core_types::{MemberId, MemberKey, SurfaceKind, TeamId, UserId};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use store::{NewClaudeLink, Sealer, Store};
use testkit::{Held, Hold};
use time::OffsetDateTime;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const TOKEN_PATH: &str = "/v1/oauth/token";
const REVOKE_PATH: &str = "/v1/oauth/token/revoke";
const PROFILE_PATH: &str = "/api/oauth/profile";

struct Harness {
    server: MockServer,
    auth: Arc<Auth>,
    store: Store,
    member: MemberId,
}

fn member_key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new("chat.example.org"),
        user: UserId::new(user),
    }
}

async fn harness() -> Harness {
    let server = MockServer::start().await;
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let member = store
        .ensure_member(&member_key("ada"), "Ada", now())
        .await
        .unwrap();
    let config = OAuthConfig {
        token_url: format!("{}{TOKEN_PATH}", server.uri()),
        revoke_url: format!("{}{REVOKE_PATH}", server.uri()),
        profile_url: format!("{}{PROFILE_PATH}", server.uri()),
        ..OAuthConfig::default()
    };
    let auth = Arc::new(Auth::new(config, store.clone()).unwrap());
    Harness {
        server,
        auth,
        store,
        member,
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn token_response(access: &str, refresh: Option<&str>) -> ResponseTemplate {
    let mut body = json!({
        "token_type": "Bearer",
        "access_token": access,
        "expires_in": 28800,
        "scope": "user:profile user:inference",
        "account": {"uuid": "acct", "email_address": "ada@example.org"},
        "organization": {"uuid": "org"},
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = json!(refresh);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn profile_response(organization_type: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "account": {"uuid": "acct", "email_address": "ada@example.org"},
        "organization": {
            "uuid": "org",
            "organization_type": organization_type,
            "rate_limit_tier": format!("default_{organization_type}"),
        },
    }))
}

async fn mount_profile(server: &MockServer, bearer: &str, organization_type: &str) {
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(profile_response(organization_type))
        .mount(server)
        .await;
}

async fn link(
    store: &Store,
    member: MemberId,
    access: &str,
    refresh: &str,
    expires_in: i64,
) -> i64 {
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from(access),
                refresh_token: SecretString::from(refresh),
                expires_at: now() + time::Duration::seconds(expires_in),
                plan: Some("claude_max".to_owned()),
                rate_limit_tier: Some("default_claude_max".to_owned()),
            },
            now(),
        )
        .await
        .unwrap()
}

/// Polls `check` until it holds, for work a refresh task finishes after its
/// callers got their result.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn stored_plan(store: &Store, member: MemberId) -> Option<String> {
    store.get_claude_link(member).await.unwrap().unwrap().plan
}

async fn wait_for_plan(store: &Store, member: MemberId, plan: &str) {
    eventually("the plan is stored", || async {
        stored_plan(store, member).await.as_deref() == Some(plan)
    })
    .await;
}

fn query(url: &Url, key: &str) -> String {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| panic!("no {key} in the URL"))
}

fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn requests_to(server: &MockServer, request_path: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == request_path)
        .collect()
}

fn paste(code: &str, state: &str) -> SecretString {
    SecretString::from(format!("{code}#{state}"))
}

#[tokio::test]
async fn start_login_builds_the_authorize_url_and_stores_a_pending_login() {
    let h = harness().await;
    let before = now();
    let start = h.auth.start_login(h.member).await.unwrap();
    let url = Url::parse(&start.url).unwrap();
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("claude.com"));
    assert_eq!(url.path(), "/cai/oauth/authorize");
    let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    assert_eq!(
        keys,
        [
            "code",
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "state",
        ]
    );
    assert_eq!(query(&url, "code"), "true");
    assert_eq!(query(&url, "client_id"), CLIENT_ID);
    assert_eq!(query(&url, "response_type"), "code");
    assert_eq!(query(&url, "redirect_uri"), REDIRECT);
    assert_eq!(query(&url, "scope"), "user:profile user:inference");
    assert_eq!(query(&url, "code_challenge_method"), "S256");

    let ttl = start.expires_at - before;
    assert!(ttl > time::Duration::seconds(598) && ttl <= time::Duration::seconds(601));

    let pending = h
        .store
        .take_pending_login(&query(&url, "state"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.member, h.member);
    assert_eq!(pending.expires_at, start.expires_at);
    assert_eq!(
        challenge_of(pending.verifier.expose_secret()),
        query(&url, "code_challenge")
    );
}

#[tokio::test]
async fn state_is_never_the_verifier_or_derived_from_it() {
    let h = harness().await;
    let mut states = std::collections::HashSet::new();
    for _ in 0..32 {
        let start = h.auth.start_login(h.member).await.unwrap();
        let url = Url::parse(&start.url).unwrap();
        let state = query(&url, "state");
        let pending = h.store.take_pending_login(&state).await.unwrap().unwrap();
        let verifier = pending.verifier.expose_secret();
        let digest = Sha256::digest(verifier.as_bytes());
        assert_eq!(verifier.len(), 43);
        assert_eq!(state.len(), 43);
        assert_ne!(state, verifier);
        assert_ne!(state, challenge_of(verifier));
        assert_ne!(state, URL_SAFE_NO_PAD.encode(verifier.as_bytes()));
        assert_ne!(state.as_bytes(), &digest[..]);
        assert!(!verifier.contains(&state) && !state.contains(verifier));
        let shared_prefix = state
            .bytes()
            .zip(verifier.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(shared_prefix < 8, "state and verifier share a prefix");
        assert!(states.insert(state));
    }
}

#[tokio::test]
async fn the_verifier_appears_in_no_url() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-1", "claude_max").await;

    let start = h.auth.start_login(h.member).await.unwrap();
    let url = Url::parse(&start.url).unwrap();
    let state = query(&url, "state");
    h.auth
        .complete_login(h.member, &paste("the-code", &state))
        .await
        .unwrap();

    let token_requests = requests_to(&h.server, TOKEN_PATH).await;
    let body: Value = token_requests[0].body_json().unwrap();
    let verifier = body["code_verifier"].as_str().unwrap().to_owned();
    assert_eq!(challenge_of(&verifier), query(&url, "code_challenge"));

    let mut urls = vec![start.url.clone(), url.query().unwrap_or("").to_owned()];
    urls.extend(url.query_pairs().map(|(_, v)| v.into_owned()));
    for request in h.server.received_requests().await.unwrap() {
        urls.push(request.url.to_string());
        for (name, value) in &request.headers {
            assert!(
                !value.to_str().unwrap_or("").contains(&verifier),
                "verifier in the {name} header"
            );
        }
    }
    for url in urls {
        assert!(!url.contains(&verifier), "verifier in {url}");
    }
}

#[tokio::test]
async fn complete_login_exchanges_the_code_and_stores_the_link() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(header("content-type", "application/json"))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-1", "claude_max").await;

    let start = h.auth.start_login(h.member).await.unwrap();
    let url = Url::parse(&start.url).unwrap();
    let state = query(&url, "state");
    let before = now();
    let linked = h
        .auth
        .complete_login(h.member, &paste("the-code", &state))
        .await
        .unwrap();
    assert_eq!(
        linked.plan,
        Some(PlanInfo {
            plan: Some(Plan::Max),
            rate_limit_tier: Some("default_claude_max".to_owned()),
        })
    );

    let body: Value = requests_to(&h.server, TOKEN_PATH).await[0]
        .body_json()
        .unwrap();
    let verifier = body["code_verifier"].as_str().unwrap();
    assert_eq!(challenge_of(verifier), query(&url, "code_challenge"));
    assert_eq!(
        body,
        json!({
            "grant_type": "authorization_code",
            "code": "the-code",
            "redirect_uri": REDIRECT,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        })
    );

    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.access_token.expose_secret(), "access-1");
    assert_eq!(stored.refresh_token.expose_secret(), "refresh-1");
    assert_eq!(stored.plan.as_deref(), Some("claude_max"));
    assert_eq!(
        stored.rate_limit_tier.as_deref(),
        Some("default_claude_max")
    );
    let lifetime = stored.expires_at - before;
    assert!(
        lifetime > time::Duration::seconds(28_790) && lifetime <= time::Duration::seconds(28_801)
    );
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
}

#[tokio::test]
async fn complete_login_accepts_a_callback_url_and_stray_whitespace() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .expect(2)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-1", "claude_pro").await;

    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    let pasted = SecretString::from(format!("  <{REDIRECT}?code=code-from-url&state={state}>\n"));
    h.auth.complete_login(h.member, &pasted).await.unwrap();

    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    let (head, tail) = state.split_at(20);
    let pasted = SecretString::from(format!("\t`wrapped-\ncode#{head}\n{tail}` "));
    let linked = h.auth.complete_login(h.member, &pasted).await.unwrap();
    assert_eq!(linked.plan.unwrap().plan, Some(Plan::Pro));

    let codes: Vec<String> = requests_to(&h.server, TOKEN_PATH)
        .await
        .iter()
        .map(|request| {
            request.body_json::<Value>().unwrap()["code"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(codes, ["code-from-url", "wrapped-code"]);
}

#[tokio::test]
async fn an_expired_pending_login_is_refused_without_an_exchange() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .expect(0)
        .mount(&h.server)
        .await;
    h.store
        .put_pending_login(
            "expired-state",
            h.member,
            &SecretString::from("verifier"),
            now() - time::Duration::seconds(1),
        )
        .await
        .unwrap();
    let err = h
        .auth
        .complete_login(h.member, &paste("code", "expired-state"))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::LoginExpired), "{err:?}");
    assert!(
        h.store
            .take_pending_login("expired-state")
            .await
            .unwrap()
            .is_none()
    );
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

#[tokio::test]
async fn another_members_code_is_refused_and_invalidated() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .expect(0)
        .mount(&h.server)
        .await;
    let bob = h
        .store
        .ensure_member(&member_key("bob"), "Bob", now())
        .await
        .unwrap();
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");

    let err = h
        .auth
        .complete_login(bob, &paste("code", &state))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::UnknownLogin), "{err:?}");
    let err = h
        .auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::UnknownLogin), "{err:?}");
    assert!(h.store.get_claude_link(bob).await.unwrap().is_none());
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

#[tokio::test]
async fn a_pasted_code_cancels_its_login_without_an_exchange() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .expect(0)
        .mount(&h.server)
        .await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");

    assert!(
        !h.auth
            .cancel_pasted_login(&SecretString::from("just-a-code"))
            .await
            .unwrap()
    );
    assert!(
        !h.auth
            .cancel_pasted_login(&paste("code", "no-such-state"))
            .await
            .unwrap()
    );
    assert!(
        h.auth
            .cancel_pasted_login(&paste("code", &state))
            .await
            .unwrap()
    );
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
    let err = h
        .auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::UnknownLogin), "{err:?}");
}

#[tokio::test]
async fn unknown_states_and_malformed_pastes_are_refused() {
    let h = harness().await;
    let err = h
        .auth
        .complete_login(h.member, &paste("code", "no-such-state"))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::UnknownLogin), "{err:?}");
    let err = h
        .auth
        .complete_login(h.member, &SecretString::from("just-a-code"))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::MalformedCode), "{err:?}");
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_new_login_replaces_the_pending_one() {
    let h = harness().await;
    let first = h.auth.start_login(h.member).await.unwrap();
    let second = h.auth.start_login(h.member).await.unwrap();
    let first_state = query(&Url::parse(&first.url).unwrap(), "state");
    let second_state = query(&Url::parse(&second.url).unwrap(), "state");
    assert!(
        h.store
            .take_pending_login(&first_state)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.store
            .take_pending_login(&second_state)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_rejected_code_is_reported_without_the_response_body() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "code the-code is invalid",
        })))
        .mount(&h.server)
        .await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    let err = h
        .auth
        .complete_login(h.member, &paste("the-code", &state))
        .await
        .unwrap_err();
    match &err {
        AuthError::CodeRejected { status, error } => {
            assert_eq!(*status, 400);
            assert_eq!(error.as_deref(), Some("invalid_grant"));
        }
        other => panic!("expected CodeRejected, got {other:?}"),
    }
    let text = format!("{err} {err:?}");
    assert_eq!(
        err.to_string(),
        "the token endpoint rejected the code (HTTP 400, invalid_grant)"
    );
    assert!(!text.contains("the-code"));
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

#[tokio::test]
async fn a_token_response_without_a_refresh_token_is_refused() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", None))
        .mount(&h.server)
        .await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    let err = h
        .auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::InvalidResponse {
                endpoint: Endpoint::Token,
                reason: "no refresh_token"
            }
        ),
        "{err:?}"
    );
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

fn widened(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "token_type": "Bearer",
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 28800,
        "scope": "user:profile user:inference user:sessions:claude_code",
    }))
}

async fn mount_revoke(server: &MockServer, refresh: &str) {
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(body_json(json!({
            "token": refresh,
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;
}

async fn login_answered(h: &Harness, answer: ResponseTemplate) -> AuthError {
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(answer)
        .expect(1)
        .mount(&h.server)
        .await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    h.auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn a_login_granted_a_wider_scope_is_refused_and_stores_nothing() {
    let h = harness().await;
    mount_revoke(&h.server, "refresh-wide").await;
    let err = login_answered(&h, widened("access-wide", "refresh-wide")).await;
    assert!(matches!(err, AuthError::ScopeRefused), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("-wide"));
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    assert!(requests_to(&h.server, PROFILE_PATH).await.is_empty());
    h.server.verify().await;
}

#[tokio::test]
async fn a_login_whose_grant_is_unstated_is_refused_and_stores_nothing() {
    let h = harness().await;
    mount_revoke(&h.server, "refresh-unstated").await;
    let answer = ResponseTemplate::new(200).set_body_json(json!({
        "token_type": "Bearer",
        "access_token": "access-unstated",
        "refresh_token": "refresh-unstated",
        "expires_in": 28800,
    }));
    let err = login_answered(&h, answer).await;
    assert!(matches!(err, AuthError::ScopeUnstated), "{err:?}");
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    h.server.verify().await;
}

#[tokio::test]
async fn a_refresh_whose_grant_is_unstated_or_unreadable_keeps_the_link() {
    for scope in [
        None,
        Some(json!("")),
        Some(json!([])),
        Some(json!(7)),
        Some(json!({"user:profile": true})),
        Some(json!(["user:inference", null])),
    ] {
        let h = harness().await;
        let mut body = json!({"access_token": "access-2", "expires_in": 28800});
        if let Some(scope) = &scope {
            body["scope"] = scope.clone();
        }
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&h.server)
            .await;
        Mock::given(method("POST"))
            .and(path(REVOKE_PATH))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        link(&h.store, h.member, "access-1", "refresh-1", 60).await;
        let token = h.auth.access_token(h.member).await.unwrap();
        assert_eq!(token.expose_secret(), "access-2", "{scope:?}");
        let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
        assert!(stored.broken_at.is_none(), "{scope:?}");
        h.server.verify().await;
    }
}

#[tokio::test]
async fn a_wider_scope_in_any_shape_refuses_a_login_and_breaks_a_refresh() {
    for scope in [
        json!([
            "user:profile",
            "user:inference",
            "user:sessions:claude_code"
        ]),
        json!(["user:profile", "user:sessions:claude_code", 7]),
        json!([["user:sessions:claude_code"]]),
        json!({"granted": ["user:profile", "user:sessions:claude_code"]}),
        json!({"user:sessions:claude_code": true}),
    ] {
        let wide = || {
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-wide",
                "refresh_token": "refresh-wide",
                "expires_in": 28800,
                "scope": scope,
            }))
        };
        let h = harness().await;
        mount_revoke(&h.server, "refresh-wide").await;
        let err = login_answered(&h, wide()).await;
        assert!(matches!(err, AuthError::ScopeRefused), "{scope}: {err:?}");
        assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
        h.server.verify().await;

        let h = harness().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(wide())
            .expect(1)
            .mount(&h.server)
            .await;
        mount_revoke(&h.server, "refresh-wide").await;
        link(&h.store, h.member, "access-1", "refresh-1", 60).await;
        let err = h.auth.access_token(h.member).await.unwrap_err();
        assert!(matches!(err, AuthError::RelinkRequired), "{scope}: {err:?}");
        let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
        assert!(stored.broken_at.is_some(), "{scope}");
        eventually("the wide grant is revoked", || async {
            !requests_to(&h.server, REVOKE_PATH).await.is_empty()
        })
        .await;
        h.server.verify().await;
    }
}

#[tokio::test]
async fn an_array_naming_only_allowed_scopes_links() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 28800,
            "scope": ["user:profile", "user:inference"],
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-1", "claude_max").await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    h.auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap();
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_some());
}

#[tokio::test]
async fn a_login_whose_scope_is_unreadable_is_refused_as_unstated() {
    let h = harness().await;
    mount_revoke(&h.server, "refresh-odd").await;
    let answer = ResponseTemplate::new(200).set_body_json(json!({
        "access_token": "access-odd",
        "refresh_token": "refresh-odd",
        "expires_in": 28800,
        "scope": {"user:profile": "user:inference"},
    }));
    let err = login_answered(&h, answer).await;
    assert!(matches!(err, AuthError::ScopeUnstated), "{err:?}");
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    h.server.verify().await;
}

#[tokio::test]
async fn a_wider_refresh_without_a_new_refresh_token_revokes_the_old_one() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-wide",
            "expires_in": 28800,
            "scope": "user:profile user:inference user:sessions:claude_code",
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_revoke(&h.server, "refresh-1").await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    eventually("the old refresh token is revoked", || async {
        !requests_to(&h.server, REVOKE_PATH).await.is_empty()
    })
    .await;
    h.server.verify().await;
}

#[tokio::test]
async fn a_refresh_granted_a_wider_scope_breaks_the_link() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(widened("access-wide", "refresh-wide"))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_revoke(&h.server, "refresh-wide").await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let mut notices = h.auth.take_relink_notices().unwrap();

    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert!(stored.broken_at.is_some());
    assert_eq!(stored.access_token.expose_secret(), "access-1");
    assert_eq!(stored.refresh_token.expose_secret(), "refresh-1");
    assert_eq!(notices.try_recv().unwrap(), h.member);
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    eventually("the wide grant is revoked", || async {
        !requests_to(&h.server, REVOKE_PATH).await.is_empty()
    })
    .await;
    h.server.verify().await;
}

#[tokio::test]
async fn the_link_is_stored_even_if_the_profile_fails() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-1", Some("refresh-1")))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    let start = h.auth.start_login(h.member).await.unwrap();
    let state = query(&Url::parse(&start.url).unwrap(), "state");
    let linked = h
        .auth
        .complete_login(h.member, &paste("code", &state))
        .await
        .unwrap();
    assert_eq!(linked.plan, None);
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.access_token.expose_secret(), "access-1");
    assert_eq!(stored.plan, None);
}

#[tokio::test]
async fn fetch_plan_sends_the_bearer_token_and_maps_the_organization_type() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .and(header("authorization", "Bearer access-1"))
        .and(header("cache-control", "no-cache"))
        .respond_with(profile_response("claude_enterprise"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .and(header("authorization", "Bearer access-2"))
        .respond_with(profile_response("claude_galaxy"))
        .mount(&h.server)
        .await;
    let plan = h
        .auth
        .fetch_plan(&SecretString::from("access-1"))
        .await
        .unwrap();
    assert_eq!(plan.plan, Some(Plan::Enterprise));
    assert_eq!(
        plan.rate_limit_tier.as_deref(),
        Some("default_claude_enterprise")
    );
    let plan = h
        .auth
        .fetch_plan(&SecretString::from("access-2"))
        .await
        .unwrap();
    assert_eq!(plan.plan, Some(Plan::Unknown("claude_galaxy".to_owned())));
    let err = h
        .auth
        .fetch_plan(&SecretString::from("access-3"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Status {
                endpoint: Endpoint::Profile,
                status: 404,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains("access-3"));
}

#[tokio::test]
async fn a_fresh_token_is_returned_without_a_refresh() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("new", Some("new")))
        .expect(0)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 3600).await;
    let source: Arc<dyn TokenSource> = h.auth.clone();
    let token = source.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-1");
}

#[tokio::test]
async fn an_unlinked_member_is_reported() {
    let h = harness().await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::NotLinked), "{err:?}");
}

#[tokio::test]
async fn a_token_expiring_within_five_minutes_is_refreshed_and_the_plan_reread() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "grant_type": "refresh_token",
            "refresh_token": "refresh-1",
            "client_id": CLIENT_ID,
            "scope": "user:profile user:inference",
        })))
        .respond_with(token_response("access-2", Some("refresh-2")))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_pro").await;
    link(&h.store, h.member, "access-1", "refresh-1", 4 * 60).await;

    let before = now();
    let token = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-2");
    wait_for_plan(&h.store, h.member, "claude_pro").await;
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.access_token.expose_secret(), "access-2");
    assert_eq!(stored.refresh_token.expose_secret(), "refresh-2");
    assert_eq!(
        stored.rate_limit_tier.as_deref(),
        Some("default_claude_pro")
    );
    assert!(stored.expires_at - before > time::Duration::seconds(28_790));

    let again = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(again.expose_secret(), "access-2");
}

#[tokio::test]
async fn an_expired_token_is_refreshed() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-2", Some("refresh-2")))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_max").await;
    link(&h.store, h.member, "access-1", "refresh-1", -3600).await;
    let token = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-2");
}

#[tokio::test]
async fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-2", None))
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_max").await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    h.auth.access_token(h.member).await.unwrap();
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.access_token.expose_secret(), "access-2");
    assert_eq!(stored.refresh_token.expose_secret(), "refresh-1");
}

#[tokio::test]
async fn a_refresh_keeps_the_old_plan_when_the_profile_fails() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-2", Some("refresh-2")))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    h.auth.access_token(h.member).await.unwrap();
    eventually("the profile is read", || async {
        !requests_to(&h.server, PROFILE_PATH).await.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.access_token.expose_secret(), "access-2");
    assert_eq!(stored.plan.as_deref(), Some("claude_max"));
    assert_eq!(
        stored.rate_limit_tier.as_deref(),
        Some("default_claude_max")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent_calls_during_expiry_cause_one_refresh() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(
            token_response("access-2", Some("refresh-2")).set_delay(Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(profile_response("claude_max"))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let (auth, member) = (h.auth.clone(), h.member);
        tasks.spawn(async move { auth.access_token(member).await });
    }
    let mut tokens = Vec::new();
    while let Some(result) = tasks.join_next().await {
        tokens.push(result.unwrap().unwrap().expose_secret().to_owned());
    }
    assert_eq!(tokens, vec!["access-2"; 10]);
    assert_eq!(requests_to(&h.server, TOKEN_PATH).await.len(), 1);
    eventually("the profile is read", || async {
        !requests_to(&h.server, PROFILE_PATH).await.is_empty()
    })
    .await;
}

#[tokio::test]
async fn a_refused_refresh_breaks_the_link_and_requires_relink() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "refresh token refresh-1 revoked",
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let mut notices = h.auth.take_relink_notices().unwrap();
    assert!(h.auth.take_relink_notices().is_none());

    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("refresh-1"));
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert!(stored.broken_at.is_some());
    assert_eq!(notices.try_recv().unwrap(), h.member);

    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    assert!(notices.try_recv().is_err());
}

#[tokio::test]
async fn terminal_oauth_errors_and_an_account_on_hold_break_the_link() {
    let bodies = [
        (401, json!({"error": "invalid_grant"})),
        (400, json!({"error": "invalid_client"})),
        (400, json!({"error": "invalid_scope"})),
        (400, json!({"error": "unauthorized_client"})),
        (
            403,
            json!({"error": "access_denied", "error_description": "account_on_hold"}),
        ),
    ];
    for (status, body) in bodies {
        let h = harness().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_json(&body))
            .mount(&h.server)
            .await;
        link(&h.store, h.member, "access-1", "refresh-1", 60).await;
        let err = h.auth.access_token(h.member).await.unwrap_err();
        assert!(
            matches!(err, AuthError::RelinkRequired),
            "{status} {body}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_4xx_without_a_terminal_oauth_error_does_not_break_the_link() {
    let responses = [
        ResponseTemplate::new(403).set_body_string("<html>Just a moment...</html>"),
        ResponseTemplate::new(401),
        ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_request"})),
        ResponseTemplate::new(403).set_body_json(json!({"error": "invalid_grant"})),
    ];
    for response in responses {
        let h = harness().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(response)
            .expect(1)
            .mount(&h.server)
            .await;
        link(&h.store, h.member, "access-1", "refresh-1", 60).await;
        let token = h.auth.access_token(h.member).await.unwrap();
        assert_eq!(token.expose_secret(), "access-1");
        let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
        assert!(stored.broken_at.is_none());
        let mut notices = h.auth.take_relink_notices().unwrap();
        assert!(notices.try_recv().is_err());
    }
}

#[tokio::test]
async fn a_transient_refresh_failure_serves_the_current_token_until_it_expires() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let token = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-1");
    assert!(
        h.store
            .get_claude_link(h.member)
            .await
            .unwrap()
            .unwrap()
            .broken_at
            .is_none()
    );

    link(&h.store, h.member, "access-1", "refresh-1", -1).await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Status {
                endpoint: Endpoint::Token,
                status: 503,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        h.store
            .get_claude_link(h.member)
            .await
            .unwrap()
            .unwrap()
            .broken_at
            .is_none()
    );
}

#[tokio::test]
async fn an_unreadable_refresh_response_does_not_break_the_link() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", -1).await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::InvalidResponse {
                endpoint: Endpoint::Token,
                ..
            }
        ),
        "{err:?}"
    );
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert!(stored.broken_at.is_none());
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_an_http_error() {
    let h = harness().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_uri = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let config = OAuthConfig {
        token_url: format!("{dead_uri}{TOKEN_PATH}"),
        ..OAuthConfig::default()
    };
    let auth = Auth::new(config, h.store.clone()).unwrap();
    link(&h.store, h.member, "access-1", "refresh-1", -1).await;
    let err = auth.access_token(h.member).await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Http {
                endpoint: Endpoint::Token,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains(&dead_uri));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let h = harness().await;
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/steal", elsewhere.uri())),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(token_response("stolen", Some("stolen")))
        .expect(0)
        .mount(&elsewhere)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", -1).await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(
        matches!(err, AuthError::Status { status: 307, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn logout_deletes_the_link_and_revokes_the_refresh_token() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "token": "refresh-1",
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 3600).await;
    assert!(h.auth.logout(h.member).await.unwrap());
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    assert!(!h.auth.logout(h.member).await.unwrap());
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::NotLinked), "{err:?}");
}

#[tokio::test]
async fn logout_succeeds_when_revocation_fails() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 3600).await;
    assert!(h.auth.logout(h.member).await.unwrap());
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_in_flight_during_logout_does_not_relink() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-2", Some("refresh-2")))
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_max").await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(body_json(json!({
            "token": "refresh-2",
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let refresh = {
        let (auth, member) = (h.auth.clone(), h.member);
        tokio::spawn(async move { auth.access_token(member).await })
    };
    eventually("the refresh is sent", || async {
        !requests_to(&h.server, TOKEN_PATH).await.is_empty()
    })
    .await;
    assert!(h.auth.logout(h.member).await.unwrap());
    refresh.await.unwrap().unwrap();
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    assert!(matches!(
        h.auth.access_token(h.member).await,
        Err(AuthError::NotLinked)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_finishing_after_the_link_was_deleted_does_not_recreate_it() {
    let h = harness().await;
    let (held, mut hold) = Held::new(token_response("access-2", Some("refresh-2")));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_max").await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(body_json(json!({
            "token": "refresh-2",
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let refresh = {
        let (auth, member) = (h.auth.clone(), h.member);
        tokio::spawn(async move { auth.access_token(member).await })
    };
    hold.arrived().await;
    assert!(h.store.delete_claude_link(h.member).await.unwrap());
    hold.release();
    let err = refresh.await.unwrap().unwrap_err();
    assert!(matches!(err, AuthError::NotLinked), "{err:?}");
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
    eventually("the orphaned refresh token is revoked", || async {
        !requests_to(&h.server, REVOKE_PATH).await.is_empty()
    })
    .await;
}

async fn cancel_once_held(h: &Harness, mut hold: Hold) {
    tokio::select! {
        result = h.auth.access_token(h.member) => {
            panic!("the call ended before it was cancelled: {result:?}")
        }
        () = hold.arrived() => {}
    }
    hold.release();
}

async fn stored_tokens(store: &Store, member: MemberId) -> (String, String) {
    let link = store.get_claude_link(member).await.unwrap().unwrap();
    (
        link.access_token.expose_secret().to_owned(),
        link.refresh_token.expose_secret().to_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_caller_does_not_lose_the_rotated_refresh_token() {
    let h = harness().await;
    let (held, hold) = Held::new(token_response("access-2", Some("refresh-2")));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_pro").await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    cancel_once_held(&h, hold).await;
    eventually("the rotated tokens are stored", || async {
        stored_tokens(&h.store, h.member).await == ("access-2".to_owned(), "refresh-2".to_owned())
    })
    .await;
    wait_for_plan(&h.store, h.member, "claude_pro").await;
    let token = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_caller_does_not_lose_the_broken_mark_or_the_notice() {
    let h = harness().await;
    let (held, hold) =
        Held::new(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let mut notices = h.auth.take_relink_notices().unwrap();

    cancel_once_held(&h, hold).await;
    let notice = tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap();
    assert_eq!(notice, Some(h.member));
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert!(stored.broken_at.is_some());
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err:?}");
    assert!(notices.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent_calls_during_a_failing_refresh_send_one_request() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(200)))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let (auth, member) = (h.auth.clone(), h.member);
        tasks.spawn(async move { auth.access_token(member).await });
    }
    let mut tokens = Vec::new();
    while let Some(result) = tasks.join_next().await {
        tokens.push(result.unwrap().unwrap().expose_secret().to_owned());
    }
    assert_eq!(tokens, vec!["access-1"; 10]);
    assert_eq!(requests_to(&h.server, TOKEN_PATH).await.len(), 1);

    let token = h.auth.access_token(h.member).await.unwrap();
    assert_eq!(token.expose_secret(), "access-1");
    assert_eq!(requests_to(&h.server, TOKEN_PATH).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_share_a_failed_refresh_of_an_expired_token() {
    let h = harness().await;
    let (held, mut hold) = Held::new(ResponseTemplate::new(503));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", -1).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..5 {
        let (auth, member) = (h.auth.clone(), h.member);
        tasks.spawn(async move { auth.access_token(member).await });
    }
    hold.arrived().await;
    eventually("all five callers wait for the refresh", || async {
        h.auth.refresh_waiters(h.member) == 5
    })
    .await;
    hold.release();
    while let Some(result) = tasks.join_next().await {
        let err = result.unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                AuthError::Status {
                    endpoint: Endpoint::Token,
                    status: 503,
                    ..
                }
            ),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn a_token_that_expires_during_a_failed_refresh_is_not_returned() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(2_500)))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 2).await;
    let err = h.auth.access_token(h.member).await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Status {
                endpoint: Endpoint::Token,
                status: 503,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_plan_is_read_after_the_members_lock_is_released() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(token_response("access-2", Some("refresh-2")))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(profile_response("claude_pro").set_delay(Duration::from_millis(1_500)))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .respond_with(ResponseTemplate::new(200))
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let token = tokio::time::timeout(Duration::from_millis(1_000), h.auth.access_token(h.member))
        .await
        .expect("the caller doesn't wait for the profile")
        .unwrap();
    assert_eq!(token.expose_secret(), "access-2");
    eventually("the profile is requested", || async {
        !requests_to(&h.server, PROFILE_PATH).await.is_empty()
    })
    .await;
    let logged_out = tokio::time::timeout(Duration::from_millis(1_000), h.auth.logout(h.member))
        .await
        .expect("logout doesn't wait for the profile");
    assert!(logged_out.unwrap());
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(h.store.get_claude_link(h.member).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_refresh_does_not_break_a_newer_login() {
    let h = harness().await;
    let (held, mut hold) =
        Held::new(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;
    let mut notices = h.auth.take_relink_notices().unwrap();

    let refresh = {
        let (auth, member) = (h.auth.clone(), h.member);
        tokio::spawn(async move { auth.access_token(member).await })
    };
    hold.arrived().await;
    link(&h.store, h.member, "access-new", "refresh-new", 3600).await;
    hold.release();
    let token = refresh.await.unwrap().unwrap();
    assert_eq!(token.expose_secret(), "access-new");
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert!(stored.broken_at.is_none());
    assert!(notices.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_finishing_after_a_new_login_does_not_overwrite_it() {
    let h = harness().await;
    let (held, mut hold) = Held::new(token_response("access-2", Some("refresh-2")));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(held)
        .expect(1)
        .mount(&h.server)
        .await;
    mount_profile(&h.server, "access-2", "claude_pro").await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(body_json(json!({
            "token": "refresh-2",
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    link(&h.store, h.member, "access-1", "refresh-1", 60).await;

    let refresh = {
        let (auth, member) = (h.auth.clone(), h.member);
        tokio::spawn(async move { auth.access_token(member).await })
    };
    hold.arrived().await;
    let fresh = link(&h.store, h.member, "access-new", "refresh-new", 3600).await;
    hold.release();
    let token = refresh.await.unwrap().unwrap();
    assert_eq!(token.expose_secret(), "access-new");
    assert_eq!(
        stored_tokens(&h.store, h.member).await,
        ("access-new".to_owned(), "refresh-new".to_owned())
    );
    eventually("the orphaned refresh token is revoked", || async {
        !requests_to(&h.server, REVOKE_PATH).await.is_empty()
    })
    .await;
    let stored = h.store.get_claude_link(h.member).await.unwrap().unwrap();
    assert_eq!(stored.generation, fresh);
    assert_eq!(stored.plan.as_deref(), Some("claude_max"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_logins_leave_exactly_one_pending_login() {
    let h = harness().await;
    let starts = tokio::join!(
        h.auth.start_login(h.member),
        h.auth.start_login(h.member),
        h.auth.start_login(h.member),
        h.auth.start_login(h.member),
        h.auth.start_login(h.member),
        h.auth.start_login(h.member),
    );
    let mut live = 0;
    for start in [starts.0, starts.1, starts.2, starts.3, starts.4, starts.5] {
        let state = query(&Url::parse(&start.unwrap().url).unwrap(), "state");
        if h.store.take_pending_login(&state).await.unwrap().is_some() {
            live += 1;
        }
    }
    assert_eq!(live, 1);
}

#[tokio::test]
async fn status_reports_the_link_the_plan_and_a_break() {
    let h = harness().await;
    assert_eq!(
        h.auth.status(h.member).await.unwrap(),
        LinkStatus {
            linked: false,
            plan: PlanInfo::default(),
            broken: false,
        }
    );
    let generation = link(&h.store, h.member, "access-1", "refresh-1", 3600).await;
    assert_eq!(
        h.auth.status(h.member).await.unwrap(),
        LinkStatus {
            linked: true,
            plan: PlanInfo {
                plan: Some(Plan::Max),
                rate_limit_tier: Some("default_claude_max".to_owned()),
            },
            broken: false,
        }
    );
    assert!(
        h.store
            .mark_claude_link_broken(h.member, generation, now())
            .await
            .unwrap()
    );
    let status = h.auth.status(h.member).await.unwrap();
    assert!(status.linked && status.broken);
}

#[tokio::test]
async fn status_reads_no_token() {
    let path = std::env::temp_dir().join(format!("auth-status-{}.db", MemberId::new_v4()));
    let url = format!("sqlite://{}", path.display());
    let key = || Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
    let store = Store::open(&url, key()).await.unwrap();
    let member = store
        .ensure_member(&member_key("ada"), "Ada", now())
        .await
        .unwrap();
    link(&store, member, "access-1", "refresh-1", 3600).await;
    drop(store);

    let store = Store::open(&url, key()).await.unwrap();
    assert!(store.get_claude_link(member).await.is_err());
    let auth = Auth::new(OAuthConfig::default(), store.clone()).unwrap();
    let status = auth.status(member).await.unwrap();
    assert!(status.linked && !status.broken);
    assert_eq!(status.plan.plan, Some(Plan::Max));
    drop((auth, store));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[tokio::test]
async fn errors_never_contain_secrets() {
    let secrets = ["access-secret", "refresh-secret", "code-secret"];
    let errors = [
        AuthError::NotLinked,
        AuthError::RelinkRequired,
        AuthError::RefreshInterrupted,
        AuthError::MalformedCode,
        AuthError::UnknownLogin,
        AuthError::LoginExpired,
        AuthError::CodeRejected {
            status: 400,
            error: None,
        },
        AuthError::Status {
            endpoint: Endpoint::Token,
            status: 500,
            error: Some("server_error".to_owned()),
        },
        AuthError::InvalidResponse {
            endpoint: Endpoint::Profile,
            reason: "not the expected JSON",
        },
        AuthError::Random,
    ];
    for err in errors {
        let text = format!("{err} {err:?}");
        for secret in secrets {
            assert!(!text.contains(secret));
        }
    }
    let h = harness().await;
    let debug = format!("{:?}", h.auth);
    assert!(debug.starts_with("Auth {"));
}

#[tokio::test]
async fn an_invalid_configuration_is_refused() {
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let config = OAuthConfig {
        token_url: "http://platform.claude.com/v1/oauth/token".to_owned(),
        ..OAuthConfig::default()
    };
    let err = Auth::new(config, store).unwrap_err();
    assert!(matches!(err, AuthError::Config(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "claude_oauth.token_url: must use https unless the host is loopback"
    );
}
