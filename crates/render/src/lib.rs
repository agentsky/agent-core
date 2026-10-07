//! Markdown rendering for the chat surfaces.
//!
//! Agents write standard Markdown. Each surface converts it with a renderer
//! built on the `pulldown-cmark` parse tree, never with regexes over the raw
//! text. [`slack::to_mrkdwn`] produces Slack mrkdwn.
//!
//! The crate does no I/O. Mention lookups go through [`MentionDirectory`],
//! which the caller backs with whatever member list it keeps.

mod mention;
pub mod slack;

/// Resolves a display name written as `@Name` to a platform user id.
///
/// Renderers call [`resolve`](Self::resolve) with the name exactly as the
/// agent wrote it, one to three space-separated words, trying the longest
/// candidate first. Case folding and any other matching rules are up to the
/// implementation. Broadcast names such as `here` are never passed in.
pub trait MentionDirectory {
    /// Returns the platform user id for `name`, or `None` when no member has
    /// that name.
    fn resolve(&self, name: &str) -> Option<String>;
}
