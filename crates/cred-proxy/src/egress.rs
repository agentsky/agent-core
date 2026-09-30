//! [`EgressProxy`]: `CONNECT` tunnels from sandboxes to allowlisted hosts.

use std::collections::HashMap;
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
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;

use crate::allowlist::{ANTHROPIC_API_HOST, EgressPolicy, HostRule, normalize_host, parse_port};
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

/// The egress proxy's caps and timeouts, given to
/// [`EgressProxy::with_limits`]. [`Default`] gives the value each field
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressLimits {
    /// Open tunnels across all sessions, counting `CONNECT`s still being
    /// checked: 256. Past it, a `CONNECT` is refused with 503 before its
    /// host is looked up.
    pub max_tunnels: usize,
    /// The same for one session: 32. Past it, the session's `CONNECT`s
    /// are refused with 429.
    pub max_session_tunnels: usize,
    /// Host lookups running at once: 32. A lookup holds its place until the
    /// resolver returns, even after [`resolve_timeout`](Self::resolve_timeout)
    /// has refused its `CONNECT`, since the system resolver blocks a thread
    /// and can't be cancelled. A `CONNECT` that finds no place within
    /// `resolve_timeout` is refused with 503.
    pub max_lookups: usize,
    /// How long looking a host up may take, waiting for a place included:
    /// 5 seconds.
    pub resolve_timeout: Duration,
    /// How long the [`EgressExtension`] may take to answer: 2 seconds.
    /// Past it, the `CONNECT` is refused with 503.
    pub extension_timeout: Duration,
    /// How long a tunnel may go without a byte in either direction before
    /// the proxy closes it: 5 minutes.
    pub idle_timeout: Duration,
    /// How long a tunnel may stay open, busy or not: 1 hour.
    pub tunnel_lifetime: Duration,
}

impl Default for EgressLimits {
    fn default() -> Self {
        Self {
            max_tunnels: 256,
            max_session_tunnels: 32,
            max_lookups: 32,
            resolve_timeout: Duration::from_secs(5),
            extension_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(300),
            tunnel_lifetime: Duration::from_secs(3600),
        }
    }
}

/// Extra hosts for one session's sandbox, on top of the configured
/// allowlist: the extension point for hosts an agent's skills declare
/// (T25).
///
/// The rules it returns go through the same checks as configured ones:
/// [`ANTHROPIC_API_HOST`] and unreachable addresses stay denied.
#[async_trait]
pub trait EgressExtension: Send + Sync {
    /// The extra rules for `session`'s agent. On failure it returns none,
    /// so a lookup that fails denies rather than allows. The proxy waits
    /// [`EgressLimits::extension_timeout`] for it.
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
/// 3. Refuses [`ANTHROPIC_API_HOST`], then takes a tunnel place, refusing
///    the `CONNECT` if the session or the proxy has
///    [`max_session_tunnels`](EgressLimits::max_session_tunnels) or
///    [`max_tunnels`](EgressLimits::max_tunnels) already.
/// 4. Refuses any host and port no rule allows: the [`EgressPolicy`]'s,
///    then the [`EgressExtension`]'s for the session.
/// 5. Resolves the host, at most
///    [`max_lookups`](EgressLimits::max_lookups) at a time, and refuses it
///    if any address it resolves to is unreachable under the policy, so a
///    name rebound to the metadata address or a private network is
///    refused.
/// 6. Connects to the addresses it checked, never to the name, so a second
///    resolution can't change where the tunnel goes, answers 200, and
///    copies bytes both ways without looking at them (no TLS
///    interception).
///
/// A tunnel keeps its place until it closes: when either side does, after
/// [`idle_timeout`](EgressLimits::idle_timeout) without traffic, after
/// [`tunnel_lifetime`](EgressLimits::tunnel_lifetime), when its session
/// has no live placeholder left (as after [`Registry::revoke_session`]),
/// or when the proxy is dropped.
///
/// A refusal is a 403 (429 or 503 when a cap is reached, 502 if the host
/// can't be resolved or reached) with a one-line plain-text reason of fixed
/// text, and is logged with the session. Log lines name the host only once
/// it is known to be a valid host name, and never the request line.
pub struct EgressProxy {
    policy: EgressPolicy,
    network: Arc<dyn Network>,
    extension: Option<Arc<dyn EgressExtension>>,
    limits: EgressLimits,
    slots: Arc<Mutex<Slots>>,
    lookups: Arc<Semaphore>,
    closing: watch::Sender<()>,
}

impl fmt::Debug for EgressProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EgressProxy")
            .field("policy", &self.policy)
            .field("extension", &self.extension.is_some())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl EgressProxy {
    /// An egress proxy enforcing `policy` over the [`SystemNetwork`], with
    /// the default [`EgressLimits`].
    pub fn new(policy: EgressPolicy) -> Self {
        let limits = EgressLimits::default();
        Self {
            policy,
            network: Arc::new(SystemNetwork),
            extension: None,
            limits,
            slots: Arc::default(),
            lookups: Arc::new(Semaphore::new(limits.max_lookups)),
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

    /// Uses `limits` instead of the defaults.
    pub fn with_limits(mut self, limits: EgressLimits) -> Self {
        self.lookups = Arc::new(Semaphore::new(limits.max_lookups));
        self.limits = limits;
        self
    }

    /// The policy it enforces.
    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    /// Its caps and timeouts.
    pub fn limits(&self) -> EgressLimits {
        self.limits
    }

    /// Answers a `CONNECT` from `peer`.
    pub(crate) async fn connect(
        &self,
        registry: &Registry,
        peer: IpAddr,
        request: Request,
    ) -> Response {
        let Some((session, revoked)) = registry.watch_source(peer) else {
            tracing::warn!(%peer, reason = Why::UnknownSource.reason(), "the egress proxy refused a CONNECT");
            return Why::UnknownSource.into_response();
        };
        match self.open(session, revoked, request).await {
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

    async fn open(
        &self,
        session: SessionId,
        revoked: watch::Receiver<()>,
        mut request: Request,
    ) -> Result<Response, Refused> {
        if !matches!(request.version(), Version::HTTP_10 | Version::HTTP_11) {
            return Err(Refused::new(Why::Version, None));
        }
        let target = Target::of(request.uri()).map_err(|why| Refused::new(why, None))?;
        let refuse = |why| Refused::new(why, Some(target.clone()));
        if target.host == ANTHROPIC_API_HOST {
            return Err(refuse(Why::Anthropic));
        }
        let slot = self.take_slot(session).map_err(refuse)?;
        self.allowed(session, &target).await.map_err(refuse)?;
        let addresses = self.resolve(&target).await.map_err(refuse)?;
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
        if revoked.has_changed().is_err() {
            return Err(refuse(Why::UnknownSource));
        }
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
            Closers {
                proxy: self.closing.subscribe(),
                session: revoked,
            },
            self.limits,
            slot,
            target,
        ));
        Ok(StatusCode::OK.into_response())
    }

    /// A tunnel place for `session`, held until the `CONNECT` is refused
    /// or its tunnel closes.
    fn take_slot(&self, session: SessionId) -> Result<Slot, Why> {
        let mut slots = lock(&self.slots);
        let open = slots.sessions.get(&session).copied().unwrap_or(0);
        if open >= self.limits.max_session_tunnels {
            return Err(Why::SessionFull);
        }
        if slots.total >= self.limits.max_tunnels {
            return Err(Why::ProxyFull);
        }
        slots.sessions.insert(session, open + 1);
        slots.total += 1;
        Ok(Slot {
            slots: Arc::clone(&self.slots),
            session,
        })
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
            Some(extension) => {
                tokio::time::timeout(self.limits.extension_timeout, extension.rules(session))
                    .await
                    .map_err(|_| Why::Extension)?
            }
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

    /// The addresses `target`'s host resolves to, once a lookup place is
    /// free. The lookup runs in a task of its own that keeps the place
    /// until the resolver returns, so lookups the timeout gave up on still
    /// count.
    async fn resolve(&self, target: &Target) -> Result<Vec<IpAddr>, Why> {
        let deadline = Instant::now() + self.limits.resolve_timeout;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.lookups).acquire_owned())
            .await
            .map_err(|_| Why::Busy)?
            .map_err(|_| Why::Busy)?;
        let network = Arc::clone(&self.network);
        let (host, port) = (target.host.clone(), target.port);
        let lookup = tokio::spawn(async move {
            let _permit = permit;
            network.resolve(&host, port).await
        });
        match tokio::time::timeout_at(deadline, lookup).await {
            Ok(Ok(Ok(mut addresses))) if !addresses.is_empty() => {
                addresses.dedup();
                Ok(addresses)
            }
            _ => Err(Why::Resolve),
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
    SessionFull,
    ProxyFull,
    Extension,
    NotAllowed,
    Port,
    Busy,
    Address(&'static str),
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
            Self::SessionFull => "session tunnel cap reached",
            Self::ProxyFull => "tunnel cap reached",
            Self::Extension => "allowlist extension timed out",
            Self::NotAllowed => "host not allowed",
            Self::Port => "port not allowed",
            Self::Busy => "lookup cap reached",
            Self::Address(why) => why,
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
            Self::SessionFull => (
                StatusCode::TOO_MANY_REQUESTS,
                "This sandbox has too many open tunnels.",
            ),
            Self::ProxyFull => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The egress proxy has too many open tunnels.",
            ),
            Self::Extension => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The session's allowlist could not be loaded.",
            ),
            Self::NotAllowed => (forbidden, "The host is not in the egress allowlist."),
            Self::Port => (forbidden, "The port is not allowed for this host."),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The egress proxy is busy looking up hosts.",
            ),
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tunnel places taken, in all and per session.
#[derive(Default)]
struct Slots {
    total: usize,
    sessions: HashMap<SessionId, usize>,
}

/// One tunnel place, given back when dropped.
struct Slot {
    slots: Arc<Mutex<Slots>>,
    session: SessionId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = lock(&self.slots);
        slots.total -= 1;
        if let Some(open) = slots.sessions.get_mut(&self.session) {
            *open -= 1;
            if *open == 0 {
                slots.sessions.remove(&self.session);
            }
        }
    }
}

/// What closes a tunnel from outside: receivers whose senders are dropped
/// when the proxy is, and when the session has no live placeholder left.
struct Closers {
    proxy: watch::Receiver<()>,
    session: watch::Receiver<()>,
}

/// Runs a tunnel until it closes, within `limits`, holding `slot`.
async fn tunnel(
    upgrade: OnUpgrade,
    upstream: TcpStream,
    mut closers: Closers,
    limits: EgressLimits,
    slot: Slot,
    target: Target,
) {
    let outcome = tokio::select! {
        outcome = relay(upgrade, upstream, limits.idle_timeout) => outcome,
        () = tokio::time::sleep(limits.tunnel_lifetime) => "lifetime reached",
        _ = closers.proxy.changed() => "proxy stopped",
        _ = closers.session.changed() => "session revoked",
    };
    tracing::debug!(
        session = %slot.session,
        host = target.host.as_str(),
        port = target.port,
        outcome,
        "an egress tunnel ended"
    );
}

/// Copies bytes between the upgraded client connection and `upstream`
/// until either closes or neither sends for `idle`.
async fn relay(upgrade: OnUpgrade, mut upstream: TcpStream, idle: Duration) -> &'static str {
    let Ok(client) = upgrade.await else {
        return "never upgraded";
    };
    let activity = Activity::default();
    let mut client = Watched {
        inner: TokioIo::new(client),
        activity: activity.clone(),
    };
    tokio::select! {
        copied = tokio::io::copy_bidirectional(&mut client, &mut upstream) => match copied {
            Ok(_) => "closed",
            Err(_) => "failed",
        },
        () = activity.idle(idle) => "idle",
    }
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
        *lock(&self.0) = Instant::now();
    }

    /// Completes once nothing has moved for `idle`.
    async fn idle(&self, idle: Duration) {
        loop {
            let deadline = *lock(&self.0) + idle;
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
            Why::SessionFull,
            Why::ProxyFull,
            Why::Extension,
            Why::NotAllowed,
            Why::Port,
            Why::Busy,
            Why::Address("private address"),
            Why::Resolve,
            Why::Connect,
        ];
        for why in all {
            assert!(!why.reason().is_empty());
            let (status, line) = why.answer();
            assert!(line.ends_with('.') && !line.contains('\n'), "{line}");
            let expected = match why {
                Why::Resolve | Why::Connect => StatusCode::BAD_GATEWAY,
                Why::SessionFull => StatusCode::TOO_MANY_REQUESTS,
                Why::ProxyFull | Why::Extension | Why::Busy => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::FORBIDDEN,
            };
            assert_eq!(status, expected, "{why:?}");
            let response = why.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
        }
        assert_eq!(Why::Address("private address").reason(), "private address");
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
