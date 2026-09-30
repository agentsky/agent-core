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
//! work in progress in `<data>/skills-work/`, which startup empties.
//! Directories are moved into place with a rename, so a session sees a
//! skill whole or not at all. A skill added, replaced or removed reaches a
//! conversation when its process next starts; the hosts change at once.
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

/// Where a skill's files come from.
#[derive(Debug, Clone, Copy)]
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
        let existing = self.inner.store.agent_skills(agent).await?;
        let names: HashSet<&str> = existing.iter().map(|s| s.name.as_str()).collect();
        if !names.contains(name) && names.len() >= MAX_SKILLS {
            return Ok(Err(Refused::TooMany));
        }
        let hosts: Vec<String> = manifest.hosts.iter().map(ToString::to_string).collect();
        let recorded = source.recorded();
        let new = NewSkill {
            agent,
            name,
            source: &recorded,
            hosts: &hosts,
            added_by: by,
        };
        let now = OffsetDateTime::now_utc();
        if hosts.is_empty() {
            move_into(&dir, &self.live_dir(agent, name), &work.0).await?;
            remove_dir(&self.pending_dir(agent, name)).await?;
            self.inner
                .store
                .put_skill(&new, SkillState::Active, now)
                .await?;
            tracing::info!(%agent, skill = name, "added a skill");
            Ok(Ok(Added::Active(manifest)))
        } else {
            move_into(&dir, &self.pending_dir(agent, name), &work.0).await?;
            self.inner
                .store
                .put_skill(&new, SkillState::Pending, now)
                .await?;
            tracing::info!(%agent, skill = name, hosts = hosts.len(), "a skill waits for its hosts to be confirmed");
            Ok(Ok(Added::Pending(manifest)))
        }
    }

    /// Puts `agent`'s skill `name`, waiting for confirmation, in use with
    /// its hosts.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn confirm(&self, agent: AgentId, name: &str) -> Result<Confirmed, SkillError> {
        let store = &self.inner.store;
        let pending = self.pending_dir(agent, name);
        if !is_dir(&pending).await? {
            store
                .delete_skill(agent, name, Some(SkillState::Pending))
                .await?;
            return Ok(Confirmed::NotPending);
        }
        let since = OffsetDateTime::now_utc() - PENDING_TTL;
        let Some(skill) = store.confirm_skill(agent, name, since).await? else {
            let expired = !store
                .delete_skill(agent, name, Some(SkillState::Pending))
                .await?
                .is_empty();
            remove_dir(&pending).await?;
            return Ok(if expired {
                Confirmed::Expired
            } else {
                Confirmed::NotPending
            });
        };
        let work = self.work_dir().await?;
        move_into(&pending, &self.live_dir(agent, name), &work.0).await?;
        tracing::info!(%agent, skill = name, hosts = skill.hosts.len(), "confirmed a skill's hosts");
        Ok(Confirmed::Active(skill))
    }

    /// Removes `agent`'s skill `name`, in use or waiting, with its hosts.
    /// Returns whether there was one.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn remove(&self, agent: AgentId, name: &str) -> Result<bool, SkillError> {
        let states = self.inner.store.delete_skill(agent, name, None).await?;
        let live = remove_dir(&self.live_dir(agent, name)).await?;
        let pending = remove_dir(&self.pending_dir(agent, name)).await?;
        tracing::info!(%agent, skill = name, rows = states.len(), live, pending, "removed a skill");
        Ok(!states.is_empty() || live || pending)
    }

    /// Deletes skills that waited too long for confirmation, with their
    /// files.
    async fn drop_expired(&self) -> Result<(), SkillError> {
        let before = OffsetDateTime::now_utc() - PENDING_TTL;
        for (agent, name) in self
            .inner
            .store
            .delete_pending_skills_before(before)
            .await?
        {
            remove_dir(&self.pending_dir(agent, &name)).await?;
        }
        Ok(())
    }

    /// Cleans up at startup: empties the work directory, and deletes skills
    /// that waited too long for confirmation, and waiting skills' files
    /// that no row records.
    ///
    /// # Errors
    ///
    /// If the store or the disk fails.
    pub async fn purge(&self) -> Result<(), SkillError> {
        remove_dir(&self.inner.data_dir.join(WORK_DIR)).await?;
        self.drop_expired().await?;
        let root = self.inner.data_dir.join(PENDING_DIR);
        let Some(agents) = read_dir_names(&root).await? else {
            return Ok(());
        };
        for agent_dir in agents {
            let Ok(agent) = agent_dir.parse::<AgentId>() else {
                remove_dir(&root.join(&agent_dir)).await?;
                continue;
            };
            let waiting: HashSet<String> = self
                .inner
                .store
                .agent_skills(agent)
                .await?
                .into_iter()
                .filter(|skill| skill.state == SkillState::Pending)
                .map(|skill| skill.name)
                .collect();
            for name in read_dir_names(&root.join(&agent_dir))
                .await?
                .unwrap_or_default()
            {
                if !waiting.contains(&name) {
                    remove_dir(&self.pending_dir(agent, &name)).await?;
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

/// Moves the directory `from` to `to`, replacing what is there: the old
/// directory is first moved aside into `aside`, a directory on the same
/// file system, and removed after.
async fn move_into(from: &Path, to: &Path, aside: &Path) -> Result<(), SkillError> {
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(io("creating a skills directory"))?;
    }
    let old = aside.join(format!("old-{}", uuid::Uuid::new_v4()));
    let replaced = match tokio::fs::rename(to, &old).await {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(io("moving a skill aside")(err)),
    };
    tokio::fs::rename(from, to)
        .await
        .map_err(io("moving a skill into place"))?;
    if replaced {
        remove_dir(&old).await?;
    }
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
