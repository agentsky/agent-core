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
/// - [`surface`](Self::surface) is what the Slack queue needs for each
///   message: the receiving binding's surface, to look bot senders up with
///   [`SlackSurface::fill_bot_sender`].
/// - [`started`](Self::started) is what a reply needs: the first time in
///   this process, it also awaits the member list, so the binding's first
///   reply already resolves `@Name` mentions.
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
    built: Mutex<HashMap<BindingId, Built>>,
}

struct Built {
    surface: Arc<SlackSurface>,
    started: bool,
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

    fn built(&self) -> MutexGuard<'_, HashMap<BindingId, Built>> {
        self.inner
            .built
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The surface of `binding`, if it is an active Slack binding in this
    /// workspace with a bot token.
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
        if let Some(built) = self.built().get(&binding) {
            return Ok(Some(Arc::clone(&built.surface)));
        }
        let Some(token) = store.bot_token(binding).await? else {
            return Ok(None);
        };
        let surface = Arc::new(SlackSurface::new(
            self.inner.client.bot(token),
            Arc::clone(&self.inner.directory),
        ));
        let mut built = self.built();
        let entry = built.entry(binding).or_insert(Built {
            surface,
            started: false,
        });
        Ok(Some(Arc::clone(&entry.surface)))
    }

    /// Like [`surface`](Self::surface), and the first time this process
    /// uses the binding for a reply, awaits
    /// [`SlackSurface::refresh_members`], so the reply resolves names. A
    /// failed refresh is logged; rendering then uses what the directory
    /// has.
    ///
    /// # Errors
    ///
    /// As for [`surface`](Self::surface).
    pub async fn started(
        &self,
        binding: BindingId,
    ) -> Result<Option<Arc<SlackSurface>>, StoreError> {
        let Some(surface) = self.surface(binding).await? else {
            return Ok(None);
        };
        let first = self
            .built()
            .get_mut(&binding)
            .is_some_and(|built| !std::mem::replace(&mut built.started, true));
        if first && let Err(err) = surface.refresh_members().await {
            tracing::warn!(%binding, error = %err, "couldn't read the Slack member list for a new binding");
        }
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
            .active_bots(SurfaceKind::Slack, self.team())
            .await?;
        self.inner
            .directory
            .set_managed_bots(bots.into_iter().map(|bot| bot.bot.user));
        Ok(())
    }
}
