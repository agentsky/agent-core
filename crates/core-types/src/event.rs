//! [`InboundEvent`]: a chat message, normalized by its surface.

use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{BindingId, ConvKind, ConvRef, InFile, MemberKey, MessageId, MsgRef, Outside, UserId};

/// The most mentions an [`InboundEvent`] carries: a surface keeps the first
/// this many different users a message mentions. The router looks each one
/// up, and a message can hold thousands.
pub const MAX_MENTIONS: usize = 100;

/// One inbound chat message, normalized by its surface. Everything after
/// this is shared between surfaces.
///
/// Surfaces don't decide whether an agent answers. They fill in what the
/// platform said, and the router gates on it.
///
/// # Bot senders
///
/// When `sender_is_bot` is true, the router looks the sender up as a
/// managed agent by [`sender`](Self::sender), the same [`MemberKey`] every
/// binding is keyed by. So a surface puts the bot's user id in
/// `sender.user` whenever it knows one, and the same id in
/// `sender_bot_user`; the two never disagree. On Rocket.Chat that is the
/// message's `u._id`. On Slack it is the event's `user` field, or, for a
/// bot message that carries only a `bot_id`, the `user_id` that `bots.info`
/// returns for it.
///
/// A bot with no known user id (a Slack `bot_id` that `bots.info` maps to
/// no user, as for legacy integrations) has `sender_bot_user: None` and its
/// bot id (`B…`) in `sender.user`. No binding has that id, so the router
/// treats the sender as an unmanaged bot and ignores it.
///
/// Its `Debug` output shows the text's length, never the text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundEvent {
    /// The platform's id for this delivery, used to drop duplicates: a Slack
    /// `event_id` or a Rocket.Chat message `_id`.
    pub event_id: String,
    /// The binding whose connection or app received the event.
    pub binding: BindingId,
    /// Who sent the message. For a bot, its user id when known, or else its
    /// bot id; see [Bot senders](#bot-senders).
    pub sender: MemberKey,
    /// Whether the sender is from outside the workspace agentd serves, and
    /// from which organization; `None` for a member of the workspace, and
    /// always on Rocket.Chat. On Slack it is what the message's own team
    /// fields say, and for Slack's copy of it also what a lookup said; the
    /// pipeline acts only on a copy whose `outside` is the event's. A bot's
    /// says nothing, since the router never takes a bot for a requester.
    #[serde(default)]
    pub outside: Option<Outside>,
    /// Whether the sender is a bot, managed by agentd or not.
    pub sender_is_bot: bool,
    /// The sender's bot user id, when the sender is a bot and its user id is
    /// known: the same id as `sender.user`. `None` for a human, and for a
    /// bot known only by its bot id.
    pub sender_bot_user: Option<UserId>,
    /// The conversation the message is in.
    pub conv: ConvRef,
    /// What kind of conversation that is.
    pub conv_kind: ConvKind,
    /// The root of the thread the message is in, or `None` at the top level
    /// of the conversation.
    pub thread_root: Option<MessageId>,
    /// The message itself.
    pub message: MsgRef,
    /// The message text as the platform sent it.
    pub text: String,
    /// Every user the message mentions, in order, each once, at most
    /// [`MAX_MENTIONS`].
    pub mentions: Vec<UserId>,
    /// The message this one replies to, if any. For a thread reply this is
    /// the thread root, so the router can check whether the agent posted it.
    pub reply_to: Option<MsgRef>,
    /// Files attached to the message.
    pub files: Vec<InFile>,
    /// When agentd received the event.
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
}

impl fmt::Debug for InboundEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundEvent")
            .field("event_id", &self.event_id)
            .field("binding", &self.binding)
            .field("sender", &self.sender)
            .field("outside", &self.outside)
            .field("sender_is_bot", &self.sender_is_bot)
            .field("sender_bot_user", &self.sender_bot_user)
            .field("conv", &self.conv)
            .field("conv_kind", &self.conv_kind)
            .field("thread_root", &self.thread_root)
            .field("message", &self.message)
            .field("text_len", &self.text.len())
            .field("mentions", &self.mentions)
            .field("reply_to", &self.reply_to)
            .field("files", &self.files)
            .field("received_at", &self.received_at)
            .finish()
    }
}

impl InboundEvent {
    /// Whether the message is in a one-to-one DM.
    pub fn is_dm(&self) -> bool {
        self.conv_kind == ConvKind::Dm
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::SurfaceKind;
    use crate::test_util::json_round_trip;

    fn sample_event(conv_kind: ConvKind) -> InboundEvent {
        let conv = ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        };
        InboundEvent {
            event_id: "Ev01".into(),
            binding: BindingId::new_v4(),
            sender: MemberKey {
                surface: SurfaceKind::Slack,
                team: "T1".into(),
                user: "U1".into(),
            },
            outside: Some(Outside {
                team: Some("T9".into()),
            }),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv.clone(),
            conv_kind,
            thread_root: Some("1.1".into()),
            message: MsgRef {
                conv: conv.clone(),
                id: "1.2".into(),
            },
            text: "hi <@U2>".into(),
            mentions: vec!["U2".into()],
            reply_to: Some(MsgRef {
                conv,
                id: "1.1".into(),
            }),
            files: vec![InFile {
                id: "F1".into(),
                name: "notes.txt".into(),
                mime_type: Some("text/plain".into()),
                size: Some(12),
                url: "https://files.example/F1".into(),
            }],
            received_at: datetime!(2026-09-30 12:34:56.789 UTC),
        }
    }

    #[test]
    fn inbound_event_serde_round_trips() {
        let json = json_round_trip(&sample_event(ConvKind::Channel));
        assert_eq!(json["received_at"], "2026-09-30T12:34:56.789Z");
        assert_eq!(json["conv_kind"], "channel");
        assert_eq!(json["outside"], serde_json::json!({"team": "T9"}));
    }

    #[test]
    fn an_event_stored_before_outside_existed_reads_as_home() {
        let mut json = serde_json::to_value(sample_event(ConvKind::Channel)).unwrap();
        json.as_object_mut().unwrap().remove("outside");
        let event: InboundEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.outside, None);
    }

    #[test]
    fn debug_shows_the_text_length_not_the_text() {
        let mut event = sample_event(ConvKind::Channel);
        event.text = "the secret plan".into();
        let debug = format!("{event:?}");
        assert!(!debug.contains("secret plan"), "{debug}");
        assert!(debug.contains("text_len: 15"), "{debug}");
        assert!(debug.contains("Ev01"), "{debug}");
        assert!(debug.contains("T9"), "the outside organization: {debug}");
    }

    #[test]
    fn is_dm_only_for_one_to_one_dms() {
        assert!(sample_event(ConvKind::Dm).is_dm());
        assert!(!sample_event(ConvKind::GroupDm).is_dm());
        assert!(!sample_event(ConvKind::Channel).is_dm());
    }
}
