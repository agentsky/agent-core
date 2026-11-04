//! [`App`]: the state every part of agentd shares.

use std::sync::Arc;

use anyhow::Context as _;
use auth::Auth;
use core_types::{Binding, BindingId, MemberKey, SurfaceKind, TeamId, UserId};
use cred_proxy::Registry;
use store::Store;
use surface_rocketchat::rest::{Credentials, RestClient};
use surface_rocketchat::{BotRoles, RocketChatConfig, RocketChatSurface};

use crate::agents::RocketChatAgents;
use crate::commands::rocketchat::{RocketChatDms, StoreDedup};
use crate::commands::{Commands, ManagerBot, Replies};
use crate::config::{Config, RC_MANAGER_TOKEN_VAR};
use crate::ctl::{Ctl, CtlSettings, SurfaceLookup};
use crate::pipeline::StoreSurfaces;
use crate::policy::Limits;
use crate::skills::{Git, Skills};
use crate::slack::agents::{AgentAppSettings, SlackAgents};
use crate::slack::bots::SlackBots;
use crate::slack::manager::SlackManager;

/// The shared state: the configuration, the store, the agentctl API, the
/// credential proxy's placeholders, account linking, command dispatch, the
/// manager bots of Rocket.Chat and Slack, and agents' Slack apps.
///
/// Cloning is cheap: every clone shares the same state. Axum handlers take it
/// as their state.
#[derive(Debug, Clone)]
pub struct App {
    config: Arc<Config>,
    store: Store,
    ctl: Ctl,
    surfaces: Arc<dyn SurfaceLookup>,
    registry: Registry,
    auth: Arc<Auth>,
    commands: Commands,
    skills: Skills,
    rocketchat: Option<RocketChatManager>,
    slack: Option<SlackManager>,
    slack_agents: Option<SlackAgents>,
}

/// The Rocket.Chat manager bot: its surface and the binding it listens as,
/// and the agents it manages.
#[derive(Debug, Clone)]
pub struct RocketChatManager {
    /// The surface, acting as the manager bot.
    pub surface: Arc<RocketChatSurface>,
    /// The binding its connection listens as. The manager bot has no
    /// stored binding, so its id is new at every start.
    pub binding: Binding,
    /// Tells bots from people over the manager's client. Every Rocket.Chat
    /// surface shares this one, so a sender is classified the same whichever
    /// connection records a message.
    pub bots: BotRoles,
    /// The manager's surface configuration, which each agent's surface
    /// copies with the bot's own credentials.
    pub surface_config: RocketChatConfig,
    /// The agents on this server.
    pub agents: RocketChatAgents,
}

impl App {
    /// An `App` over an already open `store`, with the Slack manager app
    /// `slack` if agentd serves Slack (see [`SlackManager::from_config`]).
    /// Agents' bots act through [`StoreSurfaces`], over their bindings, and
    /// skills are cloned with the system's `git`.
    ///
    /// # Errors
    ///
    /// If the HTTP clients for Claude or Rocket.Chat can't be built.
    pub fn new(config: Config, store: Store, slack: Option<SlackManager>) -> anyhow::Result<Self> {
        Self::build(config, store, slack, None)
    }

    /// An `App` over an already open `store`, with the Slack manager app
    /// `slack`, whose agents' bots act through `surfaces`, for agentctl and
    /// the turn pipeline.
    ///
    /// # Errors
    ///
    /// If the HTTP clients for Claude or Rocket.Chat can't be built.
    pub fn with_surfaces(
        config: Config,
        store: Store,
        slack: Option<SlackManager>,
        surfaces: Arc<dyn SurfaceLookup>,
    ) -> anyhow::Result<Self> {
        Self::build(config, store, slack, Some(surfaces))
    }

    fn build(
        config: Config,
        store: Store,
        slack: Option<SlackManager>,
        surfaces: Option<Arc<dyn SurfaceLookup>>,
    ) -> anyhow::Result<Self> {
        let auth = Arc::new(
            Auth::new(config.claude_oauth.clone(), store.clone())
                .context("setting up Claude account linking")?,
        );
        let rocketchat = rocketchat_manager(&config, &store)?;
        let slack_agents = slack.as_ref().map(|slack| {
            let bots = SlackBots::new(store.clone(), slack.client().clone(), slack.surface());
            SlackAgents::new(
                store.clone(),
                slack.clone(),
                bots,
                AgentAppSettings::from_config(&config),
            )
        });
        let surfaces = surfaces.unwrap_or_else(|| {
            Arc::new(StoreSurfaces::new(
                store.clone(),
                rocketchat
                    .as_ref()
                    .map(|(manager, _)| (manager.surface_config.clone(), manager.bots.clone())),
                slack_agents.as_ref().map(|agents| agents.bots().clone()),
            ))
        });
        let ctl = Ctl::new(
            store.clone(),
            CtlSettings::from_config(&config),
            Arc::clone(&surfaces),
        );
        let mut replies = Replies::new(rocketchat.as_ref().map(|(_, bot)| Arc::clone(bot)));
        if let Some(slack) = &slack {
            replies = replies.with_slack(slack);
        }
        let agents = rocketchat
            .as_ref()
            .map(|(manager, _)| manager.agents.clone());
        let git = Git::new(config.egress_policy().context("[proxy]")?);
        let skills = Skills::new(store.clone(), config.store.data_dir.clone(), git);
        let mut commands = Commands::new(
            store.clone(),
            Arc::clone(&auth),
            replies,
            agents,
            slack.clone(),
            skills.clone(),
        )
        .with_admins(config.community.admins.iter().cloned())
        .with_limits(Limits::from_config(&config.limits))
        .with_consents(ctl.consents().clone());
        if let Some(slack_agents) = &slack_agents {
            commands = commands.with_slack_agents(slack_agents.clone());
        }
        Ok(Self {
            config: Arc::new(config),
            store,
            ctl,
            surfaces,
            registry: Registry::new(),
            auth,
            commands,
            skills,
            rocketchat: rocketchat.map(|(manager, _)| manager),
            slack,
            slack_agents,
        })
    }

    /// Opens the store at `store.url` with the master key, running pending
    /// migrations, asks Slack who the manager app is if agentd serves Slack,
    /// builds the `App`, and deletes every agentctl token, scope lock and
    /// staged attachment left from before (see [`Ctl::purge`]), and skill
    /// work left from before (see [`Skills::purge`]).
    ///
    /// # Errors
    ///
    /// If the store can't be opened or migrated, Slack can't tell who the
    /// manager app is, or a purge fails.
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        let store = open_store(&config).await?;
        let slack = SlackManager::from_config(&config).await?;
        let app = Self::new(config, store, slack)?;
        let purged = app
            .ctl
            .purge()
            .await
            .context("deleting agentctl tokens and scope locks at startup")?;
        tracing::info!(
            tokens = purged.tokens,
            locks = purged.locks,
            "deleted agentctl tokens and scope locks from before the restart"
        );
        app.skills
            .purge()
            .await
            .context("cleaning up skill work from before the restart")?;
        Ok(app)
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The agentctl API.
    pub fn ctl(&self) -> &Ctl {
        &self.ctl
    }

    /// The surfaces agents' bots act through.
    pub fn surfaces(&self) -> &Arc<dyn SurfaceLookup> {
        &self.surfaces
    }

    /// The live placeholders: the credential proxy checks them, and the
    /// turn hooks mint, point and revoke them.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Claude account linking.
    pub fn auth(&self) -> &Arc<Auth> {
        &self.auth
    }

    /// Command dispatch.
    pub fn commands(&self) -> &Commands {
        &self.commands
    }

    /// Agents' skills.
    pub fn skills(&self) -> &Skills {
        &self.skills
    }

    /// The Rocket.Chat manager bot, if agentd serves Rocket.Chat.
    pub fn rocketchat(&self) -> Option<&RocketChatManager> {
        self.rocketchat.as_ref()
    }

    /// The Slack manager app, if agentd serves Slack.
    pub fn slack(&self) -> Option<&SlackManager> {
        self.slack.as_ref()
    }

    /// Agents' Slack apps, if agentd serves Slack.
    pub fn slack_agents(&self) -> Option<&SlackAgents> {
        self.slack_agents.as_ref()
    }
}

/// The Rocket.Chat manager bot from `[rocketchat]`, if the section is set:
/// its surface, and the [`ManagerBot`] that sends private replies through it.
fn rocketchat_manager(
    config: &Config,
    store: &Store,
) -> anyhow::Result<Option<(RocketChatManager, Arc<ManagerBot>)>> {
    let Some(settings) = &config.rocketchat else {
        return Ok(None);
    };
    let token = config
        .secrets
        .rc_manager_token
        .clone()
        .with_context(|| format!("{RC_MANAGER_TOKEN_VAR} is required with [rocketchat]"))?;
    let identity = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new(settings.team.as_str()),
        user: UserId::new(settings.manager_user_id.as_str()),
    };
    let credentials = Credentials {
        user_id: identity.user.clone(),
        token,
    };
    let rest =
        RestClient::new(&settings.base_url, credentials.clone()).context("rocketchat.base_url")?;
    let mut surface_config = RocketChatConfig::new(
        settings.base_url.as_str(),
        identity.team.clone(),
        credentials,
    );
    surface_config
        .websocket_url
        .clone_from(&settings.websocket_url);
    let bots = BotRoles::new(rest.clone());
    let surface = Arc::new(
        RocketChatSurface::new(
            surface_config.clone(),
            Arc::new(StoreDedup(store.clone())),
            bots.clone(),
        )
        .context("setting up the Rocket.Chat manager bot")?,
    );
    let binding = Binding {
        id: BindingId::new_v4(),
        agent: None,
        bot: identity.clone(),
    };
    let agents = RocketChatAgents::new(
        store.clone(),
        rest.clone(),
        identity.team.clone(),
        settings.avatar_url.clone(),
    )
    .with_max_per_owner(config.agents.max_per_owner);
    let bot = Arc::new(ManagerBot::new(
        identity,
        surface.clone(),
        Arc::new(RocketChatDms(rest)),
    ));
    Ok(Some((
        RocketChatManager {
            surface,
            binding,
            bots,
            surface_config,
            agents,
        },
        bot,
    )))
}

/// Opens the store `config` names, with its master key.
///
/// # Errors
///
/// If the store can't be opened or migrated.
pub async fn open_store(config: &Config) -> anyhow::Result<Store> {
    let sealer = config.sealer()?;
    Store::open(&config.store.url, sealer)
        .await
        .context("opening the store at store.url")
}
