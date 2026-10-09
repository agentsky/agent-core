//! Validated names and the member, channel and audience references that
//! commands take as arguments.

use std::fmt;
use std::str::FromStr;

use crate::{ParseError, ParseErrorKind};

/// Why one argument failed to parse. The text never repeats the argument,
/// which may be anything a member typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Reason(pub(crate) &'static str);

impl From<Reason> for ParseError {
    fn from(reason: Reason) -> Self {
        ParseError::new(ParseErrorKind::Invalid, reason.0.to_owned())
    }
}

const AGENT_NAME_RULE: Reason = Reason("An agent name is 2 to 32 characters, each a-z, 0-9 or -.");
const SKILL_NAME_RULE: Reason = Reason("A skill name is 1 to 64 characters, each a-z, 0-9 or -.");

fn is_name_char(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '-')
}

/// An agent's name: 2 to 32 characters, each `a-z`, `0-9` or `-`.
///
/// Names are case-sensitive and never folded, so `Bob` is refused rather
/// than quietly becoming `bob`.
///
/// ```
/// use commands::AgentName;
///
/// let name: AgentName = "code-helper".parse().unwrap();
/// assert_eq!(name.as_str(), "code-helper");
/// assert!("Bob".parse::<AgentName>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentName(String);

impl AgentName {
    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AgentName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(parse_agent_name(s)?)
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) fn parse_agent_name(s: &str) -> Result<AgentName, Reason> {
    if (2..=32).contains(&s.len()) && s.chars().all(is_name_char) {
        Ok(AgentName(s.to_owned()))
    } else {
        Err(AGENT_NAME_RULE)
    }
}

/// A skill's name: 1 to 64 characters, each `a-z`, `0-9` or `-`, the rule
/// Claude Code applies to a skill's `name`.
///
/// The name becomes a directory under the agent's skills, so the rule also
/// keeps `/` and `..` out of that path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SkillName(String);

impl SkillName {
    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(parse_skill_name(s)?)
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) fn parse_skill_name(s: &str) -> Result<SkillName, Reason> {
    if (1..=64).contains(&s.len()) && s.chars().all(is_name_char) {
        Ok(SkillName(s.to_owned()))
    } else {
        Err(SKILL_NAME_RULE)
    }
}

/// A member named in a command.
///
/// Members type `@name`. Slack rewrites a mention it recognizes into a
/// `<@U123>` or `<@U123|name>` token, which becomes [`UserRef::Id`]; the
/// label after `|` is dropped, since only the id is stable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserRef {
    /// A username as typed, without the `@`. The handler resolves it on the
    /// surface the command came from.
    Name(String),
    /// A platform user id, from a Slack `<@…>` token.
    Id(String),
}

/// A channel named in a command.
///
/// Members type `#name`. Slack rewrites a channel it recognizes into a
/// `<#C123|name>` or `<#C123>` token, which becomes [`RoomRef::Id`]. On
/// Rocket.Chat such a token is only ever typed, so its id is looked up
/// like a name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RoomRef {
    /// A channel name as typed, without the `#`.
    Name(String),
    /// A platform channel id, from a Slack `<#…>` token.
    Id {
        /// The id.
        id: String,
        /// The channel's name the token carries after `|`, for replies to
        /// show, if it has a plain one.
        name: Option<String>,
    },
}

impl RoomRef {
    /// How a reply shows the channel: its name, or its id without one.
    pub fn shown(&self) -> &str {
        match self {
            Self::Name(name)
            | Self::Id {
                name: Some(name), ..
            }
            | Self::Id { id: name, .. } => name,
        }
    }
}

/// Who `allow` and `deny` apply to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// One member: `@name` or a Slack `<@…>` token.
    Member(UserRef),
    /// Everyone in one channel: `#name` or a Slack `<#…>` token.
    Room(RoomRef),
    /// Everyone: `everyone`, `@everyone` or Slack's `<!everyone>`.
    Everyone,
}

const TARGET_RULE: Reason = Reason("A target is @member, #channel or everyone.");
const USER_RULE: Reason = Reason("A member is written @name.");

pub(crate) fn parse_target(s: &str) -> Result<Target, Reason> {
    if s.eq_ignore_ascii_case("everyone")
        || s.eq_ignore_ascii_case("@everyone")
        || s.eq_ignore_ascii_case("<!everyone>")
    {
        return Ok(Target::Everyone);
    }
    if let Some(user) = user_ref(s) {
        return user.map(Target::Member);
    }
    if let Some(room) = room_ref(s) {
        return room.map(Target::Room);
    }
    Err(TARGET_RULE)
}

pub(crate) fn parse_user(s: &str) -> Result<UserRef, Reason> {
    user_ref(s).unwrap_or(Err(USER_RULE))
}

/// Parses `@name` or `<@ID>` / `<@ID|label>`. `None` means `s` isn't
/// written as a member at all.
fn user_ref(s: &str) -> Option<Result<UserRef, Reason>> {
    if let Some(id) = slack_token(s, '@') {
        return Some(id.map(UserRef::Id).ok_or(USER_RULE));
    }
    let name = s.strip_prefix('@')?;
    Some(plain_name(name).map(UserRef::Name).ok_or(USER_RULE))
}

fn room_ref(s: &str) -> Option<Result<RoomRef, Reason>> {
    const ROOM_RULE: Reason = Reason("A channel is written #name.");
    if let Some(id) = slack_token(s, '#') {
        let name = s
            .trim_end_matches('>')
            .split_once('|')
            .and_then(|(_, name)| plain_name(name));
        return Some(id.map(|id| RoomRef::Id { id, name }).ok_or(ROOM_RULE));
    }
    let name = s.strip_prefix('#')?;
    Some(plain_name(name).map(RoomRef::Name).ok_or(ROOM_RULE))
}

/// Reads a Slack `<{sigil}ID>` or `<{sigil}ID|label>` token. The outer
/// `None` means `s` isn't such a token; the inner `None` means it is one,
/// but its id isn't a Slack id (uppercase letters and digits).
fn slack_token(s: &str, sigil: char) -> Option<Option<String>> {
    let body = s
        .strip_prefix('<')?
        .strip_suffix('>')?
        .strip_prefix(sigil)?;
    let id = body.split_once('|').map_or(body, |(id, _label)| id);
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    Some(valid.then(|| id.to_owned()))
}

/// A username or channel name after its sigil: not empty, and free of the
/// characters that mark sigils and Slack tokens.
fn plain_name(name: &str) -> Option<String> {
    let valid = !name.is_empty()
        && !name
            .chars()
            .any(|c| matches!(c, '@' | '#' | '<' | '>' | '|') || c.is_control());
    valid.then(|| name.to_owned())
}

const SKILL_SOURCE_RULE: Reason = Reason(
    "A skill source is an https:// Git URL, optionally ending in #ref. \
     Leave it out to add a SKILL.md or .zip attached to a direct message with me.",
);

/// The longest skill source accepted, in bytes.
const SKILL_SOURCE_MAX: usize = 2048;

/// Parses a `skill add` source: an `https://` Git URL, optionally ending
/// in `#ref`, given as typed or as a Slack link token.
///
/// agentd passes the source to `git clone`, so the rule is deliberately
/// narrow. The scheme must be `https://`, which keeps out `-` option
/// look-alikes, `ext::` and `file://` transports, and SSH forms. The host
/// is letters, digits, `.` and `-`, with an optional numeric port, so
/// credentials (`user:token@host`) are refused rather than stored. The path
/// is letters, digits and `-._~/%+`. A ref starts with a letter or digit,
/// continues with letters, digits and `._/-`, and has no `..` or `//` and no
/// trailing `/` or `.`, which also rules out a ref that `git` would read as
/// an option.
pub(crate) fn parse_skill_source(s: &str) -> Result<String, Reason> {
    let url = unwrap_slack_link(s);
    if url.len() > SKILL_SOURCE_MAX {
        return Err(SKILL_SOURCE_RULE);
    }
    let rest = url.strip_prefix("https://").ok_or(SKILL_SOURCE_RULE)?;
    let (location, git_ref) = match rest.split_once('#') {
        Some((location, git_ref)) => (location, Some(git_ref)),
        None => (rest, None),
    };
    let (authority, path) = location
        .find('/')
        .map_or((location, ""), |i| location.split_at(i));
    let valid = is_git_host(authority)
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~/%+".contains(c))
        && git_ref.is_none_or(is_git_ref);
    if valid {
        Ok(url.to_owned())
    } else {
        Err(SKILL_SOURCE_RULE)
    }
}

fn is_git_host(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    host.starts_with(|c: char| c.is_ascii_alphanumeric())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && port.is_none_or(|port| {
            (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
        })
}

fn is_git_ref(git_ref: &str) -> bool {
    git_ref.starts_with(|c: char| c.is_ascii_alphanumeric())
        && git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        && !git_ref.contains("..")
        && !git_ref.contains("//")
        && !git_ref.ends_with(['/', '.'])
}

/// Unwraps a Slack link token, `<url>` or `<url|label>`, to its URL. Slack
/// wraps links this way in message text and in slash command text. Other
/// text is returned unchanged.
fn unwrap_slack_link(s: &str) -> &str {
    match s.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        Some(body) if !body.starts_with(['@', '#', '!']) => {
            body.split_once('|').map_or(body, |(url, _label)| url)
        }
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, name: Option<&str>) -> RoomRef {
        RoomRef::Id {
            id: id.into(),
            name: name.map(Into::into),
        }
    }

    #[test]
    fn agent_names_follow_the_rule() {
        for ok in ["ab", "code-helper", "a1", "--", &"x".repeat(32)] {
            assert_eq!(parse_agent_name(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            "a",
            &"x".repeat(33),
            "Bob",
            "bob_1",
            "bob.",
            "bø",
            "a b",
        ] {
            assert_eq!(parse_agent_name(bad), Err(AGENT_NAME_RULE), "{bad:?}");
        }
    }

    #[test]
    fn agent_name_from_str_and_display() {
        let name: AgentName = "helper".parse().unwrap();
        assert_eq!(name.to_string(), "helper");
        let err = "NO".parse::<AgentName>().unwrap_err();
        assert_eq!(err.kind(), ParseErrorKind::Invalid);
        assert_eq!(err.to_string(), AGENT_NAME_RULE.0);
        assert!(!err.to_string().contains("NO"));
    }

    #[test]
    fn skill_names_follow_the_rule() {
        let name: SkillName = "x".parse().unwrap();
        assert_eq!(name.as_str(), "x");
        assert_eq!(name.to_string(), "x");
        assert!(parse_skill_name(&"x".repeat(64)).is_ok());
        for bad in ["", &"x".repeat(65), "..", "a/b", "A"] {
            assert_eq!(parse_skill_name(bad), Err(SKILL_NAME_RULE), "{bad:?}");
        }
        assert!("../etc".parse::<SkillName>().is_err());
    }

    #[test]
    fn targets_accept_raw_and_slack_forms() {
        let cases = [
            ("@bob", Target::Member(UserRef::Name("bob".into()))),
            ("<@U123>", Target::Member(UserRef::Id("U123".into()))),
            ("<@W9|bob>", Target::Member(UserRef::Id("W9".into()))),
            ("#general", Target::Room(RoomRef::Name("general".into()))),
            ("<#C42|general>", Target::Room(room("C42", Some("general")))),
            ("<#G7|>", Target::Room(room("G7", None))),
            ("<#C42>", Target::Room(room("C42", None))),
            ("everyone", Target::Everyone),
            ("Everyone", Target::Everyone),
            ("@everyone", Target::Everyone),
            ("<!everyone>", Target::Everyone),
        ];
        for (text, want) in cases {
            assert_eq!(parse_target(text), Ok(want), "{text}");
        }
    }

    #[test]
    fn targets_reject_malformed_forms() {
        for bad in ["bob", "", "<!here>", "<!channel>", "<https://x.io>", "all"] {
            assert_eq!(parse_target(bad), Err(TARGET_RULE), "{bad:?}");
        }
        for bad in ["@", "@a@b", "<@>", "<@u123>", "<@U 1>", "@a|b"] {
            assert_eq!(parse_target(bad), Err(USER_RULE), "{bad:?}");
        }
        for bad in ["#", "#a#b", "<#>", "<#c1|x>", "#a\u{7}"] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn users_reject_rooms_and_everyone() {
        assert_eq!(parse_user("@bob"), Ok(UserRef::Name("bob".into())));
        assert_eq!(parse_user("<@U1|bob>"), Ok(UserRef::Id("U1".into())));
        for bad in ["bob", "#general", "<#C1>", "everyone", "<@lower>"] {
            assert_eq!(parse_user(bad), Err(USER_RULE), "{bad:?}");
        }
    }

    #[test]
    fn skill_sources_accept_https_git_urls_with_an_optional_ref() {
        for ok in [
            "https://github.com/o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r.git#v1.2.0",
            "https://github.com/o/r#release/2026-09",
            "https://github.com/o/r#0123abcdef",
            "https://git.example.org:8443/~team/skills_repo%20x+y.git#main",
            "https://example.org",
        ] {
            assert_eq!(parse_skill_source(ok).as_deref(), Ok(ok), "{ok}");
        }
        assert_eq!(
            parse_skill_source("<https://g.it/a.git#v1|g.it/a.git#v1>").as_deref(),
            Ok("https://g.it/a.git#v1")
        );
    }

    #[test]
    fn skill_sources_that_git_could_read_as_options_are_refused() {
        for bad in [
            "--upload-pack=touch /tmp/pwned",
            "-uhttps://github.com/o/r",
            "--config=core.sshCommand=x",
            "https://github.com/o/r#--upload-pack=x",
            "https://github.com/o/r#-b",
        ] {
            assert_eq!(parse_skill_source(bad), Err(SKILL_SOURCE_RULE), "{bad}");
        }
    }

    #[test]
    fn skill_sources_other_than_https_are_refused() {
        for bad in [
            "",
            "http://github.com/o/r",
            "HTTPS://github.com/o/r",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r",
            "git://github.com/o/r",
            "file:///etc",
            "/srv/repo",
            "../repo",
            "ext::sh -c touch% /tmp/pwned",
            "github.com/o/r",
            "<@U1>",
        ] {
            assert_eq!(parse_skill_source(bad), Err(SKILL_SOURCE_RULE), "{bad:?}");
        }
    }

    #[test]
    fn skill_sources_with_a_malformed_host_path_or_ref_are_refused() {
        let long = format!("https://g.it/{}", "a".repeat(SKILL_SOURCE_MAX));
        for bad in [
            "https://",
            "https:///o/r",
            "https://user:token@github.com/o/r",
            "https://-x.org/o/r",
            "https://g.it:/o/r",
            "https://g.it:123456/o/r",
            "https://g.it:80a/o/r",
            "https://g.it/o/r?x=1",
            "https://g.it/o/r;x",
            "https://g.it/o/r$(id)",
            "https://g.it/o/ré",
            "https://g.it/o/r#",
            "https://g.it/o/r#a..b",
            "https://g.it/o/r#a//b",
            "https://g.it/o/r#a/",
            "https://g.it/o/r#a.",
            "https://g.it/o/r#.a",
            "https://g.it/o/r#a@{1}",
            "https://g.it/o/r#a#b",
            "https://g.it/o/r#a b",
            &long,
        ] {
            assert_eq!(parse_skill_source(bad), Err(SKILL_SOURCE_RULE), "{bad:?}");
        }
    }

    #[test]
    fn slack_links_unwrap_to_their_url() {
        assert_eq!(
            unwrap_slack_link("<https://g.it/a.git>"),
            "https://g.it/a.git"
        );
        assert_eq!(
            unwrap_slack_link("<https://g.it/a.git#v1|g.it/a.git#v1>"),
            "https://g.it/a.git#v1"
        );
        for other in ["https://g.it/a", "<@U1>", "<#C1|x>", "<!here>", "<x", "x>"] {
            assert_eq!(unwrap_slack_link(other), other);
        }
    }
}
