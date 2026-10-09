//! Nothing the proxy logs, at any level, holds a placeholder or a
//! credential.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use core_types::{CredentialRef, MemberId, SessionId};
use cred_proxy::{CredProxy, FixedKey, Registry};
use secrecy::{ExposeSecret as _, SecretString};
use testkit::{Logs, fake_anthropic};
use tokio::net::TcpListener;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const OAUTH_TOKEN: &str = "real-oauth-token-for-logging";
const COMMUNITY_KEY: &str = "real-community-key-for-logging";

struct AnyMember;

#[async_trait]
impl TokenSource for AnyMember {
    async fn access_token(&self, _member: MemberId) -> Result<SecretString, AuthError> {
        Ok(SecretString::from(OAUTH_TOKEN))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn logs_never_hold_placeholders_or_credentials() {
    let logs = Logs::global();
    let fake = fake_anthropic().await;
    let registry = Registry::new();
    let router = CredProxy::new(
        &fake.uri(),
        registry.clone(),
        Arc::new(AnyMember),
        Arc::new(FixedKey::new(SecretString::from(COMMUNITY_KEY))),
    )
    .unwrap()
    .into_router();
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let session = SessionId::new_v4();
    let sub = registry
        .mint(
            session,
            LOCAL,
            CredentialRef::Member(MemberId::new_v4()).kind(),
        )
        .unwrap();
    let key = registry
        .mint(session, LOCAL, CredentialRef::Community.kind())
        .unwrap();
    registry
        .point(sub.id(), CredentialRef::Member(MemberId::new_v4()))
        .unwrap();
    registry.point(key.id(), CredentialRef::Community).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let requests = [
        ("authorization", format!("Bearer {}", sub.expose_secret())),
        ("x-api-key", key.expose_secret().to_owned()),
        ("x-api-key", sub.expose_secret().to_owned()),
        (
            "authorization",
            "Bearer agentd-sub-unknown-marker".to_owned(),
        ),
    ];
    for (name, value) in requests {
        client
            .post(format!("{base}/v1/messages?beta=true"))
            .header(name, value)
            .body(r#"{"model":"m","messages":[]}"#)
            .send()
            .await
            .unwrap();
    }
    client
        .get(format!(
            "{base}/v1/path-secret-marker/models?beta=query-secret-marker"
        ))
        .header("authorization", format!("Bearer {}", sub.expose_secret()))
        .send()
        .await
        .unwrap();
    let other = registry
        .mint(SessionId::new_v4(), LOCAL, key.kind())
        .unwrap();

    let logged = logs.snapshot();
    logged
        .assert_has("forwarded a request")
        .assert_has("the credential proxy refused a request")
        .assert_has("revoked placeholders another session still had");
    for secret in [
        sub.expose_secret(),
        key.expose_secret(),
        other.expose_secret(),
        "agentd-sub-unknown-marker",
        "path-secret-marker",
        "query-secret-marker",
        OAUTH_TOKEN,
        COMMUNITY_KEY,
    ] {
        logged.assert_lacks(secret);
    }
    assert_eq!(fake.message_requests().await.len(), 2);
}
