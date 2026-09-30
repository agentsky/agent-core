//! Credential-swapping reverse proxy and egress allowlist proxy for
//! agent-core sandboxes.
//!
//! A sandbox never holds a real Claude credential. Its `claude` process gets
//! a placeholder, as `CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`, and
//! `ANTHROPIC_BASE_URL` pointing at this proxy on agentd's proxy listener.
//! The proxy swaps the placeholder for the real credential and forwards the
//! request to the one configured upstream.
//!
//! - [`Registry`] holds the live placeholders. agentd's turn hooks
//!   [`mint`](Registry::mint) one per `claude` process, bound to the
//!   session and its container's address, [`point`](Registry::point) it at
//!   each turn's credential, [`unpoint`](Registry::unpoint) it when the turn
//!   ends, and [`revoke`](Registry::revoke_session) it before the container
//!   stops and again when it dies.
//! - [`CredProxy`] is the reverse proxy. agentd serves
//!   [`CredProxy::into_router`] on the proxy listener. Its rustdoc lists the
//!   rules it enforces.
//! - [`EgressProxy`] answers `CONNECT` on the same listener
//!   ([`CredProxy::with_egress`]): tunnels to the hosts an [`EgressPolicy`]
//!   allows, never to `api.anthropic.com`, and never to an address the
//!   policy keeps out, checked after resolution. [`EGRESS_ENV`] is the
//!   environment that points a sandbox's HTTPS at it.
//! - [`CommunityKey`] supplies the community API key, and
//!   [`ProxyObserver`] sees each forwarded request's status and usage
//!   headers.
//!
//! The proxy rules (design, "Credential proxy"):
//!
//! 1. Only the credential header is swapped, only for the configured
//!    upstream: `Authorization: Bearer` for a subscription placeholder,
//!    `x-api-key` for an API-key placeholder. A placeholder of one kind
//!    never receives a credential of the other kind, and nothing is
//!    substituted in bodies or other headers.
//! 2. One placeholder per `claude` process, bound to its container's
//!    address and pointed at the current turn's credential only while that
//!    turn runs.
//! 3. Sandbox egress goes through the proxy and an allowlist only, cloud
//!    metadata endpoints are blocked, and `api.anthropic.com` is blocked so
//!    side traffic fails loudly.

#![warn(missing_docs)]

mod allowlist;
mod egress;
mod hooks;
mod proxy;
mod registry;

pub use allowlist::{
    ANTHROPIC_API_HOST, DEFAULT_PORT, EgressPolicy, HostRule, HostRuleError, PolicyError,
    normalize_host,
};
pub use egress::{
    EGRESS_ENV, EgressExtension, EgressProxy, NO_PROXY, Network, PROXY_URL, RESOLVE_TIMEOUT,
    SystemNetwork, TUNNEL_IDLE_TIMEOUT,
};
pub use hooks::{CommunityKey, CommunityKeyError, FixedKey, Observation, ProxyObserver};
pub use proxy::{CONNECT_TIMEOUT, CredProxy, DEFAULT_UPSTREAM, ProxyError};
pub use registry::{
    API_KEY_PREFIX, Placeholder, PlaceholderId, Registry, RegistryError, SUBSCRIPTION_PREFIX,
};
