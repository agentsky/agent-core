//! The egress proxy's rules, each checked through a real listener and raw
//! `CONNECT` requests. Resolution and outbound connections go through a
//! fake [`Network`], so a public-looking address can lead to a local echo
//! server and no test touches the real network.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use core_types::{CredentialKind, CredentialRef, MemberId, SessionId};
use cred_proxy::{
    CredProxy, EGRESS_ENV, EgressExtension, EgressLimits, EgressPolicy, EgressProxy, FixedKey,
    HostRule, Network, Registry,
};
use secrecy::{ExposeSecret as _, SecretString};
use testkit::{Logs, fake_anthropic};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinHandle;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const LOCAL_2: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
const STRANGER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));
const LOCAL_3: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 6));
const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3));
const PUBLIC_2: IpAddr = IpAddr::V4(Ipv4Addr::new(140, 82, 112, 4));
const WAIT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A resolver with fixed answers, and outbound connections sent to local
/// listeners by the address the proxy asked for.
#[derive(Default)]
struct FakeNetwork {
    answers: Mutex<HashMap<String, Vec<IpAddr>>>,
    routes: Mutex<HashMap<SocketAddr, SocketAddr>>,
    resolved: Mutex<Vec<String>>,
    dialed: Mutex<Vec<SocketAddr>>,
}

impl FakeNetwork {
    fn answer(&self, host: &str, addresses: &[IpAddr]) {
        lock(&self.answers).insert(host.to_owned(), addresses.to_vec());
    }

    fn route(&self, from: SocketAddr, to: SocketAddr) {
        lock(&self.routes).insert(from, to);
    }

    fn resolved(&self) -> Vec<String> {
        lock(&self.resolved).clone()
    }

    fn dialed(&self) -> Vec<SocketAddr> {
        lock(&self.dialed).clone()
    }
}

#[async_trait]
impl Network for FakeNetwork {
    async fn resolve(&self, host: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
        lock(&self.resolved).push(host.to_owned());
        if host == HANGS {
            std::future::pending::<()>().await;
        }
        lock(&self.answers)
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"))
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        lock(&self.dialed).push(addr);
        let route = lock(&self.routes).get(&addr).copied();
        match route {
            Some(to) => TcpStream::connect(to).await,
            None => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        }
    }
}

/// A host whose lookup never returns, as when the resolver hangs.
const HANGS: &str = "hangs.example.org";

/// Extra rules for one session only.
struct ForSession(SessionId, Vec<HostRule>);

#[async_trait]
impl EgressExtension for ForSession {
    async fn rules(&self, session: SessionId) -> Vec<HostRule> {
        if session == self.0 {
            self.1.clone()
        } else {
            Vec::new()
        }
    }
}

struct NoTokens;

#[async_trait]
impl TokenSource for NoTokens {
    async fn access_token(&self, _member: MemberId) -> Result<SecretString, AuthError> {
        Ok(SecretString::from("real-oauth-token"))
    }
}

fn rules(texts: &[&str]) -> Vec<HostRule> {
    texts.iter().map(|text| text.parse().unwrap()).collect()
}

fn policy(allow: &[&str]) -> EgressPolicy {
    EgressPolicy::new(rules(allow), vec!["172.30.0.0/24".parse().unwrap()])
}

/// The default limits, changed by `change`.
fn limits(change: impl FnOnce(&mut EgressLimits)) -> EgressLimits {
    let mut limits = EgressLimits::default();
    change(&mut limits);
    limits
}

/// A running proxy with egress, on a free local port.
struct Proxy {
    addr: SocketAddr,
    registry: Registry,
    network: Arc<FakeNetwork>,
    server: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: &str, egress: impl FnOnce(EgressProxy) -> EgressProxy) -> Self {
        Self::start_with(
            upstream,
            policy(&["git.example.com", "*.example.org", "alt.example.com:8443"]),
            egress,
        )
        .await
    }

    async fn start_with(
        upstream: &str,
        policy: EgressPolicy,
        egress: impl FnOnce(EgressProxy) -> EgressProxy,
    ) -> Self {
        let registry = Registry::new();
        let network = Arc::new(FakeNetwork::default());
        let egress =
            egress(EgressProxy::new(policy).with_network(Arc::clone(&network) as Arc<dyn Network>));
        let router = CredProxy::new(
            upstream,
            registry.clone(),
            Arc::new(NoTokens),
            Arc::new(FixedKey::new(SecretString::from("community"))),
        )
        .unwrap()
        .with_egress(egress)
        .into_router();
        let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
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
            network,
            server,
        }
    }

    /// Registers a session for the sandbox at `ip`.
    fn sandbox(&self, ip: IpAddr) -> SessionId {
        let session = SessionId::new_v4();
        self.registry
            .mint(session, ip, CredentialKind::Subscription)
            .unwrap();
        session
    }

    /// Sends `request` from `ip` and returns the stream and the response
    /// head.
    async fn send_from(&self, ip: IpAddr, request: &str) -> (TcpStream, String) {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::new(ip, 0)).unwrap();
        let mut stream = socket.connect(self.addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let read = tokio::time::timeout(WAIT, stream.read(&mut byte))
                .await
                .expect("no response head")
                .unwrap();
            assert_eq!(
                read,
                1,
                "closed mid-head: {}",
                String::from_utf8_lossy(&head)
            );
            head.push(byte[0]);
        }
        (stream, String::from_utf8(head).unwrap())
    }

    /// Sends `CONNECT target` from the sandbox at [`LOCAL`] and returns the
    /// stream and the response head.
    async fn connect(&self, target: &str) -> (TcpStream, String) {
        self.connect_from(LOCAL, target).await
    }

    async fn connect_from(&self, ip: IpAddr, target: &str) -> (TcpStream, String) {
        self.send_from(
            ip,
            &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
        )
        .await
    }

    /// Sends `CONNECT target` and returns the status line and the body of
    /// a refusal.
    async fn refused(&self, ip: IpAddr, target: &str) -> (String, String) {
        let (stream, head) = self.connect_from(ip, target).await;
        (status_line(&head), body(stream, &head).await)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn status_line(head: &str) -> String {
    head.lines().next().unwrap_or_default().to_owned()
}

/// Reads the body a head announced with `content-length`.
async fn body(mut stream: TcpStream, head: &str) -> String {
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("a refusal has a content-length");
    let mut body = vec![0u8; length];
    tokio::time::timeout(WAIT, stream.read_exact(&mut body))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(body).unwrap()
}

/// A local server that echoes whatever it receives, and the number of
/// connections it accepted.
async fn echo_server() -> (SocketAddr, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            *lock(&counter) += 1;
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    (addr, accepted)
}

async fn echoes(stream: &mut TcpStream, message: &[u8]) {
    stream.write_all(message).await.unwrap();
    let mut back = vec![0u8; message.len()];
    tokio::time::timeout(WAIT, stream.read_exact(&mut back))
        .await
        .expect("no echo")
        .unwrap();
    assert_eq!(back, message);
}

async fn closes(stream: &mut TcpStream) {
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, stream.read_to_end(&mut rest))
        .await
        .expect("the tunnel stayed open");
    assert!(read.is_err() || rest.is_empty(), "{read:?} {rest:?}");
}

#[tokio::test]
async fn tunnels_to_an_allowed_host() {
    let (echo, accepted) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, head) = proxy
        .send_from(
            LOCAL,
            "CONNECT Git.Example.COM.:443 HTTP/1.1\r\nHost: evil.example:443\r\n\r\n\
             early bytes",
        )
        .await;
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(!lower.contains("content-length"), "{head}");
    assert!(!lower.contains("transfer-encoding"), "{head}");
    let mut early = [0u8; 11];
    tokio::time::timeout(WAIT, stream.read_exact(&mut early))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&early, b"early bytes");
    echoes(&mut stream, b"\x16\x03\x01 not inspected").await;
    assert_eq!(proxy.network.resolved(), ["git.example.com"]);
    assert_eq!(proxy.network.dialed(), [SocketAddr::new(PUBLIC, 443)]);
    assert_eq!(*lock(&accepted), 1);

    proxy.network.answer("deep.a.example.org", &[PUBLIC]);
    let (mut stream, head) = proxy.connect("deep.a.example.org:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"wildcard").await;

    proxy.network.answer("alt.example.com", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 8443), echo);
    let (mut stream, head) = proxy.connect("alt.example.com:8443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"named port").await;
}

#[tokio::test]
async fn refuses_a_host_not_on_the_allowlist() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("other.example", &[PUBLIC]);
    for target in [
        "other.example:443",
        "example.org:443",
        "git.example.com.evil.example:443",
    ] {
        let (status, body) = proxy.refused(LOCAL, target).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
        assert_eq!(
            body, "The host is not in the egress allowlist.\n",
            "{target}"
        );
    }
    let (stream, head) = proxy
        .send_from(
            LOCAL,
            "CONNECT other.example:443 HTTP/1.1\r\nHost: git.example.com:443\r\n\r\n",
        )
        .await;
    assert_eq!(status_line(&head), "HTTP/1.1 403 Forbidden");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/plain"),
        "{head}"
    );
    assert_eq!(
        body(stream, &head).await,
        "The host is not in the egress allowlist.\n"
    );
    assert!(proxy.network.resolved().is_empty());
    assert!(proxy.network.dialed().is_empty());
}

#[tokio::test]
async fn always_denies_api_anthropic_com() {
    let policy = policy(&["*.anthropic.com"]);
    let proxy = Proxy::start_with("http://127.0.0.1:9", policy, |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("api.anthropic.com", &[PUBLIC]);
    for target in ["api.anthropic.com:443", "API.Anthropic.COM.:443"] {
        let (status, body) = proxy.refused(LOCAL, target).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
        assert_eq!(
            body,
            "api.anthropic.com is always denied; the CLI reaches it through \
             ANTHROPIC_BASE_URL.\n"
        );
    }
    assert!(proxy.network.resolved().is_empty());
    assert!(proxy.network.dialed().is_empty());
}

#[tokio::test]
async fn refuses_a_rebind_to_the_metadata_address() {
    let (echo, accepted) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    for answer in [
        vec!["169.254.169.254"],
        vec!["::ffff:169.254.169.254"],
        vec!["fd00:ec2::254"],
        vec!["140.82.112.3", "169.254.169.254"],
        vec!["127.0.0.1"],
        vec!["::1"],
        vec!["10.1.2.3"],
        vec!["192.168.1.10"],
        vec!["10.20.9.9"],
        vec!["172.30.0.2"],
        vec!["100.100.100.200"],
        vec!["192.0.0.192"],
        vec!["168.63.129.16"],
        vec!["fd20:ce::254"],
        vec!["fd00:c1::a9fe:a9fe"],
        vec!["fd12:3456::9"],
        vec!["64:ff9b::a9fe:a9fe"],
    ] {
        let addresses: Vec<IpAddr> = answer.iter().map(|a| a.parse().unwrap()).collect();
        for &ip in &addresses {
            proxy.network.route(SocketAddr::new(ip, 443), echo);
        }
        proxy.network.answer("git.example.com", &addresses);
        let (status, body) = proxy.refused(LOCAL, "git.example.com:443").await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{answer:?}");
        assert_eq!(
            body, "The host resolves to an address sandboxes may not reach.\n",
            "{answer:?}"
        );
    }
    assert!(proxy.network.dialed().is_empty());
    assert_eq!(*lock(&accepted), 0);
}

#[tokio::test]
async fn extension_rules_never_reach_private_addresses() {
    let (echo, accepted) = echo_server().await;
    let skilled = SessionId::new_v4();
    let extension = Arc::new(ForSession(skilled, rules(&["evil-skill.example.net:22"])));
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_extension(extension)
    })
    .await;
    proxy
        .registry
        .mint(skilled, LOCAL, CredentialKind::Subscription)
        .unwrap();
    let private: IpAddr = "10.20.9.9".parse().unwrap();
    proxy.network.answer("evil-skill.example.net", &[private]);
    proxy.network.route(SocketAddr::new(private, 22), echo);
    let (status, body) = proxy.refused(LOCAL, "evil-skill.example.net:22").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        body,
        "The host resolves to an address sandboxes may not reach.\n"
    );
    assert!(proxy.network.dialed().is_empty());
    assert_eq!(*lock(&accepted), 0);
}

#[tokio::test]
async fn refuses_a_port_other_than_443_unless_a_rule_names_it() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    proxy.network.answer("alt.example.com", &[PUBLIC]);
    for target in [
        "git.example.com:22",
        "git.example.com:80",
        "git.example.com:8443",
        "alt.example.com:443",
        "x.example.org:4443",
    ] {
        let (status, body) = proxy.refused(LOCAL, target).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
        assert_eq!(body, "The port is not allowed for this host.\n", "{target}");
    }
    assert!(proxy.network.resolved().is_empty());
}

#[tokio::test]
async fn refuses_ip_address_and_malformed_targets() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    for target in [
        "140.82.112.3:443",
        "169.254.169.254:443",
        "[::1]:443",
        "[fd00:ec2::254]:443",
        "127.1:443",
        "2130706433:443",
    ] {
        let (status, body) = proxy.refused(LOCAL, target).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
        assert_eq!(
            body, "CONNECT to an IP address is not allowed; name the host.\n",
            "{target}"
        );
    }
    for target in ["git.example.com", "git.example.com:0", "localhost:443"] {
        let (status, body) = proxy.refused(LOCAL, target).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
        assert_eq!(body, "The CONNECT target must be host:port.\n", "{target}");
    }
    for request in [
        "CONNECT user@git.example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n",
        "CONNECT http://git.example.com:443/ HTTP/1.1\r\nHost: x\r\n\r\n",
        "CONNECT /git.example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n",
        "CONNECT git.example.com:443:443 HTTP/1.1\r\nHost: x\r\n\r\n",
        "CONNECT git.example.com:99999 HTTP/1.1\r\nHost: x\r\n\r\n",
    ] {
        let (_, head) = proxy.send_from(LOCAL, request).await;
        let status = status_line(&head);
        assert!(
            status == "HTTP/1.1 403 Forbidden" || status == "HTTP/1.1 400 Bad Request",
            "{request:?}: {head}"
        );
    }
    assert!(proxy.network.resolved().is_empty());
    assert!(proxy.network.dialed().is_empty());
}

#[tokio::test]
async fn refuses_connect_from_an_unknown_source() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    let (status, body) = proxy.refused(STRANGER, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    assert_eq!(body, "This address has no sandbox session.\n");
    assert!(proxy.network.resolved().is_empty());
}

#[tokio::test]
async fn refuses_an_absolute_form_request_without_reaching_the_upstream() {
    let fake = fake_anthropic().await;
    let proxy = Proxy::start(&fake.uri(), |egress| egress).await;
    let session = SessionId::new_v4();
    let placeholder = proxy
        .registry
        .mint(session, LOCAL, CredentialKind::Subscription)
        .unwrap();
    proxy
        .registry
        .point(placeholder.id(), CredentialRef::Member(MemberId::new_v4()))
        .unwrap();
    proxy.network.answer("git.example.com", &[PUBLIC]);
    for request in [
        "GET http://git.example.com/ HTTP/1.1\r\nHost: git.example.com\r\n\r\n".to_owned(),
        format!(
            "POST http://git.example.com/v1/messages HTTP/1.1\r\nHost: git.example.com\r\n\
             Authorization: Bearer {}\r\nContent-Length: 2\r\n\r\n{{}}",
            placeholder.expose_secret()
        ),
        "GET https://git.example.com:443/ HTTP/1.1\r\nHost: git.example.com\r\n\r\n".to_owned(),
    ] {
        let (_, head) = proxy.send_from(LOCAL, &request).await;
        assert_eq!(status_line(&head), "HTTP/1.1 403 Forbidden", "{request}");
    }
    assert!(fake.requests().await.is_empty());
    assert!(proxy.network.resolved().is_empty());
    assert!(proxy.network.dialed().is_empty());
}

#[tokio::test]
async fn extension_rules_apply_to_their_session_only() {
    let (echo, _) = echo_server().await;
    let skilled = SessionId::new_v4();
    let extension = Arc::new(ForSession(
        skilled,
        rules(&["skill.example.net", "*.anthropic.com"]),
    ));
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_extension(extension)
    })
    .await;
    proxy
        .registry
        .mint(skilled, LOCAL, CredentialKind::Subscription)
        .unwrap();
    proxy.sandbox(LOCAL_2);
    proxy.network.answer("skill.example.net", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, head) = proxy.connect("skill.example.net:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"skill host").await;
    let (status, body) = proxy.refused(LOCAL_2, "skill.example.net:443").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    assert_eq!(body, "The host is not in the egress allowlist.\n");
    let (status, _) = proxy.refused(LOCAL, "api.anthropic.com:443").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
}

#[tokio::test]
async fn unresolvable_and_unreachable_hosts_are_bad_gateways() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    let (status, body) = proxy.refused(LOCAL, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
    assert_eq!(body, "The host could not be resolved.\n");
    proxy.network.answer("git.example.com", &[]);
    let (status, _) = proxy.refused(LOCAL, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");

    proxy.network.answer("git.example.com", &[PUBLIC]);
    let (status, body) = proxy.refused(LOCAL, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
    assert_eq!(body, "The host could not be reached.\n");

    proxy.network.answer("git.example.com", &[PUBLIC, PUBLIC_2]);
    proxy.network.route(SocketAddr::new(PUBLIC_2, 443), echo);
    let (mut stream, head) = proxy.connect("git.example.com:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"second address").await;
    assert_eq!(
        proxy.network.dialed().last(),
        Some(&SocketAddr::new(PUBLIC_2, 443))
    );
}

#[tokio::test]
async fn tunnels_close_when_idle() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| l.idle_timeout = Duration::from_millis(300)))
    })
    .await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, head) = proxy.connect("git.example.com:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        echoes(&mut stream, b"still here").await;
    }
    closes(&mut stream).await;
}

#[tokio::test]
async fn tunnels_close_when_the_proxy_is_dropped() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, head) = proxy.connect("git.example.com:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"open").await;
    drop(proxy);
    closes(&mut stream).await;
}

/// Opens a tunnel from `ip` to `git.example.com`, which the fake routes to
/// `echo`, and checks that it works.
async fn open_tunnel(proxy: &Proxy, ip: IpAddr, echo: SocketAddr) -> TcpStream {
    proxy.network.answer("git.example.com", &[PUBLIC]);
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, head) = proxy.connect_from(ip, "git.example.com:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    echoes(&mut stream, b"open").await;
    stream
}

#[tokio::test]
async fn caps_open_tunnels_per_session_and_in_all() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| {
            l.max_session_tunnels = 2;
            l.max_tunnels = 3;
        }))
    })
    .await;
    proxy.sandbox(LOCAL);
    proxy.sandbox(LOCAL_2);
    proxy.sandbox(LOCAL_3);
    for _ in 0..5 {
        let (status, _) = proxy.refused(LOCAL, "other.example:443").await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden");
    }
    let first = open_tunnel(&proxy, LOCAL, echo).await;
    let _second = open_tunnel(&proxy, LOCAL, echo).await;
    let (status, body) = proxy.refused(LOCAL, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 429 Too Many Requests");
    assert_eq!(body, "This sandbox has too many open tunnels.\n");
    let _third = open_tunnel(&proxy, LOCAL_2, echo).await;
    let (status, body) = proxy.refused(LOCAL_3, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(body, "The egress proxy has too many open tunnels.\n");
    assert_eq!(proxy.network.resolved().len(), 3);

    drop(first);
    let reopened = tokio::time::timeout(WAIT, async {
        loop {
            let (stream, head) = proxy.connect_from(LOCAL, "git.example.com:443").await;
            if head.starts_with("HTTP/1.1 200") {
                return stream;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let mut reopened = reopened.expect("the closed tunnel's place was never given back");
    echoes(&mut reopened, b"reopened").await;
}

#[tokio::test]
async fn tunnels_close_at_their_lifetime_even_when_busy() {
    let (echo, _) = echo_server().await;
    let lifetime = Duration::from_secs(2);
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| l.tunnel_lifetime = lifetime))
    })
    .await;
    proxy.sandbox(LOCAL);
    let opened = tokio::time::Instant::now();
    let mut stream = open_tunnel(&proxy, LOCAL, echo).await;
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        echoes(&mut stream, b"busy").await;
    }
    closes(&mut stream).await;
    assert!(opened.elapsed() >= lifetime, "{:?}", opened.elapsed());
}

#[tokio::test]
async fn revoking_a_session_closes_its_tunnels() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    let revoked = proxy.sandbox(LOCAL);
    proxy.sandbox(LOCAL_2);
    let mut gone = open_tunnel(&proxy, LOCAL, echo).await;
    let mut kept = open_tunnel(&proxy, LOCAL_2, echo).await;
    assert_eq!(proxy.registry.revoke_session(revoked), 1);
    closes(&mut gone).await;
    echoes(&mut kept, b"other session").await;

    proxy.sandbox(LOCAL_2);
    closes(&mut kept).await;
}

#[tokio::test]
async fn lookups_are_capped_even_after_they_time_out() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| {
            l.max_lookups = 1;
            l.resolve_timeout = Duration::from_millis(200);
        }))
    })
    .await;
    proxy.sandbox(LOCAL);
    proxy.sandbox(LOCAL_2);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    let (status, body) = proxy.refused(LOCAL, &format!("{HANGS}:443")).await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
    assert_eq!(body, "The host could not be resolved.\n");
    let (status, body) = proxy.refused(LOCAL_2, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(body, "The egress proxy is busy looking up hosts.\n");
    assert_eq!(proxy.network.resolved(), [HANGS]);
}

#[tokio::test]
async fn hung_lookups_hold_their_own_sessions_tunnel_places() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| {
            l.max_session_tunnels = 1;
            l.max_lookups = 4;
            l.resolve_timeout = Duration::from_millis(200);
        }))
    })
    .await;
    proxy.sandbox(LOCAL);
    proxy.sandbox(LOCAL_2);
    let (status, _) = proxy.refused(LOCAL, &format!("{HANGS}:443")).await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
    for _ in 0..3 {
        let (status, body) = proxy.refused(LOCAL, &format!("{HANGS}:443")).await;
        assert_eq!(status, "HTTP/1.1 429 Too Many Requests");
        assert_eq!(body, "This sandbox has too many open tunnels.\n");
    }
    open_tunnel(&proxy, LOCAL_2, echo).await;
    assert_eq!(proxy.network.resolved(), [HANGS, "git.example.com"]);
}

#[tokio::test]
async fn one_session_runs_a_quarter_of_the_lookups() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress.with_limits(limits(|l| {
            l.max_lookups = 8;
            l.resolve_timeout = Duration::from_millis(200);
        }))
    })
    .await;
    proxy.sandbox(LOCAL);
    proxy.sandbox(LOCAL_2);
    for _ in 0..2 {
        let (status, _) = proxy.refused(LOCAL, &format!("{HANGS}:443")).await;
        assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
    }
    let (status, body) = proxy.refused(LOCAL, "git.example.com:443").await;
    assert_eq!(status, "HTTP/1.1 429 Too Many Requests");
    assert_eq!(body, "This sandbox has too many host lookups running.\n");
    open_tunnel(&proxy, LOCAL_2, echo).await;
    assert_eq!(proxy.network.resolved(), [HANGS, HANGS, "git.example.com"]);
}

/// An extension that never answers.
struct Stuck;

#[async_trait]
impl EgressExtension for Stuck {
    async fn rules(&self, _session: SessionId) -> Vec<HostRule> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn an_extension_that_does_not_answer_is_refused() {
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| {
        egress
            .with_extension(Arc::new(Stuck))
            .with_limits(limits(|l| l.extension_timeout = Duration::from_millis(200)))
    })
    .await;
    proxy.sandbox(LOCAL);
    let (status, body) = proxy.refused(LOCAL, "skill.example.net:443").await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(body, "The session's allowlist could not be loaded.\n");
    assert!(proxy.network.resolved().is_empty());
    open_tunnel(&proxy, LOCAL, echo).await;
}

#[tokio::test]
async fn refuses_connect_over_http2() {
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    proxy.sandbox(LOCAL);
    proxy.network.answer("git.example.com", &[PUBLIC]);
    let stream = TcpStream::connect(proxy.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    .unwrap();
    tokio::spawn(connection);
    let request = hyper::Request::connect("git.example.com:443")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 403);
    let body = axum::body::to_bytes(axum::body::Body::new(response.into_body()), 1024)
        .await
        .unwrap();
    assert_eq!(body, "CONNECT is served over HTTP/1.1 only.\n");
    assert!(proxy.network.resolved().is_empty());
}

#[test]
fn the_sandbox_environment_points_every_proxy_variable_at_the_proxy() {
    let env: HashMap<&str, &str> = EGRESS_ENV.into_iter().collect();
    for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        assert_eq!(env[name], "http://cred-proxy.internal:8080", "{name}");
    }
    for name in ["NO_PROXY", "no_proxy"] {
        assert_eq!(env[name], "cred-proxy.internal,agentctl.internal", "{name}");
    }
    assert_eq!(env.len(), 6);
}

#[tokio::test]
async fn logs_carry_the_session_and_rule_and_never_the_host_or_request() {
    const SANDBOX: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 5));
    const UNKNOWN: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 4));
    let logs = Logs::global();
    let (echo, _) = echo_server().await;
    let proxy = Proxy::start("http://127.0.0.1:9", |egress| egress).await;
    let session = proxy.sandbox(SANDBOX);
    proxy
        .network
        .answer("allowed-secret-token.example.org", &[PUBLIC]);
    proxy.network.answer(
        "metadata-secret-token.example.org",
        &["169.254.169.254".parse().unwrap()],
    );
    proxy.network.route(SocketAddr::new(PUBLIC, 443), echo);
    let (mut stream, _) = proxy
        .send_from(
            SANDBOX,
            "CONNECT allowed-secret-token.example.org:443 HTTP/1.1\r\nHost: x\r\n\
             Proxy-Authorization: Basic cHJveHktc2VjcmV0\r\n\r\n",
        )
        .await;
    echoes(&mut stream, b"hello").await;
    proxy
        .refused(SANDBOX, "denied-secret-token.other.example:443")
        .await;
    proxy
        .refused(SANDBOX, "metadata-secret-token.example.org:443")
        .await;
    proxy
        .refused(UNKNOWN, "unknown-secret-token.example.com:443")
        .await;
    proxy
        .send_from(
            SANDBOX,
            "GET http://user:url-secret@git.example.com/p?token=query-secret HTTP/1.1\r\n\
             Host: git.example.com\r\n\r\n",
        )
        .await;
    proxy
        .send_from(
            SANDBOX,
            "CONNECT secret-looking_host.example:443 HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let logs = logs.snapshot();
    let mine = |line: &&str| line.contains("peer=127.0.0.5") || line.contains("peer=127.0.0.4");
    let tunnel_lines = |message: &str| {
        logs.lines()
            .filter(|line| line.contains(message) && line.contains(&format!("session={session}")))
            .count()
    };
    for message in ["opened an egress tunnel", "an egress tunnel ended"] {
        assert_eq!(tunnel_lines(message), 1, "{message}\n{logs}");
    }
    assert!(
        logs.lines()
            .any(|line| line.contains("opened an egress tunnel")
                && line.contains(&format!("session={session}"))
                && line.contains("rule=*.example.org")
                && line.contains("port=443")
                && line.contains(&format!("address={PUBLIC}:443"))),
        "{logs}"
    );
    let refusals: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("the egress proxy refused a CONNECT"))
        .filter(mine)
        .collect();
    assert_eq!(refusals.len(), 4, "{logs}");
    assert!(
        refusals[0].contains(&format!("session={session}"))
            && refusals[0].contains("port=443")
            && refusals[0].contains("host not allowed")
            && !refusals[0].contains("rule="),
        "{logs}"
    );
    assert!(
        refusals[1].contains("address=\"169.254.169.254\"")
            && refusals[1].contains("rule=\"*.example.org\"")
            && refusals[1].contains("link-local or cloud metadata address"),
        "{logs}"
    );
    assert!(
        refusals[2].contains("unknown source address") && !refusals[2].contains("session="),
        "{logs}"
    );
    assert!(
        refusals[3].contains("invalid target") && !refusals[3].contains("port="),
        "{logs}"
    );
    for secret in [
        "cHJveHktc2VjcmV0",
        "url-secret",
        "query-secret",
        "secret-looking_host",
        "secret-token",
    ] {
        logs.assert_lacks(secret);
    }
}
