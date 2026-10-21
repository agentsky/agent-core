//! `cloud add`, `cloud run`, `cloud list` and `cloud rm`: a member's routines
//! for cloud hand-off, and the arguments they take.

use std::fmt;
use std::str::FromStr;

use core_types::{RoutineId, RoutineToken};
use secrecy::SecretString;

use crate::ParseError;
use crate::names::{Reason, unwrap_slack_link};

/// `cloud add`, `cloud run`, `cloud list` and `cloud rm`.
///
/// `Debug` redacts the token and leaves out the task, which no log carries.
#[derive(Clone)]
pub enum CloudCommand {
    /// `cloud add <routine> <url> <token>`: register a routine's API trigger
    /// under a label, or replace the label's routine and token.
    /// Secret-bearing.
    Add {
        /// The member's label for the routine.
        label: RoutineLabel,
        /// What agentd keeps of the pasted fire URL.
        routine: RoutineUrl,
        /// The API trigger's token.
        token: RoutineToken,
    },
    /// `cloud run <routine> <task>`: fire the routine with the task.
    Run {
        /// The routine's label.
        label: RoutineLabel,
        /// The task, the rest of the text verbatim. Never logged.
        task: String,
    },
    /// `cloud list`: the member's routines and recent hand-offs.
    List,
    /// `cloud rm <routine>`: forget a routine and its token.
    Rm {
        /// The routine's label.
        label: RoutineLabel,
    },
}

impl fmt::Debug for CloudCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add {
                label,
                routine,
                token,
            } => f
                .debug_struct("Add")
                .field("label", label)
                .field("routine", routine)
                .field("token", token)
                .finish(),
            Self::Run { label, .. } => f
                .debug_struct("Run")
                .field("label", label)
                .finish_non_exhaustive(),
            Self::List => f.write_str("List"),
            Self::Rm { label } => f.debug_struct("Rm").field("label", label).finish(),
        }
    }
}

const ROUTINE_LABEL_RULE: Reason = Reason(
    "A routine's label is 1 to 64 characters, each an ASCII letter, a digit or ._/-, \
     starting with a letter or a digit.",
);

/// A member's label for one of their routines: 1 to 64 ASCII letters,
/// digits and `._/-`, starting with a letter or a digit, so a repository's
/// `owner/name` is one.
///
/// Labels are case-sensitive.
///
/// ```
/// use commands::RoutineLabel;
///
/// let label: RoutineLabel = "agentsky/agent-core".parse().unwrap();
/// assert_eq!(label.as_str(), "agentsky/agent-core");
/// assert!("-x".parse::<RoutineLabel>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoutineLabel(String);

impl RoutineLabel {
    /// The label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RoutineLabel {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(parse_routine_label(s)?)
    }
}

impl fmt::Display for RoutineLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) fn parse_routine_label(s: &str) -> Result<RoutineLabel, Reason> {
    let valid = (1..=64).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    if valid {
        Ok(RoutineLabel(s.to_owned()))
    } else {
        Err(ROUTINE_LABEL_RULE)
    }
}

const ROUTINE_TOKEN_RULE: Reason =
    Reason("A routine's token is the sk-ant-… token its API trigger showed when you generated it.");

/// Parses a routine's API trigger token into a [`RoutineToken`], whose rule
/// is the one place a token is checked. The reason never repeats it.
pub(crate) fn parse_routine_token(s: &str) -> Result<RoutineToken, Reason> {
    RoutineToken::parse(SecretString::from(s)).map_err(|_| ROUTINE_TOKEN_RULE)
}

const ROUTINE_URL_RULE: Reason = Reason(
    "A routine's URL is its API trigger's URL from claude.ai/code/routines, \
     https://api.anthropic.com/v1/claude_code/routines/trig_…/fire, \
     with nothing before or after it.",
);

const FIRE_PATH_PREFIX: &str = "/v1/claude_code/routines/";
const FIRE_PATH_SUFFIX: &str = "/fire";

/// What agentd keeps of a routine's pasted fire URL: its origin and the
/// routine's id. The URL itself is dropped, and agentd builds the request's
/// URL again from `[cloud] base_url` and the id.
///
/// The URL's scheme is `https` or `http`, it has a host and an optional
/// port and no user info, query or fragment, and its path is exactly
/// `/v1/claude_code/routines/trig_<id>/fire`, with an id of 1 to 64 ASCII
/// letters and digits. The path is matched in the text as pasted, before
/// the URL is parsed, since parsing would remove `.` and `..` segments and
/// decode `%` escapes; any `%`, and anything but printable ASCII, is
/// refused anywhere in it. Slack's `<…>` around a pasted link is taken off
/// first.
///
/// ```
/// use commands::RoutineUrl;
///
/// let url: RoutineUrl = "https://api.anthropic.com/v1/claude_code/routines/trig_01AB/fire"
///     .parse()
///     .unwrap();
/// assert_eq!(url.routine_id().as_str(), "trig_01AB");
/// assert_eq!(url.origin().ascii_serialization(), "https://api.anthropic.com");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineUrl {
    origin: url::Origin,
    routine_id: RoutineId,
}

impl RoutineUrl {
    /// The URL's origin, which agentd compares with `[cloud] base_url`'s.
    pub fn origin(&self) -> &url::Origin {
        &self.origin
    }

    /// The routine's id.
    pub fn routine_id(&self) -> &RoutineId {
        &self.routine_id
    }
}

impl FromStr for RoutineUrl {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(parse_routine_url(s)?)
    }
}

pub(crate) fn parse_routine_url(s: &str) -> Result<RoutineUrl, Reason> {
    let text = unwrap_slack_link(s);
    if !text.bytes().all(|b| b.is_ascii_graphic() && b != b'%') {
        return Err(ROUTINE_URL_RULE);
    }
    let (scheme, rest) = text.split_once("://").ok_or(ROUTINE_URL_RULE)?;
    if !["https", "http"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    {
        return Err(ROUTINE_URL_RULE);
    }
    let path_start = rest.find(['/', '\\', '?', '#']).ok_or(ROUTINE_URL_RULE)?;
    let (authority, path) = rest.split_at(path_start);
    if authority.is_empty() || authority.contains('@') {
        return Err(ROUTINE_URL_RULE);
    }
    let routine_id: RoutineId = path
        .strip_prefix(FIRE_PATH_PREFIX)
        .and_then(|rest| rest.strip_suffix(FIRE_PATH_SUFFIX))
        .and_then(|id| id.parse().ok())
        .ok_or(ROUTINE_URL_RULE)?;
    let url = url::Url::parse(text).map_err(|_| ROUTINE_URL_RULE)?;
    let plain = matches!(url.scheme(), "https" | "http")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == path;
    if !plain {
        return Err(ROUTINE_URL_RULE);
    }
    Ok(RoutineUrl {
        origin: url.origin(),
        routine_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRE: &str = "https://api.anthropic.com/v1/claude_code/routines/trig_01AB/fire";

    #[test]
    fn routine_label_grammar() {
        let longest = "a".repeat(64);
        for ok in [
            "a",
            "Z",
            "7",
            "agent-core",
            "agentsky/agent-core",
            "docs.site_v2",
            "a/b/c",
            "x-",
            "x.",
            longest.as_str(),
        ] {
            let label = parse_routine_label(ok).unwrap();
            assert_eq!(label.as_str(), ok);
            assert_eq!(label.to_string(), ok);
        }
        let too_long = "a".repeat(65);
        for bad in [
            "",
            "-x",
            ".x",
            "_x",
            "/x",
            "a b",
            "a:b",
            "a@b",
            "a#b",
            "a\\b",
            "a%2f",
            "é",
            "a\u{200b}",
            too_long.as_str(),
        ] {
            assert_eq!(parse_routine_label(bad), Err(ROUTINE_LABEL_RULE), "{bad:?}");
        }
        let err = "-SECRET".parse::<RoutineLabel>().unwrap_err();
        assert_eq!(err.to_string(), ROUTINE_LABEL_RULE.0);
        assert!(!err.to_string().contains("SECRET"));
    }

    #[test]
    fn a_routine_url_keeps_its_origin_and_routine_id() {
        let url = parse_routine_url(FIRE).unwrap();
        assert_eq!(url.routine_id().as_str(), "trig_01AB");
        assert_eq!(
            url.origin(),
            &url::Url::parse("https://api.anthropic.com")
                .unwrap()
                .origin()
        );
        assert_eq!(url, FIRE.parse::<RoutineUrl>().unwrap());

        let local =
            parse_routine_url("http://127.0.0.1:8080/v1/claude_code/routines/trig_x/fire").unwrap();
        assert_eq!(
            local.origin().ascii_serialization(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(local.routine_id().as_str(), "trig_x");

        let default_port =
            parse_routine_url("HTTPS://API.anthropic.com:443/v1/claude_code/routines/trig_1/fire")
                .unwrap();
        assert_eq!(default_port.origin(), url.origin());

        let v6 = parse_routine_url("http://[::1]:9/v1/claude_code/routines/trig_1/fire").unwrap();
        assert_eq!(v6.origin().ascii_serialization(), "http://[::1]:9");

        let longest = format!("trig_{}", "Z".repeat(64));
        let long = parse_routine_url(&format!(
            "https://h.example/v1/claude_code/routines/{longest}/fire"
        ))
        .unwrap();
        assert_eq!(long.routine_id().as_str(), longest);
    }

    #[test]
    fn a_slack_link_around_the_url_is_taken_off() {
        for wrapped in [
            format!("<{FIRE}>"),
            format!("<{FIRE}|{}>", &FIRE["https://".len()..]),
        ] {
            assert_eq!(
                parse_routine_url(&wrapped).unwrap(),
                parse_routine_url(FIRE).unwrap(),
                "{wrapped}"
            );
        }
        for bad in [format!("<{FIRE}"), format!("{FIRE}>"), format!("<@{FIRE}>")] {
            assert_eq!(parse_routine_url(&bad), Err(ROUTINE_URL_RULE), "{bad}");
        }
    }

    #[test]
    fn routine_url_must_be_the_fire_endpoint() {
        let path = "/v1/claude_code/routines/trig_1/fire";
        let too_long = format!(
            "https://api.anthropic.com/v1/claude_code/routines/trig_{}/fire",
            "a".repeat(65)
        );
        for bad in [
            format!("https://user@api.anthropic.com{path}"),
            format!("https://user:pw@api.anthropic.com{path}"),
            format!("https://@api.anthropic.com{path}"),
            format!("https://api.anthropic.com{path}?x=1"),
            format!("https://api.anthropic.com{path}?"),
            format!("https://api.anthropic.com{path}#x"),
            format!("https://api.anthropic.com{path}#"),
            "https://api.anthropic.com/v1/claude_code/./routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/./trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/./fire".to_owned(),
            "https://api.anthropic.com/v1/x/../claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_2/../trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/fire/..".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/x/../fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/%2e%2e/trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/%2e/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_%31/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/fir%65".to_owned(),
            "https://%61pi.anthropic.com/v1/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/routine_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/TRIG_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_a-b/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_a_b/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_a.b/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_é/fire".to_owned(),
            too_long,
            format!("https://api.anthropic.com{path}/"),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/fire//".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1/run".to_owned(),
            "https://api.anthropic.com/v2/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com//v1/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/V1/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/api/v1/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com\\v1\\claude_code\\routines\\trig_1\\fire".to_owned(),
            "https://api.anthropic.com\\v1/claude_code/routines/trig_1/fire".to_owned(),
            "https://api.anthropic.com/v1/claude_code/routines/trig_1\\fire".to_owned(),
            "https://api.anthropic.com".to_owned(),
            "https://api.anthropic.com/".to_owned(),
            format!("https://{path}"),
            format!("https:///api.anthropic.com{path}"),
            format!("ftp://api.anthropic.com{path}"),
            format!("file://api.anthropic.com{path}"),
            format!("javascript://api.anthropic.com{path}"),
            format!("api.anthropic.com{path}"),
            format!("//api.anthropic.com{path}"),
            path.to_owned(),
            format!("https://api.anthropic.com:99999{path}"),
            format!("https://api.anthropic.com:x{path}"),
            format!("https://exa mple.com{path}"),
            format!("https://ex\u{0}ample.com{path}"),
            format!("https://exämple.com{path}"),
            format!("https://[::1{path}"),
            String::new(),
        ] {
            assert_eq!(parse_routine_url(&bad), Err(ROUTINE_URL_RULE), "{bad:?}");
        }
    }

    #[test]
    fn routine_url_errors_never_repeat_the_text() {
        let err = "https://SECRET@x.io/v1/claude_code/routines/trig_1/fire"
            .parse::<RoutineUrl>()
            .unwrap_err();
        assert_eq!(err.to_string(), ROUTINE_URL_RULE.0);
        assert!(!format!("{err} {err:?}").contains("SECRET"));
    }

    #[test]
    fn debug_leaves_out_the_token_and_the_task() {
        let add = CloudCommand::Add {
            label: parse_routine_label("r").unwrap(),
            routine: parse_routine_url(FIRE).unwrap(),
            token: RoutineToken::parse(SecretString::from("sk-ant-oat01-SECRET")).unwrap(),
        };
        let run = CloudCommand::Run {
            label: parse_routine_label("r").unwrap(),
            task: "TASK-TEXT".to_owned(),
        };
        let rm = CloudCommand::Rm {
            label: parse_routine_label("r").unwrap(),
        };
        let debug = format!(
            "{add:?} {add:#?} {run:?} {run:#?} {:?} {rm:?}",
            CloudCommand::List
        );
        assert!(!debug.contains("SECRET"), "{debug}");
        assert!(!debug.contains("TASK"), "{debug}");
        assert!(debug.contains("REDACTED"), "{debug}");
        assert!(debug.contains("trig_01AB"), "{debug}");
        assert!(debug.contains("List"), "{debug}");
        assert!(debug.contains("Rm"), "{debug}");
    }
}
