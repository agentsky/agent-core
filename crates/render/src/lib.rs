//! Markdown rendering, splitting and directives for the chat surfaces.
//!
//! Agents write standard Markdown. A reply goes out in three steps:
//!
//! 1. [`directives::extract`] removes directives such as `[[react: eyes]]`
//!    from the Markdown.
//! 2. A surface renderer converts the rest: [`slack::to_mrkdwn`] produces
//!    Slack mrkdwn, and [`rocketchat::to_markdown`] produces Rocket.Chat
//!    Markdown. Both work on the `pulldown-cmark` parse tree, never with
//!    regexes over the raw text.
//! 3. [`split()`] cuts the rendered text into messages that fit the
//!    surface's limit, [`slack::MESSAGE_LIMIT`] or
//!    [`rocketchat::DEFAULT_MESSAGE_LIMIT`].
//!
//! Splitting comes after rendering because rendering changes the length and
//! needs whole constructs to convert, so the splitter knows both renderers'
//! output syntax instead.
//!
//! The crate does no I/O. Mention lookups go through [`MentionDirectory`],
//! which the caller backs with whatever member list it keeps.

pub mod directives;
mod mention;
pub mod rocketchat;
pub mod slack;
mod split;
mod url;
mod verbatim;

pub use split::split;

/// Resolves a display name written as `@Name` to the handle the surface's
/// mention syntax needs: a user id on Slack (`<@U123>`), a username on
/// Rocket.Chat (`@ada`).
///
/// Renderers call [`resolve`](Self::resolve) with the name exactly as the
/// agent wrote it, one to three space-separated words, trying the longest
/// candidate first. Case folding and any other matching rules are up to the
/// implementation. Broadcast names such as `here` are never passed in.
pub trait MentionDirectory {
    /// Returns the handle for `name`, or `None` when no member has that
    /// name.
    fn resolve(&self, name: &str) -> Option<String>;
}
