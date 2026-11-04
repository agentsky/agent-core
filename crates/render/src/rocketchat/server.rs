//! A port of the Rocket.Chat server's mention extraction, for tests.
//!
//! `MentionsParser.getUserMentions` in `app/mentions/lib/MentionsParser.ts`
//! (the same in 7.10.0 and 8.0.0) removes `[label](dest)` links from the raw
//! message text with `/\[[^\]]*\]\([^)]+\)/g`, then matches
//! `(^|\s|>)@(P(@(P))?(:([0-9a-zA-Z-_.]+))?)` with flags `gm`, where `P`
//! is the `UTF8_User_Names_Validation` setting, `[0-9a-zA-Z-_.]+` by
//! default.
//! `MentionsServer.getUsersByMentions` in `app/mentions/server/Mentions.ts`
//! treats a match as a broadcast when the name is `all` or `here`. It
//! ignores code, and the port does too.

/// The names the server extracts from `msg` that could be broadcasts: those
/// whose first run of name characters is `all` or `here`, ignoring case.
///
/// This is stricter than the server, which compares the whole name
/// case-sensitively. The text is checked both with and without the link
/// removal, so a link hiding a broadcast from the server still counts.
/// The names come back sorted, without duplicates.
pub(crate) fn broadcasts(msg: &str) -> Vec<String> {
    let mut names = mentions(msg);
    names.extend(mentions(&remove_links(msg)));
    names.retain(|name| {
        let run = name.split(|c: char| !is_name_char(c)).next();
        run.is_some_and(|run| ["all", "here"].iter().any(|b| run.eq_ignore_ascii_case(b)))
    });
    names.sort();
    names.dedup();
    names
}

/// `(^|\s|>)@(P(@(P))?(:([0-9a-zA-Z-_.]+))?)` with flags `gm`: the text of
/// group 2 for each match.
fn mentions(msg: &str) -> Vec<String> {
    let chars: Vec<char> = msg.chars().collect();
    let mut names = Vec::new();
    let mut p = 0;
    while p < chars.len() {
        let line_start = p == 0 || is_line_terminator(chars[p - 1]);
        let prefixed = is_js_space(chars[p]) || chars[p] == '>';
        let at = [line_start.then_some(p), prefixed.then_some(p + 1)]
            .into_iter()
            .flatten()
            .find_map(|at| name_end(&chars, at).map(|end| (at, end)));
        match at {
            Some((at, end)) => {
                names.push(chars[at + 1..end].iter().collect());
                p = end;
            }
            None => p += 1,
        }
    }
    names
}

/// Where `@(P(@(P))?(:P)?)` ends when it matches at `at`.
fn name_end(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at) != Some(&'@') {
        return None;
    }
    let mut end = run_end(chars, at + 1)?;
    for sep in ['@', ':'] {
        if chars.get(end) == Some(&sep)
            && let Some(next) = run_end(chars, end + 1)
        {
            end = next;
        }
    }
    Some(end)
}

fn run_end(chars: &[char], from: usize) -> Option<usize> {
    let len = chars
        .get(from..)?
        .iter()
        .take_while(|&&c| is_name_char(c))
        .count();
    (len > 0).then_some(from + len)
}

/// `msg.replace(/\[[^\]]*\]\([^)]+\)/g, '')`.
fn remove_links(msg: &str) -> String {
    let chars: Vec<char> = msg.chars().collect();
    let mut out = String::new();
    let mut p = 0;
    while p < chars.len() {
        if let Some(end) = link_end(&chars, p) {
            p = end;
        } else {
            out.push(chars[p]);
            p += 1;
        }
    }
    out
}

fn link_end(chars: &[char], at: usize) -> Option<usize> {
    if chars[at] != '[' {
        return None;
    }
    let close = at + 1 + chars[at + 1..].iter().position(|&c| c == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let dest = close + 2;
    let paren = dest + chars.get(dest..)?.iter().position(|&c| c == ')')?;
    (paren > dest).then_some(paren + 1)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// JavaScript's `\s`, plus anything else Unicode calls white space.
fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

#[test]
fn the_port_finds_what_the_server_finds() {
    let cases: &[(&str, &[&str])] = &[
        ("@all", &["all"]),
        ("hi @here now", &["here"]),
        ("a\n@all", &["all"]),
        (">@all", &["all"]),
        ("\u{3000}@all", &["all"]),
        ("x@all", &[]),
        ("@\u{200B}all", &[]),
        ("@allé", &["all"]),
        ("@ALL", &["ALL"]),
        ("@all.hands", &[]),
        ("@a@all", &[]),
        ("@all@server", &["all@server"]),
        ("@here:srv", &["here:srv"]),
        ("[x](y)@all", &["all"]),
        ("\n[x](y)@all", &["all"]),
        ("[@all](y)", &[]),
        ("`@all`", &[]),
        ("` @all`", &["all"]),
    ];
    for (msg, expected) in cases {
        assert_eq!(&broadcasts(msg), expected, "{msg:?}");
    }
}
