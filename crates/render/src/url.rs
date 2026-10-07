//! Bare URL detection shared by the surface renderers, ported from
//! qm-core's `trimUrlTail` in `src/slack/mrkdwn.ts`.

/// Finds a bare `http://` or `https://` URL at byte offset `at`, trimmed of
/// trailing punctuation the way qm-core's `trimUrlTail` does, except that
/// a run of `*`, `_` or `~` stays when the URL holds the same mark earlier.
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
/// A trailing run of `*`, `_` or `~` is treated like such a bracket: it
/// stays only when the same mark appears earlier in the URL after its
/// scheme, as in `…#object.__init__` or `/~~a~~`, and is dropped otherwise,
/// as a footnote star, an escaped mark or a stray closer is, so
/// `(https://x.io/a).*` keeps `).*` out.
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
            if url.get(body..run).is_some_and(|before| before.contains(c)) {
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
