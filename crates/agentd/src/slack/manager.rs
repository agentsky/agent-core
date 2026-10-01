//! [`SlackManager`]: the Slack manager app, acting with its bot token.
//!
//! The manager app is installed once in the workspace agentd serves, from
//! `deploy/slack/manager-manifest.yaml`. It declares `/agent`, takes
//! commands as direct messages, and sends every private reply and notice.
//! Its bot token is
//! [`AGENTD_SLACK_MANAGER_BOT_TOKEN`](crate::config::SLACK_MANAGER_BOT_TOKEN_VAR).
//!
//! At startup [`SlackManager::connect`] asks Slack who the token is:
//! `auth.test` gives the workspace, the bot user and the bot, and
//! `bots.info` on the bot gives the app's id and name, which `/agent me`
//! shows so members notice when another app has taken `/agent` over.

use std::fmt;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use core_types::{ConversationId, MemberKey, SurfaceError, SurfaceKind, TeamId, UserId};
use secrecy::SecretString;
use surface_slack::{SlackClient, SlackSurface, TeamDirectory, WebApi};

use crate::commands::{ManagerBot, OpenDm};
use crate::config::{Config, SLACK_MANAGER_BOT_TOKEN_VAR};

/// Who the manager app is, from `auth.test` and `bots.info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerIdentity {
    /// The workspace the app is installed in, the one agentd serves.
    pub team: TeamId,
    /// The app's bot user.
    pub bot_user: UserId,
    /// The app's bot (`B…`).
    pub bot_id: String,
    /// The app's id (`A…`).
    pub app_id: String,
    /// The app's name, as `bots.info` gives it.
    pub app_name: Option<String>,
    /// The workspace's Enterprise Grid organization, the `enterprise_id`
    /// `auth.test` gave, if it is in one. A message's sender team fields
    /// may name it for a home member.
    pub enterprise: Option<TeamId>,
}

impl ManagerIdentity {
    /// Asks Slack who `api`'s bot token belongs to.
    ///
    /// # Errors
    ///
    /// The [`SurfaceError`] of `auth.test` or `bots.info`, or
    /// [`SurfaceError::Api`] if the token isn't a bot token or its bot
    /// belongs to no app.
    pub async fn look_up(api: &WebApi) -> Result<Self, SurfaceError> {
        let auth = api.auth_test().await?;
        let bot_id = auth.bot_id.filter(|id| !id.is_empty()).ok_or_else(|| {
            SurfaceError::Api("the token is not a bot token (auth.test named no bot)".into())
        })?;
        let bot = api.bot_info(&bot_id).await?;
        let app_id = bot
            .app_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| SurfaceError::Api("bots.info named no app for the bot".into()))?;
        Ok(Self {
            team: auth.team_id,
            bot_user: auth.user_id,
            bot_id,
            app_id,
            app_name: bot.name.filter(|name| !name.trim().is_empty()),
            enterprise: auth.enterprise_id,
        })
    }
}

/// The manager app: the Web API client, the surface that posts as its bot,
/// and who it is.
///
/// Cloning is cheap and shares everything. `Debug` never shows the token.
#[derive(Clone)]
pub struct SlackManager {
    client: SlackClient,
    surface: Arc<SlackSurface>,
    identity: Arc<ManagerIdentity>,
}

impl fmt::Debug for SlackManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackManager")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl SlackManager {
    /// The manager app from the configuration, if its secrets are set:
    /// looks its identity up with the bot token.
    ///
    /// # Errors
    ///
    /// If Slack can't be asked or refuses the token. agentd doesn't start
    /// without knowing which workspace it serves.
    pub async fn from_config(config: &Config) -> anyhow::Result<Option<Self>> {
        let Some(token) = config.secrets.slack_manager_bot_token() else {
            return Ok(None);
        };
        let client = SlackClient::new(&config.slack.api_url).context("slack.api_url")?;
        let manager = Self::connect(client, token.clone())
            .await
            .with_context(|| {
                format!("asking Slack who {SLACK_MANAGER_BOT_TOKEN_VAR} belongs to")
            })?;
        let identity = manager.identity();
        tracing::info!(
            team = %identity.team,
            bot_user = %identity.bot_user,
            app_id = identity.app_id,
            "Slack manager app connected"
        );
        Ok(Some(manager))
    }

    /// The manager app acting with `token` through `client`, after looking
    /// its identity up.
    ///
    /// # Errors
    ///
    /// As for [`ManagerIdentity::look_up`].
    pub async fn connect(client: SlackClient, token: SecretString) -> Result<Self, SurfaceError> {
        let api = client.bot(token);
        let identity = ManagerIdentity::look_up(&api).await?;
        Ok(Self::with_identity(client, api, identity))
    }

    /// The manager app acting through `api`, a client of `client` with its
    /// bot token, known to be `identity`.
    pub fn with_identity(client: SlackClient, api: WebApi, identity: ManagerIdentity) -> Self {
        let directory = Arc::new(
            TeamDirectory::new(identity.team.clone()).with_home_org(identity.enterprise.clone()),
        );
        Self {
            client,
            surface: Arc::new(
                SlackSurface::new(api, directory).with_bot_user(Some(identity.bot_user.clone())),
            ),
            identity: Arc::new(identity),
        }
    }

    /// Who the app is.
    pub fn identity(&self) -> &ManagerIdentity {
        &self.identity
    }

    /// The Web API client, for calls that need no bot token, such as
    /// answering through a `response_url` or rotating a configuration
    /// token.
    pub fn client(&self) -> &SlackClient {
        &self.client
    }

    /// The surface that posts as the manager's bot.
    pub fn surface(&self) -> &Arc<SlackSurface> {
        &self.surface
    }

    /// The manager bot's own identity.
    pub fn bot(&self) -> MemberKey {
        MemberKey {
            surface: SurfaceKind::Slack,
            team: self.identity.team.clone(),
            user: self.identity.bot_user.clone(),
        }
    }

    /// The [`ManagerBot`] that sends private replies and notices as the
    /// manager app, opening DMs with `conversations.open` only with home
    /// members ([`SlackDms`]).
    pub fn manager_bot(&self) -> ManagerBot {
        ManagerBot::new(
            self.bot(),
            self.surface.clone(),
            Arc::new(SlackDms::new(
                self.surface.api().clone(),
                Arc::clone(self.surface.directory()),
            )),
        )
    }
}

/// Opens the manager bot's DMs on Slack with `conversations.open`, which
/// returns the existing DM when there is one, and only with a member of the
/// workspace.
///
/// Every Slack DM the manager bot sends is opened here, through
/// [`ManagerBot`]: [`Replies`](crate::commands::Replies)'s `dm`, `dm_room`
/// and `dm_rich`, and direct `manager_bot().dm()` calls. So this is the
/// guard that keeps agentd from DMing a member of another organization in
/// a Slack Connect conversation: before `conversations.open`, the user is
/// looked up with [`TeamDirectory::home_user`], and one it doesn't say is
/// home fails with [`SurfaceError::Forbidden`]. A lookup that fails is an
/// error, not a verdict: it is passed on as it came and isn't kept, and
/// each caller handles either as it handles a failed `conversations.open`.
#[derive(Debug, Clone)]
pub struct SlackDms {
    api: WebApi,
    directory: Arc<TeamDirectory>,
}

impl SlackDms {
    /// Opens DMs with `api`, the manager app's bot token, after asking
    /// `directory`, the workspace's, whether the user is home.
    pub fn new(api: WebApi, directory: Arc<TeamDirectory>) -> Self {
        Self { api, directory }
    }
}

#[async_trait]
impl OpenDm for SlackDms {
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        if !self.directory.home_user(&self.api, &member.user).await? {
            return Err(SurfaceError::Forbidden(
                "the user isn't a member of the workspace; no DM is opened with them".into(),
            ));
        }
        self.api.open_dm(&member.user).await
    }

    async fn name_of(&self, member: &MemberKey) -> Result<String, SurfaceError> {
        let user = self.api.user_info(&member.user).await?;
        user.name
            .or(user.real_name)
            .ok_or(SurfaceError::NotFound("the user's name".to_owned()))
    }
}
