//! Commands on Slack: which requests to the manager app are commands, and
//! which events mean a member left.
//!
//! - An `/agent` slash command. Its text is never posted to the
//!   conversation, so it is private wherever it was run, and its reply goes
//!   back through its `response_url`.
//! - A direct message to the manager app (`message.im`), parsed as a whole,
//!   as on Rocket.Chat. Its reply goes to the same DM, and the files
//!   attached to it go with the command, for `persona` and `skill add`.
//! - A click on a consent card's Approve or Decline button (a
//!   `block_actions` interaction), as `approve <id>` or `decline <id>` from
//!   whoever clicked. Its reply goes back through its `response_url`.
//! - A `user_change` event whose user is `deleted`: the member left the
//!   workspace, and their configuration token for it is deleted.
//!
//! Slack writes `&`, `<` and `>` in text as entities; command text is
//! decoded with [`unescape`] before it is parsed, so a persona reads as the
//! member typed it. Messages from bots, the manager's own replies included,
//! are never commands.

use core_types::{
    ConsentId, ConvKind, ConvRef, InFile, InboundEvent, MemberKey, SurfaceKind, UserId,
};
use serde_json::Value;
use surface_slack::normalize::unescape;
use surface_slack::{Interaction, SlackEvent, SlashCommand};

use super::Origin;
use crate::consents::card;
use crate::slack::manager::ManagerIdentity;

/// The slash command the manager app declares.
pub const SLASH_COMMAND: &str = "/agent";

/// The member, command text and origin of an `/agent` slash command; `None`
/// for any other command.
pub fn slash_command(command: SlashCommand) -> Option<(MemberKey, String, Origin)> {
    if command.command != SLASH_COMMAND {
        tracing::debug!(sender = %command.sender, "ignored a slash command other than /agent");
        return None;
    }
    let text = unescape(&command.text);
    let origin = Origin::SlackSlash {
        response_url: command.response_url,
        conv: command.conv,
    };
    Some((command.sender, text, origin))
}

/// The member, command text, origin and attached files of a direct message
/// to the manager app, which `manager` is; `None` for a message from a bot
/// or the manager itself, or one that isn't in a one-to-one DM. The files
/// feed `persona` and `skill add`.
pub fn dm_command(
    event: &InboundEvent,
    manager: &ManagerIdentity,
) -> Option<(MemberKey, String, Origin, Vec<InFile>)> {
    if event.sender_is_bot
        || event.sender_bot_user.is_some()
        || event.sender.user == manager.bot_user
        || event.conv_kind != ConvKind::Dm
    {
        return None;
    }
    let text = unescape(&event.text);
    let text = commands::strip_prefix(&text, ConvKind::Dm)?.to_owned();
    let origin = Origin::SlackDm {
        channel: event.conv.conversation.clone(),
    };
    Some((event.sender.clone(), text, origin, event.files.clone()))
}

/// The member, command text and origin of a click on a consent card's
/// Approve or Decline button: `approve <id>` or `decline <id>` from
/// whoever clicked, answered through the payload's `response_url`. `None`
/// for any other interaction, and for one that names no sender, no
/// conversation or no `response_url`. Whether the clicker may decide is
/// the command's to check.
pub fn consent_action(interaction: Interaction) -> Option<(MemberKey, String, Origin)> {
    if interaction.kind != "block_actions" {
        return None;
    }
    let payload = &interaction.payload;
    let (command, consent) = payload
        .get("actions")?
        .as_array()?
        .iter()
        .find_map(|action| {
            if action.get("block_id")?.as_str()? != card::BLOCK_ID {
                return None;
            }
            let command = match action.get("action_id")?.as_str()? {
                card::APPROVE_ACTION => "approve",
                card::DECLINE_ACTION => "decline",
                _ => return None,
            };
            let consent: ConsentId = action.get("value")?.as_str()?.parse().ok()?;
            Some((command, consent))
        })?;
    let sender = interaction.sender?;
    let channel = payload
        .get("container")
        .and_then(|container| container.get("channel_id"))
        .or_else(|| payload.get("channel").and_then(|channel| channel.get("id")))
        .and_then(Value::as_str)
        .filter(|channel| !channel.is_empty())?;
    let origin = Origin::SlackSlash {
        response_url: interaction.response_url?,
        conv: ConvRef {
            surface: SurfaceKind::Slack,
            team: sender.team.clone(),
            conversation: channel.into(),
        },
    };
    Some((sender, format!("{command} {consent}"), origin))
}

/// The member a `user_change` event says was deleted (left the workspace or
/// was deactivated), in the workspace of the event's envelope; `None` when
/// the envelope names none.
pub fn member_who_left(event: &SlackEvent) -> Option<MemberKey> {
    if event.event_type != "user_change" {
        return None;
    }
    let user = event.event.get("user")?;
    if user.get("deleted").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let id = user
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    Some(MemberKey {
        surface: SurfaceKind::Slack,
        team: event.team.clone()?,
        user: UserId::new(id),
    })
}
