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
    ConvRef, ConversationId, MemberKey, ReplyTarget, Surface, SurfaceError, SurfaceKind,
};
use secrecy::SecretString;
use surface_slack::SlackClient;

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
}

/// Opens the manager bot's direct message with a member.
#[async_trait]
pub trait OpenDm: Send + Sync {
    /// The room of the manager bot's DM with `member`, opened if there is
    /// none yet. `member` is on the manager bot's surface and team.
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError>;
}

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

    fn serves(&self, member: &MemberKey) -> bool {
        member.surface == self.identity.surface && member.team == self.identity.team
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
        let to = ReplyTarget {
            conv: ConvRef {
                surface: self.identity.surface,
                team: self.identity.team.clone(),
                conversation: room.clone(),
            },
            thread_root: None,
        };
        for chunk in self.surface.render(text) {
            self.surface.post(&to, &chunk).await?;
        }
        Ok(())
    }

    /// Sends Markdown `text` to `member` in the manager bot's DM with them.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] if the DM can't be opened or posted to.
    pub async fn dm(&self, member: &MemberKey, text: &str) -> Result<(), SurfaceError> {
        let room = self.dms.open_dm(member).await?;
        self.post(&room, text).await
    }
}

/// Sends private replies through the manager bot of each surface.
///
/// Cloning is cheap and shares the bots.
#[derive(Debug, Clone, Default)]
pub struct Replies {
    rocketchat: Option<Arc<ManagerBot>>,
    slack: Option<SlackReplies>,
}

/// The Slack manager app's bot, and the client that answers through a
/// `response_url`.
#[derive(Debug, Clone)]
struct SlackReplies {
    bot: Arc<ManagerBot>,
    client: SlackClient,
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

    /// Also replies on Slack: DMs through `bot`, the Slack manager app's
    /// bot, and slash command replies through their `response_url` with
    /// `client`.
    pub fn with_slack(mut self, bot: Arc<ManagerBot>, client: SlackClient) -> Self {
        self.slack = Some(SlackReplies { bot, client });
        self
    }

    fn bot_for(&self, member: &MemberKey) -> Result<&ManagerBot, ReplyError> {
        let bot = match member.surface {
            SurfaceKind::Slack => self.slack.as_ref().map(|slack| &*slack.bot),
            SurfaceKind::RocketChat => self.rocketchat.as_deref(),
        };
        bot.filter(|bot| bot.serves(member))
            .ok_or(ReplyError::NoManagerBot(member.surface))
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
            Origin::SlackSlash { response_url } => self.respond(response_url, text).await,
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
}
