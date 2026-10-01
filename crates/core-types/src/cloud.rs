//! Cloud hand-off: [`RoutineId`], the id of a Claude Code routine whose API
//! trigger a member registered, and [`RoutineToken`], that trigger's token.

use std::fmt;
use std::str::FromStr;

use secrecy::{ExposeSecret, SecretString};

use crate::ParseError;

/// The prefix every routine id starts with.
const PREFIX: &str = "trig_";
/// The most ASCII letters and digits after [`PREFIX`].
const MAX_SUFFIX: usize = 64;

/// The id of a routine, as its API trigger's URL names it: `trig_` and 1 to
/// 64 ASCII letters and digits, such as `trig_01ABCDEF`.
///
/// It is the only part of a pasted fire URL agentd keeps; the URL is built
/// again from `[cloud] base_url` and this id for each request. The rule
/// keeps `/`, `.`, `%` and anything else that could change the request's
/// path out of it.
///
/// ```
/// use core_types::RoutineId;
///
/// let id: RoutineId = "trig_01ABCdef".parse().unwrap();
/// assert_eq!(id.as_str(), "trig_01ABCdef");
/// assert!("trig_".parse::<RoutineId>().is_err());
/// assert!("trig_a/b".parse::<RoutineId>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoutineId(String);

impl RoutineId {
    /// The id, `trig_` included.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RoutineId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.strip_prefix(PREFIX).is_some_and(|suffix| {
            (1..=MAX_SUFFIX).contains(&suffix.len())
                && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(ParseError::new(
                "routine id",
                "not trig_ and 1 to 64 ASCII letters and digits",
            ))
        }
    }
}

impl fmt::Display for RoutineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

crate::serde_as_string!(RoutineId);

/// The prefix every routine token starts with: routine tokens are
/// `sk-ant-oat01-…`, and only the family is required, in case the version
/// changes.
const TOKEN_PREFIX: &str = "sk-ant-";
/// The longest routine token accepted, in bytes.
const TOKEN_MAX: usize = 1024;

/// A routine's API trigger token: `sk-ant-` and printable ASCII, at most
/// 1024 bytes, so it can always go into an `Authorization` header.
///
/// It is checked once, by [`parse`](Self::parse), wherever it comes from:
/// the `cloud add` command, the store opening it, the fire request taking
/// it. `Debug` redacts it, it has no `Display` or serde form, and its
/// memory is wiped on drop.
///
/// ```
/// use core_types::RoutineToken;
/// use secrecy::{ExposeSecret, SecretString};
///
/// let token = RoutineToken::parse(SecretString::from("sk-ant-oat01-abc")).unwrap();
/// assert_eq!(token.expose_secret(), "sk-ant-oat01-abc");
/// assert!(!format!("{token:?}").contains("abc"));
/// assert!(RoutineToken::parse(SecretString::from("abc")).is_err());
/// ```
#[derive(Clone)]
pub struct RoutineToken(SecretString);

impl RoutineToken {
    /// Checks `token` against the rule, without repeating it in the error.
    ///
    /// # Errors
    ///
    /// A [`ParseError`] if it doesn't start with `sk-ant-`, holds anything
    /// but printable ASCII, or is longer than 1024 bytes.
    pub fn parse(token: SecretString) -> Result<Self, ParseError> {
        let text = token.expose_secret();
        let valid = text.starts_with(TOKEN_PREFIX)
            && text.len() <= TOKEN_MAX
            && text.bytes().all(|b| b.is_ascii_graphic());
        if valid {
            Ok(Self(token))
        } else {
            Err(ParseError::new(
                "routine token",
                "not sk-ant- and at most 1024 printable ASCII characters",
            ))
        }
    }

    /// The token as a [`SecretString`], for sealing.
    pub fn as_secret(&self) -> &SecretString {
        &self.0
    }
}

impl ExposeSecret<str> for RoutineToken {
    fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for RoutineToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutineToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::json_round_trip;

    #[test]
    fn routine_ids_are_trig_and_ascii_letters_and_digits() {
        let longest = format!("trig_{}", "a".repeat(64));
        for ok in ["trig_1", "trig_01ABCdef", longest.as_str()] {
            let id: RoutineId = ok.parse().unwrap();
            assert_eq!(id.as_str(), ok);
            assert_eq!(id.to_string(), ok);
        }
        let too_long = format!("trig_{}", "a".repeat(65));
        for bad in [
            "",
            "trig_",
            "trig",
            "TRIG_1",
            "trg_1",
            "1",
            "trig_a-b",
            "trig_a_b",
            "trig_a.b",
            "trig_a/b",
            "trig_a%2e",
            "trig_é",
            "trig_a ",
            " trig_a",
            too_long.as_str(),
        ] {
            let err = bad.parse::<RoutineId>().unwrap_err();
            assert_eq!(err.what(), "routine id", "{bad:?}");
        }
    }

    #[test]
    fn routine_tokens_are_sk_ant_and_printable_ascii() {
        let longest = format!("sk-ant-{}", "a".repeat(TOKEN_MAX - 7));
        for ok in ["sk-ant-oat01-Ab_9-x", "sk-ant-", longest.as_str()] {
            let token = RoutineToken::parse(SecretString::from(ok)).unwrap();
            assert_eq!(token.expose_secret(), ok);
            assert_eq!(token.as_secret().expose_secret(), ok);
            assert_eq!(format!("{token:?}"), "RoutineToken([REDACTED])");
        }
        let too_long = format!("{longest}a");
        for bad in [
            "",
            "t",
            "SK-ANT-oat01-x",
            "xoxb-1-x",
            "sk-ant-oat01-x\u{7}",
            "sk-ant-oat01-é",
            "sk-ant-oat01 x",
            too_long.as_str(),
        ] {
            let err = RoutineToken::parse(SecretString::from(bad)).unwrap_err();
            assert_eq!(err.what(), "routine token", "{bad:?}");
            assert_eq!(
                err.reason(),
                "not sk-ant- and at most 1024 printable ASCII characters"
            );
        }
    }

    #[test]
    fn serde_uses_the_string_form() {
        let id: RoutineId = "trig_42".parse().unwrap();
        assert_eq!(json_round_trip(&id), serde_json::json!("trig_42"));
        assert!(serde_json::from_str::<RoutineId>("\"trig_a/b\"").is_err());
    }
}
