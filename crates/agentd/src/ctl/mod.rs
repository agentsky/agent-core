//! The agentctl API, served on the ctl listener: the only way a sandboxed
//! agent calls back into agentd.
//!
//! # Tokens
//!
//! Each `claude` process gets one token, from
//! [`issue_process_token`](Ctl::issue_process_token), in its
//! `AGENTCTL_TOKEN`. A warm process is fed turns over stdin and its
//! environment is fixed at start, so a token issued per turn could never
//! reach it. Instead agentd records the current turn on the token with
//! [`begin_turn`](Ctl::begin_turn) and clears it with
//! [`end_turn`](Ctl::end_turn), and a token authorizes nothing between
//! turns. [`revoke_process_token`](Ctl::revoke_process_token) deletes it
//! when the process stops. The runner's `TurnHooks` (T21) reach these
//! through agentd (T23).
//!
//! A token is 32 random bytes. The store keeps only its SHA-256 digest, in
//! `ctl_tokens`, with the session, agent, volume and container address it
//! was issued for. [`purge`](Ctl::purge) deletes every token and scope lock
//! at startup: the containers they belong to are reaped then, and Docker
//! can give their addresses to new containers.
//!
//! # Requests
//!
//! Every request is refused unless, in this order:
//!
//! 1. its bearer token is stored,
//! 2. the connection comes from the token's container address,
//! 3. a turn is running on the token, and
//! 4. the command is `attach`, or the turn is not a private task.
//!
//! The handlers then apply the target rules in [`target`](self) (from the
//! turn's [`Side`](core_types::Side)) and write to the turn's [`Outbox`],
//! which [`end_turn`](Ctl::end_turn) hands to the turn pipeline.
//! `agentctl lock` takes leases in `scope_locks`, one volume at a time. A
//! lease lasts no longer than the turn that took it: beginning or ending a
//! turn, and revoking or replacing the token, delete the session's leases,
//! so the lock is free at once rather than when the lease runs out, and an
//! acquire or renewal takes effect only while the turn it was authorized
//! under is still the token's turn.

mod api;
mod outbox;
mod target;
mod token;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use core_types::{AgentId, ConvRef, CtlErrorCode, OutFile, SessionId, Surface, TurnId, VolumeKey};
use store::{CtlPurged, CtlTurn, NewCtlToken, Store, StoreError, TokenHash};

pub use api::{DEFAULT_HISTORY_LIMIT, JSON_BODY_LIMIT, MAX_HISTORY_LIMIT};
pub use outbox::{MAX_ATTACHMENTS, MAX_POSTS, MAX_REACTIONS, Outbox, QueuedPost, QueuedReaction};
pub use store::CtlTurn as Turn;
pub use token::ProcessToken;

use api::{ApiError, Authorized, no_turn};

/// The longest message `agentctl post` accepts, in bytes.
pub const MAX_POST_BYTES: usize = 40_000;
/// How long a `shared/` lease lasts unless renewed.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);
/// The directory under the data directory where attachments are staged.
pub const STAGING_DIR: &str = "ctl-outbox";

/// Finds the surface an agent's bot uses in a conversation, for
/// `agentctl history`.
///
/// agentd implements it once surfaces are wired in (T23). Until then
/// [`NoSurfaces`] answers `None`, and `history` is not available.
pub trait SurfaceLookup: Send + Sync {
    /// The surface that acts as `agent`'s bot in `conv`, if there is one.
    fn surface(&self, agent: AgentId, conv: &ConvRef) -> Option<Arc<dyn Surface>>;
}

/// A [`SurfaceLookup`] with no surfaces.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSurfaces;

impl SurfaceLookup for NoSurfaces {
    fn surface(&self, _agent: AgentId, _conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        None
    }
}

/// The ctl server's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlSettings {
    /// Where attachments are staged: `ctl-outbox/` under the data
    /// directory. Each turn gets a directory of its own in it.
    pub staging_dir: PathBuf,
    /// The largest file `agentctl attach` may stage.
    pub attach_max_bytes: u64,
    /// How long a `shared/` lease lasts unless renewed.
    pub lease_ttl: Duration,
}

impl CtlSettings {
    /// The settings for `config`.
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            staging_dir: config.store.data_dir.join(STAGING_DIR),
            attach_max_bytes: config.limits.attach_max_bytes,
            lease_ttl: DEFAULT_LEASE_TTL,
        }
    }
}

/// What a `claude` process's token is issued for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process's session.
    pub session: SessionId,
    /// The agent the session belongs to.
    pub agent: AgentId,
    /// The volume the session mounts, which names its `shared/` lock.
    pub volume: VolumeKey,
    /// The container's address on the sandbox network, from
    /// `docker inspect`. Requests from any other address are refused.
    pub container_ip: IpAddr,
}

/// Why a turn hook failed.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// The token isn't stored: it was revoked, replaced, or purged.
    #[error("the agentctl token is unknown or revoked")]
    UnknownToken,
    /// The operating system's random number generator failed.
    #[error("the system random number generator failed")]
    Random,
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The staging directory couldn't be created or emptied.
    #[error("the agentctl staging directory: {0}")]
    Io(#[from] std::io::Error),
}

/// The ctl server's state and the functions the turn hooks call.
///
/// Cloning is cheap: every clone shares the same state.
#[derive(Clone)]
pub struct Ctl {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    settings: CtlSettings,
    surfaces: Arc<dyn SurfaceLookup>,
    outboxes: Mutex<HashMap<TokenHash, Entry>>,
}

/// A running turn's outbox, and how many attachments are being uploaded
/// into it.
struct Entry {
    outbox: Outbox,
    uploading: usize,
}

impl std::fmt::Debug for Ctl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctl")
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

impl Ctl {
    /// A ctl server over `store`.
    pub fn new(store: Store, settings: CtlSettings, surfaces: Arc<dyn SurfaceLookup>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                settings,
                surfaces,
                outboxes: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The API's routes, for the ctl listener. Requests must carry the
    /// peer's `ConnectInfo<SocketAddr>`, as agentd's listeners add it.
    pub fn router(&self) -> Router {
        api::router(self.clone())
    }

    /// The settings.
    pub fn settings(&self) -> &CtlSettings {
        &self.inner.settings
    }

    fn store(&self) -> &Store {
        &self.inner.store
    }

    fn surfaces(&self) -> &dyn SurfaceLookup {
        self.inner.surfaces.as_ref()
    }

    fn outboxes(&self) -> MutexGuard<'_, HashMap<TokenHash, Entry>> {
        self.inner
            .outboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Deletes every token and scope lock, every outbox, and the staging
    /// directory, as agentd does at startup.
    ///
    /// # Errors
    ///
    /// If the store fails, or the staging directory can't be removed.
    pub async fn purge(&self) -> Result<CtlPurged, HookError> {
        let purged = self.store().purge_ctl().await?;
        drop(std::mem::take(&mut *self.outboxes()));
        match tokio::fs::remove_dir_all(&self.settings().staging_dir).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(purged)
    }

    /// Mints the token of a new `claude` process, for its `AGENTCTL_TOKEN`.
    ///
    /// No turn is running on it yet. A token already issued for the same
    /// session is revoked, with the session's leases: a session runs one
    /// process at a time.
    ///
    /// # Errors
    ///
    /// If the random number generator or the store fails.
    pub async fn issue_process_token(
        &self,
        process: ProcessInfo,
    ) -> Result<ProcessToken, HookError> {
        let token = ProcessToken::generate().ok_or(HookError::Random)?;
        let replaced = self
            .store()
            .put_ctl_token(&NewCtlToken {
                hash: token.hash(),
                session: process.session,
                agent: process.agent,
                volume: process.volume,
                container_ip: process.container_ip,
            })
            .await?;
        let mut outboxes = self.outboxes();
        for hash in replaced {
            outboxes.remove(&hash);
        }
        drop(outboxes);
        tracing::debug!(session = %process.session, "issued an agentctl token");
        Ok(token)
    }

    /// Records `turn` on the token, so its requests are authorized for that
    /// turn, and gives the turn an empty outbox. A turn still recorded on the
    /// token is replaced, and its outbox and leases dropped.
    ///
    /// # Errors
    ///
    /// [`HookError::UnknownToken`] if the token was revoked, or if the store
    /// or the staging directory fails.
    pub async fn begin_turn(&self, token: &ProcessToken, turn: CtlTurn) -> Result<(), HookError> {
        let hash = token.hash();
        let staging = self
            .settings()
            .staging_dir
            .join(uuid::Uuid::new_v4().to_string());
        outbox::create_private_dir(&staging)?;
        let entry = Entry {
            outbox: Outbox::new(turn.id, staging),
            uploading: 0,
        };
        self.outboxes().insert(hash, entry);
        match self.store().set_ctl_turn(&hash, Some(&turn)).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.remove_outbox(&hash, turn.id);
                Err(HookError::UnknownToken)
            }
            Err(err) => {
                self.remove_outbox(&hash, turn.id);
                Err(err.into())
            }
        }
    }

    /// Clears the token's turn, so it authorizes nothing until the next
    /// [`begin_turn`](Self::begin_turn), and returns what the turn queued.
    /// `None` if no turn was running, or the token was revoked.
    ///
    /// Requests still in flight when it returns are refused, and what they
    /// staged is deleted. The session's `shared/` leases are deleted with
    /// the turn, so the lock is free at once.
    ///
    /// It ends whichever turn the token holds now, not a particular one, so
    /// the caller must not call it after another `begin_turn` on the same
    /// token replaced its turn: that would end the newer turn and take its
    /// outbox. The runner runs one turn per session at a time, which keeps
    /// each `begin_turn` and its `end_turn` together.
    ///
    /// # Errors
    ///
    /// If the store fails. The turn is still recorded then.
    pub async fn end_turn(&self, token: &ProcessToken) -> Result<Option<Outbox>, HookError> {
        let hash = token.hash();
        self.store().set_ctl_turn(&hash, None).await?;
        Ok(self.outboxes().remove(&hash).map(|entry| entry.outbox))
    }

    /// Deletes the token and the session's `shared/` leases, and drops its
    /// turn's outbox. Revoking a token twice is harmless.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn revoke_process_token(&self, token: &ProcessToken) -> Result<(), HookError> {
        let hash = token.hash();
        self.store().delete_ctl_token(&hash).await?;
        self.outboxes().remove(&hash);
        Ok(())
    }

    fn remove_outbox(&self, hash: &TokenHash, turn: TurnId) {
        let mut outboxes = self.outboxes();
        if outboxes.get(hash).is_some_and(|e| e.outbox.turn() == turn) {
            outboxes.remove(hash);
        }
    }

    /// Runs `add` on the caller's turn's outbox, or refuses with
    /// [`CtlErrorCode::NoTurn`] if that turn has ended.
    fn queue<F>(&self, caller: &Authorized, add: F) -> Result<(), ApiError>
    where
        F: FnOnce(&mut Outbox) -> Result<(), ApiError>,
    {
        let mut outboxes = self.outboxes();
        match outboxes.get_mut(&caller.hash) {
            Some(entry) if entry.outbox.turn() == caller.turn.id => add(&mut entry.outbox),
            _ => Err(no_turn()),
        }
    }

    /// Reserves one of the turn's attachment slots for an upload.
    fn reserve_attachment(&self, caller: &Authorized) -> Result<Reservation, ApiError> {
        let mut outboxes = self.outboxes();
        let entry = match outboxes.get_mut(&caller.hash) {
            Some(entry) if entry.outbox.turn() == caller.turn.id => entry,
            _ => return Err(no_turn()),
        };
        if entry.outbox.attachments_len() + entry.uploading >= MAX_ATTACHMENTS {
            return Err(ApiError(core_types::CtlError::new(
                CtlErrorCode::Refused,
                format!("this turn has already attached {MAX_ATTACHMENTS} files"),
            )));
        }
        entry.uploading += 1;
        Ok(Reservation {
            ctl: self.clone(),
            hash: caller.hash,
            turn: caller.turn.id,
            dir: entry.outbox.staging().to_owned(),
            committed: false,
        })
    }

    /// Adds an uploaded file to the outbox its reservation was made in,
    /// freeing the reserved slot in the same step, or refuses with
    /// [`CtlErrorCode::NoTurn`] if that turn has ended.
    fn commit_attachment(
        &self,
        mut reservation: Reservation,
        file: OutFile,
    ) -> Result<(), ApiError> {
        let mut outboxes = self.outboxes();
        let result = match outboxes.get_mut(&reservation.hash) {
            Some(entry) if entry.outbox.turn() == reservation.turn => {
                entry.uploading = entry.uploading.saturating_sub(1);
                reservation.committed = true;
                entry.outbox.push_attachment(file);
                Ok(())
            }
            _ => Err(no_turn()),
        };
        drop(outboxes);
        result
    }
}

/// A reserved attachment slot. Dropping it frees the slot.
struct Reservation {
    ctl: Ctl,
    hash: TokenHash,
    turn: TurnId,
    dir: PathBuf,
    committed: bool,
}

impl Reservation {
    fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut outboxes = self.ctl.outboxes();
        if let Some(entry) = outboxes.get_mut(&self.hash)
            && entry.outbox.turn() == self.turn
        {
            entry.uploading = entry.uploading.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests;
