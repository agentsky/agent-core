//! Bare URL detection shared by the surface renderers, ported from
//! qm-core's `trimUrlTail` in `src/slack/mrkdwn.ts`.

/// Finds a bare `http://` or `https://` URL at byte offset `at`, trimmed of
/// trailing punctuation the way qm-core's `trimUrlTail` does.
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
pub(crate) fn trim_url_tail(url: &str) -> &str {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let mut unmatched = PAIRS.map(|(open, close)| {
        url.matches(close).count() as isize - url.matches(open).count() as isize
    });
    let mut end = url.len();
    while let Some(c) = url[..end].chars().next_back() {
        let drop = match PAIRS.iter().position(|&(_, close)| close == c) {
            Some(pair) if unmatched[pair] > 0 => {
                unmatched[pair] -= 1;
                true
            }
            Some(_) => false,
            None => matches!(
                c,
                '*' | '_' | '~' | '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"'
            ),
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &url[..end]
}
