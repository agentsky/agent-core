//! Per-session sandbox containers for agent-core.
//!
//! A [`Sandbox`] runs one container per active session, with the scope's
//! volume mounted, as the design's "Sessions and sandboxes" section
//! describes. The runner uses only this trait:
//!
//! 1. [`ensure_volume`](Sandbox::ensure_volume) makes the host directory for
//!    an `(agent, scope)` pair and records it in the store's `volumes` table.
//! 2. [`start`](Sandbox::start) prepares the session's directories and
//!    starts a container from a [`SessionSpec`].
//! 3. [`exec`](Sandbox::exec) runs a process in it with piped stdin and
//!    stdout, at the paths [`Container::paths`] gives.
//! 4. [`stop`](Sandbox::stop) ends it. [`events`](Sandbox::events) reports
//!    every container that dies, however it died.
//!
//! Two implementations:
//!
//! - [`DockerSandbox`], the real one, over bollard. Its container
//!   configuration is the pure function [`container_config`].
//! - [`ProcessSandbox`], for tests and Docker-less development. It isolates
//!   nothing.
//!
//! The directory layout is in [`scope_dir_name`] and the `layout` module:
//! volumes are host directories `volumes/<agent id>/<hex SHA-256 of the
//! scope key>` under agentd's data directory, never named after the key
//! itself, which may hold `:` and `%`.

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use core_types::{ScopeKey, SessionId, VolumeKey};
use futures::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncWrite};

mod config;
mod docker;
mod layout;
mod process;

pub use config::{
    ConfigError, DEFAULT_CLEANUP_PERIOD_DAYS, DEFAULT_SANDBOX_UID, MAX_STOP_TIMEOUT_SECS,
    SandboxConfig,
};
pub use docker::{
    CONTAINER_PERSONA_DIR, CONTAINER_VOLUME_DIR, DockerSandbox, LABEL_AGENT, LABEL_INSTANCE,
    LABEL_SCOPE, LABEL_SESSION, container_config,
};
pub use layout::{SESSION_SUBDIRS, VOLUMES_DIR, scope_dir_name, volume_rel_path};
pub use process::ProcessSandbox;

/// The persona file's name inside the agent's persona directory.
pub const PERSONA_FILE: &str = "persona.md";

/// The error returned by [`Sandbox`] methods.
///
/// No variant carries an environment value, and errors from `exec` never
/// carry Docker's message, since the environment it sends holds the
/// session's credentials.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The configuration is invalid.
    #[error("invalid sandbox configuration: {0}")]
    Config(#[from] ConfigError),
    /// A [`SessionSpec`], or an `exec` argument, is invalid.
    #[error("invalid session spec: {0}")]
    InvalidSpec(&'static str),
    /// A filesystem operation failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        source: io::Error,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] store::StoreError),
    /// A Docker API call failed.
    #[error("docker {op} failed{}{}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default(), message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Docker {
        /// The operation, such as `create container`.
        op: &'static str,
        /// Docker's HTTP status, when it answered.
        status: Option<u16>,
        /// What went wrong, when it can't hold a secret.
        message: Option<String>,
    },
    /// The container doesn't exist, or has stopped.
    #[error("no such container")]
    NotFound,
    /// The container has no address on the sandbox network.
    #[error("the container has no address on the sandbox network")]
    NoAddress,
    /// The event stream broke, fell behind or ended, so deaths may have
    /// been missed. Subscribe again, and compare [`Sandbox::list_managed`]
    /// with what is running.
    #[error("container events may have been missed")]
    EventsMissed,
}

/// A `Result` whose error is [`SandboxError`].
pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

/// A volume: the host directory holding one agent's files for one scope.
///
/// Only [`Sandbox::ensure_volume`] makes one, so its path is always the one
/// derived from its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRef {
    key: VolumeKey,
    path: PathBuf,
    owner: Option<(u32, u32)>,
}

impl VolumeRef {
    /// The agent and scope the volume serves.
    pub fn key(&self) -> &VolumeKey {
        &self.key
    }

    /// The volume's directory, as agentd sees it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The uid and gid the sandbox gives what agents may write in the
    /// volume, the user agents run as, or `None` when it leaves them to
    /// agentd's own user.
    pub fn owner(&self) -> Option<(u32, u32)> {
        self.owner
    }

    /// `sessions/<session>/`, mounted read-write in that session only.
    pub fn session_dir(&self, session: SessionId) -> PathBuf {
        self.path.join("sessions").join(session.to_string())
    }

    /// `shared/`, mounted into every session of the scope.
    pub fn shared_dir(&self) -> PathBuf {
        self.path.join("shared")
    }

    /// `memory/`, which only the agent's `Private` volume has.
    pub fn memory_dir(&self) -> Option<PathBuf> {
        (self.key.scope == ScopeKey::Private).then(|| self.path.join("memory"))
    }
}

/// How a session mounts its scope's `shared/` directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedAccess {
    /// Read-write: sessions the owner requested, and every session of a
    /// channel or DM scope.
    ReadWrite,
    /// Read-only: a private task a non-owner requested.
    ReadOnly,
}

/// What [`Sandbox::start`] starts.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// The session. Its directory is `sessions/<session>/` on the volume.
    pub session: SessionId,
    /// The scope's volume.
    pub volume: VolumeRef,
    /// The image, normally [`SandboxConfig::image`].
    pub image: String,
    /// Environment for the container. It must hold no secrets: Docker
    /// shows it to anyone who can inspect the container. Per-process
    /// credentials go to [`Sandbox::exec`]. `HOME` and `TMPDIR` are set by
    /// the sandbox and refused here.
    pub env: BTreeMap<String, String>,
    /// The agent's persona directory, holding [`PERSONA_FILE`]. Mounted
    /// read-only.
    pub persona_dir: PathBuf,
    /// The agent's skills directory, if it has skills. Mounted read-only
    /// as `$CLAUDE_CONFIG_DIR/skills`.
    pub skills_dir: Option<PathBuf>,
    /// How `shared/` is mounted.
    pub shared: SharedAccess,
    /// Whether `memory/` is mounted (read-write). Only sessions the owner
    /// requested ask for it, and only the `Private` volume has it.
    pub memory: bool,
    /// Extra container labels. Keys starting with `agentd.` are reserved.
    pub labels: BTreeMap<String, String>,
}

impl SessionSpec {
    /// A spec with the least access: `shared/` read-only, no `memory/`, no
    /// skills, no extra environment or labels.
    pub fn new(
        session: SessionId,
        volume: VolumeRef,
        image: impl Into<String>,
        persona_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            session,
            volume,
            image: image.into(),
            env: BTreeMap::new(),
            persona_dir: persona_dir.into(),
            skills_dir: None,
            shared: SharedAccess::ReadOnly,
            memory: false,
            labels: BTreeMap::new(),
        }
    }
}

/// A container's id: Docker's, or a made-up one for [`ProcessSandbox`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContainerId(pub String);

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The paths a session's processes use, as they see them. Docker and
/// process sandboxes differ here, so the runner takes paths only from
/// [`Container::paths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    /// The working directory, `sessions/<id>/work`.
    pub work: PathBuf,
    /// `CLAUDE_CONFIG_DIR`, `sessions/<id>/claude`.
    pub claude_config: PathBuf,
    /// `HOME`, `sessions/<id>/home`.
    pub home: PathBuf,
    /// `TMPDIR`, `sessions/<id>/tmp`.
    pub tmp: PathBuf,
    /// The persona file, for `--append-system-prompt-file`.
    pub persona_file: PathBuf,
    /// The scope's `shared/` directory.
    pub shared: PathBuf,
    /// The `memory/` directory, when the session mounts it.
    pub memory: Option<PathBuf>,
}

/// A started sandbox for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    id: ContainerId,
    session: SessionId,
    paths: SessionPaths,
}

impl Container {
    /// The container's id.
    pub fn id(&self) -> &ContainerId {
        &self.id
    }

    /// The session it runs.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The paths as the session's processes see them.
    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }
}

/// A container [`Sandbox::list_managed`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedContainer {
    /// Its id.
    pub id: ContainerId,
    /// The session in its `agentd.session` label, if that parses.
    pub session: Option<SessionId>,
    /// Whether it is running.
    pub running: bool,
}

/// Something that happened to a managed container.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerEvent {
    /// The container stopped, however it stopped, including through
    /// [`Sandbox::stop`].
    Died {
        /// The container.
        container: ContainerId,
        /// The session in its label, if that parses.
        session: Option<SessionId>,
    },
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// The exit code, or 128 plus the signal number for a process killed
    /// by a signal, as Docker reports it. `None` when it couldn't be read.
    pub code: Option<i32>,
}

impl ExitStatus {
    /// Whether the process exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A process started with [`Sandbox::exec`]: its stdin, its stdout and a
/// handle to wait for it or kill it. Its stderr is discarded.
pub struct ChildIo {
    /// Waits for or kills the process. Declared before `stdin`, so a drop
    /// under a process sandbox kills a process still waiting for input
    /// instead of closing its input first and killing it in the middle of
    /// exiting. A dropped Docker child kills nothing.
    pub child: ChildHandle,
    /// The process's stdin. Shutting it down closes the stream.
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    /// The process's stdout. It ends when the process exits.
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
}

impl fmt::Debug for ChildIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildIo").finish_non_exhaustive()
    }
}

/// Waits for or kills a process started with [`Sandbox::exec`].
#[derive(Debug)]
pub struct ChildHandle(ChildInner);

#[derive(Debug)]
enum ChildInner {
    Process(process::ProcessChild),
    Docker(docker::DockerChild),
}

impl ChildHandle {
    /// Waits for the process to exit.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Io`] or [`SandboxError::Docker`] if the status can't
    /// be read.
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        match &mut self.0 {
            ChildInner::Process(child) => child.wait().await,
            ChildInner::Docker(child) => child.wait().await,
        }
    }

    /// Sends the process SIGKILL; [`wait`](Self::wait) then reaps it.
    ///
    /// Processes it started may survive it: a process sandbox kills its
    /// process group, a Docker sandbox only the process. Stopping the
    /// container ends them all.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Docker`] if the kill can't be sent, including when
    /// a Docker sandbox hasn't learned the process's pid within two
    /// seconds. Nothing was signalled then, so [`wait`](Self::wait) may
    /// not return: stop the container instead.
    pub async fn kill(&mut self) -> Result<()> {
        match &mut self.0 {
            ChildInner::Process(child) => child.kill().await,
            ChildInner::Docker(child) => child.kill().await,
        }
    }
}

/// Runs sessions in sandboxes. See the [crate docs](crate).
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    /// Creates the volume for `key` if it's missing, with its `sessions/`
    /// and `shared/` directories (and `memory/` on a `Private` volume), and
    /// records it in the `volumes` table. Two agents in one scope get two
    /// volumes.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Io`] or [`SandboxError::Store`].
    async fn ensure_volume(&self, key: &VolumeKey) -> Result<VolumeRef>;

    /// Prepares the session's directories and starts its container.
    ///
    /// It creates `sessions/<session>/` with `work/`, `claude/`, `home/`
    /// and `tmp/`, and writes `claude/settings.json` with
    /// `cleanupPeriodDays`. An entry the agent replaced with a symlink or a
    /// file is replaced with a directory again, the session directory and
    /// those in it get mode `0755` again, and `settings.json` is rewritten
    /// every time, so earlier runs of the agent can't change any of them.
    ///
    /// The session must have no running container: that repair is meant to
    /// run while nothing in the session directory runs. Stop the old
    /// container first.
    ///
    /// [`DockerSandbox`] checks here, on every start, that
    /// [`SandboxConfig::network`] names an existing `internal` network by
    /// its name.
    ///
    /// # Errors
    ///
    /// [`SandboxError::InvalidSpec`] for a spec that breaks a rule on
    /// [`SessionSpec`], [`SandboxError::Config`] for a network that isn't
    /// an internal network's name, [`SandboxError::Io`] or
    /// [`SandboxError::Docker`].
    async fn start(&self, spec: &SessionSpec) -> Result<Container>;

    /// Runs `argv` in the container, in [`SessionPaths::work`], with
    /// `env` added to the container's environment. `env` may hold
    /// credentials. This crate never logs it or puts it in an error, but
    /// bollard logs every Docker request body, this one included, at debug
    /// level under the `bollard` target: a log subscriber must cap that
    /// target at `info`, as agentd's does.
    ///
    /// # Errors
    ///
    /// [`SandboxError::InvalidSpec`] for an empty `argv` or a bad variable
    /// name, [`SandboxError::NotFound`] after [`stop`](Self::stop),
    /// [`SandboxError::Io`] or [`SandboxError::Docker`].
    async fn exec(
        &self,
        container: &Container,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<ChildIo>;

    /// The container's address on the sandbox network: its network
    /// identity for the credential proxy and the ctl API.
    ///
    /// # Errors
    ///
    /// [`SandboxError::NotFound`], [`SandboxError::NoAddress`] or
    /// [`SandboxError::Docker`].
    async fn ip(&self, container: &ContainerId) -> Result<IpAddr>;

    /// Stops and removes the container. Stopping one that is already gone
    /// succeeds.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Docker`].
    async fn stop(&self, container: &ContainerId) -> Result<()>;

    /// Every container of this sandbox, running or not.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Docker`].
    async fn list_managed(&self) -> Result<Vec<ManagedContainer>>;

    /// A stream of [`ContainerEvent`]s, from now on.
    ///
    /// It never ends without an `Err` item, and ends after its first one.
    /// An `Err` means events may have been missed, including because the
    /// stream ended: subscribe again, and compare
    /// [`list_managed`](Self::list_managed) with what is running.
    fn events(&self) -> BoxStream<'static, Result<ContainerEvent>>;

    /// Stops every container [`list_managed`](Self::list_managed) finds.
    /// agentd calls it at startup: placeholder mappings and agentctl tokens
    /// don't survive a restart, so no container from before one may be
    /// used. Returns how many it stopped.
    ///
    /// # Errors
    ///
    /// The error listing returned, or the first error stopping returned,
    /// after trying every container.
    async fn reap_orphans(&self) -> Result<usize> {
        let found = self.list_managed().await?;
        let mut first_error = None;
        for container in &found {
            if let Err(err) = self.stop(&container.id).await {
                tracing::warn!(container = %container.id, error = %err, "reaping an orphan failed");
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(found.len()), Err)
    }
}

/// `HOME` and `TMPDIR`, which the sandbox sets.
const RESERVED_ENV: [&str; 2] = ["HOME", "TMPDIR"];

/// Checks environment variable names and values: a name is non-empty and
/// holds no `=`, and neither holds NUL.
fn check_env<'a>(env: impl IntoIterator<Item = (&'a String, &'a String)>) -> Result<()> {
    for (key, value) in env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(SandboxError::InvalidSpec(
                "environment names must be non-empty without `=`, and neither names nor values may hold NUL",
            ));
        }
    }
    Ok(())
}

/// Checks the rules on [`SessionSpec`] that both sandboxes share.
fn check_spec(spec: &SessionSpec) -> Result<()> {
    check_env(&spec.env)?;
    if spec
        .env
        .keys()
        .any(|key| RESERVED_ENV.contains(&key.as_str()))
    {
        return Err(SandboxError::InvalidSpec(
            "HOME and TMPDIR are set by the sandbox",
        ));
    }
    if spec.labels.keys().any(|key| key.starts_with("agentd.")) {
        return Err(SandboxError::InvalidSpec(
            "label keys starting with `agentd.` are reserved",
        ));
    }
    if spec.memory && spec.volume.memory_dir().is_none() {
        return Err(SandboxError::InvalidSpec(
            "only the agent's Private volume has memory/",
        ));
    }
    let paths = std::iter::once(&spec.persona_dir).chain(&spec.skills_dir);
    if !paths
        .into_iter()
        .all(|path| config::is_plain_absolute(path))
    {
        return Err(SandboxError::InvalidSpec(
            "persona and skills directories must be absolute paths without `..`",
        ));
    }
    Ok(())
}

/// Checks `exec`'s arguments.
fn check_exec(argv: &[String], env: &BTreeMap<String, String>) -> Result<()> {
    if argv.is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(SandboxError::InvalidSpec(
            "argv must be non-empty and hold no NUL",
        ));
    }
    check_env(env)
}

#[cfg(test)]
pub(crate) mod test_util {
    use core_types::{AgentId, ConvRef, SurfaceKind};
    use store::{Sealer, Store};
    pub(crate) use testkit::TempDir;

    use super::*;

    pub(crate) async fn memory_store() -> Store {
        let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
        Store::open_in_memory(sealer).await.unwrap()
    }

    /// A channel scope whose team and conversation ids hold `:` and `%`.
    pub(crate) fn awkward_channel() -> ScopeKey {
        ScopeKey::Channel(ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example.org:3000".into(),
            conversation: "room%2Fx:y".into(),
        })
    }

    pub(crate) fn volume(data_dir: &Path, agent: AgentId, scope: ScopeKey) -> VolumeRef {
        let key = VolumeKey { agent, scope };
        VolumeRef {
            path: data_dir.join(volume_rel_path(&key)),
            key,
            owner: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use core_types::AgentId;

    use super::test_util::*;
    use super::*;

    fn spec(scope: ScopeKey) -> SessionSpec {
        SessionSpec::new(
            SessionId::new_v4(),
            volume(Path::new("/data"), AgentId::new_v4(), scope),
            "img",
            "/data/agents/a",
        )
    }

    #[test]
    fn a_new_spec_has_the_least_access() {
        let spec = spec(ScopeKey::Private);
        assert_eq!(spec.shared, SharedAccess::ReadOnly);
        assert!(!spec.memory);
        assert_eq!(spec.skills_dir, None);
        check_spec(&spec).unwrap();
    }

    #[test]
    fn spec_rules_are_checked() {
        type Change = fn(&mut SessionSpec);
        let cases: [(&str, Change); 8] = [
            ("home", |s| {
                s.env.insert("HOME".into(), "/x".into());
            }),
            ("tmpdir", |s| {
                s.env.insert("TMPDIR".into(), "/x".into());
            }),
            ("equals", |s| {
                s.env.insert("A=B".into(), "x".into());
            }),
            ("empty name", |s| {
                s.env.insert(String::new(), "x".into());
            }),
            ("nul value", |s| {
                s.env.insert("A".into(), "x\0".into());
            }),
            ("reserved label", |s| {
                s.labels.insert("agentd.session".into(), "x".into());
            }),
            ("relative persona", |s| s.persona_dir = "agents/a".into()),
            ("dotdot skills", |s| {
                s.skills_dir = Some("/data/skills/../../etc".into());
            }),
        ];
        for (name, change) in cases {
            let mut spec = spec(ScopeKey::Private);
            change(&mut spec);
            assert!(
                matches!(check_spec(&spec), Err(SandboxError::InvalidSpec(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn memory_is_only_on_the_private_volume() {
        let mut private = spec(ScopeKey::Private);
        private.memory = true;
        check_spec(&private).unwrap();
        let mut channel = spec(awkward_channel());
        channel.memory = true;
        assert!(matches!(
            check_spec(&channel),
            Err(SandboxError::InvalidSpec(_))
        ));
        assert_eq!(channel.volume.memory_dir(), None);
    }

    #[test]
    fn exec_arguments_are_checked() {
        let env = BTreeMap::new();
        assert!(check_exec(&[], &env).is_err());
        assert!(check_exec(&["a\0".into()], &env).is_err());
        let bad_env = BTreeMap::from([("A=B".to_string(), "v".to_string())]);
        assert!(check_exec(&["true".into()], &bad_env).is_err());
        check_exec(&["true".into()], &env).unwrap();
    }

    #[test]
    fn volume_ref_paths() {
        let agent = AgentId::new_v4();
        let volume = volume(Path::new("/data"), agent, ScopeKey::Private);
        let session = SessionId::new_v4();
        let base = Path::new("/data/volumes")
            .join(agent.to_string())
            .join(scope_dir_name(&ScopeKey::Private));
        assert_eq!(volume.path(), base);
        assert_eq!(volume.key().agent, agent);
        assert_eq!(
            volume.session_dir(session),
            base.join("sessions").join(session.to_string())
        );
        assert_eq!(volume.shared_dir(), base.join("shared"));
        assert_eq!(volume.memory_dir(), Some(base.join("memory")));
    }

    #[test]
    fn errors_render_without_secrets() {
        let err = SandboxError::Docker {
            op: "create exec",
            status: Some(500),
            message: None,
        };
        assert_eq!(err.to_string(), "docker create exec failed (HTTP 500)");
        let err = SandboxError::Docker {
            op: "create container",
            status: None,
            message: Some("no such image".into()),
        };
        assert_eq!(
            err.to_string(),
            "docker create container failed: no such image"
        );
        assert!(ExitStatus { code: Some(0) }.success());
        assert!(!ExitStatus { code: None }.success());
        assert_eq!(ContainerId("abc".into()).to_string(), "abc");
    }
}
