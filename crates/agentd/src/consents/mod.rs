//! Private tasks: consents asked for with `agentctl private`, decided by
//! the agent's owner, and the work each decision owes.
//!
//! [`Consents::request`] records a consent for a channel turn's task and
//! copies the files the turn named into `consents/<id>/` under the data
//! directory. A task the owner asked for is approved at once; any other
//! waits for the owner, who gets a consent card from the manager bot
//! ([`Consents::send_cards`]) and answers it with its buttons on Slack or
//! `approve <id>` and `decline <id>` anywhere ([`Consents::decide`]). Only
//! the owner, by any identity of theirs, can decide. A card nobody answers
//! within `[limits] consent_ttl_secs` expires ([`Consents::expire`]), and
//! a decided card is updated with the outcome ([`Consents::close_cards`]).
//!
//! Everything a consent owes lives in the `consents` table, so it survives
//! restarts and each part is done by one instance at a time: the card is
//! sent at least once with bounded retries, and the work (running the
//! approved task, or posting the declined or expired outcome in the
//! thread) is leased, so a task whose instance died runs again. The
//! [`Pipeline`] does the work ([`Pipeline::settle_consents`]).
//! [`Consents::run`] does all of it: at startup, whenever a consent is
//! asked for or decided, and every [`CONSENT_SWEEP_INTERVAL`].

pub mod card;
mod staging;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_types::{ConsentId, MemberKey, PrivateRequest};
use store::{Consent, ConsentState, CtlToken, CtlTurn, NewConsent, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};
use tokio::time::MissedTickBehavior;

pub use card::Card;
pub use staging::StageError;

use crate::commands::Replies;
use crate::ctl::{create_private_dir, remove_dir};
use crate::pipeline::Pipeline;

/// The directory under the data directory where consents' files wait.
pub const CONSENTS_DIR: &str = "consents";
/// The longest task text, in characters: what a Slack plain-text section,
/// which shows it on the card, holds.
pub const MAX_TASK_CHARS: usize = 3000;
/// The most files one private task may be handed.
pub const MAX_FILES: usize = 10;
/// How often consents are looked at without being woken.
pub const CONSENT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// How long a claim on a card keeps other instances from sending it.
pub const CARD_LEASE: Duration = Duration::from_secs(10 * 60);
/// How long after a failed card another is tried.
pub const CARD_RETRY: Duration = Duration::from_secs(60);
/// How many times a card is tried before it is left to expire.
pub const CARD_MAX_ATTEMPTS: u32 = 10;
/// How long a staging directory no consent owns is kept, for a request
/// still being recorded.
const ORPHAN_AGE: Duration = Duration::from_secs(10 * 60);

/// How [`Consents`] work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentSettings {
    /// agentd's data directory, where the calling sessions' directories
    /// are.
    pub data_dir: PathBuf,
    /// Where consents' files wait: [`CONSENTS_DIR`] under the data
    /// directory.
    pub dir: PathBuf,
    /// How long a card waits for the owner.
    pub ttl: Duration,
    /// The largest file a task may be handed, as for `agentctl attach`.
    pub attach_max_bytes: u64,
}

impl ConsentSettings {
    /// The settings for `config`.
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            ttl: config.limits.consent_ttl(),
            attach_max_bytes: config.limits.attach_max_bytes,
            ..Self::in_data_dir(&config.store.data_dir)
        }
    }

    /// The default settings for the data directory `data_dir`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            dir: data_dir.join(CONSENTS_DIR),
            ttl: Duration::from_secs(crate::config::DEFAULT_CONSENT_TTL_SECS),
            attach_max_bytes: crate::config::DEFAULT_ATTACH_MAX_BYTES,
        }
    }
}

/// Why [`Consents::request`] refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// The task text is empty or too long, or there are too many files.
    #[error("{0}")]
    BadRequest(String),
    /// A file couldn't be handed to the task.
    #[error(transparent)]
    Stage(#[from] StageError),
    /// The agent is gone.
    #[error("the agent was deleted")]
    NoAgent,
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The consent's directory couldn't be made, or a copy's task failed.
    #[error("the consent's directory: {0}")]
    Io(#[from] std::io::Error),
}

/// What became of a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    /// It was recorded.
    Recorded,
    /// There is no such consent, or its agent isn't the decider's.
    NotYours,
    /// It was decided or expired already.
    Settled(ConsentState),
}

/// Consents for private tasks. See the [module docs](self).
///
/// Cloning is cheap: every clone shares the same state.
#[derive(Debug, Clone)]
pub struct Consents {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    settings: ConsentSettings,
    wake: Notify,
}

impl Consents {
    /// Consents in `store`.
    pub fn new(store: Store, settings: ConsentSettings) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                settings,
                wake: Notify::new(),
            }),
        }
    }

    /// The settings.
    pub fn settings(&self) -> &ConsentSettings {
        &self.inner.settings
    }

    /// Has [`run`](Self::run) look at the consents now.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// The clock consents run on. Their times are compared with each
    /// other and with the store's, never with the pipeline's.
    pub(crate) fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    /// Where consent `id`'s files wait.
    fn dir_of(&self, id: ConsentId) -> PathBuf {
        self.inner.settings.dir.join(id.to_string())
    }

    /// Records a consent for `request`, asked for in `turn` by the process
    /// `token` was issued to, copying the files it names from that
    /// process's session directory. The consent is approved at once when
    /// the turn's requester is the agent's owner, and waits for the owner
    /// otherwise.
    ///
    /// # Errors
    ///
    /// [`RequestError::BadRequest`] for an empty or long task or too many
    /// files, [`RequestError::Stage`] for a file that can't be handed over,
    /// and the others if agentd failed. Nothing is recorded then.
    pub async fn request(
        &self,
        token: &CtlToken,
        turn: &CtlTurn,
        request: PrivateRequest,
    ) -> Result<ConsentId, RequestError> {
        if request.task.trim().is_empty() {
            return Err(RequestError::BadRequest("the task is empty".to_owned()));
        }
        if request.task.chars().count() > MAX_TASK_CHARS {
            return Err(RequestError::BadRequest(format!(
                "the task is over {MAX_TASK_CHARS} characters"
            )));
        }
        if request.files.len() > MAX_FILES {
            return Err(RequestError::BadRequest(format!(
                "a private task can be handed at most {MAX_FILES} files"
            )));
        }
        let store = &self.inner.store;
        let agent = store
            .agent(token.agent)
            .await?
            .ok_or(RequestError::NoAgent)?;
        let id = ConsentId::new_v4();
        let dir = self.dir_of(id);
        let names = match self.stage(token, &request.files, &dir).await {
            Ok(names) => names,
            Err(err) => {
                discard(dir).await;
                return Err(err);
            }
        };
        let attachments_json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_owned());
        let now = Self::now();
        let owners = turn.requester.member == Some(agent.owner);
        let created = store
            .create_consent(
                &NewConsent {
                    id,
                    agent: agent.id,
                    requester: &turn.requester,
                    hop: turn.hop,
                    task: &request.task,
                    attachments_json: &attachments_json,
                    thread: &turn.thread,
                    origin_session: token.session,
                    expires_at: now + self.inner.settings.ttl,
                    approved_by_owner: owners.then_some(&turn.requester.key),
                },
                now,
            )
            .await;
        if let Err(err) = created {
            discard(dir).await;
            return Err(err.into());
        }
        tracing::info!(
            consent = %id,
            agent = %agent.id,
            session = %token.session,
            files = names.len(),
            approved = owners,
            "a private task was asked for"
        );
        self.wake();
        Ok(id)
    }

    /// Copies `files` from `token`'s session directory into `dir`.
    async fn stage(
        &self,
        token: &CtlToken,
        files: &[String],
        dir: &Path,
    ) -> Result<Vec<String>, RequestError> {
        let session_dir = self
            .inner
            .settings
            .data_dir
            .join(sandbox::volume_rel_path(&token.volume))
            .join("sessions")
            .join(token.session.to_string());
        let files = files.to_vec();
        let dir = dir.to_owned();
        let cap = self.inner.settings.attach_max_bytes;
        tokio::task::spawn_blocking(move || {
            create_private_dir(&dir)?;
            Ok(staging::stage(&session_dir, &files, &dir, cap)?)
        })
        .await
        .map_err(|err| RequestError::Io(std::io::Error::other(err)))?
    }

    /// Records `by`'s decision on consent `id`: approved, or declined.
    /// Only the owner of the consent's agent can decide, by any identity
    /// linked to them; for anyone else the consent is as good as unknown.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn decide(
        &self,
        by: &MemberKey,
        id: ConsentId,
        approve: bool,
    ) -> Result<Decided, StoreError> {
        let store = &self.inner.store;
        let Some(consent) = store.consent(id).await? else {
            return Ok(Decided::NotYours);
        };
        let owner = store.agent(consent.agent).await?.map(|agent| agent.owner);
        let member = store.member_for_identity(by).await?;
        if owner.is_none() || member != owner {
            tracing::info!(consent = %id, member = %by, "refused a decision on a consent by someone other than its owner");
            return Ok(Decided::NotYours);
        }
        if store
            .decide_consent(id, approve, by, Self::now())
            .await?
            .is_some()
        {
            tracing::info!(consent = %id, approve, "the owner decided a private task");
            self.wake();
            return Ok(Decided::Recorded);
        }
        let state = store
            .consent(id)
            .await?
            .map_or(ConsentState::Expired, |consent| consent.state);
        Ok(Decided::Settled(match state {
            ConsentState::Pending => ConsentState::Expired,
            settled => settled,
        }))
    }

    /// Marks every card that waited past its expiry expired, and returns
    /// how many.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn expire(&self) -> Result<usize, StoreError> {
        let expired = self.inner.store.expire_consents(Self::now()).await?;
        for consent in &expired {
            tracing::info!(consent = %consent.id, "a private task's consent card expired");
        }
        Ok(expired.len())
    }

    /// Sends the cards owed, through `replies`, and returns how many were
    /// sent. A card goes to the owner's identity in the task's workspace,
    /// or else any a manager bot reaches; with none, it waits unclaimed. A
    /// card that fails is tried again after [`CARD_RETRY`], up to
    /// [`CARD_MAX_ATTEMPTS`] times, and then left to expire.
    ///
    /// # Errors
    ///
    /// If the store fails. Cards sent stay sent.
    pub async fn send_cards(&self, replies: &Replies) -> Result<usize, StoreError> {
        let store = &self.inner.store;
        let mut sent = 0;
        for consent in store
            .consent_cards_owed(Self::now(), CARD_MAX_ATTEMPTS)
            .await?
        {
            let Some(agent) = store.agent(consent.agent).await? else {
                continue;
            };
            let identities = store.member_identities(agent.owner).await?;
            let reachable = |key: &&MemberKey| replies.can_dm(key);
            let Some(owner) = identities
                .iter()
                .filter(reachable)
                .find(|key| {
                    key.surface == consent.thread.conv.surface
                        && key.team == consent.thread.conv.team
                })
                .or_else(|| identities.iter().find(reachable))
            else {
                tracing::debug!(consent = %consent.id, "no manager bot reaches the owner; the card waits");
                continue;
            };
            let now = Self::now();
            let Some(attempt) = store
                .claim_consent_card(consent.id, now, now + CARD_LEASE, CARD_MAX_ATTEMPTS)
                .await?
            else {
                continue;
            };
            let files = attachments(&consent);
            let card = Card {
                consent: &consent,
                agent: &agent.name,
                files: &files,
            }
            .open();
            match replies.dm_rich(owner, &card).await {
                Ok(posted) => {
                    store.record_consent_card(consent.id, &posted).await?;
                    tracing::info!(consent = %consent.id, "sent a consent card to the owner");
                    sent += 1;
                }
                Err(err) => {
                    tracing::warn!(consent = %consent.id, attempt, error = %err, "couldn't send a consent card");
                    store
                        .defer_consent_card(consent.id, Self::now() + CARD_RETRY)
                        .await?;
                }
            }
        }
        Ok(sent)
    }

    /// Updates each decided or expired consent's card with its outcome,
    /// through `replies`, once: a card that can't be updated stays as it
    /// is, and its buttons answer that the task was settled.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn close_cards(&self, replies: &Replies) -> Result<usize, StoreError> {
        let store = &self.inner.store;
        let mut closed = 0;
        for consent in store.consent_cards_to_close().await? {
            let Some(posted) = consent.card.clone() else {
                continue;
            };
            if !store
                .claim_consent_card_close(consent.id, Self::now())
                .await?
            {
                continue;
            }
            let name = store
                .agent(consent.agent)
                .await?
                .map_or_else(|| "this agent".to_owned(), |agent| agent.name);
            let files = attachments(&consent);
            let card = Card {
                consent: &consent,
                agent: &name,
                files: &files,
            }
            .closed();
            match replies.update_rich(&posted, &card).await {
                Ok(()) => closed += 1,
                Err(err) => {
                    tracing::warn!(consent = %consent.id, error = %err, "couldn't update a consent card with its outcome");
                }
            }
        }
        Ok(closed)
    }

    /// Copies the files consent `consent` was handed into `work`, a new
    /// session's working directory.
    ///
    /// # Errors
    ///
    /// If a file can't be copied.
    pub(crate) async fn hand_over(&self, consent: &Consent, work: PathBuf) -> std::io::Result<()> {
        let dir = self.dir_of(consent.id);
        let names = attachments(consent);
        tokio::task::spawn_blocking(move || staging::hand_over(&dir, &names, &work))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Deletes consent `id`'s files, once its work is done.
    pub(crate) async fn forget_files(&self, id: ConsentId) {
        discard(self.dir_of(id)).await;
    }

    /// Deletes the staging directories no unfinished consent owns that are
    /// older than [`ORPHAN_AGE`]: those of requests that failed, or of
    /// consents whose work finished while their files couldn't be deleted.
    async fn sweep_files(&self) -> Result<(), StoreError> {
        let dir = self.inner.settings.dir.clone();
        let found = tokio::task::spawn_blocking(move || -> Vec<(ConsentId, PathBuf)> {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return Vec::new();
            };
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .is_ok_and(|at| at.elapsed().is_ok_and(|age| age > ORPHAN_AGE))
                })
                .filter_map(|entry| {
                    let id = entry.file_name().to_str()?.parse().ok()?;
                    Some((id, entry.path()))
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        for (id, path) in found {
            let owned = self
                .inner
                .store
                .consent(id)
                .await?
                .is_some_and(|consent| consent.finished_at.is_none());
            if !owned {
                discard(path).await;
            }
        }
        Ok(())
    }

    /// Looks at the consents now, then whenever woken and every `every`,
    /// until `stopping` becomes true or its sender is dropped: expires
    /// cards, sends and closes cards through `pipeline`'s manager bots,
    /// deletes orphaned files, and has `pipeline` do the work decided
    /// consents owe. A pass in progress finishes first.
    pub async fn run(
        self,
        pipeline: Pipeline,
        every: Duration,
        mut stopping: watch::Receiver<bool>,
    ) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                () = self.inner.wake.notified() => {}
                _ = ticks.tick() => {}
            }
            if let Err(err) = self.pass(pipeline.replies(), &pipeline).await {
                tracing::warn!(error = %err, "looking at private tasks' consents failed");
            }
        }
    }

    /// One pass of [`run`](Self::run).
    async fn pass(&self, replies: &Replies, pipeline: &Pipeline) -> Result<(), StoreError> {
        self.expire().await?;
        self.send_cards(replies).await?;
        self.close_cards(replies).await?;
        pipeline.settle_consents(self).await?;
        self.sweep_files().await
    }
}

/// The names of the files `consent` was handed, in their staged order.
pub(crate) fn attachments(consent: &Consent) -> Vec<String> {
    serde_json::from_str(&consent.attachments_json).unwrap_or_else(|err| {
        tracing::warn!(consent = %consent.id, error = %err, "a consent's file list doesn't parse");
        Vec::new()
    })
}

/// Removes `dir`, in a blocking task.
async fn discard(dir: PathBuf) {
    let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
}
