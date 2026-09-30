//! Slack surface for agent-core.
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
//! - [`web`]: the Web API client. [`SlackClient`] holds the connection pool
//!   and a per-token rate limiter by method tier; [`WebApi`] acts with one
//!   binding's bot token. [`SlackClient::respond_ephemeral`] answers a slash
//!   command or interaction privately through its `response_url`, and
//!   [`SlackClient::rotate_config_token`] renews a member's app
//!   configuration token.
//! - [`directory`]: the per-workspace caches: members by name from
//!   `users.list`, for `@Name` mentions, and bot users by bot id from
//!   `bots.info`.
//! - [`surface`](mod@surface): [`SlackSurface`], the
//!   [`Surface`](core_types::Surface) for one binding.
//! - [`manifest`]: agent apps, created from [`manifest::agent_manifest`]
//!   with [`SlackClient::create_app`], installed through
//!   [`manifest::install_url`] and [`SlackClient::install_app`], and
//!   deleted with [`SlackClient::delete_app`].
//!
//! A normalized bot message that carried only a `bot_id` names its sender
//! by that id. Before routing it, the receiver of [`SlackInbound`] passes it
//! to [`SlackSurface::fill_bot_sender`] of the receiving binding, which
//! looks the bot's user up.

#![warn(missing_docs)]

pub mod directory;
pub mod inbound;
pub mod ingress;
mod limit;
pub mod manifest;
pub mod normalize;
pub mod surface;
pub mod verify;
pub mod web;

pub use directory::{MemberDirectory, TeamDirectory};
pub use inbound::{Interaction, SlackEvent, SlackInbound, SlashCommand};
pub use ingress::{
    BindingRef, BoxError, Dedup, InFlight, Queue, SigningSecrets, SlackApp, ingress,
};
pub use surface::SlackSurface;
pub use web::{ConfigToken, CreatedApp, Installation, SlackClient, WebApi};
