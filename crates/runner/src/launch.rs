//! [`LaunchSpec`] and the pure functions that turn it into `claude`'s argv
//! and environment.

use std::collections::BTreeMap;
use std::fmt;

use core_types::{CredentialKind, SessionId};
use sandbox::SessionPaths;
use secrecy::{ExposeSecret, SecretString};

use crate::{ProcessConfig, Result, RunnerError};

/// The tools the agent gets: Claude Code's built-ins, and nothing else.
pub(crate) const TOOLS: &str = "Bash,Read,Edit,Write,Glob,Grep";

/// The variable that carries a subscription placeholder.
pub(crate) const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The variable that carries an API-key placeholder.
pub(crate) const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Variables the runner sets itself, which [`LaunchSpec::env`] may not.
const RESERVED_ENV: [&str; 8] = [
    "HOME",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_PROJECT_DIR_NAME",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_AUTOUPDATER",
    OAUTH_TOKEN_ENV,
    API_KEY_ENV,
];

/// Prefixes of variables [`LaunchSpec::env`] may not set: every
/// `ANTHROPIC_*` variable (the base URL, the API key, `ANTHROPIC_AUTH_TOKEN`
/// and the rest) and every Claude OAuth variable. Credentials and the API's
/// address reach the process only through the runner.
const RESERVED_ENV_PREFIXES: [&str; 2] = ["ANTHROPIC_", "CLAUDE_CODE_OAUTH_"];

/// Whether the session is new or continues from its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStart {
    /// The session has never started: `--session-id <id>`. The CLI refuses
    /// it if the session's transcript already exists.
    New,
    /// The session has started before: `--resume <id>`, which continues
    /// from its transcript. The CLI refuses it if there is no transcript.
    Resume,
}

/// What [`ClaudeProcess::start`](crate::ClaudeProcess::start) starts, on
/// top of what the container already fixes (the session and its paths).
///
/// `Debug` shows the environment's names, never its values, and the
/// placeholder not at all.
pub struct LaunchSpec {
    /// `--session-id` or `--resume`.
    pub start: SessionStart,
    /// `--model`, when the router chose one. Letters, digits and `.`, `_`,
    /// `-`, `:`, `[`, `]`, `@`, `/`, at most 128, not starting with `-`.
    pub model: Option<String>,
    /// Which credential kind the process runs on. It picks the variable
    /// the placeholder goes in, so the process has exactly one of
    /// `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY`.
    pub credential: CredentialKind,
    /// The process's placeholder token, which the credential proxy swaps
    /// for the turn's real credential. Non-empty, printable ASCII.
    pub placeholder: SecretString,
    /// Everything else the caller adds, such as `AGENTCTL_TOKEN` and the
    /// egress proxy variables. It may not set `HOME`, `TMPDIR`, the
    /// variables of the design's credential proxy block, or any
    /// `ANTHROPIC_*` or `CLAUDE_CODE_OAUTH_*` variable: the runner sets
    /// those. Values may be secrets; they go only to
    /// [`Sandbox::exec`](sandbox::Sandbox::exec).
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("start", &self.start)
            .field("model", &self.model)
            .field("credential", &self.credential)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Whether `model` is safe as `--model`'s value: it can't be read as a flag
/// and holds only characters model names and aliases use.
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && !model.starts_with('-')
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:[]@/".contains(c))
}

/// A path as a string for argv or the environment.
fn path_str(path: &std::path::Path) -> Result<&str> {
    path.to_str()
        .ok_or(RunnerError::InvalidSpec("container paths must be UTF-8"))
}

/// `claude`'s argv, as the design's launch flags have it.
pub(crate) fn argv(
    config: &ProcessConfig,
    paths: &SessionPaths,
    session: SessionId,
    spec: &LaunchSpec,
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = [
        config.claude_bin.as_str(),
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        TOOLS,
        "--strict-mcp-config",
        "--setting-sources",
        "user",
        "--permission-mode",
        "bypassPermissions",
        "--append-system-prompt-file",
        path_str(&paths.persona_file)?,
    ]
    .map(String::from)
    .into();
    if let Some(model) = &spec.model {
        if !valid_model(model) {
            return Err(RunnerError::InvalidSpec(
                "the model must be a plain model name",
            ));
        }
        argv.extend(["--model".into(), model.clone()]);
    }
    let flag = match spec.start {
        SessionStart::New => "--session-id",
        SessionStart::Resume => "--resume",
    };
    argv.extend([flag.into(), session.to_string()]);
    Ok(argv)
}

/// `claude`'s environment: the caller's [`LaunchSpec::env`], the design's
/// credential proxy block with the placeholder in the one variable its
/// credential kind uses, and `HOME` and `TMPDIR` from the container's
/// paths.
pub(crate) fn env(
    config: &ProcessConfig,
    paths: &SessionPaths,
    session: SessionId,
    spec: &LaunchSpec,
) -> Result<BTreeMap<String, String>> {
    if spec.env.keys().any(|key| {
        RESERVED_ENV.contains(&key.as_str())
            || RESERVED_ENV_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
    }) {
        return Err(RunnerError::InvalidSpec(
            "env may not set HOME, TMPDIR, the credential proxy variables, or ANTHROPIC_* and CLAUDE_CODE_OAUTH_* variables",
        ));
    }
    let placeholder = spec.placeholder.expose_secret();
    if placeholder.is_empty() || !placeholder.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(RunnerError::InvalidSpec(
            "the placeholder must be non-empty printable ASCII",
        ));
    }
    let credential_var = match spec.credential {
        CredentialKind::Subscription => OAUTH_TOKEN_ENV,
        CredentialKind::ApiKey => API_KEY_ENV,
    };
    let mut env = spec.env.clone();
    let session = session.to_string();
    let set = [
        (credential_var, placeholder),
        ("ANTHROPIC_BASE_URL", config.anthropic_base_url.as_str()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CONFIG_DIR", path_str(&paths.claude_config)?),
        ("CLAUDE_CODE_PROJECT_DIR_NAME", session.as_str()),
        ("HOME", path_str(&paths.home)?),
        ("TMPDIR", path_str(&paths.tmp)?),
    ];
    env.extend(set.map(|(key, value)| (key.to_owned(), value.to_owned())));
    Ok(env)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const PLACEHOLDER: &str = "agentd-placeholder-0123";

    fn paths() -> SessionPaths {
        let session = PathBuf::from("/volume/sessions/s1");
        SessionPaths {
            work: session.join("work"),
            claude_config: session.join("claude"),
            home: session.join("home"),
            tmp: session.join("tmp"),
            persona_file: "/agent/persona.md".into(),
            shared: "/volume/shared".into(),
            memory: None,
        }
    }

    fn spec(start: SessionStart, credential: CredentialKind) -> LaunchSpec {
        LaunchSpec {
            start,
            model: None,
            credential,
            placeholder: SecretString::from(PLACEHOLDER),
            env: BTreeMap::from([
                ("AGENTCTL_TOKEN".to_string(), "ctl-secret".to_string()),
                (
                    "HTTPS_PROXY".to_string(),
                    "http://cred-proxy.internal:8080".to_string(),
                ),
            ]),
        }
    }

    #[test]
    fn argv_has_the_design_flags_and_session_id_for_a_new_session() {
        let session = SessionId::new_v4();
        let config = ProcessConfig::default();
        let argv = argv(
            &config,
            &paths(),
            session,
            &spec(SessionStart::New, CredentialKind::Subscription),
        )
        .unwrap();
        let expected: Vec<String> = [
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "Bash,Read,Edit,Write,Glob,Grep",
            "--strict-mcp-config",
            "--setting-sources",
            "user",
            "--permission-mode",
            "bypassPermissions",
            "--append-system-prompt-file",
            "/agent/persona.md",
            "--session-id",
            &session.to_string(),
        ]
        .map(String::from)
        .into();
        assert_eq!(argv, expected);
    }

    #[test]
    fn argv_resumes_and_passes_the_model() {
        let session = SessionId::new_v4();
        let mut spec = spec(SessionStart::Resume, CredentialKind::ApiKey);
        spec.model = Some("claude-opus-5-5[1m]".into());
        let argv = argv(&ProcessConfig::default(), &paths(), session, &spec).unwrap();
        let tail: Vec<&str> = argv[argv.len() - 4..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "--model",
                "claude-opus-5-5[1m]",
                "--resume",
                &session.to_string()
            ]
        );
        assert!(!argv.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn a_model_that_could_be_a_flag_is_refused() {
        for model in [
            "",
            "--dangerously-skip-permissions",
            "a b",
            "m\n",
            &"m".repeat(129),
        ] {
            let mut spec = spec(SessionStart::New, CredentialKind::Subscription);
            spec.model = Some(model.to_owned());
            let result = argv(
                &ProcessConfig::default(),
                &paths(),
                SessionId::new_v4(),
                &spec,
            );
            assert!(
                matches!(result, Err(RunnerError::InvalidSpec(_))),
                "{model:?}"
            );
        }
    }

    #[test]
    fn env_has_the_proxy_block_and_the_subscription_placeholder_only() {
        let session = SessionId::new_v4();
        let env = env(
            &ProcessConfig::default(),
            &paths(),
            session,
            &spec(SessionStart::New, CredentialKind::Subscription),
        )
        .unwrap();
        let expected: BTreeMap<String, String> = [
            ("AGENTCTL_TOKEN", "ctl-secret"),
            ("ANTHROPIC_BASE_URL", "http://cred-proxy.internal:8080"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", PLACEHOLDER),
            ("CLAUDE_CODE_PROJECT_DIR_NAME", &session.to_string()),
            ("CLAUDE_CONFIG_DIR", "/volume/sessions/s1/claude"),
            ("DISABLE_AUTOUPDATER", "1"),
            ("HOME", "/volume/sessions/s1/home"),
            ("HTTPS_PROXY", "http://cred-proxy.internal:8080"),
            ("TMPDIR", "/volume/sessions/s1/tmp"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(env, expected);
    }

    #[test]
    fn env_puts_an_api_key_placeholder_in_anthropic_api_key_only() {
        let env = env(
            &ProcessConfig::default(),
            &paths(),
            SessionId::new_v4(),
            &spec(SessionStart::Resume, CredentialKind::ApiKey),
        )
        .unwrap();
        assert_eq!(env.get("ANTHROPIC_API_KEY").unwrap(), PLACEHOLDER);
        assert!(!env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        let holding: Vec<_> = env.values().filter(|v| v.as_str() == PLACEHOLDER).collect();
        assert_eq!(holding.len(), 1);
    }

    #[test]
    fn env_may_not_carry_credentials_or_runner_variables() {
        for key in [
            "HOME",
            "TMPDIR",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_PROJECT_DIR_NAME",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "DISABLE_AUTOUPDATER",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
        ] {
            let mut spec = spec(SessionStart::New, CredentialKind::Subscription);
            spec.env.insert(key.into(), "sk-ant-real".into());
            let err = env(
                &ProcessConfig::default(),
                &paths(),
                SessionId::new_v4(),
                &spec,
            )
            .unwrap_err();
            assert!(matches!(err, RunnerError::InvalidSpec(_)), "{key}");
            assert!(!err.to_string().contains("sk-ant-real"));
        }
    }

    #[test]
    fn a_bad_placeholder_is_refused_without_echoing_it() {
        for placeholder in ["", "has space", "line\nbreak", "é"] {
            let mut spec = spec(SessionStart::New, CredentialKind::Subscription);
            spec.placeholder = SecretString::from(placeholder);
            let err = env(
                &ProcessConfig::default(),
                &paths(),
                SessionId::new_v4(),
                &spec,
            )
            .unwrap_err();
            assert!(
                matches!(err, RunnerError::InvalidSpec(_)),
                "{placeholder:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let mut paths = paths();
        paths.persona_file = PathBuf::from(std::ffi::OsStr::from_bytes(b"/agent/\xff.md"));
        let spec = spec(SessionStart::New, CredentialKind::Subscription);
        let result = argv(
            &ProcessConfig::default(),
            &paths,
            SessionId::new_v4(),
            &spec,
        );
        assert!(matches!(result, Err(RunnerError::InvalidSpec(_))));
        let mut paths = self::paths();
        paths.home = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/\xff"));
        let result = env(
            &ProcessConfig::default(),
            &paths,
            SessionId::new_v4(),
            &spec,
        );
        assert!(matches!(result, Err(RunnerError::InvalidSpec(_))));
    }

    #[test]
    fn debug_shows_names_not_values() {
        let spec = spec(SessionStart::New, CredentialKind::Subscription);
        let debug = format!("{spec:?}");
        assert!(debug.contains("AGENTCTL_TOKEN"), "{debug}");
        assert!(!debug.contains("ctl-secret"), "{debug}");
        assert!(!debug.contains(PLACEHOLDER), "{debug}");
    }
}
