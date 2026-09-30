//! Wiring Slack into agentd.
//!
//! [`surface_slack`] serves the request URLs and verifies requests; this
//! module gives it what it needs from agentd:
//!
//! - [`ConfigSigningSecrets`]: the manager app's signing secret, from
//!   [`AGENTD_SLACK_MANAGER_SIGNING_SECRET`](crate::config::SLACK_MANAGER_SIGNING_SECRET_VAR),
//!   and its bot user. Agent bindings' secrets come from the store once
//!   agent apps exist.
//! - [`StoreDedup`]: deduplication in the store's `processed_events`.
//! - [`Inbound`]: where verified requests go. Commands to the manager app
//!   go to the [`CommandIntake`](crate::commands::intake::CommandIntake),
//!   and a `user_change` saying a member left deletes their configuration
//!   token; the rest is logged by kind and dropped until the turn pipeline
//!   takes agents' messages.
//! - [`manager`]: the manager app itself.

pub mod manager;

use std::sync::Arc;

use axum::Router;
use core_types::{SendError, Sender, Sink, UserId};
use secrecy::SecretString;
use store::Store;
use surface_slack::{
    BindingRef, BoxError, Dedup, Queue, SigningSecrets, SlackApp, SlackEvent, SlackInbound, ingress,
};
use time::OffsetDateTime;

use crate::app::App;
use crate::commands::intake::CommandSubmitter;
use crate::commands::slack::{dm_command, member_who_left, slash_command};
use manager::ManagerIdentity;

/// How many acknowledged Slack requests may wait to be handled. Beyond
/// that, requests get 503: Slack retries events, but not slash commands or
/// interactions.
pub const QUEUE_CAPACITY: usize = 1024;

/// The router serving the Slack request URLs, and the queue behind it. Run
/// the queue with [`run_queue`].
pub fn routes(app: &App) -> (Router, Queue) {
    let secrets = ConfigSigningSecrets::new(
        app.config().secrets.slack_manager_signing_secret(),
        app.slack().map(|slack| slack.identity().bot_user.clone()),
    );
    if secrets.manager.is_some() {
        tracing::info!("Slack manager app: requests to /slack/b/manager/ are verified");
    } else {
        tracing::info!("Slack manager app: not configured; /slack/b/manager/ answers 404");
    }
    ingress(Arc::new(secrets), QUEUE_CAPACITY)
}

/// Handles the queue until it closes: deduplicates through the store and
/// hands each request to `out`, normally an [`Inbound`].
pub async fn run_queue(queue: Queue, store: Store, out: Sender<SlackInbound>) {
    queue.run(Arc::new(StoreDedup(store)), out).await;
}

/// Signing secrets from configuration: the manager app's only.
#[derive(Debug, Clone)]
pub struct ConfigSigningSecrets {
    manager: Option<SecretString>,
    bot_user: Option<UserId>,
}

impl ConfigSigningSecrets {
    /// Knows the manager binding when `manager` holds its signing secret,
    /// with `bot_user` as its bot user, and no other binding.
    pub fn new(manager: Option<&SecretString>, bot_user: Option<UserId>) -> Self {
        Self {
            manager: manager.cloned(),
            bot_user,
        }
    }
}

#[async_trait::async_trait]
impl SigningSecrets for ConfigSigningSecrets {
    async fn lookup(&self, binding: BindingRef) -> Result<Option<SlackApp>, BoxError> {
        Ok(match binding {
            BindingRef::Manager => self.manager.clone().map(|secret| SlackApp {
                signing_secret: Some(secret),
                bot_user: self.bot_user.clone(),
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
        Ok(self
            .0
            .mark_event_processed(source, key, OffsetDateTime::now_utc())
            .await?)
    }
}

/// Where the Slack queue hands verified requests.
///
/// Requests to the manager app: an `/agent` slash command or a DM to the
/// app goes to the command intake, and a `user_change` whose user is
/// `deleted` deletes that member's configuration token for the workspace.
/// Everything else, agents' messages included, is logged by binding and
/// kind and dropped until the turn pipeline (T31) takes it.
#[derive(Debug, Clone)]
pub struct Inbound {
    store: Store,
    manager: Option<(ManagerIdentity, CommandSubmitter)>,
}

impl Inbound {
    /// Hands the manager app's commands to `commands` when agentd serves
    /// Slack (`manager` is the app), and deletes left members' tokens in
    /// `store`.
    pub fn new(store: Store, manager: Option<ManagerIdentity>, commands: CommandSubmitter) -> Self {
        Self {
            store,
            manager: manager.map(|identity| (identity, commands)),
        }
    }

    async fn member_left(&self, event: &SlackEvent) {
        let Some(key) = member_who_left(event) else {
            return;
        };
        let deleted = match self.store.member_for_identity(&key).await {
            Ok(Some(member)) => {
                self.store
                    .delete_slack_config_token(member, &key.team)
                    .await
            }
            Ok(None) => Ok(false),
            Err(err) => Err(err),
        };
        match deleted {
            Ok(true) => {
                tracing::info!(member = %key, "a member left the workspace; deleted their configuration token")
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(member = %key, error = %err, "couldn't delete the configuration token of a member who left")
            }
        }
    }
}

#[async_trait::async_trait]
impl Sink<SlackInbound> for Inbound {
    async fn send(&self, item: SlackInbound) -> Result<(), SendError> {
        let (binding, kind) = (item.binding(), item.kind());
        let manager = self
            .manager
            .as_ref()
            .filter(|_| binding == BindingRef::MANAGER_ID);
        let Some((identity, commands)) = manager else {
            tracing::debug!(%binding, kind, "no handler for this Slack request yet; dropped it");
            return Ok(());
        };
        let command = match item {
            SlackInbound::Command(command) => slash_command(command),
            SlackInbound::Message(event) => dm_command(&event, identity),
            SlackInbound::Event(event) => {
                self.member_left(&event).await;
                None
            }
            SlackInbound::Interaction(_) => None,
        };
        match command {
            Some((member, text, origin)) => commands.submit(member, text, origin).await,
            None => {
                tracing::debug!(
                    kind,
                    "a request to the Slack manager app that isn't a command"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;

    #[tokio::test]
    async fn only_the_manager_is_known_and_only_with_a_secret() {
        let secret = SecretString::from("manager-secret");
        let secrets = ConfigSigningSecrets::new(Some(&secret), Some(UserId::new("U0MANAGER")));
        let manager = secrets.lookup(BindingRef::Manager).await.unwrap().unwrap();
        assert_eq!(
            manager.signing_secret.unwrap().expose_secret(),
            "manager-secret"
        );
        assert_eq!(manager.bot_user, Some(UserId::new("U0MANAGER")));
        let agent = BindingRef::Agent(core_types::BindingId::new_v4());
        assert!(secrets.lookup(agent).await.unwrap().is_none());

        let none = ConfigSigningSecrets::new(None, None);
        assert!(none.lookup(BindingRef::Manager).await.unwrap().is_none());
        assert!(!format!("{secrets:?}").contains("manager-secret"));
    }
}
