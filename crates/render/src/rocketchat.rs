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
/// The text passes through unchanged except for mentions:
///
/// - `@all` and `@here` are neutralized with a zero-width space after the
///   `@`, ignoring case, so they can't notify the room. This covers the
///   whole output, code and link destinations included, because the server
///   looks for mentions in the raw text: after an `@`, it reads the longest
///   run of ASCII letters, digits, `.`, `_` and `-`, and a run equal to
///   `all` or `here` is a broadcast, wherever it is and whatever follows.
///   So `@allé` and `` `@here` `` are neutralized too, and so is a run
///   that only adds trailing `.`, `_` or `-` (`@here.`), while `@allison`
///   and `@all.hands` are not. An `@` right after `/` is left alone,
///   because the server only reads a mention after the start of a line,
///   whitespace or `>`, so `https://x.io/@all` stays a working link.
///   Broadcasts are never offered to the directory.
/// - Outside code spans, code blocks, link destinations and URLs, `@Name`
///   becomes `@username` when `directory` resolves it, and stays text
///   otherwise. On Rocket.Chat the directory returns usernames. A username
///   that isn't made of ASCII letters, digits, `.`, `_` and `-`, or that is
///   a broadcast name, is ignored, so a directory entry can't turn a
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
///     "**Thanks** @ada, and @\u{200B}all: see `@\u{200B}here`",
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
    neutralize_broadcasts(&out)
}

fn rewrite(text: &str, directory: &dyn MentionDirectory, out: &mut String) {
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if c == '@' {
            if let Some(end) = broadcast_end(text, i) {
                out.push_str(&text[i..end]);
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

/// Inserts a zero-width space after every `@` that could start a broadcast
/// in the server's grammar, `(^|\s|>)@`. An `@` after `/` can't, so URLs
/// such as `https://x.io/@all` keep working.
fn neutralize_broadcasts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (i, _) in text.match_indices('@') {
        if !text[..i].ends_with('/') && broadcast_end(text, i).is_some() {
            out.push_str(&text[at..=i]);
            out.push(ZERO_WIDTH_SPACE);
            at = i + 1;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Returns the end of the name after the `@` at byte offset `at` of `text`
/// when the server could read that name as a broadcast.
fn broadcast_end(text: &str, at: usize) -> Option<usize> {
    let rest = text[at..].strip_prefix('@')?;
    let len = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
    is_broadcast(&rest[..len]).then_some(at + 1 + len)
}

/// Whether `name`, a run of name characters, is a broadcast name, ignoring
/// case and trailing `.`, `_` and `-`. The server's default grammar keeps
/// those in the name, so `@here.` doesn't notify today, but neutralizing it
/// costs nothing and stays safe under a grammar that ends names earlier.
fn is_broadcast(name: &str) -> bool {
    let name = name.trim_end_matches(['.', '_', '-']);
    BROADCASTS.iter().any(|b| name.eq_ignore_ascii_case(b))
}

/// The characters of a name in the server's mention pattern: its default
/// `UTF8_User_Names_Validation` setting, `[0-9a-zA-Z-_.]+`.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn is_username(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_name_char) && !is_broadcast(name)
}

#[cfg(test)]
pub(crate) mod server;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directives;

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
    fn broadcasts_are_neutralized_everywhere() {
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
            ("a dotted name", "@all.hands", "@all.hands"),
            ("inline code", "`@all`", "`@\u{200B}all`"),
            (
                "inline code after a space",
                "run ` @all` now",
                "run ` @\u{200B}all` now",
            ),
            ("fenced code", "```\n@here\n```", "```\n@\u{200B}here\n```"),
            (
                "tilde fence",
                "~~~\nx @all\n~~~",
                "~~~\nx @\u{200B}all\n~~~",
            ),
            (
                "indented code",
                "    @here indented",
                "    @\u{200B}here indented",
            ),
            (
                "link destination after a slash",
                "[x](https://x.io/@all)",
                "[x](https://x.io/@all)",
            ),
            (
                "link destination after another character",
                "[x](https://x.io/~@all)",
                "[x](https://x.io/~@\u{200B}all)",
            ),
            (
                "link title across lines",
                "[x](https://a.io '\n@all')",
                "[x](https://a.io '\n@\u{200B}all')",
            ),
            (
                "autolink after a slash",
                "<https://x.io/@here>",
                "<https://x.io/@here>",
            ),
            (
                "bare URL after a slash",
                "https://x.io/@all ok",
                "https://x.io/@all ok",
            ),
            (
                "a slash only shields the @ right after it",
                "https://x.io/@all @all\n@here >@all /x @here",
                "https://x.io/@all @\u{200B}all\n@\u{200B}here >@\u{200B}all /x @\u{200B}here",
            ),
            (
                "inline HTML",
                "<a href=x>@all</a>",
                "<a href=x>@\u{200B}all</a>",
            ),
            ("quote", "> @here", "> @\u{200B}here"),
            ("quote without a space", ">@all", ">@\u{200B}all"),
            ("after a letter", "x@all", "x@\u{200B}all"),
            (
                "a non-ASCII letter ends the name",
                "hey @allé",
                "hey @\u{200B}allé",
            ),
            (
                "a non-ASCII digit ends the name",
                "hey @here\u{663}",
                "hey @\u{200B}here\u{663}",
            ),
            (
                "a federated name",
                "@all@server @here:srv",
                "@\u{200B}all@server @\u{200B}here:srv",
            ),
            (
                "Unicode spaces",
                "\u{A0}@all\u{3000}@here",
                "\u{A0}@\u{200B}all\u{3000}@\u{200B}here",
            ),
            ("already neutral", "@\u{200B}all", "@\u{200B}all"),
            (
                "Slack names are not broadcasts here",
                "@channel",
                "@channel",
            ),
        ]);
    }

    #[test]
    fn broadcast_names_never_reach_the_directory() {
        check(&[
            ("a member named all", "@all", "@\u{200B}all"),
            (
                "a longer name starting with a broadcast",
                "@All Hands",
                "@\u{200B}All Hands",
            ),
        ]);
    }

    struct Evil;

    impl MentionDirectory for Evil {
        fn resolve(&self, name: &str) -> Option<String> {
            match name {
                "Bob" => Some("allé".into()),
                "Carol" => Some("here".into()),
                "Dan" => Some("all.".into()),
                "Erin" => Some("ⅰall".into()),
                _ => None,
            }
        }
    }

    #[test]
    fn a_directory_entry_cannot_become_a_broadcast() {
        for (input, expected) in [
            ("hi @Bob", "hi @Bob"),
            ("hi @Carol", "hi @Carol"),
            ("hi @Dan", "hi @Dan"),
            ("hi @Erin", "hi @Erin"),
        ] {
            assert_eq!(to_markdown(input, &Evil), expected);
        }
    }

    const PROVEN: &[&str] = &[
        "```\n@all\n```",
        "    @here indented",
        "run ` @all` now",
        "hey @allé",
        "hey @here\u{663}",
        "[x](https://a.io '\n@all')",
        "<a href=x>@all</a>",
        "> ```\n> @all\n> ```",
        "- item\n\n      @here in a list's code",
        "text\n[x](y)@all",
        "[x](y)\n@here",
        "@all\u{2028}@here",
        "https://x.io/@all",
        "[x](https://x.io/@all)",
        "<https://x.io/@here>",
        "[a](/@all)",
        "/[a](b)@all",
    ];

    #[test]
    fn the_server_finds_no_broadcast_in_the_output() {
        for md in PROVEN {
            let out = to_markdown(md, &TEAM);
            assert_eq!(server::broadcasts(&out), Vec::<String>::new(), "{md:?}");
            assert_eq!(
                server::broadcasts(&to_markdown(md, &Evil)),
                Vec::<String>::new()
            );
        }
        assert!(server::broadcasts(&to_markdown("hi @Bob", &Evil)).is_empty());
    }

    #[test]
    fn stripping_directives_cannot_expose_a_broadcast() {
        for md in [
            "[[react: eyes]]```\n@all\n```",
            "x\n[[react: eyes]]\n    @here",
            "[[react: eyes]]\n@all",
            "a [[react: eyes]]@here",
            "```\n[[react: eyes]]\n@all",
        ] {
            let (text, _) = directives::extract(md);
            let out = to_markdown(&text, &TEAM);
            assert!(server::broadcasts(&out).is_empty(), "{md:?} gave {out:?}");
        }
    }

    #[test]
    fn property_no_output_holds_a_broadcast() {
        const PARTS: &[&str] = &[
            "@all",
            "@here",
            "@ALL",
            "@Here",
            "@allé",
            "@here\u{663}",
            "@all.",
            "x",
            " ",
            "\n",
            "\n\n",
            ">",
            "> ",
            "`",
            "```\n",
            "~~~\n",
            "    ",
            "[",
            "]",
            "(",
            ")",
            "](",
            "@",
            "@Ada",
            "@Sneaky",
            "@All Hands",
            "\u{A0}",
            "\u{200B}",
            "é",
            "https://x.io/",
            "/",
            "<",
            ">",
            "'",
            "*",
            "_",
            "-",
            ".",
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for seed in 0..4000 {
            let mut md = String::new();
            for _ in 0..(seed % 24) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                md.push_str(PARTS[(state % PARTS.len() as u64) as usize]);
            }
            let out = to_markdown(&md, &TEAM);
            assert!(
                server::broadcasts(&out).is_empty(),
                "seed {seed}: {md:?} gave {out:?}"
            );
        }
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
                "@nobody https://x.io/@allow",
                "@nobody https://x.io/@allow",
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
