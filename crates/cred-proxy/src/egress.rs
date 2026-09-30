//! [`EgressProxy`]: `CONNECT` tunnels from sandboxes to allowlisted hosts.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use core_types::SessionId;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::allowlist::{
    ANTHROPIC_API_HOST, EgressPolicy, HostRule, Unreachable, normalize_host, parse_port,
};
use crate::proxy::CONNECT_TIMEOUT;
use crate::registry::Registry;

/// The proxy's address as sandboxes know it: agentd's proxy listener,
/// under its alias on the sandbox network.
pub const PROXY_URL: &str = "http://cred-proxy.internal:8080";

/// Hosts a sandbox reaches directly rather than through [`PROXY_URL`]:
/// the credential proxy itself (`ANTHROPIC_BASE_URL`) and the agentctl API.
pub const NO_PROXY: &str = "cred-proxy.internal,agentctl.internal";

/// The environment that sends a sandbox's HTTPS through the egress proxy,
/// in both the upper- and lowercase spellings tools read (curl, and so
/// git, reads only `http_proxy` in lowercase). `HTTP_PROXY` is set too so
/// plain HTTP gets the proxy's refusal instead of a timeout; the proxy
/// offers no plain HTTP egress.
pub const EGRESS_ENV: [(&str, &str); 6] = [
    ("HTTPS_PROXY", PROXY_URL),
    ("https_proxy", PROXY_URL),
    ("HTTP_PROXY", PROXY_URL),
    ("http_proxy", PROXY_URL),
    ("NO_PROXY", NO_PROXY),
    ("no_proxy", NO_PROXY),
];

/// How long resolving a host may take.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a tunnel may go without a byte in either direction before the
/// proxy closes it, unless [`EgressProxy::with_idle_timeout`] says
/// otherwise.
pub const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Extra hosts for one session's sandbox, on top of the configured
/// allowlist: the extension point for hosts an agent's skills declare
/// (T25).
///
/// The rules it returns go through the same checks as configured ones:
/// [`ANTHROPIC_API_HOST`] and unreachable addresses stay denied.
#[async_trait]
pub trait EgressExtension: Send + Sync {
    /// The extra rules for `session`'s agent. On failure it returns none,
    /// so a lookup that fails denies rather than allows.
    async fn rules(&self, session: SessionId) -> Vec<HostRule>;
}

/// How the egress proxy resolves hosts and connects to them. The default,
/// [`SystemNetwork`], uses the system resolver and TCP; tests supply their
/// own.
#[async_trait]
pub trait Network: Send + Sync {
    /// The addresses `host` resolves to.
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>>;
    /// A connection to `addr`, an address [`resolve`](Self::resolve)
    /// returned and the policy allowed.
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream>;
}

/// The system resolver (`getaddrinfo`) and plain TCP.
///
/// Hosts are resolved as fully qualified names, with a trailing dot, so the
/// resolver's search domains never apply: with Kubernetes' `ndots:5`,
/// `github.com` would otherwise be tried as
/// `github.com.<namespace>.svc.cluster.local` first.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemNetwork;

#[async_trait]
impl Network for SystemNetwork {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        let fqdn = format!("{}.", host.trim_end_matches('.'));
        Ok(tokio::net::lookup_host((fqdn.as_str(), port))
            .await?
            .map(|addr| addr.ip())
            .collect())
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

/// The egress proxy: served on the proxy listener next to the reverse
/// proxy, by [`CredProxy::with_egress`](crate::CredProxy::with_egress). It
/// answers `CONNECT` only.
///
/// For each `CONNECT` it:
///
/// 1. Identifies the sandbox's session by the connection's peer address,
///    through the [`Registry`], and refuses an address no live placeholder
///    is bound to.
/// 2. Takes the target from the request line only, never from `Host`: an
///    authority-form `host:port` over HTTP/1, with no user info. IP
///    addresses are refused, so every tunnel goes to a named host.
/// 3. Refuses [`ANTHROPIC_API_HOST`], then any host and port no rule
///    allows: the [`EgressPolicy`]'s, then the [`EgressExtension`]'s for
///    the session.
/// 4. Resolves the host and refuses it if any address it resolves to is
///    unreachable under the policy, so a name rebound to the metadata
///    address or a private network is refused.
/// 5. Connects to the addresses it checked, never to the name, so a second
///    resolution can't change where the tunnel goes, answers 200, and
///    copies bytes both ways without looking at them (no TLS
///    interception). A tunnel closes when either side does, after
///    [`TUNNEL_IDLE_TIMEOUT`] without traffic, or when the proxy is
///    dropped.
///
/// A refusal is a 403 (502 if the host can't be resolved or reached) with
/// a one-line plain-text reason of fixed text, and is logged with the
/// session. Log lines name the host only once it is known to be a valid
/// host name, and never the request line.
pub struct EgressProxy {
    policy: EgressPolicy,
    network: Arc<dyn Network>,
    extension: Option<Arc<dyn EgressExtension>>,
    idle_timeout: Duration,
    closing: watch::Sender<()>,
}

impl fmt::Debug for EgressProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EgressProxy")
            .field("policy", &self.policy)
            .field("extension", &self.extension.is_some())
            .field("idle_timeout", &self.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl EgressProxy {
    /// An egress proxy enforcing `policy` over the [`SystemNetwork`].
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            policy,
            network: Arc::new(SystemNetwork),
            extension: None,
            idle_timeout: TUNNEL_IDLE_TIMEOUT,
            closing: watch::Sender::new(()),
        }
    }

    /// Resolves and connects through `network` instead.
    pub fn with_network(mut self, network: Arc<dyn Network>) -> Self {
        self.network = network;
        self
    }

    /// Adds `extension`'s rules to each session's allowlist.
    pub fn with_extension(mut self, extension: Arc<dyn EgressExtension>) -> Self {
        self.extension = Some(extension);
        self
    }

    /// Closes tunnels after `timeout` without traffic instead of
    /// [`TUNNEL_IDLE_TIMEOUT`].
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Answers a `CONNECT` from `peer`.
    pub(crate) async fn connect(
        &self,
        registry: &Registry,
        peer: IpAddr,
        request: Request,
    ) -> Response {
        let Some(session) = registry.session_at(peer) else {
            tracing::warn!(%peer, reason = Why::UnknownSource.reason(), "the egress proxy refused a CONNECT");
            return Why::UnknownSource.into_response();
        };
        match self.open(session, request).await {
            Ok(response) => response,
            Err(Refused {
                why,
                target,
                address,
            }) => {
                tracing::warn!(
                    %peer,
                    %session,
                    host = target.as_ref().map(|t| t.host.as_str()),
                    port = target.as_ref().map(|t| t.port),
                    address = address.map(|a| a.to_string()),
                    reason = why.reason(),
                    "the egress proxy refused a CONNECT"
                );
                why.into_response()
            }
        }
    }

    async fn open(&self, session: SessionId, mut request: Request) -> Result<Response, Refused> {
        if !matches!(request.version(), Version::HTTP_10 | Version::HTTP_11) {
            return Err(Refused::new(Why::Version, None));
        }
        let target = Target::of(request.uri()).map_err(|why| Refused::new(why, None))?;
        let refuse = |why| Refused::new(why, Some(target.clone()));
        if target.host == ANTHROPIC_API_HOST {
            return Err(refuse(Why::Anthropic));
        }
        self.allowed(session, &target).await.map_err(refuse)?;
        let resolved = tokio::time::timeout(
            RESOLVE_TIMEOUT,
            self.network.resolve(&target.host, target.port),
        )
        .await;
        let mut addresses = match resolved {
            Ok(Ok(addresses)) if !addresses.is_empty() => addresses,
            _ => return Err(refuse(Why::Resolve)),
        };
        addresses.dedup();
        if let Some((address, why)) = addresses
            .iter()
            .find_map(|&ip| self.policy.unreachable(ip).map(|why| (ip, why)))
        {
            return Err(Refused {
                address: Some(address),
                ..refuse(Why::Address(why))
            });
        }
        let (upstream, address) = self
            .dial(&addresses, target.port)
            .await
            .ok_or_else(|| refuse(Why::Connect))?;
        tracing::debug!(
            %session,
            host = target.host.as_str(),
            port = target.port,
            %address,
            "opened an egress tunnel"
        );
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(tunnel(
            upgrade,
            upstream,
            self.closing.subscribe(),
            self.idle_timeout,
            session,
            target,
        ));
        Ok(StatusCode::OK.into_response())
    }

    /// Whether a rule allows `target`: `Err(Port)` when a rule names the
    /// host on another port. The extension is asked only when no
    /// configured rule allows it.
    async fn allowed(&self, session: SessionId, target: &Target) -> Result<(), Why> {
        let allows = |rule: &HostRule| rule.allows(&target.host, target.port);
        if self.policy.rules().iter().any(allows) {
            return Ok(());
        }
        let extra = match &self.extension {
            Some(extension) => extension.rules(session).await,
            None => Vec::new(),
        };
        let rules = || self.policy.rules().iter().chain(&extra);
        if extra.iter().any(allows) {
            Ok(())
        } else if rules().any(|rule| rule.names(&target.host)) {
            Err(Why::Port)
        } else {
            Err(Why::NotAllowed)
        }
    }

    /// A connection to the first of `addresses` that answers on `port`,
    /// tried in order within [`CONNECT_TIMEOUT`] for all of them, so a long
    /// answer of silent addresses can't hold the request.
    async fn dial(&self, addresses: &[IpAddr], port: u16) -> Option<(TcpStream, SocketAddr)> {
        let attempts = async {
            for &ip in addresses {
                let address = SocketAddr::new(ip, port);
                match self.network.connect(address).await {
                    Ok(stream) => return Some((stream, address)),
                    Err(err) => {
                        tracing::debug!(%address, error = %err, "an egress connection failed");
                    }
                }
            }
            None
        };
        tokio::time::timeout(CONNECT_TIMEOUT, attempts)
            .await
            .unwrap_or_else(|_| {
                tracing::debug!(port, "egress connections timed out");
                None
            })
    }
}

/// A `CONNECT` target: a valid, normalized host name and a port.
#[derive(Debug, Clone)]
struct Target {
    host: String,
    port: u16,
}

impl Target {
    /// The target of a request line whose target is `uri`.
    fn of(uri: &Uri) -> Result<Self, Why> {
        if uri.scheme().is_some() || uri.path_and_query().is_some() {
            return Err(Why::Target);
        }
        let authority = uri.authority().ok_or(Why::Target)?.as_str();
        if authority.contains('@') {
            return Err(Why::Target);
        }
        let (host, port) = authority.rsplit_once(':').ok_or(Why::Target)?;
        let port = parse_port(port).ok_or(Why::Target)?;
        if host.starts_with('[') || host.parse::<Ipv4Addr>().is_ok() {
            return Err(Why::IpLiteral);
        }
        let host = normalize_host(host).ok_or_else(|| {
            let numeric = host
                .trim_end_matches('.')
                .rsplit('.')
                .next()
                .and_then(|last| last.bytes().next())
                .is_some_and(|first| first.is_ascii_digit());
            if numeric { Why::IpLiteral } else { Why::Target }
        })?;
        Ok(Self { host, port })
    }
}

/// A refused `CONNECT`, with what is known about it for the log line.
struct Refused {
    why: Why,
    target: Option<Target>,
    address: Option<IpAddr>,
}

impl Refused {
    fn new(why: Why, target: Option<Target>) -> Self {
        Self {
            why,
            target,
            address: None,
        }
    }
}

/// Why the egress proxy refused a `CONNECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    UnknownSource,
    Version,
    Target,
    IpLiteral,
    Anthropic,
    NotAllowed,
    Port,
    Address(Unreachable),
    Resolve,
    Connect,
}

impl Why {
    /// A short name for the log line.
    fn reason(self) -> &'static str {
        match self {
            Self::UnknownSource => "unknown source address",
            Self::Version => "CONNECT over HTTP/2",
            Self::Target => "invalid target",
            Self::IpLiteral => "IP address target",
            Self::Anthropic => "api.anthropic.com",
            Self::NotAllowed => "host not allowed",
            Self::Port => "port not allowed",
            Self::Address(why) => why.describe(),
            Self::Resolve => "resolution failed",
            Self::Connect => "connection failed",
        }
    }

    /// The status and the one line the client gets.
    fn answer(self) -> (StatusCode, &'static str) {
        let forbidden = StatusCode::FORBIDDEN;
        match self {
            Self::UnknownSource => (forbidden, "This address has no sandbox session."),
            Self::Version => (forbidden, "CONNECT is served over HTTP/1.1 only."),
            Self::Target => (forbidden, "The CONNECT target must be host:port."),
            Self::IpLiteral => (
                forbidden,
                "CONNECT to an IP address is not allowed; name the host.",
            ),
            Self::Anthropic => (
                forbidden,
                "api.anthropic.com is always denied; the CLI reaches it through ANTHROPIC_BASE_URL.",
            ),
            Self::NotAllowed => (forbidden, "The host is not in the egress allowlist."),
            Self::Port => (forbidden, "The port is not allowed for this host."),
            Self::Address(_) => (
                forbidden,
                "The host resolves to an address sandboxes may not reach.",
            ),
            Self::Resolve => (StatusCode::BAD_GATEWAY, "The host could not be resolved."),
            Self::Connect => (StatusCode::BAD_GATEWAY, "The host could not be reached."),
        }
    }
}

impl IntoResponse for Why {
    fn into_response(self) -> Response {
        let (status, line) = self.answer();
        (
            status,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            Body::from(format!("{line}\n")),
        )
            .into_response()
    }
}

/// Copies bytes between the upgraded client connection and `upstream`
/// until either closes, neither sends for `idle`, or `closing` ends.
async fn tunnel(
    upgrade: OnUpgrade,
    mut upstream: TcpStream,
    mut closing: watch::Receiver<()>,
    idle: Duration,
    session: SessionId,
    target: Target,
) {
    let client = match upgrade.await {
        Ok(client) => client,
        Err(err) => {
            tracing::debug!(%session, error = %err, "a CONNECT was never upgraded");
            return;
        }
    };
    let activity = Activity::default();
    let mut client = Watched {
        inner: TokioIo::new(client),
        activity: activity.clone(),
    };
    let outcome = tokio::select! {
        copied = tokio::io::copy_bidirectional(&mut client, &mut upstream) => match copied {
            Ok(_) => "closed",
            Err(_) => "failed",
        },
        () = activity.idle(idle) => "idle",
        _ = closing.changed() => "proxy stopped",
    };
    tracing::debug!(
        %session,
        host = target.host.as_str(),
        port = target.port,
        outcome,
        "an egress tunnel ended"
    );
}

/// When a tunnel last moved a byte.
#[derive(Clone)]
struct Activity(Arc<Mutex<Instant>>);

impl Default for Activity {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }
}

impl Activity {
    fn touch(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// Completes once nothing has moved for `idle`.
    async fn idle(&self, idle: Duration) {
        loop {
            let deadline = *self.0.lock().unwrap_or_else(PoisonError::into_inner) + idle;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// A connection that records in [`Activity`] every read and write that
/// moves bytes.
struct Watched<S> {
    inner: S,
    activity: Activity,
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.activity.touch();
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if matches!(poll, Poll::Ready(Ok(n)) if n > 0) {
            this.activity.touch();
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(text: &str) -> Result<(String, u16), Why> {
        let uri: Uri = text.parse().map_err(|_| Why::Target)?;
        Target::of(&uri).map(|t| (t.host, t.port))
    }

    #[tokio::test]
    async fn the_system_network_connects_without_nagle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = SystemNetwork
            .connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        assert!(stream.nodelay().unwrap());
    }

    #[test]
    fn targets_are_authority_form_host_and_port() {
        assert_eq!(
            target("GitHub.COM.:443"),
            Ok(("github.com".to_owned(), 443))
        );
        assert_eq!(
            target("git.example.com:0022"),
            Ok(("git.example.com".to_owned(), 22))
        );
        for text in [
            "github.com",
            "github.com:",
            "github.com:0",
            "github.com:99999",
            "user@github.com:443",
            "user:pw@github.com:443",
            "https://github.com:443",
            "https://github.com:443/",
            "/github.com:443",
            "*.github.com:443",
            "github..com:443",
            "localhost:443",
        ] {
            assert_eq!(target(text), Err(Why::Target), "{text}");
        }
    }

    #[test]
    fn ip_address_targets_are_refused() {
        for text in [
            "1.1.1.1:443",
            "169.254.169.254:443",
            "[::1]:443",
            "[fd00:ec2::254]:443",
            "[::ffff:169.254.169.254]:443",
            "127.1:443",
            "2130706433:443",
            "0x7f.1:443",
            "1.1.1.1.:443",
        ] {
            assert_eq!(target(text), Err(Why::IpLiteral), "{text}");
        }
    }

    #[test]
    fn every_refusal_is_one_line_of_fixed_text() {
        let all = [
            Why::UnknownSource,
            Why::Version,
            Why::Target,
            Why::IpLiteral,
            Why::Anthropic,
            Why::NotAllowed,
            Why::Port,
            Why::Address(Unreachable::Own),
            Why::Resolve,
            Why::Connect,
        ];
        for why in all {
            assert!(!why.reason().is_empty());
            let (status, line) = why.answer();
            assert!(line.ends_with('.') && !line.contains('\n'), "{line}");
            let expected = if matches!(why, Why::Resolve | Why::Connect) {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::FORBIDDEN
            };
            assert_eq!(status, expected, "{why:?}");
            let response = why.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
        }
        assert_eq!(
            Why::Address(Unreachable::Private).reason(),
            "private address"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn activity_goes_idle_only_after_a_quiet_interval() {
        let activity = Activity::default();
        let idle = Duration::from_secs(10);
        let waiting = tokio::spawn({
            let activity = activity.clone();
            async move { activity.idle(idle).await }
        });
        tokio::time::sleep(Duration::from_secs(8)).await;
        activity.touch();
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert!(!waiting.is_finished());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(waiting.is_finished());
    }
}
