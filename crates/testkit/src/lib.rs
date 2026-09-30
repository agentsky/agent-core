//! Test doubles and fakes for agent-core. Only ever a dev-dependency.
//!
//! - [`MockSurface`]: a [`Surface`](core_types::Surface) that records every
//!   call and lets a test inject events.
//! - The `fake-claude` binary, a stand-in for the Claude Code CLI, which
//!   other crates find with [`fake_claude_path`] and script with
//!   [`claude::Turn`]. [`claude`] documents what it checks and prints.
//! - [`fake_anthropic`]: a local server that answers like the Anthropic API.
//! - [`fixtures`]: stream-json lines captured from the real CLI.

#![warn(missing_docs)]

pub mod anthropic;
pub mod claude;
pub mod fixtures;
pub mod surface;

pub use anthropic::{FakeAnthropic, fake_anthropic};
pub use claude::{Turn, fake_claude_path, write_script};
pub use surface::{Call, MockSurface, UploadedFile};
