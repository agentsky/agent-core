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
//! be repaired. Linux has no `fchmod` for an `O_PATH` handle, so modes
//! and owners are set through `/proc/self/fd/<handle>`, which names the
//! handle's directory and not whatever its path names now, and a directory
//! is listed by opening `.` relative to its handle.
//!
//! A directory tree the agent left where something else belongs is
//! removed by `remove_tree_at`, not `std::fs::remove_dir_all`: agentd
//! runs as the sandbox user, without root's override of file modes, so a
//! directory the agent made `0` or `0555` (as Go's module cache is) must
//! get its owner's `rwx` back before it can be listed or emptied; a tree
//! tens of thousands of levels deep must not take a stack frame and a file
//! descriptor per level; and a directory of millions of entries must not
//! be read into memory at once. [`remove_tree`] is the same remover for
//! agentd's own trees that hold what an agent wrote.
//!
//! A start whose spec sets [`clear_files`](crate::SessionSpec::clear_files)
//! also clears what earlier turns left, before `settings.json` and
//! `claude/skills` are put back: every entry of the session directory but
//! its four subdirectories, everything in `home/` and `tmp/`, and
//! everything in `claude/` but the session's transcript,
//! `projects/<session id>/<session id>.jsonl`, and the
//! `projects/<session id>/<session id>/` directory of its subagents'
//! transcripts and tool results, which are kept whole. The same handles
//! are used: each directory is listed and each entry looked up relative to
//! its parent's handle, every entry that goes is removed with
//! `remove_tree_at`, and a transcript entry that is a symlink or of the
//! wrong type is removed, not kept. `work/` is not cleared.
//!
//! The repair runs only in [`Sandbox::start`](crate::Sandbox::start),
//! before this start's container exists. Another container of the same
//! session can still be running, as during a blue-green overlap of two
//! agentd instances, so nothing here relies on the tree holding still: the
//! handles keep every step inside the session directory, `settings.json`
//! is replaced by a rename, so that container never finds it missing, and
//! the remover gives up after a time rather than chase entries a writer
//! keeps adding.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, chown, fchown};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// The directory in `claude/` that holds the transcripts.
const PROJECTS_DIR: &str = "projects";

/// The session subdirectories that clearing empties.
const CLEARED_SUBDIRS: [&str; 2] = ["home", "tmp"];

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
        clear_files: bool,
    ) -> Result<PathBuf> {
        let this = self.clone();
        let volume = volume.clone();
        blocking(move || {
            this.create_volume_dirs(&volume)?;
            let dir = volume.session_dir(session);
            create_dir(&dir, None)?;
            let session_dir = open_dir(&dir)?;
            set_mode(&session_dir)?;
            let deadline = Deadline::new();
            if clear_files {
                clear_dir(
                    &session_dir,
                    |name, kind| {
                        kind == FileType::Directory
                            && SESSION_SUBDIRS.iter().any(|sub| name == *sub)
                    },
                    &deadline,
                )?;
            }
            for sub in SESSION_SUBDIRS.into_iter().filter(|sub| *sub != CLAUDE_DIR) {
                let handle = this.repair_dir_at(&session_dir, sub)?;
                if clear_files && CLEARED_SUBDIRS.contains(&sub) {
                    clear_dir(&handle, |_, _| false, &deadline)?;
                }
            }
            let claude = this.repair_dir_at(&session_dir, CLAUDE_DIR)?;
            if clear_files {
                this.clear_claude_dir(&claude, session, &deadline)?;
            }
            this.write_settings(&claude, &deadline)?;
            this.place_skills(&claude, &skills, &deadline)?;
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

    /// Writes `settings.json` into the `claude/` directory `dir`: a new
    /// file is renamed into place, which replaces a file or a symlink there
    /// at once without following it, and a directory the agent left there
    /// is removed first, by `deadline`. Every step is relative to `dir`, so
    /// nothing is written outside it.
    fn write_settings(&self, dir: &File, deadline: &Deadline) -> Result<()> {
        const TARGET: &str = "settings.json";
        if kind_at(dir, TARGET)? == Some(FileType::Directory) {
            remove_tree_at(dir, OsStr::new(TARGET), deadline)?;
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
    /// relative to `dir`, removing what is there by `deadline`.
    fn place_skills(&self, dir: &File, entry: &SkillsEntry, deadline: &Deadline) -> Result<()> {
        let target = match entry {
            SkillsEntry::Untouched => return Ok(()),
            SkillsEntry::MountPoint => return self.repair_dir_at(dir, SKILLS_DIR).map(drop),
            SkillsEntry::Link(target) => target.as_deref(),
        };
        let what = "linking the skills directory";
        match kind_at(dir, SKILLS_DIR)? {
            Some(_) if target.is_some() => {
                remove_tree_at(dir, OsStr::new(SKILLS_DIR), deadline)?;
            }
            Some(FileType::Symlink) => {
                rustix::fs::unlinkat(dir, SKILLS_DIR, AtFlags::empty()).map_err(errno(what))?;
            }
            _ => {}
        }
        if let Some(target) = target {
            rustix::fs::symlinkat(target, dir, SKILLS_DIR).map_err(errno(what))?;
        }
        Ok(())
    }

    /// Clears the `claude/` directory `claude` down to `session`'s
    /// transcript: `projects/<session>/<session>.jsonl`, a regular file,
    /// and the `projects/<session>/<session>/` directory, both kept whole.
    /// Everything else goes, `settings.json` and `skills` included, which
    /// are put back after. `projects/` and `projects/<session>/` are
    /// repaired like the session's own directories, and created if
    /// missing. It fails once `deadline` passes.
    fn clear_claude_dir(
        &self,
        claude: &File,
        session: SessionId,
        deadline: &Deadline,
    ) -> Result<()> {
        let id = session.to_string();
        let transcript = format!("{id}.jsonl");
        clear_dir(
            claude,
            |name, kind| name == PROJECTS_DIR && kind == FileType::Directory,
            deadline,
        )?;
        let projects = self.repair_dir_at(claude, PROJECTS_DIR)?;
        clear_dir(
            &projects,
            |name, kind| name == id.as_str() && kind == FileType::Directory,
            deadline,
        )?;
        let project = self.repair_dir_at(&projects, &id)?;
        clear_dir(
            &project,
            |name, kind| match kind {
                FileType::RegularFile => name == transcript.as_str(),
                FileType::Directory => name == id.as_str(),
                _ => false,
            },
            deadline,
        )
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

/// Removes every entry of the directory `dir` that `keep` doesn't keep,
/// given its name and type, with [`remove_tree_at`]. It reads `dir` again
/// until a read finds nothing to remove, so an entry added meanwhile goes
/// too, and fails once `deadline` passes.
fn clear_dir(
    dir: &File,
    keep: impl Fn(&OsStr, FileType) -> bool,
    deadline: &Deadline,
) -> Result<()> {
    loop {
        let mut removed = false;
        let mut entries = read_dir(dir, None, deadline)?;
        while let Some(entry) = next_entry(entries.as_mut(), deadline)? {
            let name = OsStr::from_bytes(entry.name.to_bytes());
            match entry_kind(dir, &entry)? {
                Some(kind) if !keep(name, kind) => {
                    remove_tree_at(dir, name, deadline)?;
                    removed = true;
                }
                _ => {}
            }
        }
        if !removed {
            return Ok(());
        }
    }
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
fn open_dir_at<P: rustix::path::Arg>(parent: &File, name: P) -> Result<File> {
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

/// The type of `name` in `parent`, not following a symlink, or `None` if
/// there is no such entry.
fn kind_at<P: rustix::path::Arg>(parent: &File, name: P) -> Result<Option<FileType>> {
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
        Err(Errno::NOENT) => Ok(None),
        Err(err) => Err(errno("inspecting a session directory")(err)),
    }
}

/// How long one removal, or the removals that make one clear, may take.
const REMOVAL_TIME: Duration = Duration::from_secs(60);

/// When a removal gives up, so a writer adding entries as fast as they go
/// can't keep it, and the session start or task waiting for it, busy for
/// ever. One deadline can be shared by several removals that together make
/// one clear.
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn new() -> Self {
        Self(Instant::now() + REMOVAL_TIME)
    }

    fn check(&self) -> Result<()> {
        if Instant::now() < self.0 {
            return Ok(());
        }
        Err(SandboxError::Io {
            what: "removing a directory tree",
            source: io::Error::other("the removal ran out of time"),
        })
    }
}

/// Removes the file or directory tree at `path`, without following a
/// symlink in its last component or anywhere inside the tree, as the
/// session directories' repair does; a missing `path` is not an error, nor
/// is an entry that another removal of the same tree took first. The
/// directories above `path` are followed as any path is.
///
/// It is for agentd's own directories that hold what an agent wrote, such
/// as a private task's session directory: the tree may be read-only,
/// unreadable, very deep or very wide, and `std::fs::remove_dir_all` fails
/// or aborts on some of those. It gives up after a minute; what it
/// removed by then stays removed.
///
/// # Errors
///
/// [`SandboxError::InvalidSpec`] for a path with no parent or file name,
/// and [`SandboxError::Io`] when the tree couldn't be removed, as when
/// agentd doesn't own a directory in it or something kept writing to it
/// until the time ran out.
pub fn remove_tree(path: &Path) -> Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(SandboxError::InvalidSpec(
            "a path to remove needs a parent and a name",
        ));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let parent = match rustix::fs::open(parent, flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(()),
        Err(err) => return Err(errno("opening a directory")(err)),
    };
    remove_tree_at(&parent, name, &Deadline::new())
}

/// Removes `name` in `parent`: a directory with everything in it, or any
/// other entry, a symlink included, by unlinking it. No symlink is
/// followed: every entry is looked up with `AT_SYMLINK_NOFOLLOW` and
/// opened with `O_NOFOLLOW` relative to its parent's handle. An entry
/// found missing, because another removal of the same tree took it, is
/// skipped.
///
/// Each directory gets mode `0700` through its handle before it is read,
/// and before it is moved if a move fails with `EACCES`, as it does for a
/// directory without write permission, so a tree the agent made unreadable
/// or read-only goes as long as agentd owns it. It works depth first without recursion, with a
/// handful of files open and one directory read at a time, as a stream:
/// files are unlinked and empty directories removed as they are read, and
/// the first directory found with something in it is moved up into the
/// top directory and emptied next, and its parent's listing goes on from
/// where it stopped once it is gone ([`Levels`]). So the top directory
/// holds the directories still being emptied, at most one per level, a
/// parent whose last entry was the moved directory being removed at once.
///
/// A directory read to the end is removed, and read again from the start
/// if that finds it not empty: something added an entry, or the listing
/// went on from a position that, on a filesystem whose positions count
/// entries, skipped entries once earlier ones were removed. It fails once
/// `deadline` passes, and at once when the top directory is found not
/// empty but `name` no longer names it: another removal took it and
/// something made a new directory there, and the old one, read through its
/// handle, only ever reads empty.
fn remove_tree_at(parent: &File, name: &OsStr, deadline: &Deadline) -> Result<()> {
    let what = "removing a directory tree";
    match kind_at(parent, name)? {
        None => return Ok(()),
        Some(FileType::Directory) => {}
        Some(_) => return skip_missing(rustix::fs::unlinkat(parent, name, AtFlags::empty()), what),
    }
    let Some(top) = open_for_removal(parent, name)? else {
        return Ok(());
    };
    let mut levels = Levels::new();
    let mut resume = None;
    loop {
        let depth = levels.depth;
        let current = if depth == 0 {
            top.try_clone().map_err(io_err(what))?
        } else {
            match open_for_removal(&top, levels.name(depth))? {
                Some(dir) => dir,
                None => {
                    resume = levels.ascend();
                    continue;
                }
            }
        };
        let mut descended = false;
        let mut entries = read_dir(&current, resume.take(), deadline)?;
        while let Some(entry) = next_entry(entries.as_mut(), deadline)? {
            let Some(kind) = entry_kind(&current, &entry)? else {
                continue;
            };
            if kind != FileType::Directory {
                skip_missing(
                    rustix::fs::unlinkat(&current, &entry.name, AtFlags::empty()),
                    what,
                )?;
                continue;
            }
            match rustix::fs::unlinkat(&current, &entry.name, AtFlags::REMOVEDIR) {
                Ok(()) | Err(Errno::NOENT) => continue,
                Err(Errno::NOTEMPTY | Errno::EXIST) => {}
                Err(err) => return Err(errno(what)(err)),
            }
            if !move_up(&current, &entry.name, &top, &levels.name(depth + 1))? {
                continue;
            }
            levels.descend(entry.cookie);
            if depth > 0
                && next_entry(entries.as_mut(), deadline)?.is_none()
                && matches!(
                    rustix::fs::unlinkat(&top, levels.name(depth), AtFlags::REMOVEDIR),
                    Ok(()) | Err(Errno::NOENT)
                )
            {
                levels.ascend();
                match rustix::fs::renameat(&top, levels.name(depth + 1), &top, levels.name(depth)) {
                    Ok(()) => {}
                    Err(Errno::NOENT) => resume = levels.ascend(),
                    Err(err) => return Err(errno(what)(err)),
                }
            }
            descended = true;
            break;
        }
        if descended {
            continue;
        }
        let removed = if depth == 0 {
            rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
        } else {
            rustix::fs::unlinkat(&top, levels.name(depth), AtFlags::REMOVEDIR)
        };
        match removed {
            Ok(()) | Err(Errno::NOENT) if depth == 0 => return Ok(()),
            Ok(()) | Err(Errno::NOENT) => resume = levels.ascend(),
            Err(Errno::NOTEMPTY | Errno::EXIST) if depth == 0 && replaced(parent, name, &top)? => {
                return Err(SandboxError::Io {
                    what,
                    source: io::Error::other("another directory took its place meanwhile"),
                });
            }
            Err(Errno::NOTEMPTY | Errno::EXIST) => {}
            Err(err) => return Err(errno(what)(err)),
        }
    }
}

/// How many levels of [`Levels`] keep where their directory's listing
/// stopped.
const KEPT_POSITIONS: usize = 1024;

/// The directories one removal has moved up into its top directory and is
/// still emptying: a count, since level `n` is named
/// `.removing-<the removal's id>-<n>`, and where the listing of each of
/// the deepest [`KEPT_POSITIONS`] stopped, so going back up to one reads on
/// from there rather than from the start. A level above those is read from
/// the start again, which only a tree that deep makes happen, so the
/// listing that repeats is paid for by the work done below it.
struct Levels {
    id: uuid::fmt::Simple,
    depth: usize,
    positions: VecDeque<i64>,
}

impl Levels {
    fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple(),
            depth: 0,
            positions: VecDeque::new(),
        }
    }

    fn name(&self, depth: usize) -> String {
        format!(".removing-{}-{depth}", self.id)
    }

    /// Goes down a level from one whose listing stopped at `position`.
    fn descend(&mut self, position: i64) {
        if self.positions.len() == KEPT_POSITIONS {
            self.positions.pop_front();
        }
        self.positions.push_back(position);
        self.depth += 1;
    }

    /// Goes back up a level, returning where its listing stopped if that
    /// was kept.
    fn ascend(&mut self) -> Option<i64> {
        self.depth -= 1;
        self.positions.pop_back()
    }
}

/// Moves the directory `name` in `parent` to `to` in `top`, giving it mode
/// `0700` first if a read-only directory can't be moved, and says whether
/// it was there to move. The moved directory is opened by its new name
/// afterwards, never through a handle taken before the move, since the
/// name may have come to hold another directory meanwhile.
fn move_up(parent: &File, name: &CStr, top: &File, to: &str) -> Result<bool> {
    let what = "moving a directory to remove";
    match rustix::fs::renameat(parent, name, top, to) {
        Ok(()) => return Ok(true),
        Err(Errno::NOENT) => return Ok(false),
        Err(Errno::ACCESS) => {}
        Err(err) => return Err(errno(what)(err)),
    }
    if open_for_removal(parent, name)?.is_none() {
        return Ok(false);
    }
    match rustix::fs::renameat(parent, name, top, to) {
        Ok(()) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(err) => Err(errno(what)(err)),
    }
}

/// Whether `name` in `parent` is now another entry than the directory
/// `dir` is open on; not if it is missing.
fn replaced(parent: &File, name: &OsStr, dir: &File) -> Result<bool> {
    let what = "removing a directory tree";
    let open = rustix::fs::fstat(dir).map_err(errno(what))?;
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(now) => Ok(now.st_dev != open.st_dev || now.st_ino != open.st_ino),
        Err(Errno::NOENT) => Ok(false),
        Err(err) => Err(errno(what)(err)),
    }
}

/// `result`, with a missing entry, which another removal took, counted as
/// done.
fn skip_missing(result: rustix::io::Result<()>, what: &'static str) -> Result<()> {
    match result {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(errno(what)(err)),
    }
}

/// Opens the directory `name` in `parent` as a handle and gives its owner
/// `rwx` on it, mode `0700`, so it can be listed, emptied and moved, or
/// `None` if it is missing.
fn open_for_removal<P: rustix::path::Arg>(parent: &File, name: P) -> Result<Option<File>> {
    let dir = match rustix::fs::openat(parent, name, DIR_HANDLE, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(errno("opening a directory to remove")(err)),
    };
    fs::set_permissions(handle_path(&dir), Permissions::from_mode(0o700))
        .map_err(io_err("making a directory removable"))?;
    Ok(Some(dir))
}

/// A stream of the names in the directory `dir`, read through its handle a
/// few at a time from `position`, or from the start if there is none or
/// the filesystem refuses it, or `None` if the directory was removed
/// meanwhile. It fails once `deadline` passes.
fn read_dir(
    dir: &File,
    position: Option<i64>,
    deadline: &Deadline,
) -> Result<Option<rustix::fs::Dir>> {
    let what = "listing a directory";
    deadline.check()?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let open = || match rustix::fs::openat(dir, c".", flags, Mode::empty()) {
        Ok(fd) => rustix::fs::Dir::new(fd).map(Some).map_err(errno(what)),
        Err(Errno::NOENT) => Ok(None),
        Err(err) => Err(errno(what)(err)),
    };
    let Some(mut entries) = open()? else {
        return Ok(None);
    };
    match position.map(|position| entries.seek(position)) {
        Some(Err(_)) => open(),
        _ => Ok(Some(entries)),
    }
}

/// An entry [`next_entry`] read.
struct Entry {
    name: CString,
    /// The type the listing gives, which may be [`FileType::Unknown`].
    kind: FileType,
    /// Where the listing goes on after it.
    cookie: i64,
}

/// The next entry in `entries` but `.` and `..`, or `None` at the end. It
/// fails once `deadline` passes.
fn next_entry(entries: Option<&mut rustix::fs::Dir>, deadline: &Deadline) -> Result<Option<Entry>> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    while let Some(entry) = entries.read() {
        deadline.check()?;
        let entry = entry.map_err(errno("listing a directory"))?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            return Ok(Some(Entry {
                name: name.to_owned(),
                kind: entry.file_type(),
                cookie: entry.offset(),
            }));
        }
    }
    Ok(None)
}

/// The type of `entry` in `dir`: the one its listing gave, or, when that
/// is unknown, looked up, `None` meaning the entry is gone.
fn entry_kind(dir: &File, entry: &Entry) -> Result<Option<FileType>> {
    match entry.kind {
        FileType::Unknown => kind_at(dir, &entry.name),
        kind => Ok(Some(kind)),
    }
}

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
            data_dir: dir.path().to_owned(),
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
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: awkward_channel(),
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        assert_eq!(volume.path(), dir.join(volume_rel_path(&key)));
        assert!(volume.path().join("sessions").is_dir());
        assert!(volume.shared_dir().is_dir());
        assert!(!volume.path().join("memory").exists());
        let mode = fs::metadata(dir.join(VOLUMES_DIR)).unwrap().mode();
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
        let dir = TempDir::new("sandbox-test");
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
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
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
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert!(session_dir.join("work/file").is_file());
    }

    #[tokio::test]
    async fn symlinks_the_agent_left_are_replaced_not_followed() {
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: awkward_channel(),
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();

        fs::remove_dir_all(session_dir.join("claude")).unwrap();
        symlink(outside.path(), session_dir.join("claude")).unwrap();
        fs::remove_dir_all(session_dir.join("work")).unwrap();
        fs::write(session_dir.join("work"), "not a dir").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert!(
            fs::symlink_metadata(session_dir.join("claude"))
                .unwrap()
                .is_dir()
        );
        assert!(session_dir.join("work").is_dir());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);

        let target = outside.join("victim");
        fs::write(&target, "keep").unwrap();
        let settings = session_dir.join("claude/settings.json");
        fs::remove_file(&settings).unwrap();
        symlink(&target, &settings).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(fs::symlink_metadata(&settings).unwrap().is_file());

        fs::remove_file(&settings).unwrap();
        fs::create_dir_all(settings.join("nested")).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
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
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session_dir = layout
            .prepare_session_dirs(&volume, SessionId::new_v4(), SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let claude = session_dir.join("claude");
        let handle = open_dir(&claude).unwrap();
        fs::remove_file(claude.join("settings.json")).unwrap();
        fs::create_dir_all(claude.join("settings.json/nested")).unwrap();
        fs::rename(&claude, session_dir.join("claude-old")).unwrap();
        symlink(outside.path(), &claude).unwrap();
        let deadline = Deadline::new();
        layout.write_settings(&handle, &deadline).unwrap();
        layout
            .place_skills(&handle, &SkillsEntry::MountPoint, &deadline)
            .unwrap();
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
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
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        let skills = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let entry = session_dir.join("claude/skills");
        symlink(outside.path(), &entry).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint, false)
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&entry).unwrap().is_dir());
        fs::write(entry.join("left"), "x").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert!(entry.join("left").is_file());
        layout
            .prepare_session_dirs(
                &volume,
                session,
                SkillsEntry::Link(Some(skills.path().to_owned())),
                false,
            )
            .await
            .unwrap();
        assert_eq!(fs::read_link(&entry).unwrap(), skills.path());
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Link(None), false)
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&entry).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(skills.path().is_dir());
    }

    #[tokio::test]
    async fn modes_the_agent_set_are_reset() {
        let dir = TempDir::new("sandbox-test");
        let me = fs::metadata(dir.path()).unwrap();
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
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint, false)
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
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint, false)
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

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Fills the session directory with what turns leave: dotfiles in
    /// `home/` and `tmp/`, user memory, an auto-memory directory and another
    /// project in `claude/`, and the transcript with its directory.
    fn leave_files(session_dir: &Path, session: SessionId) {
        let project = session_dir.join(format!("claude/projects/{session}"));
        write(&session_dir.join("home/.bashrc"), "evil");
        write(&session_dir.join("home/.config/git/config"), "evil");
        write(&session_dir.join("tmp/script.sh"), "evil");
        write(&session_dir.join("claude/CLAUDE.md"), "evil");
        write(&session_dir.join("claude/shell-snapshots/s.sh"), "evil");
        write(&session_dir.join("claude/projects/other/x.jsonl"), "evil");
        write(&project.join("memory/MEMORY.md"), "evil");
        write(&project.join("other.jsonl"), "evil");
        write(&project.join(format!("{session}.jsonl")), "transcript");
        write(
            &project.join(format!("{session}/subagents/agent-1.jsonl")),
            "subagent",
        );
        write(&session_dir.join("work/kept"), "work");
    }

    #[tokio::test]
    async fn clearing_keeps_only_the_transcript_and_the_work() {
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint, false)
            .await
            .unwrap();
        leave_files(&session_dir, session);
        write(&session_dir.join("claude/skills/mine/SKILL.md"), "evil");
        write(&volume.shared_dir().join("kept"), "shared");
        write(&volume.memory_dir().unwrap().join("kept"), "memory");
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::MountPoint, true)
            .await
            .unwrap();
        assert!(names(&session_dir.join("home")).is_empty());
        assert!(names(&session_dir.join("tmp")).is_empty());
        assert_eq!(
            names(&session_dir.join("claude")),
            ["projects", "settings.json", "skills"]
        );
        assert!(names(&session_dir.join("claude/skills")).is_empty());
        assert_eq!(read_settings(&session_dir)["cleanupPeriodDays"], 42);
        assert_eq!(
            names(&session_dir.join("claude/projects")),
            [session.to_string()]
        );
        let project = session_dir.join(format!("claude/projects/{session}"));
        assert_eq!(
            names(&project),
            [session.to_string(), format!("{session}.jsonl")]
        );
        assert_eq!(
            fs::read_to_string(project.join(format!("{session}.jsonl"))).unwrap(),
            "transcript"
        );
        assert!(
            project
                .join(format!("{session}/subagents/agent-1.jsonl"))
                .is_file()
        );
        assert!(session_dir.join("work/kept").is_file());
        assert!(volume.shared_dir().join("kept").is_file());
        assert!(volume.memory_dir().unwrap().join("kept").is_file());
        for sub in SESSION_SUBDIRS {
            let mode = fs::metadata(session_dir.join(sub)).unwrap().mode();
            assert_eq!(mode & 0o7777, 0o755, "{sub}");
        }
    }

    #[tokio::test]
    async fn without_the_flag_nothing_is_cleared() {
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: awkward_channel(),
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        leave_files(&session_dir, session);
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert_eq!(names(&session_dir.join("home")), [".bashrc", ".config"]);
        assert_eq!(names(&session_dir.join("tmp")), ["script.sh"]);
        assert_eq!(
            names(&session_dir.join("claude")),
            ["CLAUDE.md", "projects", "settings.json", "shell-snapshots"]
        );
        let mut projects = vec!["other".to_owned(), session.to_string()];
        projects.sort();
        assert_eq!(names(&session_dir.join("claude/projects")), projects);
        assert_eq!(
            names(&session_dir.join(format!("claude/projects/{session}"))).len(),
            4
        );
    }

    #[tokio::test]
    async fn clearing_removes_stray_entries_fifos_and_a_file_named_projects() {
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        write(&session_dir.join("CLAUDE.md"), "evil");
        write(&session_dir.join("payload/run.sh"), "evil");
        let fifo = |dir: &Path, name: &str| {
            rustix::fs::mknodat(
                open_dir(dir).unwrap(),
                name,
                FileType::Fifo,
                Mode::RUSR | Mode::WUSR,
                0,
            )
            .unwrap();
        };
        fifo(&session_dir, "pipe");
        fifo(&session_dir.join("home"), "pipe");
        fifo(&session_dir.join("tmp"), "pipe");
        fs::remove_dir_all(session_dir.join("claude/projects")).ok();
        write(&session_dir.join("claude/projects"), "not a directory");
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, true)
            .await
            .unwrap();
        assert_eq!(names(&session_dir), ["claude", "home", "tmp", "work"]);
        assert!(names(&session_dir.join("home")).is_empty());
        assert!(names(&session_dir.join("tmp")).is_empty());
        assert_eq!(
            names(&session_dir.join("claude")),
            ["projects", "settings.json"]
        );
        assert_eq!(
            names(&session_dir.join("claude/projects")),
            [session.to_string()]
        );
    }

    #[tokio::test]
    async fn clearing_removes_unreadable_and_read_only_directories_without_root() {
        let dir = TempDir::new("sandbox-test");
        if fs::metadata(dir.path()).unwrap().uid() == 0 {
            return;
        }
        let outside = TempDir::new("sandbox-test");
        fs::write(outside.join("victim"), "keep").unwrap();
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        leave_files(&session_dir, session);
        for sub in ["home", "tmp", "claude"] {
            locked_tree(&session_dir.join(sub).join("go"), outside.path());
        }
        let projects = session_dir.join("claude/projects");
        locked_tree(&projects.join("other"), outside.path());
        fs::set_permissions(&projects, Permissions::from_mode(0o000)).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, true)
            .await
            .unwrap();
        assert!(names(&session_dir.join("home")).is_empty());
        assert!(names(&session_dir.join("tmp")).is_empty());
        assert_eq!(
            names(&session_dir.join("claude")),
            ["projects", "settings.json"]
        );
        assert_eq!(names(&projects), [session.to_string()]);
        let project = projects.join(session.to_string());
        assert_eq!(
            names(&project),
            [session.to_string(), format!("{session}.jsonl")]
        );
        assert_eq!(
            fs::read_to_string(project.join(format!("{session}.jsonl"))).unwrap(),
            "transcript"
        );
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
    }

    #[tokio::test]
    async fn clearing_removes_symlinks_planted_as_the_transcript_without_following_them() {
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let victim = outside.join("victim.jsonl");
        fs::write(&victim, "keep").unwrap();
        let victim_dir = outside.join("victim");
        write(&victim_dir.join("inside"), "keep");
        let project = session_dir.join(format!("claude/projects/{session}"));
        fs::create_dir_all(&project).unwrap();
        symlink(&victim, project.join(format!("{session}.jsonl"))).unwrap();
        symlink(&victim_dir, project.join(session.to_string())).unwrap();
        symlink(&victim_dir, session_dir.join("home/link")).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, true)
            .await
            .unwrap();
        assert!(names(&project).is_empty());
        assert!(names(&session_dir.join("home")).is_empty());

        fs::remove_dir_all(session_dir.join("claude/projects")).unwrap();
        symlink(outside.path(), session_dir.join("claude/projects")).unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, true)
            .await
            .unwrap();
        assert!(
            fs::symlink_metadata(session_dir.join("claude/projects"))
                .unwrap()
                .is_dir()
        );
        assert!(names(&project).is_empty());

        fs::create_dir(project.join(format!("{session}.jsonl"))).unwrap();
        fs::write(project.join(session.to_string()), "not a dir").unwrap();
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, true)
            .await
            .unwrap();
        assert!(names(&project).is_empty());

        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(
            fs::read_to_string(victim_dir.join("inside")).unwrap(),
            "keep"
        );
        assert_eq!(names(outside.path()), ["victim", "victim.jsonl"]);
    }

    #[tokio::test]
    async fn a_file_where_a_volume_directory_belongs_is_an_error() {
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        fs::write(dir.join(VOLUMES_DIR), "x").unwrap();
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let err = layout.ensure_volume(&key).await.unwrap_err();
        assert!(matches!(err, SandboxError::Io { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn agent_writable_directories_are_given_to_the_sandbox_user() {
        let dir = TempDir::new("sandbox-test");
        let me = fs::metadata(dir.path()).unwrap();
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
            .prepare_session_dirs(&volume, SessionId::new_v4(), SkillsEntry::Untouched, false)
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

    /// Builds at `root` what an agent or a build can leave: `0555`
    /// directories holding `0444` files (as Go's module cache is), a mode
    /// `0` directory, a `0500` one, and a symlink to `outside`.
    fn locked_tree(root: &Path, outside: &Path) {
        let chmod = |path: &Path, mode: u32| {
            fs::set_permissions(root.join(path), Permissions::from_mode(mode)).unwrap();
        };
        fs::create_dir_all(root.join("mod/pkg@v1/sub")).unwrap();
        fs::create_dir_all(root.join("zero/inner")).unwrap();
        fs::create_dir_all(root.join("r500/inner")).unwrap();
        for file in [
            "mod/pkg@v1/go.mod",
            "mod/pkg@v1/sub/a.go",
            "zero/inner/f",
            "r500/f",
        ] {
            fs::write(root.join(file), "x").unwrap();
            chmod(Path::new(file), 0o444);
        }
        symlink(outside, root.join("mod/link")).unwrap();
        for (path, mode) in [
            ("mod/pkg@v1/sub", 0o555),
            ("mod/pkg@v1", 0o555),
            ("mod", 0o555),
            ("zero/inner", 0),
            ("zero", 0),
            ("r500/inner", 0o500),
            ("r500", 0o500),
            ("", 0o555),
        ] {
            chmod(Path::new(path), mode);
        }
    }

    #[tokio::test]
    async fn read_only_and_unreadable_trees_are_removed_without_root() {
        let dir = TempDir::new("sandbox-test");
        if fs::metadata(dir.path()).unwrap().uid() == 0 {
            return;
        }
        let outside = TempDir::new("sandbox-test");
        fs::write(outside.join("victim"), "keep").unwrap();
        let skills = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let settings = session_dir.join("claude/settings.json");
        fs::remove_file(&settings).unwrap();
        locked_tree(&settings, outside.path());
        locked_tree(&session_dir.join("claude/skills"), outside.path());
        layout
            .prepare_session_dirs(
                &volume,
                session,
                SkillsEntry::Link(Some(skills.path().to_owned())),
                false,
            )
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&settings).unwrap().is_file());
        assert_eq!(
            names(&session_dir.join("claude")),
            ["settings.json", "skills"]
        );
        assert_eq!(read_settings(&session_dir)["cleanupPeriodDays"], 42);
        assert_eq!(
            fs::read_link(session_dir.join("claude/skills")).unwrap(),
            skills.path()
        );
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
    }

    #[tokio::test]
    async fn a_symlink_inside_a_removed_tree_is_unlinked_not_followed() {
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        fs::create_dir(outside.join("inside")).unwrap();
        fs::write(outside.join("inside/victim"), "keep").unwrap();
        let target = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let skills = session_dir.join("claude/skills");
        fs::create_dir_all(skills.join("a/b")).unwrap();
        symlink(outside.path(), skills.join("a/b/out")).unwrap();
        symlink(outside.join("inside"), skills.join("out")).unwrap();
        layout
            .prepare_session_dirs(
                &volume,
                session,
                SkillsEntry::Link(Some(target.path().to_owned())),
                false,
            )
            .await
            .unwrap();
        assert_eq!(fs::read_link(&skills).unwrap(), target.path());
        assert_eq!(
            fs::read_to_string(outside.join("inside/victim")).unwrap(),
            "keep"
        );
    }

    #[tokio::test]
    async fn a_tree_deeper_than_the_stack_allows_recursion_is_removed() {
        const DEPTH: usize = 16_000;
        let dir = TempDir::new("sandbox-test");
        let layout = layout(&dir, None).await;
        let key = VolumeKey {
            agent: AgentId::new_v4(),
            scope: ScopeKey::Private,
        };
        let volume = layout.ensure_volume(&key).await.unwrap();
        let session = SessionId::new_v4();
        let session_dir = layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        let settings = session_dir.join("claude/settings.json");
        fs::remove_file(&settings).unwrap();
        fs::create_dir(&settings).unwrap();
        let mut level = open_dir(&settings).unwrap();
        for _ in 0..DEPTH {
            mkdir_at(&level, "d").unwrap();
            level = open_dir_at(&level, "d").unwrap();
        }
        drop(level);
        layout
            .prepare_session_dirs(&volume, session, SkillsEntry::Untouched, false)
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&settings).unwrap().is_file());
        assert_eq!(names(&session_dir.join("claude")), ["settings.json"]);
    }

    /// Builds a tree at `root` of `width` files and `width` directories,
    /// half of them empty and half holding one file, `levels` levels deep,
    /// and returns how many entries it has.
    fn wide_tree(root: &Path, width: usize, levels: usize) -> u64 {
        fs::create_dir(root).unwrap();
        let mut entries = 1;
        for i in 0..width {
            fs::write(root.join(format!("f{i}")), "x").unwrap();
            let sub = root.join(format!("d{i}"));
            if levels > 1 {
                entries += wide_tree(&sub, width, levels - 1);
            } else {
                fs::create_dir(&sub).unwrap();
                entries += 1;
                if i % 2 == 0 {
                    fs::write(sub.join("f"), "x").unwrap();
                    entries += 1;
                }
            }
            entries += 1;
        }
        entries
    }

    fn remove_with(root: &Path, deadline: &Deadline) -> Result<()> {
        let parent = open_dir(root.parent().unwrap()).unwrap();
        remove_tree_at(&parent, root.file_name().unwrap(), deadline)
    }

    #[test]
    fn a_wide_tree_is_removed() {
        let dir = TempDir::new("sandbox-test");
        let tree = dir.join("tree");
        wide_tree(&tree, 2000, 1);
        wide_tree(&tree.join("d0/deeper"), 30, 2);
        remove_with(&tree, &Deadline::new()).unwrap();
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn levels_past_the_kept_positions_are_removed() {
        let dir = TempDir::new("sandbox-test");
        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        let mut level = open_dir(&tree).unwrap();
        for i in 0..3000 {
            let sibling = format!("s{i}");
            mkdir_at(&level, &sibling).unwrap();
            mkdir_at(&open_dir_at(&level, sibling.as_str()).unwrap(), "x").unwrap();
            let next = format!("d{i}");
            mkdir_at(&level, &next).unwrap();
            level = open_dir_at(&level, next.as_str()).unwrap();
        }
        drop(level);
        remove_with(&tree, &Deadline::new()).unwrap();
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn only_the_deepest_positions_are_kept() {
        let mut levels = Levels::new();
        for position in 0..KEPT_POSITIONS as i64 + 5 {
            levels.descend(position);
        }
        assert_eq!(levels.depth, KEPT_POSITIONS + 5);
        assert_eq!(levels.ascend(), Some(KEPT_POSITIONS as i64 + 4));
        let kept = (1..KEPT_POSITIONS)
            .filter(|_| levels.ascend().is_some())
            .count();
        assert_eq!(kept, KEPT_POSITIONS - 1);
        assert_eq!(levels.ascend(), None);
        assert_eq!(levels.depth, 4);
    }

    #[test]
    fn a_position_the_filesystem_refuses_reads_from_the_start() {
        let dir = TempDir::new("sandbox-test");
        fs::write(dir.join("a"), "x").unwrap();
        fs::write(dir.join("b"), "x").unwrap();
        let handle = open_dir(dir.path()).unwrap();
        let deadline = Deadline::new();
        let mut entries = read_dir(&handle, Some(-1), &deadline).unwrap();
        let mut read = Vec::new();
        while let Some(entry) = next_entry(entries.as_mut(), &deadline).unwrap() {
            read.push(entry.name.into_string().unwrap());
        }
        read.sort();
        assert_eq!(read, ["a", "b"]);
    }

    #[test]
    fn a_directory_in_the_removed_ones_place_is_told_apart() {
        let dir = TempDir::new("sandbox-test");
        let parent = open_dir(dir.path()).unwrap();
        fs::create_dir(dir.join("t")).unwrap();
        let top = open_dir(&dir.join("t")).unwrap();
        assert!(!replaced(&parent, OsStr::new("t"), &top).unwrap());
        fs::remove_dir(dir.join("t")).unwrap();
        assert!(!replaced(&parent, OsStr::new("t"), &top).unwrap());
        fs::create_dir(dir.join("t")).unwrap();
        fs::write(dir.join("t/x"), "x").unwrap();
        assert!(replaced(&parent, OsStr::new("t"), &top).unwrap());
    }

    #[test]
    fn an_unknown_type_is_looked_up() {
        let dir = TempDir::new("sandbox-test");
        fs::write(dir.join("f"), "x").unwrap();
        let handle = open_dir(dir.path()).unwrap();
        let entry = |name: &str, kind| Entry {
            name: CString::new(name).unwrap(),
            kind,
            cookie: 0,
        };
        let kind = |name, kind| entry_kind(&handle, &entry(name, kind)).unwrap();
        assert_eq!(kind("f", FileType::Unknown), Some(FileType::RegularFile));
        assert_eq!(kind("gone", FileType::Unknown), None);
        assert_eq!(kind("gone", FileType::Directory), Some(FileType::Directory));
    }

    #[test]
    fn a_writer_that_keeps_adding_entries_runs_out_the_time() {
        const ALLOWED: std::time::Duration = std::time::Duration::from_millis(300);
        let dir = TempDir::new("sandbox-test");
        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..2)
            .map(|writer| {
                let stop = stop.clone();
                let top = open_dir(&tree).unwrap();
                std::thread::spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = mkdir_at(&top, &format!("w{writer}-{i}"));
                        let _ = mkdir_at(&top, &format!("w{writer}-{i}/x"));
                        i += 1;
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let started = Instant::now();
        let removed = remove_with(&tree, &Deadline(started + ALLOWED));
        let took = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for writer in writers {
            writer.join().unwrap();
        }
        if let Err(err) = removed {
            assert!(err.to_string().contains("out of time"), "{err}");
        }
        assert!(
            took < ALLOWED + std::time::Duration::from_secs(10),
            "{took:?}"
        );
    }

    #[test]
    fn two_removals_of_one_tree_both_succeed() {
        for _ in 0..20 {
            let dir = TempDir::new("sandbox-test");
            let tree = dir.join("tree");
            wide_tree(&tree, 6, 3);
            let mut level = open_dir(&tree).unwrap();
            for _ in 0..200 {
                mkdir_at(&level, "d").unwrap();
                level = open_dir_at(&level, "d").unwrap();
            }
            drop(level);
            let removals: Vec<_> = (0..2)
                .map(|_| {
                    let tree = tree.clone();
                    std::thread::spawn(move || remove_tree(&tree))
                })
                .collect();
            for removal in removals {
                removal.join().unwrap().unwrap();
            }
            assert_eq!(names(dir.path()), Vec::<String>::new());
        }
    }

    #[test]
    fn remove_tree_removes_a_path_without_following_symlinks() {
        let dir = TempDir::new("sandbox-test");
        let outside = TempDir::new("sandbox-test");
        fs::write(outside.join("victim"), "keep").unwrap();
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/f"), "x").unwrap();
        symlink(outside.path(), tree.join("a/out")).unwrap();
        remove_tree(&tree).unwrap();
        assert!(fs::symlink_metadata(&tree).is_err());
        remove_tree(&tree).unwrap();
        remove_tree(&dir.join("missing/also")).unwrap();
        let link = dir.join("link");
        symlink(outside.path(), &link).unwrap();
        remove_tree(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        fs::write(dir.join("file"), "x").unwrap();
        remove_tree(&dir.join("file")).unwrap();
        assert_eq!(names(dir.path()), Vec::<String>::new());
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
        assert!(matches!(
            remove_tree(Path::new("/")),
            Err(SandboxError::InvalidSpec(_))
        ));
        fs::create_dir_all(dir.join("real/tree/a")).unwrap();
        symlink(dir.join("real"), dir.join("alias")).unwrap();
        remove_tree(&dir.join("alias/tree")).unwrap();
        assert_eq!(names(&dir.join("real")), Vec::<String>::new());
    }

    #[tokio::test]
    async fn giving_directories_away_without_permission_fails() {
        let dir = TempDir::new("sandbox-test");
        if fs::metadata(dir.path()).unwrap().uid() == 0 {
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
