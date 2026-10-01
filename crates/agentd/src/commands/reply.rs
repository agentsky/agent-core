//! Private replies: command replies and notices only the member sees.
//!
//! On Rocket.Chat every private reply is a direct message from the manager
//! bot. On Slack a slash command is answered through its `response_url`, as
//! an ephemeral message only the member sees, and anything else by a DM
//! from the manager app.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use core_types::{
    ConvRef, ConversationId, MemberKey, MsgRef, ReplyTarget, Surface, SurfaceError, SurfaceKind,
    TeamId,
};
use secrecy::SecretString;
use serde_json::Value;
use surface_slack::{SlackClient, WebApi};

use crate::slack::manager::SlackManager;

use super::Origin;

/// Why a private reply couldn't be sent. The message names no text.
#[derive(Debug, thiserror::Error)]
pub enum ReplyError {
    /// No manager bot serves the member's surface and team.
    #[error("no manager bot serves this member's {0} workspace")]
    NoManagerBot(SurfaceKind),
    /// The surface refused or failed.
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    /// A message that must be one, such as a consent card, would be split
    /// in several.
    #[error("the message doesn't fit in one")]
    TooLong,
}

/// Opens the manager bot's direct message with a member.
#[async_trait]
pub trait OpenDm: Send + Sync {
    /// The room of the manager bot's DM with `member`, opened if there is
    /// none yet. `member` is on the manager bot's surface and team.
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError>;

    /// The name `member` goes by on the manager bot's surface, which people
    /// there know them by. `member` is on the manager bot's surface and
    /// team.
    ///
    /// # Errors
    ///
    /// If it can't be looked up; by default it never can.
    async fn name_of(&self, _member: &MemberKey) -> Result<String, SurfaceError> {
        Err(SurfaceError::Unsupported("looking up a member's name"))
    }
}

/// A member's `name` as agentd shows it: without the presentation and
/// joining characters [`without_joiners`](crate::ctl::without_joiners)
/// drops, each other control or invisible character replaced by U+FFFD so
/// the name can't pass for another, and at most [`MAX_NAME_CHARS`] long.
/// `None` for a name with nothing to show but blanks and backticks.
pub(crate) fn shown_name(name: &str) -> Option<String> {
    let name: String = crate::ctl::without_joiners(name)
        .chars()
        .map(|c| {
            if c.is_control() || crate::ctl::is_invisible(c) {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .take(MAX_NAME_CHARS)
        .collect();
    name.chars()
        .any(|c| c != '`' && !c.is_whitespace())
        .then_some(name)
}

/// The longest name [`Replies::name_of`] gives, in characters.
pub const MAX_NAME_CHARS: usize = 80;

/// The manager bot on one surface and team: who posts private replies there.
pub struct ManagerBot {
    identity: MemberKey,
    surface: Arc<dyn Surface>,
    dms: Arc<dyn OpenDm>,
}

impl fmt::Debug for ManagerBot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagerBot")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl ManagerBot {
    /// The manager bot `identity`, posting through `surface` (acting as the
    /// bot) and opening DMs with `dms`.
    pub fn new(identity: MemberKey, surface: Arc<dyn Surface>, dms: Arc<dyn OpenDm>) -> Self {
        Self {
            identity,
            surface,
            dms,
        }
    }

    /// The manager bot's own identity.
    pub fn identity(&self) -> &MemberKey {
        &self.identity
    }

    fn serves(&self, surface: SurfaceKind, team: &TeamId) -> bool {
        surface == self.identity.surface && *team == self.identity.team
    }

    /// Markdown `text` rendered and split for the surface.
    pub fn render(&self, text: &str) -> Vec<String> {
        self.surface.render(text)
    }

    /// Posts Markdown `text` in `room`, rendered and split for the surface.
    ///
    /// # Errors
    ///
    /// The first [`SurfaceError`]; chunks after it aren't posted.
    pub async fn post(&self, room: &ConversationId, text: &str) -> Result<(), SurfaceError> {
        let to = self.target(room);
        for chunk in self.surface.render(text) {
            self.surface.post(&to, &chunk).await?;
        }
        Ok(())
    }

    /// The top level of `room`, on the bot's surface and team.
    fn target(&self, room: &ConversationId) -> ReplyTarget {
        ReplyTarget {
            conv: ConvRef {
                surface: self.identity.surface,
                team: self.identity.team.clone(),
                conversation: room.clone(),
            },
            thread_root: None,
        }
    }

    /// Markdown `text` rendered for the surface as one message.
    ///
    /// # Errors
    ///
    /// [`ReplyError::TooLong`] if it renders to more than one.
    fn render_one(&self, text: &str) -> Result<String, ReplyError> {
        let mut chunks = self.surface.render(text).into_iter();
        match (chunks.next(), chunks.next()) {
            (Some(one), None) => Ok(one),
            _ => Err(ReplyError::TooLong),
        }
    }

    /// Sends Markdown `text` to `member` in the manager bot's DM with them.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] if the DM can't be opened or posted to.
    pub async fn dm(&self, member: &MemberKey, text: &str) -> Result<(), SurfaceError> {
        let room = self.dm_room(member).await?;
        self.post(&room, text).await
    }

    /// The room of the manager bot's DM with `member`, opened if there is
    /// none yet.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] if the DM can't be opened.
    pub async fn dm_room(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        self.dms.open_dm(member).await
    }
}

/// A message with more than Markdown where the surface shows it: a
/// consent card, with buttons on Slack.
#[derive(Debug, Clone, PartialEq)]
pub struct Rich {
    /// The whole message as Markdown, for surfaces without blocks.
    pub markdown: String,
    /// The notification's text on Slack, beside the blocks.
    pub fallback: String,
    /// The message as Slack Block Kit, when there is one.
    pub blocks: Option<Value>,
}

/// Sends private replies through the manager bot of each surface.
///
/// Cloning is cheap and shares the bots.
#[derive(Debug, Clone, Default)]
pub struct Replies {
    rocketchat: Option<Arc<ManagerBot>>,
    slack: Option<SlackReplies>,
}

/// The Slack manager app's bot, the client that answers through a
/// `response_url`, and the bot's Web API, which posts blocks.
#[derive(Debug, Clone)]
struct SlackReplies {
    bot: Arc<ManagerBot>,
    client: SlackClient,
    api: WebApi,
}

impl Replies {
    /// Replies through `rocketchat`, the Rocket.Chat manager bot, if agentd
    /// serves Rocket.Chat.
    pub fn new(rocketchat: Option<Arc<ManagerBot>>) -> Self {
        Self {
            rocketchat,
            slack: None,
        }
    }

    /// Also replies on Slack as the manager app `manager`: DMs through its
    /// bot, and slash command replies through their `response_url`.
    pub fn with_slack(mut self, manager: &SlackManager) -> Self {
        self.slack = Some(SlackReplies {
            bot: Arc::new(manager.manager_bot()),
            client: manager.client().clone(),
            api: manager.surface().api().clone(),
        });
        self
    }

    fn bot_for(&self, member: &MemberKey) -> Result<&ManagerBot, ReplyError> {
        self.bot_on(member.surface, &member.team)
    }

    fn bot_on(&self, surface: SurfaceKind, team: &TeamId) -> Result<&ManagerBot, ReplyError> {
        let bot = match surface {
            SurfaceKind::Slack => self.slack.as_ref().map(|slack| &*slack.bot),
            SurfaceKind::RocketChat => self.rocketchat.as_deref(),
        };
        bot.filter(|bot| bot.serves(surface, team))
            .ok_or(ReplyError::NoManagerBot(surface))
    }

    fn slack_on(&self, surface: SurfaceKind, team: &TeamId) -> Option<&SlackReplies> {
        self.slack
            .as_ref()
            .filter(|slack| slack.bot.serves(surface, team))
    }

    /// Answers a slash command privately through its `response_url`, with
    /// Markdown `text` rendered and split for Slack.
    async fn respond(&self, response_url: &SecretString, text: &str) -> Result<(), ReplyError> {
        let slack = self
            .slack
            .as_ref()
            .ok_or(ReplyError::NoManagerBot(SurfaceKind::Slack))?;
        for chunk in slack.bot.render(text) {
            slack.client.respond_ephemeral(response_url, &chunk).await?;
        }
        Ok(())
    }

    /// The name `member` goes by on their surface, looked up by the manager
    /// bot that serves their surface and team, at most [`MAX_NAME_CHARS`]
    /// long, with each control or invisible character replaced by U+FFFD,
    /// so the name can't pass for another by hiding a character. `None`
    /// when no manager bot serves them or the lookup fails.
    pub async fn name_of(&self, member: &MemberKey) -> Option<String> {
        let bot = self.bot_for(member).ok()?;
        match bot.dms.name_of(member).await {
            Ok(name) => shown_name(&name),
            Err(err) => {
                tracing::debug!(member = %member, error = %err, "couldn't look up a member's name");
                None
            }
        }
    }

    /// Whether a manager bot can DM `member`.
    pub fn can_dm(&self, member: &MemberKey) -> bool {
        self.bot_for(member).is_ok()
    }

    /// Sends Markdown `text` privately to `member`, who sent a command from
    /// `origin`: through the `response_url` of a Slack slash command, as an
    /// ephemeral message; in the Slack manager app's DM a command came from;
    /// and in the manager bot's DM on Rocket.Chat (the DM the command came
    /// from, or a new one for a channel command).
    ///
    /// # Errors
    ///
    /// [`ReplyError::NoManagerBot`] if no manager bot serves the member (a
    /// slash command needs only the Slack manager app), and
    /// [`ReplyError::Surface`] if posting fails.
    pub async fn reply_private(
        &self,
        member: &MemberKey,
        origin: &Origin,
        text: &str,
    ) -> Result<(), ReplyError> {
        match origin {
            Origin::SlackSlash { response_url, .. } => self.respond(response_url, text).await,
            Origin::SlackDm { channel: room } | Origin::RocketChatDm { room } => {
                Ok(self.bot_for(member)?.post(room, text).await?)
            }
            Origin::RocketChatChannel { .. } => self.dm(member, text).await,
        }
    }

    /// Sends Markdown `text` to `member` in a DM from the manager bot, for
    /// notices that answer no command.
    ///
    /// # Errors
    ///
    /// As for [`reply_private`](Self::reply_private).
    pub async fn dm(&self, member: &MemberKey, text: &str) -> Result<(), ReplyError> {
        Ok(self.bot_for(member)?.dm(member, text).await?)
    }

    /// The room of the manager bot's DM with `member`, opened if there is
    /// none yet.
    ///
    /// # Errors
    ///
    /// As for [`reply_private`](Self::reply_private).
    pub async fn dm_room(&self, member: &MemberKey) -> Result<ConversationId, ReplyError> {
        Ok(self.bot_for(member)?.dm_room(member).await?)
    }

    /// Sends `message` to `member` in a DM from the manager bot, as one
    /// message: its blocks on Slack, its Markdown elsewhere. Returns where
    /// it went.
    ///
    /// # Errors
    ///
    /// As for [`reply_private`](Self::reply_private), and
    /// [`ReplyError::TooLong`], sending nothing, for Markdown the surface
    /// would split.
    pub async fn dm_rich(&self, member: &MemberKey, message: &Rich) -> Result<MsgRef, ReplyError> {
        let bot = self.bot_for(member)?;
        let room = bot.dm_room(member).await?;
        let conv = ConvRef {
            surface: member.surface,
            team: member.team.clone(),
            conversation: room.clone(),
        };
        if let (Some(slack), Some(blocks)) =
            (self.slack_on(member.surface, &member.team), &message.blocks)
        {
            let id = slack
                .api
                .post_blocks(&room, &message.fallback, blocks)
                .await?;
            return Ok(MsgRef { conv, id });
        }
        let text = bot.render_one(&message.markdown)?;
        Ok(bot.surface.post(&bot.target(&room), &text).await?)
    }

    /// Replaces the manager bot's message `msg`, which
    /// [`dm_rich`](Self::dm_rich) sent, with `message`, whole: its blocks
    /// on Slack, and elsewhere its Markdown.
    ///
    /// # Errors
    ///
    /// As for [`dm_rich`](Self::dm_rich).
    pub async fn update_rich(&self, msg: &MsgRef, message: &Rich) -> Result<(), ReplyError> {
        let conv = &msg.conv;
        let bot = self.bot_on(conv.surface, &conv.team)?;
        if let (Some(slack), Some(blocks)) =
            (self.slack_on(conv.surface, &conv.team), &message.blocks)
        {
            slack
                .api
                .update_blocks(&msg.conv.conversation, &msg.id, &message.fallback, blocks)
                .await?;
            return Ok(());
        }
        let text = bot.render_one(&message.markdown)?;
        Ok(bot.surface.edit(msg, &text).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_name_marks_what_it_hides_and_is_capped() {
        assert_eq!(shown_name("bob.smith").as_deref(), Some("bob.smith"));
        assert_eq!(
            shown_name("alice\u{200B}").as_deref(),
            Some("alice\u{FFFD}"),
            "can't pass for alice"
        );
        assert_eq!(
            shown_name("a\u{7}\u{202E}b").as_deref(),
            Some("a\u{FFFD}\u{FFFD}b")
        );
        assert_eq!(shown_name("   "), None);
        assert_eq!(shown_name(" `` "), None, "nothing left to show as code");
        assert_eq!(
            shown_name("\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}")
                .as_deref(),
            Some("\u{0645}\u{06CC}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}"),
            "joiners are dropped, not marked"
        );
        assert_eq!(
            shown_name(&"x".repeat(200)).map(|name| name.chars().count()),
            Some(MAX_NAME_CHARS)
        );
    }
}
