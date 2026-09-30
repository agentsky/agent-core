//! Where volumes and session directories live on the host, and creating
//! them.
//!
//! ```text
//! <data dir>/volumes/                      0700, agentd's
//!   <agent id>/<hex SHA-256 of scope key>/ one volume
//!     sessions/<session id>/               mounted read-write in its session
//!       work/ claude/ home/ tmp/           the sandbox user's
//!       claude/settings.json               rewritten before every start
//!     shared/                              the sandbox user's
//!     memory/                              Private volumes only
//! ```
//!
//! agentd runs as the sandbox user, so the agent can rename, replace,
//! remove or `chmod` anything inside its session directory, and inside
//! `shared/` and `memory/`. Host-side code here therefore never follows a
//! symlink there: an entry that should be a directory but is a symlink or
//! a file is replaced, each of those directories is given back to the
//! sandbox user with mode `0755`, and `settings.json` is written to a new
//! file and renamed into place. The directories above those mounts
//! (`volumes/`, the volume and its `sessions/`) are reachable from no
//! sandbox, so they are only created, never replaced.
//!
//! Inside the session directory every step goes through handles: the
//! session directory is opened once, each entry is looked up, replaced and
//! opened relative to its parent's handle, with `O_NOFOLLOW`, and the
//! `claude/` handle is where `settings.json` and `claude/skills` are
//! written. A handle is opened with `O_PATH`, which needs no permission on
//! the directory itself, so a directory the agent made mode `0` can still
//! be repaired. Linux has no `fchmod` for an `O_PATH` handle, so modes,
//! owners and recursive removals go through `/proc/self/fd/<handle>`,
//! which names the handle's directory and not whatever its path names
//! now.
//!
//! The repair runs only in [`Sandbox::start`](crate::Sandbox::start),
//! before the session's container exists, so nothing in the sandbox races
//! it; the handles make it not rely on that.

use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, chown, fchown};
use std::path::{Path, PathBuf};

use core_types::{ScopeKey, SessionId, VolumeKey};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use store::Store;

use crate::{Result, SandboxError, VolumeRef};

/// The directory under the data directory that holds every volume.
pub const VOLUMES_DIR: &str = "volumes";

/// The four directories each session gets inside `sessions/<id>/`.
pub const SESSION_SUBDIRS: [&str; 4] = ["work", "claude", "home", "tmp"];

/// The session subdirectory that is `CLAUDE_CONFIG_DIR`.
const CLAUDE_DIR: &str = "claude";

/// Where the skills directory appears in `claude/`.
const SKILLS_DIR: &str = "skills";

/// The mode of every directory the agent can write, reset on each start.
const DIR_MODE: u32 = 0o755;

/// A volume's directory name for `scope`: the lowercase hex SHA-256 of the
/// scope key's string form. It is 64 characters from `[0-9a-f]` for every
/// key, however long, and never holds the `:` or `%` a key may contain.
///
/// ```
/// use core_types::ScopeKey;
///
/// let name = sandbox::scope_dir_name(&ScopeKey::Private);
/// assert_eq!(name.len(), 64);
/// assert!(name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
/// ```
pub fn scope_dir_name(scope: &ScopeKey) -> String {
    Sha256::digest(scope.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A volume's directory relative to the data directory:
/// `volumes/<agent id>/<scope dir>`.
pub fn volume_rel_path(key: &VolumeKey) -> PathBuf {
    Path::new(VOLUMES_DIR)
        .join(key.agent.to_string())
        .join(scope_dir_name(&key.scope))
}

/// What [`Layout::prepare_session_dirs`] makes of `claude/skills`.
#[derive(Debug, Clone)]
pub(crate) enum SkillsEntry {
    /// Nothing: it is left as it is.
    Untouched,
    /// A real directory owned by the sandbox user, for Docker to mount the
    /// skills directory on.
    MountPoint,
    /// A symlink to the skills directory, or with `None` no symlink: one an
    /// earlier start left is removed.
    Link(Option<PathBuf>),
}

/// Creates volumes and session directories. Shared by both sandboxes.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub(crate) store: Store,
    pub(crate) data_dir: PathBuf,
    /// The uid and gid that agent-writable directories are given, or `None`
    /// to leave them to the current user.
    pub(crate) owner: Option<(u32, u32)>,
    pub(crate) cleanup_period_days: u32,
}

impl Layout {
    pub(crate) async fn ensure_volume(&self, key: &VolumeKey) -> Result<VolumeRef> {
        let rel = volume_rel_path(key);
        let volume = VolumeRef {
            key: key.clone(),
            path: self.data_dir.join(&rel),
        };
        let this = self.clone();
        let for_task = volume.clone();
        blocking(move || this.create_volume_dirs(&for_task)).await?;
        let rel = rel
            .to_str()
            .ok_or(SandboxError::InvalidSpec("volume path is not UTF-8"))?;
        self.store.put_volume(key, rel).await?;
        Ok(volume)
    }

    pub(crate) async fn prepare_session_dirs(
        &self,
        volume: &VolumeRef,
        session: SessionId,
        skills: SkillsEntry,
    ) -> Result<PathBuf> {
        let this = self.clone();
        let volume = volume.clone();
        blocking(move || {
            this.create_volume_dirs(&volume)?;
            let dir = volume.session_dir(session);
            create_dir(&dir, None)?;
            let session_dir = open_dir(&dir)?;
            set_mode(&session_dir)?;
            for sub in SESSION_SUBDIRS.into_iter().filter(|sub| *sub != CLAUDE_DIR) {
                this.repair_dir_at(&session_dir, sub)?;
            }
            let claude = this.repair_dir_at(&session_dir, CLAUDE_DIR)?;
            this.write_settings(&claude)?;
            this.place_skills(&claude, &skills)?;
            Ok(dir)
        })
        .await
    }

    /// Makes `name` in `parent`, an agent-writable directory, a real
    /// directory owned by the sandbox user with mode `0755`, and returns a
    /// handle on it.
    fn repair_dir_at(&self, parent: &File, name: &str) -> Result<File> {
        match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {}
            Ok(_) => {
                rustix::fs::unlinkat(parent, name, AtFlags::empty())
                    .map_err(errno("replacing a non-directory"))?;
                mkdir_at(parent, name)?;
            }
            Err(Errno::NOENT) => mkdir_at(parent, name)?,
            Err(err) => return Err(errno("inspecting a session directory")(err)),
        }
        let dir = open_dir_at(parent, name)?;
        self.claim(&dir)?;
        Ok(dir)
    }

    fn create_volume_dirs(&self, volume: &VolumeRef) -> Result<()> {
        let volumes = self.data_dir.join(VOLUMES_DIR);
        create_dir(&volumes, Some(0o700))?;
        let agent_dir = volume.path.parent().unwrap_or(&volumes);
        create_dir(agent_dir, None)?;
        create_dir(&volume.path, None)?;
        create_dir(&volume.path.join("sessions"), None)?;
        let shared = volume.shared_dir();
        create_dir(&shared, None)?;
        self.claim(&open_dir(&shared)?)?;
        if let Some(memory) = volume.memory_dir() {
            create_dir(&memory, None)?;
            self.claim(&open_dir(&memory)?)?;
        }
        Ok(())
    }

    /// Writes `settings.json` into the `claude/` directory `dir` through a
    /// new file renamed over the old one, so a symlink the agent left there
    /// is replaced, not followed. A directory in its place is renamed aside
    /// and removed. Every step is relative to `dir`, so nothing is written
    /// outside it.
    fn write_settings(&self, dir: &File) -> Result<()> {
        const TARGET: &str = "settings.json";
        let is_dir = rustix::fs::statat(dir, TARGET, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory);
        if is_dir {
            let aside = format!(".settings.json.{}.old", uuid::Uuid::new_v4());
            rustix::fs::renameat(dir, TARGET, dir, &aside)
                .map_err(errno("replacing settings.json"))?;
            let _ = fs::remove_dir_all(handle_path(dir).join(&aside));
        }
        let body = settings_json(self.cleanup_period_days);
        let temp = format!(".settings.json.{}", uuid::Uuid::new_v4());
        let file = rustix::fs::openat(
            dir,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(errno("writing settings.json"))?;
        let written = (|| {
            (&file).write_all(body.as_bytes())?;
            if let Some((uid, gid)) = self.owner {
                fchown(&file, Some(uid), Some(gid))?;
            }
            rustix::fs::renameat(dir, &temp, dir, TARGET).map_err(io::Error::from)
        })();
        if written.is_err() {
            let _ = rustix::fs::unlinkat(dir, &temp, AtFlags::empty());
        }
        written.map_err(io_err("writing settings.json"))
    }

    /// Makes `skills` in the `claude/` directory `dir` what `entry` says,
    /// relative to `dir`.
    fn place_skills(&self, dir: &File, entry: &SkillsEntry) -> Result<()> {
        let target = match entry {
            SkillsEntry::Untouched => return Ok(()),
            SkillsEntry::MountPoint => return self.repair_dir_at(dir, SKILLS_DIR).map(drop),
            SkillsEntry::Link(target) => target.as_deref(),
        };
        let what = "linking the skills directory";
        match rustix::fs::statat(dir, SKILLS_DIR, AtFlags::SYMLINK_NOFOLLOW)
            .map(|stat| FileType::from_raw_mode(stat.st_mode))
        {
            Ok(FileType::Directory) if target.is_some() => {
                fs::remove_dir_all(handle_path(dir).join(SKILLS_DIR)).map_err(io_err(what))?;
            }
            Ok(kind)
                if kind == FileType::Symlink
                    || (kind != FileType::Directory && target.is_some()) =>
            {
                rustix::fs::unlinkat(dir, SKILLS_DIR, AtFlags::empty()).map_err(errno(what))?;
            }
            Ok(_) | Err(Errno::NOENT) => {}
            Err(err) => return Err(errno(what)(err)),
        }
        if let Some(target) = target {
            rustix::fs::symlinkat(target, dir, SKILLS_DIR).map_err(errno(what))?;
        }
        Ok(())
    }

    /// Gives the directory `dir` to the sandbox user, when its owner
    /// differs, and sets its mode to `0755`.
    fn claim(&self, dir: &File) -> Result<()> {
        if let Some((uid, gid)) = self.owner {
            let meta = dir.metadata().map_err(io_err("inspecting a directory"))?;
            if meta.uid() != uid || meta.gid() != gid {
                chown(handle_path(dir), Some(uid), Some(gid)).map_err(io_err(
                    "giving a directory to the sandbox user (agentd must run as that user or as root)",
                ))?;
            }
        }
        set_mode(dir)
    }
}

/// The contents of a session's `settings.json`.
pub(crate) fn settings_json(cleanup_period_days: u32) -> String {
    let mut body = serde_json::to_string_pretty(&serde_json::json!({
        "cleanupPeriodDays": cleanup_period_days,
    }))
    .unwrap_or_default();
    body.push('\n');
    body
}

/// Creates the directory `path` if it's missing. An existing entry must be
/// a real directory.
fn create_dir(path: &Path, mode: Option<u32>) -> Result<()> {
    let mut builder = DirBuilder::new();
    builder.mode(mode.unwrap_or(0o755));
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
                Ok(())
            } else {
                Err(SandboxError::Io {
                    what: "creating a directory where something else exists",
                    source: err,
                })
            }
        }
        Err(err) => Err(io_err("creating a directory")(err)),
    }
}

/// Opens the directory `path` as a handle, failing on anything but a real
/// directory.
fn open_dir(path: &Path) -> Result<File> {
    rustix::fs::open(path, DIR_HANDLE, Mode::empty())
        .map(File::from)
        .map_err(errno("opening a directory"))
}

/// [`open_dir`] for `name` in `parent`.
fn open_dir_at(parent: &File, name: &str) -> Result<File> {
    rustix::fs::openat(parent, name, DIR_HANDLE, Mode::empty())
        .map(File::from)
        .map_err(errno("opening a session directory"))
}

/// A handle on a directory that follows no symlink and needs no
/// permission on the directory itself.
const DIR_HANDLE: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn mkdir_at(parent: &File, name: &str) -> Result<()> {
    rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(DIR_MODE))
        .map_err(errno("creating a session directory"))
}

/// The path of the directory a handle is open on, whatever its own path
/// names now: `/proc/self/fd/<fd>`, a link the kernel resolves to the
/// handle itself.
fn handle_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

/// Sets the mode of the directory `dir` to `0755` unless it is already.
fn set_mode(dir: &File) -> Result<()> {
    let meta = dir.metadata().map_err(io_err("inspecting a directory"))?;
    if meta.mode() & 0o7777 != DIR_MODE {
        fs::set_permissions(handle_path(dir), Permissions::from_mode(DIR_MODE))
            .map_err(io_err("resetting a directory's mode"))?;
    }
    Ok(())
}

fn io_err(what: &'static str) -> impl Fn(io::Error) -> SandboxError {
    move |source| SandboxError::Io { what, source }
}

fn errno(what: &'static str) -> impl Fn(Errno) -> SandboxError {
    move |err| SandboxError::Io {
        what,
        source: err.into(),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| SandboxError::Io {
            what: "filesystem task",
            source: io::Error::other("the task panicked or was cancelled"),
        })?
}

#[cfg(test)]
mod tests {
    use core_types::{AgentId, ConvRef, SurfaceKind};

    use super::*;

    #[test]
    fn scope_dir_names_are_hex_digests() {
        assert_eq!(
            scope_dir_name(&ScopeKey::Private),
            "715dc8493c36579a5b116995100f635e3572fdf8703e708ef1a08d943b36774e"
        );
        let colon_and_percent: ScopeKey = "ch:rocketchat:host%3A3000:a%3Ab%25c".parse().unwrap();
        assert_eq!(
            scope_dir_name(&colon_and_percent),
            "10b398911da1be7074107fece38f38ece2f3706cdb41e3c77b12fde4308854ef"
        );
        let long = ScopeKey::Channel(ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "x".repeat(1000).as_str().into(),
            conversation: "C:1%2".into(),
        });
        let name = scope_dir_name(&long);
        assert_eq!(name.len(), 64);
        assert!(name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(name, scope_dir_name(&ScopeKey::Private));
    }

    #[test]
    fn volume_paths_are_agent_then_digest() {
        let agent = AgentId::new_v4();
        let key = VolumeKey {
            agent,
            scope: ScopeKey::Private,
        };
        assert_eq!(
            volume_rel_path(&key),
            Path::new("volumes")
                .join(agent.to_string())
                .join(scope_dir_name(&ScopeKey::Private))
        );
    }

    #[test]
    fn settings_json_sets_the_cleanup_period() {
        let body = settings_json(3650);
        assert!(body.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value, serde_json::json!({"cleanupPeriodDays": 3650}));
    }
}

#[cfg(test)]
mod fs_tests {
    use std::os::unix::fs::symlink;

    use core_types::{AgentId, SessionId};

    use super::*;
    use crate::test_util::{TempDir, awkward_channel, memory_store};

    async fn layout(dir: &TempDir, owner: Option<(u32, u32)>) -> Layout {
        Layout {
            store: memory_store().await,
            data_dir: dir.0.clone(),
            owner,
            cleanup_period_days: 42,
        }
    }

    fn read_settings(session_dir: &Path) -> serde_json::Value {
        let text = fs::read_to_string(session_dir.join("claude/settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn ensure_volume_creates_the_directories_and_the_row() {
        let dir = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: awkward_channel(),
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        assert_eq!(volume.path(), dir.0.join(volume_rel_path(&key)));
        assert!(volume.path().join("sessions").is_dir());
        assert!(volume.shared_dir().is_dir());
        assert!(!volume.path().join("memory").exists());
        let mode = fs::metadata(dir.0.join(VOLUMES_DIR)).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
        let row = layout.store.volume(&key).await.unwrap().unwrap();
        assert_eq!(Path::new(&row.path), volume_rel_path(&key));
        assert_eq!(
            layout.store.volume_by_path(&row.path).await.unwrap(),
            Some(row)
        );

        let private = VolumeKey {
            agent: key.agent,
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&private).await.unwrap();
        assert!(volume.memory_dir().unwrap().is_dir());
    }

    #[tokio::test]
    async fn a_lost_row_is_recorded_again_with_the_same_path() {
        let dir = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let first = layout.ensure_volume(&key).await.unwrap();
        fs::write(first.shared_dir().join("kept"), "x").unwrap();
        let fresh = Layout {
            store: memory_store().await,
            ..layout.clone()
        };
        let again = fresh.ensure_volume(&key).await.unwrap();
        assert_eq!(again, first);
        assert!(again.shared_dir().join("kept").is_file());
        assert!(fresh.store.volume(&key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn prepare_creates_the_session_layout_and_settings() {
        let dir = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert_eq!(session_dir, volume.session_dir(session));
        for sub in SESSION_SUBDIRS {
            assert!(session_dir.join(sub).is_dir(), "{sub}");
        }
        assert_eq!(
            read_settings(&session_dir),
            serde_json::json!({"cleanupPeriodDays": 42})
        );
        let names: Vec<_> = fs::read_dir(session_dir.join("claude"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
        fs::write(session_dir.join("work/file"), "x").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert!(session_dir.join("work/file").is_file());
    }

    #[tokio::test]
    async fn symlinks_the_agent_left_are_replaced_not_followed() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: awkward_channel(),
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();

        fs::remove_dir_all(session_dir.join("claude")).unwrap();
        symlink(&outside.0, session_dir.join("claude")).unwrap();
        fs::remove_dir_all(session_dir.join("work")).unwrap();
        fs::write(session_dir.join("work"), "not a dir").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert!(
            fs::symlink_metadata(session_dir.join("claude"))
                .unwrap()
                .is_dir()
        );
        assert!(session_dir.join("work").is_dir());
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);

        let target = outside.0.join("victim");
        fs::write(&target, "keep").unwrap();
        let settings = session_dir.join("claude/settings.json");
        fs::remove_file(&settings).unwrap();
        symlink(&target, &settings).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(fs::symlink_metadata(&settings).unwrap().is_file());

        fs::remove_file(&settings).unwrap();
        fs::create_dir_all(settings.join("nested")).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert_eq!(read_settings(&session_dir)["cleanupPeriodDays"], 42);
        let names: Vec<_> = fs::read_dir(session_dir.join("claude"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
    }

    #[tokio::test]
    async fn settings_are_written_through_the_claude_handle_not_its_path() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session_dir = layout
            .prepare_session_dirs(&volume, SessionId::new_v4(), SkillsEntry::Untouched)
            .await
            .unwrap();
        let claude = session_dir.join("claude");
        let handle = open_dir(&claude).unwrap();
        fs::remove_file(claude.join("settings.json")).unwrap();
        fs::create_dir_all(claude.join("settings.json/nested")).unwrap();
        fs::rename(&claude, session_dir.join("claude-old")).unwrap();
        symlink(&outside.0, &claude).unwrap();
        layout.write_settings(&handle).unwrap();
        layout
            .place_skills(&handle, &SkillsEntry::MountPoint)
            .unwrap();
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
        let old = session_dir.join("claude-old");
        let mut names: Vec<_> = fs::read_dir(&old)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["settings.json", "skills"]);
        assert!(
            fs::symlink_metadata(old.join("settings.json"))
                .unwrap()
                .is_file()
        );
        assert!(fs::symlink_metadata(old.join("skills")).unwrap().is_dir());
        assert!(open_dir(&claude).is_err());
    }

    #[tokio::test]
    async fn skills_mount_points_and_links_replace_what_the_agent_left() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let skills = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        let entry = session_dir.join("claude/skills");
        symlink(&outside.0, &entry).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint)
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&entry).unwrap().is_dir());
        fs::write(entry.join("left"), "x").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched)
            .await
            .unwrap();
        assert!(entry.join("left").is_file());
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Link(Some(skills.0.clone())))
            .await
            .unwrap();
        assert_eq!(fs::read_link(&entry).unwrap(), skills.0);
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Link(None))
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&entry).is_err());
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
        assert!(skills.0.is_dir());
    }

    #[tokio::test]
    async fn modes_the_agent_set_are_reset() {
        let dir = TempDir::new();
        let me = fs::metadata(&dir.0).unwrap();
        let owner = if me.uid() == 0 {
            (54321, 54322)
        } else {
            (me.uid(), me.gid())
        };
        let layout = layout(&dir, Some(owner)).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint)
            .await
            .unwrap();
        fs::write(session_dir.join("work/kept"), "x").unwrap();
        let chmod = |path: &Path, mode: u32| {
            fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
        };
        chmod(&session_dir.join("claude/skills"), 0);
        chmod(&session_dir.join("claude"), 0);
        chmod(&session_dir.join("work"), 0o555);
        chmod(&session_dir.join("home"), 0o2700);
        chmod(&session_dir.join("tmp"), 0);
        chmod(&volume.shared_dir(), 0);
        chmod(&volume.memory_dir().unwrap(), 0o500);
        chmod(&session_dir, 0);
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint)
            .await
            .unwrap();
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o7777;
        for path in [
            session_dir.clone(),
            session_dir.join("claude"),
            session_dir.join("claude/skills"),
            session_dir.join("work"),
            session_dir.join("home"),
            session_dir.join("tmp"),
            volume.shared_dir(),
            volume.memory_dir().unwrap(),
        ] {
            assert_eq!(mode(&path), 0o755, "{}", path.display());
        }
        assert!(session_dir.join("work/kept").is_file());
        assert_eq!(read_settings(&session_dir)["cleanupPeriodDays"], 42);
    }

    #[tokio::test]
    async fn a_file_where_a_volume_directory_belongs_is_an_error() {
        let dir = TempDir::new();
        let layout = layout(&dir, None).await;
        fs::write(dir.0.join(VOLUMES_DIR), "x").unwrap();
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let err = layout.ensure_volume(&key).await.unwrap_err();
        assert!(matches!(err, SandboxError::Io { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn agent_writable_directories_are_given_to_the_sandbox_user() {
        let dir = TempDir::new();
        let me = fs::metadata(&dir.0).unwrap();
        let owner = if me.uid() == 0 {
            (54321, 54322)
        } else {
            (me.uid(), me.gid())
        };
        let layout = layout(&dir, Some(owner)).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session_dir = layout
            .prepare_session_dirs(&volume, SessionId::new_v4(), SkillsEntry::Untouched)
            .await
            .unwrap();
        let owned = |path: &Path| {
            let meta = fs::symlink_metadata(path).unwrap();
            (meta.uid(), meta.gid())
        };
        for path in [
            volume.shared_dir(),
            volume.memory_dir().unwrap(),
            session_dir.join("work"),
            session_dir.join("claude/settings.json"),
        ] {
            assert_eq!(owned(&path), owner, "{}", path.display());
        }
        assert_eq!(owned(volume.path()), (me.uid(), me.gid()));
        assert_eq!(owned(&session_dir), (me.uid(), me.gid()));
    }

    #[tokio::test]
    async fn giving_directories_away_without_permission_fails() {
        let dir = TempDir::new();
        if fs::metadata(&dir.0).unwrap().uid() == 0 {
            return;
        }
        let layout = layout(&dir, Some((1, 1))).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let err = layout.ensure_volume(&key).await.unwrap_err();
        assert!(matches!(err, SandboxError::Io { .. }), "{err:?}");
    }
}
