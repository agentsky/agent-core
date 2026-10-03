//! The agent-core daemon: surfaces, routing, runner and credential proxy in
//! one binary.
//!
//! The binary is a thin wrapper around [`cli::main`]. Everything else is
//! here, so tests and later tasks can build an [`App`] and a
//! [`Server`](server::Server) in-process:
//!
//! - [`config`]: the TOML file plus secret environment variables.
//! - [`app`]: [`App`], the shared state.
//! - [`ctl`]: the agentctl API and the turn hooks' token functions.
//! - [`server`]: the listeners, `/healthz`, and graceful shutdown.
//! - [`slack`]: the Slack request URLs' signing secrets, deduplication and
//!   queue.
//! - [`sweeper`]: deleting expired rows every minute.
//! - [`telemetry`]: log output and field redaction.
//! - [`net`]: subnets and the public listener's guard.

#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod config;
pub mod ctl;
pub mod net;
pub mod server;
pub mod slack;
pub mod sweeper;
pub mod telemetry;

pub use app::App;
pub use config::Config;
