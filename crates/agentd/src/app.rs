//! [`App`]: the state every part of agentd shares.

use std::sync::Arc;

use anyhow::Context as _;
use store::Store;

use crate::config::Config;

/// The shared state: the configuration and the store, and later the
/// surfaces, the runner and the credential proxy.
///
/// Cloning is cheap: every clone shares the same state. Axum handlers take it
/// as their state.
#[derive(Debug, Clone)]
pub struct App {
    config: Arc<Config>,
    store: Store,
}

impl App {
    /// An `App` over an already open `store`.
    pub fn new(config: Config, store: Store) -> Self {
        Self {
            config: Arc::new(config),
            store,
        }
    }

    /// Opens the store at `store.url` with the master key, running pending
    /// migrations, and builds the `App`.
    ///
    /// # Errors
    ///
    /// If the store can't be opened or migrated.
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        let store = open_store(&config).await?;
        Ok(Self::new(config, store))
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.store
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
