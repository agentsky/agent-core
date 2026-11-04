//! Who a turn runs for, on which credential, and with what reach.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ConsentId, MemberId, MemberKey};

/// The kind of credential a turn runs on. A warm `claude` process can't
/// switch kinds, so a change restarts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A member's Claude subscription, through its OAuth token.
    Subscription,
    /// An Anthropic API key: the community key.
    ApiKey,
}

/// Whose credential a turn runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRef {
    /// A linked member's subscription.
    Member(MemberId),
    /// The community API key an admin configured.
    Community,
}

impl CredentialRef {
    /// The kind of credential this is.
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Member(_) => CredentialKind::Subscription,
            Self::Community => CredentialKind::ApiKey,
        }
    }
}

/// Who caused a turn, and so who pays for it. An agent-to-agent hop inherits
/// the requester of the turn whose message mentioned the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Requester {
    /// The member, when the requester is linked or otherwise known to agentd.
    pub member: Option<MemberId>,
    /// The requester's identity on the surface.
    pub key: MemberKey,
}

/// How many agent-to-agent hops led to a turn. A turn a person started is
/// hop 0. Its serde form is a number.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Hop(pub u8);

impl Hop {
    /// Hop 0: a turn a person started.
    pub const ZERO: Self = Self(0);

    /// The hop after this one, or `None` if it would overflow.
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for Hop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// What kind of turn this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// A turn in a DM or a channel thread.
    Normal,
    /// A private task, run after the consent it names. agentctl offers only
    /// `attach` inside one.
    PrivateTask(ConsentId),
}

/// Which side of the agent a turn runs on. The router decides it, and
/// agentctl's target rules check it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The owner's DMs and owner-requested private tasks: what the owner
    /// granted.
    Owner,
    /// Every channel turn, the owner's included, since channel text is
    /// untrusted: persona, skills and thread context only.
    Public,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SurfaceKind;
    use crate::test_util::json_round_trip;

    #[test]
    fn credential_ref_kind() {
        assert_eq!(
            CredentialRef::Member(MemberId::new_v4()).kind(),
            CredentialKind::Subscription
        );
        assert_eq!(CredentialRef::Community.kind(), CredentialKind::ApiKey);
    }

    #[test]
    fn hop_next_counts_up_and_stops_at_the_top() {
        assert_eq!(Hop::ZERO.next(), Some(Hop(1)));
        assert_eq!(Hop(u8::MAX).next(), None);
        assert_eq!(Hop::default(), Hop::ZERO);
        assert_eq!(Hop(3).to_string(), "3");
    }

    #[test]
    fn turn_types_serde_round_trip() {
        for kind in [CredentialKind::Subscription, CredentialKind::ApiKey] {
            json_round_trip(&kind);
        }
        assert_eq!(
            json_round_trip(&CredentialKind::ApiKey),
            serde_json::json!("api_key")
        );

        let member = MemberId::new_v4();
        assert_eq!(
            json_round_trip(&CredentialRef::Member(member)),
            serde_json::json!({"member": member.to_string()})
        );
        assert_eq!(
            json_round_trip(&CredentialRef::Community),
            serde_json::json!("community")
        );

        let key = MemberKey {
            surface: SurfaceKind::RocketChat,
            team: "chat.example.com".into(),
            user: "u1".into(),
        };
        json_round_trip(&Requester {
            member: Some(member),
            key: key.clone(),
        });
        json_round_trip(&Requester { member: None, key });

        assert_eq!(json_round_trip(&Hop(2)), serde_json::json!(2));
        assert!(serde_json::from_str::<Hop>("256").is_err());

        let consent = ConsentId::new_v4();
        assert_eq!(
            json_round_trip(&TurnKind::Normal),
            serde_json::json!("normal")
        );
        assert_eq!(
            json_round_trip(&TurnKind::PrivateTask(consent)),
            serde_json::json!({"private_task": consent.to_string()})
        );

        assert_eq!(json_round_trip(&Side::Owner), serde_json::json!("owner"));
        assert_eq!(json_round_trip(&Side::Public), serde_json::json!("public"));
    }
}
