//! [`StoreSurfaces`]: the surface each agent's bot acts through, from its
//! bindings in the store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use core_types::{AgentId, BindingId, ConvRef, Surface, SurfaceKind};
use store::{BindingState, Store, StoreError};
use surface_rocketchat::rest::Credentials;
use surface_rocketchat::{BotRoles, RocketChatConfig, RocketChatSurface};

use crate::commands::rocketchat::StoreDedup;
use crate::ctl::SurfaceLookup;
use crate::slack::bots::SlackBots;

/// The surfaces agents' bots act through, built from their active bindings
/// and kept per binding.
///
/// - On Rocket.Chat, a [`RocketChatSurface`] with the bot's token, on the
///   server the manager bot's configuration names, sharing its
///   [`BotRoles`].
/// - On Slack, the binding's surface from [`SlackBots`], which the first
///   time also awaits the workspace's member list. Every lookup gives the
///   directory the workspace's managed agents' bot users
///   ([`SlackBots::name_managed`]), so an agent keeps a name a human
///   shares, and an agent installed since, here or on another instance, is
///   named at once.
///
/// A binding that isn't active, or an agent without one on the
/// conversation's surface and team, has no surface.
pub struct StoreSurfaces {
    store: Store,
    rocketchat: Option<(RocketChatConfig, BotRoles)>,
    slack: Option<SlackBots>,
    built: Mutex<HashMap<BindingId, Arc<dyn Surface>>>,
}

impl std::fmt::Debug for StoreSurfaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSurfaces")
            .field(
                "rocketchat",
                &self.rocketchat.as_ref().map(|(c, _)| &c.team),
            )
            .field("slack", &self.slack.as_ref().map(SlackBots::team))
            .finish_non_exhaustive()
    }
}

impl StoreSurfaces {
    /// Surfaces over `store`'s bindings: on the Rocket.Chat server whose
    /// manager configuration is `rocketchat` (each bot's surface copies it
    /// with the bot's credentials), and among `slack`'s bots in the
    /// workspace agentd serves.
    pub fn new(
        store: Store,
        rocketchat: Option<(RocketChatConfig, BotRoles)>,
        slack: Option<SlackBots>,
    ) -> Self {
        Self {
            store,
            rocketchat,
            slack,
            built: Mutex::new(HashMap::new()),
        }
    }

    fn built(&self) -> MutexGuard<'_, HashMap<BindingId, Arc<dyn Surface>>> {
        self.built.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn lookup(
        &self,
        agent: AgentId,
        conv: &ConvRef,
    ) -> Result<Option<Arc<dyn Surface>>, StoreError> {
        let Some(binding) = self
            .store
            .bindings_of(agent)
            .await?
            .into_iter()
            .find(|binding| {
                binding.surface == conv.surface
                    && binding.team == conv.team
                    && binding.state == BindingState::Active
            })
        else {
            return Ok(None);
        };
        if conv.surface == SurfaceKind::Slack {
            let Some(bots) = self.slack.as_ref().filter(|bots| *bots.team() == conv.team) else {
                return Ok(None);
            };
            bots.name_managed().await?;
            let surface = bots.started(binding.id).await?;
            return Ok(surface.map(|surface| surface as Arc<dyn Surface>));
        }
        if let Some(surface) = self.built().get(&binding.id) {
            return Ok(Some(Arc::clone(surface)));
        }
        let (Some(bot), Some(token)) = (binding.bot_user, self.store.bot_token(binding.id).await?)
        else {
            return Ok(None);
        };
        let Some((template, bots)) = self
            .rocketchat
            .as_ref()
            .filter(|(template, _)| template.team == conv.team)
        else {
            return Ok(None);
        };
        let mut config = template.clone();
        config.credentials = Credentials {
            user_id: bot,
            token,
        };
        let surface: Arc<dyn Surface> = match RocketChatSurface::new(
            config,
            Arc::new(StoreDedup(self.store.clone())),
            bots.clone(),
        ) {
            Ok(surface) => Arc::new(surface),
            Err(err) => {
                tracing::error!(binding = %binding.id, error = %err, "couldn't set up an agent's Rocket.Chat surface");
                return Ok(None);
            }
        };
        self.built().insert(binding.id, Arc::clone(&surface));
        Ok(Some(surface))
    }
}

#[async_trait::async_trait]
impl SurfaceLookup for StoreSurfaces {
    async fn surface(&self, agent: AgentId, conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        match self.lookup(agent, conv).await {
            Ok(surface) => surface,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "looking up an agent's surface failed");
                None
            }
        }
    }
}
