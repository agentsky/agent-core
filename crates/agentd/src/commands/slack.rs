//! Commands on Slack: which requests to the manager app are commands, and
//! which events mean a member left.
//!
//! - An `/agent` slash command. Its text is never posted to the
//!   conversation, so it is private wherever it was run, and its reply goes
//!   back through its `response_url`.
//! - A direct message to the manager app (`message.im`), parsed as a whole,
//!   as on Rocket.Chat. Its reply goes to the same DM, and the files
//!   attached to it go with the command, for `persona`.
//! - A `user_change` event whose user is `deleted`: the member left the
//!   workspace, and their configuration token for it is deleted.
//!
//! Slack writes `&`, `<` and `>` in text as entities; command text is
//! decoded with [`unescape`] before it is parsed, so a persona reads as the
//! member typed it. Messages from bots, the manager's own replies included,
//! are never commands.

use core_types::{ConvKind, InFile, InboundEvent, MemberKey, SurfaceKind, UserId};
use serde_json::Value;
use surface_slack::normalize::unescape;
use surface_slack::{SlackEvent, SlashCommand};

use super::Origin;
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
    };
    Some((command.sender, text, origin))
}

/// The member, command text, origin and attached files of a direct message
/// to the manager app, which `manager` is; `None` for a message from a bot
/// or the manager itself, or one that isn't in a one-to-one DM. The files
/// feed `persona`.
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
