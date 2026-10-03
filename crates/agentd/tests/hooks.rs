//! agentd's turn hooks against the real credential proxy and agentctl API:
//! what a process gets, and what each hook points, clears and revokes.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use agentd::community::StoreCommunityKey;
use agentd::ctl::{Ctl, CtlSettings, NoSurfaces};
use agentd::pipeline::{AGENTCTL_TOKEN_VAR, AGENTCTL_URL_VAR, Hooks};
use auth::{Auth, OAuthConfig, TokenSource};
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use core_types::{
    AgentId, ConvRef, CredentialKind, CredentialRef, Hop, MemberId, MemberKey, MessageId,
    Requester, ScopeKey, Side, SurfaceKind, ThreadKey, TurnId, TurnKind,
};
use cred_proxy::{CredProxy, EGRESS_ENV, Registry, SUBSCRIPTION_PREFIX};
use http_body_util::BodyExt as _;
use runner::{ProcessEnv, Session, TurnHooks, TurnRequest};
use secrecy::{ExposeSecret as _, SecretString};
use store::{NewClaudeLink, Sealer, Store};
use testkit::{FakeAnthropic, TempDir, fake_anthropic};
use time::OffsetDateTime;
use tower::ServiceExt as _;

const ACCESS_TOKEN: &str = "real-access-token";
const CONTAINER: &str = "127.0.0.1";

struct Rig {
    store: Store,
    registry: Registry,
    ctl: Ctl,
    hooks: Hooks,
    proxy: Router,
    fake: FakeAnthropic,
    member: MemberId,
    session: Session,
    _dir: TempDir,
}

async fn rig() -> Rig {
    let dir = TempDir::new("agentd-test");
    let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
    let store = Store::open_in_memory(sealer).await.unwrap();
    let now = OffsetDateTime::now_utc();
    let alice = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: "chat.example".into(),
        user: "alice".into(),
    };
    let member = store.ensure_member(&alice, "alice", now).await.unwrap();
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from(ACCESS_TOKEN),
                refresh_token: SecretString::from("refresh"),
                expires_at: now + Duration::from_secs(24 * 60 * 60),
                plan: None,
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    let fake = fake_anthropic().await;
    let registry = Registry::new();
    let ctl = Ctl::new(
        store.clone(),
        CtlSettings {
            staging_dir: dir.join("ctl-outbox"),
            attach_max_bytes: 1024,
            lease_ttl: Duration::from_secs(30),
        },
        Arc::new(NoSurfaces),
    );
    let hooks = Hooks::new(
        registry.clone(),
        ctl.clone(),
        "http://agentctl.test:8081",
        BTreeMap::from([
            ("NO_PROXY".to_owned(), "127.0.0.2".to_owned()),
            ("EXTRA".to_owned(), "value".to_owned()),
        ]),
    );
    let tokens: Arc<dyn TokenSource> =
        Arc::new(Auth::new(OAuthConfig::default(), store.clone()).unwrap());
    let proxy = CredProxy::new(
        &fake.uri(),
        registry.clone(),
        tokens,
        Arc::new(StoreCommunityKey::new(store.clone())),
    )
    .unwrap()
    .into_router();
    let thread = ThreadKey {
        conv: ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example".into(),
            conversation: "GENERAL".into(),
        },
        root: Some(MessageId::new("m1")),
    };
    let scope = ScopeKey::Channel(thread.conv.clone());
    let session = store
        .session_for_thread(AgentId::new_v4(), &thread, &scope, now)
        .await
        .unwrap()
        .session;
    Rig {
        store,
        registry,
        ctl,
        hooks,
        proxy,
        fake,
        member,
        session,
        _dir: dir,
    }
}

impl Rig {
    fn turn(&self, credential: CredentialRef) -> TurnRequest {
        TurnRequest {
            turn: TurnId::new_v4(),
            message: "hi".into(),
            credential,
            model: None,
            requester: Requester {
                member: Some(self.member),
                key: MemberKey {
                    surface: SurfaceKind::RocketChat,
                    team: "chat.example".into(),
                    user: "alice".into(),
                },
            },
            hop: Hop::ZERO,
            side: Side::Public,
            kind: TurnKind::Normal,
            trigger: Some(MessageId::new("m2")),
        }
    }

    /// A request to the credential proxy from the container, carrying
    /// `placeholder` as a bearer token: its status and body.
    async fn through_proxy(&self, placeholder: &SecretString) -> (StatusCode, String) {
        self.through_proxy_in(
            "authorization",
            format!("Bearer {}", placeholder.expose_secret()),
        )
        .await
    }

    /// A request to the credential proxy from the container, carrying
    /// `value` in the header `name`: its status and body.
    async fn through_proxy_in(&self, name: &str, value: String) -> (StatusCode, String) {
        let mut request = Request::post("/v1/messages?beta=true")
            .header(name, value)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"m","messages":[]}"#))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            CONTAINER.parse().unwrap(),
            40_000,
        )));
        let response = self.proxy.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn start(&self) -> (ProcessEnv, agentd::pipeline::ProcessHandle) {
        self.hooks
            .process_starting(
                &self.session,
                CONTAINER.parse::<IpAddr>().unwrap(),
                CredentialKind::Subscription,
            )
            .await
            .unwrap()
    }
}

fn env_value<'a>(env: &'a ProcessEnv, name: &str) -> &'a str {
    env.env
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .expose_secret()
}

#[tokio::test]
async fn a_process_gets_a_placeholder_an_agentctl_token_and_the_egress_proxy() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    assert!(
        env.placeholder
            .expose_secret()
            .starts_with(SUBSCRIPTION_PREFIX)
    );
    assert_eq!(process.session(), rig.session.id);
    for (name, value) in EGRESS_ENV {
        if name != "NO_PROXY" {
            assert_eq!(env_value(&env, name), value, "{name}");
        }
    }
    assert_eq!(env_value(&env, "NO_PROXY"), "127.0.0.2");
    assert_eq!(env_value(&env, "EXTRA"), "value");
    assert_eq!(
        env_value(&env, AGENTCTL_URL_VAR),
        "http://agentctl.test:8081"
    );
    let token = env_value(&env, AGENTCTL_TOKEN_VAR);
    assert_eq!(token.len(), 43);
    let debug = format!("{env:?} {process:?}");
    assert!(!debug.contains(token), "{debug}");
    assert!(!debug.contains(env.placeholder.expose_secret()), "{debug}");
}

#[tokio::test]
async fn a_placeholder_works_only_while_its_turn_runs() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(rig.fake.requests().await.is_empty());

    let turn = rig.turn(CredentialRef::Member(rig.member));
    rig.hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap();
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = rig.fake.message_requests().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].headers.get("authorization").unwrap(),
        &format!("Bearer {ACCESS_TOKEN}")
    );

    let outbox = rig
        .hooks
        .turn_finished(&rig.session, &process, &turn)
        .await
        .unwrap()
        .expect("the turn's outbox");
    assert_eq!(outbox.turn(), turn.turn);
    assert!(outbox.is_empty());
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(rig.fake.message_requests().await.len(), 1);
}

#[tokio::test]
async fn a_credential_of_the_other_kind_fails_the_turn_start() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    let turn = rig.turn(CredentialRef::Community);
    let err = rig
        .hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap_err();
    assert!(!err.to_string().contains(env.placeholder.expose_secret()));
    rig.hooks
        .turn_finished(&rig.session, &process, &turn)
        .await
        .unwrap();
    let (status, _) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_owners_side_runs_only_in_the_private_session() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    let turn = TurnRequest {
        side: Side::Owner,
        ..rig.turn(CredentialRef::Member(rig.member))
    };
    let err = rig
        .hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("owner's side"), "{err}");
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        rig.hooks
            .turn_finished(&rig.session, &process, &turn)
            .await
            .unwrap()
            .is_none()
    );

    let private = rig
        .store
        .session_for_thread(
            rig.session.agent,
            &ThreadKey {
                conv: ConvRef {
                    surface: SurfaceKind::RocketChat,
                    team: "chat.example".into(),
                    conversation: "DM-OWNER".into(),
                },
                root: None,
            },
            &ScopeKey::Private,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
        .session;
    let (env, process) = rig
        .hooks
        .process_starting(
            &private,
            CONTAINER.parse::<IpAddr>().unwrap(),
            CredentialKind::Subscription,
        )
        .await
        .unwrap();
    rig.hooks
        .turn_starting(&private, &process, &turn)
        .await
        .unwrap();
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        rig.hooks
            .turn_finished(&private, &process, &turn)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn stopping_revokes_the_placeholder_and_the_token_and_is_idempotent() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    rig.hooks
        .process_stopping(&rig.session, &process)
        .await
        .unwrap();
    rig.hooks
        .process_stopping(&rig.session, &process)
        .await
        .unwrap();
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("no credential placeholder"), "{body}");

    let turn = rig.turn(CredentialRef::Member(rig.member));
    let err = rig
        .hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no live placeholder"), "{err}");
    let finished = rig
        .hooks
        .turn_finished(&rig.session, &process, &turn)
        .await
        .unwrap();
    assert!(finished.is_none());
}

#[tokio::test]
async fn a_late_stop_for_an_old_process_leaves_the_new_one_alone() {
    let rig = rig().await;
    let (_, old) = rig.start().await;
    let (env, new) = rig.start().await;
    rig.hooks
        .process_stopping(&rig.session, &old)
        .await
        .unwrap();

    let turn = rig.turn(CredentialRef::Member(rig.member));
    rig.hooks
        .turn_starting(&rig.session, &new, &turn)
        .await
        .unwrap();
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        rig.hooks
            .turn_finished(&rig.session, &new, &turn)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_token_that_cant_be_issued_takes_the_placeholder_back() {
    let rig = rig().await;
    rig.store.close().await;
    let err = rig
        .hooks
        .process_starting(
            &rig.session,
            CONTAINER.parse::<IpAddr>().unwrap(),
            CredentialKind::Subscription,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("database"), "{err}");
    assert_eq!(rig.registry.revoke_session(rig.session.id), 0);
}

#[tokio::test]
async fn a_turn_whose_token_record_fails_still_unpoints() {
    let rig = rig().await;
    let (env, process) = rig.start().await;
    let turn = rig.turn(CredentialRef::Member(rig.member));
    rig.hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap();
    rig.store.close().await;
    let err = rig
        .hooks
        .turn_finished(&rig.session, &process, &turn)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("database"), "{err}");
    let (status, body) = rig.through_proxy(&env.placeholder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    drop(rig.ctl);
}

#[tokio::test]
async fn a_community_turn_gets_the_key_an_admin_set_and_only_through_the_proxy() {
    const KEY: &str = "sk-ant-api03-community-hooks-test";
    let rig = rig().await;
    let (env, process) = rig
        .hooks
        .process_starting(
            &rig.session,
            CONTAINER.parse::<IpAddr>().unwrap(),
            CredentialKind::ApiKey,
        )
        .await
        .unwrap();
    for value in env.env.values() {
        assert!(!value.expose_secret().contains(KEY));
    }
    let placeholder = env.placeholder.expose_secret().to_owned();
    let turn = rig.turn(CredentialRef::Community);
    rig.hooks
        .turn_starting(&rig.session, &process, &turn)
        .await
        .unwrap();
    let (status, body) = rig.through_proxy_in("x-api-key", placeholder.clone()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no key is set yet: {body}"
    );
    assert!(rig.fake.requests().await.is_empty());

    let admin: MemberKey = "rocketchat:chat.example:root".parse().unwrap();
    rig.store
        .set_community_api_key(&SecretString::from(KEY), &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let (status, body) = rig.through_proxy_in("x-api-key", placeholder.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = rig.fake.message_requests().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].headers.get("x-api-key").unwrap(), KEY);
    assert!(seen[0].headers.get("authorization").is_none());
    assert!(!body.contains(KEY));

    rig.store
        .clear_community_api_key(&admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let (status, body) = rig.through_proxy_in("x-api-key", placeholder.clone()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a cleared key is gone at once"
    );
    assert!(!body.contains(KEY));
    assert_eq!(rig.fake.message_requests().await.len(), 1);

    rig.store
        .set_community_api_key(&SecretString::from(KEY), &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    rig.hooks
        .turn_finished(&rig.session, &process, &turn)
        .await
        .unwrap();
    let (status, _) = rig.through_proxy_in("x-api-key", placeholder).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "between turns the placeholder reaches no key"
    );
    assert_eq!(rig.fake.message_requests().await.len(), 1);
}
