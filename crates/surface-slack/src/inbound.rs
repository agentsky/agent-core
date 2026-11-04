//! [`SlackInbound`]: what the ingress hands the rest of agentd once a
//! request is verified, deduplicated and normalized.

use std::fmt;

use core_types::{BindingId, ConvRef, InboundEvent, MemberKey, TeamId};
use secrecy::SecretString;
use serde_json::{Map, Value};
use time::OffsetDateTime;

use crate::ingress::InFlight;

/// One verified request from Slack, after deduplication.
#[derive(Debug)]
pub enum SlackInbound {
    /// A `message` event the router can use, normalized, with its place
    /// among its binding's requests in flight. Boxed, since it is much
    /// larger than the other variants.
    Message(Box<InboundEvent>, InFlight),
    /// Any other Events API event, such as `user_change` or
    /// `app_uninstalled`, as Slack sent it.
    Event(SlackEvent),
    /// A slash command.
    Command(SlashCommand),
    /// An interactivity payload: a button press, a modal submission, a
    /// shortcut.
    Interaction(Interaction),
}

impl SlackInbound {
    /// The binding whose app received the request.
    pub fn binding(&self) -> BindingId {
        match self {
            Self::Message(event, _) => event.binding,
            Self::Event(event) => event.binding,
            Self::Command(command) => command.binding,
            Self::Interaction(interaction) => interaction.binding,
        }
    }

    /// The workspace the request came from: a message's conversation's, an
    /// event's envelope's, a command's or an interaction's sender's.
    /// `None` when Slack named none.
    pub fn team(&self) -> Option<&TeamId> {
        match self {
            Self::Message(event, _) => Some(&event.conv.team),
            Self::Event(event) => event.team.as_ref(),
            Self::Command(command) => Some(&command.sender.team),
            Self::Interaction(interaction) => {
                interaction.sender.as_ref().map(|sender| &sender.team)
            }
        }
    }

    /// What kind of request this is, for logs: `message`, `event`,
    /// `command` or `interaction`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Message(..) => "message",
            Self::Event(_) => "event",
            Self::Command(_) => "command",
            Self::Interaction(_) => "interaction",
        }
    }
}

/// An Events API event other than `message`.
///
/// Its `Debug` output leaves out the event object, which can hold message
/// text, such as an `app_mention`'s or a `message_changed`'s.
#[derive(Clone)]
pub struct SlackEvent {
    /// The binding whose app received the event.
    pub binding: BindingId,
    /// The envelope's `team_id`, when it has one.
    pub team: Option<TeamId>,
    /// The envelope's `event_id`.
    pub event_id: String,
    /// The event's `type`, such as `user_change`.
    pub event_type: String,
    /// The envelope's `event` object, unchanged.
    pub event: Value,
    /// When agentd received the request.
    pub received_at: OffsetDateTime,
}

impl fmt::Debug for SlackEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackEvent")
            .field("binding", &self.binding)
            .field("team", &self.team)
            .field("event_id", &self.event_id)
            .field("event_type", &self.event_type)
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

/// A slash command.
///
/// Its `Debug` output leaves out the text, which can hold a secret
/// (`/agent slack-token …`), and the response URL.
pub struct SlashCommand {
    /// The binding whose app received the command.
    pub binding: BindingId,
    /// Who ran it.
    pub sender: MemberKey,
    /// The conversation it was run in.
    pub conv: ConvRef,
    /// The command, such as `/agent`.
    pub command: String,
    /// Everything after the command, as Slack sent it: mentions, channels
    /// and links escaped as `<@U…|name>` tokens, and `&`, `<` and `>` as
    /// entities.
    pub text: String,
    /// Where to send the reply, for 30 minutes. Anyone holding it can post
    /// to the conversation, so it is kept secret.
    pub response_url: SecretString,
    /// The id for opening a modal in reply, valid for 3 seconds.
    pub trigger_id: Option<String>,
    /// When agentd received the request.
    pub received_at: OffsetDateTime,
}

impl fmt::Debug for SlashCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlashCommand")
            .field("binding", &self.binding)
            .field("sender", &self.sender)
            .field("conv", &self.conv)
            .field("command", &self.command)
            .field("text_len", &self.text.len())
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

/// An interactivity payload.
///
/// Its `Debug` output leaves out the payload, which holds what the member
/// typed into a modal, and the response URL.
pub struct Interaction {
    /// The binding whose app received the payload.
    pub binding: BindingId,
    /// The payload's `type`, such as `block_actions` or `view_submission`.
    pub kind: String,
    /// Who interacted, from `team.id` and `user.id`, when both are present.
    pub sender: Option<MemberKey>,
    /// The payload's `response_url`, when it has one. Kept secret like a
    /// [`SlashCommand`]'s.
    pub response_url: Option<SecretString>,
    /// The payload as Slack sent it, less `token` (the deprecated
    /// verification token) and `response_url`.
    pub payload: Map<String, Value>,
    /// When agentd received the request.
    pub received_at: OffsetDateTime,
}

impl fmt::Debug for Interaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interaction")
            .field("binding", &self.binding)
            .field("kind", &self.kind)
            .field("sender", &self.sender)
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_events_debug_leaves_out_the_event_object() {
        let event = SlackEvent {
            binding: BindingId::new_v4(),
            team: Some("T1".into()),
            event_id: "Ev1".into(),
            event_type: "app_mention".into(),
            event: json!({ "type": "app_mention", "text": "the launch code is 1234" }),
            received_at: OffsetDateTime::UNIX_EPOCH,
        };
        let debug = format!("{event:?}");
        assert!(!debug.contains("launch code"), "{debug}");
        assert!(
            debug.contains("app_mention") && debug.contains("Ev1"),
            "{debug}"
        );
    }
}
