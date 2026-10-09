//! Test doubles and fakes for agent-core. Only ever a dev-dependency.
//!
//! - [`MockSurface`]: a [`Surface`](core_types::Surface) that records every
//!   call and lets a test inject events.
//! - The `fake-claude` binary, a stand-in for the Claude Code CLI, which
//!   other crates find with [`fake_claude_path`] and script with
//!   [`claude::Turn`]. [`claude`] documents what it checks and prints.
//!   [`agentctl_path`] builds `agentctl` for scripts that run it.
//! - [`fake_anthropic`]: a local server that answers like the Anthropic API.
//! - [`child`]: whether dropping a child's handles killed it before closing
//!   its stdin.
//! - [`fixtures`]: stream-json lines captured from the real CLI.
//! - [`held`]: a wiremock responder that answers when the test says so.
//! - [`Logs`]: a test binary's log lines, captured by one global
//!   subscriber.
//! - [`FakeProxy`]: a proxy that records what reached it, and
//!   [`proxy::assert_proxied_only_elsewhere`].
//! - [`rocketchat`]: fake Rocket.Chat REST and realtime servers.
//! - [`slack`]: Slack request signing and payload fixtures.
//! - [`TempDir`]: a directory for one test, removed when the test ends.

#![warn(missing_docs)]

pub mod anthropic;
pub mod child;
pub mod claude;
pub mod fixtures;
pub mod held;
pub mod logs;
pub mod proxy;
pub mod rocketchat;
pub mod slack;
pub mod surface;
mod temp_dir;

pub use anthropic::{FakeAnthropic, fake_anthropic};
pub use claude::{Turn, agentctl_path, fake_claude_path, write_script};
pub use held::{Held, Hold};
pub use logs::{Logged, Logs};
pub use proxy::FakeProxy;
pub use surface::{Call, MockSurface, Op, UploadedFile};
pub use temp_dir::TempDir;
