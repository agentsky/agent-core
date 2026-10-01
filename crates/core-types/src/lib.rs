//! IDs, keys, events, the `Surface` trait and agentctl wire types for
//! agent-core.
//!
//! This crate does no I/O. It holds the types every other crate agrees on:
//!
//! - [`ids`]: UUID newtypes such as [`AgentId`] and [`SessionId`].
//! - [`cloud`]: [`RoutineId`], the id of a member's cloud hand-off routine.
//! - [`surface`]: chat-platform identifiers and references such as
//!   [`MemberKey`], [`ConvRef`] and [`MsgRef`].
//! - [`scope`]: [`ScopeKey`] and [`VolumeKey`], with stable string forms.
//! - [`event`]: [`InboundEvent`], what a surface hands the shared core.
//! - [`turn`]: who a turn runs for and on which credential.
//! - [`surface_trait`]: the [`Surface`] trait and the types it uses.
//! - [`ctl`]: request and response types of the agentctl API.
//! - [`net`]: [`Cidr`] subnets.
//! - [`throttle`]: [`Throttle`], which lets a repeated log line through
//!   once per interval.
//!
//! Every item is re-exported at the crate root, except
//! [`surface_trait::Result`], which would shadow `std`'s.

#![warn(missing_docs)]

pub mod cloud;
pub mod ctl;
pub mod event;
pub mod ids;
pub mod net;
pub mod scope;
pub mod surface;
pub mod surface_trait;
pub mod throttle;
pub mod turn;

pub use cloud::*;
pub use ctl::*;
pub use event::*;
pub use ids::*;
pub use net::*;
pub use scope::*;
pub use surface::*;
pub use surface_trait::{
    Binding, Caps, InFile, LengthUnit, Limit, Msg, OutFile, Posted, SendError, Sender, Sink,
    Surface, SurfaceError,
};
pub use throttle::Throttle;
pub use turn::*;

/// The error returned when a string form (an ID, a key, a surface name) fails
/// to parse.
///
/// It names what was being parsed and why it failed, but never repeats the
/// input, which may be anything a user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what}: {reason}")]
pub struct ParseError {
    what: &'static str,
    reason: &'static str,
}

impl ParseError {
    pub(crate) const fn new(what: &'static str, reason: &'static str) -> Self {
        Self { what, reason }
    }

    /// What was being parsed, for example `"scope key"`.
    pub fn what(&self) -> &'static str {
        self.what
    }

    /// Why it failed, for example `"unknown prefix"`.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Implements `Serialize` and `Deserialize` through a type's `Display` and
/// `FromStr`, so its serde form is exactly its string form.
macro_rules! serde_as_string {
    ($ty:ty) => {
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <String as serde::Deserialize>::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}
pub(crate) use serde_as_string;

#[cfg(test)]
pub(crate) mod test_util {
    use std::fmt::Debug;

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// Serializes `value` to JSON and back, checks the result is equal, and
    /// returns the JSON.
    pub(crate) fn json_round_trip<T>(value: &T) -> serde_json::Value
    where
        T: Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, value);
        json
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_names_what_and_why_without_the_input() {
        let err = ParseError::new("scope key", "unknown prefix");
        assert_eq!(err.what(), "scope key");
        assert_eq!(err.reason(), "unknown prefix");
        assert_eq!(err.to_string(), "invalid scope key: unknown prefix");
    }
}
