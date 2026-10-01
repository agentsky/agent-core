//! The files a private task is handed: copied out of the calling session's
//! directory when the consent is asked for, kept under the consent's own
//! directory until its work is done, and copied into the private
//! session's `work/` before its turn.
//!
//! The calling session's directory is the agent's to write, so nothing
//! here follows a symlink in it: the session directory is opened once, and
//! each component of a path below it is opened relative to its parent's
//! handle with `O_NOFOLLOW`, directories with `O_PATH`, and the file
//! itself with `O_NONBLOCK`, so a FIFO can't hold the request up. Only a
//! regular file is copied, and at most the attachment cap of all of them
//! together, counted in the bytes actually read. A name a Claude session
//! would read as its configuration, a dotfile or `CLAUDE.md`, is refused.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, fchown};
use std::path::{Component, Path};

use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::ctl::is_plain_file_name;

/// Why a file couldn't be handed to a private task. Each names the path
/// as the request gave it.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// The path isn't relative to the session directory, or has `.` or
    /// `..` in it.
    #[error("{0} is not a path in this session's directory")]
    BadPath(String),
    /// Nothing is there.
    #[error("{0} doesn't exist")]
    NotFound(String),
    /// A directory, a symlink, or anything else but a regular file.
    #[error("{0} is not a regular file")]
    NotAFile(String),
    /// The file takes the files over the attachment cap, which they share.
    #[error("{0} takes the files over the {1}-byte limit for a private task's files together")]
    TooLarge(String, u64),
    /// The file's name couldn't be shown as an attachment's.
    #[error(
        "{0} has a file name that isn't plain: at most 255 bytes of UTF-8, with no control or \
         invisible formatting characters"
    )]
    BadName(String),
    /// The file's name is one a Claude session reads as its
    /// configuration.
    #[error("{0} is a dotfile or CLAUDE.md, which a private task isn't handed; rename it")]
    ConfigName(String),
    /// Two files have the same name.
    #[error("two files are named {0}")]
    Duplicate(String),
    /// Reading or copying failed.
    #[error("copying a file failed: {0}")]
    Io(#[from] io::Error),
}

const DIR: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Copies each of `files`, a path relative to `session_dir`, into `dir`,
/// which must exist, as `0`, `1`, and so on, and returns their names.
/// Files over `cap` bytes together are refused.
///
/// # Errors
///
/// The first [`StageError`]. What was copied before it is left in `dir`.
pub(super) fn stage(
    session_dir: &Path,
    files: &[String],
    dir: &Path,
    cap: u64,
) -> Result<Vec<String>, StageError> {
    let mut names = Vec::with_capacity(files.len());
    let mut left = cap;
    for (index, file) in files.iter().enumerate() {
        let (name, source) = open_in(session_dir, file)?;
        if names.contains(&name) {
            return Err(StageError::Duplicate(name));
        }
        if source.metadata()?.len() > left {
            return Err(StageError::TooLarge(file.clone(), cap));
        }
        let mut dest = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(index.to_string()))?;
        let copied = io::copy(&mut source.take(left.saturating_add(1)), &mut dest)?;
        left = left
            .checked_sub(copied)
            .ok_or_else(|| StageError::TooLarge(file.clone(), cap))?;
        names.push(name);
    }
    Ok(names)
}

/// Opens `file`, a path relative to `session_dir`, without following a
/// symlink anywhere below `session_dir`, and returns its name with it.
fn open_in(session_dir: &Path, file: &str) -> Result<(String, File), StageError> {
    let bad_path = || StageError::BadPath(file.to_owned());
    let components = Path::new(file)
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(bad_path()),
        })
        .collect::<Result<Vec<&OsStr>, _>>()?;
    let Some((last, parents)) = components.split_last() else {
        return Err(bad_path());
    };
    let name = last
        .to_str()
        .filter(|name| is_plain_file_name(name))
        .ok_or_else(|| StageError::BadName(file.to_owned()))?
        .to_owned();
    if is_config_name(&name) {
        return Err(StageError::ConfigName(file.to_owned()));
    }
    let refused = |errno: Errno| match errno {
        Errno::NOENT | Errno::NOTDIR => StageError::NotFound(file.to_owned()),
        Errno::LOOP | Errno::ISDIR | Errno::NXIO => StageError::NotAFile(file.to_owned()),
        other => StageError::Io(other.into()),
    };
    let mut dir = rustix::fs::open(session_dir, DIR, Mode::empty()).map_err(refused)?;
    for parent in parents {
        dir = rustix::fs::openat(&dir, *parent, DIR, Mode::empty()).map_err(refused)?;
    }
    let fd = rustix::fs::openat(&dir, *last, FILE, Mode::empty()).map_err(refused)?;
    let stat = rustix::fs::fstat(&fd).map_err(refused)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(StageError::NotAFile(file.to_owned()));
    }
    Ok((name, File::from(fd)))
}

/// Whether `name` is one a Claude session reads as its configuration: a
/// dotfile, such as `.mcp.json` or `.claude`, or `CLAUDE.md` or
/// `CLAUDE.local.md` in any case.
fn is_config_name(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("CLAUDE.md")
        || name.eq_ignore_ascii_case("CLAUDE.local.md")
}

/// Copies the files [`stage`] put in `dir` into `work`, a new session's
/// working directory, under their `names`, owned by `owner`, the uid and
/// gid agents run as, if the sandbox sets one, so the task can change them.
///
/// # Errors
///
/// If a staged file can't be read, or a copy can't be made or given to
/// `owner`, as when a file of that name is already in `work`.
pub(super) fn hand_over(
    dir: &Path,
    names: &[String],
    work: &Path,
    owner: Option<(u32, u32)>,
) -> io::Result<()> {
    for (index, name) in names.iter().enumerate() {
        let mut source = File::open(dir.join(index.to_string()))?;
        let mut dest = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(work.join(name))?;
        if let Some((uid, gid)) = owner {
            let meta = dest.metadata()?;
            if (meta.uid(), meta.gid()) != (uid, gid) {
                fchown(&dest, Some(uid), Some(gid))?;
            }
        }
        io::copy(&mut source, &mut dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agentd-stage-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session() -> (TempDir, TempDir) {
        let session = TempDir::new();
        std::fs::create_dir_all(session.0.join("work/sub")).unwrap();
        std::fs::write(session.0.join("work/in.txt"), "input").unwrap();
        std::fs::write(session.0.join("work/sub/data.csv"), "a,b").unwrap();
        (session, TempDir::new())
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn staged_files_are_copied_out_and_handed_over_by_name() {
        let (session, staged) = session();
        let both = files(&["work/in.txt", "work/sub/data.csv"]);
        let err = stage(&session.0, &both, &staged.0, 7).unwrap_err();
        assert!(
            matches!(&err, StageError::TooLarge(file, 7) if file == "work/sub/data.csv"),
            "the cap counts the files together: {err}"
        );
        for entry in std::fs::read_dir(&staged.0).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let names = stage(&session.0, &both, &staged.0, 8).unwrap();
        assert_eq!(names, ["in.txt", "data.csv"]);
        std::fs::write(session.0.join("work/in.txt"), "changed later").unwrap();
        let work = TempDir::new();
        let me = std::fs::metadata(&work.0).unwrap();
        let owner = (me.uid(), me.gid());
        hand_over(&staged.0, &names, &work.0, Some(owner)).unwrap();
        let handed = std::fs::metadata(work.0.join("in.txt")).unwrap();
        assert_eq!((handed.uid(), handed.gid()), owner);
        assert_eq!(
            std::fs::read_to_string(work.0.join("in.txt")).unwrap(),
            "input"
        );
        assert_eq!(
            std::fs::read_to_string(work.0.join("data.csv")).unwrap(),
            "a,b"
        );
        assert!(hand_over(&staged.0, &names, &work.0, None).is_err());
        let unowned = TempDir::new();
        hand_over(&staged.0, &names, &unowned.0, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(unowned.0.join("data.csv")).unwrap(),
            "a,b"
        );
    }

    #[test]
    fn paths_outside_symlinks_and_odd_files_are_refused() {
        let (session, staged) = session();
        let outside = TempDir::new();
        std::fs::write(outside.0.join("secret"), "private").unwrap();
        symlink(outside.0.join("secret"), session.0.join("work/link")).unwrap();
        symlink(&outside.0, session.0.join("work/dirlink")).unwrap();
        std::fs::write(session.0.join("work/big"), "123456").unwrap();
        std::fs::write(session.0.join("work/bad\u{202E}name"), "x").unwrap();
        std::fs::write(session.0.join("work/.mcp.json"), "{}").unwrap();
        std::fs::write(session.0.join("work/claude.MD"), "x").unwrap();
        let cases = [
            ("/etc/passwd", "is not a path"),
            ("../other/work/x", "is not a path"),
            ("work/../work/in.txt", "is not a path"),
            ("./work/in.txt", "is not a path"),
            ("", "is not a path"),
            ("work/missing", "doesn't exist"),
            ("work/in.txt/x", "doesn't exist"),
            ("work/link", "is not a regular file"),
            ("work/dirlink/secret", "doesn't exist"),
            ("work/sub", "is not a regular file"),
            ("work/big", "limit for a private task's files together"),
            ("work/bad\u{202E}name", "isn't plain"),
            ("work/.mcp.json", "dotfile or CLAUDE.md"),
            ("work/claude.MD", "dotfile or CLAUDE.md"),
        ];
        for (path, reason) in cases {
            let err = stage(&session.0, &files(&[path]), &staged.0, 5).unwrap_err();
            assert!(err.to_string().contains(reason), "{path}: {err}");
            for entry in std::fs::read_dir(&staged.0).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
        let err = stage(
            &session.0,
            &files(&["work/in.txt", "work/in.txt"]),
            &staged.0,
            5,
        )
        .unwrap_err();
        assert!(matches!(err, StageError::Duplicate(name) if name == "in.txt"));
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let (session, staged) = session();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            session.0.join("work/fifo"),
            FileType::Fifo,
            Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();
        let err = stage(&session.0, &files(&["work/fifo"]), &staged.0, 5).unwrap_err();
        assert!(matches!(err, StageError::NotAFile(_)), "{err}");
    }
}
