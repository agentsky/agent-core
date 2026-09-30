//! Markdown for Rocket.Chat.
//!
//! Rocket.Chat renders Markdown itself, so the agent's text passes through
//! almost unchanged. Only mentions are rewritten, with the `@Name` grammar
//! the Slack renderer uses.

use core_types::{LengthUnit, Limit};

use crate::url::bare_url;
use crate::verbatim::{self, Scope};
use crate::{MentionDirectory, mention};

/// Names Rocket.Chat treats as broadcasts to a whole room.
const BROADCASTS: &[&str] = &["all", "here"];

/// Inserted after the `@` of a broadcast so it can't notify anyone.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// The default message limit: the server's default `Message_MaxAllowedSize`
/// of 5,000, which Rocket.Chat checks against the JavaScript string length,
/// so it counts UTF-16 code units. Servers can change the setting, so the
/// surface takes the value from configuration and uses this as the default.
pub const DEFAULT_MESSAGE_LIMIT: Limit = Limit {
    max: 5_000,
    unit: LengthUnit::Utf16,
};

/// Converts agent-written Markdown to Rocket.Chat Markdown.
///
/// The text passes through unchanged except outside code spans, code
/// blocks, link destinations and URLs:
///
/// - `@all` and `@here` are neutralized with a zero-width space after the
///   `@`, ignoring case, so they can't notify the room. They are never
///   offered to the directory.
/// - `@Name` becomes `@username` when `directory` resolves it, and stays
///   text otherwise. On Rocket.Chat the directory returns usernames. A
///   username that isn't made of letters, digits, `.`, `_` and `-`, or that
///   is a broadcast name, is ignored, so a directory entry can't turn a
///   mention into `@all`.
///
/// # Examples
///
/// ```
/// use render::{MentionDirectory, rocketchat::to_markdown};
///
/// struct Team;
///
/// impl MentionDirectory for Team {
///     fn resolve(&self, name: &str) -> Option<String> {
///         (name == "Ada Lovelace").then(|| "ada".to_string())
///     }
/// }
///
/// assert_eq!(
///     to_markdown("**Thanks** @Ada Lovelace, and @all: see `@here`", &Team),
///     "**Thanks** @ada, and @\u{200B}all: see `@here`",
/// );
/// ```
pub fn to_markdown(md: &str, directory: &dyn MentionDirectory) -> String {
    let mut out = String::with_capacity(md.len());
    let mut at = 0;
    for range in verbatim::ranges(md, Scope::CodeAndLinkTargets) {
        rewrite(&md[at..range.start], directory, &mut out);
        out.push_str(&md[range.clone()]);
        at = range.end;
    }
    rewrite(&md[at..], directory, &mut out);
    out
}

fn rewrite(text: &str, directory: &dyn MentionDirectory, out: &mut String) {
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if c == '@' {
            if let Some(end) = mention::broadcast(text, i, BROADCASTS) {
                out.push('@');
                out.push(ZERO_WIDTH_SPACE);
                out.push_str(&text[i + 1..end]);
                i = end;
                continue;
            }
            if let Some(found) = mention::scan(text, i, BROADCASTS, directory) {
                match found.id.filter(|id| is_username(id)) {
                    Some(username) => {
                        out.push('@');
                        out.push_str(&username);
                    }
                    None => out.push_str(&text[i..found.end]),
                }
                i = found.end;
                continue;
            }
        }
        if let Some(url) = bare_url(text, i) {
            out.push_str(url);
            i += url.len();
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
}

fn is_username(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !BROADCASTS.iter().any(|b| name.eq_ignore_ascii_case(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(&'static [(&'static str, &'static str)]);

    impl MentionDirectory for Directory {
        fn resolve(&self, name: &str) -> Option<String> {
            let name = name.to_lowercase();
            self.0
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, username)| (*username).to_string())
        }
    }

    const TEAM: Directory = Directory(&[
        ("ada", "ada.l"),
        ("alex morgan", "amorgan"),
        ("alex", "alex"),
        ("all", "all.hands"),
        ("sneaky", "all"),
        ("spacey", "has space"),
    ]);

    fn check(cases: &[(&str, &str, &str)]) {
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|&(name, input, expected)| {
                let actual = to_markdown(input, &TEAM);
                (actual != expected).then(|| {
                    format!("{name}\n  input:    {input:?}\n  expected: {expected:?}\n  actual:   {actual:?}")
                })
            })
            .collect();
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn markdown_passes_through() {
        let md = "# Title\n\n**bold** _em_ ~~gone~~ <b>html</b> & a < b\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quote\n\n- item\n  1. nested\n";
        assert_eq!(to_markdown(md, &TEAM), md);
    }

    #[test]
    fn broadcasts_are_neutralized_outside_code() {
        check(&[
            ("@all", "hey @all", "hey @\u{200B}all"),
            ("@here", "@here now", "@\u{200B}here now"),
            (
                "ignores case",
                "@ALL and @Here",
                "@\u{200B}ALL and @\u{200B}Here",
            ),
            ("in emphasis", "**@all**", "**@\u{200B}all**"),
            (
                "in a link label",
                "[@all](https://x.io)",
                "[@\u{200B}all](https://x.io)",
            ),
            (
                "followed by punctuation",
                "@all, @here.",
                "@\u{200B}all, @\u{200B}here.",
            ),
            ("not an email", "me@all.io", "me@all.io"),
            ("longer words are names", "@allison", "@allison"),
            ("inline code", "`@all`", "`@all`"),
            ("fenced code", "```\n@here\n```", "```\n@here\n```"),
            ("indented code", "    @all\n", "    @all\n"),
            (
                "link destination",
                "[x](https://x.io/@all)",
                "[x](https://x.io/@all)",
            ),
            ("autolink", "<https://x.io/@here>", "<https://x.io/@here>"),
            ("bare URL", "https://x.io/@all ok", "https://x.io/@all ok"),
            (
                "Slack names are not broadcasts here",
                "@channel",
                "@channel",
            ),
        ]);
    }

    #[test]
    fn broadcast_names_never_reach_the_directory() {
        check(&[("a member named all", "@all", "@\u{200B}all")]);
    }

    #[test]
    fn names_resolve_to_usernames() {
        check(&[
            ("one word", "thanks @Ada!", "thanks @ada.l!"),
            ("longest name first", "@Alex Morgan said", "@amorgan said"),
            ("capitalized next word", "@Alex Torres", "@Alex Torres"),
            ("unknown stays text", "@nobody here", "@nobody here"),
            (
                "in a label",
                "[@Ada](https://x.io)",
                "[@ada.l](https://x.io)",
            ),
            ("not in code", "`@Ada`", "`@Ada`"),
            (
                "URL after an unknown name",
                "@nobody https://x.io/@all",
                "@nobody https://x.io/@all",
            ),
            ("a username that is a broadcast", "@Sneaky", "@Sneaky"),
            ("a username with a space", "@Spacey", "@Spacey"),
        ]);
    }

    #[test]
    fn the_default_limit_counts_utf16() {
        assert_eq!(DEFAULT_MESSAGE_LIMIT.max, 5_000);
        assert_eq!(DEFAULT_MESSAGE_LIMIT.unit, LengthUnit::Utf16);
    }
}
