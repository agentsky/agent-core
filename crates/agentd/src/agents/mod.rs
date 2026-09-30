//! Agents' bot identities on Rocket.Chat, and the connections agentd keeps
//! as them.
//!
//! - [`RocketChatAgents`]: the manager's side of an agent's life. It creates
//!   an agent's bot user and token, retires (deactivates) the bot users of
//!   deleted agents, and makes the other Rocket.Chat calls the agent
//!   commands need.
//! - [`Supervisor`]: one realtime connection per active binding, kept in
//!   step with the store at startup, whenever a command pokes it, and every
//!   [`SUPERVISE_INTERVAL`].
//! - [`Acknowledge`]: where every connection passes the messages that
//!   aren't commands, until turns exist (T23).
//!
//! # Durability
//!
//! The store is the only record of what should exist. Creating an agent
//! first stores it with a `creating` binding, so a creation that crashes
//! halfway is found and [abandoned](Store::abandon_creation) once
//! [`CREATION_LEASE`] has passed. A deleted agent's bot user owes
//! retirement until it is deactivated: each attempt claims it with a lease,
//! a failure is retried after a backoff, and a crash mid-attempt leaves it
//! to the next claim once the lease ends. Connections are derived from the
//! active bindings, so a restart, or another instance creating or deleting
//! an agent, is picked up by the next pass.

mod ack;
mod supervisor;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use core_types::{BindingId, ConversationId, SurfaceError, SurfaceKind, TeamId, UserId};
use store::{Store, StoreError};
use surface_rocketchat::rest::{Credentials, NewBotUser, RestClient, RoomType};
use time::OffsetDateTime;
use tokio::sync::Notify;

pub use ack::{ACK_EMOJI, Acknowledge};
pub use supervisor::{SUPERVISE_INTERVAL, Supervisor};

/// How long a creation may take before it counts as abandoned. A creation
/// is a handful of REST calls, each bounded by a 30-second timeout and at
/// most one rate-limit wait.
pub const CREATION_LEASE: Duration = Duration::from_secs(10 * 60);

/// How long a retirement claim keeps other attempts away.
pub const RETIRE_LEASE: Duration = Duration::from_secs(10 * 60);

/// How long after the first failed retirement it is tried again.
pub const RETIRE_BACKOFF_INITIAL: Duration = Duration::from_secs(60);

/// The longest wait between retirement attempts.
pub const RETIRE_BACKOFF_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// How many attempts a retirement gets: about three days of retries.
pub const RETIRE_MAX_ATTEMPTS: u32 = 20;

/// The name of the personal access token each bot is given.
pub const BOT_TOKEN_NAME: &str = "agentd";

/// Usernames Rocket.Chat reads as broadcasts, so a bot can't be mentioned
/// by them.
const BROADCAST_NAMES: &[&str] = &["all", "here"];

/// How long to wait after failed retirement attempt number `attempt`.
fn backoff(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(31);
    RETIRE_BACKOFF_INITIAL
        .saturating_mul(1 << doublings)
        .min(RETIRE_BACKOFF_MAX)
}

/// A bot user created for an agent, from [`RocketChatAgents::create_bot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedBot {
    /// Its user id.
    pub user: UserId,
    /// Its username, what members type after `@`.
    pub username: String,
}

/// Why [`RocketChatAgents::create_bot`] failed. The binding's creation is
/// abandoned in every case.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// Every username tried is taken.
    #[error("the usernames {0:?} are taken")]
    NamesTaken(Vec<String>),
    /// The creation was abandoned meanwhile, after [`CREATION_LEASE`].
    #[error("the creation was abandoned")]
    Abandoned,
    /// Rocket.Chat refused or failed.
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The manager's side of agents on one Rocket.Chat server.
///
/// Cloning is cheap and shares everything.
#[derive(Debug, Clone)]
pub struct RocketChatAgents {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    rest: RestClient,
    team: TeamId,
    avatar_url: Option<String>,
    wake: Notify,
}

impl RocketChatAgents {
    /// Agents on the server `rest` talks to, as the manager, whose
    /// identities carry `team`. New bots set `avatar_url` as their avatar,
    /// if given.
    pub fn new(store: Store, rest: RestClient, team: TeamId, avatar_url: Option<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                rest,
                team,
                avatar_url,
                wake: Notify::new(),
            }),
        }
    }

    /// The team every identity on this server carries.
    pub fn team(&self) -> &TeamId {
        &self.inner.team
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The manager's REST client.
    pub fn rest(&self) -> &RestClient {
        &self.inner.rest
    }

    /// Asks the [`Supervisor`] for a pass now, as after creating or
    /// deleting an agent. Pokes while a pass runs lead to one more pass.
    pub fn poke(&self) {
        self.inner.wake.notify_one();
    }

    /// Completes at the next [`poke`](Self::poke), or at once if one came
    /// since the last call.
    pub(crate) async fn poked(&self) {
        self.inner.wake.notified().await;
    }

    /// The username of `user`.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] if `users.info` fails.
    pub async fn username(&self, user: &UserId) -> Result<String, SurfaceError> {
        Ok(self.inner.rest.user_info(user).await?.username)
    }

    /// The id of the user named `username`, or `None` if there is none.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] other than the server not knowing the name.
    pub async fn user_named(&self, username: &str) -> Result<Option<UserId>, SurfaceError> {
        match self.inner.rest.user_by_username(username).await {
            Ok(user) => Ok(Some(user.id)),
            Err(SurfaceError::Api(_) | SurfaceError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Downloads the file `id` named `name` from a message the manager
    /// received, refusing more than `max` bytes.
    ///
    /// # Errors
    ///
    /// A [`SurfaceError`] as for [`RestClient::download`].
    pub async fn download(&self, id: &str, name: &str, max: u64) -> Result<Bytes, SurfaceError> {
        self.inner.rest.download(id, name, max).await
    }

    /// Adds the bot user `bot` to `room`, a channel or private group the
    /// manager is in.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Unsupported`] for a direct message, and any error
    /// `rooms.info` or the invite returns, such as
    /// [`SurfaceError::Forbidden`] where the manager isn't a member.
    pub async fn invite(&self, room: &ConversationId, bot: &UserId) -> Result<(), SurfaceError> {
        let info = self.inner.rest.room_info(room).await?;
        match info.room_type {
            RoomType::Channel | RoomType::Group => {
                self.inner.rest.invite(room, &info.room_type, bot).await
            }
            _ => Err(SurfaceError::Unsupported("inviting into this room type")),
        }
    }

    /// Creates the bot user for the `creating` binding `binding` of the
    /// agent `name`, owned by the member whose username is `owner`, obtains
    /// its token and activates the binding.
    ///
    /// The username is `name`, or `<owner>-<name>` when that is taken (or
    /// is a broadcast name, `all` or `here`). The display name is `name`.
    /// The bot user is recorded on the binding as soon as it exists, so a
    /// failure after that leaves it to be retired. On any failure the
    /// creation is abandoned, which deletes the agent, and the bot user,
    /// if one was made, is retired.
    ///
    /// # Errors
    ///
    /// [`CreateError`]; the caller only reports it.
    pub async fn create_bot(
        &self,
        binding: BindingId,
        name: &str,
        owner: &str,
    ) -> Result<CreatedBot, CreateError> {
        let result = self.try_create_bot(binding, name, owner).await;
        if result.is_err() {
            self.abandon(binding).await;
        }
        result
    }

    async fn try_create_bot(
        &self,
        binding: BindingId,
        name: &str,
        owner: &str,
    ) -> Result<CreatedBot, CreateError> {
        let store = &self.inner.store;
        let rest = &self.inner.rest;
        let prefixed = format!("{owner}-{name}");
        let usernames: Vec<&str> = if BROADCAST_NAMES.contains(&name) {
            vec![&prefixed]
        } else {
            vec![name, &prefixed]
        };
        let email = format!("agent-{binding}@agent-core.invalid");
        let mut created = None;
        for username in &usernames {
            let new = NewBotUser {
                username,
                name,
                email: &email,
            };
            match rest.create_bot_user(&new).await {
                Ok(user) => {
                    created = Some(user);
                    break;
                }
                Err(SurfaceError::Api(message)) if message.contains("error-field-unavailable") => {}
                Err(err) => return Err(err.into()),
            }
        }
        let Some((user, password)) = created else {
            return Err(CreateError::NamesTaken(
                usernames.iter().map(|&u| u.to_owned()).collect(),
            ));
        };
        if !store
            .set_binding_bot_user(binding, &user.id, &user.username)
            .await?
        {
            self.deactivate_orphan(&user.id).await;
            return Err(CreateError::Abandoned);
        }
        let credentials = rest
            .issue_bot_token(&user.username, password, BOT_TOKEN_NAME)
            .await?;
        if let Some(url) = &self.inner.avatar_url
            && let Err(err) = rest
                .with_credentials(credentials.clone())
                .set_avatar(&user.id, url)
                .await
        {
            tracing::warn!(%binding, error = %err, "couldn't set a new bot's avatar");
        }
        if !store
            .activate_binding(binding, &credentials.token, OffsetDateTime::now_utc())
            .await?
        {
            return Err(CreateError::Abandoned);
        }
        tracing::info!(%binding, bot = %user.id, "created an agent's bot user");
        Ok(CreatedBot {
            user: user.id,
            username: user.username,
        })
    }

    /// Deactivates a bot user no binding records, best effort.
    async fn deactivate_orphan(&self, user: &UserId) {
        if let Err(err) = self.inner.rest.set_active(user, false).await {
            tracing::warn!(bot = %user, error = %err, "couldn't deactivate a bot user no binding records");
        }
    }

    /// Abandons the creation of `binding` now, and retires its bot user if
    /// it has one.
    async fn abandon(&self, binding: BindingId) {
        let now = OffsetDateTime::now_utc();
        match self.inner.store.abandon_creation(binding, now, now).await {
            Ok(true) => {
                if let Err(err) = self.retire(binding).await {
                    tracing::warn!(%binding, error = %err, "couldn't retire an abandoned bot user");
                }
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't abandon a failed creation");
            }
        }
    }

    /// Abandons every creation on this server that started more than
    /// [`CREATION_LEASE`] ago, and returns how many.
    ///
    /// # Errors
    ///
    /// A [`StoreError`]; creations already abandoned stay abandoned.
    pub async fn abandon_stale(&self) -> Result<usize, StoreError> {
        let now = OffsetDateTime::now_utc();
        let before = now - CREATION_LEASE;
        let store = &self.inner.store;
        let mut abandoned = 0;
        for binding in store
            .stale_creations(SurfaceKind::RocketChat, &self.inner.team, before)
            .await?
        {
            if store.abandon_creation(binding, before, now).await? {
                tracing::warn!(%binding, "abandoned an agent creation that never finished");
                abandoned += 1;
            }
        }
        Ok(abandoned)
    }

    /// Retires the bot user of the disabled `binding`: claims the
    /// retirement, deactivates the bot user, and marks it retired, or
    /// defers the next attempt if Rocket.Chat refused. A bot user that no
    /// longer exists counts as retired. Returns whether it is retired now;
    /// false also when there is nothing to claim.
    ///
    /// # Errors
    ///
    /// A [`StoreError`].
    pub async fn retire(&self, binding: BindingId) -> Result<bool, StoreError> {
        let store = &self.inner.store;
        let Some(bot) = store.binding(binding).await?.and_then(|b| b.bot_user) else {
            return Ok(false);
        };
        let now = OffsetDateTime::now_utc();
        let Some(attempt) = store
            .claim_retirement(binding, now, now + RETIRE_LEASE, RETIRE_MAX_ATTEMPTS)
            .await?
        else {
            return Ok(false);
        };
        match self.inner.rest.set_active(&bot, false).await {
            Ok(()) | Err(SurfaceError::NotFound(_)) => {
                store
                    .mark_retired(binding, OffsetDateTime::now_utc())
                    .await?;
                tracing::info!(%binding, %bot, "deactivated a deleted agent's bot user");
                Ok(true)
            }
            Err(err) => {
                tracing::warn!(%binding, %bot, attempt, error = %err, "couldn't deactivate a deleted agent's bot user");
                let retry_at = OffsetDateTime::now_utc() + backoff(attempt);
                store.defer_retirement(binding, retry_at).await?;
                if attempt >= RETIRE_MAX_ATTEMPTS {
                    tracing::warn!(%binding, %bot, "giving up on deactivating the bot user");
                }
                Ok(false)
            }
        }
    }

    /// Retires every bot user on this server that owes it and may be
    /// claimed now, and returns how many were retired.
    ///
    /// # Errors
    ///
    /// A [`StoreError`]; bot users already retired stay retired.
    pub async fn retire_pending(&self) -> Result<usize, StoreError> {
        let pending = self
            .inner
            .store
            .pending_retirements(
                SurfaceKind::RocketChat,
                &self.inner.team,
                OffsetDateTime::now_utc(),
                RETIRE_MAX_ATTEMPTS,
            )
            .await?;
        let mut retired = 0;
        for retirement in pending {
            if self.retire(retirement.binding).await? {
                retired += 1;
            }
        }
        Ok(retired)
    }

    /// A REST client acting as a bot, sharing the manager's connection
    /// pool.
    pub(crate) fn as_bot(&self, credentials: Credentials) -> RestClient {
        self.inner.rest.with_credentials(credentials)
    }
}

#[cfg(test)]
mod tests;
