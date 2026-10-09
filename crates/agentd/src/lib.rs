//! The agent-core daemon: surfaces, routing, runner and credential proxy in
//! one binary.
//!
//! The binary is a thin wrapper around [`cli::main`]. Everything else is
//! here, so tests and later tasks can build an [`App`] and a
//! [`Server`](server::Server) in-process:
//!
//! - [`config`]: the TOML file plus secret environment variables.
//! - [`app`]: [`App`], the shared state.
//! - [`agents`]: agents' bot identities on Rocket.Chat and the connections
//!   agentd keeps as them.
//! - [`commands`]: `/agent` command dispatch, the account and agent commands, and
//!   the relink notice.
//! - [`community`]: the community API key, which the credential proxy
//!   reads from the store.
//! - [`consents`]: private tasks' consents, their cards, and their
//!   expiry.
//! - [`ctl`]: the agentctl API and the turn hooks' token functions.
//! - [`pipeline`]: the runner's sessions and sandboxes, and the turn hooks
//!   that give each process its placeholder and agentctl token.
//! - [`policy`]: agents' limits and allow and deny rules, and the
//!   community's caps on threads and hops.
//! - [`server`]: the listeners, `/healthz`, and graceful shutdown.
//! - [`skills`]: agents' skills, the bundled `agentctl` skill, and the
//!   egress hosts skills declare.
//! - [`slack`]: the Slack request URLs' signing secrets, deduplication and
//!   queue.
//! - [`sweeper`]: deleting expired rows every minute.
//! - [`telemetry`]: log output and field redaction.
//! - [`net`]: subnets and the public listener's guard.

#![warn(missing_docs)]

pub mod agents;
pub mod app;
pub mod cli;
pub mod commands;
pub mod community;
pub mod config;
pub mod consents;
pub mod ctl;
pub mod net;
pub mod pipeline;
pub mod policy;
pub mod server;
pub mod skills;
pub mod slack;
pub mod sweeper;
pub mod telemetry;

pub use app::App;
pub use config::Config;
