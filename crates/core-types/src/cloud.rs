//! Cloud hand-off: [`RoutineId`], the id of a Claude Code routine whose API
//! trigger a member registered.

use std::fmt;
use std::str::FromStr;

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
    fn serde_uses_the_string_form() {
        let id: RoutineId = "trig_42".parse().unwrap();
        assert_eq!(json_round_trip(&id), serde_json::json!("trig_42"));
        assert!(serde_json::from_str::<RoutineId>("\"trig_a/b\"").is_err());
    }
}
