//! Text directives the agent writes into a reply, such as `[[react: eyes]]`.
//!
//! The behavioral reference is qm-core's `extractReactions` in
//! `src/slack/reactions.ts` and `extractDirectives` in
//! `src/slack/directives.ts`. The tests marked `qm-core` are ported from its
//! `test/slack-reactions.test.ts`.

use std::ops::Range;

use crate::verbatim::{self, Scope};

/// The most reactions one reply may add, as in qm-core.
pub const MAX_REACTIONS: usize = 5;

const REACT: &[u8] = b"[[react:";

/// An instruction the agent embedded in its reply.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Directive {
    /// Add a reaction to the message the reply answers.
    React {
        /// The emoji short name, lowercase and without colons (`eyes`,
        /// `+1`, `thumbsup::skin-tone-3`).
        emoji: String,
    },
}

/// Removes directives from `text` and returns the remaining text with the
/// directives found, in order.
///
/// Call it on the agent's Markdown before rendering.
///
/// - `[[react: <emoji>]]` adds a reaction. The keyword ignores ASCII case,
///   one directive may name several emoji separated by spaces or commas,
///   and names may carry colons (`:eyes:`). A name that isn't a valid short
///   name is dropped. Duplicates are dropped, and at most
///   [`MAX_REACTIONS`] reactions are returned.
/// - A directive naming a target message after `@` is removed without
///   effect: reacting to another message isn't supported yet.
/// - A directive inside a code span or code block is left as text.
/// - An unclosed `[[react:` on the last line, with no `]` after it, is
///   removed up to the end without effect, so a truncated reply doesn't
///   show the markup.
///
/// Text without directives comes back unchanged. Otherwise a line that held
/// only directives is removed, whitespace next to a directive at the start
/// or end of a line goes with it, at most one blank line is left where lines
/// were removed, and leading blank lines and trailing whitespace are
/// trimmed. Code is left alone, apart from that trim when the text ends
/// inside a code block.
///
/// # Examples
///
/// ```
/// use render::directives::{Directive, extract};
///
/// let (text, directives) = extract("On it. [[react: eyes]]\n\nUse `[[react: x]]` to react.");
/// assert_eq!(text, "On it.\n\nUse `[[react: x]]` to react.");
/// assert_eq!(directives, [Directive::React { emoji: "eyes".into() }]);
/// ```
pub fn extract(text: &str) -> (String, Vec<Directive>) {
    let found = find(text);
    if found.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut directives: Vec<Directive> = Vec::new();
    for inner in found.iter().filter_map(|(_, inner)| *inner) {
        for emoji in reactions(inner) {
            let directive = Directive::React { emoji };
            if directives.len() < MAX_REACTIONS && !directives.contains(&directive) {
                directives.push(directive);
            }
        }
    }
    let ranges: Vec<Range<usize>> = found.into_iter().map(|(range, _)| range).collect();
    (strip(text, &ranges), directives)
}

/// Finds the directives outside code: their byte ranges and, unless the
/// directive is unclosed, the text between `[[react:` and `]]`.
fn find(text: &str) -> Vec<(Range<usize>, Option<&str>)> {
    let bytes = text.as_bytes();
    let mut code: Option<Vec<Range<usize>>> = None;
    let mut found = Vec::new();
    let mut next_close: Option<Option<usize>> = None;
    let last_line = text.trim_end().rfind('\n').map_or(0, |i| i + 1);
    let mut at = 0;
    while let Some(offset) = text[at..].find("[[") {
        let start = at + offset;
        at = start + 1;
        let is_react = bytes
            .get(start..start + REACT.len())
            .is_some_and(|b| b.eq_ignore_ascii_case(REACT));
        if !is_react {
            continue;
        }
        let inner_start = start + REACT.len();
        let close = match next_close {
            Some(Some(close)) if close >= inner_start => Some(close),
            Some(None) => None,
            _ => {
                let close = text[inner_start..].find(']').map(|i| inner_start + i);
                next_close = Some(close);
                close
            }
        };
        let (range, inner) = match close {
            Some(close) if text[close..].starts_with("]]") => {
                (start..close + 2, Some(&text[inner_start..close]))
            }
            Some(_) => continue,
            None if inner_start < last_line => continue,
            None => (start..text.len(), None),
        };
        let code = code.get_or_insert_with(|| verbatim::ranges(text, Scope::Code));
        if verbatim::overlaps(code, &range) {
            continue;
        }
        at = range.end;
        found.push((range, inner));
    }
    found
}

/// The valid emoji names of one directive's text.
fn reactions(inner: &str) -> Vec<String> {
    if inner.contains('@') {
        return Vec::new();
    }
    inner
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter_map(normalize)
        .collect()
}

/// Lowercases a name and strips its colons. Returns `None` unless it is a
/// short name: `[a-z0-9_+'-]+`, optionally followed by `::skin-tone-2` to
/// `::skin-tone-6`.
fn normalize(raw: &str) -> Option<String> {
    let name = raw.trim_matches(':').to_ascii_lowercase();
    let (base, tone) = match name.split_once("::") {
        Some((base, tone)) => (base, Some(tone)),
        None => (name.as_str(), None),
    };
    let base_ok = !base.is_empty()
        && base
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_+'-".contains(c));
    let tone_ok = tone.is_none_or(|tone| {
        tone.strip_prefix("skin-tone-")
            .is_some_and(|n| matches!(n, "2" | "3" | "4" | "5" | "6"))
    });
    (base_ok && tone_ok).then_some(name)
}

/// Removes `directives` (sorted, disjoint byte ranges) from `text`, with the
/// whitespace rules [`extract`] describes.
fn strip(text: &str, directives: &[Range<usize>]) -> String {
    let bytes = text.as_bytes();
    let mut in_directive = vec![false; bytes.len()];
    for range in directives {
        in_directive[range.clone()].fill(true);
    }
    let filler = |i: usize| in_directive[i] || matches!(bytes[i], b' ' | b'\t' | b'\r');
    let mut next_content = vec![bytes.len(); bytes.len() + 1];
    for i in (0..bytes.len()).rev() {
        next_content[i] = if filler(i) { next_content[i + 1] } else { i };
    }
    let mut removals: Vec<Range<usize>> = Vec::new();
    let mut prev_content: Option<usize> = None;
    let mut ranges = directives.iter().peekable();
    for i in 0..=bytes.len() {
        while let Some(range) = ranges.next_if(|range| range.start == i) {
            let before = prev_content.filter(|&p| bytes[p] != b'\n');
            let line_start = prev_content.map_or(0, |p| p + 1);
            let after = next_content[range.end];
            let rest_of_line = after == bytes.len() || bytes[after] == b'\n';
            removals.push(match (before, rest_of_line) {
                (None, true) => line_start..(after + 1).min(bytes.len()),
                (Some(content), true) => content + 1..after,
                (None, false) => range.start..after,
                (Some(_), false) => range.clone(),
            });
        }
        if i < bytes.len() && !filler(i) {
            prev_content = Some(i);
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut kept_from = 0;
    let mut removed_line = false;
    for removal in merge(removals) {
        push_kept(&mut out, &text[kept_from..removal.start], removed_line);
        removed_line = text[removal.clone()].contains('\n');
        kept_from = removal.end;
    }
    push_kept(&mut out, &text[kept_from..], removed_line);
    let trimmed = out.trim_end();
    let leading = trimmed.len() - trimmed.trim_start().len();
    let skip = trimmed[..leading].rfind('\n').map_or(0, |i| i + 1);
    trimmed[skip..].to_string()
}

/// Appends text kept between removals. After a removal that took a line
/// break, leading newlines of `segment` are dropped so that at most one
/// blank line is left at the join.
fn push_kept(out: &mut String, segment: &str, removed_line: bool) {
    if !removed_line {
        out.push_str(segment);
        return;
    }
    let trailing = out.len() - out.trim_end_matches('\n').len();
    let leading = segment.len() - segment.trim_start_matches('\n').len();
    let drop = leading.min((trailing + leading).saturating_sub(2));
    out.push_str(&segment[drop..]);
}

fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn react(names: &[&str]) -> Vec<Directive> {
        names
            .iter()
            .map(|name| Directive::React {
                emoji: (*name).to_string(),
            })
            .collect()
    }

    fn check(cases: &[(&str, &str, &str, &[&str])]) {
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|&(name, input, text, names)| {
                let actual = extract(input);
                let expected = (text.to_string(), react(names));
                (actual != expected).then(|| {
                    format!("{name}\n  input:    {input:?}\n  expected: {expected:?}\n  actual:   {actual:?}")
                })
            })
            .collect();
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn qm_core_cases() {
        check(&[
            (
                "qm-core: a directive on its own line",
                "On it!\n\n[[react: eyes white_check_mark]]\n\nLooking now.",
                "On it!\n\nLooking now.",
                &["eyes", "white_check_mark"],
            ),
            (
                "qm-core: inline, commas and several directives",
                "Done [[react: white_check_mark]] and [[react: tada, rocket]] shipping.",
                "Done  and  shipping.",
                &["white_check_mark", "tada", "rocket"],
            ),
            (
                "qm-core: no directive",
                "just a normal reply",
                "just a normal reply",
                &[],
            ),
            (
                "qm-core: a react-only reply",
                "[[react: eyes]]",
                "",
                &["eyes"],
            ),
            (
                "qm-core: a reply without directives is unchanged byte for byte",
                "```\nline1   \n\n\n\nline2\n```\n",
                "```\nline1   \n\n\n\nline2\n```\n",
                &[],
            ),
            (
                "qm-core: an unclosed directive at the end",
                "on it [[react: eyes",
                "on it",
                &[],
            ),
            (
                "qm-core: an empty directive is removed",
                "hi [[react:]] there",
                "hi  there",
                &[],
            ),
            (
                "qm-core: a directive across a newline",
                "done\n[[react: eyes\nwhite_check_mark]]\n",
                "done",
                &["eyes", "white_check_mark"],
            ),
            ("qm-core: empty input", "", "", &[]),
            (
                "qm-core: inline code stays literal",
                "Use `[[react: eyes]]` to react.",
                "Use `[[react: eyes]]` to react.",
                &[],
            ),
            (
                "qm-core: a fenced block stays literal",
                "Example:\n```\n[[react: eyes white_check_mark]]\n```",
                "Example:\n```\n[[react: eyes white_check_mark]]\n```",
                &[],
            ),
            (
                "qm-core: code and a real directive",
                "done [[react: tada]] see `[[react: eyes]]`",
                "done  see `[[react: eyes]]`",
                &["tada"],
            ),
        ]);
    }

    #[test]
    fn targets_are_not_supported() {
        check(&[
            (
                "qm-core syntax: a timestamp target has no effect",
                "On it [[react: tada @ 1717360800.000100]] and [[react: eyes]]",
                "On it  and",
                &["eyes"],
            ),
            (
                "qm-core syntax: a short id target has no effect",
                "hmm [[react: saluting_face @ ~abc123]] ok",
                "hmm  ok",
                &[],
            ),
        ]);
    }

    #[test]
    fn names_are_normalized() {
        check(&[
            (
                "colons and case",
                "[[react: :Eyes: +1]]",
                "",
                &["eyes", "+1"],
            ),
            (
                "skin tones",
                "[[react: thumbsup::skin-tone-3 wave::skin-tone-9]]",
                "",
                &["thumbsup::skin-tone-3"],
            ),
            (
                "invalid names are dropped",
                "[[react: bad/name 👀 ok-name it's :: x::y]]",
                "",
                &["ok-name", "it's"],
            ),
            (
                "duplicates after normalizing",
                "[[react: eyes :eyes: EYES]] [[react: eyes]]",
                "",
                &["eyes"],
            ),
            (
                "at most five",
                "[[react: a b c d e f g]]",
                "",
                &["a", "b", "c", "d", "e"],
            ),
            ("the keyword ignores case", "[[REACT: tada]]", "", &["tada"]),
        ]);
    }

    #[test]
    fn whitespace_around_removed_directives() {
        check(&[
            (
                "at the end of a line",
                "On it. [[react: eyes]]\nNext line.",
                "On it.\nNext line.",
                &["eyes"],
            ),
            (
                "at the start of a line keeps its indent",
                "- item\n  [[react: eyes]]  more",
                "- item\n  more",
                &["eyes"],
            ),
            (
                "several on one line",
                "a\n[[react: x]] [[react: y]]\nb",
                "a\nb",
                &["x", "y"],
            ),
            (
                "at the end of a line after text, two of them",
                "ok [[react: x]] [[react: y]]\n\n\nnext",
                "ok\n\n\nnext",
                &["x", "y"],
            ),
            (
                "blank lines collapse only at the removal",
                "a\n\n[[react: x]]\n\nb\n\n\n\nc",
                "a\n\nb\n\n\n\nc",
                &["x"],
            ),
            (
                "leading blank lines go",
                "\n\n[[react: x]]\n\n  text",
                "  text",
                &["x"],
            ),
            (
                "CRLF line endings",
                "a\r\n[[react: x]]\r\nb",
                "a\r\nb",
                &["x"],
            ),
            (
                "code after a removal is untouched",
                "[[react: x]]\n```\n  a  \n\n\n\nb\n```",
                "```\n  a  \n\n\n\nb\n```",
                &["x"],
            ),
        ]);
    }

    #[test]
    fn brackets_that_are_not_directives() {
        check(&[
            (
                "one closing bracket",
                "[[react: eyes] and more",
                "[[react: eyes] and more",
                &[],
            ),
            (
                "an unclosed directive before more lines",
                "I can write [[react: like so\n\nand keep going",
                "I can write [[react: like so\n\nand keep going",
                &[],
            ),
            (
                "an unclosed directive in code",
                "see `[[react:` here",
                "see `[[react:` here",
                &[],
            ),
            (
                "a directive running into code",
                "[[react: `eyes`]] ok",
                "[[react: `eyes`]] ok",
                &[],
            ),
            (
                "other double brackets",
                "[[wiki link]]",
                "[[wiki link]]",
                &[],
            ),
            (
                "a later directive after an invalid one",
                "[[react: a] [[react: b]]",
                "[[react: a]",
                &["b"],
            ),
            (
                "after a directive inside code",
                "`[[react: a]]` [[react: b]]",
                "`[[react: a]]`",
                &["b"],
            ),
        ]);
    }

    #[test]
    fn many_directives_stay_linear() {
        let text = "[[react:".repeat(50_000);
        let (out, directives) = extract(&text);
        assert!(directives.is_empty());
        assert_eq!(out, "");
        let text = "[[react: a] ".repeat(50_000);
        assert_eq!(extract(&text).0, text);
        let text = "[[react:`x` ".repeat(50_000);
        assert_eq!(extract(&text).0, text);
        let text = format!("{}\nend", "[[react: x ".repeat(50_000));
        assert_eq!(extract(&text).0, text);
        let text = "[[react: eyes]] ".repeat(50_000);
        let (out, directives) = extract(&text);
        assert_eq!(out, "");
        assert_eq!(directives, react(&["eyes"]));
    }
}
