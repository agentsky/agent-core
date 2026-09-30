//! Wiring Slack into agentd.
//!
//! [`surface_slack`] serves the request URLs and verifies requests; this
//! module gives it what it needs from agentd:
//!
//! - [`ConfigSigningSecrets`]: the manager app's signing secret, from
//!   [`AGENTD_SLACK_MANAGER_SIGNING_SECRET`](crate::config::SLACK_MANAGER_SIGNING_SECRET_VAR),
//!   and its bot user; [`StoreSigningSecrets`] adds agent bindings' from the
//!   store in front of it.
//! - [`StoreDedup`]: deduplication in the store's `processed_events`.
//! - [`Inbound`]: where verified requests go. Commands to the manager app
//!   go to the [`CommandIntake`](crate::commands::intake::CommandIntake),
//!   and a `user_change` saying a member left deletes their configuration
//!   token. Messages to agents' apps go to [`Messages`], which looks their
//!   bot senders up and hands them to the turn pipeline outside the Slack
//!   queue. The rest is logged by kind and dropped.
//! - [`manager`]: the manager app itself.
//! - [`agents`]: agents' apps, created from manifests and installed through
//!   `GET /slack/oauth/callback`.
//! - [`bots`]: the surfaces agents' bots act through.
//!
//! agentd serves one workspace, the manager app's. Requests to any app from
//! another workspace, or naming none, are dropped, and agent bindings in
//! another workspace are unknown to the ingress.

pub mod agents;
pub mod bots;
pub mod manager;

use std::future::Future;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::routing::get;
use core_types::{InboundEvent, SendError, Sender, Sink, TeamId, UserId};
use secrecy::SecretString;
use store::Store;
use surface_slack::manifest::OAUTH_CALLBACK_PATH;
use surface_slack::{
    BindingRef, BoxError, Dedup, Queue, SigningSecrets, SlackApp, SlackEvent, SlackInbound, ingress,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::app::App;
use crate::commands::intake::CommandSubmitter;
use crate::commands::slack::{dm_command, member_who_left, slash_command};
use bots::SlackBots;
use manager::ManagerIdentity;

/// How many acknowledged Slack requests may wait to be handled. Beyond
/// that, requests get 503: Slack retries events, but not slash commands or
/// interactions.
pub const QUEUE_CAPACITY: usize = 1024;

/// How many agents' messages may wait for [`Messages`] to hand them to the
/// turn pipeline. Beyond that, they are dropped with a warning.
pub const MESSAGES_CAPACITY: usize = 256;

/// The router serving the Slack request URLs, and the queue behind it, and
/// with agent apps, their OAuth callback. Run the queue with
/// [`run_queue`].
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
    let secrets = StoreSigningSecrets::new(
        secrets,
        app.slack()
            .map(|slack| (app.store().clone(), slack.identity().team.clone())),
    );
    let (router, queue) = ingress(Arc::new(secrets), QUEUE_CAPACITY);
    let router = match app.slack_agents() {
        Some(agents) => router.merge(
            Router::new()
                .route(OAUTH_CALLBACK_PATH, get(agents::oauth_callback))
                .with_state(agents.clone()),
        ),
        None => router,
    };
    (router, queue)
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

/// Signing secrets for every binding: agent bindings' from the store, in the
/// workspace agentd serves, while they are `creating` (known, but without a
/// secret yet), `pending_install` or `active`; the manager app's from
/// [`ConfigSigningSecrets`]. Disabled bindings, those of deleted agents,
/// are unknown, so their requests get 404.
#[derive(Debug, Clone)]
pub struct StoreSigningSecrets {
    config: ConfigSigningSecrets,
    agents: Option<(Store, TeamId)>,
}

impl StoreSigningSecrets {
    /// The manager's secret from `config`, and agent bindings' from the
    /// store in the workspace given with it; without one, only the manager
    /// is known.
    pub fn new(config: ConfigSigningSecrets, agents: Option<(Store, TeamId)>) -> Self {
        Self { config, agents }
    }
}

#[async_trait::async_trait]
impl SigningSecrets for StoreSigningSecrets {
    async fn lookup(&self, binding: BindingRef) -> Result<Option<SlackApp>, BoxError> {
        let (BindingRef::Agent(id), Some((store, team))) = (binding, &self.agents) else {
            return self.config.lookup(binding).await;
        };
        Ok(store.slack_app_keys(id, team).await?.map(|keys| SlackApp {
            signing_secret: keys.signing_secret,
            bot_user: keys.bot_user,
        }))
    }
}

/// Where the Slack queue sends agents' messages, so it never waits for
/// them: they wait here, at most [`MESSAGES_CAPACITY`], for the worker
/// [`new`](Self::new) returns, which looks each one's binding up, fills in
/// its bot sender with [`SlackSurface::fill_bot_sender`], and hands it to the
/// turn pipeline once [`connect`](Self::connect)ed, one at a time in the
/// order they came. Until then, and in an agentd that runs no turns, they
/// are dropped, as are messages to a binding that isn't active (still
/// waiting for its install, or deleted).
///
/// Cloning is cheap and shares the queue and the connection.
///
/// [`SlackSurface::fill_bot_sender`]: surface_slack::SlackSurface::fill_bot_sender
#[derive(Debug, Clone)]
pub struct Messages {
    queue: mpsc::Sender<InboundEvent>,
    onward: Arc<OnceLock<Sender<InboundEvent>>>,
}

impl Messages {
    /// Messages to the agents whose bots are `bots`, and the worker that
    /// hands them on. The worker ends once every clone is dropped and the
    /// messages waiting are handed on.
    pub fn new(bots: SlackBots) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (queue, waiting) = mpsc::channel(MESSAGES_CAPACITY);
        let onward: Arc<OnceLock<Sender<InboundEvent>>> = Arc::default();
        let worker = forward(bots, waiting, Arc::clone(&onward));
        (Self { queue, onward }, worker)
    }

    /// Hands messages to `onward` from now on. Only the first call counts.
    pub fn connect(&self, onward: Sender<InboundEvent>) {
        if self.onward.set(onward).is_err() {
            tracing::warn!("Slack messages are connected already; ignored another connection");
        }
    }

    /// Queues `event` for the worker without waiting: dropped with a
    /// warning when the queue is full, and with a debug line when agentd
    /// runs no turns.
    fn hand(&self, event: InboundEvent) {
        let binding = event.binding;
        if self.onward.get().is_none() {
            tracing::debug!(%binding, "agentd runs no turns; dropped a message to an agent");
            return;
        }
        match self.queue.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(%binding, "too many agents' messages waiting; dropped one");
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(%binding, "agents' messages aren't handled any more; dropped one");
            }
        }
    }
}

/// The worker of [`Messages`].
async fn forward(
    bots: SlackBots,
    mut waiting: mpsc::Receiver<InboundEvent>,
    onward: Arc<OnceLock<Sender<InboundEvent>>>,
) {
    while let Some(mut event) = waiting.recv().await {
        let binding = event.binding;
        let Some(onward) = onward.get() else {
            tracing::debug!(%binding, "agentd runs no turns; dropped a message to an agent");
            continue;
        };
        let surface = match bots.surface(binding).await {
            Ok(Some(surface)) => surface,
            Ok(None) => {
                tracing::debug!(%binding, "a message to an agent's app that isn't active; dropped it");
                continue;
            }
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't look an agent's binding up; dropped its message");
                continue;
            }
        };
        if let Err(err) = surface.fill_bot_sender(&mut event).await {
            tracing::warn!(%binding, error = %err, "couldn't look a bot sender up; its message goes on as it is");
        }
        if onward.send(event).await.is_err() {
            tracing::warn!(%binding, "the turn pipeline is gone; dropped a message");
        }
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
/// Requests from any workspace but the one agentd serves, or naming none,
/// are dropped. Then:
///
/// - To the manager app: an `/agent` slash command or a DM to the app goes
///   to the command intake, and a `user_change` whose user is `deleted`
///   deletes that member's configuration token for the workspace.
/// - To an agent's app: a message goes to [`Messages`], without waiting.
///
/// Everything else is logged by binding and kind and dropped.
#[derive(Debug, Clone)]
pub struct Inbound {
    store: Store,
    manager: Option<(ManagerIdentity, CommandSubmitter)>,
    agents: Option<Messages>,
}

impl Inbound {
    /// Hands the manager app's commands to `commands` when agentd serves
    /// Slack (`manager` is the app), and deletes left members' tokens in
    /// `store`.
    pub fn new(store: Store, manager: Option<ManagerIdentity>, commands: CommandSubmitter) -> Self {
        Self {
            store,
            manager: manager.map(|identity| (identity, commands)),
            agents: None,
        }
    }

    /// Also hands messages to agents' apps to `messages`.
    pub fn with_agents(mut self, messages: Messages) -> Self {
        self.agents = Some(messages);
        self
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
        let Some((identity, commands)) = &self.manager else {
            tracing::debug!(%binding, kind, "agentd doesn't serve Slack; dropped a request");
            return Ok(());
        };
        if binding != BindingRef::MANAGER_ID {
            if item.team() != Some(&identity.team) {
                tracing::debug!(%binding, kind, team = ?item.team(), "a request to an agent's app from another workspace; dropped it");
                return Ok(());
            }
            match item {
                SlackInbound::Message(event) => match &self.agents {
                    Some(messages) => messages.hand(*event),
                    None => {
                        tracing::debug!(%binding, "agentd doesn't serve agents' apps; dropped a message")
                    }
                },
                _ => {
                    tracing::debug!(%binding, kind, "a request to an agent's app that isn't a message; dropped it")
                }
            }
            return Ok(());
        }
        if item.team() != Some(&identity.team) {
            tracing::debug!(
                kind,
                team = ?item.team(),
                "a request to the Slack manager app from another workspace; dropped it"
            );
            return Ok(());
        }
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
            Some((member, text, origin)) => commands.submit(member, text, origin, Vec::new()).await,
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
