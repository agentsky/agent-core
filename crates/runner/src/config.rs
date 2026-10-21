//! [`ProcessConfig`]: how `claude` processes are started and how long a
//! turn may take. [`PoolConfig`]: how many containers stay warm, for how
//! long, and which requesters share them ([`RequesterGroups`]).

use std::collections::HashSet;
use std::time::Duration;

use core_types::{MemberKey, ParseError};
use serde::Deserialize;

/// The default [`ProcessConfig::claude_bin`]: `claude`, found on the
/// sandbox image's `PATH`.
pub const DEFAULT_CLAUDE_BIN: &str = "claude";

/// The default [`ProcessConfig::anthropic_base_url`]: the credential proxy
/// on the sandbox network, as the design's credential proxy block has it.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "http://cred-proxy.internal:8080";

/// The default [`ProcessConfig::turn_timeout_secs`]: 30 minutes.
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 30 * 60;

/// The longest [`ProcessConfig::turn_timeout_secs`] allowed: a day.
const MAX_TURN_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// The default [`PoolConfig::idle_timeout_secs`]: 15 minutes.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 15 * 60;

/// The default [`PoolConfig::scope_container_cap`].
pub const DEFAULT_SCOPE_CONTAINER_CAP: usize = 4;

/// The default [`PoolConfig::global_container_cap`].
pub const DEFAULT_GLOBAL_CONTAINER_CAP: usize = 32;

/// The longest [`PoolConfig::idle_timeout_secs`] allowed: a day.
const MAX_IDLE_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// The largest container cap allowed.
const MAX_CONTAINER_CAP: usize = 4096;

/// Settings for starting `claude` processes, deserializable from TOML.
///
/// Every key has a default, and unknown keys are errors.
/// [`validate`](Self::validate) checks what serde can't.
///
/// ```
/// let config: runner::ProcessConfig = toml::from_str("turn_timeout_secs = 600")?;
/// config.validate()?;
/// assert_eq!(config.turn_timeout(), std::time::Duration::from_secs(600));
/// assert_eq!(config.claude_bin, "claude");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// The `claude` executable, as the sandbox runs it: a name looked up
    /// on the container's `PATH`, or an absolute path.
    #[serde(default = "default_claude_bin")]
    pub claude_bin: String,
    /// `ANTHROPIC_BASE_URL` for the process: the credential proxy.
    #[serde(default = "default_anthropic_base_url")]
    pub anthropic_base_url: String,
    /// How long one turn may take, in seconds, before its process is killed
    /// and the turn fails.
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
}

fn default_claude_bin() -> String {
    DEFAULT_CLAUDE_BIN.into()
}

fn default_anthropic_base_url() -> String {
    DEFAULT_ANTHROPIC_BASE_URL.into()
}

fn default_turn_timeout_secs() -> u64 {
    DEFAULT_TURN_TIMEOUT_SECS
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            claude_bin: default_claude_bin(),
            anthropic_base_url: default_anthropic_base_url(),
            turn_timeout_secs: default_turn_timeout_secs(),
        }
    }
}

/// An invalid [`ProcessConfig`]. It names the key, never its value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{key}: {reason}")]
pub struct ConfigError {
    key: &'static str,
    reason: &'static str,
}

impl ConfigError {
    /// The key that is wrong.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// Why its value was refused. It never repeats the value.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl ProcessConfig {
    /// [`turn_timeout_secs`](Self::turn_timeout_secs) as a [`Duration`].
    pub fn turn_timeout(&self) -> Duration {
        Duration::from_secs(self.turn_timeout_secs)
    }

    /// Checks what serde can't: a non-empty `claude_bin` without NUL or
    /// whitespace that doesn't start with `-`, an `http://` or `https://`
    /// base URL without whitespace or control characters, and a turn
    /// timeout from one second to a day.
    ///
    /// # Errors
    ///
    /// The first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |key, reason| Err(ConfigError { key, reason });
        let bin = &self.claude_bin;
        if bin.is_empty()
            || bin.starts_with('-')
            || bin.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return bad(
                "claude_bin",
                "must be a program name or path, without whitespace, and not start with `-`",
            );
        }
        let url = &self.anthropic_base_url;
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"));
        if rest.is_none_or(str::is_empty)
            || url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return bad(
                "anthropic_base_url",
                "must be an http:// or https:// URL without whitespace",
            );
        }
        if !(1..=MAX_TURN_TIMEOUT_SECS).contains(&self.turn_timeout_secs) {
            return bad(
                "turn_timeout_secs",
                "must be from 1 second to 86400 (a day)",
            );
        }
        Ok(())
    }
}

/// How the warm pool keeps containers, deserializable from TOML.
///
/// Every key has a default, and unknown keys are errors.
/// [`validate`](Self::validate) checks what serde can't.
///
/// ```
/// let config: runner::PoolConfig = toml::from_str("scope_container_cap = 2")?;
/// config.validate()?;
/// assert_eq!(config.scope_container_cap, 2);
/// assert_eq!(config.idle_timeout(), std::time::Duration::from_secs(900));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    /// How long a session's container and process stay warm after its last
    /// turn, in seconds, before they are reaped. The next turn resumes from
    /// the transcript.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// How many containers one scope of one agent (one volume) may have at
    /// once. A session that needs another waits, after an idle container of
    /// the scope is reaped for it if there is one.
    #[serde(default = "default_scope_container_cap")]
    pub scope_container_cap: usize,
    /// How many containers may run at once in all.
    #[serde(default = "default_global_container_cap")]
    pub global_container_cap: usize,
    /// Requesters who share a session's warm container and files. Default
    /// none: every requester stands alone.
    #[serde(default)]
    pub requester_groups: RequesterGroups,
}

/// Groups of requesters who trust each other with their credentials, so
/// one session's warm container and its `home/`, `tmp/` and `claude/`
/// files may pass between them: a turn of another member of the group
/// neither recycles the container nor clears the files.
///
/// In TOML it is an array of groups, each an array of member keys in
/// their string form, `<surface>:<team>:<user>`.
/// [`PoolConfig::validate`] refuses a group of fewer than two members and
/// a member listed twice, in one group or in two, so that
/// [`same_party`](Self::same_party) is an equivalence.
///
/// ```
/// use core_types::MemberKey;
///
/// let config: runner::PoolConfig =
///     toml::from_str(r#"requester_groups = [["slack:T1:U1", "slack:T1:U2"]]"#)?;
/// config.validate()?;
/// let key = |user: &str| format!("slack:T1:{user}").parse::<MemberKey>();
/// let groups = &config.requester_groups;
/// assert!(groups.same_party(&key("U1")?, &key("U2")?));
/// assert!(!groups.same_party(&key("U1")?, &key("U3")?));
/// assert!(groups.same_party(&key("U3")?, &key("U3")?));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<Vec<String>>")]
pub struct RequesterGroups(Vec<Vec<MemberKey>>);

impl RequesterGroups {
    /// The groups `groups`, unchecked until [`PoolConfig::validate`].
    pub fn new(groups: Vec<Vec<MemberKey>>) -> Self {
        Self(groups)
    }

    /// Whether `a` and `b` are the same party: the same key, or both in
    /// one group.
    pub fn same_party(&self, a: &MemberKey, b: &MemberKey) -> bool {
        a == b
            || self
                .0
                .iter()
                .any(|group| group.contains(a) && group.contains(b))
    }

    /// Checks that every group has two members or more and no member is
    /// listed twice.
    fn validate(&self) -> Result<(), &'static str> {
        let mut seen = HashSet::new();
        for group in &self.0 {
            if group.len() < 2 {
                return Err("every group must list two members or more");
            }
            if !group.iter().all(|member| seen.insert(member)) {
                return Err("a member may be listed only once, in one group");
            }
        }
        Ok(())
    }
}

impl TryFrom<Vec<Vec<String>>> for RequesterGroups {
    type Error = ParseError;

    fn try_from(groups: Vec<Vec<String>>) -> Result<Self, ParseError> {
        groups
            .iter()
            .map(|group| group.iter().map(|key| key.parse()).collect())
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

fn default_idle_timeout_secs() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}

fn default_scope_container_cap() -> usize {
    DEFAULT_SCOPE_CONTAINER_CAP
}

fn default_global_container_cap() -> usize {
    DEFAULT_GLOBAL_CONTAINER_CAP
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            scope_container_cap: default_scope_container_cap(),
            global_container_cap: default_global_container_cap(),
            requester_groups: RequesterGroups::default(),
        }
    }
}

impl PoolConfig {
    /// [`idle_timeout_secs`](Self::idle_timeout_secs) as a [`Duration`].
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    /// Checks what serde can't: an idle timeout from one second to a day,
    /// caps from 1 to 4096, and requester groups of two members or more
    /// that list no member twice.
    ///
    /// # Errors
    ///
    /// The first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |key, reason| Err(ConfigError { key, reason });
        if !(1..=MAX_IDLE_TIMEOUT_SECS).contains(&self.idle_timeout_secs) {
            return bad(
                "idle_timeout_secs",
                "must be from 1 second to 86400 (a day)",
            );
        }
        for (key, cap) in [
            ("scope_container_cap", self.scope_container_cap),
            ("global_container_cap", self.global_container_cap),
        ] {
            if !(1..=MAX_CONTAINER_CAP).contains(&cap) {
                return bad(key, "must be from 1 to 4096");
            }
        }
        self.requester_groups
            .validate()
            .or_else(|reason| bad("requester_groups", reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_defaults_match_the_plan() {
        let config: PoolConfig = toml::from_str("").unwrap();
        assert_eq!(config, PoolConfig::default());
        assert_eq!(config.idle_timeout(), Duration::from_secs(900));
        assert_eq!(config.scope_container_cap, 4);
        assert_eq!(config.global_container_cap, 32);
        config.validate().unwrap();
        assert!(toml::from_str::<PoolConfig>("idle_timeout = 5").is_err());
    }

    #[test]
    fn pool_validation_names_the_key() {
        type Change = fn(&mut PoolConfig);
        let cases: [(&str, Change); 6] = [
            ("idle_timeout_secs", |c| c.idle_timeout_secs = 0),
            ("idle_timeout_secs", |c| c.idle_timeout_secs = 86_401),
            ("scope_container_cap", |c| c.scope_container_cap = 0),
            ("scope_container_cap", |c| c.scope_container_cap = 4097),
            ("global_container_cap", |c| c.global_container_cap = 0),
            ("global_container_cap", |c| c.global_container_cap = 4097),
        ];
        for (key, change) in cases {
            let mut config = PoolConfig::default();
            change(&mut config);
            assert_eq!(config.validate().unwrap_err().key(), key);
        }
        let edge = PoolConfig {
            idle_timeout_secs: 86_400,
            scope_container_cap: 4096,
            global_container_cap: 1,
            requester_groups: RequesterGroups::default(),
        };
        edge.validate().unwrap();
    }

    fn key(user: &str) -> MemberKey {
        format!("slack:T1:{user}").parse().unwrap()
    }

    #[test]
    fn requester_groups_parse_from_key_strings() {
        let config: PoolConfig = toml::from_str(
            r#"requester_groups = [["slack:T1:U1", "rocketchat:chat.example.com:a%3Ab"], ["slack:T1:U2", "slack:T1:U3", "slack:T2:U2"]]"#,
        )
        .unwrap();
        config.validate().unwrap();
        let groups = &config.requester_groups;
        let rocket: MemberKey = "rocketchat:chat.example.com:a%3Ab".parse().unwrap();
        assert!(groups.same_party(&key("U1"), &rocket));
        assert!(groups.same_party(&rocket, &key("U1")));
        assert!(groups.same_party(&key("U2"), &key("U3")));
        assert!(!groups.same_party(&key("U1"), &key("U2")));
        assert!(!groups.same_party(&key("U4"), &key("U5")));
        assert!(groups.same_party(&key("U4"), &key("U4")));
        assert_eq!(
            PoolConfig::default().requester_groups,
            RequesterGroups::new(Vec::new())
        );
        assert!(
            !PoolConfig::default()
                .requester_groups
                .same_party(&key("U1"), &key("U2"))
        );

        let err =
            toml::from_str::<PoolConfig>(r#"requester_groups = [["teams:T1:U1", "slack:T1:U2"]]"#)
                .unwrap_err();
        assert!(err.to_string().contains("invalid member key"), "{err}");
        assert!(toml::from_str::<PoolConfig>(r#"requester_groups = ["slack:T1:U1"]"#).is_err());
    }

    #[test]
    fn requester_groups_must_be_disjoint_and_of_two_or_more() {
        for groups in [
            vec![vec![]],
            vec![vec![key("U1")]],
            vec![vec![key("U1"), key("U1")]],
            vec![vec![key("U1"), key("U2")], vec![key("U2"), key("U3")]],
        ] {
            let config = PoolConfig {
                requester_groups: RequesterGroups::new(groups),
                ..PoolConfig::default()
            };
            assert_eq!(config.validate().unwrap_err().key(), "requester_groups");
        }
    }

    #[test]
    fn defaults_match_the_design() {
        let config: ProcessConfig = toml::from_str("").unwrap();
        assert_eq!(config, ProcessConfig::default());
        assert_eq!(config.claude_bin, "claude");
        assert_eq!(config.anthropic_base_url, "http://cred-proxy.internal:8080");
        assert_eq!(config.turn_timeout(), Duration::from_secs(1800));
        config.validate().unwrap();
    }

    #[test]
    fn unknown_keys_are_refused() {
        let err = toml::from_str::<ProcessConfig>("turn_timeout = 5").unwrap_err();
        assert!(err.to_string().contains("turn_timeout"), "{err}");
    }

    #[test]
    fn validation_names_the_key() {
        type Change = fn(&mut ProcessConfig);
        let cases: [(&str, Change); 10] = [
            ("claude_bin", |c| c.claude_bin = String::new()),
            ("claude_bin", |c| c.claude_bin = "-p".into()),
            ("claude_bin", |c| c.claude_bin = "clau de".into()),
            ("claude_bin", |c| c.claude_bin = "claude\0".into()),
            ("anthropic_base_url", |c| {
                c.anthropic_base_url = "ftp://x".into();
            }),
            ("anthropic_base_url", |c| {
                c.anthropic_base_url = "http://".into();
            }),
            ("anthropic_base_url", |c| {
                c.anthropic_base_url = "http://x\n".into();
            }),
            ("anthropic_base_url", |c| {
                c.anthropic_base_url = "http://a b".into();
            }),
            ("turn_timeout_secs", |c| c.turn_timeout_secs = 0),
            ("turn_timeout_secs", |c| c.turn_timeout_secs = 86_401),
        ];
        for (key, change) in cases {
            let mut config = ProcessConfig::default();
            change(&mut config);
            let err = config.validate().unwrap_err();
            assert_eq!(err.key(), key, "{err}");
            assert_eq!(err.to_string(), format!("{key}: {}", err.reason()));
        }
        let edge = ProcessConfig {
            claude_bin: "/usr/local/bin/claude".into(),
            anthropic_base_url: "https://proxy".into(),
            turn_timeout_secs: 86_400,
        };
        edge.validate().unwrap();
    }
}
