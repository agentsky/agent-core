//! Slack surface for agent-core.
//!
//! This crate holds the HTTPS side of Slack so far:
//!
//! - [`verify`]: `v0` request signatures, checked in constant time within a
//!   five-minute window.
//! - [`ingress`](mod@ingress): the request URLs `POST /slack/b/{binding}/events`,
//!   `…/interactivity` and `…/commands`, which verify, acknowledge at once
//!   and queue; and the [`Queue`] behind them, which deduplicates and
//!   normalizes. agentd supplies the [`SigningSecrets`] and [`Dedup`]
//!   implementations, so this crate doesn't depend on the store.
//! - [`normalize`]: `message` events to [`InboundEvent`](core_types::InboundEvent).
//! - [`inbound`]: [`SlackInbound`], what the queue hands on.

#![warn(missing_docs)]

pub mod inbound;
pub mod ingress;
pub mod normalize;
pub mod verify;

pub use inbound::{Interaction, SlackEvent, SlackInbound, SlashCommand};
pub use ingress::{BindingRef, BoxError, Dedup, Queue, SigningSecrets, SlackApp, ingress};
