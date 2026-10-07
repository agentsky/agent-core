//! Private tasks: consents asked for with `agentctl private`, decided by
//! the agent's owner, and the work each decision owes.
//!
//! [`Consents::request`] records a consent for a channel turn's task and
//! copies the files the turn named into `consents/<id>/` under the data
//! directory. A task the owner asked for in their own DM with the agent,
//! on the owner's side, is approved at once. Any other waits for the owner,
//! a task the owner asked for in a channel or group DM included, since
//! others' messages there reach the turn that asked: the owner gets a
//! consent card from the manager bot ([`Consents::send_cards`]) and answers
//! it with its buttons on Slack or `approve <id>` and `decline <id>`
//! anywhere ([`Consents::decide`]). Only the owner, by any identity of
//! theirs, can decide. A card nobody answers within `[limits]
//! consent_ttl_secs` expires ([`Consents::expire`]), as does one too long
//! for one message or whose agent was deleted, and a decided card is
//! updated with the outcome ([`Consents::close_cards`]).
//!
//! Each requester may have [`MAX_OPEN_PER_REQUESTER`] consents of an agent
//! unfinished, and everyone but the owner [`MAX_OPEN_PER_AGENT`] together,
//! so the files they hold and the cards the owner gets stay bounded and
//! others can't use up the owner's own.
//!
//! Everything a consent owes lives in the `consents` table, so it survives
//! restarts and each part is done by one instance at a time: the card is
//! sent at least once, retried until the consent expires, and the work
//! (running the approved task, or posting the outcome in the thread) is
//! leased, so a task whose instance died is taken up again. The
//! [`Pipeline`] does the work ([`Pipeline::settle_consents`]).
//! [`Consents::run`] does all of it: at startup, whenever a consent is
//! asked for or decided or something falls due, and at least every
//! [`CONSENT_SWEEP_INTERVAL`].

pub mod card;
mod staging;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_types::{ConsentId, MemberKey, PrivateRequest, SessionId, Side, SurfaceError, VolumeKey};
use store::{
    AgentState, Consent, ConsentState, CtlToken, CtlTurn, NewConsent, OpenLimits, Store, StoreError,
};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};

pub use card::Card;
pub use staging::StageError;

use crate::commands::{Replies, ReplyError};
use crate::ctl::{create_private_dir, is_invisible, remove_dir, without_joiners};
use crate::pipeline::Pipeline;

/// The directory under the data directory where consents' files wait.
pub const CONSENTS_DIR: &str = "consents";
/// The longest task text, in UTF-16 code units, as Slack counts it: what
/// a Slack plain-text section, which shows it on the card, holds.
pub const MAX_TASK_LEN: usize = card::SLACK_TEXT_MAX;
/// The most files one private task may be handed: as many as a Slack
/// `context` block holds elements, since a Slack card gives each file's
/// name its own.
pub const MAX_FILES: usize = 10;
/// The most consents one agent may have unfinished at once that anyone but
/// its owner asked for.
pub const MAX_OPEN_PER_AGENT: u32 = 10;
/// The most consents one requester's identity may have unfinished at once
/// with one agent.
pub const MAX_OPEN_PER_REQUESTER: u32 = 3;
/// How often consents are looked at at least, besides when woken or when
/// something falls due.
pub const CONSENT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// How long a claim on a card keeps other instances from sending it.
pub const CARD_LEASE: Duration = Duration::from_secs(10 * 60);
/// How long after a card's first failed send it is tried again. Each
/// failure after that doubles the wait, up to [`CARD_RETRY_MAX`], until
/// the consent expires.
pub const CARD_RETRY: Duration = Duration::from_secs(60);
/// The longest wait between two tries of a card.
pub const CARD_RETRY_MAX: Duration = Duration::from_secs(15 * 60);
/// The limits on unfinished consents.
const OPEN_LIMITS: OpenLimits = OpenLimits {
    per_requester: MAX_OPEN_PER_REQUESTER,
    per_agent: MAX_OPEN_PER_AGENT,
};
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
    /// The most bytes a task may be handed, all its files together: one
    /// attachment's cap, as for `agentctl attach`.
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
    /// The task text is empty or too long, there are too many files, or
    /// the card wouldn't fit one message.
    #[error("{0}")]
    BadRequest(String),
    /// A file couldn't be handed to the task.
    #[error(transparent)]
    Stage(#[from] StageError),
    /// The agent is paused or deleted.
    #[error("the agent is paused or was deleted")]
    Inactive,
    /// The agent, or the requester with it, has as many consents
    /// unfinished as they may.
    #[error(
        "too many private tasks of this agent's are waiting for their owner or running; ask \
         again once some are done"
    )]
    TooMany,
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
    /// the agent's owner asked for it in their own DM with the agent, the
    /// one turn on the owner's side, whose context is the owner's own
    /// words. It waits for the owner otherwise: in a channel or group DM
    /// the turn reads others' messages, and a hop's requester is inherited
    /// from another agent's post.
    ///
    /// # Errors
    ///
    /// [`RequestError::BadRequest`] for an empty or long task, one with
    /// control or invisible characters the card wouldn't show, too many
    /// files or a card too long for one message, [`RequestError::Stage`]
    /// for a file that can't be handed over, [`RequestError::Inactive`]
    /// for an agent that isn't active, [`RequestError::TooMany`] past the
    /// limits on unfinished consents, and the others if agentd failed.
    /// Nothing is recorded then.
    pub async fn request(
        &self,
        token: &CtlToken,
        turn: &CtlTurn,
        mut request: PrivateRequest,
    ) -> Result<ConsentId, RequestError> {
        request.task = without_joiners(&request.task);
        if request.task.trim().is_empty() {
            return Err(RequestError::BadRequest("the task is empty".to_owned()));
        }
        if request.task.encode_utf16().count() > MAX_TASK_LEN {
            return Err(RequestError::BadRequest(format!(
                "the task is over {MAX_TASK_LEN} UTF-16 code units, in which an emoji counts \
                 as two"
            )));
        }
        if let Some(why) = unshowable(&request.task) {
            return Err(RequestError::BadRequest(why.to_owned()));
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
            .filter(|agent| agent.state == AgentState::Active)
            .ok_or(RequestError::Inactive)?;
        if !store
            .consent_room(agent.id, &turn.requester, OPEN_LIMITS)
            .await?
        {
            return Err(RequestError::TooMany);
        }
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
        let asked = owners && turn.side == Side::Owner;
        let new = NewConsent {
            id,
            agent: agent.id,
            requester: &turn.requester,
            hop: turn.hop,
            task: &request.task,
            attachments_json: &attachments_json,
            thread: &turn.thread,
            origin_session: token.session,
            expires_at: now + self.inner.settings.ttl,
            approved_by_owner: asked.then_some(&turn.requester.key),
        };
        let draft = new.draft(now);
        let fits = Card {
            consent: &draft,
            agent: &agent.name,
            files: &names,
            owners,
            paused: false,
            surface: turn.thread.conv.surface,
            requester_name: None,
        }
        .check();
        let created = match fits {
            Err(why) if !asked => Err(RequestError::BadRequest(why)),
            _ => store
                .create_consent(&new, OPEN_LIMITS, now)
                .await
                .map_err(RequestError::from)
                .and_then(|created| created.ok_or(RequestError::TooMany)),
        };
        if let Err(err) = created {
            discard(dir).await;
            return Err(err);
        }
        tracing::info!(
            consent = %id,
            agent = %agent.id,
            session = %token.session,
            hop = turn.hop.0,
            files = names.len(),
            approved = asked,
            "a private task was asked for"
        );
        self.wake();
        Ok(id)
    }

    /// The directory of session `session` on volume `volume`.
    pub(crate) fn session_dir(&self, volume: &VolumeKey, session: SessionId) -> PathBuf {
        self.inner
            .settings
            .data_dir
            .join(sandbox::volume_rel_path(volume))
            .join("sessions")
            .join(session.to_string())
    }

    /// Copies `files` from `token`'s session directory into `dir`.
    async fn stage(
        &self,
        token: &CtlToken,
        files: &[String],
        dir: &Path,
    ) -> Result<Vec<String>, RequestError> {
        let session_dir = self.session_dir(&token.volume, token.session);
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
        let owner = store
            .agent(consent.agent)
            .await?
            .filter(|agent| agent.state != AgentState::Deleted)
            .map(|agent| agent.owner);
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

    /// Marks every card that waited past its expiry, or whose agent was
    /// deleted, expired, and returns how many.
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
    /// or else any a manager bot reaches, and says so when the agent is
    /// paused. A card that fails, or that no manager bot of this instance
    /// can send (another instance's may), is tried again after
    /// [`CARD_RETRY`], doubling up to [`CARD_RETRY_MAX`], until the consent
    /// expires; the thread is then told the card couldn't reach the owner.
    /// A consent whose card won't fit one message expires at once. A
    /// deleted agent's consents are left to [`expire`](Self::expire).
    ///
    /// # Errors
    ///
    /// If the store fails. Cards sent stay sent.
    pub async fn send_cards(&self, replies: &Replies) -> Result<usize, StoreError> {
        let store = &self.inner.store;
        let mut sent = 0;
        for consent in store.consent_cards_owed(Self::now()).await? {
            let Some(agent) = store
                .agent(consent.agent)
                .await?
                .filter(|agent| agent.state != AgentState::Deleted)
            else {
                continue;
            };
            let now = Self::now();
            let Some(attempt) = store
                .claim_consent_card(consent.id, now, now + CARD_LEASE)
                .await?
            else {
                continue;
            };
            let identities = store.member_identities(agent.owner).await?;
            let reachable = |key: &&MemberKey| replies.can_dm(key);
            let owner = identities
                .iter()
                .filter(reachable)
                .find(|key| {
                    key.surface == consent.thread.conv.surface
                        && key.team == consent.thread.conv.team
                })
                .or_else(|| identities.iter().find(reachable));
            let sent_to = match owner {
                Some(owner) => self.send_card(replies, &consent, &agent, owner).await,
                None => Err(ReplyError::NoManagerBot(consent.thread.conv.surface)),
            };
            match sent_to {
                Ok(posted) => {
                    record_card(store, consent.id, &posted).await?;
                    tracing::info!(consent = %consent.id, "sent a consent card to the owner");
                    sent += 1;
                }
                Err(ReplyError::TooLong) => {
                    self.unreachable(&consent, "the card doesn't fit one message")
                        .await?;
                }
                Err(err) => {
                    let retry = card_retry(attempt);
                    tracing::warn!(consent = %consent.id, attempt, error = %err, retry_secs = retry.as_secs(), "couldn't send a consent card");
                    store
                        .defer_consent_card(consent.id, attempt, Self::now() + retry)
                        .await?;
                }
            }
        }
        Ok(sent)
    }

    /// Sends `consent`'s card, for its agent `agent`, to `owner` through
    /// `replies`, naming the requester as they are known on their surface.
    async fn send_card(
        &self,
        replies: &Replies,
        consent: &Consent,
        agent: &store::Agent,
        owner: &MemberKey,
    ) -> Result<core_types::MsgRef, ReplyError> {
        let name = replies.name_of(&consent.requester.key).await;
        let files = attachments(consent);
        let card = Card {
            consent,
            agent: &agent.name,
            files: &files,
            owners: consent.requester.member == Some(agent.owner),
            paused: agent.state == AgentState::Paused,
            surface: owner.surface,
            requester_name: name.as_deref(),
        }
        .open();
        replies.dm_rich(owner, &card).await
    }

    /// Expires `consent` at once, before its card reached the owner, for
    /// the reason `why`.
    async fn unreachable(&self, consent: &Consent, why: &str) -> Result<(), StoreError> {
        if self
            .inner
            .store
            .expire_consent(consent.id, Self::now())
            .await?
        {
            tracing::warn!(consent = %consent.id, why, "a private task's consent card can't reach the owner; it expired");
            self.wake();
        }
        Ok(())
    }

    /// Updates each decided or expired consent's card with its outcome,
    /// through `replies`. An update that fails on the way (a rate limit or
    /// a transport error) releases its claim, so a later pass tries again;
    /// a card the platform refuses to update stays as it is, and its
    /// buttons answer that the task was settled.
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
            let claimed_at = Self::now();
            if !store
                .claim_consent_card_close(consent.id, claimed_at)
                .await?
            {
                continue;
            }
            let agent = store.agent(consent.agent).await?;
            let files = attachments(&consent);
            let name = replies.name_of(&consent.requester.key).await;
            let card = Card {
                consent: &consent,
                agent: agent.as_ref().map_or("this agent", |agent| &agent.name),
                files: &files,
                owners: agent
                    .as_ref()
                    .is_some_and(|agent| consent.requester.member == Some(agent.owner)),
                paused: false,
                surface: posted.conv.surface,
                requester_name: name.as_deref(),
            }
            .closed();
            match replies.update_rich(&posted, &card).await {
                Ok(()) => closed += 1,
                Err(
                    err @ ReplyError::Surface(
                        SurfaceError::RateLimited { .. } | SurfaceError::Transport(_),
                    ),
                ) => {
                    tracing::warn!(consent = %consent.id, error = %err, "couldn't update a consent card with its outcome; it will be tried again");
                    store
                        .release_consent_card_close(consent.id, claimed_at)
                        .await?;
                }
                Err(err) => {
                    tracing::warn!(consent = %consent.id, error = %err, "couldn't update a consent card with its outcome");
                }
            }
        }
        Ok(closed)
    }

    /// Copies the files consent `consent` was handed into `work`, a new
    /// session's working directory, giving them to `owner`, the uid and gid
    /// agents run as, when the sandbox runs them as one.
    ///
    /// # Errors
    ///
    /// If a file can't be copied.
    pub(crate) async fn hand_over(
        &self,
        consent: &Consent,
        work: PathBuf,
        owner: Option<(u32, u32)>,
    ) -> std::io::Result<()> {
        let dir = self.dir_of(consent.id);
        let names = attachments(consent);
        tokio::task::spawn_blocking(move || staging::hand_over(&dir, &names, &work, owner))
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

    /// Looks at the consents now, then whenever woken, when the next thing
    /// a consent owes falls due ([`Store::next_consent_deadline`]), and at
    /// least every `every`, until `stopping` becomes true or its sender is
    /// dropped: expires cards, sends and closes cards through `pipeline`'s
    /// manager bots, deletes orphaned files, and has `pipeline` do the work
    /// decided consents owe. A pass in progress finishes first.
    pub async fn run(
        self,
        pipeline: Pipeline,
        every: Duration,
        mut stopping: watch::Receiver<bool>,
    ) {
        while !*stopping.borrow() {
            if let Err(err) = self.pass(pipeline.replies(), &pipeline).await {
                tracing::warn!(error = %err, "looking at private tasks' consents failed");
            }
            let wait = self.next_wait(every).await;
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                () = self.inner.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// How long until the next pass: until the next deadline, or `every`
    /// if that is sooner or the store can't say.
    async fn next_wait(&self, every: Duration) -> Duration {
        let now = Self::now();
        match self.inner.store.next_consent_deadline(now).await {
            Ok(Some(due)) => Duration::try_from(due - now).map_or(every, |wait| wait.min(every)),
            Ok(None) => every,
            Err(err) => {
                tracing::warn!(error = %err, "couldn't read when consents next fall due");
                every
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

/// The deepest a line of a task may be indented, in columns, a tab
/// counting as [`TAB_COLUMNS`].
const MAX_INDENT: usize = 32;
/// The columns a tab counts as in a task's indentation.
const TAB_COLUMNS: usize = 8;
/// The longest run of blanks a task may have after a line's first visible
/// character, in columns, enough to align a table.
const MAX_BLANK_RUN: usize = 16;
/// The most blank lines a task may have in a row.
const MAX_BLANK_LINES: usize = 2;
/// The most combining diacritical marks a task may stack on a character.
const MAX_MARK_RUN: usize = 4;

/// Why the owner's card couldn't show `task` as the model reads it, if it
/// couldn't: a control character other than a newline or tab, or an
/// invisible one ([`is_invisible`]), which shows as nothing; a line
/// indented more than [`MAX_INDENT`] columns, or a run of blanks wider than
/// [`MAX_BLANK_RUN`] columns after a line's first visible character, which
/// can push the rest of the line out of a code block's view; more than
/// [`MAX_BLANK_LINES`] blank lines in a row, which can push the rest below
/// the fold, a blank line counting only as that however wide it is; or more than [`MAX_MARK_RUN`] combining diacritical marks in
/// a row, which can draw over the card's own text.
fn unshowable(task: &str) -> Option<&'static str> {
    if task
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_invisible(c))
    {
        return Some(
            "the task has control or invisible characters, which the owner's card wouldn't show",
        );
    }
    let mut marks = 0;
    for c in task.chars() {
        marks = if is_diacritical_mark(c) { marks + 1 } else { 0 };
        if marks > MAX_MARK_RUN {
            return Some(
                "the task stacks more than 4 combining marks on a character, which could draw \
                 over the owner's card",
            );
        }
    }
    let mut blank_lines = 0;
    for line in task.split('\n') {
        if line.chars().all(is_blank) {
            blank_lines += 1;
            if blank_lines > MAX_BLANK_LINES {
                return Some(
                    "the task has more than 2 blank lines in a row, which could hide what \
                     follows from the owner's card",
                );
            }
            continue;
        }
        blank_lines = 0;
        let indent: usize = line
            .chars()
            .take_while(|c| is_blank(*c))
            .map(blank_columns)
            .sum();
        if indent > MAX_INDENT {
            return Some(
                "the task has a line indented more than 32 columns, which could hide it from \
                 the owner's card",
            );
        }
        let mut blanks = 0;
        for c in line.chars().skip_while(|c| is_blank(*c)) {
            blanks = if is_blank(c) {
                blanks + blank_columns(c)
            } else {
                0
            };
            if blanks > MAX_BLANK_RUN {
                return Some(
                    "the task has a run of more than 16 columns of spaces or tabs, which could \
                     hide the rest of a line from the owner's card",
                );
            }
        }
    }
    None
}

/// Whether `c` shows as blank space: whitespace, and the characters that
/// aren't whitespace but draw as nothing, the Braille blank and the
/// musical null notehead.
fn is_blank(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{2800}' | '\u{1D159}')
}

/// The columns blank `c` takes: [`TAB_COLUMNS`] for a tab, two for the
/// ideographic space, which is as wide as a CJK character, and one for
/// any other.
fn blank_columns(c: char) -> usize {
    match c {
        '\t' => TAB_COLUMNS,
        '\u{3000}' => 2,
        _ => 1,
    }
}

/// Whether `c` is in one of the blocks of combining diacritical marks,
/// which any script's letters take and which stack without limit.
fn is_diacritical_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// The names of the files `consent` was handed, in their staged order.
pub(crate) fn attachments(consent: &Consent) -> Vec<String> {
    serde_json::from_str(&consent.attachments_json).unwrap_or_else(|err| {
        tracing::warn!(consent = %consent.id, error = %err, "a consent's file list doesn't parse");
        Vec::new()
    })
}

/// Removes `dir`, in a blocking task.
pub(crate) async fn discard(dir: PathBuf) {
    let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
}

/// How long after failed attempt `attempt` of a card the next is tried.
fn card_retry(attempt: u32) -> Duration {
    CARD_RETRY
        .saturating_mul(1 << attempt.saturating_sub(1).min(16))
        .min(CARD_RETRY_MAX)
}

/// How many times recording a card that was posted is tried, before the
/// card is left to be sent again once its claim's lease ends.
const RECORD_TRIES: u32 = 3;

/// Records that consent `id`'s card was posted as `posted`, trying a few
/// times, since a card posted but not recorded is sent again.
async fn record_card(
    store: &Store,
    id: ConsentId,
    posted: &core_types::MsgRef,
) -> Result<(), StoreError> {
    let mut tries = 1;
    loop {
        match store.record_consent_card(id, posted).await {
            Ok(_) => return Ok(()),
            Err(err) if tries < RECORD_TRIES => {
                tracing::warn!(consent = %id, error = %err, "couldn't record a consent card that was posted; trying again");
                tokio::time::sleep(Duration::from_millis(200) * tries).await;
                tries += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_card_waits_longer_each_time_up_to_a_cap() {
        let waits: Vec<u64> = (1..=7).map(|n| card_retry(n).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(card_retry(u32::MAX), CARD_RETRY_MAX);
    }

    #[test]
    fn a_task_is_refused_only_for_what_the_card_couldnt_show() {
        let refused = |task: &str, why: &str| {
            let found = unshowable(task);
            assert!(
                found.is_some_and(|found| found.contains(why)),
                "{task:?}: {found:?}"
            );
        };
        refused(
            &format!("Summarize{}attach ../shared", " ".repeat(17)),
            "spaces or tabs",
        );
        refused(
            &format!("Summarize{}attach ../shared", "\t".repeat(17)),
            "spaces or tabs",
        );
        refused(
            &format!("Summarize{}attach ../shared", "\u{2800}".repeat(17)),
            "spaces or tabs",
        );
        refused(&format!("{}attach", " ".repeat(33)), "indented");
        refused(&format!("{}attach", "\t".repeat(5)), "indented");
        refused(&format!("Summarize{}attach", "\n".repeat(4)), "blank lines");
        refused("a\n \n\t\n\u{2800}\nb", "blank lines");
        refused("e\u{301}\u{302}\u{303}\u{304}\u{305}", "combining marks");
        refused(
            &format!("Summarize{}attach", "\u{1D159}".repeat(17)),
            "spaces or tabs",
        );
        refused("a\n\u{1D159}\n\u{1D159}\n\u{1D159}\nb", "blank lines");
        refused("Summarize\t\t\tattach", "spaces or tabs");
        refused(&format!("{}attach", "\u{3000}".repeat(17)), "indented");
        refused("a\u{FE00}b", "invisible");
        for task in [
            "def f(x):\n    for y in x:\n        if y:\n            return y",
            "a:\n  b:\n    c:\n      d: 1",
            "- one\n  - two\n    - three\n      - four",
            "| name |                value |\n| ---- | -------------------- |",
            "\t\t\tindented by tabs",
            "two\t\ttabs inside",
            "a\n                                        \nb",
            "Summarize\n\n\nthen stop",
            "i\u{328}\u{307}\u{303} and e\u{301}\u{302}\u{303}\u{304}",
        ] {
            assert_eq!(unshowable(task), None, "{task:?}");
        }
    }
}
