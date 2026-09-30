//! [`App`]: the state every part of agentd shares.

use std::sync::Arc;

use anyhow::Context as _;
use store::Store;

use crate::config::Config;
use crate::ctl::{Ctl, CtlSettings, NoSurfaces, SurfaceLookup};

/// The shared state: the configuration, the store and the agentctl API, and
/// later the surfaces, the runner and the credential proxy.
///
/// Cloning is cheap: every clone shares the same state. Axum handlers take it
/// as their state.
#[derive(Debug, Clone)]
pub struct App {
    config: Arc<Config>,
    store: Store,
    ctl: Ctl,
}

impl App {
    /// An `App` over an already open `store`, with no surfaces for
    /// `agentctl history` yet.
    pub fn new(config: Config, store: Store) -> Self {
        Self::with_surfaces(config, store, Arc::new(NoSurfaces))
    }

    /// An `App` over an already open `store`, whose agentctl API finds
    /// surfaces through `surfaces`.
    pub fn with_surfaces(config: Config, store: Store, surfaces: Arc<dyn SurfaceLookup>) -> Self {
        let ctl = Ctl::new(store.clone(), CtlSettings::from_config(&config), surfaces);
        Self {
            config: Arc::new(config),
            store,
            ctl,
        }
    }

    /// Opens the store at `store.url` with the master key, running pending
    /// migrations, builds the `App`, and deletes every agentctl token,
    /// scope lock and staged attachment left from before (see
    /// [`Ctl::purge`]).
    ///
    /// # Errors
    ///
    /// If the store can't be opened or migrated, or the purge fails.
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        let store = open_store(&config).await?;
        let app = Self::new(config, store);
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
