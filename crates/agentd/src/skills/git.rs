//! Cloning a skill's Git repository, on agentd's own network.
//!
//! The source is what [`commands`]' parser accepted: an `https://` URL with
//! an optional `#ref`, in a character set no option or other transport
//! fits. agentd still treats it as hostile:
//!
//! - The host must be a DNS name ([`cred_proxy::normalize_host`]), so no
//!   IP literal, single-label name such as a Compose service, or numeric
//!   form reaches `git`. agentd resolves it itself and refuses it if any
//!   address is one the egress proxy never reaches ([`EgressPolicy`]):
//!   private, loopback, link-local and metadata addresses, agentd's own
//!   networks. `git` then connects only to the addresses checked
//!   (`http.curloptResolve`, Git 2.37 or later), so a second lookup can't
//!   rebind the name, and follows no redirect.
//! - `git clone --depth=1 --single-branch --no-recurse-submodules
//!   --no-tags`, the ref only inside `--branch=<ref>` and the URL after
//!   `--`, so neither can be read as an option.
//! - Only the `https` transport (`protocol.allow=never`), no credential
//!   helper or prompt, no system or global configuration, objects checked
//!   as they arrive (`transfer.fsckObjects`), and symlinks checked out as
//!   plain files (`core.symlinks=false`).
//! - An environment of its own, so nothing of agentd's reaches it, and a
//!   process group of its own, which is killed as a whole when the clone
//!   takes longer than [`CLONE_TIMEOUT`] or its directory grows past
//!   [`MAX_CLONE_BYTES`].

use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use cred_proxy::{EgressPolicy, Network, SystemNetwork, normalize_host};

/// How long a clone may take.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long resolving the Git host may take.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most a clone's directory may hold while it runs, `.git` included.
pub const MAX_CLONE_BYTES: u64 = 4 * super::package::MAX_SKILL_BYTES;
/// The most entries a clone's directory may hold while it runs.
const MAX_CLONE_ENTRIES: usize = 20_000;
/// How often a running clone's directory is measured.
const MEASURE_EVERY: Duration = Duration::from_millis(250);
/// The `PATH` `git` runs with.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Why a clone failed. Its message is for the owner, and names nothing
/// but the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloneError {
    /// The host isn't a DNS name.
    #[error(
        "The Git host must be a DNS name such as github.com; addresses and single-label \
         names aren't allowed."
    )]
    Host,
    /// The host didn't resolve.
    #[error("I couldn't look up {0}.")]
    Resolve(String),
    /// The host resolves to an address agentd never reaches.
    #[error("{host} resolves to a {why}, which I don't clone from.")]
    Unreachable {
        /// The host.
        host: String,
        /// What kind of address it is.
        why: &'static str,
    },
    /// `git` exited with an error.
    #[error(
        "Git couldn't clone that. Check that the URL is right, that the repository is public, \
         that the URL isn't a redirect (use the address it moved to), and that the ref after \
         `#` is a branch or tag."
    )]
    Failed,
    /// The clone took too long.
    #[error("Cloning took longer than {} seconds, so I stopped it.", CLONE_TIMEOUT.as_secs())]
    TimedOut,
    /// The clone grew too large.
    #[error(
        "The repository is too large: a skill's clone may take at most {} MB.",
        MAX_CLONE_BYTES / (1024 * 1024)
    )]
    TooLarge,
    /// `git` couldn't be run, or agentd failed around it. Logged; the owner
    /// gets a generic reply.
    #[error("running git failed: {0}")]
    Internal(String),
}

/// Clones skills' repositories with the `git` program.
#[derive(Clone)]
pub struct Git {
    program: PathBuf,
    policy: EgressPolicy,
    network: Arc<dyn Network>,
    local: Option<(String, PathBuf)>,
    timeout: Duration,
    max_bytes: u64,
}

impl fmt::Debug for Git {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Git")
            .field("program", &self.program)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl Git {
    /// Clones with `git` from the `PATH` above, refusing hosts that resolve
    /// to addresses `policy` never reaches, resolved by the system resolver.
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            program: PathBuf::from("git"),
            policy,
            network: Arc::new(SystemNetwork),
            local: None,
            timeout: CLONE_TIMEOUT,
            max_bytes: MAX_CLONE_BYTES,
        }
    }

    /// Runs `program` instead of `git`, with a shorter `timeout` and
    /// `max_bytes`, so tests reach the limits quickly.
    #[cfg(test)]
    fn with_limits(mut self, program: &Path, timeout: Duration, max_bytes: u64) -> Self {
        self.program = program.to_owned();
        self.timeout = timeout;
        self.max_bytes = max_bytes;
        self
    }

    /// Resolves hosts through `network` instead.
    pub fn with_network(mut self, network: Arc<dyn Network>) -> Self {
        self.network = network;
        self
    }

    /// For tests only: clones every source starting with `prefix`, an
    /// `https://` URL prefix, from the local directory `dir` instead,
    /// without resolving the host, so a test serves a fixture repository
    /// with no network. agentd never calls it, and no configuration key
    /// reaches it.
    pub fn serving_prefix_from_directory_for_tests(mut self, prefix: &str, dir: &Path) -> Self {
        self.local = Some((prefix.to_owned(), dir.to_owned()));
        self
    }

    /// Clones `source`, an `https://` URL with an optional `#ref` that
    /// [`commands::parse`] accepted, into `dest`, which must not exist.
    ///
    /// # Errors
    ///
    /// [`CloneError`] saying why.
    pub async fn clone_into(&self, source: &str, dest: &Path) -> Result<(), CloneError> {
        let (url, git_ref) = match source.split_once('#') {
            Some((url, git_ref)) => (url, Some(git_ref)),
            None => (source, None),
        };
        let local = self
            .local
            .as_ref()
            .filter(|(prefix, _)| url.starts_with(prefix.as_str()));
        let pin = match local {
            Some(_) => None,
            None => Some(self.pin(url).await?),
        };
        let args = clone_args(url, git_ref, dest, pin.as_deref(), local);
        self.run(&args, dest).await
    }

    /// The `http.curloptResolve` entry pinning `url`'s host to the
    /// addresses it resolves to, once all are known to be reachable.
    async fn pin(&self, url: &str) -> Result<String, CloneError> {
        let (host, port) = host_and_port(url).ok_or(CloneError::Host)?;
        let addresses = tokio::time::timeout(RESOLVE_TIMEOUT, self.network.resolve(&host, port))
            .await
            .ok()
            .and_then(Result::ok)
            .filter(|addresses| !addresses.is_empty())
            .ok_or_else(|| CloneError::Resolve(host.clone()))?;
        if let Some(why) = addresses.iter().find_map(|&ip| self.policy.unreachable(ip)) {
            return Err(CloneError::Unreachable { host, why });
        }
        Ok(resolve_entry(&host, port, &addresses))
    }

    async fn run(&self, args: &[OsString], dest: &Path) -> Result<(), CloneError> {
        let home = dest
            .parent()
            .ok_or_else(|| CloneError::Internal("the clone has no parent directory".into()))?;
        let mut child = tokio::process::Command::new(&self.program)
            .args(args)
            .env_clear()
            .env("PATH", PATH)
            .env("HOME", home)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| CloneError::Internal(err.to_string()))?;
        let mut group = Group(
            child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw),
        );
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut measure = tokio::time::interval(MEASURE_EVERY);
        let outcome = loop {
            tokio::select! {
                status = child.wait() => {
                    group.0 = None;
                    break Ok(status);
                }
                () = tokio::time::sleep_until(deadline) => break Err(CloneError::TimedOut),
                _ = measure.tick() => {
                    let dir = dest.to_owned();
                    let max = self.max_bytes;
                    let large = tokio::task::spawn_blocking(move || too_large(&dir, max))
                        .await
                        .unwrap_or(true);
                    if large {
                        break Err(CloneError::TooLarge);
                    }
                }
            }
        };
        match outcome {
            Ok(status) => exited(status.map_err(|err| CloneError::Internal(err.to_string()))?),
            Err(err) => {
                group.kill();
                let _ = child.wait().await;
                Err(err)
            }
        }
    }
}

/// `git`'s process group, killed whole when dropped unless `git` was seen
/// to exit first: `git` runs `git-remote-https` and `index-pack` as
/// children, which killing `git` alone would leave running. The group is
/// signalled only while its leader is unreaped, so its id can't have been
/// reused.
struct Group(Option<rustix::process::Pid>);

impl Group {
    fn kill(&mut self) {
        if let Some(group) = self.0.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

fn exited(status: ExitStatus) -> Result<(), CloneError> {
    if status.success() {
        Ok(())
    } else {
        tracing::info!(code = ?status.code(), "git clone of a skill failed");
        Err(CloneError::Failed)
    }
}

/// Whether the directory at `dir` holds more than `max_bytes` or
/// [`MAX_CLONE_ENTRIES`]. Symlinks aren't followed. A directory that
/// doesn't exist yet holds nothing.
fn too_large(dir: &Path, max_bytes: u64) -> bool {
    let mut bytes = 0u64;
    let mut entries = 0usize;
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            entries += 1;
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                bytes = bytes.saturating_add(meta.len());
            }
            if entries > MAX_CLONE_ENTRIES || bytes > max_bytes {
                return true;
            }
        }
    }
    false
}

/// The host of an `https://` URL, [normalized](normalize_host), and its
/// port, 443 unless the URL names one; `None` unless the host is a DNS
/// name.
fn host_and_port(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok().filter(|&port| port != 0)?),
        None => (authority, 443),
    };
    Some((normalize_host(host)?, port))
}

/// A curl `--resolve` entry, `host:port:addr[,addr…]`, with IPv6 addresses
/// in brackets.
fn resolve_entry(host: &str, port: u16, addresses: &[IpAddr]) -> String {
    let addresses: Vec<String> = addresses
        .iter()
        .map(|ip| match ip.to_canonical() {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        })
        .collect();
    format!("{host}:{port}:{}", addresses.join(","))
}

/// `git`'s arguments for cloning `url` at `git_ref` into `dest`, pinned to
/// `pin` when given, or served from `local`'s directory for tests.
fn clone_args(
    url: &str,
    git_ref: Option<&str>,
    dest: &Path,
    pin: Option<&str>,
    local: Option<&(String, PathBuf)>,
) -> Vec<OsString> {
    let mut config: Vec<String> = vec![
        "protocol.allow=never".into(),
        "protocol.https.allow=always".into(),
        "http.followRedirects=false".into(),
        "credential.helper=".into(),
        "core.symlinks=false".into(),
        "core.hooksPath=/dev/null".into(),
        "transfer.fsckObjects=true".into(),
    ];
    if let Some(pin) = pin {
        config.push(format!("http.curloptResolve={pin}"));
    }
    if let Some((prefix, dir)) = local {
        config.push(format!(
            "url.file://{}/.insteadOf={prefix}",
            dir.display().to_string().trim_end_matches('/')
        ));
        config.push("protocol.file.allow=always".into());
    }
    let mut args: Vec<OsString> = Vec::new();
    for entry in config {
        args.push("-c".into());
        args.push(entry.into());
    }
    for arg in [
        "clone",
        "--quiet",
        "--depth=1",
        "--single-branch",
        "--no-recurse-submodules",
        "--no-tags",
        "--template=",
    ] {
        args.push(arg.into());
    }
    if let Some(git_ref) = git_ref {
        args.push(format!("--branch={git_ref}").into());
    }
    args.push("--".into());
    args.push(url.into());
    args.push(dest.as_os_str().to_owned());
    args
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use async_trait::async_trait;
    use core_types::Cidr;
    use tokio::net::TcpStream;

    use super::*;

    struct Answers(Vec<IpAddr>);

    #[async_trait]
    impl Network for Answers {
        async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
            Ok(self.0.clone())
        }

        async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
            Err(io::Error::other("no connections in tests"))
        }
    }

    fn git(answers: Vec<IpAddr>) -> Git {
        let own = vec![Cidr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 114, 7)), 32).unwrap()];
        Git::new(EgressPolicy::new(Vec::new(), own)).with_network(Arc::new(Answers(answers)))
    }

    #[test]
    fn hosts_must_be_dns_names() {
        assert_eq!(
            host_and_port("https://GitHub.com./o/r.git"),
            Some(("github.com".into(), 443))
        );
        assert_eq!(
            host_and_port("https://git.example.org:8443/r"),
            Some(("git.example.org".into(), 8443))
        );
        for url in [
            "https://169.254.169.254/latest",
            "https://10.0.0.1/r",
            "https://mongodb/r",
            "https://rocketchat:3000/r",
            "https://127.1/r",
            "https://git.example.org:0/r",
            "https://git.example.org:99999/r",
            "http://github.com/r",
        ] {
            assert_eq!(host_and_port(url), None, "{url}");
        }
    }

    #[test]
    fn the_pin_names_every_address_and_brackets_ipv6() {
        let addresses = [
            IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3)),
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x50c0, 0, 0, 0, 0, 0, 0x154)),
            IpAddr::V6(Ipv4Addr::new(140, 82, 112, 4).to_ipv6_mapped()),
        ];
        assert_eq!(
            resolve_entry("github.com", 443, &addresses),
            "github.com:443:140.82.112.3,[2606:50c0::154],140.82.112.4"
        );
    }

    #[test]
    fn the_url_follows_a_lone_dash_dash_and_the_ref_stays_in_its_option() {
        let args = clone_args(
            "https://github.com/o/r.git",
            Some("v1.2"),
            Path::new("/work/src"),
            Some("github.com:443:140.82.112.3"),
            None,
        );
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let dashes = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &args[dashes..],
            ["--", "https://github.com/o/r.git", "/work/src"]
        );
        assert!(args.contains(&"--branch=v1.2".to_owned()));
        assert!(!args.contains(&"v1.2".to_owned()));
        for config in [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "http.followRedirects=false",
            "core.symlinks=false",
            "transfer.fsckObjects=true",
            "http.curloptResolve=github.com:443:140.82.112.3",
        ] {
            let at = args.iter().position(|a| a == config).unwrap();
            assert_eq!(args[at - 1], "-c", "{config}");
            assert!(at < dashes);
        }
        assert!(!args.iter().any(|a| a.contains("file")));
        for flag in ["--depth=1", "--single-branch", "--no-recurse-submodules"] {
            assert!(args[..dashes].contains(&flag.to_owned()), "{flag}");
        }
    }

    #[tokio::test]
    async fn hosts_resolving_to_unreachable_addresses_are_refused() {
        for (address, why) in [
            (IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), "link-local"),
            (IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), "private"),
            (IpAddr::V4(Ipv4Addr::LOCALHOST), "loopback"),
            (IpAddr::V4(Ipv4Addr::new(203, 0, 114, 7)), "own network"),
        ] {
            let git = git(vec![IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3)), address]);
            let err = git
                .clone_into("https://git.example.org/r.git", Path::new("/nonexistent/x"))
                .await
                .unwrap_err();
            let CloneError::Unreachable { host, why: said } = &err else {
                panic!("{err:?}");
            };
            assert_eq!(host, "git.example.org");
            assert!(said.contains(why), "{said} for {address}");
            assert!(
                err.to_string()
                    .starts_with("git.example.org resolves to a ")
            );
        }
        let err = git(Vec::new())
            .clone_into("https://git.example.org/r.git", Path::new("/nonexistent/x"))
            .await
            .unwrap_err();
        assert_eq!(err, CloneError::Resolve("git.example.org".into()));
        let err = git(Vec::new())
            .clone_into("https://10.0.0.1/r.git", Path::new("/nonexistent/x"))
            .await
            .unwrap_err();
        assert_eq!(err, CloneError::Host);
    }

    #[test]
    fn a_directory_is_too_large_by_bytes_or_entries() {
        let dir = std::env::temp_dir().join(format!("agentd-clone-{}", uuid::Uuid::new_v4()));
        assert!(!too_large(&dir, 10), "a directory not made yet");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/f"), b"x").unwrap();
        assert!(!too_large(&dir, 10));
        let big = std::fs::File::create(dir.join("big")).unwrap();
        big.set_len(11).unwrap();
        assert!(too_large(&dir, 10));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A stand-in for `git` that starts a child in its group, notes the
    /// child's pid in `pid` next to the clone, then writes `bytes` bytes
    /// into the clone and sleeps.
    fn stand_in(dir: &Path, bytes: u64) -> PathBuf {
        let script = dir.join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor dest; do :; done\nmkdir -p \"$dest\"\nsleep 60 &\n\
                 echo $! > \"$dest/../pid\"\nhead -c {bytes} /dev/zero > \"$dest/pack\"\nsleep 60\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    /// Whether the process `pid` is gone or a zombie.
    fn gone(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
            .map_or(true, |stat| stat.contains(") Z "))
    }

    async fn stopped_clone(bytes: u64) -> CloneError {
        let dir = std::env::temp_dir().join(format!("agentd-clone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("repos")).unwrap();
        let git = git(Vec::new())
            .with_limits(&stand_in(&dir, bytes), Duration::from_secs(2), 1024)
            .serving_prefix_from_directory_for_tests("https://git.test/", &dir.join("repos"));
        let started = tokio::time::Instant::now();
        let err = git
            .clone_into("https://git.test/r.git", &dir.join("work/src"))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(dir.join("work/pid")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gone(&pid) {
            assert!(std::time::Instant::now() < deadline, "git's child survived");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&dir).unwrap();
        err
    }

    #[tokio::test]
    async fn a_clone_past_its_time_or_size_is_killed_with_its_children() {
        assert_eq!(stopped_clone(10).await, CloneError::TimedOut);
        assert_eq!(stopped_clone(4096).await, CloneError::TooLarge);
    }
}
