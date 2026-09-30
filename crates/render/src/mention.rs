//! The `@Name` grammar shared by the surface renderers, ported from qm-core's
//! `PLAIN_MENTION` in `src/slack/mrkdwn.ts`.
//!
//! A name is one to three words separated by single spaces. A word is one or
//! more dot-separated segments, and a segment starts and ends with a letter or
//! digit and may contain `_`, `'` and `-` in between.

use std::ops::Range;

use crate::MentionDirectory;

const MAX_WORDS: usize = 3;

/// A mention-shaped token found at an `@`.
pub(crate) struct Mention {
    /// Byte offset just past the text this token consumes: the resolved name,
    /// or the first word when nothing resolved. The words after it are left
    /// for the caller to scan, so a URL there is still found.
    pub(crate) end: usize,
    /// The platform id, when a candidate name resolved.
    pub(crate) id: Option<String>,
}

/// Scans a mention starting at the `@` at byte offset `at` of `text`.
///
/// Returns `None` when no mention can start there: the `@` follows a letter,
/// a digit, `<` or `@` (an email address or a wire token), no word follows it,
/// or the only word runs into `/` or `@` (a package scope or a path). Names
/// equal to one of `broadcasts` are skipped, ignoring ASCII case. When a
/// shorter name resolves but the next word is capitalized, the token names
/// somebody else and stays unresolved.
pub(crate) fn scan(
    text: &str,
    at: usize,
    broadcasts: &[&str],
    directory: &dyn MentionDirectory,
) -> Option<Mention> {
    if !text[at..].starts_with('@') || !boundary_before(text, at, &['<', '@']) {
        return None;
    }
    let start = at + 1;
    let mut words: Vec<Range<usize>> = Vec::new();
    let mut pos = start;
    while words.len() < MAX_WORDS {
        let from = match words.last() {
            None => pos,
            Some(_) if text[pos..].starts_with(' ') => pos + 1,
            Some(_) => break,
        };
        let Some(end) = word_end(text, from) else {
            break;
        };
        words.push(from..end);
        pos = end;
    }
    if matches!(text[pos..].chars().next(), Some('/' | '@')) {
        words.pop();
    }
    let end = words.first()?.end;
    for n in (1..=words.len()).rev() {
        let candidate = &text[start..words[n - 1].end];
        if broadcasts.iter().any(|b| candidate.eq_ignore_ascii_case(b)) {
            continue;
        }
        let Some(id) = directory.resolve(candidate) else {
            continue;
        };
        let next_capitalized = words
            .get(n)
            .and_then(|w| text[w.start..].chars().next())
            .is_some_and(char::is_uppercase);
        if next_capitalized {
            return Some(Mention { end, id: None });
        }
        return Some(Mention {
            end: words[n - 1].end,
            id: Some(id),
        });
    }
    Some(Mention { end, id: None })
}

/// Returns the end of a typed broadcast (`@here` and the like) starting at the
/// `@` at byte offset `at`, when the word after it equals one of `broadcasts`,
/// ignoring ASCII case, and the `@` doesn't follow a letter or digit.
pub(crate) fn broadcast(text: &str, at: usize, broadcasts: &[&str]) -> Option<usize> {
    if !text[at..].starts_with('@') || !boundary_before(text, at, &[]) {
        return None;
    }
    let rest = &text[at + 1..];
    let len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let word = &rest[..len];
    broadcasts
        .iter()
        .any(|b| word.eq_ignore_ascii_case(b))
        .then_some(at + 1 + len)
}

fn boundary_before(text: &str, at: usize, also: &[char]) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_alphanumeric() || also.contains(&c)))
}

fn word_end(text: &str, from: usize) -> Option<usize> {
    let mut end = segment_end(text, from)?;
    while text[end..].starts_with('.') {
        match segment_end(text, end + 1) {
            Some(next) => end = next,
            None => break,
        }
    }
    Some(end)
}

fn segment_end(text: &str, from: usize) -> Option<usize> {
    let mut chars = text[from..].char_indices();
    let (_, first) = chars.next()?;
    if !first.is_alphanumeric() {
        return None;
    }
    let mut end = from + first.len_utf8();
    for (offset, c) in chars {
        if c.is_alphanumeric() {
            end = from + offset + c.len_utf8();
        } else if !matches!(c, '_' | '\'' | '-') {
            break;
        }
    }
    Some(end)
}
