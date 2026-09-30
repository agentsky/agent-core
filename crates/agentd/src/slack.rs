//! Wiring the Slack ingress into agentd.
//!
//! [`surface_slack`] serves the request URLs and verifies requests; this
//! module gives it what it needs from agentd:
//!
//! - [`ConfigSigningSecrets`]: the manager app's signing secret, from
//!   [`AGENTD_SLACK_MANAGER_SIGNING_SECRET`](crate::config::SLACK_MANAGER_SIGNING_SECRET_VAR).
//!   Agent bindings' secrets come from the store once agent apps exist.
//! - [`StoreDedup`]: deduplication in the store's `processed_events`.
//! - [`Unrouted`]: where verified requests go until the command handlers and
//!   the turn pipeline take them. It logs each one's kind and drops it.

use std::sync::Arc;

use axum::Router;
use core_types::{SendError, Sender, Sink};
use secrecy::SecretString;
use store::Store;
use surface_slack::{
    BindingRef, BoxError, Dedup, Queue, SigningSecrets, SlackApp, SlackInbound, ingress,
};

use crate::app::App;

/// How many acknowledged Slack requests may wait to be handled. Beyond
/// that, requests get 503 and Slack retries them.
pub const QUEUE_CAPACITY: usize = 1024;

/// The router serving the Slack request URLs, and the queue behind it. Run
/// the queue with [`run_queue`].
pub fn routes(app: &App) -> (Router, Queue) {
    let secrets = ConfigSigningSecrets::new(app.config().secrets.slack_manager_signing_secret());
    if secrets.manager.is_some() {
        tracing::info!("Slack manager app: requests to /slack/b/manager/ are verified");
    } else {
        tracing::info!("Slack manager app: not configured; /slack/b/manager/ answers 404");
    }
    ingress(Arc::new(secrets), QUEUE_CAPACITY)
}

/// Handles the queue until it closes: deduplicates through the store and
/// hands each request to `out`, which is [`Unrouted`] until the command
/// handlers and the turn pipeline exist.
pub async fn run_queue(queue: Queue, store: Store, out: Sender<SlackInbound>) {
    queue.run(Arc::new(StoreDedup(store)), out).await;
}

/// Signing secrets from configuration: the manager app's only.
#[derive(Debug, Clone)]
pub struct ConfigSigningSecrets {
    manager: Option<SecretString>,
}

impl ConfigSigningSecrets {
    /// Knows the manager binding when `manager` holds its signing secret,
    /// and no other binding.
    pub fn new(manager: Option<&SecretString>) -> Self {
        Self {
            manager: manager.cloned(),
        }
    }
}

#[async_trait::async_trait]
impl SigningSecrets for ConfigSigningSecrets {
    async fn lookup(&self, binding: BindingRef) -> Result<Option<SlackApp>, BoxError> {
        Ok(match binding {
            BindingRef::Manager => self.manager.clone().map(|secret| SlackApp {
                signing_secret: Some(secret),
                bot_user: None,
            }),
            BindingRef::Agent(_) => None,
        })
    }
}

/// Deduplication in the store's `processed_events`, which the sweeper
/// empties after [`store::PROCESSED_EVENT_RETENTION`].
#[derive(Debug, Clone)]
pub struct StoreDedup(pub Store);

#[async_trait::async_trait]
impl Dedup for StoreDedup {
    async fn first_time(&self, source: &str, key: &str) -> Result<bool, BoxError> {
        Ok(self.0.mark_event_processed(source, key).await?)
    }
}

/// Drops every request after logging its binding and kind: nothing
/// consumes Slack requests yet.
#[derive(Debug, Clone, Copy)]
pub struct Unrouted;

#[async_trait::async_trait]
impl Sink<SlackInbound> for Unrouted {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        tracing::debug!(
            binding = %item.binding(),
            kind = item.kind(),
            "no handler for Slack requests yet; dropped one"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;

    #[tokio::test]
    async fn only_the_manager_is_known_and_only_with_a_secret() {
        let secret = SecretString::from("manager-secret");
        let secrets = ConfigSigningSecrets::new(Some(&secret));
        let manager = secrets.lookup(BindingRef::Manager).await.unwrap().unwrap();
        assert_eq!(
            manager.signing_secret.unwrap().expose_secret(),
            "manager-secret"
        );
        assert_eq!(manager.bot_user, None);
        let agent = BindingRef::Agent(core_types::BindingId::new_v4());
        assert!(secrets.lookup(agent).await.unwrap().is_none());

        let none = ConfigSigningSecrets::new(None);
        assert!(none.lookup(BindingRef::Manager).await.unwrap().is_none());
        assert!(!format!("{secrets:?}").contains("manager-secret"));
    }
}
