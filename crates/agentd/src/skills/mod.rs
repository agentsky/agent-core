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
//! The row and the files can't change together, so they change in the
//! order that never grants hosts to files the owner didn't confirm them
//! for: `add` records the row first (a new active row carries no hosts,
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
//! They pass the same checks as configured rules, so `api.anthropic.com`
//! and private, loopback and metadata addresses stay out of reach.

pub mod git;
pub mod package;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use core_types::{AgentId, MemberId, SessionId};
use cred_proxy::{EgressExtension, HostRule};
use store::{AgentSkill, NewSkill, SkillState, Store, StoreError};
use time::OffsetDateTime;

pub use git::{CloneError, Git};
pub use package::{BUNDLED_NAME, Manifest, Problem};

use package::CheckError;

use crate::sweeper::SWEEP_INTERVAL;

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

/// Whether the pending row `waiting` waited longer than [`PENDING_TTL`].
fn is_expired(waiting: &AgentSkill) -> bool {
    waiting.added_at < OffsetDateTime::now_utc() - PENDING_TTL
}

/// What [`Skills::confirm`] answers for the pending row `waiting` once it
/// is gone: that it expired, if it had, or that nothing waits.
fn gone(waiting: &AgentSkill) -> Confirmed {
    if is_expired(waiting) {
        Confirmed::Expired
    } else {
        Confirmed::NotPending
    }
}

/// What [`Skills::confirm`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// The skill is in use, with its hosts.
    Active(AgentSkill),
    /// No skill of that name waits for confirmation.
    NotPending,
    /// It waited longer than [`PENDING_TTL`] and is gone.
    Expired,
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
            package::find_skill(&fetched)
        })
        .await?;
        let (dir, manifest) = match checked {
            Ok(found) => found,
            Err(err) => return refused(err),
        };
        let name = manifest.name.as_str();
        let hosts = host_names(&manifest);
        let recorded = source.recorded();
        let new = NewSkill {
            agent,
            name,
            source: &recorded,
            hosts: &hosts,
            added_by: by,
        };
        let state = if hosts.is_empty() {
            SkillState::Active
        } else {
            SkillState::Pending
        };
        let pending = self.pending_dir(agent, name);
        if state == SkillState::Pending {
            self.inner
                .store
                .delete_skill(agent, name, Some(SkillState::Pending))
                .await?;
            remove_dir(&pending).await?;
        }
        let now = OffsetDateTime::now_utc();
        if !self
            .inner
            .store
            .put_skill(&new, state, MAX_SKILLS, now)
            .await?
        {
            return Ok(Err(Refused::TooMany));
        }
        if state == SkillState::Pending {
            move_into(&dir, &pending, &work.0).await?;
            tracing::info!(%agent, skill = name, hosts = hosts.len(), "a skill waits for its hosts to be confirmed");
            return Ok(Ok(Added::Pending(manifest)));
        }
        move_into(&dir, &self.live_dir(agent, name), &work.0).await?;
        if let Err(err) = remove_dir(&pending).await {
            tracing::warn!(%agent, skill = name, error = %err, "couldn't remove a superseded pending skill; adding or removing it again will");
        }
        tracing::info!(%agent, skill = name, "added a skill");
        Ok(Ok(Added::Active(manifest)))
    }

    /// Puts `agent`'s skill `name`, waiting for confirmation, in use with
    /// its hosts.
    ///
    /// The files move into place before the row becomes active, so a
    /// failure between the two leaves files without their hosts, never
    /// hosts for files the owner didn't confirm; a failure moving them
    /// leaves the skill waiting. Only the pending row this reads becomes
    /// active, and only for files declaring its hosts: if the files declare
    /// others, they are left waiting, and if the row went or was replaced
    /// meanwhile, the files are put back; either way nothing is confirmed.
    /// A row it finds expired or without files is dropped, unless the skill
    /// was added again first: the row waiting now is then read and
    /// confirmed instead, once.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn confirm(&self, agent: AgentId, name: &str) -> Result<Confirmed, SkillError> {
        match self.waiting(agent, name).await? {
            Some(waiting) => self.confirm_row(&waiting).await,
            None => Ok(Confirmed::NotPending),
        }
    }

    /// `agent`'s pending row for the skill `name`, if one waits.
    async fn waiting(&self, agent: AgentId, name: &str) -> Result<Option<AgentSkill>, SkillError> {
        Ok(self
            .inner
            .store
            .agent_skills(agent)
            .await?
            .into_iter()
            .find(|skill| skill.name == name && skill.state == SkillState::Pending))
    }

    /// Confirms the pending row `waiting` as [`confirm`](Self::confirm)
    /// read it, or the row waiting now, once, if that one was replaced
    /// before it could be dropped.
    async fn confirm_row(&self, waiting: &AgentSkill) -> Result<Confirmed, SkillError> {
        if let Some(confirmed) = self.try_confirm_row(waiting).await? {
            return Ok(confirmed);
        }
        let current = self.waiting(waiting.agent, &waiting.name).await?;
        Ok(match current {
            Some(current) if current != *waiting => self
                .try_confirm_row(&current)
                .await?
                .unwrap_or_else(|| gone(&current)),
            _ => gone(waiting),
        })
    }

    /// Confirms the pending row `waiting`, or drops it when it expired or
    /// its files are gone. `None` when it couldn't be dropped because it is
    /// no longer as `waiting` read it. The files are first moved into a
    /// work directory, where nothing else replaces them, so their inode and
    /// hosts are those of the files that move into place.
    async fn try_confirm_row(&self, waiting: &AgentSkill) -> Result<Option<Confirmed>, SkillError> {
        let store = &self.inner.store;
        let (agent, name) = (waiting.agent, waiting.name.as_str());
        let pending = self.pending_dir(agent, name);
        if is_expired(waiting) || !is_dir(&pending).await? {
            if !store.delete_pending_skill(waiting).await? {
                return Ok(None);
            }
            remove_dir(&pending).await?;
            return Ok(Some(gone(waiting)));
        }
        let work = self.work_dir().await?;
        let new = work.0.join("new");
        tokio::fs::rename(&pending, &new)
            .await
            .map_err(io("moving a skill aside"))?;
        let moved = inode(&new).await?;
        if declared_hosts(&new).await.as_ref() != Some(&waiting.hosts) {
            wait_again(&new, &pending).await;
            tracing::info!(%agent, skill = name, "a skill's files don't declare its pending row's hosts; left it waiting");
            return Ok(Some(Confirmed::NotPending));
        }
        let live = self.live_dir(agent, name);
        if let Err(err) = move_into(&new, &live, &work.0).await {
            wait_again(&new, &pending).await;
            return Err(err);
        }
        let Some(skill) = store.confirm_skill(waiting).await? else {
            self.put_back(&live, moved, &work.0).await?;
            tracing::info!(%agent, skill = name, "a skill's pending row changed while it was confirmed; undid the move");
            return Ok(Some(Confirmed::NotPending));
        };
        tracing::info!(%agent, skill = name, hosts = skill.hosts.len(), "confirmed a skill's hosts");
        Ok(Some(Confirmed::Active(skill)))
    }

    /// Undoes [`try_confirm_row`](Self::try_confirm_row)'s move into
    /// `live` once its row is gone, unless something else has taken the
    /// place of the files it moved, `moved` by inode: moves back the skill
    /// it set aside in `aside`, or removes the files it moved when there
    /// was none.
    async fn put_back(
        &self,
        live: &Path,
        moved: Option<u64>,
        aside: &Path,
    ) -> Result<(), SkillError> {
        if inode(live).await? != moved {
            return Ok(());
        }
        let old = aside.join("old");
        if is_dir(&old).await? {
            let back = self.work_dir().await?;
            move_into(&old, live, &back.0).await
        } else {
            remove_dir(live).await.map(drop)
        }
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
        let rows = self.inner.store.delete_skill(agent, name, None).await?;
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
    /// files. The sweeper calls it every [`SWEEP_INTERVAL`], and startup
    /// once. It lets a skill wait that long past [`PENDING_TTL`], so a
    /// confirmation that began before the deadline finds its row.
    ///
    /// # Errors
    ///
    /// If deleting the rows fails. A skill whose files can't be removed is
    /// logged and left to startup's [`purge`](Self::purge), and the others
    /// are still removed.
    pub async fn drop_expired(&self) -> Result<(), SkillError> {
        let before = OffsetDateTime::now_utc() - PENDING_TTL - SWEEP_INTERVAL;
        for (agent, name) in self
            .inner
            .store
            .delete_pending_skills_before(before)
            .await?
        {
            if let Err(err) = self.drop_expired_files(agent, &name).await {
                tracing::warn!(%agent, skill = name.as_str(), error = %err, "couldn't remove an expired skill's files; startup will");
            }
        }
        Ok(())
    }

    /// Removes the files of `agent`'s skill `name` once
    /// [`drop_expired`](Self::drop_expired) deleted its pending row, unless
    /// the skill was added again since and waits under a new pending row:
    /// the files waiting are then the new row's.
    async fn drop_expired_files(&self, agent: AgentId, name: &str) -> Result<(), SkillError> {
        let added_again = self
            .inner
            .store
            .agent_skills(agent)
            .await?
            .iter()
            .any(|skill| skill.name == name && skill.state == SkillState::Pending);
        if !added_again {
            remove_dir(&self.pending_dir(agent, name)).await?;
        }
        Ok(())
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
/// `None` if it can't be read as one.
async fn declared_hosts(dir: &Path) -> Option<Vec<String>> {
    let text = tokio::fs::read_to_string(dir.join(package::SKILL_FILE))
        .await
        .ok()?;
    package::parse_skill_file(&text)
        .ok()
        .map(|manifest| host_names(&manifest))
}

/// Moves a skill [`Skills::try_confirm_row`] took from `pending` back from
/// `new`, unless another has taken its place since.
async fn wait_again(new: &Path, pending: &Path) {
    if let Err(err) = tokio::fs::rename(new, pending).await {
        tracing::warn!(dir = %pending.display(), error = %err, "couldn't put a skill back to wait for confirmation");
    }
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
        if replaced {
            let _ = tokio::fs::rename(&old, to).await;
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

/// The inode of `dir`, which a rename keeps, or `None` if it doesn't
/// exist.
async fn inode(dir: &Path) -> Result<Option<u64>, SkillError> {
    use std::os::unix::fs::MetadataExt as _;
    match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) => Ok(Some(meta.ino())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io("reading a skill directory")(err)),
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

/// The egress proxy's [`EgressExtension`]: a session's extra hosts are the
/// hosts its agent's active skills declare, read from the store at each
/// `CONNECT` the configured allowlist doesn't already allow.
#[derive(Debug, Clone)]
pub struct SkillHosts(pub Store);

#[async_trait]
impl EgressExtension for SkillHosts {
    async fn rules(&self, session: SessionId) -> Vec<HostRule> {
        match self.0.skill_hosts_for_session(session).await {
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
