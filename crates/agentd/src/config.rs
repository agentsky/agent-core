//! Configuration: one TOML file plus secret environment variables.
//!
//! The file holds everything but secrets; `config/agentd.example.toml`
//! documents every key. Secrets come only from the environment:
//! [`AGENTD_MASTER_KEY`](MASTER_KEY_VAR), [`AGENTD_RC_MANAGER_TOKEN`](RC_MANAGER_TOKEN_VAR)
//! and [`AGENTD_SLACK_MANAGER_*`](SLACK_MANAGER_PREFIX). Each value has one
//! source, so there is no precedence to get wrong.
//!
//! Other variables starting with `AGENTD_`:
//!
//! - Kubernetes service links, which Kubernetes sets for a Service named
//!   `agentd` (or `agentd-…`) in the same namespace, are skipped silently:
//!   names ending in `_PORT`, `_SERVICE_HOST` or `_SERVICE_PORT`, holding
//!   `_SERVICE_PORT_`, or ending in `_PORT_<number>_<TCP|UDP|SCTP>`
//!   optionally followed by `_PROTO`, `_PORT` or `_ADDR`.
//! - A near miss of a secret's name is an error, so a misspelled secret
//!   fails at startup: within two edits of `AGENTD_MASTER_KEY` or
//!   `AGENTD_RC_MANAGER_TOKEN`, or starting within two edits of
//!   `AGENTD_SLACK_MANAGER_` without being a valid Slack manager name.
//! - Anything else is ignored, and named in [`Config::unknown_env`] for
//!   `serve` and `migrate` to log as a warning.
//!
//! A secret may be neither empty nor start or end with white space. The
//! trailing newline of a secret mounted from a file would otherwise become
//! part of it, and a signing secret with one fails every request.
//!
//! Every other `AGENTD_SLACK_MANAGER_*` secret requires
//! [`AGENTD_SLACK_MANAGER_SIGNING_SECRET`](SLACK_MANAGER_SIGNING_SECRET_VAR),
//! so a misspelling of that name fails at startup too, and the signing
//! secret requires
//! [`AGENTD_SLACK_MANAGER_BOT_TOKEN`](SLACK_MANAGER_BOT_TOKEN_VAR): the
//! manager app answers commands with it.
//!
//! The environment is passed in rather than read from the process, so tests
//! supply their own (`std::env::set_var` is `unsafe` in edition 2024, and the
//! workspace forbids `unsafe`).
//!
//! Every [`ConfigError`] names the key or variable at fault, and none of them
//! repeats a secret.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use auth::OAuthConfig;
use core_types::{Cidr, MemberKey};
use cred_proxy::{DEFAULT_UPSTREAM, EgressLimits, EgressPolicy, EgressProxy, HostRule};
use router::ModelPolicy;
use runner::{
    DEFAULT_CLAUDE_BIN, DEFAULT_GLOBAL_CONTAINER_CAP, DEFAULT_IDLE_TIMEOUT_SECS,
    DEFAULT_SCOPE_CONTAINER_CAP, DEFAULT_TURN_TIMEOUT_SECS, PoolConfig, ProcessConfig,
};
use sandbox::SandboxConfig;
use secrecy::SecretString;
use serde::Deserialize;
use serde_path_to_error::Segment;
use store::Sealer;
use tracing_subscriber::EnvFilter;

use crate::agents::DEFAULT_MAX_PER_OWNER;

/// The master key for encryption at rest: 32 bytes, standard base64, as
/// `agentd gen-key` prints it. Required.
pub const MASTER_KEY_VAR: &str = "AGENTD_MASTER_KEY";
/// The Rocket.Chat manager's personal access token. Required when the
/// `[rocketchat]` section is present.
pub const RC_MANAGER_TOKEN_VAR: &str = "AGENTD_RC_MANAGER_TOKEN";
/// The prefix of the Slack manager app's secrets, such as
/// `AGENTD_SLACK_MANAGER_SIGNING_SECRET`. They are collected into
/// [`Secrets::slack_manager`] by lowercased suffix.
pub const SLACK_MANAGER_PREFIX: &str = "AGENTD_SLACK_MANAGER_";
/// The Slack manager app's signing secret, which verifies requests to
/// `/slack/b/manager/…`. The manager binding is known only when it is set,
/// and every other [`AGENTD_SLACK_MANAGER_*`](SLACK_MANAGER_PREFIX)
/// variable requires it.
pub const SLACK_MANAGER_SIGNING_SECRET_VAR: &str = "AGENTD_SLACK_MANAGER_SIGNING_SECRET";
/// The key of [`SLACK_MANAGER_SIGNING_SECRET_VAR`] in
/// [`Secrets::slack_manager`].
const SLACK_MANAGER_SIGNING_SECRET: &str = "signing_secret";
/// The Slack manager app's bot token (`xoxb-…`), which answers commands and
/// sends the manager's DMs. Required with
/// [`SLACK_MANAGER_SIGNING_SECRET_VAR`], and the other way round.
pub const SLACK_MANAGER_BOT_TOKEN_VAR: &str = "AGENTD_SLACK_MANAGER_BOT_TOKEN";
/// The key of [`SLACK_MANAGER_BOT_TOKEN_VAR`] in [`Secrets::slack_manager`].
const SLACK_MANAGER_BOT_TOKEN: &str = "bot_token";
/// The prefix of every variable agentd reads. Unknown ones are sorted as the
/// [module docs](self) describe.
const ENV_PREFIX: &str = "AGENTD_";
/// How many single-character edits away from a secret's name an unknown
/// variable's name may be and still be refused as a misspelling of it.
const NEAR_MISS_EDITS: usize = 2;

/// The default for `server.drain_timeout_secs`.
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;
/// The largest `server.drain_timeout_secs` accepted.
pub const MAX_DRAIN_TIMEOUT_SECS: u64 = 3600;
/// The default for `server.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info";
/// The default for `limits.attach_max_bytes`: 50 MiB.
pub const DEFAULT_ATTACH_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// The default for `slack.api_url`.
pub const DEFAULT_SLACK_API_URL: &str = surface_slack::web::DEFAULT_BASE_URL;
/// The default for `slack.install_reminder_secs`: an hour.
pub const DEFAULT_INSTALL_REMINDER_SECS: u64 = 60 * 60;

/// agentd's configuration, validated.
#[derive(Debug)]
#[non_exhaustive]
pub struct Config {
    /// `[server]`: the public listener, shutdown and logging.
    pub server: ServerConfig,
    /// `[internal]`: the listeners sandboxes reach.
    pub internal: InternalConfig,
    /// `[store]`: the database and the data directory.
    pub store: StoreConfig,
    /// `[limits]`: caps on what agents may do.
    pub limits: LimitsConfig,
    /// `[proxy]`: the credential proxy's upstream, and what sandboxes may
    /// reach through the egress proxy.
    pub proxy: ProxyConfig,
    /// `[sandbox]`: the containers sessions run in. agentd runs turns only
    /// when it is set.
    pub sandbox: Option<SandboxConfig>,
    /// `[runner]`: the `claude` processes turns run in, and how long they
    /// stay warm.
    pub runner: RunnerConfig,
    /// `[agents]`: caps on members' agents.
    pub agents: AgentsConfig,
    /// `[community]`: who the community admins are.
    pub community: CommunityConfig,
    /// `[claude_oauth]`: Claude Code's OAuth parameters, for linking
    /// accounts. Every key has a default, so the section is optional.
    pub claude_oauth: OAuthConfig,
    /// `[rocketchat]`: the Rocket.Chat server and its manager bot, if agentd
    /// serves Rocket.Chat.
    pub rocketchat: Option<RocketChatConfig>,
    /// `[slack]`: where the Slack Web API is. Every key has a default, so
    /// the section is optional. agentd serves Slack when the manager app's
    /// secrets are set.
    pub slack: SlackConfig,
    /// Secrets from the environment.
    pub secrets: Secrets,
    /// Unknown `AGENTD_` variables that were ignored, by name, sorted.
    /// Kubernetes service links aren't listed.
    pub unknown_env: Vec<String>,
}

/// The file's sections. Each task adds the section it first needs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    server: ServerConfig,
    internal: InternalConfig,
    store: StoreConfig,
    #[serde(default)]
    limits: LimitsConfig,
    #[serde(default)]
    proxy: ProxyConfig,
    sandbox: Option<SandboxConfig>,
    #[serde(default)]
    runner: RunnerConfig,
    #[serde(default)]
    agents: AgentsConfig,
    #[serde(default)]
    community: CommunityConfig,
    #[serde(default)]
    claude_oauth: OAuthConfig,
    rocketchat: Option<RocketChatConfig>,
    #[serde(default)]
    slack: SlackConfig,
}

/// `[server]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ServerConfig {
    /// `listen`: the public listener's address, agentd's address on the
    /// `egress` network, such as `172.31.0.2:8443`. Never an unspecified
    /// address such as `0.0.0.0`, `[::]` or `[::ffff:0.0.0.0]`, and never
    /// inside `internal.sandbox_subnet`.
    pub listen: SocketAddr,
    /// `drain_timeout_secs`: how long shutdown waits for in-flight requests
    /// before dropping them.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
    /// `log_filter`: which log lines to write, in `tracing_subscriber`'s
    /// `EnvFilter` syntax, such as `info` or `info,agentd=debug`.
    #[serde(default = "default_log_filter")]
    pub log_filter: String,
}

impl ServerConfig {
    /// `drain_timeout_secs` as a [`Duration`].
    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.drain_timeout_secs)
    }
}

/// `[internal]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct InternalConfig {
    /// `proxy_listen`: the credential proxy listener's address, agentd's
    /// address on the `sandbox` network, port 8080. Must be inside
    /// `sandbox_subnet`, and with `[sandbox]` its port must be
    /// [`cred_proxy::PROXY_URL`]'s, where sandboxes reach it.
    pub proxy_listen: SocketAddr,
    /// `ctl_listen`: the agentctl API listener's address, agentd's address on
    /// the `sandbox` network, port 8081. Must be inside `sandbox_subnet`, and
    /// with `[sandbox]` its port must be
    /// [`AGENTCTL_URL`](crate::pipeline::AGENTCTL_URL)'s, where sandboxes
    /// reach it.
    pub ctl_listen: SocketAddr,
    /// `sandbox_subnet`: the `sandbox` network. The public listener refuses
    /// connections from it, and must not be inside it; the proxy and ctl
    /// listeners must be.
    pub sandbox_subnet: Cidr,
}

/// `[store]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct StoreConfig {
    /// `url`: the SQLite database, such as
    /// `sqlite:///var/lib/agentd/agentd.db`, or `sqlite::memory:` for tests.
    pub url: String,
    /// `data_dir`: agentd's data directory, an absolute path, such as
    /// `/var/lib/agentd`. Files agents attach are staged under
    /// `ctl-outbox/` in it until their turn's reply is posted.
    pub data_dir: PathBuf,
}

/// `[rocketchat]`. The manager bot's token comes from
/// [`AGENTD_RC_MANAGER_TOKEN`](RC_MANAGER_TOKEN_VAR).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RocketChatConfig {
    /// The server's base URL, `https://` (or `http://`), such as
    /// `https://chat.example.com`.
    pub base_url: String,
    /// The realtime endpoint, `wss://` or `ws://`, when it isn't
    /// `<base_url>/websocket`.
    #[serde(default)]
    pub websocket_url: Option<String>,
    /// The id agentd gives the server, which every stored Rocket.Chat
    /// identity carries, such as `chat.example.com`. Changing it later makes
    /// every member a stranger.
    pub team: String,
    /// The manager bot's user `_id`, whose personal access token is
    /// `AGENTD_RC_MANAGER_TOKEN`.
    pub manager_user_id: String,
    /// An `https://` (or `http://`) image URL each new agent's bot sets as
    /// its avatar. Without it, bots keep Rocket.Chat's default avatar.
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// `[slack]`. Every key has a default, so the section is optional. The
/// manager app's secrets come from
/// [`AGENTD_SLACK_MANAGER_*`](SLACK_MANAGER_PREFIX).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct SlackConfig {
    /// `api_url`: the Web API's base URL, [`DEFAULT_SLACK_API_URL`] unless
    /// a test points it at a fake.
    pub api_url: String,
    /// `public_url`: agentd's public HTTPS URL as Slack reaches the public
    /// listener, such as `https://agentd.example.com`. Agent apps' request
    /// URLs and their OAuth redirect URL are built from it. Without it,
    /// `/agent create` on Slack is refused.
    pub public_url: Option<String>,
    /// `public_posting`: whether agent apps ask for `chat:write.public`,
    /// which lets them post in public channels they aren't in. Off by
    /// default. It applies to apps created after it changes.
    pub public_posting: bool,
    /// `install_reminder_secs`: how long an agent's app may wait to be
    /// installed before its owner is reminded, once, from 60 to 604800.
    /// Default 3600.
    pub install_reminder_secs: u64,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_SLACK_API_URL.to_owned(),
            public_url: None,
            public_posting: false,
            install_reminder_secs: DEFAULT_INSTALL_REMINDER_SECS,
        }
    }
}

impl SlackConfig {
    /// [`public_url`](Self::public_url) without trailing slashes, if set.
    pub fn public_url(&self) -> Option<String> {
        self.public_url
            .as_deref()
            .and_then(surface_slack::manifest::public_url)
    }

    /// [`install_reminder_secs`](Self::install_reminder_secs) as a
    /// [`Duration`].
    pub fn install_reminder(&self) -> Duration {
        Duration::from_secs(self.install_reminder_secs)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        surface_slack::SlackClient::new(&self.api_url).map_err(|_| {
            invalid(
                "slack.api_url",
                "must be an http:// or https:// URL without user info, query or fragment",
            )
        })?;
        if let Some(url) = &self.public_url
            && surface_slack::manifest::public_url(url).is_none()
        {
            return Err(invalid(
                "slack.public_url",
                "must be an https:// URL without user info, query or fragment",
            ));
        }
        if !(60..=604_800).contains(&self.install_reminder_secs) {
            return Err(invalid(
                "slack.install_reminder_secs",
                "must be from 60 to 604800",
            ));
        }
        Ok(())
    }
}

/// `[limits]`. Every key has a default, so the section is optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct LimitsConfig {
    /// `attach_max_bytes`: the largest file `agentctl attach` may stage.
    pub attach_max_bytes: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            attach_max_bytes: DEFAULT_ATTACH_MAX_BYTES,
        }
    }
}

/// `[proxy]`. Every key has a default, so the section is optional; without
/// it sandboxes reach no host but the credential proxy's upstream.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct ProxyConfig {
    /// `upstream`: where the credential proxy forwards the requests
    /// sandboxes send to `ANTHROPIC_BASE_URL`, [`DEFAULT_UPSTREAM`] unless a
    /// test points it at a fake. An `https://` URL, or an `http://` one
    /// whose host is a loopback IP address, with no credentials, query or
    /// fragment. agentd logs a warning at startup when it isn't the
    /// default.
    pub upstream: String,
    /// `allow`: the hosts sandboxes may open HTTPS tunnels to, as
    /// [`HostRule`]s such as `github.com`, `*.githubusercontent.com` or
    /// `git.example.com:8443`. Port 443 unless a rule names another.
    /// `api.anthropic.com` is refused as a rule, and denied whatever a
    /// wildcard says.
    pub allow: Vec<HostRule>,
    /// `max_tunnels`: open egress tunnels across all sandboxes, default
    /// 256 ([`EgressLimits::max_tunnels`]).
    pub max_tunnels: usize,
    /// `max_session_tunnels`: open egress tunnels per sandbox, default 32
    /// ([`EgressLimits::max_session_tunnels`]). At most `max_tunnels`.
    pub max_session_tunnels: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        let limits = EgressLimits::default();
        Self {
            upstream: DEFAULT_UPSTREAM.to_owned(),
            allow: Vec::new(),
            max_tunnels: limits.max_tunnels,
            max_session_tunnels: limits.max_session_tunnels,
        }
    }
}

/// `[runner]`. Every key has a default, so the section is optional.
///
/// The addresses a process uses are not configuration: it reaches the
/// credential proxy as `ANTHROPIC_BASE_URL` at
/// [`cred_proxy::PROXY_URL`] and the agentctl API at
/// [`AGENTCTL_URL`](crate::pipeline::AGENTCTL_URL), the names agentd has on
/// the sandbox network, which the egress environment's `NO_PROXY` names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct RunnerConfig {
    /// `claude_bin`: the `claude` executable in the sandbox image, a name
    /// looked up on its `PATH` or an absolute path. Default `claude`.
    pub claude_bin: String,
    /// `turn_timeout_secs`: how long one turn may take before its process
    /// is killed and the turn fails, from 1 to 86400. Default 1800.
    pub turn_timeout_secs: u64,
    /// `idle_timeout_secs`: how long a session's container and process stay
    /// warm after its last turn, from 1 to 86400. Default 900.
    pub idle_timeout_secs: u64,
    /// `scope_container_cap`: how many containers one agent may run in one
    /// scope at once, from 1 to 4096. Default 4.
    pub scope_container_cap: usize,
    /// `global_container_cap`: how many containers may run at once in all,
    /// from 1 to 4096. Default 32.
    pub global_container_cap: usize,
    /// `working_emoji`: the reaction an agent's bot puts on the message it
    /// is answering while the turn runs, and takes off after. A short name
    /// such as `eyes`, without colons. Default `hourglass_flowing_sand`.
    pub working_emoji: String,
    /// `[runner.models]`: which model a requester's plan gets. Without it,
    /// processes use the CLI's default model. A model that isn't a plain
    /// model name fails the turns that would use it.
    pub models: Option<ModelPolicy>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            claude_bin: DEFAULT_CLAUDE_BIN.to_owned(),
            turn_timeout_secs: DEFAULT_TURN_TIMEOUT_SECS,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            scope_container_cap: DEFAULT_SCOPE_CONTAINER_CAP,
            global_container_cap: DEFAULT_GLOBAL_CONTAINER_CAP,
            working_emoji: crate::pipeline::DEFAULT_WORKING_EMOJI.to_owned(),
            models: None,
        }
    }
}

impl RunnerConfig {
    /// The processes' settings: [`claude_bin`](Self::claude_bin) and
    /// [`turn_timeout_secs`](Self::turn_timeout_secs), with the credential
    /// proxy's name on the sandbox network as `ANTHROPIC_BASE_URL`.
    pub fn process(&self) -> ProcessConfig {
        ProcessConfig {
            claude_bin: self.claude_bin.clone(),
            anthropic_base_url: cred_proxy::PROXY_URL.to_owned(),
            turn_timeout_secs: self.turn_timeout_secs,
        }
    }

    /// The warm pool's settings.
    pub fn pool(&self) -> PoolConfig {
        PoolConfig {
            idle_timeout_secs: self.idle_timeout_secs,
            scope_container_cap: self.scope_container_cap,
            global_container_cap: self.global_container_cap,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let named =
            |err: runner::ConfigError| invalid(format!("runner.{}", err.key()), err.reason());
        self.process().validate().map_err(named)?;
        self.pool().validate().map_err(named)?;
        let emoji = &self.working_emoji;
        if emoji.is_empty()
            || emoji.len() > 64
            || !emoji
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_+-'".contains(&b))
        {
            return Err(invalid(
                "runner.working_emoji",
                "must be a short emoji name such as eyes: lowercase letters, digits, _, +, ' and -",
            ));
        }
        Ok(())
    }
}

/// `[agents]`. Every key has a default, so the section is optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct AgentsConfig {
    /// `max_per_owner`: how many agents that aren't deleted one member may
    /// have. `create` refuses more.
    pub max_per_owner: u32,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_per_owner: DEFAULT_MAX_PER_OWNER,
        }
    }
}

/// `[community]`. Every key has a default, so the section is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct CommunityConfig {
    /// `admins`: the community admins, each named by one surface identity
    /// in its string form, `<surface>:<team>:<user>`, such as
    /// `slack:T0123ABCD:U0456EFGH` or `rocketchat:chat.example.com:aBcD1234`.
    /// Only they may run `/agent admin …`. An identity is matched exactly,
    /// so an admin who uses two surfaces is listed once for each. Empty by
    /// default: nobody is an admin.
    #[serde(deserialize_with = "member_keys")]
    pub admins: Vec<MemberKey>,
}

/// Reads a list of member keys in their string form.
fn member_keys<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<MemberKey>, D::Error> {
    Vec::<String>::deserialize(de)?
        .iter()
        .map(|text| {
            text.parse().map_err(|_| {
                serde::de::Error::custom(
                    "not a member identity: write <surface>:<team>:<user>, such as \
                     slack:T0123ABCD:U0456EFGH",
                )
            })
        })
        .collect()
}

/// Secrets, read from the environment only.
#[derive(Debug)]
#[non_exhaustive]
pub struct Secrets {
    /// [`AGENTD_MASTER_KEY`](MASTER_KEY_VAR). Known to be a valid key.
    pub master_key: SecretString,
    /// [`AGENTD_RC_MANAGER_TOKEN`](RC_MANAGER_TOKEN_VAR), if set.
    pub rc_manager_token: Option<SecretString>,
    /// Every [`AGENTD_SLACK_MANAGER_*`](SLACK_MANAGER_PREFIX) variable, keyed
    /// by its lowercased suffix: `AGENTD_SLACK_MANAGER_SIGNING_SECRET` is
    /// `signing_secret`.
    pub slack_manager: BTreeMap<String, SecretString>,
}

impl Secrets {
    /// [`AGENTD_SLACK_MANAGER_SIGNING_SECRET`](SLACK_MANAGER_SIGNING_SECRET_VAR),
    /// if set.
    pub fn slack_manager_signing_secret(&self) -> Option<&SecretString> {
        self.slack_manager.get(SLACK_MANAGER_SIGNING_SECRET)
    }

    /// [`AGENTD_SLACK_MANAGER_BOT_TOKEN`](SLACK_MANAGER_BOT_TOKEN_VAR), if
    /// set. It is set exactly when the signing secret is.
    pub fn slack_manager_bot_token(&self) -> Option<&SecretString> {
        self.slack_manager.get(SLACK_MANAGER_BOT_TOKEN)
    }
}

/// Why the configuration couldn't be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file couldn't be read.
    #[error("can't read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The file isn't valid TOML.
    #[error("invalid TOML on line {line}: {message}")]
    Syntax {
        /// The line, counting from 1.
        line: usize,
        /// What the parser found.
        message: String,
    },
    /// A key or an environment variable has a bad value, or is missing.
    #[error("{key}: {message}")]
    Invalid {
        /// The dotted key, such as `server.listen`, or the variable name.
        key: String,
        /// What is wrong with it.
        message: String,
    },
}

impl ConfigError {
    /// The key or variable at fault, for [`ConfigError::Invalid`].
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Invalid { key, .. } => Some(key),
            Self::Read { .. } | Self::Syntax { .. } => None,
        }
    }
}

fn invalid(key: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        message: message.into(),
    }
}

impl Config {
    /// Reads the file at `path` and the secrets in `env`, and validates both.
    ///
    /// `env` is the process environment in production
    /// (`std::env::vars_os()`); variables without the `AGENTD_` prefix are
    /// ignored.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the first problem found.
    pub fn load<E, K, V>(path: &Path, env: E) -> Result<Self, ConfigError>
    where
        E: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text, env)
    }

    /// Like [`load`](Self::load), with the file's contents given as `text`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the first problem found.
    pub fn parse<E, K, V>(text: &str, env: E) -> Result<Self, ConfigError>
    where
        E: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let file = parse_file(text)?;
        file.validate()?;
        let (secrets, unknown_env) = Secrets::from_env(env)?;
        if file.rocketchat.is_some() && secrets.rc_manager_token.is_none() {
            return Err(invalid(
                RC_MANAGER_TOKEN_VAR,
                "must be set when [rocketchat] is configured",
            ));
        }
        Ok(Self {
            server: file.server,
            internal: file.internal,
            store: file.store,
            limits: file.limits,
            proxy: file.proxy,
            sandbox: file.sandbox,
            runner: file.runner,
            agents: file.agents,
            community: file.community,
            claude_oauth: file.claude_oauth,
            rocketchat: file.rocketchat,
            slack: file.slack,
            secrets,
            unknown_env,
        })
    }

    /// Builds the egress proxy from `[proxy]`: its rules and tunnel caps,
    /// with agentd's listener addresses and the sandbox network out of
    /// reach.
    ///
    /// # Errors
    ///
    /// Never for a loaded `Config`, whose `[proxy]` section was checked the
    /// same way.
    pub fn egress_proxy(&self) -> Result<EgressProxy, ConfigError> {
        let limits = EgressLimits {
            max_tunnels: self.proxy.max_tunnels,
            max_session_tunnels: self.proxy.max_session_tunnels,
            ..EgressLimits::default()
        };
        Ok(EgressProxy::new(self.egress_policy()?).with_limits(limits))
    }

    /// The egress policy from `[proxy]`: its rules, with agentd's listener
    /// addresses and the sandbox network out of reach. Cloning a skill's
    /// repository checks the Git host's addresses against it too.
    ///
    /// # Errors
    ///
    /// Never for a loaded `Config`, whose `[proxy]` section was checked the
    /// same way.
    pub fn egress_policy(&self) -> Result<EgressPolicy, ConfigError> {
        egress_policy(&self.proxy, &self.server, &self.internal)
    }

    /// Builds the store's [`Sealer`] from the master key.
    ///
    /// # Errors
    ///
    /// Never for a loaded `Config`, whose key was checked; the `Result` names
    /// the variable if that ever changes.
    pub fn sealer(&self) -> Result<Sealer, ConfigError> {
        Sealer::from_base64(&self.secrets.master_key)
            .map_err(|err| invalid(MASTER_KEY_VAR, err.to_string()))
    }
}

fn egress_policy(
    proxy: &ProxyConfig,
    server: &ServerConfig,
    internal: &InternalConfig,
) -> Result<EgressPolicy, ConfigError> {
    let mut own = vec![internal.sandbox_subnet];
    for (key, addr) in [
        ("server.listen", server.listen),
        ("internal.proxy_listen", internal.proxy_listen),
        ("internal.ctl_listen", internal.ctl_listen),
    ] {
        let ip = addr.ip().to_canonical();
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        own.push(Cidr::new(ip, prefix).map_err(|err| invalid(key, err.to_string()))?);
    }
    Ok(EgressPolicy::new(proxy.allow.clone(), own))
}

fn default_drain_timeout_secs() -> u64 {
    DEFAULT_DRAIN_TIMEOUT_SECS
}

fn default_log_filter() -> String {
    DEFAULT_LOG_FILTER.to_owned()
}

fn parse_file(text: &str) -> Result<File, ConfigError> {
    let deserializer = toml::Deserializer::parse(text).map_err(|err| ConfigError::Syntax {
        line: line_of(text, err.span()),
        message: err.message().to_owned(),
    })?;
    serde_path_to_error::deserialize(deserializer).map_err(|err| {
        let inner = err.inner();
        let key = dotted_key(err.path(), inner.message());
        let line = line_of(text, inner.span());
        invalid(key, format!("{} (line {line})", inner.message()))
    })
}

/// The 1-based line holding the start of `span`, or 1 without one.
fn line_of(text: &str, span: Option<std::ops::Range<usize>>) -> usize {
    let start = span.map_or(0, |span| span.start.min(text.len()));
    text.as_bytes()[..start]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// The dotted key an error is about. serde reports a missing field at its
/// parent, so the field's name, which serde's message quotes, is appended.
fn dotted_key(path: &serde_path_to_error::Path, message: &str) -> String {
    let mut parts: Vec<String> = path
        .iter()
        .filter_map(|segment| match segment {
            Segment::Map { key } => Some(key.clone()),
            Segment::Seq { index } => Some(index.to_string()),
            Segment::Enum { variant } => Some(variant.clone()),
            Segment::Unknown => None,
        })
        .collect();
    if let Some(field) = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split_once('`'))
        .map(|(field, _)| field)
    {
        parts.push(field.to_owned());
    }
    if parts.is_empty() {
        "(file)".to_owned()
    } else {
        parts.join(".")
    }
}

impl File {
    fn validate(&self) -> Result<(), ConfigError> {
        let listeners = [
            ("server.listen", self.server.listen),
            ("internal.proxy_listen", self.internal.proxy_listen),
            ("internal.ctl_listen", self.internal.ctl_listen),
        ];
        for (i, (key, addr)) in listeners.iter().enumerate() {
            if canonical(*addr).ip().is_unspecified() {
                return Err(invalid(
                    *key,
                    format!(
                        "{addr} listens on every interface; give agentd's own address on the \
                         network this listener serves"
                    ),
                ));
            }
            if addr.port() != 0
                && let Some((other, _)) = listeners[..i]
                    .iter()
                    .find(|(_, a)| canonical(*a) == canonical(*addr))
            {
                return Err(invalid(*key, format!("{addr} is already used by {other}")));
            }
        }
        let subnet = self.internal.sandbox_subnet;
        if subnet.contains(self.server.listen.ip()) {
            return Err(invalid(
                "server.listen",
                format!(
                    "{} is inside internal.sandbox_subnet ({subnet}); the public listener must \
                     not be on the sandbox network",
                    self.server.listen.ip(),
                ),
            ));
        }
        for (key, addr) in &listeners[1..] {
            if !subnet.contains(addr.ip()) {
                return Err(invalid(
                    *key,
                    format!(
                        "{} is outside internal.sandbox_subnet ({subnet}); this listener serves \
                         sandboxes only, so give agentd's own address on the sandbox network",
                        addr.ip(),
                    ),
                ));
            }
        }
        if self.server.drain_timeout_secs > MAX_DRAIN_TIMEOUT_SECS {
            return Err(invalid(
                "server.drain_timeout_secs",
                format!("must be at most {MAX_DRAIN_TIMEOUT_SECS}"),
            ));
        }
        EnvFilter::try_new(&self.server.log_filter)
            .map_err(|err| invalid("server.log_filter", err.to_string()))?;
        if !self.store.url.starts_with("sqlite:") {
            return Err(invalid(
                "store.url",
                "must be a sqlite: URL, such as sqlite:///var/lib/agentd/agentd.db",
            ));
        }
        if !self.store.data_dir.is_absolute() {
            return Err(invalid(
                "store.data_dir",
                "must be an absolute path, such as /var/lib/agentd",
            ));
        }
        if self.limits.attach_max_bytes == 0 {
            return Err(invalid("limits.attach_max_bytes", "must be at least 1"));
        }
        if self.proxy.max_tunnels == 0 {
            return Err(invalid("proxy.max_tunnels", "must be at least 1"));
        }
        if self.proxy.max_session_tunnels == 0 {
            return Err(invalid("proxy.max_session_tunnels", "must be at least 1"));
        }
        if self.proxy.max_session_tunnels > self.proxy.max_tunnels {
            return Err(invalid(
                "proxy.max_session_tunnels",
                "must be at most proxy.max_tunnels",
            ));
        }
        egress_policy(&self.proxy, &self.server, &self.internal)?;
        cred_proxy::check_upstream(&self.proxy.upstream).map_err(|_| {
            invalid(
                "proxy.upstream",
                "must be an https:// URL, or http:// to a loopback IP address, with no user \
                 info, query or fragment",
            )
        })?;
        if let Some(sandbox) = &self.sandbox {
            sandbox
                .validate()
                .map_err(|err| invalid(format!("sandbox.{}", err.key), err.message))?;
            for (key, addr, url) in [
                (
                    "internal.proxy_listen",
                    self.internal.proxy_listen,
                    cred_proxy::PROXY_URL,
                ),
                (
                    "internal.ctl_listen",
                    self.internal.ctl_listen,
                    crate::pipeline::AGENTCTL_URL,
                ),
            ] {
                if url_port(url) != Some(addr.port()) {
                    return Err(invalid(
                        key,
                        format!(
                            "{addr} must use the port of {url} with [sandbox]: sandboxes reach \
                             this listener there, and only that port is let through"
                        ),
                    ));
                }
            }
        }
        self.runner.validate()?;
        if self.agents.max_per_owner == 0 {
            return Err(invalid("agents.max_per_owner", "must be at least 1"));
        }
        self.claude_oauth
            .validate()
            .map_err(|err| invalid(format!("claude_oauth.{}", err.key), err.reason))?;
        if let Some(rocketchat) = &self.rocketchat {
            rocketchat.validate()?;
        }
        self.slack.validate()
    }
}

impl RocketChatConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        surface_rocketchat::realtime::websocket_url(&self.base_url).map_err(|_| {
            invalid(
                "rocketchat.base_url",
                "must be an http:// or https:// URL, such as https://chat.example.com",
            )
        })?;
        if let Some(url) = &self.websocket_url
            && !(url.starts_with("wss://") || url.starts_with("ws://"))
        {
            return Err(invalid(
                "rocketchat.websocket_url",
                "must be a ws:// or wss:// URL",
            ));
        }
        if let Some(url) = &self.avatar_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(invalid(
                "rocketchat.avatar_url",
                "must be an http:// or https:// URL",
            ));
        }
        for (key, value) in [
            ("rocketchat.team", &self.team),
            ("rocketchat.manager_user_id", &self.manager_user_id),
        ] {
            if value.trim().is_empty() {
                return Err(invalid(key, "must not be empty"));
            }
        }
        Ok(())
    }
}

/// The port of `url`, an `http://host:port` URL such as
/// [`cred_proxy::PROXY_URL`].
fn url_port(url: &str) -> Option<u16> {
    url.rsplit_once(':')?.1.parse().ok()
}

/// `addr` with an IPv4-mapped IPv6 address replaced by its IPv4 address, so
/// `[::ffff:0.0.0.0]:8080` is checked as the `0.0.0.0:8080` it binds.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

impl Secrets {
    /// The secrets in `env`, and the unknown `AGENTD_` variables it ignored.
    fn from_env<E, K, V>(env: E) -> Result<(Self, Vec<String>), ConfigError>
    where
        E: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut master_key = None;
        let mut rc_manager_token = None;
        let mut slack_manager = BTreeMap::new();
        let mut unknown = Vec::new();
        for (name, value) in env {
            let name: OsString = name.into();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(ENV_PREFIX) {
                continue;
            }
            if name == MASTER_KEY_VAR {
                master_key = Some(secret(name, value.into())?);
            } else if name == RC_MANAGER_TOKEN_VAR {
                rc_manager_token = Some(secret(name, value.into())?);
            } else if is_service_link(name) {
                continue;
            } else if let Some(suffix) = name.strip_prefix(SLACK_MANAGER_PREFIX)
                && !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                slack_manager.insert(suffix.to_ascii_lowercase(), secret(name, value.into())?);
            } else if let Some(known) = near_miss(name) {
                return Err(invalid(
                    name,
                    format!(
                        "unknown variable, and too close to {known} to ignore; agentd reads \
                         only {MASTER_KEY_VAR}, {RC_MANAGER_TOKEN_VAR} and \
                         {SLACK_MANAGER_PREFIX}<NAME> (uppercase letters, digits and _) from \
                         the environment, and everything else from the config file"
                    ),
                ));
            } else {
                unknown.push(name.to_owned());
            }
        }
        unknown.sort();
        let master_key = master_key.ok_or_else(|| {
            invalid(
                MASTER_KEY_VAR,
                "is not set; generate a key with `agentd gen-key`",
            )
        })?;
        Sealer::from_base64(&master_key).map_err(|err| invalid(MASTER_KEY_VAR, err.to_string()))?;
        if !slack_manager.is_empty() && !slack_manager.contains_key(SLACK_MANAGER_SIGNING_SECRET) {
            return Err(invalid(
                SLACK_MANAGER_SIGNING_SECRET_VAR,
                format!(
                    "is not set, but other {SLACK_MANAGER_PREFIX}* variables are; the manager \
                     app's requests can't be verified without it (is one of them misspelled?)"
                ),
            ));
        }
        if !slack_manager.is_empty() && !slack_manager.contains_key(SLACK_MANAGER_BOT_TOKEN) {
            return Err(invalid(
                SLACK_MANAGER_BOT_TOKEN_VAR,
                format!(
                    "is not set, but {SLACK_MANAGER_SIGNING_SECRET_VAR} is; the manager app \
                     answers commands with its bot token (is it misspelled?)"
                ),
            ));
        }
        Ok((
            Self {
                master_key,
                rc_manager_token,
                slack_manager,
            },
            unknown,
        ))
    }
}

/// Whether `name` is one of the variables Kubernetes sets for a Service
/// ("service links"): `<SVC>_SERVICE_HOST`, `<SVC>_SERVICE_PORT`,
/// `<SVC>_SERVICE_PORT_<PORT_NAME>`, `<SVC>_PORT`, and
/// `<SVC>_PORT_<number>_<protocol>` with its `_PROTO`, `_PORT` and `_ADDR`
/// variants.
fn is_service_link(name: &str) -> bool {
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let is_protocol = |part: &str| matches!(part, "TCP" | "UDP" | "SCTP");
    let parts: Vec<&str> = name.split('_').collect();
    let port_protocol = match parts.as_slice() {
        [_, .., "PORT", number, protocol] | [_, .., "PORT", number, protocol, "PROTO" | "ADDR"] => {
            is_number(number) && is_protocol(protocol)
        }
        _ => false,
    };
    port_protocol
        || name.contains("_SERVICE_PORT_")
        || matches!(
            parts.as_slice(),
            [_, .., "SERVICE", "HOST" | "PORT"] | [_, .., "PORT"]
        )
}

/// The secret `name` looks like a misspelling of, as the [module
/// docs](self) define it, or `None`.
fn near_miss(name: &str) -> Option<&'static str> {
    [MASTER_KEY_VAR, RC_MANAGER_TOKEN_VAR]
        .into_iter()
        .find(|known| edit_distances(known, name).last() <= Some(&NEAR_MISS_EDITS))
        .or_else(|| {
            let closest = edit_distances(SLACK_MANAGER_PREFIX, name).into_iter().min();
            (closest <= Some(NEAR_MISS_EDITS)).then_some("AGENTD_SLACK_MANAGER_<NAME>")
        })
}

/// The edit (Levenshtein) distance from `target` to each prefix of `name`,
/// from the empty prefix to the whole of `name`, counting bytes.
fn edit_distances(target: &str, name: &str) -> Vec<usize> {
    let name = name.as_bytes();
    let mut row: Vec<usize> = (0..=name.len()).collect();
    for (i, t) in target.bytes().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for j in 1..=name.len() {
            let above = row[j];
            row[j] = (diagonal + usize::from(name[j - 1] != t))
                .min(above + 1)
                .min(row[j - 1] + 1);
            diagonal = above;
        }
    }
    row
}

fn secret(name: &str, value: OsString) -> Result<SecretString, ConfigError> {
    let value = value
        .into_string()
        .map_err(|_| invalid(name, "is not valid UTF-8"))?;
    if value.trim().is_empty() {
        return Err(invalid(name, "is set but empty"));
    }
    if value.trim() != value {
        return Err(invalid(
            name,
            "starts or ends with white space, such as the trailing newline of a mounted \
             file; remove it",
        ));
    }
    Ok(SecretString::from(value))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use secrecy::ExposeSecret;

    use super::*;

    pub(crate) const MINIMAL: &str = r#"
[server]
listen = "127.0.0.1:0"

[internal]
proxy_listen = "127.0.0.2:0"
ctl_listen = "127.0.0.2:0"
sandbox_subnet = "127.0.0.2/32"

[store]
url = "sqlite::memory:"
data_dir = "/nonexistent/agentd"
"#;

    /// [`MINIMAL`] with the ports sandboxes reach, as `[sandbox]` requires.
    pub(crate) fn sandboxed() -> String {
        MINIMAL
            .replacen(
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"127.0.0.2:8080\"",
                1,
            )
            .replacen(
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"127.0.0.2:8081\"",
                1,
            )
    }

    pub(crate) fn key() -> String {
        Sealer::generate_key().unwrap().expose_secret().to_owned()
    }

    pub(crate) fn env() -> Vec<(String, String)> {
        vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            (MASTER_KEY_VAR.to_owned(), key()),
        ]
    }

    fn with(text: &str, env: Vec<(String, String)>) -> Result<Config, ConfigError> {
        Config::parse(text, env)
    }

    fn file_err(text: &str) -> ConfigError {
        with(text, env()).unwrap_err()
    }

    fn env_err(extra: &[(&str, &str)]) -> ConfigError {
        let mut env = env();
        env.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        with(MINIMAL, env).unwrap_err()
    }

    fn replace(from: &str, to: &str) -> String {
        assert!(MINIMAL.contains(from), "{from}");
        MINIMAL.replacen(from, to, 1)
    }

    #[test]
    fn a_minimal_file_loads_with_defaults() {
        let config = with(MINIMAL, env()).unwrap();
        assert_eq!(
            config.server.listen,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        );
        assert_eq!(config.server.drain_timeout(), Duration::from_secs(30));
        assert_eq!(config.server.log_filter, "info");
        assert_eq!(
            config.internal.sandbox_subnet,
            "127.0.0.2/32".parse().unwrap()
        );
        assert_eq!(config.store.url, "sqlite::memory:");
        assert_eq!(config.store.data_dir, Path::new("/nonexistent/agentd"));
        assert_eq!(config.limits.attach_max_bytes, 50 * 1024 * 1024);
        assert_eq!(config.agents.max_per_owner, 10);
        assert!(config.secrets.rc_manager_token.is_none());
        assert!(config.secrets.slack_manager.is_empty());
        assert!(config.unknown_env.is_empty());
        assert!(config.proxy.allow.is_empty());
        assert_eq!(config.proxy.upstream, "https://api.anthropic.com");
        assert!(config.sandbox.is_none());
        let process = config.runner.process();
        assert_eq!(process, runner::ProcessConfig::default());
        assert_eq!(config.runner.pool(), runner::PoolConfig::default());
        let egress = config.egress_proxy().unwrap();
        assert!(egress.policy().rules().is_empty());
        assert_eq!(egress.limits(), cred_proxy::EgressLimits::default());
        config.sealer().unwrap();
    }

    const DOCKER_ADDRESSES: &str = r#"
[server]
listen = "172.31.0.2:8443"

[internal]
proxy_listen = "172.30.0.2:8080"
ctl_listen = "172.30.0.2:8081"
sandbox_subnet = "172.30.0.0/24"

[store]
url = "sqlite::memory:"
data_dir = "/nonexistent/agentd"

[proxy]
"#;

    #[test]
    fn proxy_rules_and_tunnel_caps_load() {
        let text = format!(
            "{DOCKER_ADDRESSES}allow = [\"GitHub.com\", \"*.example.com:8443\"]\n\
             max_tunnels = 100\nmax_session_tunnels = 100\n"
        );
        let config = with(&text, env()).unwrap();
        let rules: Vec<String> = config.proxy.allow.iter().map(ToString::to_string).collect();
        assert_eq!(rules, ["github.com", "*.example.com:8443"]);
        let egress = config.egress_proxy().unwrap();
        assert_eq!(egress.policy().rules().len(), 2);
        let limits = egress.limits();
        assert_eq!((limits.max_tunnels, limits.max_session_tunnels), (100, 100));
        assert_eq!(
            limits.tunnel_lifetime,
            cred_proxy::EgressLimits::default().tunnel_lifetime
        );
    }

    #[test]
    fn proxy_errors_name_the_entry() {
        for (proxy, key, message) in [
            (
                "allow = [\"github.com\", \"1.2.3.4\"]",
                "proxy.allow.1",
                "IP addresses are not allowed",
            ),
            ("allow = [\"*.com\"]", "proxy.allow.0", "two or more labels"),
            (
                "allow = [\"github.com\", \"API.anthropic.com\"]",
                "proxy.allow.1",
                "always denied",
            ),
            (
                "allow_private = [\"10.0.0.0/8\"]",
                "proxy.allow_private",
                "unknown field",
            ),
            ("max_tunnels = 0", "proxy.max_tunnels", "at least 1"),
            (
                "max_session_tunnels = 0",
                "proxy.max_session_tunnels",
                "at least 1",
            ),
            (
                "max_tunnels = 8\nmax_session_tunnels = 9",
                "proxy.max_session_tunnels",
                "at most proxy.max_tunnels",
            ),
            ("max_tunnels = -1", "proxy.max_tunnels", "invalid value"),
            ("deny = []", "proxy.deny", "unknown field"),
        ] {
            let err = file_err(&format!("{DOCKER_ADDRESSES}{proxy}\n"));
            assert_eq!(err.key(), Some(key), "{proxy}: {err}");
            assert!(err.to_string().contains(message), "{proxy}: {err}");
        }
    }

    #[test]
    fn the_sandbox_section_is_the_sandbox_crates_config() {
        let base = sandboxed();
        let config = with(
            &format!(
                "{base}\n[sandbox]\nimage = \"agent-core/sandbox:dev\"\nnetwork = \"sbx\"\n\
                 host_data_dir = \"/srv/agentd\"\n"
            ),
            env(),
        )
        .unwrap();
        let sandbox = config.sandbox.unwrap();
        assert_eq!(sandbox.image, "agent-core/sandbox:dev");
        assert_eq!(sandbox.network, "sbx");
        assert_eq!(sandbox.host_data_dir, Some(PathBuf::from("/srv/agentd")));
        assert_eq!(sandbox.uid, sandbox::DEFAULT_SANDBOX_UID);

        for (section, key, message) in [
            ("network = \"x\"", "sandbox.image", "missing field"),
            ("image = \" \"", "sandbox.image", "must not be empty"),
            (
                "image = \"i\"\nnetwork = \"bridge\"",
                "sandbox.network",
                "internal sandbox network",
            ),
            ("image = \"i\"\nuid = 0", "sandbox.uid", "never run as root"),
            ("image = \"i\"\nbogus = 1", "sandbox.bogus", "unknown field"),
        ] {
            let err = file_err(&format!("{base}\n[sandbox]\n{section}\n"));
            assert_eq!(err.key(), Some(key), "{section}: {err}");
            assert!(err.to_string().contains(message), "{section}: {err}");
        }
    }

    #[test]
    fn with_a_sandbox_the_internal_listeners_use_the_ports_sandboxes_reach() {
        assert_eq!(url_port(cred_proxy::PROXY_URL), Some(8080));
        assert_eq!(url_port(crate::pipeline::AGENTCTL_URL), Some(8081));
        let section = "\n[sandbox]\nimage = \"i\"\n";
        with(&format!("{}{section}", sandboxed()), env()).unwrap();
        with(MINIMAL, env()).unwrap();
        for (from, to, key, url) in [
            (
                "proxy_listen = \"127.0.0.2:8080\"",
                "proxy_listen = \"127.0.0.2:0\"",
                "internal.proxy_listen",
                cred_proxy::PROXY_URL,
            ),
            (
                "proxy_listen = \"127.0.0.2:8080\"",
                "proxy_listen = \"127.0.0.2:9090\"",
                "internal.proxy_listen",
                cred_proxy::PROXY_URL,
            ),
            (
                "ctl_listen = \"127.0.0.2:8081\"",
                "ctl_listen = \"127.0.0.2:0\"",
                "internal.ctl_listen",
                crate::pipeline::AGENTCTL_URL,
            ),
            (
                "ctl_listen = \"127.0.0.2:8081\"",
                "ctl_listen = \"127.0.0.2:8082\"",
                "internal.ctl_listen",
                crate::pipeline::AGENTCTL_URL,
            ),
        ] {
            let text = sandboxed().replacen(from, to, 1);
            with(&text, env()).unwrap();
            let err = file_err(&format!("{text}{section}"));
            assert_eq!(err.key(), Some(key), "{to}: {err}");
            assert!(err.to_string().contains(url), "{to}: {err}");
        }
    }

    #[test]
    fn the_runner_section_sets_the_processes_and_the_pool() {
        let config = with(
            &format!(
                "{MINIMAL}\n[runner]\nclaude_bin = \"/opt/claude\"\nturn_timeout_secs = 60\n\
                 idle_timeout_secs = 120\nscope_container_cap = 2\nglobal_container_cap = 8\n"
            ),
            env(),
        )
        .unwrap();
        let process = config.runner.process();
        assert_eq!(process.claude_bin, "/opt/claude");
        assert_eq!(process.anthropic_base_url, cred_proxy::PROXY_URL);
        assert_eq!(process.turn_timeout(), Duration::from_secs(60));
        let pool = config.runner.pool();
        assert_eq!(pool.idle_timeout(), Duration::from_secs(120));
        assert_eq!(
            (pool.scope_container_cap, pool.global_container_cap),
            (2, 8)
        );
        assert_eq!(config.runner.working_emoji, "hourglass_flowing_sand");
        assert!(config.runner.models.is_none());

        let config = with(
            &format!(
                "{MINIMAL}\n[runner]\nworking_emoji = \"eyes\"\n[runner.models]\n\
                 default = \"m1\"\nplans = {{ claude_max = \"m2\" }}\n"
            ),
            env(),
        )
        .unwrap();
        assert_eq!(config.runner.working_emoji, "eyes");
        let models = config.runner.models.unwrap();
        assert_eq!(models.model_for(Some("claude_max")), "m2");
        assert_eq!(models.model_for(None), "m1");

        for (section, key) in [
            ("working_emoji = \":eyes:\"", "runner.working_emoji"),
            ("working_emoji = \"\"", "runner.working_emoji"),
            ("[runner.models]\nplans = {}", "runner.models.default"),
            ("claude_bin = \"-x\"", "runner.claude_bin"),
            ("turn_timeout_secs = 0", "runner.turn_timeout_secs"),
            ("idle_timeout_secs = 0", "runner.idle_timeout_secs"),
            ("scope_container_cap = 0", "runner.scope_container_cap"),
            ("global_container_cap = 5000", "runner.global_container_cap"),
            (
                "anthropic_base_url = \"http://x\"",
                "runner.anthropic_base_url",
            ),
        ] {
            let err = file_err(&format!("{MINIMAL}\n[runner]\n{section}\n"));
            assert_eq!(err.key(), Some(key), "{section}: {err}");
        }
    }

    #[test]
    fn the_proxy_upstream_is_configurable_and_checked() {
        for good in [
            "http://127.0.0.1:9",
            "http://[::1]:9/anthropic",
            "https://llm-gateway.example.com",
        ] {
            let config = with(
                &format!("{MINIMAL}\n[proxy]\nupstream = \"{good}\"\n"),
                env(),
            )
            .unwrap();
            assert_eq!(config.proxy.upstream, good);
        }
        for bad in [
            "ftp://x",
            "https://u:p@x",
            "https://x/?q=1",
            "nope",
            "http://api.anthropic.com",
            "http://localhost:9",
            "http://10.0.0.1:9",
        ] {
            let err = file_err(&format!("{MINIMAL}\n[proxy]\nupstream = \"{bad}\"\n"));
            assert_eq!(err.key(), Some("proxy.upstream"), "{bad}: {err}");
            assert!(!err.to_string().contains(bad), "{bad}: {err}");
        }
    }

    #[test]
    fn the_example_file_loads() {
        let text = include_str!("../../../config/agentd.example.toml");
        let config = with(text, with_rc_token()).unwrap();
        assert_eq!(config.server.listen.port(), 8443);
        assert_eq!(config.internal.proxy_listen.port(), 8080);
        assert_eq!(config.internal.ctl_listen.port(), 8081);
        assert_eq!(config.claude_oauth, OAuthConfig::default());
        assert_eq!(config.rocketchat.unwrap().team, "chat.example.com");
        assert_eq!(config.proxy.upstream, DEFAULT_UPSTREAM);
        let sandbox = config.sandbox.unwrap();
        assert_eq!(sandbox, SandboxConfig::new("agent-core/sandbox:dev"));
        assert_eq!(config.runner.process(), ProcessConfig::default());
        assert_eq!(config.runner.pool(), PoolConfig::default());
    }

    #[test]
    fn secrets_come_from_the_environment() {
        let mut env = env();
        env.push((RC_MANAGER_TOKEN_VAR.to_owned(), "rc-token".to_owned()));
        env.push((
            "AGENTD_SLACK_MANAGER_SIGNING_SECRET".to_owned(),
            "sig-value".to_owned(),
        ));
        env.push((
            "AGENTD_SLACK_MANAGER_CLIENT_SECRET".to_owned(),
            "cli-value".to_owned(),
        ));
        env.push((
            SLACK_MANAGER_BOT_TOKEN_VAR.to_owned(),
            "bot-value".to_owned(),
        ));
        let config = with(MINIMAL, env).unwrap();
        assert_eq!(
            config
                .secrets
                .rc_manager_token
                .as_ref()
                .unwrap()
                .expose_secret(),
            "rc-token"
        );
        let slack: Vec<_> = config
            .secrets
            .slack_manager
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose_secret()))
            .collect();
        assert_eq!(
            slack,
            [
                ("bot_token", "bot-value"),
                ("client_secret", "cli-value"),
                ("signing_secret", "sig-value")
            ]
        );
        let debug = format!("{config:?}");
        for secret in ["rc-token", "sig-value", "cli-value", "bot-value"] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }

    const ROCKETCHAT: &str = r#"
[rocketchat]
base_url = "https://chat.example.com"
team = "chat.example.com"
manager_user_id = "manager-id"
"#;

    fn with_rc_token() -> Vec<(String, String)> {
        let mut env = env();
        env.push((RC_MANAGER_TOKEN_VAR.to_owned(), "rc-token".to_owned()));
        env
    }

    #[test]
    fn rocketchat_is_optional_and_loads_with_its_token() {
        assert!(with(MINIMAL, env()).unwrap().rocketchat.is_none());
        let config = with(&format!("{MINIMAL}{ROCKETCHAT}"), with_rc_token()).unwrap();
        let rocketchat = config.rocketchat.unwrap();
        assert_eq!(rocketchat.base_url, "https://chat.example.com");
        assert_eq!(rocketchat.websocket_url, None);
        assert_eq!(rocketchat.team, "chat.example.com");
        assert_eq!(rocketchat.manager_user_id, "manager-id");
        assert_eq!(rocketchat.avatar_url, None);
        let text = format!(
            "{MINIMAL}{}",
            ROCKETCHAT.replacen(
                "team =",
                "avatar_url = \"https://img.example/a.png\"\nteam =",
                1
            )
        );
        let config = with(&text, with_rc_token()).unwrap();
        assert_eq!(
            config.rocketchat.unwrap().avatar_url.as_deref(),
            Some("https://img.example/a.png")
        );
    }

    #[test]
    fn rocketchat_needs_the_manager_token() {
        let err = with(&format!("{MINIMAL}{ROCKETCHAT}"), env()).unwrap_err();
        assert_eq!(err.key(), Some(RC_MANAGER_TOKEN_VAR), "{err}");
    }

    #[test]
    fn rocketchat_values_are_checked() {
        for (from, to, key) in [
            (
                "https://chat.example.com",
                "ftp://chat.example.com",
                "rocketchat.base_url",
            ),
            (
                "team = \"chat.example.com\"",
                "team = \" \"",
                "rocketchat.team",
            ),
            (
                "manager_user_id = \"manager-id\"",
                "manager_user_id = \"\"",
                "rocketchat.manager_user_id",
            ),
            (
                "team =",
                "websocket_url = \"https://chat.example.com/websocket\"\nteam =",
                "rocketchat.websocket_url",
            ),
            (
                "team =",
                "avatar_url = \"file:///etc/passwd\"\nteam =",
                "rocketchat.avatar_url",
            ),
        ] {
            let text = format!("{MINIMAL}{}", ROCKETCHAT.replacen(from, to, 1));
            let err = with(&text, with_rc_token()).unwrap_err();
            assert_eq!(err.key(), Some(key), "{err}");
        }
        let text = format!(
            "{MINIMAL}{}",
            ROCKETCHAT.replacen(
                "team =",
                "websocket_url = \"wss://rt.example.com/websocket\"\nteam =",
                1
            )
        );
        let config = with(&text, with_rc_token()).unwrap();
        assert_eq!(
            config.rocketchat.unwrap().websocket_url.as_deref(),
            Some("wss://rt.example.com/websocket")
        );
    }

    #[test]
    fn claude_oauth_defaults_and_is_checked() {
        let config = with(MINIMAL, env()).unwrap();
        assert_eq!(config.claude_oauth, OAuthConfig::default());
        let text = format!("{MINIMAL}\n[claude_oauth]\ntoken_url = \"http://example.com/token\"\n");
        let err = with(&text, env()).unwrap_err();
        assert_eq!(err.key(), Some("claude_oauth.token_url"), "{err}");
        let text = format!("{MINIMAL}\n[claude_oauth]\nbogus = 1\n");
        let err = with(&text, env()).unwrap_err();
        assert_eq!(err.key(), Some("claude_oauth.bogus"), "{err}");
    }

    /// The environment with both Slack manager secrets.
    fn with_slack_manager() -> Vec<(String, String)> {
        let mut env = env();
        env.push((
            SLACK_MANAGER_SIGNING_SECRET_VAR.to_owned(),
            "sig-value".to_owned(),
        ));
        env.push((
            SLACK_MANAGER_BOT_TOKEN_VAR.to_owned(),
            "bot-value".to_owned(),
        ));
        env
    }

    #[test]
    fn the_slack_manager_secrets_are_read_by_name() {
        let config = with(MINIMAL, env()).unwrap();
        assert!(config.secrets.slack_manager_signing_secret().is_none());
        assert!(config.secrets.slack_manager_bot_token().is_none());
        let config = with(MINIMAL, with_slack_manager()).unwrap();
        assert_eq!(
            config
                .secrets
                .slack_manager_signing_secret()
                .map(ExposeSecret::expose_secret),
            Some("sig-value")
        );
        assert_eq!(
            config
                .secrets
                .slack_manager_bot_token()
                .map(ExposeSecret::expose_secret),
            Some("bot-value")
        );
    }

    #[test]
    fn the_signing_secret_needs_the_bot_token() {
        for name in [
            SLACK_MANAGER_SIGNING_SECRET_VAR,
            "AGENTD_SLACK_MANAGER_BOT_TOKN",
        ] {
            let mut extra = vec![(SLACK_MANAGER_SIGNING_SECRET_VAR, "sig-value")];
            if name != SLACK_MANAGER_SIGNING_SECRET_VAR {
                extra.push((name, "bot-value"));
            }
            let err = env_err(&extra);
            assert_eq!(err.key(), Some(SLACK_MANAGER_BOT_TOKEN_VAR), "{err}");
            assert!(err.to_string().contains("misspelled"), "{err}");
            assert!(!err.to_string().contains("value"), "{err}");
        }
    }

    #[test]
    fn slack_api_url_defaults_and_is_checked() {
        let config = with(MINIMAL, env()).unwrap();
        assert_eq!(config.slack.api_url, "https://slack.com/api/");
        let text = format!("{MINIMAL}\n[slack]\napi_url = \"http://127.0.0.1:9/api/\"\n");
        let config = with(&text, env()).unwrap();
        assert_eq!(config.slack.api_url, "http://127.0.0.1:9/api/");
        for bad in [
            "ftp://slack.com/api/",
            "https://slack.com/api/?x=1",
            "not a url",
        ] {
            let text = format!("{MINIMAL}\n[slack]\napi_url = \"{bad}\"\n");
            let err = with(&text, env()).unwrap_err();
            assert_eq!(err.key(), Some("slack.api_url"), "{err}");
        }
        let err = file_err(&format!("{MINIMAL}\n[slack]\nbogus = 1\n"));
        assert!(err.key().unwrap().starts_with("slack"), "{err}");
    }

    #[test]
    fn slack_agent_app_keys_default_and_are_checked() {
        let config = with(MINIMAL, env()).unwrap();
        assert_eq!(config.slack.public_url(), None);
        assert!(!config.slack.public_posting);
        assert_eq!(config.slack.install_reminder(), Duration::from_secs(3600));
        let text = format!(
            "{MINIMAL}\n[slack]\npublic_url = \"https://agentd.example.com/\"\n\
             public_posting = true\ninstall_reminder_secs = 60\n"
        );
        let config = with(&text, env()).unwrap();
        assert_eq!(
            config.slack.public_url().as_deref(),
            Some("https://agentd.example.com")
        );
        assert!(config.slack.public_posting);
        assert_eq!(config.slack.install_reminder(), Duration::from_secs(60));
        for (key, value) in [
            ("public_url", "\"http://agentd.example.com\""),
            ("public_url", "\"https://agentd.example.com/?x=1\""),
            ("install_reminder_secs", "59"),
            ("install_reminder_secs", "604801"),
        ] {
            let text = format!("{MINIMAL}\n[slack]\n{key} = {value}\n");
            let err = with(&text, env()).unwrap_err();
            assert_eq!(err.key(), Some(format!("slack.{key}").as_str()), "{err}");
        }
    }

    #[test]
    fn other_slack_manager_secrets_need_the_signing_secret() {
        for name in [
            "AGENTD_SLACK_MANAGER_BOT_TOKEN",
            "AGENTD_SLACK_MANAGER_SIGNNG_SECRET",
        ] {
            let err = env_err(&[(name, "value")]);
            assert_eq!(err.key(), Some(SLACK_MANAGER_SIGNING_SECRET_VAR), "{err}");
            assert!(err.to_string().contains("misspelled"), "{err}");
            assert!(!err.to_string().contains("value"), "{err}");
        }
    }

    #[test]
    fn the_environment_is_accepted_as_os_strings() {
        let env: Vec<(OsString, OsString)> = env()
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Config::parse(MINIMAL, env).unwrap();
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let path = Path::new("/nonexistent/agentd.toml");
        let err = Config::load(path, env()).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err:?}");
        assert!(
            err.to_string().contains("/nonexistent/agentd.toml"),
            "{err}"
        );
        assert_eq!(err.key(), None);
    }

    #[test]
    fn invalid_toml_names_the_line() {
        let err = file_err("[server]\nlisten = \"127.0.0.1:0\"\nbroken =\n");
        assert!(
            matches!(err, ConfigError::Syntax { line: 3, .. }),
            "{err:?}"
        );
        assert!(
            err.to_string().starts_with("invalid TOML on line 3: "),
            "{err}"
        );
        assert_eq!(err.key(), None);
    }

    #[test]
    fn a_missing_key_is_named() {
        let err = file_err(&replace("listen = \"127.0.0.1:0\"\n", ""));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
        let err = file_err(&replace("sandbox_subnet = \"127.0.0.2/32\"\n", ""));
        assert_eq!(err.key(), Some("internal.sandbox_subnet"), "{err}");
    }

    #[test]
    fn a_missing_section_is_named() {
        let err = file_err(&replace(
            "[store]\nurl = \"sqlite::memory:\"\ndata_dir = \"/nonexistent/agentd\"\n",
            "",
        ));
        assert_eq!(err.key(), Some("store"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_named() {
        let err = file_err(&replace("[server]\n", "[server]\nlisen = \"x\"\n"));
        assert!(err.key().unwrap().starts_with("server"), "{err}");
        assert!(err.to_string().contains("lisen"), "{err}");
        let err = file_err(&format!("{MINIMAL}\n[slack]\nx = 1\n"));
        assert!(err.to_string().contains("slack"), "{err}");
    }

    #[test]
    fn a_secret_in_the_file_is_refused_without_repeating_it() {
        let err = file_err(&replace(
            "[store]\n",
            "[store]\nmaster_key = \"c2VjcmV0LWtleS12YWx1ZQ==\"\n",
        ));
        assert!(err.to_string().contains("master_key"), "{err}");
        assert!(!err.to_string().contains("c2VjcmV0"), "{err}");
    }

    #[test]
    fn a_wrongly_typed_value_is_named_with_its_line() {
        let err = file_err(&replace("listen = \"127.0.0.1:0\"", "listen = 8443"));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
        assert!(err.to_string().contains("(line 3)"), "{err}");
        let err = file_err(&replace(
            "[server]\n",
            "[server]\ndrain_timeout_secs = -1\n",
        ));
        assert_eq!(err.key(), Some("server.drain_timeout_secs"), "{err}");
    }

    #[test]
    fn a_bad_address_is_named() {
        let err = file_err(&replace(
            "listen = \"127.0.0.1:0\"",
            "listen = \"localhost\"",
        ));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
    }

    #[test]
    fn listeners_never_bind_every_interface() {
        for (from, to, key) in [
            (
                "listen = \"127.0.0.1:0\"",
                "listen = \"0.0.0.0:8443\"",
                "server.listen",
            ),
            (
                "listen = \"127.0.0.1:0\"",
                "listen = \"[::]:8443\"",
                "server.listen",
            ),
            (
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"0.0.0.0:8080\"",
                "internal.proxy_listen",
            ),
            (
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"[::]:8081\"",
                "internal.ctl_listen",
            ),
            (
                "listen = \"127.0.0.1:0\"",
                "listen = \"[::ffff:0.0.0.0]:8443\"",
                "server.listen",
            ),
            (
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"[::ffff:0.0.0.0]:8080\"",
                "internal.proxy_listen",
            ),
            (
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"[::ffff:0:0]:8081\"",
                "internal.ctl_listen",
            ),
        ] {
            let err = file_err(&replace(from, to));
            assert_eq!(err.key(), Some(key), "{err}");
            assert!(err.to_string().contains("every interface"), "{err}");
        }
    }

    #[test]
    fn listeners_need_distinct_addresses() {
        for ctl in ["127.0.0.2:8080", "[::ffff:127.0.0.2]:8080"] {
            let text = replace(
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"127.0.0.2:8080\"",
            )
            .replacen(
                "ctl_listen = \"127.0.0.2:0\"",
                &format!("ctl_listen = \"{ctl}\""),
                1,
            );
            let err = file_err(&text);
            assert_eq!(err.key(), Some("internal.ctl_listen"), "{ctl}: {err}");
            assert!(err.to_string().contains("internal.proxy_listen"), "{err}");
        }
    }

    #[test]
    fn the_public_listener_is_not_on_the_sandbox_network() {
        let err = file_err(&replace(
            "listen = \"127.0.0.1:0\"",
            "listen = \"127.0.0.2:8443\"",
        ));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
        assert!(err.to_string().contains("internal.sandbox_subnet"), "{err}");

        let err = file_err(&replace(
            "listen = \"127.0.0.1:0\"",
            "listen = \"[::ffff:127.0.0.2]:8443\"",
        ));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
    }

    #[test]
    fn the_internal_listeners_are_on_the_sandbox_network() {
        for (from, to, key) in [
            (
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"172.31.0.2:8080\"",
                "internal.proxy_listen",
            ),
            (
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"127.0.0.3:8081\"",
                "internal.ctl_listen",
            ),
            (
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"[::1]:8081\"",
                "internal.ctl_listen",
            ),
        ] {
            let err = file_err(&replace(from, to));
            assert_eq!(err.key(), Some(key), "{err}");
            assert!(
                err.to_string().contains("outside internal.sandbox_subnet"),
                "{err}"
            );
        }

        let text = replace(
            "proxy_listen = \"127.0.0.2:0\"",
            "proxy_listen = \"[::ffff:127.0.0.2]:0\"",
        );
        with(&text, env()).unwrap();
    }

    #[test]
    fn an_ipv4_mapped_sandbox_subnet_still_guards_the_listeners() {
        let text = replace("\"127.0.0.2/32\"", "\"::ffff:127.0.0.2/128\"");
        let config = with(&text, env()).unwrap();
        assert_eq!(
            config.internal.sandbox_subnet,
            "127.0.0.2/32".parse().unwrap()
        );
        let err =
            file_err(&text.replacen("listen = \"127.0.0.1:0\"", "listen = \"127.0.0.2:8443\"", 1));
        assert_eq!(err.key(), Some("server.listen"), "{err}");
    }

    #[test]
    fn a_bad_subnet_is_named() {
        for bad in ["172.30.0.0", "172.30.0.1/24", "172.30.0.0/40", "nope/8"] {
            let err = file_err(&replace("\"127.0.0.2/32\"", &format!("\"{bad}\"")));
            assert_eq!(err.key(), Some("internal.sandbox_subnet"), "{bad}: {err}");
        }
    }

    #[test]
    fn the_drain_timeout_is_bounded() {
        let text = replace("[server]\n", "[server]\ndrain_timeout_secs = 3601\n");
        let err = file_err(&text);
        assert_eq!(err.key(), Some("server.drain_timeout_secs"), "{err}");
        let text = replace("[server]\n", "[server]\ndrain_timeout_secs = 0\n");
        assert_eq!(
            with(&text, env()).unwrap().server.drain_timeout(),
            Duration::ZERO
        );
    }

    #[test]
    fn a_bad_log_filter_is_named() {
        let err = file_err(&replace(
            "[server]\n",
            "[server]\nlog_filter = \"agentd=loud\"\n",
        ));
        assert_eq!(err.key(), Some("server.log_filter"), "{err}");
    }

    #[test]
    fn the_store_url_must_be_sqlite() {
        let err = file_err(&replace("sqlite::memory:", "postgres://db/agentd"));
        assert_eq!(err.key(), Some("store.url"), "{err}");
    }

    #[test]
    fn the_data_dir_must_be_absolute() {
        let err = file_err(&replace("\"/nonexistent/agentd\"", "\"var/lib/agentd\""));
        assert_eq!(err.key(), Some("store.data_dir"), "{err}");
        let err = file_err(&replace("data_dir = \"/nonexistent/agentd\"\n", ""));
        assert_eq!(err.key(), Some("store.data_dir"), "{err}");
    }

    #[test]
    fn the_attach_cap_is_configurable_and_positive() {
        let text = format!("{MINIMAL}\n[limits]\nattach_max_bytes = 1024\n");
        assert_eq!(with(&text, env()).unwrap().limits.attach_max_bytes, 1024);
        let err = file_err(&format!("{MINIMAL}\n[limits]\nattach_max_bytes = 0\n"));
        assert_eq!(err.key(), Some("limits.attach_max_bytes"), "{err}");
    }

    #[test]
    fn the_agent_limit_is_configurable_and_positive() {
        let text = format!("{MINIMAL}\n[agents]\nmax_per_owner = 3\n");
        assert_eq!(with(&text, env()).unwrap().agents.max_per_owner, 3);
        let err = file_err(&format!("{MINIMAL}\n[agents]\nmax_per_owner = 0\n"));
        assert_eq!(err.key(), Some("agents.max_per_owner"), "{err}");
    }

    #[test]
    fn community_admins_are_member_identities_and_none_by_default() {
        assert!(with(MINIMAL, env()).unwrap().community.admins.is_empty());
        let text = format!(
            "{MINIMAL}\n[community]\nadmins = [\"slack:T0123:U0456\", \
             \"rocketchat:chat.example.com:aBcD\"]\n"
        );
        let community = with(&text, env()).unwrap().community;
        let slack: MemberKey = "slack:T0123:U0456".parse().unwrap();
        let rocket: MemberKey = "rocketchat:chat.example.com:aBcD".parse().unwrap();
        assert_eq!(community.admins, [slack, rocket]);

        let err = file_err(&format!(
            "{MINIMAL}\n[community]\nadmins = [\"slack:T0123:U0456\", \"U0456\"]\n"
        ));
        assert_eq!(err.key(), Some("community.admins"), "{err}");
        assert!(err.to_string().contains("<surface>:<team>:<user>"), "{err}");
        let err = file_err(&format!("{MINIMAL}\n[community]\nadmin = []\n"));
        assert!(err.to_string().contains("admin"), "{err}");
    }

    #[test]
    fn the_master_key_is_required() {
        let err = with(MINIMAL, vec![("PATH".to_owned(), "/usr/bin".to_owned())]).unwrap_err();
        assert_eq!(err.key(), Some(MASTER_KEY_VAR), "{err}");
        assert!(err.to_string().contains("agentd gen-key"), "{err}");
    }

    #[test]
    fn a_bad_master_key_is_named_without_repeating_it() {
        for bad in ["not base64!", "c2hvcnQ="] {
            let err = with(MINIMAL, vec![(MASTER_KEY_VAR.to_owned(), bad.to_owned())]).unwrap_err();
            assert_eq!(err.key(), Some(MASTER_KEY_VAR), "{err}");
            assert!(!err.to_string().contains(bad), "{err}");
        }
    }

    #[test]
    fn empty_secrets_are_refused() {
        for name in [RC_MANAGER_TOKEN_VAR, "AGENTD_SLACK_MANAGER_BOT_TOKEN"] {
            let err = env_err(&[(name, " ")]);
            assert_eq!(err.key(), Some(name), "{err}");
            assert!(err.to_string().contains("empty"), "{err}");
        }
        let err = with(MINIMAL, vec![(MASTER_KEY_VAR.to_owned(), String::new())]).unwrap_err();
        assert_eq!(err.key(), Some(MASTER_KEY_VAR), "{err}");
    }

    #[test]
    fn secrets_with_surrounding_white_space_are_refused() {
        let key = key();
        for name in [
            MASTER_KEY_VAR,
            RC_MANAGER_TOKEN_VAR,
            SLACK_MANAGER_SIGNING_SECRET_VAR,
        ] {
            for value in [
                format!("{key}\n"),
                format!("{key}\r\n"),
                format!(" {key}"),
                format!("{key}\t"),
            ] {
                let mut env = env();
                env.retain(|(k, _)| k != name);
                env.push((name.to_owned(), value.clone()));
                let err = with(MINIMAL, env).unwrap_err();
                assert_eq!(err.key(), Some(name), "{err}");
                let message = err.to_string();
                assert!(message.contains("white space"), "{message}");
                assert!(!message.contains(&key), "{message}");
            }
        }
        let mut env = env();
        env.push((
            SLACK_MANAGER_SIGNING_SECRET_VAR.to_owned(),
            "inner space is kept".to_owned(),
        ));
        env.push((
            SLACK_MANAGER_BOT_TOKEN_VAR.to_owned(),
            "bot-value".to_owned(),
        ));
        let config = with(MINIMAL, env).unwrap();
        assert_eq!(
            config
                .secrets
                .slack_manager_signing_secret()
                .unwrap()
                .expose_secret(),
            "inner space is kept"
        );
    }

    #[test]
    fn near_misses_of_secret_names_are_refused() {
        for (name, known) in [
            ("AGENTD_MASTERKEY", MASTER_KEY_VAR),
            ("AGENTD_MASTER_KY", MASTER_KEY_VAR),
            ("AGENTD_MASTR_KEYS", MASTER_KEY_VAR),
            ("AGENTD_RC_MANAGER_TOKENS", RC_MANAGER_TOKEN_VAR),
            ("AGENTD_RC_MANGER_TOKN", RC_MANAGER_TOKEN_VAR),
            ("AGENTD_SLACK_MANAGER_", "AGENTD_SLACK_MANAGER_<NAME>"),
            (
                "AGENTD_SLACK_MANAGER_bot_token",
                "AGENTD_SLACK_MANAGER_<NAME>",
            ),
            (
                "AGENTD_SLACK_MANGER_SIGNING_SECRET",
                "AGENTD_SLACK_MANAGER_<NAME>",
            ),
            (
                "AGENTD_SLACKMANAGER_BOT_TOKEN",
                "AGENTD_SLACK_MANAGER_<NAME>",
            ),
        ] {
            let err = env_err(&[(name, "value")]);
            assert_eq!(err.key(), Some(name), "{err}");
            assert!(err.to_string().contains("unknown variable"), "{err}");
            assert!(err.to_string().contains(known), "{name}: {err}");
            assert!(!err.to_string().contains("value"), "{err}");
        }
    }

    #[test]
    fn kubernetes_service_links_are_skipped() {
        let names = [
            "AGENTD_PORT",
            "AGENTD_SERVICE_HOST",
            "AGENTD_SERVICE_PORT",
            "AGENTD_SERVICE_PORT_PUBLIC",
            "AGENTD_SERVICE_PORT_HTTP_ALT",
            "AGENTD_PORT_8443_TCP",
            "AGENTD_PORT_8443_TCP_PROTO",
            "AGENTD_PORT_8443_TCP_PORT",
            "AGENTD_PORT_8443_TCP_ADDR",
            "AGENTD_PORT_53_UDP",
            "AGENTD_INTERNAL_PORT",
            "AGENTD_INTERNAL_SERVICE_HOST",
            "AGENTD_INTERNAL_PORT_8080_SCTP_ADDR",
        ];
        let mut env = env();
        env.extend(
            names
                .iter()
                .map(|name| ((*name).to_owned(), "tcp://10.0.0.11:8443".to_owned())),
        );
        let config = with(MINIMAL, env).unwrap();
        assert!(config.unknown_env.is_empty(), "{:?}", config.unknown_env);
        for name in names {
            assert!(is_service_link(name), "{name}");
        }
        for name in [
            "AGENTD_PORT_8443",
            "AGENTD_PORT_X_TCP",
            "AGENTD_PORT_8443_HTTP",
            "AGENTD_PORT_8443_TCP_HOST",
            "AGENTD_SERVICE",
            "AGENTD_LOG",
        ] {
            assert!(!is_service_link(name), "{name}");
        }
    }

    #[test]
    fn service_links_under_the_slack_manager_prefix_are_not_secrets() {
        let names = [
            "AGENTD_SLACK_MANAGER_PORT",
            "AGENTD_SLACK_MANAGER_SERVICE_HOST",
            "AGENTD_SLACK_MANAGER_SERVICE_PORT",
            "AGENTD_SLACK_MANAGER_PORT_8443_TCP_ADDR",
        ];
        let mut env = env();
        env.extend(
            names
                .iter()
                .map(|name| ((*name).to_owned(), "tcp://10.0.0.12:8443".to_owned())),
        );
        let config = with(MINIMAL, env).unwrap();
        assert!(config.secrets.slack_manager.is_empty());
        assert!(config.unknown_env.is_empty(), "{:?}", config.unknown_env);
    }

    #[test]
    fn other_unknown_agentd_variables_are_ignored_and_listed() {
        let mut env = env();
        for name in ["AGENTD_LOG", "AGENTD_DEBUG", "AGENTD_CONFIG"] {
            env.push((name.to_owned(), "value".to_owned()));
        }
        env.push(("NOT_AGENTD_LOG".to_owned(), "value".to_owned()));
        let config = with(MINIMAL, env).unwrap();
        assert_eq!(
            config.unknown_env,
            ["AGENTD_CONFIG", "AGENTD_DEBUG", "AGENTD_LOG"]
        );
    }

    #[test]
    fn edit_distances_cover_every_prefix() {
        assert_eq!(edit_distances("abc", "abc"), [3, 2, 1, 0]);
        assert_eq!(edit_distances("abc", "axc"), [3, 2, 2, 1]);
        assert_eq!(edit_distances("", "ab"), [0, 1, 2]);
        assert_eq!(edit_distances("ab", ""), [2]);
        assert_eq!(edit_distances("kitten", "sitting").last(), Some(&3));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_secret_is_refused_and_non_utf8_names_are_ignored() {
        use std::os::unix::ffi::OsStringExt;

        let mut env: Vec<(OsString, OsString)> = env()
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        env.push((OsString::from_vec(b"X\xff".to_vec()), "x".into()));
        Config::parse(MINIMAL, env.clone()).unwrap();
        env.push((
            RC_MANAGER_TOKEN_VAR.into(),
            OsString::from_vec(b"\xff".to_vec()),
        ));
        let err = Config::parse(MINIMAL, env).unwrap_err();
        assert_eq!(err.key(), Some(RC_MANAGER_TOKEN_VAR), "{err}");
    }

    #[test]
    fn line_of_counts_from_one() {
        assert_eq!(line_of("a\nb\nc", Some(4..5)), 3);
        assert_eq!(line_of("a\nb", None), 1);
        assert_eq!(line_of("a", Some(99..100)), 1);
    }
}
