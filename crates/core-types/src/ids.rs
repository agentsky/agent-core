//! UUID newtypes for the things agentd mints: members, agents, sessions,
//! turns, consents, bindings, scope-lock leases, and cloud hand-off routines
//! and hand-offs.
//!
//! Each type has one string form, the lowercase hyphenated UUID
//! (`67e55044-10b1-426f-9247-bb680e5fe0c8`). `Display` writes it, and
//! `FromStr` and serde accept only it, so a string that parses always
//! renders back to itself and one ID never has two spellings in a key.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

use crate::ParseError;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// Mints a new random (version 4) ID.
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an existing UUID.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The UUID inside.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_canonical_uuid(s, $what).map(Self)
            }
        }

        crate::serde_as_string!($name);
    };
}

fn parse_canonical_uuid(s: &str, what: &'static str) -> Result<Uuid, ParseError> {
    let uuid = Uuid::try_parse(s).map_err(|_| ParseError::new(what, "not a UUID"))?;
    let mut buf = Uuid::encode_buffer();
    if uuid.hyphenated().encode_lower(&mut buf) == s {
        Ok(uuid)
    } else {
        Err(ParseError::new(what, "not in lowercase hyphenated form"))
    }
}

uuid_id!(
    /// A member: one person, who may have identities on several surfaces.
    MemberId,
    "member id"
);
uuid_id!(
    /// An agent: a persona owned by one member.
    AgentId,
    "agent id"
);
uuid_id!(
    /// A Claude Code session. Passed to the CLI as `--session-id` or
    /// `--resume`, so a new one is always minted with [`SessionId::new_v4`].
    SessionId,
    "session id"
);
uuid_id!(
    /// One turn: one user message fed to a session and the reply it produces.
    TurnId,
    "turn id"
);
uuid_id!(
    /// A consent request for a private task.
    ConsentId,
    "consent id"
);
uuid_id!(
    /// A binding: one chat identity (a bot user) through which an agent, or
    /// the manager bot, is present on a surface.
    BindingId,
    "binding id"
);
uuid_id!(
    /// One grant of a scope's `shared/` lock, picked by agentctl for each
    /// `agentctl lock` and sent with each of its acquires. Renew and release
    /// name it, so two `agentctl lock` runs in one session never share a
    /// lease.
    LeaseId,
    "lease id"
);
uuid_id!(
    /// A routine a member registered for cloud hand-off: one
    /// `cloud_routines` row.
    CloudRoutineId,
    "cloud routine id"
);
uuid_id!(
    /// One cloud hand-off: a `cloud run` that fired, or tried to fire, a
    /// member's routine.
    CloudHandoffId,
    "cloud hand-off id"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::json_round_trip;

    const CANONICAL: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

    #[test]
    fn display_and_parse_round_trip() {
        let id = AgentId::new_v4();
        assert_eq!(id.to_string().parse::<AgentId>(), Ok(id));
        let parsed: SessionId = CANONICAL.parse().unwrap();
        assert_eq!(parsed.to_string(), CANONICAL);
    }

    #[test]
    fn new_v4_mints_distinct_version_4_ids() {
        let a = SessionId::new_v4();
        let b = SessionId::new_v4();
        assert_ne!(a, b);
        assert_eq!(a.as_uuid().get_version_num(), 4);
    }

    #[test]
    fn parse_rejects_garbage() {
        let err = "not-a-uuid".parse::<MemberId>().unwrap_err();
        assert_eq!(err.what(), "member id");
        assert_eq!(err.reason(), "not a UUID");
        assert!("".parse::<TurnId>().is_err());
    }

    #[test]
    fn parse_rejects_non_canonical_spellings() {
        for spelling in [
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
            "{67e55044-10b1-426f-9247-bb680e5fe0c8}",
            "urn:uuid:67e55044-10b1-426f-9247-bb680e5fe0c8",
        ] {
            let err = spelling.parse::<ConsentId>().unwrap_err();
            assert_eq!(
                err.reason(),
                "not in lowercase hyphenated form",
                "{spelling}"
            );
        }
    }

    #[test]
    fn uuid_conversions() {
        let uuid = Uuid::new_v4();
        let id = BindingId::from_uuid(uuid);
        assert_eq!(*id.as_uuid(), uuid);
        assert_eq!(BindingId::from(uuid), id);
        assert_eq!(Uuid::from(id), uuid);
    }

    #[test]
    fn serde_uses_the_string_form() {
        let id: MemberId = CANONICAL.parse().unwrap();
        assert_eq!(json_round_trip(&id), serde_json::json!(CANONICAL));
        let agent = AgentId::new_v4();
        assert_eq!(
            json_round_trip(&agent),
            serde_json::json!(agent.to_string())
        );
        let session = SessionId::new_v4();
        assert_eq!(
            json_round_trip(&session),
            serde_json::json!(session.to_string())
        );
        let turn = TurnId::new_v4();
        assert_eq!(json_round_trip(&turn), serde_json::json!(turn.to_string()));
        let consent = ConsentId::new_v4();
        assert_eq!(
            json_round_trip(&consent),
            serde_json::json!(consent.to_string())
        );
        let binding = BindingId::new_v4();
        assert_eq!(
            json_round_trip(&binding),
            serde_json::json!(binding.to_string())
        );
        let lease = LeaseId::new_v4();
        assert_eq!(
            json_round_trip(&lease),
            serde_json::json!(lease.to_string())
        );
        let routine = CloudRoutineId::new_v4();
        assert_eq!(
            json_round_trip(&routine),
            serde_json::json!(routine.to_string())
        );
        let handoff = CloudHandoffId::new_v4();
        assert_eq!(
            json_round_trip(&handoff),
            serde_json::json!(handoff.to_string())
        );
    }

    #[test]
    fn serde_rejects_non_canonical_and_non_strings() {
        assert!(
            serde_json::from_str::<AgentId>("\"67E55044-10B1-426F-9247-BB680E5FE0C8\"").is_err()
        );
        assert!(serde_json::from_str::<AgentId>("42").is_err());
    }
}
