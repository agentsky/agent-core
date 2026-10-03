//! The proxy's rules, each checked through a real listener, a real client
//! and a recording upstream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use axum::body::Body;
use axum::routing::post;
use bytes::Bytes;
use core_types::{CredentialKind, CredentialRef, MemberId, SessionId};
use cred_proxy::{
    CommunityKey, CommunityKeyError, CredProxy, FixedKey, Observation, Placeholder, ProxyObserver,
    Registry,
};
use futures::{SinkExt as _, StreamExt as _};
use secrecy::{ExposeSecret as _, SecretString};
use testkit::claude::{API_KEY_BETA, OAUTH_BETA};
use testkit::fake_anthropic;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const LOCAL_2: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
const ELSEWHERE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 9, 9, 9));
const COMMUNITY_KEY: &str = "sk-ant-api-community-real";
const WAIT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A gate a token lookup waits at: `entered` fires when it arrives,
/// `release` lets it go on.
struct Gate {
    member: MemberId,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

/// A [`TokenSource`] with a fixed answer per member.
#[derive(Default)]
struct Tokens {
    answers: Mutex<HashMap<MemberId, Result<&'static str, AuthError>>>,
    gate: Mutex<Option<Gate>>,
}

impl Tokens {
    fn answer(&self, member: MemberId, answer: Result<&'static str, AuthError>) {
        lock(&self.answers).insert(member, answer);
    }

    fn gate(&self, member: MemberId) -> (Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *lock(&self.gate) = Some(Gate {
            member,
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        (entered, release)
    }
}

#[async_trait]
impl TokenSource for Tokens {
    async fn access_token(&self, member: MemberId) -> Result<SecretString, AuthError> {
        let gate = lock(&self.gate)
            .as_ref()
            .filter(|gate| gate.member == member)
            .map(|gate| (Arc::clone(&gate.entered), Arc::clone(&gate.release)));
        if let Some((entered, release)) = gate {
            entered.notify_one();
            release.notified().await;
        }
        lock(&self.answers)
            .get(&member)
            .cloned()
            .unwrap_or(Err(AuthError::NotLinked))
            .map(SecretString::from)
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Observation>>);

impl ProxyObserver for Recorder {
    fn observe(&self, observation: &Observation) {
        lock(&self.0).push(observation.clone());
    }
}

struct Unconfigured;

#[async_trait]
impl CommunityKey for Unconfigured {
    async fn api_key(&self) -> Result<SecretString, CommunityKeyError> {
        Err(CommunityKeyError::NotConfigured)
    }
}

/// A running proxy on a free local port.
struct Proxy {
    addr: SocketAddr,
    registry: Registry,
    tokens: Arc<Tokens>,
    observed: Arc<Recorder>,
}

impl Proxy {
    async fn start(upstream: &str) -> Self {
        Self::start_with(
            upstream,
            Arc::new(FixedKey::new(SecretString::from(COMMUNITY_KEY))),
        )
        .await
    }

    async fn start_with(upstream: &str, community: Arc<dyn CommunityKey>) -> Self {
        let registry = Registry::new();
        let tokens = Arc::new(Tokens::default());
        let observed = Arc::new(Recorder::default());
        let router = CredProxy::new(
            upstream,
            registry.clone(),
            Arc::clone(&tokens) as Arc<dyn TokenSource>,
            community,
        )
        .unwrap()
        .with_observer(Arc::clone(&observed) as Arc<dyn ProxyObserver>)
        .into_router();
        let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            addr,
            registry,
            tokens,
            observed,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// A placeholder for a new session at `ip`, pointed at `credential`.
    fn pointed(&self, ip: IpAddr, credential: CredentialRef) -> (SessionId, Placeholder) {
        let session = SessionId::new_v4();
        (session, self.pointed_in(session, ip, credential))
    }

    /// A placeholder for `session` at `ip`, pointed at `credential`. An
    /// address belongs to one session, so placeholders sharing an address
    /// share a session.
    fn pointed_in(&self, session: SessionId, ip: IpAddr, credential: CredentialRef) -> Placeholder {
        let placeholder = self.registry.mint(session, ip, credential.kind()).unwrap();
        self.registry.point(placeholder.id(), credential).unwrap();
        placeholder
    }

    /// A linked member whose access token is `token`, and a placeholder
    /// for a new session at `ip` pointed at them.
    fn linked(&self, ip: IpAddr, token: &'static str) -> (MemberId, Placeholder) {
        self.linked_in(SessionId::new_v4(), ip, token)
    }

    /// Like [`linked`](Self::linked), for `session`.
    fn linked_in(
        &self,
        session: SessionId,
        ip: IpAddr,
        token: &'static str,
    ) -> (MemberId, Placeholder) {
        let member = MemberId::new_v4();
        self.tokens.answer(member, Ok(token));
        let placeholder = self.pointed_in(session, ip, CredentialRef::Member(member));
        (member, placeholder)
    }
}

fn client_from(ip: IpAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .local_address(ip)
        .build()
        .unwrap()
}

fn client() -> reqwest::Client {
    client_from(LOCAL)
}

const MESSAGE: &str = r#"{"model":"claude-test","messages":[{"role":"user","content":"hi"}]}"#;

fn bearer(placeholder: &Placeholder) -> String {
    format!("Bearer {}", placeholder.expose_secret())
}

/// Sends `request` as raw bytes and returns everything the server wrote
/// before closing.
async fn raw(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("the server did not close the connection")
        .unwrap();
    String::from_utf8(response).unwrap()
}

fn header<'a>(request: &'a testkit::anthropic::Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap())
}

#[tokio::test]
async fn swaps_bearer_for_subscription_placeholder() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (member, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let response = client()
        .post(proxy.url("/v1/messages?beta=true"))
        .header("authorization", bearer(&placeholder))
        .header("anthropic-beta", OAUTH_BETA)
        .header("anthropic-version", "2023-06-01")
        .header("x-app", "cli")
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");

    let requests = fake.message_requests().await;
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(
        header(sent, "authorization"),
        Some("Bearer real-oauth-token")
    );
    assert_eq!(header(sent, "x-api-key"), None);
    assert_eq!(header(sent, "anthropic-beta"), Some(OAUTH_BETA));
    assert_eq!(header(sent, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(header(sent, "x-app"), Some("cli"));
    assert_eq!(sent.url.query(), Some("beta=true"));
    assert_eq!(sent.body, MESSAGE.as_bytes());

    let observed = lock(&proxy.observed.0);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].credential, CredentialRef::Member(member));
    assert_eq!(observed[0].status, 200);
}

#[tokio::test]
async fn swaps_x_api_key_for_api_key_placeholder() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (session, placeholder) = proxy.pointed(LOCAL, CredentialRef::Community);
    assert_eq!(placeholder.kind(), CredentialKind::ApiKey);
    let response = client()
        .post(proxy.url("/v1/messages?beta=true"))
        .header("x-api-key", placeholder.expose_secret())
        .header("anthropic-beta", API_KEY_BETA)
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let requests = fake.message_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(header(&requests[0], "x-api-key"), Some(COMMUNITY_KEY));
    assert_eq!(header(&requests[0], "authorization"), None);
    assert_eq!(header(&requests[0], "anthropic-beta"), Some(API_KEY_BETA));
    assert_eq!(requests[0].url.query(), Some("beta=true"));
    assert_eq!(lock(&proxy.observed.0)[0].session, session);
}

#[tokio::test]
async fn refuses_placeholder_of_wrong_kind() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (session, key) = proxy.pointed(LOCAL, CredentialRef::Community);
    let (_, sub) = proxy.linked_in(session, LOCAL, "real-oauth-token");
    let cases = [
        ("x-api-key", sub.expose_secret().to_owned()),
        ("authorization", bearer(&key)),
    ];
    for (name, value) in cases {
        let response = client()
            .post(proxy.url("/v1/messages"))
            .header(name, value)
            .body(MESSAGE)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{name}");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(
            body["error"]["message"],
            "The placeholder was sent in the wrong header for its kind."
        );
    }
    let both = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&sub))
        .header("x-api-key", key.expose_secret())
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), 400);
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn refuses_unknown_source_ip() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (_, placeholder) = proxy.linked(ELSEWHERE, "real-oauth-token");
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
    let hello = client().head(proxy.url("/api/hello")).send().await.unwrap();
    assert_eq!(hello.status(), 403);
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn refuses_placeholder_bound_to_other_ip() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (_, mine) = proxy.linked(LOCAL, "token-of-local");
    let (_, theirs) = proxy.linked(LOCAL_2, "token-of-local-2");
    let stolen = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&theirs))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(stolen.status(), 401);
    let unknown = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", "Bearer agentd-sub-never-minted")
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 401);
    assert_eq!(
        stolen.text().await.unwrap(),
        unknown.text().await.unwrap(),
        "a stolen placeholder reads like an unknown one"
    );
    assert!(fake.requests().await.is_empty());

    let own = client_from(LOCAL_2)
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&theirs))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), 200);
    let requests = fake.message_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests[0], "authorization"),
        Some("Bearer token-of-local-2")
    );
    drop(mine);
}

#[tokio::test]
async fn never_substitutes_in_body() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let text = placeholder.expose_secret();
    let body = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"{text} and Bearer {text}"}}]}}"#
    );
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .header("x-echo", text)
        .header("x-forwarded-token", bearer(&placeholder))
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let requests = fake.message_requests().await;
    let sent = &requests[0];
    assert_eq!(sent.body, body.as_bytes());
    assert_eq!(header(sent, "x-echo"), Some(text));
    assert_eq!(
        header(sent, "x-forwarded-token"),
        Some(bearer(&placeholder).as_str())
    );
    assert_eq!(
        header(sent, "authorization"),
        Some("Bearer real-oauth-token")
    );
    assert!(
        !String::from_utf8_lossy(&sent.body).contains("real-oauth-token"),
        "the real token never enters a body"
    );
}

#[tokio::test]
async fn ignores_client_host_header() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("host", "evil.example")
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let requests = fake.message_requests().await;
    assert_eq!(requests.len(), 1);
    let fake_authority = fake.uri().trim_start_matches("http://").to_owned();
    assert_eq!(header(&requests[0], "host"), Some(fake_authority.as_str()));

    let absolute = raw(
        proxy.addr,
        &format!(
            "POST http://evil.example/v1/messages HTTP/1.1\r\nHost: evil.example\r\n\
             Authorization: {}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            bearer(&placeholder)
        ),
    )
    .await;
    assert!(absolute.starts_with("HTTP/1.1 403"), "{absolute}");
    let connect = raw(
        proxy.addr,
        "CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example:443\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(connect.starts_with("HTTP/1.1 405"), "{connect}");
    assert_eq!(fake.requests().await.len(), 1);
}

/// An upstream that answers `POST /v1/messages` with a stream whose first
/// event is sent at once and the rest only after `release` fires.
async fn held_sse_upstream(release: oneshot::Receiver<()>) -> String {
    let release = Arc::new(Mutex::new(Some(release)));
    let app = axum::Router::new().route(
        "/v1/messages",
        post(move || {
            let release = lock(&release).take();
            async move {
                let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, Infallible>>(4);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(Bytes::from_static(
                            b"event: message_start\ndata: {}\n\n",
                        )))
                        .await;
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                    let _ = tx
                        .send(Ok(Bytes::from_static(b"event: message_stop\ndata: {}\n\n")))
                        .await;
                });
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(rx),
                )
            }
        }),
    );
    serve(app).await
}

async fn serve(app: axum::Router) -> String {
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn streams_sse_without_buffering() {
    let (release, held) = oneshot::channel();
    let upstream = held_sse_upstream(held).await;
    let proxy = Proxy::start(&upstream).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let mut response = client()
        .post(proxy.url("/v1/messages?beta=true"))
        .header("authorization", bearer(&placeholder))
        .body(r#"{"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let first = tokio::time::timeout(WAIT, response.chunk())
        .await
        .expect("the first event did not arrive while the upstream was still streaming")
        .unwrap()
        .unwrap();
    assert!(first.starts_with(b"event: message_start"), "{first:?}");
    release.send(()).unwrap();
    let mut rest = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(rest, b"event: message_stop\ndata: {}\n\n");
}

#[tokio::test]
async fn streams_request_body_without_buffering() {
    let (first_seen, saw_first) = oneshot::channel::<()>();
    let first_seen = Arc::new(Mutex::new(Some(first_seen)));
    let app = axum::Router::new().route(
        "/v1/messages",
        post(move |body: Body| {
            let first_seen = lock(&first_seen).take();
            async move {
                let mut stream = body.into_data_stream();
                let mut received = stream.next().await.unwrap().unwrap().to_vec();
                if let Some(first_seen) = first_seen {
                    let _ = first_seen.send(());
                }
                while let Some(chunk) = stream.next().await {
                    received.extend_from_slice(&chunk.unwrap());
                }
                String::from_utf8(received).unwrap()
            }
        }),
    );
    let upstream = serve(app).await;
    let proxy = Proxy::start(&upstream).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, Infallible>>(4);
    let request = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(reqwest::Body::wrap_stream(rx))
        .send();
    let sender = async move {
        tx.send(Ok(Bytes::from_static(b"first "))).await.unwrap();
        tokio::time::timeout(WAIT, saw_first)
            .await
            .expect("the upstream did not see the first chunk before the body ended")
            .unwrap();
        tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
    };
    let (response, ()) = tokio::join!(request, sender);
    assert_eq!(response.unwrap().text().await.unwrap(), "first second");
}

#[tokio::test]
async fn revoked_placeholder_is_refused() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let send = |placeholder: &Placeholder| {
        client()
            .post(proxy.url("/v1/messages"))
            .header("authorization", bearer(placeholder))
            .body(MESSAGE)
            .send()
    };
    let session = SessionId::new_v4();
    let (_, placeholder) = proxy.linked_in(session, LOCAL, "real-oauth-token");
    let (_, sibling) = proxy.linked_in(session, LOCAL, "sibling-token");
    assert_eq!(send(&placeholder).await.unwrap().status(), 200);
    assert!(proxy.registry.revoke(placeholder.id()));
    assert_eq!(send(&placeholder).await.unwrap().status(), 401);
    assert_eq!(send(&sibling).await.unwrap().status(), 200);
    assert_eq!(proxy.registry.revoke_session(session), 1);
    assert_eq!(
        send(&sibling).await.unwrap().status(),
        403,
        "with its session revoked, the address is unknown"
    );
    let sent: Vec<Option<String>> = fake
        .message_requests()
        .await
        .iter()
        .map(|request| header(request, "authorization").map(str::to_owned))
        .collect();
    assert_eq!(
        sent,
        [
            Some("Bearer real-oauth-token".to_owned()),
            Some("Bearer sibling-token".to_owned())
        ]
    );
}

#[tokio::test]
async fn unpointed_placeholder_is_refused() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let send = || {
        client()
            .post(proxy.url("/v1/messages"))
            .header("authorization", bearer(&placeholder))
            .body(MESSAGE)
            .send()
    };
    assert_eq!(send().await.unwrap().status(), 200);
    assert!(proxy.registry.unpoint(placeholder.id()));
    let idle = send().await.unwrap();
    assert_eq!(idle.status(), 403);
    let body: serde_json::Value = idle.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
    assert_eq!(
        body["error"]["message"],
        "No turn is running for this placeholder."
    );
    assert_eq!(fake.requests().await.len(), 1);
    assert!(proxy.registry.revoke(placeholder.id()));
    assert!(!proxy.registry.unpoint(placeholder.id()));
}

#[tokio::test]
async fn refuses_methods_outside_the_allowlist() {
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let reached = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&reached);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            *lock(&counter) += 1;
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).await.is_ok() {
                head.push(byte[0]);
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: message/http\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                head.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.write_all(&head).await;
        }
    });
    let proxy = Proxy::start(&upstream).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let track = reqwest::Method::from_bytes(b"TRACK").unwrap();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    for method in [reqwest::Method::TRACE, track, propfind] {
        let response = client()
            .request(method.clone(), proxy.url("/v1/messages"))
            .header("authorization", bearer(&placeholder))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method}");
        assert_eq!(
            response.headers()["allow"],
            "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"
        );
        let body = response.text().await.unwrap();
        assert!(!body.contains("real-oauth-token"), "{method}: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["type"], "invalid_request_error");
    }
    assert_eq!(*lock(&reached), 0, "a refused method reached the upstream");

    let echoed = client()
        .get(proxy.url("/v1/models"))
        .header("authorization", bearer(&placeholder))
        .send()
        .await
        .unwrap();
    assert_eq!(echoed.status(), 200);
    assert!(
        echoed
            .text()
            .await
            .unwrap()
            .to_ascii_lowercase()
            .starts_with("get /v1/models http/1.1\r\n")
    );
    assert_eq!(*lock(&reached), 1);
}

#[tokio::test]
async fn answers_hello_locally() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let _placeholder = proxy.linked(LOCAL, "real-oauth-token");
    let hello = client().head(proxy.url("/api/hello")).send().await.unwrap();
    assert_eq!(hello.status(), 200);
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn a_repointed_turn_keeps_its_credential_in_flight() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let first = MemberId::new_v4();
    let second = MemberId::new_v4();
    proxy.tokens.answer(first, Ok("first-token"));
    proxy.tokens.answer(second, Ok("second-token"));
    let (_, placeholder) = proxy.pointed(LOCAL, CredentialRef::Member(first));
    let (entered, release) = proxy.tokens.gate(first);
    let request = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send();
    let repoint = async {
        tokio::time::timeout(WAIT, entered.notified())
            .await
            .unwrap();
        proxy
            .registry
            .point(placeholder.id(), CredentialRef::Member(second))
            .unwrap();
        release.notify_one();
    };
    let (response, ()) = tokio::join!(request, repoint);
    assert_eq!(response.unwrap().status(), 200);
    let next = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(next.status(), 200);
    let sent: Vec<Option<String>> = fake
        .message_requests()
        .await
        .iter()
        .map(|request| header(request, "authorization").map(str::to_owned))
        .collect();
    assert_eq!(
        sent,
        [
            Some("Bearer first-token".to_owned()),
            Some("Bearer second-token".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_placeholder_revoked_while_its_token_is_fetched_is_refused() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let session = SessionId::new_v4();
    let (member, placeholder) = proxy.linked_in(session, LOCAL, "real-oauth-token");
    let _sibling = proxy.linked_in(session, LOCAL, "other-token");
    let (entered, release) = proxy.tokens.gate(member);
    let request = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send();
    let revoke = async {
        tokio::time::timeout(WAIT, entered.notified())
            .await
            .unwrap();
        assert!(proxy.registry.revoke(placeholder.id()));
        release.notify_one();
    };
    let (response, ()) = tokio::join!(request, revoke);
    assert_eq!(response.unwrap().status(), 401);
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn unlinked_and_unavailable_credentials_are_answered_without_forwarding() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start_with(&fake.uri(), Arc::new(Unconfigured)).await;
    let cases = [
        (Err(AuthError::NotLinked), 401),
        (Err(AuthError::RelinkRequired), 401),
        (Err(AuthError::RefreshInterrupted), 503),
    ];
    for (answer, status) in cases {
        let member = MemberId::new_v4();
        proxy.tokens.answer(member, answer);
        let (_, placeholder) = proxy.pointed(LOCAL, CredentialRef::Member(member));
        let response = client()
            .post(proxy.url("/v1/messages"))
            .header("authorization", bearer(&placeholder))
            .body(MESSAGE)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let (_, key) = proxy.pointed(LOCAL, CredentialRef::Community);
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("x-api-key", key.expose_secret())
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "No community API key is configured."
    );
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn refusals_never_echo_the_presented_token() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri()).await;
    let session = SessionId::new_v4();
    let unpointed = proxy
        .registry
        .mint(session, LOCAL, CredentialKind::Subscription)
        .unwrap();
    let (_, elsewhere) = proxy.linked(LOCAL_2, "real-oauth-token");
    let key = proxy.pointed_in(session, LOCAL, CredentialRef::Community);
    let marker = "agentd-sub-MARKER0123456789";
    let cases: Vec<(Vec<(&str, String)>, u16)> = vec![
        (vec![("authorization", format!("Bearer {marker}"))], 401),
        (vec![("authorization", format!("Basic {marker}"))], 401),
        (vec![("x-api-key", marker.to_owned())], 401),
        (vec![("authorization", bearer(&unpointed))], 403),
        (vec![("authorization", bearer(&elsewhere))], 401),
        (vec![("authorization", bearer(&key))], 401),
        (
            vec![
                ("authorization", format!("Bearer {marker}")),
                ("x-api-key", key.expose_secret().to_owned()),
            ],
            400,
        ),
        (
            vec![
                ("x-api-key", marker.to_owned()),
                ("x-api-key", key.expose_secret().to_owned()),
            ],
            400,
        ),
        (vec![], 401),
    ];
    for (headers, status) in cases {
        let mut request = client().post(proxy.url("/v1/messages")).body(MESSAGE);
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), status, "{headers:?}");
        let head = format!("{:?}", response.headers());
        let body = response.text().await.unwrap();
        for (_, value) in &headers {
            let token = value.rsplit(' ').next().unwrap();
            assert!(!body.contains(token), "{body}");
            assert!(!head.contains(token), "{head}");
        }
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["type"], "error");
    }
    assert!(fake.requests().await.is_empty());
}

#[tokio::test]
async fn hop_by_hop_headers_are_stripped_both_ways() {
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let seen = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let mut body = [0u8; 2];
        stream.read_exact(&mut body).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: x-up\r\nx-up: 1\r\n\
                  Keep-Alive: timeout=5\r\nProxy-Authenticate: Basic\r\nx-kept: 1\r\n\r\nok",
            )
            .await
            .unwrap();
        String::from_utf8(head).unwrap().to_ascii_lowercase()
    });
    let proxy = Proxy::start(&upstream).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let response = raw(
        proxy.addr,
        &format!(
            "POST /v1/messages HTTP/1.1\r\nHost: cred-proxy.internal\r\n\
             Authorization: {}\r\nConnection: close, x-drop\r\nx-drop: 1\r\n\
             Keep-Alive: timeout=5\r\nProxy-Authorization: Basic abc\r\nTE: trailers\r\n\
             Expect: 100-continue\r\nx-kept: 1\r\nContent-Length: 2\r\n\r\n{{}}",
            bearer(&placeholder)
        ),
    )
    .await;
    let head = seen.await.unwrap();
    assert!(
        head.contains("authorization: bearer real-oauth-token\r\n"),
        "{head}"
    );
    assert!(head.contains("x-kept: 1\r\n"), "{head}");
    assert!(head.contains("content-length: 2\r\n"), "{head}");
    for gone in [
        "x-drop",
        "keep-alive",
        "proxy-authorization",
        "te:",
        "expect",
        "cred-proxy.internal",
    ] {
        assert!(!head.contains(gone), "{gone} reached the upstream: {head}");
    }
    let response = response.to_ascii_lowercase();
    assert!(response.contains("http/1.1 200 ok\r\n"), "{response}");
    assert!(response.contains("x-kept: 1\r\n"), "{response}");
    for gone in ["x-up", "keep-alive", "proxy-authenticate"] {
        assert!(
            !response.contains(gone),
            "{gone} reached the client: {response}"
        );
    }
    assert!(response.ends_with("\r\n\r\nok"), "{response}");
}

#[tokio::test]
async fn observer_sees_status_and_usage_headers() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(429)
                .insert_header("anthropic-ratelimit-unified-status", "rejected")
                .insert_header("retry-after", "30")
                .insert_header("request-id", "req_1"),
        )
        .mount(&upstream)
        .await;
    let proxy = Proxy::start(&upstream.uri()).await;
    let (member, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["request-id"], "req_1");
    let observed = lock(&proxy.observed.0);
    assert_eq!(observed.len(), 1);
    let seen = &observed[0];
    assert_eq!(seen.status, 429);
    assert_eq!(seen.credential, CredentialRef::Member(member));
    assert_eq!(seen.usage["anthropic-ratelimit-unified-status"], "rejected");
    assert_eq!(seen.usage["retry-after"], "30");
    assert!(!seen.usage.contains_key("request-id"));
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_bad_gateway() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind((LOCAL, 0).into()).unwrap();
    let upstream = format!("http://{}", socket.local_addr().unwrap());
    let proxy = Proxy::start(&upstream).await;
    let (_, placeholder) = proxy.linked(LOCAL, "real-oauth-token");
    let response = client()
        .post(proxy.url("/v1/messages"))
        .header("authorization", bearer(&placeholder))
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(lock(&proxy.observed.0).is_empty());
}

#[tokio::test]
async fn a_request_without_its_peer_address_is_refused() {
    let fake = fake_anthropic().await;
    let registry = Registry::new();
    let router = CredProxy::new(
        &fake.uri(),
        registry.clone(),
        Arc::new(Tokens::default()),
        Arc::new(Unconfigured),
    )
    .unwrap()
    .into_router();
    let placeholder = registry
        .mint(SessionId::new_v4(), LOCAL, CredentialKind::ApiKey)
        .unwrap();
    registry
        .point(placeholder.id(), CredentialRef::Community)
        .unwrap();
    let upstream = serve(router).await;
    let response = client()
        .post(format!("{upstream}/v1/messages"))
        .header("x-api-key", placeholder.expose_secret())
        .body(MESSAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert!(fake.requests().await.is_empty());
}

#[test]
fn upstream_must_be_valid() {
    let err = CredProxy::new(
        "ftp://example.com",
        Registry::new(),
        Arc::new(Tokens::default()),
        Arc::new(Unconfigured),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid upstream URL: the scheme must be http or https"
    );
    let err = CredProxy::new(
        "http://api.anthropic.com",
        Registry::new(),
        Arc::new(Tokens::default()),
        Arc::new(Unconfigured),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid upstream URL: plain http is allowed only to a loopback IP address"
    );
    let proxy = CredProxy::new(
        cred_proxy::DEFAULT_UPSTREAM,
        Registry::new(),
        Arc::new(Tokens::default()),
        Arc::new(Unconfigured),
    )
    .unwrap();
    assert!(format!("{proxy:?}").contains("https://api.anthropic.com/"));
}
