//! [`SlackBots`]: the [`SlackSurface`] of each agent's bot in the workspace
//! agentd serves.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use core_types::{BindingId, SurfaceKind, TeamId};
use store::{BindingState, Store, StoreError};
use surface_slack::{SlackClient, SlackSurface, TeamDirectory};

/// The surfaces agents' Slack bots act through, one per active binding,
/// built from the store on first use and kept. Every binding shares the
/// workspace's one [`TeamDirectory`], the manager app's.
///
/// - [`surface`](Self::surface) is the binding's surface, for looking bot
///   senders up with [`SlackSurface::fill_bot_sender`] and for replies. It
///   never waits for the workspace's member list: when the directory has
///   none, or a stale one, it starts reading it in the background
///   ([`SlackSurface::refresh_in_background`]), which a turn's reply then
///   renders with.
/// - [`name_managed`](Self::name_managed) passes the workspace's active
///   agents' bot users to [`TeamDirectory::set_managed_bots`]. agentd calls
///   it whenever those change here (an install, a deletion) and on every
///   lookup of an agent's surface, which also picks up what another
///   instance changed.
///
/// A binding that isn't active, or not in this workspace, has no surface;
/// that is checked in the store on every call, so a deleted agent's bot
/// stops at once. Cloning is cheap and shares everything.
#[derive(Clone)]
pub struct SlackBots {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    client: SlackClient,
    directory: Arc<TeamDirectory>,
    built: Mutex<HashMap<BindingId, Arc<SlackSurface>>>,
}

impl fmt::Debug for SlackBots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackBots")
            .field("team", self.team())
            .field("built", &self.built().len())
            .finish_non_exhaustive()
    }
}

impl SlackBots {
    /// The bots of `store`'s Slack bindings in `directory`'s workspace,
    /// acting through `client`.
    pub fn new(store: Store, client: SlackClient, directory: Arc<TeamDirectory>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                client,
                directory,
                built: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The workspace.
    pub fn team(&self) -> &TeamId {
        self.inner.directory.team()
    }

    fn built(&self) -> MutexGuard<'_, HashMap<BindingId, Arc<SlackSurface>>> {
        self.inner
            .built
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The surface of `binding`, if it is an active Slack binding in this
    /// workspace with a bot token. Starts reading the member list in the
    /// background when the directory has none or a stale one.
    ///
    /// # Errors
    ///
    /// If the store can't be read, or the token doesn't decrypt.
    pub async fn surface(
        &self,
        binding: BindingId,
    ) -> Result<Option<Arc<SlackSurface>>, StoreError> {
        let store = &self.inner.store;
        let Some(row) = store.binding(binding).await? else {
            return Ok(None);
        };
        if row.surface != SurfaceKind::Slack
            || row.team != *self.team()
            || row.state != BindingState::Active
        {
            return Ok(None);
        }
        let built = self.built().get(&binding).map(Arc::clone);
        let surface = match built {
            Some(surface) => surface,
            None => {
                let Some(token) = store.bot_token(binding).await? else {
                    return Ok(None);
                };
                let surface = Arc::new(SlackSurface::new(
                    self.inner.client.bot(token),
                    Arc::clone(&self.inner.directory),
                ));
                Arc::clone(self.built().entry(binding).or_insert(surface))
            }
        };
        surface.refresh_in_background();
        Ok(Some(surface))
    }

    /// Gives the workspace's directory the bot users of its active agent
    /// bindings, so a name an agent shares with a human resolves to the
    /// agent.
    ///
    /// # Errors
    ///
    /// If the store can't be read.
    pub async fn name_managed(&self) -> Result<(), StoreError> {
        let bots = self
            .inner
            .store
            .active_bot_users(SurfaceKind::Slack, self.team())
            .await?;
        self.inner.directory.set_managed_bots(bots);
        Ok(())
    }
}
