//! [`SandboxConfig`]: the `[sandbox]` section of agentd's configuration.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// The default for [`SandboxConfig::cleanup_period_days`]: ten years, so
/// idle threads keep their transcripts. Claude Code's own default is 30.
pub const DEFAULT_CLEANUP_PERIOD_DAYS: u32 = 3650;

/// The uid and gid sandboxes run as by default, the `agent` user of the
/// sandbox image.
pub const DEFAULT_SANDBOX_UID: u32 = 10001;

/// Sandbox settings, deserializable from the `[sandbox]` TOML section.
///
/// Every key but `image` has a default. Unknown keys are errors, and
/// [`validate`](Self::validate) checks what serde can't.
///
/// ```
/// let config: sandbox::SandboxConfig = toml::from_str(r#"image = "agent-sandbox:2.1.285""#)?;
/// config.validate()?;
/// assert_eq!(config.network, "sandbox");
/// assert_eq!(config.uid, 10001);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// The pinned sandbox image that agentd starts sessions from.
    pub image: String,
    /// The Docker network sandboxes attach to, and the only one. It must
    /// be `internal`, so it has no route out, and this must be Docker's
    /// name for it, not its ID: Compose prefixes the project name unless
    /// the network sets `name:`. Docker's own modes are refused here:
    /// `host`, `none`, `default` and `bridge` (which has a route out), and
    /// anything with a `:`, such as `container:<id>`. Whether the network
    /// exists under this name and is `internal` is checked by
    /// [`DockerSandbox`](crate::DockerSandbox) on every start.
    #[serde(default = "default_network")]
    pub network: String,
    /// agentd's data directory as the Docker daemon sees it, when agentd
    /// runs in a container that mounts it somewhere else. Bind mount sources
    /// are host paths, so every path under agentd's data directory is
    /// rewritten to this prefix. Unset, paths are passed as they are.
    #[serde(default)]
    pub host_data_dir: Option<PathBuf>,
    /// The uid sandboxes run as. Never 0.
    #[serde(default = "default_uid")]
    pub uid: u32,
    /// The gid sandboxes run as. Never 0.
    #[serde(default = "default_uid")]
    pub gid: u32,
    /// Memory limit per sandbox, in MiB, with no swap on top.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    /// CPU limit per sandbox, in CPUs: at least 0.01, Docker's minimum, and
    /// at most 1024. Docker receives it in billionths of a CPU.
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    /// Maximum number of processes per sandbox.
    #[serde(default = "default_pids_limit")]
    pub pids_limit: u32,
    /// Size of the sandbox's `/tmp` tmpfs, in MiB. It counts toward the
    /// memory limit.
    #[serde(default = "default_tmp_size_mb")]
    pub tmp_size_mb: u64,
    /// How long `stop` waits, in seconds, after sending SIGTERM to the
    /// container's init before killing the container, at most
    /// [`MAX_STOP_TIMEOUT_SECS`]. Only the init and its idle command get
    /// SIGTERM; processes started with `exec` get no grace period and are
    /// killed when the container stops.
    #[serde(default = "default_stop_timeout_secs")]
    pub stop_timeout_secs: u32,
    /// `cleanupPeriodDays` in each session's `settings.json`.
    #[serde(default = "default_cleanup_period_days")]
    pub cleanup_period_days: u32,
    /// This agentd's name on the Docker host, in the `agentd.instance`
    /// label. Listing, reaping and events see only containers with the same
    /// name, so two agentd (or test runs) on one host leave each other's
    /// sandboxes alone. Two instances must never share one data directory:
    /// each could run the same session, and the runner reads a resumed
    /// session's restored cost only in a container it has just started,
    /// which assumes no other agentd runs a process on that session's
    /// transcript.
    #[serde(default = "default_instance")]
    pub instance: String,
}

fn default_network() -> String {
    "sandbox".into()
}

fn default_uid() -> u32 {
    DEFAULT_SANDBOX_UID
}

fn default_memory_mb() -> u64 {
    4096
}

fn default_cpus() -> f64 {
    2.0
}

fn default_pids_limit() -> u32 {
    1024
}

fn default_tmp_size_mb() -> u64 {
    512
}

fn default_stop_timeout_secs() -> u32 {
    10
}

fn default_cleanup_period_days() -> u32 {
    DEFAULT_CLEANUP_PERIOD_DAYS
}

fn default_instance() -> String {
    "agentd".into()
}

/// A [`SandboxConfig`] value that [`validate`](SandboxConfig::validate)
/// refused: the key, relative to the `[sandbox]` section, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{key}: {message}")]
pub struct ConfigError {
    /// The key, such as `memory_mb`.
    pub key: &'static str,
    /// Why the value was refused. It never repeats the value.
    pub message: &'static str,
}

/// The largest [`SandboxConfig::stop_timeout_secs`]. A stop request waits
/// that long before Docker answers, and bollard gives up on a request after
/// 120 seconds, which would leave the container stopping and not removed.
pub const MAX_STOP_TIMEOUT_SECS: u32 = 60;

/// Network names that select a Docker network mode instead of a network.
const RESERVED_NETWORKS: [&str; 4] = ["host", "none", "default", "bridge"];

const MAX_MEMORY_MB: u64 = 1 << 20;
const MIN_NANO_CPUS: i64 = 10_000_000;
const MAX_NANO_CPUS: i64 = 1024 * 1_000_000_000;

impl SandboxConfig {
    /// A configuration with every default and the given image.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            network: default_network(),
            host_data_dir: None,
            uid: default_uid(),
            gid: default_uid(),
            memory_mb: default_memory_mb(),
            cpus: default_cpus(),
            pids_limit: default_pids_limit(),
            tmp_size_mb: default_tmp_size_mb(),
            stop_timeout_secs: default_stop_timeout_secs(),
            cleanup_period_days: default_cleanup_period_days(),
            instance: default_instance(),
        }
    }

    /// [`cpus`](Self::cpus) in billionths of a CPU, rounded to the nearest,
    /// as Docker's `NanoCpus` takes it. Docker reads 0 as no limit at all,
    /// so [`validate`](Self::validate) refuses any `cpus` this makes less
    /// than 0.01 CPU, NaN included.
    pub(crate) fn nano_cpus(&self) -> i64 {
        (self.cpus * 1e9).round() as i64
    }

    /// Checks what serde can't.
    ///
    /// # Errors
    ///
    /// The first key whose value is out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fail = |key, message| Err(ConfigError { key, message });
        if self.image.trim().is_empty() {
            return fail("image", "must not be empty");
        }
        if self.network.trim().is_empty() {
            return fail("network", "must not be empty");
        }
        if self.network.contains(':')
            || RESERVED_NETWORKS
                .iter()
                .any(|mode| self.network.eq_ignore_ascii_case(mode))
        {
            return fail(
                "network",
                "must name the internal sandbox network, not host, none, default, bridge or a `:` mode",
            );
        }
        if let Some(dir) = &self.host_data_dir
            && !is_plain_absolute(dir)
        {
            return fail("host_data_dir", "must be an absolute path without `..`");
        }
        if self.uid == 0 {
            return fail("uid", "sandboxes never run as root");
        }
        if self.gid == 0 {
            return fail("gid", "sandboxes never run in the root group");
        }
        if !(64..=MAX_MEMORY_MB).contains(&self.memory_mb) {
            return fail("memory_mb", "must be between 64 and 1048576");
        }
        if !(MIN_NANO_CPUS..=MAX_NANO_CPUS).contains(&self.nano_cpus()) {
            return fail("cpus", "must be between 0.01 and 1024");
        }
        if self.pids_limit == 0 {
            return fail("pids_limit", "must be at least 1");
        }
        if !(1..=MAX_MEMORY_MB).contains(&self.tmp_size_mb) {
            return fail("tmp_size_mb", "must be between 1 and 1048576");
        }
        if self.stop_timeout_secs > MAX_STOP_TIMEOUT_SECS {
            return fail("stop_timeout_secs", "must be at most 60");
        }
        if self.cleanup_period_days == 0 {
            return fail("cleanup_period_days", "must be at least 1");
        }
        if !is_label_name(&self.instance) {
            return fail(
                "instance",
                "must be 1 to 63 characters from [A-Za-z0-9_.-], starting with a letter or digit",
            );
        }
        Ok(())
    }
}

/// Whether `path` is absolute and has no `..` components, so
/// joining or stripping prefixes on it can't escape a directory.
pub(crate) fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn is_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_section_gets_every_default() {
        let config: SandboxConfig = toml::from_str(r#"image = "img""#).unwrap();
        assert_eq!(config, SandboxConfig::new("img"));
        assert_eq!(config.cleanup_period_days, 3650);
        assert_eq!(config.instance, "agentd");
        assert_eq!(config.host_data_dir, None);
        config.validate().unwrap();
    }

    #[test]
    fn every_key_deserializes() {
        let config: SandboxConfig = toml::from_str(
            r#"
            image = "img"
            network = "net"
            host_data_dir = "/srv/agentd"
            uid = 1001
            gid = 1002
            memory_mb = 2048
            cpus = 0.5
            pids_limit = 64
            tmp_size_mb = 32
            stop_timeout_secs = 3
            cleanup_period_days = 7
            instance = "test-1"
            "#,
        )
        .unwrap();
        assert_eq!(config.network, "net");
        assert_eq!(config.host_data_dir, Some(PathBuf::from("/srv/agentd")));
        assert_eq!((config.uid, config.gid), (1001, 1002));
        assert_eq!(config.memory_mb, 2048);
        assert!((config.cpus - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.pids_limit, 64);
        assert_eq!(config.tmp_size_mb, 32);
        assert_eq!(config.stop_timeout_secs, 3);
        assert_eq!(config.cleanup_period_days, 7);
        assert_eq!(config.instance, "test-1");
        config.validate().unwrap();
    }

    #[test]
    fn unknown_keys_and_a_missing_image_are_refused() {
        let err = toml::from_str::<SandboxConfig>("image = \"i\"\nmemory = 1").unwrap_err();
        assert!(err.to_string().contains("memory"), "{err}");
        let err = toml::from_str::<SandboxConfig>("network = \"n\"").unwrap_err();
        assert!(err.to_string().contains("image"), "{err}");
    }

    #[test]
    fn out_of_range_values_name_their_key() {
        type Change = fn(&mut SandboxConfig);
        let cases: [(&str, Change); 25] = [
            ("image", |c| c.image = " ".into()),
            ("network", |c| c.network = String::new()),
            ("network", |c| c.network = "host".into()),
            ("network", |c| c.network = "none".into()),
            ("network", |c| c.network = "default".into()),
            ("network", |c| c.network = "Bridge".into()),
            ("network", |c| c.network = "container:abc".into()),
            ("host_data_dir", |c| c.host_data_dir = Some("rel".into())),
            ("host_data_dir", |c| {
                c.host_data_dir = Some("/a/../b".into())
            }),
            ("uid", |c| c.uid = 0),
            ("gid", |c| c.gid = 0),
            ("memory_mb", |c| c.memory_mb = 63),
            ("memory_mb", |c| c.memory_mb = MAX_MEMORY_MB + 1),
            ("cpus", |c| c.cpus = 0.0),
            ("cpus", |c| c.cpus = f64::NAN),
            ("cpus", |c| c.cpus = 1e-10),
            ("cpus", |c| c.cpus = 0.0099),
            ("cpus", |c| c.cpus = -1.0),
            ("cpus", |c| c.cpus = f64::NEG_INFINITY),
            ("pids_limit", |c| c.pids_limit = 0),
            ("tmp_size_mb", |c| c.tmp_size_mb = 0),
            ("stop_timeout_secs", |c| {
                c.stop_timeout_secs = MAX_STOP_TIMEOUT_SECS + 1
            }),
            ("cleanup_period_days", |c| c.cleanup_period_days = 0),
            ("instance", |c| c.instance = "-x".into()),
            ("instance", |c| c.instance = "a:b".into()),
        ];
        for (key, change) in cases {
            let mut config = SandboxConfig::new("img");
            change(&mut config);
            let err = config.validate().unwrap_err();
            assert_eq!(err.key, key, "{err}");
        }
        let mut config = SandboxConfig::new("img");
        config.instance = "x".repeat(64);
        assert_eq!(config.validate().unwrap_err().key, "instance");
        config.instance = "x".repeat(63);
        config.validate().unwrap();
        config.stop_timeout_secs = MAX_STOP_TIMEOUT_SECS;
        config.network = "sandbox-bridge".into();
        config.validate().unwrap();
        config.cpus = 0.01;
        config.validate().unwrap();
        assert_eq!(config.nano_cpus(), MIN_NANO_CPUS);
        config.cpus = 2.01;
        assert_eq!(config.nano_cpus(), 2_010_000_000);
        config.cpus = 1024.0;
        config.validate().unwrap();
        config.cpus = f64::INFINITY;
        assert_eq!(config.validate().unwrap_err().key, "cpus");
    }

    #[test]
    fn plain_absolute_paths() {
        assert!(is_plain_absolute(Path::new("/var/lib/agentd")));
        assert!(!is_plain_absolute(Path::new("var/lib")));
        assert!(!is_plain_absolute(Path::new("/var/../etc")));
    }
}
