//! [`InboundEvent`]: a chat message, normalized by its surface.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{BindingId, ConvKind, ConvRef, InFile, MemberKey, MessageId, MsgRef, UserId};

/// One inbound chat message, normalized by its surface. Everything after
/// this is shared between surfaces.
///
/// Surfaces don't decide whether an agent answers. They fill in what the
/// platform said, and the router gates on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundEvent {
    /// The platform's id for this delivery, used to drop duplicates: a Slack
    /// `event_id` or a Rocket.Chat message `_id`.
    pub event_id: String,
    /// The binding whose connection or app received the event.
    pub binding: BindingId,
    /// Who sent the message.
    pub sender: MemberKey,
    /// Whether the sender is a bot user, managed by agentd or not.
    pub sender_is_bot: bool,
    /// The sender's bot user id, when the sender is a bot and the platform
    /// named one. On Slack a bot message may carry only a `bot_id`, and
    /// this is filled from `bots.info`.
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
    /// Every user the message mentions, in order.
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
    }

    #[test]
    fn is_dm_only_for_one_to_one_dms() {
        assert!(sample_event(ConvKind::Dm).is_dm());
        assert!(!sample_event(ConvKind::GroupDm).is_dm());
        assert!(!sample_event(ConvKind::Channel).is_dm());
    }
}
