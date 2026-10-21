//! Private replies: command replies and notices only the member sees.
//!
//! On Rocket.Chat every private reply is a direct message from the manager
//! bot. On Slack a slash command is answered through its `response_url` and
//! anything else by a manager DM; that arm is added with the Slack manager
//! app (T30), and until then it fails with [`ReplyError::SlackUnavailable`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use core_types::{
    ConvRef, ConversationId, MemberKey, ReplyTarget, Surface, SurfaceError, SurfaceKind,
};

use super::Origin;

/// Why a private reply couldn't be sent. The message names no text.
#[derive(Debug, thiserror::Error)]
pub enum ReplyError {
    /// Private replies on Slack come with the Slack manager app (T30).
    #[error("private replies on Slack are not available yet")]
    SlackUnavailable,
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
}

impl Replies {
    /// Replies through `rocketchat`, the Rocket.Chat manager bot, if agentd
    /// serves Rocket.Chat.
    pub fn new(rocketchat: Option<Arc<ManagerBot>>) -> Self {
        Self { rocketchat }
    }

    fn bot_for(&self, member: &MemberKey) -> Result<&ManagerBot, ReplyError> {
        match member.surface {
            SurfaceKind::Slack => Err(ReplyError::SlackUnavailable),
            SurfaceKind::RocketChat => self
                .rocketchat
                .as_deref()
                .filter(|bot| bot.serves(member))
                .ok_or(ReplyError::NoManagerBot(member.surface)),
        }
    }

    /// Whether a manager bot can DM `member`.
    pub fn can_dm(&self, member: &MemberKey) -> bool {
        self.bot_for(member).is_ok()
    }

    /// Sends Markdown `text` privately to `member`, who sent a command from
    /// `origin`: in the manager bot's DM on Rocket.Chat (the DM the command
    /// came from, or a new one for a channel command). Slack replies come
    /// with T30.
    ///
    /// # Errors
    ///
    /// [`ReplyError::SlackUnavailable`] for a Slack origin,
    /// [`ReplyError::NoManagerBot`] if no manager bot serves the member, and
    /// [`ReplyError::Surface`] if posting fails.
    pub async fn reply_private(
        &self,
        member: &MemberKey,
        origin: &Origin,
        text: &str,
    ) -> Result<(), ReplyError> {
        match origin {
            Origin::SlackSlash { .. } => Err(ReplyError::SlackUnavailable),
            Origin::RocketChatDm { room } => Ok(self.bot_for(member)?.post(room, text).await?),
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
