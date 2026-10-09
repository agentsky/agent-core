//! Agents' skills: the bundled `agentctl` skill, the skills owners add with
//! `/agent skill add`, and the egress hosts those skills declare.
//!
//! # Where they live
//!
//! Each agent's skills are directories in `<data>/skills/<agent>/`
//! ([`runner::skills_dir`]), which every session of the agent mounts
//! read-only as its `$CLAUDE_CONFIG_DIR/skills`, where Claude Code finds
//! them. The bundled skill, [`BUNDLED_NAME`], is written there before
//! every turn ([`write_bundled`]), so every agent has it and an upgrade of
//! agentd updates it; no owner may add or remove a skill of that name.
//!
//! A skill that waits for its owner to confirm its hosts is kept outside
//! what sandboxes mount, in `<data>/skills-pending/<agent>/<name>/`, and
//! work in progress in `<data>/skills-work/`. Directories are moved into
//! place with a rename, so a session sees a skill whole or not at all;
//! replacing a skill moves the old one aside first, so a session starting
//! between the two renames sees neither. A skill added, replaced or
//! removed changes in every sandbox of the agent at once, running ones
//! included, since they mount the directory itself; a conversation already
//! running may keep what it loaded of the old files until its process next
//! starts. Its hosts change for new connections at once; a tunnel already
//! open ends within the egress proxy's idle and lifetime limits.
//!
//! Every add, confirmation, removal and expiry of a skill holds the
//! skill's lease ([`Store::acquire_skill_lease`]) for its moves and row
//! writes, so one runs at a time for each agent's skill name, on every
//! instance: a blue-green deploy runs two agentd processes over the same
//! directories and store. The row and the files still can't change
//! together, so for a failure or a crash between the two they change in an
//! order that keeps rows and files matching where it can: `add` records
//! the row first (a new active row carries no hosts,
//! and replaces any that did; a new pending row first drops the pending
//! row and files it replaces, so no older files wait under its hosts),
//! `confirm` moves the files before it makes active only the row it read,
//! and `remove` deletes the rows before the files. Whatever a failure
//! between the two steps leaves, startup removes ([`Skills::purge`]): work
//! directories, and skill directories whose name no row records, once
//! they are older than [`STALE_AFTER`], so a clone another instance is
//! still making is left alone.
//!
//! # Adding one
//!
//! [`Skills::add`] takes the files from a Git clone ([`git`]) or an upload
//! (a `SKILL.md`, or a `.zip`), and checks them ([`package`]): the size
//! and shape limits, and a `SKILL.md` whose front matter has a `name` and
//! a `description`. A skill whose front matter declares `allowed-hosts`
//! is held back until the owner confirms those hosts
//! ([`Skills::confirm`]), within [`PENDING_TTL`].
//!
//! # Hosts
//!
//! The `agent_skills` table records each skill with its hosts. Only an
//! active skill's hosts count: [`SkillHosts`] is the egress proxy's
//! [`EgressExtension`], mapping a session to its agent's confirmed hosts.
//! A skill's hosts are granted only while its live `SKILL.md` declares
//! exactly those hosts ([`Skills::granted_hosts`]), so hosts never cover
//! files that don't declare them, whatever state the disk was left in;
//! the lease, the order of the steps and the undo only keep rows and files
//! matching. They pass the same checks as configured rules, so
//! `api.anthropic.com` and private, loopback and metadata addresses stay
//! out of reach.

pub mod git;
pub mod package;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_types::{AgentId, LeaseId, MemberId, SessionId};
use cred_proxy::{EgressExtension, HostRule};
use store::{AgentSkill, NewSkill, SkillState, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::RwLock;

pub use git::{CloneError, Git};
pub use package::{BUNDLED_NAME, Manifest, Problem};

use package::CheckError;

use crate::sweeper::SWEEP_INTERVAL;
use crate::throttle::Throttle;

/// The bundled skill's `SKILL.md`, documenting `agentctl`.
pub const BUNDLED_SKILL: &str = include_str!("../../assets/skills/agentctl/SKILL.md");
/// How long a skill waits for its owner to confirm its hosts.
pub const PENDING_TTL: Duration = Duration::from_secs(60 * 60);
/// The most skills an agent may have, besides the bundled one, counting
/// those waiting for confirmation.
pub const MAX_SKILLS: usize = 32;
/// Where skills waiting for confirmation are kept, under the data
/// directory.
pub const PENDING_DIR: &str = "skills-pending";
/// Where skills are fetched and checked, under the data directory.
pub const WORK_DIR: &str = "skills-work";
/// How long since it last changed before startup takes a directory no row
/// records as left over: longer than any clone, with a margin.
pub const STALE_AFTER: Duration = Duration::from_secs(git::CLONE_TIMEOUT.as_secs() + 180);
/// How long a skill's lease lasts unless released: past the moves and the
/// two store calls a change makes, each of which may wait the pool's
/// 30-second acquire timeout and then the store's 5-second busy timeout.
/// A lease a crash left keeps the name busy that long. The store writes
/// check the lease, so a change that outlives it writes nothing.
pub const LEASE_TTL: Duration = Duration::from_secs(120);
/// How long an add, confirmation or removal waits for another change to
/// the same skill to finish before saying one is in progress.
pub const LEASE_WAIT: Duration = Duration::from_secs(2);
/// How often a change waiting for a skill's lease tries again.
const LEASE_RETRY: Duration = Duration::from_millis(100);
/// How often a skill whose files in use don't declare its hosts is warned
/// about; the denials between are logged at debug level, and counted.
pub const MISMATCH_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Writes the bundled skill into `agent`'s skills directory under
/// `data_dir`, if it isn't there as it should be. Returns whether it
/// changed.
///
/// # Errors
///
/// [`runner::RunnerError::Io`] if it can't be written.
pub async fn write_bundled(data_dir: &Path, agent: AgentId) -> runner::Result<bool> {
    let dir = runner::skills_dir(data_dir, agent).join(BUNDLED_NAME);
    runner::write_if_changed(&dir, package::SKILL_FILE, BUNDLED_SKILL.as_bytes()).await
}

/// Where a skill's files come from. Its `Debug` shows an upload's length,
/// not its bytes.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// A Git repository: an `https://` URL with an optional `#ref`, as
    /// [`commands::parse`] accepts it.
    Git(&'a str),
    /// A file the owner attached: a `SKILL.md` (any `.md` name), or a
    /// `.zip` holding one.
    Upload {
        /// The file's name, which says which it is.
        name: &'a str,
        /// Its bytes.
        bytes: &'a [u8],
    },
}

impl fmt::Debug for Source<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(url) => f.debug_tuple("Git").field(url).finish(),
            Self::Upload { name, bytes } => f
                .debug_struct("Upload")
                .field("name", name)
                .field("bytes_len", &bytes.len())
                .finish(),
        }
    }
}

impl Source<'_> {
    /// What the `agent_skills` row records as the source.
    fn recorded(&self) -> String {
        match self {
            Self::Git(url) => (*url).to_owned(),
            Self::Upload { name, .. } => format!("upload:{name}"),
        }
    }
}

/// What [`Skills::add`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    /// The skill is in use, replacing one of the same name if there was.
    Active(Manifest),
    /// The skill waits for its owner to confirm the hosts it declares.
    Pending(Manifest),
}

/// What [`Skills::remove`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// It removed the skill in use, and with it any hosts its owner
    /// confirmed: `had_hosts` says whether there were any.
    Active {
        /// Whether the skill had confirmed hosts.
        had_hosts: bool,
    },
    /// It removed a skill still waiting for its owner to confirm its
    /// hosts, or files no row records, such as those a removal that failed
    /// on the disk left after deleting the rows: no host is granted for it
    /// now, though one may have been before.
    Unconfirmed,
    /// The agent has no skill of that name.
    NotFound,
    /// It refused: [`BUNDLED_NAME`] is built into every agent.
    Bundled,
    /// Another change to the skill is in progress; nothing changed.
    Busy,
}

/// Why [`Skills::add`] refused a skill: something the owner can fix, said
/// in its message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The files, or the `SKILL.md`, were refused.
    #[error(transparent)]
    Problem(#[from] Problem),
    /// The clone failed.
    #[error(transparent)]
    Clone(CloneError),
    /// An attached file that is neither a `.md` nor a `.zip`.
    #[error("Attach the skill as its SKILL.md, or as a .zip holding it.")]
    FileKind,
    /// The agent has as many skills as it may.
    #[error("An agent may have at most {MAX_SKILLS} skills. Remove one first with `skill rm`.")]
    TooMany,
    /// Another change to the skill is in progress.
    #[error("Another change to this skill is in progress. Try again in a moment.")]
    Busy,
}

/// Why a skill operation failed on agentd's side. Logged, never shown.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Reading or writing the disk failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        source: std::io::Error,
    },
    /// Cloning failed on agentd's side.
    #[error("{0}")]
    Git(String),
    /// A blocking task panicked.
    #[error("a skill task failed: {0}")]
    Task(String),
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> SkillError {
    move |source| SkillError::Io { what, source }
}

/// What [`Skills::add`] returns when checking the files failed: the
/// owner's problem to fix, or agentd's failure.
fn refused(err: CheckError) -> Result<Result<Added, Refused>, SkillError> {
    match err {
        CheckError::Problem(problem) => Ok(Err(Refused::Problem(problem))),
        CheckError::Io { what, source } => Err(SkillError::Io { what, source }),
    }
}

/// What [`Skills::confirm`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// The skill is in use, with its hosts.
    Active(AgentSkill),
    /// No skill of that name waits for confirmation.
    NotPending,
    /// It waited longer than [`PENDING_TTL`]. The sweeper drops it.
    Expired,
    /// Another change to the skill is in progress; nothing changed.
    Busy,
}

/// Adds, confirms and removes agents' skills.
///
/// Cloning is cheap and shares everything.
#[derive(Clone)]
pub struct Skills {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    data_dir: PathBuf,
    git: Git,
    changes: Arc<RwLock<()>>,
    mismatches: Mutex<Throttle<(AgentId, String)>>,
}

impl fmt::Debug for Skills {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Skills")
            .field("data_dir", &self.inner.data_dir)
            .field("git", &self.inner.git)
            .finish_non_exhaustive()
    }
}

impl Skills {
    /// Skills recorded in `store`, with their files under `data_dir`,
    /// cloned with `git`.
    pub fn new(store: Store, data_dir: PathBuf, git: Git) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                data_dir,
                git,
                changes: Arc::default(),
                mismatches: Mutex::new(Throttle::new(MISMATCH_WARN_INTERVAL)),
            }),
        }
    }

    fn live_dir(&self, agent: AgentId, name: &str) -> PathBuf {
        runner::skills_dir(&self.inner.data_dir, agent).join(name)
    }

    fn pending_dir(&self, agent: AgentId, name: &str) -> PathBuf {
        self.inner
            .data_dir
            .join(PENDING_DIR)
            .join(agent.to_string())
            .join(name)
    }

    /// A new directory for work in progress, removed when the returned
    /// guard is dropped.
    async fn work_dir(&self) -> Result<WorkDir, SkillError> {
        let path = self
            .inner
            .data_dir
            .join(WORK_DIR)
            .join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(io("creating a skill work directory"))?;
        Ok(WorkDir(path))
    }

    /// The agent's skills, active and pending, by name.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn list(&self, agent: AgentId) -> Result<Vec<AgentSkill>, SkillError> {
        Ok(self.inner.store.agent_skills(agent).await?)
    }

    /// Fetches the skill `source` names, checks it, and adds it to `agent`
    /// on behalf of `by`: in use at once, or, when it declares hosts,
    /// waiting for `by` to confirm them.
    ///
    /// # Errors
    ///
    /// `Ok(Err(Refused))` for a skill the owner has to fix, `Err` when
    /// agentd failed.
    pub async fn add(
        &self,
        agent: AgentId,
        source: Source<'_>,
        by: MemberId,
    ) -> Result<Result<Added, Refused>, SkillError> {
        let work = self.work_dir().await?;
        let fetched = work.0.join("src");
        match source {
            Source::Git(url) => match self.inner.git.clone_into(url, &fetched).await {
                Ok(()) => {}
                Err(CloneError::Internal(err)) => return Err(SkillError::Git(err)),
                Err(err) => return Ok(Err(Refused::Clone(err))),
            },
            Source::Upload { name, bytes } => {
                let lower = name.to_ascii_lowercase();
                let unpack: fn(&[u8], &Path) -> Result<(), CheckError> = if lower.ends_with(".zip")
                {
                    package::unpack_zip
                } else if lower.ends_with(".md") {
                    package::write_skill_file
                } else {
                    return Ok(Err(Refused::FileKind));
                };
                let bytes = bytes.to_vec();
                let dir = fetched.clone();
                if let Err(err) = blocking(move || unpack(&bytes, &dir)).await? {
                    return refused(err);
                }
            }
        }
        let checked = blocking(move || {
            package::check_tree(&fetched)?;
            let (dir, manifest) = package::find_skill(&fetched)?;
            let digest = package::tree_digest(&dir)?;
            Ok((dir, manifest, digest))
        })
        .await?;
        let (dir, manifest, digest) = match checked {
            Ok(found) => found,
            Err(err) => return refused(err),
        };
        let recorded = source.recorded();
        let name = manifest.name.as_str().to_owned();
        let added = self
            .leased(agent, &name, LEASE_WAIT, move |skills, lease| async move {
                let hosts = host_names(&manifest);
                let new = NewSkill {
                    agent,
                    name: manifest.name.as_str(),
                    source: &recorded,
                    hosts: &hosts,
                    digest: &digest,
                    added_by: by,
                };
                let put = skills.put_added(&new, &dir, &work.0, lease).await?;
                Ok(if !put {
                    Err(Refused::TooMany)
                } else if hosts.is_empty() {
                    Ok(Added::Active(manifest))
                } else {
                    Ok(Added::Pending(manifest))
                })
            })
            .await?;
        Ok(added.unwrap_or(Err(Refused::Busy)))
    }

    /// Records the skill `new` and moves its files from `dir` into place,
    /// under the skill's `lease`, and returns whether it recorded it: not
    /// when the agent has as many skills as it may. `work` is the add's
    /// work directory, on the same file system.
    async fn put_added(
        &self,
        new: &NewSkill<'_>,
        dir: &Path,
        work: &Path,
        lease: LeaseId,
    ) -> Result<bool, SkillError> {
        let (agent, name) = (new.agent, new.name);
        let store = &self.inner.store;
        let state = if new.hosts.is_empty() {
            SkillState::Active
        } else {
            SkillState::Pending
        };
        let pending = self.pending_dir(agent, name);
        if state == SkillState::Pending {
            store
                .delete_skill(
                    agent,
                    name,
                    Some(SkillState::Pending),
                    lease,
                    OffsetDateTime::now_utc(),
                )
                .await?;
            remove_dir(&pending).await?;
        }
        let now = OffsetDateTime::now_utc();
        if !store.put_skill(new, state, MAX_SKILLS, now, lease).await? {
            return Ok(false);
        }
        if state == SkillState::Pending {
            move_into(dir, &pending, work).await?;
            tracing::info!(%agent, skill = name, hosts = new.hosts.len(), "a skill waits for its hosts to be confirmed");
            return Ok(true);
        }
        move_into(dir, &self.live_dir(agent, name), work).await?;
        if let Err(err) = remove_dir(&pending).await {
            tracing::warn!(%agent, skill = name, error = %err, "couldn't remove a superseded pending skill; adding or removing it again will");
        }
        tracing::info!(%agent, skill = name, "added a skill");
        Ok(true)
    }

    /// Puts `agent`'s skill `name`, waiting for confirmation, in use with
    /// its hosts, under the skill's lease.
    ///
    /// The files move into place before the row becomes active, so a
    /// failure between the two leaves files without their hosts, never
    /// hosts for files the owner didn't confirm; a failure moving them
    /// leaves the skill waiting. Only the pending row this reads becomes
    /// active, and only for files declaring its hosts. It never deletes a
    /// row: one that expired, or whose files aren't waiting, is left to
    /// [`drop_expired`](Self::drop_expired) or to adding the skill again.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn confirm(&self, agent: AgentId, name: &str) -> Result<Confirmed, SkillError> {
        let owned = name.to_owned();
        let confirmed = self
            .leased(agent, name, LEASE_WAIT, move |skills, lease| async move {
                skills.confirm_leased(agent, &owned, lease).await
            })
            .await?;
        Ok(confirmed.unwrap_or(Confirmed::Busy))
    }

    async fn confirm_leased(
        &self,
        agent: AgentId,
        name: &str,
        lease: LeaseId,
    ) -> Result<Confirmed, SkillError> {
        let waiting = self
            .inner
            .store
            .agent_skills(agent)
            .await?
            .into_iter()
            .find(|skill| skill.name == name && skill.state == SkillState::Pending);
        match waiting {
            Some(waiting) => self.confirm_row(&waiting, lease).await,
            None => Ok(Confirmed::NotPending),
        }
    }

    /// Confirms the pending row `waiting` as [`confirm`](Self::confirm)
    /// read it, under the skill's `lease`. Files that aren't in the pending
    /// directory, because an add failed or died before moving them in, or
    /// that declare other hosts than the row, leave nothing to confirm. If
    /// the row can't be made active, the move is undone ([`put_back`])
    /// before saying so, unless the lease was lost: whoever holds it now
    /// owns the files, which stay as a crash would leave them.
    ///
    /// A confirmation stopped after its move and before its row write
    /// ([`moved_in`](Self::moved_in)) is finished: the row becomes active
    /// for the files in use, which are the ones it was added with.
    async fn confirm_row(
        &self,
        waiting: &AgentSkill,
        lease: LeaseId,
    ) -> Result<Confirmed, SkillError> {
        if waiting.added_at < OffsetDateTime::now_utc() - PENDING_TTL {
            return Ok(Confirmed::Expired);
        }
        let store = &self.inner.store;
        let (agent, name) = (waiting.agent, waiting.name.as_str());
        if self.moved_in(waiting).await? {
            let confirmed = store
                .confirm_skill(waiting, lease, OffsetDateTime::now_utc())
                .await?;
            tracing::info!(%agent, skill = name, confirmed = confirmed.is_some(), "finished a confirmation stopped after its move");
            return Ok(confirmed.map_or(Confirmed::NotPending, Confirmed::Active));
        }
        let pending = self.pending_dir(agent, name);
        if declared_hosts(&pending).await.as_ref() != Some(&waiting.hosts) {
            tracing::info!(%agent, skill = name, "no files declaring a pending skill's hosts wait for it; confirmed nothing");
            return Ok(Confirmed::NotPending);
        }
        let live = self.live_dir(agent, name);
        let work = self.work_dir().await?;
        move_into(&pending, &live, &work.0).await?;
        match store
            .confirm_skill(waiting, lease, OffsetDateTime::now_utc())
            .await
        {
            Ok(Some(skill)) => {
                tracing::info!(%agent, skill = name, hosts = skill.hosts.len(), "confirmed a skill's hosts");
                Ok(Confirmed::Active(skill))
            }
            Ok(None) => {
                put_back(&live, &pending, &work.0).await?;
                tracing::info!(%agent, skill = name, "a skill's pending row changed while it was confirmed; undid the move");
                Ok(Confirmed::NotPending)
            }
            Err(StoreError::SkillLeaseLost) => {
                tracing::warn!(%agent, skill = name, "a confirmation lost its lease after its move; left the files to the next change");
                Err(StoreError::SkillLeaseLost.into())
            }
            Err(err) => {
                if let Err(undo) = put_back(&live, &pending, &work.0).await {
                    tracing::warn!(%agent, skill = name, error = %undo, "couldn't undo a failed confirmation's move");
                }
                Err(err.into())
            }
        }
    }

    /// Whether the files `waiting` was added with are already in use: its
    /// pending directory is gone and the live files' digest is the row's,
    /// as a confirmation stopped after its move and before its row write
    /// leaves them.
    async fn moved_in(&self, waiting: &AgentSkill) -> Result<bool, SkillError> {
        let (agent, name) = (waiting.agent, waiting.name.as_str());
        if is_dir(&self.pending_dir(agent, name)).await? {
            return Ok(false);
        }
        let live = self.live_dir(agent, name);
        let digest = blocking(move || package::tree_digest(&live)).await?;
        Ok(digest.is_ok_and(|digest| digest == waiting.digest))
    }

    /// Removes `agent`'s skill `name`, in use or waiting, with its hosts,
    /// and says which it was. The bundled skill is never removed.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn remove(&self, agent: AgentId, name: &str) -> Result<Removed, SkillError> {
        if name == BUNDLED_NAME {
            return Ok(Removed::Bundled);
        }
        let owned = name.to_owned();
        let removed = self
            .leased(agent, name, LEASE_WAIT, move |skills, lease| async move {
                skills.remove_leased(agent, &owned, lease).await
            })
            .await?;
        Ok(removed.unwrap_or(Removed::Busy))
    }

    async fn remove_leased(
        &self,
        agent: AgentId,
        name: &str,
        lease: LeaseId,
    ) -> Result<Removed, SkillError> {
        let rows = self
            .inner
            .store
            .delete_skill(agent, name, None, lease, OffsetDateTime::now_utc())
            .await?;
        let live = remove_dir(&self.live_dir(agent, name)).await?;
        let pending = remove_dir(&self.pending_dir(agent, name)).await?;
        tracing::info!(%agent, skill = name, rows = rows.len(), live, pending, "removed a skill");
        let active = rows.iter().find(|row| row.state == SkillState::Active);
        Ok(if let Some(active) = active {
            Removed::Active {
                had_hosts: !active.hosts.is_empty(),
            }
        } else if !rows.is_empty() || live || pending {
            Removed::Unconfirmed
        } else {
            Removed::NotFound
        })
    }

    /// Deletes skills that waited too long for confirmation, with their
    /// files, each under its lease, and then the leases that ended, such as
    /// those a crash left on names nothing changes again. The sweeper calls
    /// it every [`SWEEP_INTERVAL`], and startup once. It lets a skill wait
    /// that long past [`PENDING_TTL`], so a confirmation shortly after the
    /// deadline answers that the skill waited too long.
    ///
    /// # Errors
    ///
    /// If listing the expired skills or deleting the ended leases fails. A
    /// name whose lease is held is skipped until the next sweep, and one
    /// whose lease or row can't be read or written is logged and tried
    /// again then; the others still go. Files whose row was deleted but
    /// that can't be removed are logged too: never mounted, they stay on
    /// disk until startup's [`purge`](Self::purge) if no row has the name,
    /// or else, since purge keeps every directory of a name with a row,
    /// until the name is next added or removed.
    pub async fn drop_expired(&self) -> Result<(), SkillError> {
        let store = &self.inner.store;
        let before = OffsetDateTime::now_utc() - PENDING_TTL - SWEEP_INTERVAL;
        for (agent, name) in store.pending_skills_before(before).await? {
            let owned = name.clone();
            let dropped = self
                .leased(
                    agent,
                    &name,
                    Duration::ZERO,
                    move |skills, lease| async move {
                        skills
                            .drop_expired_skill(agent, &owned, before, lease)
                            .await
                    },
                )
                .await;
            if let Err(err) = dropped {
                tracing::warn!(%agent, skill = name.as_str(), error = %err, "couldn't drop an expired skill or its files");
            }
        }
        store
            .delete_ended_skill_leases(OffsetDateTime::now_utc())
            .await?;
        Ok(())
    }

    /// Deletes `agent`'s pending skill `name`, under its `lease`, if it was
    /// added before `before`, and then its files. A confirmation stopped
    /// after its move is never finished here: only the owner's `confirm`
    /// consents to the pending row's hosts, and the grant check
    /// ([`granted_hosts`](Self::granted_hosts)) already denies live files
    /// the active row doesn't match.
    async fn drop_expired_skill(
        &self,
        agent: AgentId,
        name: &str,
        before: OffsetDateTime,
        lease: LeaseId,
    ) -> Result<(), SkillError> {
        let now = OffsetDateTime::now_utc();
        if self
            .inner
            .store
            .delete_pending_skill_before(agent, name, before, lease, now)
            .await?
        {
            remove_dir(&self.pending_dir(agent, name)).await?;
        }
        Ok(())
    }

    /// Runs the change `work` makes, given the lease on `agent`'s skill
    /// `name`, which keeps every other add, confirmation, removal and
    /// expiry of that name, on any instance, waiting until it is done.
    /// Waits up to `wait` for the lease, and returns `None` without running
    /// `work` if it is still held.
    ///
    /// The lease is taken, `work` run and the lease released in a task of
    /// their own, which the caller only awaits: a command aborted at
    /// shutdown leaves it running, so a change's moves and row writes
    /// aren't cut apart there, and shutdown waits for it a while
    /// ([`drain`](Self::drain)) before closing the store. The lease is
    /// released whether `work` returns or panics. If agentd exits first,
    /// the runtime drops the task at its next await, which, unlike a crash,
    /// also runs its work directory's guard, removing a skill a move set
    /// aside there; the lease ends after [`LEASE_TTL`].
    async fn leased<T, F, W>(
        &self,
        agent: AgentId,
        name: &str,
        wait: Duration,
        work: W,
    ) -> Result<Option<T>, SkillError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, SkillError>> + Send + 'static,
        W: FnOnce(Skills, LeaseId) -> F + Send + 'static,
    {
        let skills = self.clone();
        let name = name.to_owned();
        tokio::spawn(async move {
            let _running = skills.inner.changes.clone().read_owned().await;
            let store = &skills.inner.store;
            let deadline = tokio::time::Instant::now() + wait;
            let lease = loop {
                let now = OffsetDateTime::now_utc();
                if let Some(lease) = store
                    .acquire_skill_lease(agent, &name, now, LEASE_TTL)
                    .await?
                {
                    break lease;
                }
                if tokio::time::Instant::now() >= deadline {
                    tracing::info!(%agent, skill = name.as_str(), "another change to a skill holds its lease; changed nothing");
                    return Ok(None);
                }
                tokio::time::sleep(LEASE_RETRY).await;
            };
            let done = tokio::spawn(work(skills.clone(), lease)).await;
            match store.release_skill_lease(agent, &name, lease).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(%agent, skill = name.as_str(), "a skill's lease ran out before its change finished");
                }
                Err(err) => {
                    tracing::warn!(%agent, skill = name.as_str(), error = %err, "couldn't release a skill's lease; it ends on its own");
                }
            }
            done.map_err(|err| SkillError::Task(err.to_string()))?
                .map(Some)
        })
        .await
        .map_err(|err| SkillError::Task(err.to_string()))?
    }

    /// Waits up to `timeout` for the skill changes running to finish, as
    /// shutdown does before closing the store, and returns whether they
    /// did. A change that starts while it waits waits for it; one that
    /// starts after shutdown has closed the store fails before taking a
    /// lease.
    pub async fn drain(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.inner.changes.write())
            .await
            .is_ok()
    }

    /// The hosts `session`'s agent may reach for its skills: each active
    /// skill's hosts, granted only while the skill's live `SKILL.md`
    /// declares exactly those hosts. This is what keeps hosts from ever
    /// covering files that don't declare them, whatever a crash, an
    /// aborted change or a lapsed lease left on disk; a skill whose files
    /// declare others, or can't be read, grants none, and is warned about
    /// at most once every [`MISMATCH_WARN_INTERVAL`].
    ///
    /// Only the front matter is read, so each skill costs a bounded read.
    /// The row's hosts were written by `host_names` at add time, and are
    /// compared with what it gives for the files today: a change to how
    /// [`package::parse_skill_file`] or [`HostRule`] normalizes a host must
    /// migrate the stored rows, or existing skills lose their hosts.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn granted_hosts(&self, session: SessionId) -> Result<Vec<String>, SkillError> {
        let mut hosts = Vec::new();
        for skill in self.inner.store.active_skills_for_session(session).await? {
            let declared = declared_hosts(&self.live_dir(skill.agent, &skill.name)).await;
            if declared.as_ref() == Some(&skill.hosts) {
                hosts.extend(skill.hosts);
                continue;
            }
            let declared = declared.as_ref().map(Vec::len);
            let warn = self
                .inner
                .mismatches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record((skill.agent, skill.name.clone()), Instant::now());
            match warn {
                Some(quiet) => {
                    tracing::warn!(agent = %skill.agent, skill = skill.name.as_str(), hosts = skill.hosts.len(), ?declared, denied_since_last_warning = quiet, "a skill's files in use don't declare its hosts; granted none of them")
                }
                None => {
                    tracing::debug!(agent = %skill.agent, skill = skill.name.as_str(), hosts = skill.hosts.len(), ?declared, "a skill's files in use don't declare its hosts; granted none of them")
                }
            }
        }
        hosts.sort();
        hosts.dedup();
        Ok(hosts)
    }

    /// Cleans up at startup: deletes skills that waited too long for
    /// confirmation, and removes what no row records once it is older than
    /// [`STALE_AFTER`]: work directories, and skills' files, waiting or in
    /// use, whose name has no row in either state (the bundled skill
    /// aside). A row of either state keeps both directories of its name,
    /// so a move in progress between them is never taken for left over.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn purge(&self) -> Result<(), SkillError> {
        self.purge_older_than(STALE_AFTER).await
    }

    async fn purge_older_than(&self, age: Duration) -> Result<(), SkillError> {
        self.drop_expired().await?;
        let data = &self.inner.data_dir;
        for work in read_dir_names(&data.join(WORK_DIR))
            .await?
            .unwrap_or_default()
        {
            remove_stale(&data.join(WORK_DIR).join(work), age).await?;
        }
        for (root, live) in [
            (data.join(PENDING_DIR), false),
            (data.join(runner::SKILLS_DIR), true),
        ] {
            for agent_dir in read_dir_names(&root).await?.unwrap_or_default() {
                let Ok(agent) = agent_dir.parse::<AgentId>() else {
                    remove_stale(&root.join(&agent_dir), age).await?;
                    continue;
                };
                let recorded: HashSet<String> = self
                    .inner
                    .store
                    .agent_skills(agent)
                    .await?
                    .into_iter()
                    .map(|skill| skill.name)
                    .collect();
                for name in read_dir_names(&root.join(&agent_dir))
                    .await?
                    .unwrap_or_default()
                {
                    let bundled = live && name == BUNDLED_NAME;
                    if !bundled && !recorded.contains(&name) {
                        remove_stale(&root.join(&agent_dir).join(&name), age).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A work directory, removed on drop.
struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.0.display(), error = %err, "couldn't remove a skill work directory");
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SkillError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| SkillError::Task(err.to_string()))
}

/// The hosts `manifest` declares, as a row records them.
fn host_names(manifest: &Manifest) -> Vec<String> {
    manifest.hosts.iter().map(ToString::to_string).collect()
}

/// The hosts the `SKILL.md` in the skill directory `dir` declares, or
/// `None` if it can't be read as one. Only the first
/// [`package::MAX_FRONT_MATTER_BYTES`] are parsed, the most front matter
/// may take, and of a file longer than that only its whole lines, so a
/// line cut by the limit never counts: every file the checks accepted at
/// add reads as it did then.
async fn declared_hosts(dir: &Path) -> Option<Vec<String>> {
    use tokio::io::AsyncReadExt as _;
    let limit = package::MAX_FRONT_MATTER_BYTES;
    let file = tokio::fs::File::open(dir.join(package::SKILL_FILE))
        .await
        .ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() > limit {
        bytes.truncate(limit);
        let whole = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(whole);
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    package::parse_skill_file(text)
        .ok()
        .map(|manifest| host_names(&manifest))
}

/// Undoes [`Skills::confirm_row`]'s move of the files in `pending` into
/// `live`: moves them back, and the skill they replaced, which the move set
/// aside in `aside`, back into `live`. Files that can't move back are moved
/// into `aside` instead, whose guard removes them, so the old skill can
/// take their place and its row never covers them. Restoring it is tried
/// whatever happened to them; only if they can't leave `live` at all does
/// it fail, and the old skill then goes with `aside`.
async fn put_back(live: &Path, pending: &Path, aside: &Path) -> Result<(), SkillError> {
    let mut back = Ok(());
    if let Err(err) = tokio::fs::rename(live, pending).await {
        back = Err(io("putting a skill back to wait")(err));
        if let Err(err) = tokio::fs::rename(live, aside.join("new")).await {
            tracing::warn!(dir = %live.display(), error = %err, "couldn't move aside files a confirmation couldn't put back");
        }
    }
    match tokio::fs::rename(aside.join("old"), live).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(dir = %live.display(), error = %err, "couldn't put back the skill a confirmation replaced");
            return Err(io("putting a skill back")(err));
        }
    }
    back
}

/// Moves the directory `from` to `to`, replacing what is there: the old
/// directory is first moved aside into `aside`, a work directory on the
/// same file system whose guard removes it, and moved back if `from`
/// can't take its place.
async fn move_into(from: &Path, to: &Path, aside: &Path) -> Result<(), SkillError> {
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(io("creating a skills directory"))?;
    }
    let old = aside.join("old");
    let replaced = match tokio::fs::rename(to, &old).await {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(io("moving a skill aside")(err)),
    };
    if let Err(err) = tokio::fs::rename(from, to).await {
        if replaced && let Err(restore) = tokio::fs::rename(&old, to).await {
            tracing::warn!(dir = %to.display(), error = %restore, "couldn't move back the skill a failed move set aside; it goes with the work directory");
        }
        return Err(io("moving a skill into place")(err));
    }
    Ok(())
}

/// Removes `dir` unless it changed within `age`, going by its status
/// change time, which creating it, or adding or removing an entry in it,
/// updates.
async fn remove_stale(dir: &Path, age: Duration) -> Result<(), SkillError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io("reading a skill directory")(err)),
    };
    let changed =
        OffsetDateTime::from_unix_timestamp(meta.ctime()).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    if OffsetDateTime::now_utc() - changed < age {
        return Ok(());
    }
    if meta.is_dir() {
        remove_dir(dir).await?;
    } else {
        tokio::fs::remove_file(dir)
            .await
            .map_err(io("removing a stray file"))?;
    }
    tracing::info!(path = %dir.display(), "removed a skill directory no row records");
    Ok(())
}

/// Removes the directory `dir` and everything in it, without following
/// symlinks. Returns whether it existed.
async fn remove_dir(dir: &Path) -> Result<bool, SkillError> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io("removing a skill directory")(err)),
    }
}

async fn is_dir(dir: &Path) -> Result<bool, SkillError> {
    match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) => Ok(meta.is_dir()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io("reading a skill directory")(err)),
    }
}

/// The names in the directory `dir`, or `None` if it doesn't exist.
async fn read_dir_names(dir: &Path) -> Result<Option<Vec<String>>, SkillError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io("reading a skills directory")(err)),
    };
    let mut names = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(io("reading a skills directory"))?
    {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(Some(names))
}

/// The egress proxy's [`EgressExtension`]: a session's extra hosts are
/// the hosts its agent's active skills declare
/// ([`Skills::granted_hosts`]), at each `CONNECT` the configured allowlist
/// doesn't already allow.
#[derive(Debug, Clone)]
pub struct SkillHosts(pub Skills);

#[async_trait]
impl EgressExtension for SkillHosts {
    async fn rules(&self, session: SessionId) -> Vec<HostRule> {
        match self.0.granted_hosts(session).await {
            Ok(hosts) => hosts
                .iter()
                .filter_map(|host| match host.parse::<HostRule>() {
                    Ok(rule) => Some(rule),
                    Err(err) => {
                        tracing::warn!(%session, error = %err, "a stored skill host isn't a valid rule; skipped it");
                        None
                    }
                })
                .collect(),
            Err(err) => {
                tracing::warn!(%session, error = %err, "couldn't read a session's skill hosts; allowing none");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests;
