//! Nothing the proxy logs, at any level, holds a placeholder or a
//! credential.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use core_types::{CredentialRef, MemberId, SessionId};
use cred_proxy::{CredProxy, FixedKey, Registry};
use secrecy::{ExposeSecret as _, SecretString};
use testkit::fake_anthropic;
use tokio::net::TcpListener;
use tracing_subscriber::fmt::MakeWriter;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const OAUTH_TOKEN: &str = "real-oauth-token-for-logging";
const COMMUNITY_KEY: &str = "real-community-key-for-logging";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct AnyMember;

#[async_trait]
impl TokenSource for AnyMember {
    async fn access_token(&self, _member: MemberId) -> Result<SecretString, AuthError> {
        Ok(SecretString::from(OAUTH_TOKEN))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn logs_never_hold_placeholders_or_credentials() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

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
    let other = registry
        .mint(SessionId::new_v4(), LOCAL, key.kind())
        .unwrap();

    let logs = capture.text();
    assert!(logs.contains("forwarded a request"), "{logs}");
    assert!(
        logs.contains("the credential proxy refused a request"),
        "{logs}"
    );
    assert!(
        logs.contains("revoked placeholders another session still had"),
        "{logs}"
    );
    for secret in [
        sub.expose_secret(),
        key.expose_secret(),
        other.expose_secret(),
        "agentd-sub-unknown-marker",
        OAUTH_TOKEN,
        COMMUNITY_KEY,
    ] {
        assert!(!logs.contains(secret), "a secret reached the log:\n{logs}");
    }
    assert_eq!(fake.message_requests().await.len(), 2);
}
