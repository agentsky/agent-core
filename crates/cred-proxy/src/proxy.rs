//! [`CredProxy`]: the reverse proxy in front of the one configured upstream.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use auth::{AuthError, TokenSource};
use axum::Router;
use axum::body::{Body, HttpBody as _};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{
    ALLOW, AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, EXPECT, HOST,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use core_types::{CredentialKind, CredentialRef, SessionId};
use reqwest::Url;
use secrecy::{ExposeSecret as _, SecretString};

use crate::egress::EgressProxy;
use crate::hooks::{CommunityKey, CommunityKeyError, Observation, ProxyObserver, usage_headers};
use crate::registry::{Denial, Grant, Registry};

/// The upstream the proxy forwards to unless configured otherwise.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// How long connecting to the upstream may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The header an API key travels in.
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

/// The methods the proxy forwards. Every other one is refused before any
/// credential is looked up: `TRACE` would echo the real credential back,
/// `CONNECT` is the egress proxy's, and an extension method has no known
/// meaning to the upstream.
const FORWARDED_METHODS: [Method; 7] = [
    Method::GET,
    Method::HEAD,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::OPTIONS,
];

/// The `Allow` header of a refused method's answer: the forwarded methods.
const ALLOWED: &str = "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS";

/// Headers that describe one connection, never forwarded in either
/// direction (RFC 9110 section 7.6.1), next to any header `Connection`
/// names.
const HOP_BY_HOP: [HeaderName; 9] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// The error returned by [`CredProxy::new`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyError {
    /// The upstream URL can't be used.
    #[error("invalid upstream URL: {0}")]
    Upstream(&'static str),
    /// The HTTP client couldn't be built.
    #[error("building the HTTP client failed: {0}")]
    Client(#[source] reqwest::Error),
}

/// The credential-swapping reverse proxy, served on agentd's proxy listener
/// through [`into_router`](Self::into_router).
///
/// For each request it:
///
/// 1. Identifies the sandbox by the connection's peer address, which the
///    listener supplies as [`ConnectInfo<SocketAddr>`], and refuses an
///    address no live placeholder is bound to.
/// 2. Answers `HEAD /api/hello`, the CLI's connectivity check, with 200
///    itself.
/// 3. Refuses every method but `GET`, `HEAD`, `POST`, `PUT`, `PATCH`,
///    `DELETE` and `OPTIONS`, so `TRACE` can't echo the real credential,
///    and refuses absolute-form requests, so nothing the client sends can
///    name another host. `CONNECT` goes to the [`EgressProxy`] given to
///    [`with_egress`](Self::with_egress), and is refused without one. The `Host` header is
///    dropped; the upstream's own is sent.
/// 4. Takes the placeholder from `Authorization: Bearer` or `x-api-key`,
///    exactly one of them, and refuses it unless it is live, bound to that
///    address, of the kind its header carries, and pointed at a credential.
/// 5. Replaces that header's value with the real credential, a member's
///    access token from [`TokenSource`] or the community key from
///    [`CommunityKey`], and forwards the request to the same path and query
///    on the upstream. The body and every other header pass through
///    untouched, except hop-by-hop headers and `Expect`, which are dropped.
/// 6. Streams the response back as it arrives, without hop-by-hop headers,
///    after telling the [`ProxyObserver`], if any.
///
/// A refusal is an Anthropic-style JSON error with a fixed message. No
/// response or log line contains a placeholder or a credential.
pub struct CredProxy {
    upstream: Upstream,
    http: reqwest::Client,
    registry: Registry,
    tokens: Arc<dyn TokenSource>,
    community: Arc<dyn CommunityKey>,
    observer: Option<Arc<dyn ProxyObserver>>,
    egress: Option<EgressProxy>,
}

impl fmt::Debug for CredProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredProxy")
            .field("upstream", &self.upstream.base.as_str())
            .field("registry", &self.registry)
            .field("egress", &self.egress)
            .finish_non_exhaustive()
    }
}

impl CredProxy {
    /// A proxy forwarding to `upstream`, usually [`DEFAULT_UPSTREAM`],
    /// checking placeholders against `registry`.
    ///
    /// # Errors
    ///
    /// [`ProxyError::Upstream`] unless `upstream` is an `http` or `https`
    /// URL with a host and no credentials, query or fragment, and
    /// [`ProxyError::Client`] if the HTTP client can't be built.
    pub fn new(
        upstream: &str,
        registry: Registry,
        tokens: Arc<dyn TokenSource>,
        community: Arc<dyn CommunityKey>,
    ) -> Result<Self, ProxyError> {
        let upstream = Upstream::parse(upstream)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|err| ProxyError::Client(err.without_url()))?;
        Ok(Self {
            upstream,
            http,
            registry,
            tokens,
            community,
            observer: None,
            egress: None,
        })
    }

    /// Tells `observer` about every forwarded request.
    pub fn with_observer(mut self, observer: Arc<dyn ProxyObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Serves `CONNECT` with `egress`, on the same listener. Without it,
    /// `CONNECT` is refused like any other method outside the allowlist.
    pub fn with_egress(mut self, egress: EgressProxy) -> Self {
        self.egress = Some(egress);
        self
    }

    /// The routes to serve on the proxy listener: every method and path.
    /// Requests must carry the peer's [`ConnectInfo<SocketAddr>`], as
    /// agentd's listeners and
    /// [`into_make_service_with_connect_info`](Router::into_make_service_with_connect_info)
    /// insert it; one without it is refused.
    pub fn into_router(self) -> Router {
        Router::new().fallback(handle).with_state(Arc::new(self))
    }

    async fn forward(&self, peer: IpAddr, request: Request) -> Result<Response, Rejected> {
        let refuse = |refusal, session| Rejected { refusal, session };
        if self.registry.session_at(peer).is_none() {
            return Err(refuse(Refusal::UnknownSource, None));
        }
        if !FORWARDED_METHODS.contains(request.method()) {
            return Err(refuse(Refusal::Method, None));
        }
        let uri = request.uri();
        if request.version() < Version::HTTP_2
            && (uri.scheme().is_some() || uri.authority().is_some())
        {
            return Err(refuse(Refusal::AbsoluteForm, None));
        }
        if request.method() == Method::HEAD && uri.path() == "/api/hello" {
            return Ok(StatusCode::OK.into_response());
        }
        let (kind, token) = presented(request.headers()).map_err(|r| refuse(r, None))?;
        let grant = self
            .registry
            .authorize(token, peer, kind)
            .map_err(|denial| match denial {
                Denial::Unknown => refuse(Refusal::UnknownPlaceholder, None),
                Denial::OtherSource(session) => refuse(Refusal::OtherSource, Some(session)),
                Denial::WrongKind(session) => refuse(Refusal::WrongKind, Some(session)),
                Denial::NotPointed(session) => refuse(Refusal::NotPointed, Some(session)),
            })?;
        let session = Some(grant.session);
        let path = uri.path_and_query().map_or("/", |pq| pq.as_str());
        let url = self
            .upstream
            .url(path)
            .ok_or_else(|| refuse(Refusal::BadPath, session))?;
        let secret = self
            .credential(grant)
            .await
            .map_err(|r| refuse(r, session))?;
        if !self.registry.is_live(grant.id, peer) {
            return Err(refuse(Refusal::UnknownPlaceholder, session));
        }
        let value = credential_value(kind, &secret).map_err(|r| refuse(r, session))?;
        drop(secret);

        let (parts, body) = request.into_parts();
        let mut headers = parts.headers;
        if headers.contains_key(TRANSFER_ENCODING) {
            headers.remove(CONTENT_LENGTH);
        }
        strip_hop_by_hop(&mut headers);
        headers.remove(HOST);
        headers.remove(EXPECT);
        headers.insert(credential_header(kind), value);
        let mut upstream = self
            .http
            .request(parts.method.clone(), url)
            .headers(headers);
        if !body.is_end_stream() {
            upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }
        let response = upstream.send().await.map_err(|err| {
            tracing::warn!(
                session = %grant.session,
                error = %err.without_url(),
                "the upstream request failed"
            );
            refuse(Refusal::Upstream, session)
        })?;
        let status = response.status();
        tracing::debug!(
            session = %grant.session,
            method = %parts.method,
            path = parts.uri.path(),
            status = status.as_u16(),
            "forwarded a request"
        );
        if let Some(observer) = &self.observer {
            observer.observe(&Observation {
                session: grant.session,
                credential: grant.credential,
                status,
                usage: usage_headers(response.headers()),
            });
        }
        let (mut parts, body) = axum::http::Response::<reqwest::Body>::from(response).into_parts();
        strip_hop_by_hop(&mut parts.headers);
        Ok(Response::from_parts(parts, Body::new(body)))
    }

    async fn credential(&self, grant: Grant) -> Result<SecretString, Refusal> {
        match grant.credential {
            CredentialRef::Member(member) => {
                self.tokens
                    .access_token(member)
                    .await
                    .map_err(|err| match err {
                        AuthError::NotLinked | AuthError::RelinkRequired => Refusal::NotLinked,
                        other => {
                            tracing::warn!(
                                session = %grant.session,
                                error = %other,
                                "no access token for the pointed member"
                            );
                            Refusal::CredentialUnavailable
                        }
                    })
            }
            CredentialRef::Community => self.community.api_key().await.map_err(|err| match err {
                CommunityKeyError::NotConfigured => Refusal::NoCommunityKey,
                other => {
                    tracing::warn!(
                        session = %grant.session,
                        error = %other,
                        "no community API key"
                    );
                    Refusal::CredentialUnavailable
                }
            }),
        }
    }
}

async fn handle(State(proxy): State<Arc<CredProxy>>, request: Request) -> Response {
    let Some(ConnectInfo(peer)) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied()
    else {
        tracing::error!("a request reached the credential proxy without its peer address");
        return Refusal::NoPeer.into_response();
    };
    let peer = peer.ip().to_canonical();
    if request.method() == Method::CONNECT
        && let Some(egress) = &proxy.egress
    {
        return egress.connect(&proxy.registry, peer, request).await;
    }
    match proxy.forward(peer, request).await {
        Ok(response) => response,
        Err(Rejected { refusal, session }) => {
            tracing::warn!(
                %peer,
                session = session.map(|s| s.to_string()),
                reason = refusal.reason(),
                "the credential proxy refused a request"
            );
            refusal.into_response()
        }
    }
}

/// The configured upstream: an `http` or `https` origin and an optional
/// base path.
struct Upstream {
    base: Url,
}

impl Upstream {
    fn parse(text: &str) -> Result<Self, ProxyError> {
        let base = Url::parse(text).map_err(|_| ProxyError::Upstream("not a URL"))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(ProxyError::Upstream("the scheme must be http or https"));
        }
        if base.host().is_none() {
            return Err(ProxyError::Upstream("there is no host"));
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(ProxyError::Upstream("credentials are not allowed"));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(ProxyError::Upstream("a query or fragment is not allowed"));
        }
        Ok(Self { base })
    }

    fn prefix(&self) -> &str {
        self.base.path().trim_end_matches('/')
    }

    /// The upstream URL for a request's origin-form `path_and_query`, or
    /// `None` if the result would leave the upstream's origin or base path.
    fn url(&self, path_and_query: &str) -> Option<Url> {
        if !path_and_query.starts_with('/') {
            return None;
        }
        let base = self.base.as_str().trim_end_matches('/');
        let url = Url::parse(&format!("{base}{path_and_query}")).ok()?;
        let prefix = self.prefix();
        let inside = url.path() == prefix
            || url
                .path()
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'));
        (url.origin() == self.base.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && inside)
            .then_some(url)
    }
}

/// The header a credential of `kind` travels in.
fn credential_header(kind: CredentialKind) -> HeaderName {
    match kind {
        CredentialKind::Subscription => AUTHORIZATION,
        CredentialKind::ApiKey => X_API_KEY,
    }
}

/// The credential header a request carries, and the token in it.
fn presented(headers: &HeaderMap) -> Result<(CredentialKind, &str), Refusal> {
    match (
        single(headers, &AUTHORIZATION)?,
        single(headers, &X_API_KEY)?,
    ) {
        (Some(_), Some(_)) => Err(Refusal::BothCredentials),
        (None, None) => Err(Refusal::MissingCredential),
        (Some(value), None) => {
            let text = value.to_str().map_err(|_| Refusal::NotBearer)?;
            let (scheme, token) = text.split_once(' ').ok_or(Refusal::NotBearer)?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(Refusal::NotBearer);
            }
            Ok((CredentialKind::Subscription, token.trim()))
        }
        (None, Some(value)) => {
            let token = value.to_str().map_err(|_| Refusal::UnknownPlaceholder)?;
            Ok((CredentialKind::ApiKey, token.trim()))
        }
    }
}

/// The one value of `name`, if any.
fn single<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<Option<&'a HeaderValue>, Refusal> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(Refusal::RepeatedCredential);
    }
    Ok(first)
}

/// The header value carrying `secret` as a credential of `kind`, marked
/// sensitive so HTTP/2 never indexes it.
fn credential_value(kind: CredentialKind, secret: &SecretString) -> Result<HeaderValue, Refusal> {
    let value = match kind {
        CredentialKind::Subscription => {
            HeaderValue::try_from(format!("Bearer {}", secret.expose_secret()))
        }
        CredentialKind::ApiKey => HeaderValue::from_str(secret.expose_secret()),
    };
    let mut value = value.map_err(|_| Refusal::BadCredential)?;
    value.set_sensitive(true);
    Ok(value)
}

/// Removes the hop-by-hop headers and every header `Connection` names.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named.iter().chain(&HOP_BY_HOP) {
        headers.remove(name);
    }
}

/// A refused request, with the session its placeholder belongs to when
/// that is known, for the log line.
struct Rejected {
    refusal: Refusal,
    session: Option<SessionId>,
}

/// Why the proxy refused a request. Every message is fixed text, so a
/// refusal can never echo what the client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    NoPeer,
    UnknownSource,
    Method,
    AbsoluteForm,
    MissingCredential,
    BothCredentials,
    RepeatedCredential,
    NotBearer,
    UnknownPlaceholder,
    OtherSource,
    WrongKind,
    NotPointed,
    BadPath,
    NotLinked,
    NoCommunityKey,
    CredentialUnavailable,
    BadCredential,
    Upstream,
}

impl Refusal {
    /// A short name for the log line.
    fn reason(self) -> &'static str {
        match self {
            Self::NoPeer => "no peer address",
            Self::UnknownSource => "unknown source address",
            Self::Method => "method not forwarded",
            Self::AbsoluteForm => "absolute-form target",
            Self::MissingCredential => "no credential header",
            Self::BothCredentials => "both credential headers",
            Self::RepeatedCredential => "repeated credential header",
            Self::NotBearer => "Authorization is not Bearer",
            Self::UnknownPlaceholder => "unknown or revoked placeholder",
            Self::OtherSource => "placeholder bound to another address",
            Self::WrongKind => "placeholder in the wrong header",
            Self::NotPointed => "placeholder not pointed",
            Self::BadPath => "path outside the upstream",
            Self::NotLinked => "member not linked or relink required",
            Self::NoCommunityKey => "no community key configured",
            Self::CredentialUnavailable => "credential unavailable",
            Self::BadCredential => "credential is not a valid header value",
            Self::Upstream => "upstream unreachable",
        }
    }

    /// The HTTP status, the Anthropic error type and the message the
    /// client gets. An address mismatch reads exactly like an unknown
    /// placeholder, so the answer doesn't confirm that a token exists.
    fn answer(self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::NoPeer => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "The credential proxy could not identify the connection.",
            ),
            Self::UnknownSource => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "This address has no credential placeholder.",
            ),
            Self::Method => (
                StatusCode::METHOD_NOT_ALLOWED,
                "invalid_request_error",
                "Only GET, HEAD, POST, PUT, PATCH, DELETE and OPTIONS are forwarded.",
            ),
            Self::AbsoluteForm => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "Requests for other hosts are not forwarded.",
            ),
            Self::MissingCredential => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Send the placeholder in Authorization: Bearer or x-api-key.",
            ),
            Self::BothCredentials => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Send only one of Authorization and x-api-key.",
            ),
            Self::RepeatedCredential => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The credential header appears more than once.",
            ),
            Self::NotBearer => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Authorization must carry a Bearer placeholder.",
            ),
            Self::UnknownPlaceholder | Self::OtherSource => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "The placeholder is not valid here.",
            ),
            Self::WrongKind => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "The placeholder was sent in the wrong header for its kind.",
            ),
            Self::NotPointed => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "No turn is running for this placeholder.",
            ),
            Self::BadPath => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The request path is not valid.",
            ),
            Self::NotLinked => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "The requester's Claude login is missing or expired; run /agent login.",
            ),
            Self::NoCommunityKey => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "No community API key is configured.",
            ),
            Self::CredentialUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "The credential could not be obtained; try again.",
            ),
            Self::BadCredential => (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "The stored credential can't be sent.",
            ),
            Self::Upstream => (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "The upstream could not be reached.",
            ),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (status, kind, message) = self.answer();
        let body = serde_json::json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        });
        let mut response = (
            status,
            [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            body.to_string(),
        )
            .into_response();
        if self == Self::Method {
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static(ALLOWED));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_urls_keep_the_origin_and_base_path() {
        let root = Upstream::parse("https://api.anthropic.com").unwrap();
        assert_eq!(
            root.url("/v1/messages?beta=true").unwrap().as_str(),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(
            root.url("//evil.example/v1").unwrap().host_str(),
            Some("api.anthropic.com")
        );
        assert_eq!(
            root.url("/@evil.example/").unwrap().host_str(),
            Some("api.anthropic.com")
        );
        assert!(root.url("evil.example/v1").is_none());

        let nested = Upstream::parse("http://127.0.0.1:9/anthropic/").unwrap();
        assert_eq!(
            nested.url("/v1/messages").unwrap().as_str(),
            "http://127.0.0.1:9/anthropic/v1/messages"
        );
        assert!(nested.url("/../admin").is_none());
        assert!(nested.url("/%2e%2e/admin").is_none());
        assert!(nested.url("/../anthropicx/v1").is_none());
        assert!(nested.url("/v1/../../x").is_none());
        assert!(nested.url("/v1/../ok").is_some());
    }

    #[test]
    fn upstreams_must_be_plain_http_origins() {
        for (text, reason) in [
            ("not a url", "not a URL"),
            ("ftp://example.com", "the scheme must be http or https"),
            ("https://user:pw@example.com", "credentials are not allowed"),
            (
                "https://example.com/?a=b",
                "a query or fragment is not allowed",
            ),
            (
                "https://example.com/#x",
                "a query or fragment is not allowed",
            ),
        ] {
            match Upstream::parse(text) {
                Err(ProxyError::Upstream(got)) => assert_eq!(got, reason, "{text}"),
                other => panic!("{text}: {:?}", other.map(|u| u.base)),
            }
        }
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_other_schemes_are_refused() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer  tok"));
        assert_eq!(
            presented(&headers),
            Ok((CredentialKind::Subscription, "tok"))
        );
        for value in ["Basic dXNlcjpwdw==", "Bearer", "tok"] {
            headers.insert(AUTHORIZATION, HeaderValue::from_static(value));
            assert_eq!(presented(&headers), Err(Refusal::NotBearer), "{value}");
        }
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
        );
        assert_eq!(presented(&headers), Err(Refusal::NotBearer));
        let mut keys = HeaderMap::new();
        keys.insert(X_API_KEY, HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(presented(&keys), Err(Refusal::UnknownPlaceholder));
    }

    #[test]
    fn hop_by_hop_headers_and_connection_names_are_stripped() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("connection", "keep-alive, X-Drop"),
            ("connection", "x-also"),
            ("keep-alive", "timeout=5"),
            ("x-drop", "1"),
            ("x-also", "1"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("proxy-authorization", "Basic x"),
            ("upgrade", "websocket"),
            ("anthropic-beta", "b"),
        ] {
            headers.append(name, HeaderValue::from_static(value));
        }
        strip_hop_by_hop(&mut headers);
        let names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["anthropic-beta"]);
    }

    #[test]
    fn a_credential_that_is_not_a_header_value_is_refused() {
        let bad = SecretString::from("line\nbreak");
        assert_eq!(
            credential_value(CredentialKind::ApiKey, &bad),
            Err(Refusal::BadCredential)
        );
        let good =
            credential_value(CredentialKind::Subscription, &SecretString::from("real")).unwrap();
        assert!(good.is_sensitive());
        assert_eq!(good, "Bearer real");
    }

    #[test]
    fn the_allow_header_lists_the_forwarded_methods() {
        let listed: Vec<&str> = FORWARDED_METHODS.iter().map(Method::as_str).collect();
        assert_eq!(listed.join(", "), ALLOWED);
        let response = Refusal::Method.into_response();
        assert_eq!(response.headers()[ALLOW], ALLOWED);
        assert!(
            !Refusal::NotPointed
                .into_response()
                .headers()
                .contains_key(ALLOW)
        );
    }

    #[test]
    fn every_refusal_has_a_reason_and_a_json_answer() {
        let all = [
            Refusal::NoPeer,
            Refusal::UnknownSource,
            Refusal::Method,
            Refusal::AbsoluteForm,
            Refusal::MissingCredential,
            Refusal::BothCredentials,
            Refusal::RepeatedCredential,
            Refusal::NotBearer,
            Refusal::UnknownPlaceholder,
            Refusal::OtherSource,
            Refusal::WrongKind,
            Refusal::NotPointed,
            Refusal::BadPath,
            Refusal::NotLinked,
            Refusal::NoCommunityKey,
            Refusal::CredentialUnavailable,
            Refusal::BadCredential,
            Refusal::Upstream,
        ];
        for refusal in all {
            assert!(!refusal.reason().is_empty());
            let (status, kind, message) = refusal.answer();
            assert!(status.is_client_error() || status.is_server_error());
            assert!(kind.ends_with("_error"));
            assert!(message.ends_with('.'));
            let response = refusal.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        }
        assert_eq!(
            Refusal::OtherSource.answer(),
            Refusal::UnknownPlaceholder.answer()
        );
    }
}
