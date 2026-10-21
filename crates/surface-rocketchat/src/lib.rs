//! Rocket.Chat surface for agent-core.
//!
//! - [`RocketChatSurface`]: the [`Surface`](core_types::Surface) for one bot
//!   user, over the REST client and one realtime connection.
//! - [`rest`]: the REST client, used to create bot users, post, edit,
//!   react, upload and read history.
//! - [`realtime`]: the DDP client that hears the bot's rooms.
//!
//! Several bots in one room each receive every message. The surface
//! delivers a message only when [`Dedup`], which agentd backs with the
//! store's processed events, records it first.

#![warn(missing_docs)]

mod ddp;
mod normalize;
pub mod realtime;
pub mod rest;
mod surface;

pub use surface::{BotRoles, DEDUP_SOURCE, Dedup, RocketChatConfig, RocketChatSurface};
