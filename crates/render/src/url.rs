//! Bare URL detection shared by the surface renderers, ported from
//! qm-core's `trimUrlTail` in `src/slack/mrkdwn.ts`.

/// Finds a bare `http://` or `https://` URL at byte offset `at`, trimmed of
/// trailing punctuation the way qm-core's `trimUrlTail` does, except that
/// a run of `*`, `_` or `~` right after an alphanumeric character stays when
/// the URL holds the same mark earlier, after its scheme.
pub(crate) fn bare_url(text: &str, at: usize) -> Option<&str> {
    let rest = &text[at..];
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|scheme| rest.starts_with(scheme))?;
    if text[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let len = rest.find(ends_url).unwrap_or(rest.len());
    let url = trim_url_tail(&rest[..len]);
    (url.len() > scheme.len()).then_some(url)
}

/// Whether `c` ends a bare URL.
pub(crate) fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '|')
}

/// Drops trailing punctuation, and closing brackets that have no opening
/// partner inside the URL, so `(see https://x.io/a).` keeps `)` and `.` out.
/// A trailing run of `*`, `_` or `~` stays only when it follows an
/// alphanumeric character (Unicode's) and the same mark appears earlier in
/// the URL after its scheme, as in `…#object.__init__` or `/~~a~~`.
/// Otherwise it is dropped, as a footnote star or a stray closer is, and
/// trimming goes on, so `(https://x.io/a).*` keeps `).*` out and
/// `(https://x.io/_a)_` keeps `)_` out. In decoded text it can't tell an
/// escaped mark; the Slack renderer ends a URL before an escaped mark in
/// that trailing run.
fn trim_url_tail(url: &str) -> &str {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let body = url.find("://").map_or(0, |at| at + 3);
    let mut unmatched = PAIRS.map(|(open, close)| {
        url.matches(close).count() as isize - url.matches(open).count() as isize
    });
    let mut end = url.len();
    while let Some(c) = url[..end].chars().next_back() {
        let next = if let Some(pair) = PAIRS.iter().position(|&(_, close)| close == c) {
            if unmatched[pair] <= 0 {
                break;
            }
            unmatched[pair] -= 1;
            end - c.len_utf8()
        } else if matches!(c, '*' | '_' | '~') {
            let run = url[..end].trim_end_matches(c).len();
            let closes_a_word = url[..run]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
            if closes_a_word && url.get(body..run).is_some_and(|before| before.contains(c)) {
                break;
            }
            run
        } else if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"') {
            end - c.len_utf8()
        } else {
            break;
        };
        end = next;
    }
    &url[..end]
}
