//! Chat-platform identifiers and references.
//!
//! Every identity and conversation carries its surface and team, never a bare
//! user or channel id, so the same agent on two platforms, or in a Slack
//! Connect channel seen from two workspaces, is never confused.
//!
//! [`MemberKey`] and [`ConvRef`] have a string form,
//! `<surface>:<team>:<id>`, used in keys such as [`ScopeKey`](crate::ScopeKey).
//! Platform ids are opaque, so inside it `%`, `:` and `/` are written as
//! `%25`, `%3A` and `%2F`. The ids Slack and Rocket.Chat use today contain
//! none of them and appear unchanged. Parsing accepts only the form `Display`
//! writes, so a key string that parses always renders back to itself.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ParseError;

/// A chat platform.
///
/// Its string form, in `Display`, `FromStr` and serde, is `slack` or
/// `rocketchat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SurfaceKind {
    /// Slack.
    Slack,
    /// Rocket.Chat.
    RocketChat,
}

impl SurfaceKind {
    /// The string form: `slack` or `rocketchat`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::RocketChat => "rocketchat",
        }
    }
}

impl fmt::Display for SurfaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SurfaceKind {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "slack" => Ok(Self::Slack),
            "rocketchat" => Ok(Self::RocketChat),
            _ => Err(ParseError::new("surface", "unknown surface")),
        }
    }
}

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        ///
        /// An opaque platform id. Its serde form is a plain string.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps a platform id.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// A workspace: a Slack team id (`T…`), or the id agentd gives a
    /// Rocket.Chat server.
    TeamId
);
string_id!(
    /// A user or bot user: a Slack user id (`U…`) or a Rocket.Chat user
    /// `_id`.
    UserId
);
string_id!(
    /// A conversation: a Slack channel id (`C…`, `G…`, `D…`) or a
    /// Rocket.Chat room id.
    ConversationId
);
string_id!(
    /// A message: a Slack message `ts` or a Rocket.Chat message `_id`. A
    /// Slack `ts` is unique only within its channel, so a message is named by
    /// a [`MsgRef`], not by this alone.
    MessageId
);

/// A member's identity on one surface: `(surface, team, user)`.
///
/// Its string form is `<surface>:<team>:<user>` (see the [module
/// docs](self) for escaping). Its serde form is a struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemberKey {
    /// The platform.
    pub surface: SurfaceKind,
    /// The workspace.
    pub team: TeamId,
    /// The user within the workspace.
    pub user: UserId,
}

impl fmt::Display for MemberKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_triple(f, self.surface, self.team.as_str(), self.user.as_str())
    }
}

impl FromStr for MemberKey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (surface, team, user) = parse_triple(s, "member key")?;
        Ok(Self {
            surface,
            team: TeamId(team),
            user: UserId(user),
        })
    }
}

/// A conversation on one surface: `(surface, team, conversation)`.
///
/// Its string form is `<surface>:<team>:<conversation>` (see the [module
/// docs](self) for escaping). Its serde form is a struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConvRef {
    /// The platform.
    pub surface: SurfaceKind,
    /// The workspace.
    pub team: TeamId,
    /// The conversation within the workspace.
    pub conversation: ConversationId,
}

impl fmt::Display for ConvRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_triple(
            f,
            self.surface,
            self.team.as_str(),
            self.conversation.as_str(),
        )
    }
}

impl FromStr for ConvRef {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (surface, team, conversation) = parse_triple(s, "conversation")?;
        Ok(Self {
            surface,
            team: TeamId(team),
            conversation: ConversationId(conversation),
        })
    }
}

/// What kind of conversation a message arrived in, as the surface reports
/// it.
///
/// The router turns it into a [`ScopeKind`](crate::ScopeKind); the owner's
/// own DM becomes the agent's `Private` scope there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvKind {
    /// A direct message between one member and the bot.
    Dm,
    /// A direct message with several members.
    GroupDm,
    /// A public or private channel.
    Channel,
}

/// A thread: the key a session is looked up by, together with the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    /// The conversation.
    pub conv: ConvRef,
    /// The thread's root message. `None` means the conversation itself, which
    /// is how a DM's one continuous session is keyed.
    pub root: Option<MessageId>,
}

/// Where a reply goes: a conversation, and a thread in it if any.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplyTarget {
    /// The conversation.
    pub conv: ConvRef,
    /// The root of the thread to reply in. `None` posts to the conversation
    /// itself.
    pub thread_root: Option<MessageId>,
}

impl From<ThreadKey> for ReplyTarget {
    fn from(thread: ThreadKey) -> Self {
        Self {
            conv: thread.conv,
            thread_root: thread.root,
        }
    }
}

/// One message on a surface: its conversation and its id there.
///
/// This is what editing, reacting and attribution need. A Slack `ts` is
/// unique only within a channel, so the conversation is always part of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MsgRef {
    /// The conversation the message is in.
    pub conv: ConvRef,
    /// The message's id in that conversation.
    pub id: MessageId,
}

/// A position in a conversation's history, for paging backwards with
/// [`Surface::history`](crate::Surface::history).
///
/// It holds a message id: `history` returns messages older than that message.
/// Its serde form is a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    /// Wraps a cursor value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The cursor as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<MessageId> for Cursor {
    fn from(id: MessageId) -> Self {
        Self(id.0)
    }
}

/// Writes one segment of a key string, escaping `%`, `:` and `/`.
fn write_segment(f: &mut fmt::Formatter<'_>, segment: &str) -> fmt::Result {
    let mut rest = segment;
    while let Some(at) = rest.find(['%', ':', '/']) {
        f.write_str(&rest[..at])?;
        f.write_str(match rest.as_bytes()[at] {
            b'%' => "%25",
            b':' => "%3A",
            _ => "%2F",
        })?;
        rest = &rest[at + 1..];
    }
    f.write_str(rest)
}

/// Reads one segment written by [`write_segment`]. Anything `write_segment`
/// would not have written, such as a raw `/`, another `%` escape or a
/// lowercase one, is refused.
fn parse_segment(segment: &str, what: &'static str) -> Result<String, ParseError> {
    let mut out = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(at) = rest.find(['%', ':', '/']) {
        out.push_str(&rest[..at]);
        if rest.as_bytes()[at] != b'%' {
            return Err(ParseError::new(what, "unescaped separator"));
        }
        let escape = rest.get(at..at + 3);
        out.push(match escape {
            Some("%25") => '%',
            Some("%3A") => ':',
            Some("%2F") => '/',
            _ => return Err(ParseError::new(what, "bad escape")),
        });
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Writes `<surface>:<a>:<b>`, escaping `a` and `b`.
fn write_triple(f: &mut fmt::Formatter<'_>, surface: SurfaceKind, a: &str, b: &str) -> fmt::Result {
    f.write_str(surface.as_str())?;
    f.write_str(":")?;
    write_segment(f, a)?;
    f.write_str(":")?;
    write_segment(f, b)
}

/// Parses `<surface>:<a>:<b>` as written by [`write_triple`].
fn parse_triple(s: &str, what: &'static str) -> Result<(SurfaceKind, String, String), ParseError> {
    let mut parts = s.split(':');
    let (Some(surface), Some(a), Some(b), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::new(what, "expected three `:`-separated parts"));
    };
    let surface = surface
        .parse()
        .map_err(|_| ParseError::new(what, "unknown surface"))?;
    Ok((surface, parse_segment(a, what)?, parse_segment(b, what)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::json_round_trip;

    fn conv(team: &str, conversation: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: team.into(),
            conversation: conversation.into(),
        }
    }

    #[test]
    fn surface_kind_string_forms_agree() {
        for kind in [SurfaceKind::Slack, SurfaceKind::RocketChat] {
            assert_eq!(kind.to_string().parse::<SurfaceKind>(), Ok(kind));
            assert_eq!(json_round_trip(&kind), serde_json::json!(kind.as_str()));
        }
        assert_eq!(SurfaceKind::RocketChat.to_string(), "rocketchat");
        assert!("Slack".parse::<SurfaceKind>().is_err());
    }

    #[test]
    fn string_ids_convert_and_serialize_as_plain_strings() {
        let team = TeamId::new("T012");
        assert_eq!(team.as_str(), "T012");
        assert_eq!(team.to_string(), "T012");
        assert_eq!(team.as_ref(), "T012");
        assert_eq!(TeamId::from(String::from("T012")), team);
        assert_eq!(json_round_trip(&team), serde_json::json!("T012"));
        assert_eq!(
            json_round_trip(&UserId::from("U1")),
            serde_json::json!("U1")
        );
        assert_eq!(
            json_round_trip(&ConversationId::from("C1")),
            serde_json::json!("C1")
        );
        assert_eq!(
            json_round_trip(&MessageId::from("1700000000.000100")),
            serde_json::json!("1700000000.000100")
        );
    }

    #[test]
    fn member_key_string_form_round_trips() {
        let key = MemberKey {
            surface: SurfaceKind::RocketChat,
            team: "chat.example.com".into(),
            user: "aBc123".into(),
        };
        assert_eq!(key.to_string(), "rocketchat:chat.example.com:aBc123");
        assert_eq!(key.to_string().parse::<MemberKey>(), Ok(key));
    }

    #[test]
    fn separators_in_ids_are_escaped_and_round_trip() {
        let c = conv("chat.example.com:3000", "a/b%c:d");
        let text = c.to_string();
        assert_eq!(text, "slack:chat.example.com%3A3000:a%2Fb%25c%3Ad");
        assert_eq!(text.parse::<ConvRef>(), Ok(c));
        let empty = conv("", "");
        assert_eq!(empty.to_string(), "slack::");
        assert_eq!("slack::".parse::<ConvRef>(), Ok(empty));
    }

    #[test]
    fn key_parse_accepts_only_what_display_writes() {
        for bad in [
            "slack:T1",
            "slack:T1:C1:extra",
            "teams:T1:C1",
            "slack:T1:a/b",
            "slack:T1:%3a",
            "slack:T1:%41",
            "slack:T1:%",
            "slack:T1:%3",
        ] {
            assert!(bad.parse::<ConvRef>().is_err(), "{bad}");
            assert!(bad.parse::<MemberKey>().is_err(), "{bad}");
        }
        let err = "teams:T1:U1".parse::<MemberKey>().unwrap_err();
        assert_eq!(err.what(), "member key");
        assert_eq!(err.reason(), "unknown surface");
    }

    #[test]
    fn references_serde_round_trip() {
        let c = conv("T1", "C1");
        assert_eq!(
            json_round_trip(&c),
            serde_json::json!({"surface": "slack", "team": "T1", "conversation": "C1"})
        );
        json_round_trip(&MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: "U1".into(),
        });
        json_round_trip(&ThreadKey {
            conv: c.clone(),
            root: None,
        });
        json_round_trip(&ReplyTarget {
            conv: c.clone(),
            thread_root: Some("1.2".into()),
        });
        json_round_trip(&MsgRef {
            conv: c,
            id: "1.2".into(),
        });
        for kind in [ConvKind::Dm, ConvKind::GroupDm, ConvKind::Channel] {
            json_round_trip(&kind);
        }
        assert_eq!(
            json_round_trip(&ConvKind::GroupDm),
            serde_json::json!("group_dm")
        );
        assert_eq!(
            json_round_trip(&Cursor::new("1.2")),
            serde_json::json!("1.2")
        );
    }

    #[test]
    fn thread_key_becomes_a_reply_target() {
        let thread = ThreadKey {
            conv: conv("T1", "C1"),
            root: Some("1.2".into()),
        };
        let target = ReplyTarget::from(thread.clone());
        assert_eq!(target.conv, thread.conv);
        assert_eq!(target.thread_root, thread.root);
    }

    #[test]
    fn cursor_from_message_id() {
        let cursor = Cursor::from(MessageId::new("1.2"));
        assert_eq!(cursor.as_str(), "1.2");
    }
}
