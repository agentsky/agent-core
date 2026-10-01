//! Checking a skill's files: what an upload unpacks to, what a clone
//! leaves, and the `SKILL.md` front matter.
//!
//! Everything here reads content the owner supplied, from wherever they got
//! it, so every limit is enforced while reading rather than trusted from
//! what the content declares:
//!
//! - At most [`MAX_SKILL_BYTES`] of files in all, [`MAX_FILES`] files and
//!   directories, [`MAX_DEPTH`] levels, and paths of [`MAX_PATH_BYTES`].
//! - Names are plain: no empty, `.` or `..` component, no `\`, and no
//!   control or invisible formatting character, so nothing lands outside
//!   the skill's directory or hides what it is.
//! - Only regular files and directories. A symlink in an archive is
//!   refused rather than followed or copied; so are devices, sockets and
//!   FIFOs. A clone has neither: `git` checks symlinks out as plain files
//!   holding their targets, and a symlink found in a tree is refused all
//!   the same.
//! - Modes are rewritten: directories `0755`, files `0644`, or `0755` when
//!   an execute bit was set, so a skill may ship scripts, but never
//!   set-id, sticky or group- or world-writable files.
//! - A `.zip` holds stored or deflated entries only, none encrypted, and
//!   each entry's bytes are counted as they are inflated, so a small
//!   archive can't unpack past the limits.
//! - `SKILL.md` is at most [`MAX_SKILL_MD_BYTES`] of UTF-8, and its front
//!   matter at most [`MAX_FRONT_MATTER_BYTES`]; the YAML parser refuses
//!   alias expansion past its own limits, so a "billion laughs" document
//!   fails instead of growing.

use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use commands::SkillName;
use cred_proxy::HostRule;
use serde::Deserialize;

use crate::ctl::is_invisible;

/// The most bytes of files a skill may hold, unpacked.
pub const MAX_SKILL_BYTES: u64 = 10 * 1024 * 1024;
/// The most files and directories a skill may hold.
pub const MAX_FILES: usize = 1_000;
/// The deepest a skill's files may be, in path components.
pub const MAX_DEPTH: usize = 16;
/// The longest path of a skill's file, from the skill's top, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// The largest `SKILL.md`.
pub const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
/// The largest front matter in a `SKILL.md`.
pub const MAX_FRONT_MATTER_BYTES: usize = 16 * 1024;
/// The most hosts a skill may declare.
pub const MAX_HOSTS: usize = 16;
/// The longest description, in characters, as Claude Code allows.
pub const MAX_DESCRIPTION_CHARS: usize = 1_024;
/// The skill file's name.
pub const SKILL_FILE: &str = "SKILL.md";
/// The directory of resource forks macOS adds to the archives it makes,
/// which a zip's entries under are skipped.
const MACOS_METADATA: &str = "__MACOSX";
/// The name of the skill agentd bundles with every agent, which no owner
/// may add or remove.
pub const BUNDLED_NAME: &str = "agentctl";

/// What a `SKILL.md`'s front matter says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The skill's name, which is also its directory's.
    pub name: SkillName,
    /// What it is for, which Claude Code shows the model.
    pub description: String,
    /// The hosts it asks the agent's sandboxes to reach, from
    /// `allowed-hosts`, without repeats.
    pub hosts: Vec<HostRule>,
}

/// Why a skill's files were refused. Its message is for the owner, and
/// never repeats the content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    /// No `SKILL.md` at the top, or in the one directory at the top.
    #[error("I found no SKILL.md at the top of the skill, or in the one directory at its top.")]
    NoSkillFile,
    /// `SKILL.md` is too large or not text.
    #[error("SKILL.md must be UTF-8 text of at most {} KB.", MAX_SKILL_MD_BYTES / 1024)]
    SkillFile,
    /// No front matter between `---` lines at the start of `SKILL.md`.
    #[error(
        "SKILL.md must start with front matter: a `---` line, YAML with `name` and \
         `description`, and another `---` line, in at most {} KB.",
        MAX_FRONT_MATTER_BYTES / 1024
    )]
    NoFrontMatter,
    /// The front matter isn't YAML of the expected shape.
    #[error(
        "SKILL.md's front matter isn't valid: it must be YAML with `name` and `description` \
         as text, and `allowed-hosts`, if present, as a list of hosts."
    )]
    FrontMatter,
    /// `name` is missing or breaks the rule.
    #[error("SKILL.md's `name` must be 1 to 64 characters, each a-z, 0-9 or -.")]
    Name,
    /// `name` is the bundled skill's.
    #[error("The skill name `agentctl` is taken by the skill every agent has built in.")]
    Reserved,
    /// `description` is missing, empty or too long.
    #[error(
        "SKILL.md needs a `description` of 1 to {MAX_DESCRIPTION_CHARS} characters, saying \
         what the skill is for."
    )]
    Description,
    /// An `allowed-hosts` entry isn't a host rule agentd accepts.
    #[error("Entry {index} of `allowed-hosts` isn't a host agentd allows: {reason}.")]
    Host {
        /// Its position, from 1.
        index: usize,
        /// Why, from [`HostRule`]'s parser, which never repeats its input.
        reason: String,
    },
    /// Too many hosts.
    #[error("A skill may ask for at most {MAX_HOSTS} hosts in `allowed-hosts`.")]
    TooManyHosts,
    /// Over [`MAX_SKILL_BYTES`], [`MAX_FILES`], [`MAX_DEPTH`] or
    /// [`MAX_PATH_BYTES`].
    #[error(
        "The skill is too large: at most {} MB of files, {MAX_FILES} files and directories, \
         {MAX_DEPTH} levels deep, and paths of at most {MAX_PATH_BYTES} bytes.",
        MAX_SKILL_BYTES / (1024 * 1024)
    )]
    TooLarge,
    /// A name that isn't plain.
    #[error(
        "The skill has a file name agentd won't write: an empty, `.` or `..` part, a `\\`, \
         or a control or invisible character."
    )]
    BadName,
    /// Two entries with the same name, or a file where a directory goes.
    #[error("The archive names the same path twice.")]
    Duplicate,
    /// A symlink.
    #[error("The skill holds a symbolic link; only files and directories are allowed.")]
    Symlink,
    /// A device, socket or FIFO.
    #[error("The skill holds something that is neither a file nor a directory.")]
    SpecialFile,
    /// Not a zip agentd can read.
    #[error(
        "That isn't a .zip I can read: it must be an unencrypted zip of stored or deflated \
         files."
    )]
    Archive,
}

/// Why checking a skill's files failed: a [`Problem`] with them, or a
/// failure on agentd's side, which is logged, not shown.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// The files were refused.
    #[error(transparent)]
    Problem(#[from] Problem),
    /// Writing or reading the disk failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        source: std::io::Error,
    },
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> CheckError {
    move |source| CheckError::Io { what, source }
}

#[derive(Deserialize)]
struct FrontMatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "allowed-hosts")]
    allowed_hosts: Option<Hosts>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Hosts {
    List(Vec<String>),
    Line(String),
}

/// Reads the front matter of a `SKILL.md` whose text is `text`.
///
/// # Errors
///
/// The [`Problem`] with it.
pub fn parse_skill_file(text: &str) -> Result<Manifest, Problem> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(Problem::NoFrontMatter);
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        if yaml.len() + line.len() > MAX_FRONT_MATTER_BYTES {
            return Err(Problem::NoFrontMatter);
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err(Problem::NoFrontMatter);
    }
    if yaml.trim().is_empty() {
        return Err(Problem::Name);
    }
    let front: FrontMatter = serde_norway::from_str(&yaml).map_err(|_| Problem::FrontMatter)?;
    let name: SkillName = front
        .name
        .as_deref()
        .map(str::trim)
        .and_then(|name| name.parse().ok())
        .ok_or(Problem::Name)?;
    if name.as_str() == BUNDLED_NAME {
        return Err(Problem::Reserved);
    }
    let description = front
        .description
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty() && d.chars().count() <= MAX_DESCRIPTION_CHARS)
        .ok_or(Problem::Description)?;
    let entries = match front.allowed_hosts {
        None => Vec::new(),
        Some(Hosts::List(list)) => list,
        Some(Hosts::Line(line)) => line
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|host| !host.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    if entries.len() > MAX_HOSTS {
        return Err(Problem::TooManyHosts);
    }
    let mut hosts: Vec<HostRule> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let host = |reason: String| Problem::Host {
            index: i + 1,
            reason,
        };
        let entry = entry.trim();
        if entry.starts_with('*') {
            return Err(host(
                "a skill names each host it needs; wildcards aren't allowed".into(),
            ));
        }
        let rule: HostRule = entry
            .parse()
            .map_err(|err: cred_proxy::HostRuleError| host(err.to_string()))?;
        if !hosts.contains(&rule) {
            hosts.push(rule);
        }
    }
    Ok(Manifest {
        name,
        description,
        hosts,
    })
}

/// Whether `name` is a plain path component.
fn plain_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control() || is_invisible(c))
}

/// Running totals, checked against the limits.
#[derive(Debug, Default)]
struct Budget {
    bytes: u64,
    entries: usize,
}

impl Budget {
    /// Counts an entry `depth` components deep whose path from the top is
    /// `path_bytes` long.
    fn entry(&mut self, depth: usize, path_bytes: usize) -> Result<(), Problem> {
        self.entries += 1;
        if self.entries > MAX_FILES || depth > MAX_DEPTH || path_bytes > MAX_PATH_BYTES {
            return Err(Problem::TooLarge);
        }
        Ok(())
    }

    fn bytes(&mut self, more: u64) -> Result<(), Problem> {
        self.bytes = self.bytes.saturating_add(more);
        if self.bytes > MAX_SKILL_BYTES {
            return Err(Problem::TooLarge);
        }
        Ok(())
    }
}

/// The mode a file gets: `0755` when `mode` has an execute bit, else
/// `0644`.
fn file_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o644 } else { 0o755 }
}

/// Writes an uploaded `SKILL.md` into `dir`, a new directory.
///
/// # Errors
///
/// [`Problem::SkillFile`] if it is too large, or an I/O failure.
pub fn write_skill_file(bytes: &[u8], dir: &Path) -> Result<(), CheckError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SKILL_MD_BYTES {
        return Err(Problem::SkillFile.into());
    }
    fs::create_dir(dir).map_err(io("creating a skill's directory"))?;
    new_file(&dir.join(SKILL_FILE), 0o644)?
        .write_all(bytes)
        .map_err(io("writing SKILL.md"))
}

fn new_file(path: &Path, mode: u32) -> Result<fs::File, CheckError> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                CheckError::Problem(Problem::Duplicate)
            } else {
                io("creating a skill's file")(err)
            }
        })
}

/// Unpacks the zip archive `bytes` into `dir`, a new directory.
///
/// # Errors
///
/// The [`Problem`] with the archive, or an I/O failure.
pub fn unpack_zip(bytes: &[u8], dir: &Path) -> Result<(), CheckError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| Problem::Archive)?;
    if archive.len() > MAX_FILES {
        return Err(Problem::TooLarge.into());
    }
    fs::create_dir(dir).map_err(io("creating a skill's directory"))?;
    let mut budget = Budget::default();
    let mut seen = HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|_| Problem::Archive)?;
        if entry.encrypted() {
            return Err(Problem::Archive.into());
        }
        if entry.is_symlink() {
            return Err(Problem::Symlink.into());
        }
        let is_dir = entry.is_dir();
        let name = entry.name().to_owned();
        let parts: Vec<&str> = name
            .strip_suffix('/')
            .filter(|_| is_dir)
            .unwrap_or(&name)
            .split('/')
            .collect();
        if !parts.iter().all(|part| plain_component(part)) {
            return Err(Problem::BadName.into());
        }
        if parts[0] == MACOS_METADATA {
            continue;
        }
        let joined = parts.join("/");
        budget.entry(parts.len(), joined.len())?;
        if !seen.insert(joined) {
            return Err(Problem::Duplicate.into());
        }
        let mode = entry.unix_mode().unwrap_or(0o644);
        let kind = mode & 0o170_000;
        if !is_dir && kind != 0 && kind != 0o100_000 {
            return Err(Problem::SpecialFile.into());
        }
        let mut path = dir.to_owned();
        for (i, part) in parts.iter().enumerate() {
            path.push(part);
            let last = i + 1 == parts.len();
            if last && !is_dir {
                break;
            }
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(Problem::Duplicate.into()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&path).map_err(io("creating a skill's directory"))?;
                }
                Err(err) => return Err(io("reading a skill's directory")(err)),
            }
        }
        if is_dir {
            continue;
        }
        budget.bytes(entry.size())?;
        let mut file = new_file(&path, file_mode(mode))?;
        let limit = entry.size().saturating_add(1);
        let copied = std::io::copy(&mut (&mut entry).take(limit), &mut file).map_err(|err| {
            if err.kind() == std::io::ErrorKind::InvalidData {
                CheckError::Problem(Problem::Archive)
            } else {
                io("unpacking a file")(err)
            }
        })?;
        if copied != entry.size() {
            return Err(Problem::Archive.into());
        }
    }
    Ok(())
}

/// Checks the tree at `root`, a directory agentd just wrote, against the
/// limits, and rewrites its modes. A `.git` directory at the top, which a
/// clone leaves, is removed first.
///
/// # Errors
///
/// The [`Problem`] with the tree, or an I/O failure.
pub fn check_tree(root: &Path) -> Result<(), CheckError> {
    let git = root.join(".git");
    if fs::symlink_metadata(&git).is_ok_and(|meta| meta.is_dir()) {
        fs::remove_dir_all(&git).map_err(io("removing a clone's .git"))?;
    }
    let mut budget = Budget::default();
    let mut stack = vec![(root.to_owned(), 0usize, 0usize)];
    while let Some((dir, depth, dir_bytes)) = stack.pop() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))
            .map_err(io("setting a skill directory's mode"))?;
        for entry in fs::read_dir(&dir).map_err(io("reading a skill's directory"))? {
            let entry = entry.map_err(io("reading a skill's directory"))?;
            let name = entry.file_name();
            if !name.to_str().is_some_and(plain_component) {
                return Err(Problem::BadName.into());
            }
            let path_bytes = dir_bytes + usize::from(depth > 0) + name.len();
            budget.entry(depth + 1, path_bytes)?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(io("reading a skill's file"))?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(Problem::Symlink.into());
            } else if kind.is_dir() {
                stack.push((path, depth + 1, path_bytes));
            } else if kind.is_file() {
                budget.bytes(meta.len())?;
                let mode = file_mode(meta.permissions().mode());
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                    .map_err(io("setting a skill file's mode"))?;
            } else {
                return Err(Problem::SpecialFile.into());
            }
        }
    }
    Ok(())
}

/// Finds the skill in `root`, a tree [`check_tree`] passed: `root` itself
/// when it holds `SKILL.md`, or else the one directory in it, when that is
/// all it holds and it holds `SKILL.md`. Returns that directory and what
/// its `SKILL.md` says. Anything else is [`Problem::NoSkillFile`], a lone
/// file at the top included.
///
/// # Errors
///
/// The [`Problem`] with the skill, or an I/O failure.
pub fn find_skill(root: &Path) -> Result<(PathBuf, Manifest), CheckError> {
    let dir = if is_file(&root.join(SKILL_FILE))? {
        root.to_owned()
    } else {
        let mut entries = fs::read_dir(root).map_err(io("reading a skill's directory"))?;
        match (entries.next(), entries.next()) {
            (Some(only), None) => {
                let only = only.map_err(io("reading a skill's directory"))?.path();
                if !is_file(&only.join(SKILL_FILE))? {
                    return Err(Problem::NoSkillFile.into());
                }
                only
            }
            _ => return Err(Problem::NoSkillFile.into()),
        }
    };
    let file = dir.join(SKILL_FILE);
    let meta = fs::symlink_metadata(&file).map_err(io("reading SKILL.md"))?;
    if meta.len() > MAX_SKILL_MD_BYTES {
        return Err(Problem::SkillFile.into());
    }
    let bytes = fs::read(&file).map_err(io("reading SKILL.md"))?;
    let text = String::from_utf8(bytes).map_err(|_| Problem::SkillFile)?;
    Ok((dir, parse_skill_file(&text)?))
}

fn is_file(path: &Path) -> Result<bool, CheckError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(io("reading a skill's file")(err)),
    }
}

#[cfg(test)]
mod tests;
