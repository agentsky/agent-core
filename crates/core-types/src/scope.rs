//! Scopes, and the volume keys built from them.
//!
//! A scope is where a conversation happens. It decides which volume a
//! session mounts and what the agent may touch. One volume exists per
//! `(agent, scope)`, named by a [`VolumeKey`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{AgentId, ConvKind, ConvRef, ParseError};

/// The kind of a [`ScopeKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// A DM with a member who is not the agent's owner. It runs on the
    /// public side.
    Dm,
    /// A channel.
    Channel,
    /// A group DM.
    GroupDm,
    /// The agent's private side: the owner's DMs with it and its private
    /// tasks.
    Private,
}

/// A scope, with a stable string form used to name volumes and locks.
///
/// | Scope | String form |
/// | --- | --- |
/// | `Dm(conv)` | `dm:<surface>:<team>:<conversation>` |
/// | `Channel(conv)` | `ch:<surface>:<team>:<conversation>` |
/// | `GroupDm(conv)` | `gdm:<surface>:<team>:<conversation>` |
/// | `Private` | `private` |
///
/// Team and conversation ids are escaped as the [`surface`](crate::surface)
/// module describes, so the string never contains `/` and is safe as one
/// path segment. `FromStr` accepts only what `Display` writes, so the two
/// round-trip both ways. The serde form is the string form.
///
/// A `Private` scope names no conversation. It is the same for every agent,
/// and a [`VolumeKey`] tells agents apart.
///
/// ```
/// use core_types::{ConvRef, ScopeKey, SurfaceKind};
///
/// let scope = ScopeKey::Channel(ConvRef {
///     surface: SurfaceKind::Slack,
///     team: "T012".into(),
///     conversation: "C345".into(),
/// });
/// assert_eq!(scope.to_string(), "ch:slack:T012:C345");
/// assert_eq!("ch:slack:T012:C345".parse(), Ok(scope));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeKey {
    /// A non-owner's DM with the agent.
    Dm(ConvRef),
    /// A channel.
    Channel(ConvRef),
    /// A group DM.
    GroupDm(ConvRef),
    /// The agent's private side.
    Private,
}

impl ScopeKey {
    /// The scope of a conversation of the given kind. The owner's DM is not
    /// this: the router gives it [`ScopeKey::Private`].
    pub fn for_conversation(kind: ConvKind, conv: ConvRef) -> Self {
        match kind {
            ConvKind::Dm => Self::Dm(conv),
            ConvKind::GroupDm => Self::GroupDm(conv),
            ConvKind::Channel => Self::Channel(conv),
        }
    }

    /// The scope's kind.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Dm(_) => ScopeKind::Dm,
            Self::Channel(_) => ScopeKind::Channel,
            Self::GroupDm(_) => ScopeKind::GroupDm,
            Self::Private => ScopeKind::Private,
        }
    }

    /// The conversation the scope names, or `None` for `Private`.
    pub fn conv(&self) -> Option<&ConvRef> {
        match self {
            Self::Dm(conv) | Self::Channel(conv) | Self::GroupDm(conv) => Some(conv),
            Self::Private => None,
        }
    }
}

const PRIVATE: &str = "private";

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prefix, conv) = match self {
            Self::Dm(conv) => ("dm", conv),
            Self::Channel(conv) => ("ch", conv),
            Self::GroupDm(conv) => ("gdm", conv),
            Self::Private => return f.write_str(PRIVATE),
        };
        write!(f, "{prefix}:{conv}")
    }
}

impl FromStr for ScopeKey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const WHAT: &str = "scope key";
        if s == PRIVATE {
            return Ok(Self::Private);
        }
        let Some((prefix, rest)) = s.split_once(':') else {
            return Err(ParseError::new(WHAT, "unknown prefix"));
        };
        let variant: fn(ConvRef) -> Self = match prefix {
            "dm" => Self::Dm,
            "ch" => Self::Channel,
            "gdm" => Self::GroupDm,
            _ => return Err(ParseError::new(WHAT, "unknown prefix")),
        };
        rest.parse()
            .map(variant)
            .map_err(|err: ParseError| ParseError::new(WHAT, err.reason()))
    }
}

crate::serde_as_string!(ScopeKey);

/// Names one volume: one per `(agent, scope)`.
///
/// Two agents in one channel never share a volume. The owner's DMs and the
/// agent's private tasks share the agent's `Private` volume.
///
/// Its string form is `<agent>/<scope>`, for example
/// `67e55044-10b1-426f-9247-bb680e5fe0c8/ch:slack:T012:C345`. It has exactly
/// one `/`, since a [`ScopeKey`] never contains one. `FromStr` accepts only
/// what `Display` writes. The serde form is the string form.
///
/// ```
/// use core_types::{AgentId, ScopeKey, VolumeKey};
///
/// let key = VolumeKey { agent: AgentId::new_v4(), scope: ScopeKey::Private };
/// assert_eq!(key.to_string(), format!("{}/private", key.agent));
/// assert_eq!(key.to_string().parse(), Ok(key));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeKey {
    /// The agent that owns the volume.
    pub agent: AgentId,
    /// The scope the volume serves.
    pub scope: ScopeKey,
}

impl fmt::Display for VolumeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.agent, self.scope)
    }
}

impl FromStr for VolumeKey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const WHAT: &str = "volume key";
        let (agent, scope) = s
            .split_once('/')
            .ok_or(ParseError::new(WHAT, "expected `<agent>/<scope>`"))?;
        Ok(Self {
            agent: agent
                .parse()
                .map_err(|_| ParseError::new(WHAT, "bad agent id"))?,
            scope: scope
                .parse()
                .map_err(|_| ParseError::new(WHAT, "bad scope key"))?,
        })
    }
}

crate::serde_as_string!(VolumeKey);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SurfaceKind;
    use crate::test_util::json_round_trip;

    fn conv(surface: SurfaceKind, team: &str, conversation: &str) -> ConvRef {
        ConvRef {
            surface,
            team: team.into(),
            conversation: conversation.into(),
        }
    }

    fn all_scopes() -> Vec<ScopeKey> {
        vec![
            ScopeKey::Dm(conv(SurfaceKind::Slack, "T012", "D345")),
            ScopeKey::Channel(conv(SurfaceKind::RocketChat, "chat.example.com", "GENERAL")),
            ScopeKey::GroupDm(conv(SurfaceKind::Slack, "T012", "G678")),
            ScopeKey::Channel(conv(SurfaceKind::RocketChat, "host:3000", "a/b%c")),
            ScopeKey::Dm(conv(SurfaceKind::Slack, "", "")),
            ScopeKey::Private,
        ]
    }

    #[test]
    fn scope_key_renders_the_documented_forms() {
        let rendered: Vec<String> = all_scopes().iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                "dm:slack:T012:D345",
                "ch:rocketchat:chat.example.com:GENERAL",
                "gdm:slack:T012:G678",
                "ch:rocketchat:host%3A3000:a%2Fb%25c",
                "dm:slack::",
                "private",
            ]
        );
    }

    #[test]
    fn scope_key_round_trips() {
        for scope in all_scopes() {
            let text = scope.to_string();
            assert!(!text.contains('/'), "{text}");
            assert_eq!(text.parse::<ScopeKey>(), Ok(scope.clone()));
            assert_eq!(json_round_trip(&scope), serde_json::json!(text));
        }
    }

    #[test]
    fn scope_key_rejects_malformed_strings() {
        for bad in [
            "",
            "Private",
            "private:slack:T1:C1",
            "channel:slack:T1:C1",
            "ch",
            "ch:",
            "ch:slack:T1",
            "ch:slack:T1:C1:x",
            "ch:teams:T1:C1",
            "ch:slack:T1:C/1",
            "ch:slack:T1:%2f",
        ] {
            let err = bad.parse::<ScopeKey>().unwrap_err();
            assert_eq!(err.what(), "scope key", "{bad}");
        }
        assert!(serde_json::from_str::<ScopeKey>("\"ch:slack\"").is_err());
    }

    #[test]
    fn scope_key_kind_and_conv() {
        let c = conv(SurfaceKind::Slack, "T1", "C1");
        for (kind, scope_kind) in [
            (ConvKind::Dm, ScopeKind::Dm),
            (ConvKind::GroupDm, ScopeKind::GroupDm),
            (ConvKind::Channel, ScopeKind::Channel),
        ] {
            let scope = ScopeKey::for_conversation(kind, c.clone());
            assert_eq!(scope.kind(), scope_kind);
            assert_eq!(scope.conv(), Some(&c));
        }
        assert_eq!(ScopeKey::Private.kind(), ScopeKind::Private);
        assert_eq!(ScopeKey::Private.conv(), None);
    }

    #[test]
    fn scope_kind_serde_round_trips() {
        for kind in [
            ScopeKind::Dm,
            ScopeKind::Channel,
            ScopeKind::GroupDm,
            ScopeKind::Private,
        ] {
            json_round_trip(&kind);
        }
        assert_eq!(
            json_round_trip(&ScopeKind::GroupDm),
            serde_json::json!("group_dm")
        );
    }

    #[test]
    fn volume_key_round_trips() {
        let agent = AgentId::new_v4();
        for scope in all_scopes() {
            let key = VolumeKey {
                agent,
                scope: scope.clone(),
            };
            let text = key.to_string();
            assert_eq!(text, format!("{agent}/{scope}"));
            assert_eq!(text.matches('/').count(), 1, "{text}");
            assert_eq!(text.parse::<VolumeKey>(), Ok(key.clone()));
            assert_eq!(json_round_trip(&key), serde_json::json!(text));
        }
    }

    #[test]
    fn two_agents_in_one_channel_get_two_volume_keys() {
        let scope = ScopeKey::Channel(conv(SurfaceKind::Slack, "T1", "C1"));
        let a = VolumeKey {
            agent: AgentId::new_v4(),
            scope: scope.clone(),
        };
        let b = VolumeKey {
            agent: AgentId::new_v4(),
            scope,
        };
        assert_ne!(a, b);
        assert_ne!(a.to_string(), b.to_string());
    }

    #[test]
    fn volume_key_rejects_malformed_strings() {
        let agent = AgentId::new_v4();
        for (bad, reason) in [
            ("private".to_owned(), "expected `<agent>/<scope>`"),
            ("not-a-uuid/private".to_owned(), "bad agent id"),
            (
                format!("{}/private", agent.to_string().to_uppercase()),
                "bad agent id",
            ),
            (format!("{agent}/"), "bad scope key"),
            (format!("{agent}/private/x"), "bad scope key"),
            (format!("{agent}/ch:slack:T1:C/1"), "bad scope key"),
        ] {
            let err = bad.parse::<VolumeKey>().unwrap_err();
            assert_eq!((err.what(), err.reason()), ("volume key", reason), "{bad}");
        }
    }
}
