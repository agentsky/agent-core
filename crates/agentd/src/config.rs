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

use secrecy::SecretString;
use serde::Deserialize;
use serde_path_to_error::Segment;
use store::Sealer;
use tracing_subscriber::EnvFilter;

use crate::net::Cidr;

/// The master key for encryption at rest: 32 bytes, standard base64, as
/// `agentd gen-key` prints it. Required.
pub const MASTER_KEY_VAR: &str = "AGENTD_MASTER_KEY";
/// The Rocket.Chat manager's personal access token. Optional until the
/// Rocket.Chat surface is configured.
pub const RC_MANAGER_TOKEN_VAR: &str = "AGENTD_RC_MANAGER_TOKEN";
/// The prefix of the Slack manager app's secrets, such as
/// `AGENTD_SLACK_MANAGER_SIGNING_SECRET`. They are collected into
/// [`Secrets::slack_manager`] by lowercased suffix.
pub const SLACK_MANAGER_PREFIX: &str = "AGENTD_SLACK_MANAGER_";
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

/// agentd's configuration, validated.
#[derive(Debug)]
#[non_exhaustive]
pub struct Config {
    /// `[server]`: the public listener, shutdown and logging.
    pub server: ServerConfig,
    /// `[internal]`: the listeners sandboxes reach.
    pub internal: InternalConfig,
    /// `[store]`: the database.
    pub store: StoreConfig,
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
    /// `sandbox_subnet`.
    pub proxy_listen: SocketAddr,
    /// `ctl_listen`: the agentctl API listener's address, agentd's address on
    /// the `sandbox` network, port 8081. Must be inside `sandbox_subnet`.
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
        Ok(Self {
            server: file.server,
            internal: file.internal,
            store: file.store,
            secrets,
            unknown_env,
        })
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
        Ok(())
    }
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
            } else if let Some(suffix) = name.strip_prefix(SLACK_MANAGER_PREFIX)
                && !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                slack_manager.insert(suffix.to_ascii_lowercase(), secret(name, value.into())?);
            } else if is_service_link(name) {
                continue;
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
"#;

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
        assert!(config.secrets.rc_manager_token.is_none());
        assert!(config.secrets.slack_manager.is_empty());
        assert!(config.unknown_env.is_empty());
        config.sealer().unwrap();
    }

    #[test]
    fn the_example_file_loads() {
        let text = include_str!("../../../config/agentd.example.toml");
        let config = with(text, env()).unwrap();
        assert_eq!(config.server.listen.port(), 8443);
        assert_eq!(config.internal.proxy_listen.port(), 8080);
        assert_eq!(config.internal.ctl_listen.port(), 8081);
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
                ("client_secret", "cli-value"),
                ("signing_secret", "sig-value")
            ]
        );
        let debug = format!("{config:?}");
        for secret in ["rc-token", "sig-value", "cli-value"] {
            assert!(!debug.contains(secret), "{debug}");
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
        let err = file_err(&replace("[store]\nurl = \"sqlite::memory:\"\n", ""));
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
