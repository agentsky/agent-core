//! [`StoreSurfaces`]: the surface each agent's bot acts through, from its
//! bindings in the store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use core_types::{AgentId, BindingId, ConvRef, Surface, SurfaceKind};
use store::{BindingState, Store, StoreError};
use surface_rocketchat::rest::Credentials;
use surface_rocketchat::{BotRoles, RocketChatConfig, RocketChatSurface};
use surface_slack::{SlackClient, SlackSurface, TeamDirectory};

use crate::commands::rocketchat::StoreDedup;
use crate::ctl::SurfaceLookup;

/// The surfaces agents' bots act through, built from their active bindings
/// and kept per binding.
///
/// - On Rocket.Chat, a [`RocketChatSurface`] with the bot's token, on the
///   server the manager bot's configuration names, sharing its
///   [`BotRoles`].
/// - On Slack, a [`SlackSurface`] with the binding's bot token and the
///   workspace's one [`TeamDirectory`], the manager app's. Every lookup
///   gives the directory the workspace's managed agents' bot users
///   ([`TeamDirectory::set_managed_bots`]), so an agent keeps a name a
///   human shares, and an agent installed since is named at once.
///
/// A binding that isn't active, or an agent without one on the
/// conversation's surface and team, has no surface.
pub struct StoreSurfaces {
    store: Store,
    rocketchat: Option<(RocketChatConfig, BotRoles)>,
    slack: Option<(SlackClient, Arc<TeamDirectory>)>,
    built: Mutex<HashMap<BindingId, Arc<dyn Surface>>>,
}

impl std::fmt::Debug for StoreSurfaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSurfaces")
            .field(
                "rocketchat",
                &self.rocketchat.as_ref().map(|(c, _)| &c.team),
            )
            .field("slack", &self.slack.as_ref().map(|(_, d)| d.team()))
            .finish_non_exhaustive()
    }
}

impl StoreSurfaces {
    /// Surfaces over `store`'s bindings: on the Rocket.Chat server whose
    /// manager configuration is `rocketchat` (each bot's surface copies it
    /// with the bot's credentials), and in the Slack workspace of
    /// `slack`'s directory.
    pub fn new(
        store: Store,
        rocketchat: Option<(RocketChatConfig, BotRoles)>,
        slack: Option<(SlackClient, Arc<TeamDirectory>)>,
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
            self.name_managed_bots(conv).await?;
        }
        if let Some(surface) = self.built().get(&binding.id) {
            return Ok(Some(Arc::clone(surface)));
        }
        let (Some(bot), Some(token)) = (binding.bot_user, self.store.bot_token(binding.id).await?)
        else {
            return Ok(None);
        };
        let surface: Arc<dyn Surface> = match conv.surface {
            SurfaceKind::RocketChat => {
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
                match RocketChatSurface::new(
                    config,
                    Arc::new(StoreDedup(self.store.clone())),
                    bots.clone(),
                ) {
                    Ok(surface) => Arc::new(surface),
                    Err(err) => {
                        tracing::error!(binding = %binding.id, error = %err, "couldn't set up an agent's Rocket.Chat surface");
                        return Ok(None);
                    }
                }
            }
            SurfaceKind::Slack => {
                let Some((client, directory)) = self
                    .slack
                    .as_ref()
                    .filter(|(_, directory)| *directory.team() == conv.team)
                else {
                    return Ok(None);
                };
                Arc::new(SlackSurface::new(client.bot(token), Arc::clone(directory)))
            }
        };
        self.built().insert(binding.id, Arc::clone(&surface));
        Ok(Some(surface))
    }

    /// Gives the workspace's directory its managed agents' bot users.
    async fn name_managed_bots(&self, conv: &ConvRef) -> Result<(), StoreError> {
        let Some((_, directory)) = self
            .slack
            .as_ref()
            .filter(|(_, directory)| *directory.team() == conv.team)
        else {
            return Ok(());
        };
        let bots = self
            .store
            .active_bots(SurfaceKind::Slack, &conv.team)
            .await?;
        directory.set_managed_bots(bots.into_iter().map(|bot| bot.bot.user));
        Ok(())
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
