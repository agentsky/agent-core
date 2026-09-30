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
//! agentd runs as the sandbox user, so the agent can rename, replace or
//! remove anything inside its session directory, and inside `shared/` and
//! `memory/`. Host-side code here therefore never follows a symlink there:
//! an entry that should be a directory but is a symlink or a file is
//! replaced, and `settings.json` is written to a new file and renamed into
//! place. The directories above those mounts (`volumes/`, the volume and
//! its `sessions/`) are reachable from no sandbox, so they are only
//! created, never replaced.
//!
//! The repair runs only in [`Sandbox::start`](crate::Sandbox::start),
//! before the session's container exists, so nothing in the sandbox races
//! it. Writing `settings.json` doesn't rely on that: it goes through a
//! handle on `claude/` opened without following a symlink, so a `claude`
//! swapped for a symlink after its repair makes the write fail instead of
//! landing elsewhere.

use std::fs::{self, DirBuilder, File};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, fchown, lchown};
use std::path::{Path, PathBuf};

use core_types::{ScopeKey, SessionId, VolumeKey};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};
use store::Store;

use crate::{Result, SandboxError, VolumeRef};

/// The directory under the data directory that holds every volume.
pub const VOLUMES_DIR: &str = "volumes";

/// The four directories each session gets inside `sessions/<id>/`.
pub const SESSION_SUBDIRS: [&str; 4] = ["work", "claude", "home", "tmp"];

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
    ) -> Result<PathBuf> {
        let this = self.clone();
        let volume = volume.clone();
        blocking(move || {
            this.create_volume_dirs(&volume)?;
            let dir = volume.session_dir(session);
            create_dir(&dir, None)?;
            for sub in SESSION_SUBDIRS {
                this.repair_dir(&dir.join(sub))?;
            }
            this.write_settings(&dir.join("claude"))?;
            Ok(dir)
        })
        .await
    }

    /// Makes `path`, inside an agent-writable tree, a real directory owned
    /// by the sandbox user.
    pub(crate) fn repair_dir(&self, path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                fs::remove_file(path).map_err(io_err("replacing a non-directory"))?;
                create_dir(path, None)?;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => create_dir(path, None)?,
            Err(err) => return Err(io_err("inspecting a session directory")(err)),
        }
        self.own(path)
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
        self.own(&shared)?;
        if let Some(memory) = volume.memory_dir() {
            create_dir(&memory, None)?;
            self.own(&memory)?;
        }
        Ok(())
    }

    /// Writes `settings.json` into `claude_dir` through a new file renamed
    /// over the old one, so a symlink the agent left there is replaced, not
    /// followed. Every step is relative to a handle on `claude_dir` opened
    /// without following a symlink and checked to be owned as expected, so
    /// nothing is written outside it.
    pub(crate) fn write_settings(&self, claude_dir: &Path) -> Result<()> {
        const TARGET: &str = "settings.json";
        let dir = rustix::fs::open(
            claude_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|err| io_err("opening the claude directory")(err.into()))?;
        let meta = dir
            .metadata()
            .map_err(io_err("inspecting the claude directory"))?;
        if self
            .owner
            .is_some_and(|owner| owner != (meta.uid(), meta.gid()))
        {
            return Err(SandboxError::Io {
                what: "checking the claude directory",
                source: io::Error::other("it changed owner while it was prepared"),
            });
        }
        let is_dir = rustix::fs::statat(&dir, TARGET, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory);
        if is_dir {
            let aside = format!(".settings.json.{}.old", uuid::Uuid::new_v4());
            rustix::fs::renameat(&dir, TARGET, &dir, &aside)
                .map_err(|err| io_err("replacing settings.json")(err.into()))?;
            let _ = fs::remove_dir_all(claude_dir.join(&aside));
        }
        let body = settings_json(self.cleanup_period_days);
        let temp = format!(".settings.json.{}", uuid::Uuid::new_v4());
        let file = rustix::fs::openat(
            &dir,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(|err| io_err("writing settings.json")(err.into()))?;
        let written = (|| {
            (&file).write_all(body.as_bytes())?;
            if let Some((uid, gid)) = self.owner {
                fchown(&file, Some(uid), Some(gid))?;
            }
            rustix::fs::renameat(&dir, &temp, &dir, TARGET).map_err(io::Error::from)
        })();
        if written.is_err() {
            let _ = rustix::fs::unlinkat(&dir, &temp, AtFlags::empty());
        }
        written.map_err(io_err("writing settings.json"))
    }

    fn own(&self, path: &Path) -> Result<()> {
        let Some((uid, gid)) = self.owner else {
            return Ok(());
        };
        let meta = fs::symlink_metadata(path).map_err(io_err("inspecting a directory"))?;
        if meta.uid() != uid || meta.gid() != gid {
            lchown(path, Some(uid), Some(gid)).map_err(io_err(
                "giving a directory to the sandbox user (agentd must run as that user or as root)",
            ))?;
        }
        Ok(())
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

fn io_err(what: &'static str) -> impl Fn(io::Error) -> SandboxError {
    move |source| SandboxError::Io { what, source }
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
        let session_dir = layout.prepare_session_dirs(&volume, session).await.unwrap();
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
        layout.prepare_session_dirs(&volume, session).await.unwrap();
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
        let session_dir = layout.prepare_session_dirs(&volume, session).await.unwrap();

        fs::remove_dir_all(session_dir.join("claude")).unwrap();
        symlink(&outside.0, session_dir.join("claude")).unwrap();
        fs::remove_dir_all(session_dir.join("work")).unwrap();
        fs::write(session_dir.join("work"), "not a dir").unwrap();
        layout.prepare_session_dirs(&volume, session).await.unwrap();
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
        layout.prepare_session_dirs(&volume, session).await.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(fs::symlink_metadata(&settings).unwrap().is_file());

        fs::remove_file(&settings).unwrap();
        fs::create_dir_all(settings.join("nested")).unwrap();
        layout.prepare_session_dirs(&volume, session).await.unwrap();
        assert_eq!(read_settings(&session_dir)["cleanupPeriodDays"], 42);
        let names: Vec<_> = fs::read_dir(session_dir.join("claude"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
    }

    #[tokio::test]
    async fn settings_are_never_written_through_a_swapped_claude_directory() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session_dir = layout
            .prepare_session_dirs(&volume, SessionId::new_v4())
            .await
            .unwrap();
        let claude = session_dir.join("claude");
        fs::rename(&claude, session_dir.join("claude-old")).unwrap();
        symlink(&outside.0, &claude).unwrap();
        let err = layout.write_settings(&claude).unwrap_err();
        assert!(matches!(err, SandboxError::Io { .. }), "{err:?}");
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
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
            .prepare_session_dirs(&volume, SessionId::new_v4())
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
