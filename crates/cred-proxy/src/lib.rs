//! Credential-swapping reverse proxy for agent-core sandboxes.
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
//!   each turn's credential, and [`revoke`](Registry::revoke_session) it
//!   before the container stops and again when it dies.
//! - [`CredProxy`] is the reverse proxy. agentd serves
//!   [`CredProxy::into_router`] on the proxy listener. Its rustdoc lists the
//!   rules it enforces.
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
//!    address and pointed at the current turn's credential.

#![warn(missing_docs)]

mod hooks;
mod proxy;
mod registry;

pub use hooks::{CommunityKey, CommunityKeyError, FixedKey, Observation, ProxyObserver};
pub use proxy::{CONNECT_TIMEOUT, CredProxy, DEFAULT_UPSTREAM, ProxyError};
pub use registry::{
    API_KEY_PREFIX, Placeholder, PlaceholderId, Registry, RegistryError, SUBSCRIPTION_PREFIX,
};
