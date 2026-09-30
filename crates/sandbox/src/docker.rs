//! [`DockerSandbox`]: one Docker container per session, over bollard.
//!
//! [`container_config`] builds the whole container configuration as a pure
//! function; the rest of this module sends requests and reads answers.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerSummaryStateEnum, EventMessage, HostConfig, Mount, MountType,
};
use bollard::query_parameters::{
    EventsOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder,
};
use core_types::{SessionId, VolumeKey};
use futures::stream::{self, BoxStream, StreamExt};
use store::Store;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::layout::Layout;
use crate::{
    ChildHandle, ChildInner, ChildIo, Container, ContainerEvent, ContainerId, ExitStatus,
    ManagedContainer, PERSONA_FILE, Result, Sandbox, SandboxConfig, SandboxError, SessionPaths,
    SessionSpec, SharedAccess, VolumeRef, check_exec, check_spec, config,
};

/// Where the volume's directories appear in a container.
pub const CONTAINER_VOLUME_DIR: &str = "/volume";

/// Where the agent's persona directory appears in a container.
pub const CONTAINER_PERSONA_DIR: &str = "/agent";

/// The label holding a container's session id. Every managed container has
/// it.
pub const LABEL_SESSION: &str = "agentd.session";

/// The label holding a container's agent id.
pub const LABEL_AGENT: &str = "agentd.agent";

/// The label holding a container's scope key, in its string form.
pub const LABEL_SCOPE: &str = "agentd.scope";

/// The label holding [`SandboxConfig::instance`].
pub const LABEL_INSTANCE: &str = "agentd.instance";

/// Wraps every `exec` so the process's pid (in the container) is the first
/// line of its stdout, which is how [`ChildHandle::kill`] finds it: Docker
/// can't signal an exec'd process. `exec` keeps the pid, and `"$@"` passes
/// argv through without shell interpretation. The image needs `/bin/sh`.
const EXEC_WRAPPER: [&str; 4] = ["/bin/sh", "-c", "echo $$; exec \"$@\"", "sh"];

/// How long [`ChildHandle::wait`] polls Docker for an exit code after the
/// output ends.
const EXIT_POLL: Duration = Duration::from_secs(10);

/// How long [`ChildHandle::kill`] waits for the pid line before giving up.
const PID_WAIT: Duration = Duration::from_secs(2);

/// The container configuration for `spec`: a pure function of its inputs.
///
/// - The image, as `uid:gid` from `config` (never root), with `sleep
///   infinity` as the command under Docker's init, so the runner can exec
///   into it.
/// - Bind mounts through bollard's `Mounts` API, never `binds` strings:
///   `sessions/<id>/` read-write at `/volume/sessions/<id>`, `shared/` at
///   `/volume/shared` (read-only unless the spec says otherwise),
///   `memory/` at `/volume/memory` only when asked for, the skills
///   directory read-only at `/volume/sessions/<id>/claude/skills`, and the
///   persona directory read-only at `/agent`. Sources under `data_dir` are
///   rewritten to [`SandboxConfig::host_data_dir`] when it is set.
/// - `HOME` and `TMPDIR` in the session's `home/` and `tmp/`, then the
///   spec's environment; the working directory is `work/`.
/// - A tmpfs `/tmp` mounted with `exec`, since Docker's default is
///   `noexec`.
/// - The sandbox network only, `no-new-privileges`, every capability
///   dropped, memory (without swap), CPU and PID limits, a read-only root
///   filesystem, and not privileged.
/// - Labels `agentd.session`, `agentd.agent`, `agentd.scope` and
///   `agentd.instance`, after the spec's own.
///
/// # Errors
///
/// [`SandboxError::Config`] if `config` doesn't
/// [validate](SandboxConfig::validate), and [`SandboxError::InvalidSpec`] if
/// the spec breaks a rule on [`SessionSpec`], or a path to mount isn't
/// UTF-8, or is outside `data_dir` while `host_data_dir` is set.
pub fn container_config(
    config: &SandboxConfig,
    data_dir: &Path,
    spec: &SessionSpec,
) -> Result<ContainerCreateBody> {
    config.validate()?;
    check_spec(spec)?;
    if spec.image.trim().is_empty() {
        return Err(SandboxError::InvalidSpec("the image must not be empty"));
    }
    let paths = container_paths(spec);
    let host = |local: &Path| host_path(config, data_dir, local);
    let bind = |source: String, target: &Path, read_only: bool| Mount {
        source: Some(source),
        target: Some(target.to_string_lossy().into_owned()),
        typ: Some(MountType::BIND),
        read_only: Some(read_only),
        ..Default::default()
    };
    let session_dir = paths.work.parent().unwrap_or(&paths.work).to_path_buf();
    let mut mounts = vec![
        bind(
            host(&spec.volume.session_dir(spec.session))?,
            &session_dir,
            false,
        ),
        bind(
            host(&spec.volume.shared_dir())?,
            &paths.shared,
            spec.shared == SharedAccess::ReadOnly,
        ),
    ];
    if let (Some(local), Some(target)) = (spec.volume.memory_dir(), &paths.memory) {
        mounts.push(bind(host(&local)?, target, false));
    }
    if let Some(skills) = &spec.skills_dir {
        mounts.push(bind(
            host(skills)?,
            &paths.claude_config.join("skills"),
            true,
        ));
    }
    mounts.push(bind(
        host(&spec.persona_dir)?,
        Path::new(CONTAINER_PERSONA_DIR),
        true,
    ));

    let mut env = vec![
        format!("HOME={}", paths.home.display()),
        format!("TMPDIR={}", paths.tmp.display()),
    ];
    env.extend(spec.env.iter().map(|(key, value)| format!("{key}={value}")));

    let mut labels: HashMap<String, String> = spec.labels.clone().into_iter().collect();
    labels.extend(managed_labels(config, spec.volume.key(), spec.session));

    let memory = i64::try_from(config.memory_mb * 1024 * 1024)
        .map_err(|_| SandboxError::InvalidSpec("memory limit out of range"))?;
    let host_config = HostConfig {
        mounts: Some(mounts),
        network_mode: Some(config.network.clone()),
        readonly_rootfs: Some(true),
        privileged: Some(false),
        security_opt: Some(vec!["no-new-privileges".into()]),
        cap_drop: Some(vec!["ALL".into()]),
        memory: Some(memory),
        memory_swap: Some(memory),
        nano_cpus: Some((config.cpus * 1e9) as i64),
        pids_limit: Some(i64::from(config.pids_limit)),
        tmpfs: Some(HashMap::from([(
            "/tmp".to_string(),
            format!(
                "rw,exec,nosuid,nodev,size={}m,mode=1777",
                config.tmp_size_mb
            ),
        )])),
        init: Some(true),
        ..Default::default()
    };
    Ok(ContainerCreateBody {
        image: Some(spec.image.clone()),
        user: Some(format!("{}:{}", config.uid, config.gid)),
        cmd: Some(vec!["sleep".into(), "infinity".into()]),
        env: Some(env),
        working_dir: Some(paths.work.to_string_lossy().into_owned()),
        labels: Some(labels),
        stop_timeout: Some(i64::from(config.stop_timeout_secs)),
        attach_stdin: Some(false),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        open_stdin: Some(false),
        tty: Some(false),
        host_config: Some(host_config),
        ..Default::default()
    })
}

/// The paths inside a container for `spec`.
fn container_paths(spec: &SessionSpec) -> SessionPaths {
    let volume = Path::new(CONTAINER_VOLUME_DIR);
    let session = volume.join("sessions").join(spec.session.to_string());
    SessionPaths {
        work: session.join("work"),
        claude_config: session.join("claude"),
        home: session.join("home"),
        tmp: session.join("tmp"),
        persona_file: Path::new(CONTAINER_PERSONA_DIR).join(PERSONA_FILE),
        shared: volume.join("shared"),
        memory: (spec.memory && spec.volume.memory_dir().is_some()).then(|| volume.join("memory")),
    }
}

/// The labels every managed container carries.
fn managed_labels(
    config: &SandboxConfig,
    key: &VolumeKey,
    session: SessionId,
) -> [(String, String); 4] {
    [
        (LABEL_SESSION.into(), session.to_string()),
        (LABEL_AGENT.into(), key.agent.to_string()),
        (LABEL_SCOPE.into(), key.scope.to_string()),
        (LABEL_INSTANCE.into(), config.instance.clone()),
    ]
}

/// A bind mount's source: `local` as the Docker daemon sees it.
fn host_path(config: &SandboxConfig, data_dir: &Path, local: &Path) -> Result<String> {
    if !config::is_plain_absolute(local) {
        return Err(SandboxError::InvalidSpec(
            "mount sources must be absolute paths without `..`",
        ));
    }
    let path: PathBuf = match &config.host_data_dir {
        Some(host) => host.join(local.strip_prefix(data_dir).map_err(|_| {
            SandboxError::InvalidSpec("a mounted path is outside the data directory")
        })?),
        None => local.to_path_buf(),
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| SandboxError::InvalidSpec("a mounted path is not UTF-8"))
}

/// Label filters selecting this instance's managed containers.
fn managed_filters(config: &SandboxConfig) -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "label".to_string(),
        vec![
            LABEL_SESSION.to_string(),
            format!("{LABEL_INSTANCE}={}", config.instance),
        ],
    )])
}

/// A `die` event for one of this instance's containers, as a
/// [`ContainerEvent`]. Anything else is `None`.
fn died_event(config: &SandboxConfig, message: EventMessage) -> Option<ContainerEvent> {
    if message.action.as_deref() != Some("die") {
        return None;
    }
    let actor = message.actor?;
    let attributes = actor.attributes.unwrap_or_default();
    if attributes.get(LABEL_INSTANCE) != Some(&config.instance) {
        return None;
    }
    let session = attributes.get(LABEL_SESSION)?.parse().ok();
    Some(ContainerEvent::Died {
        container: ContainerId(actor.id?),
        session,
    })
}

/// Docker's `since` for events from `now` on. The request is sent only
/// when the stream is first polled, and Docker replays buffered events
/// from this time, so none between [`Sandbox::events`] and that poll is
/// lost.
fn since(now: std::time::SystemTime) -> String {
    let elapsed = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:09}", elapsed.as_secs(), elapsed.subsec_nanos())
}

/// Maps a bollard error. Docker's message is kept only when `keep_message`
/// is true: an exec request carries credentials in its environment, so its
/// errors drop the message.
fn docker_err(
    op: &'static str,
    keep_message: bool,
) -> impl Fn(bollard::errors::Error) -> SandboxError {
    move |err| {
        use bollard::errors::Error as E;
        let (status, message) = match err {
            E::DockerResponseServerError {
                status_code,
                message,
            } => (
                Some(status_code),
                keep_message.then(|| message.chars().take(300).collect()),
            ),
            E::RequestTimeoutError => (None, Some("timed out".into())),
            E::IOError { err } => (None, Some(err.to_string())),
            E::HyperResponseError { err } => (None, Some(err.to_string())),
            E::JsonDataError { .. } | E::JsonSerdeError { .. } => {
                (None, Some("unexpected response".into()))
            }
            _ => (None, None),
        };
        SandboxError::Docker {
            op,
            status,
            message,
        }
    }
}

fn status_of(err: &bollard::errors::Error) -> Option<u16> {
    match err {
        bollard::errors::Error::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// Splits the pid line [`EXEC_WRAPPER`] prints off the front of stdout.
#[derive(Debug, Default)]
struct PidSplitter {
    line: Vec<u8>,
    done: bool,
}

impl PidSplitter {
    /// The most bytes a pid line can take; anything longer isn't one.
    const MAX_LINE: usize = 24;

    /// Feeds a chunk of stdout. Returns the pid once its line is complete,
    /// and the bytes to pass on.
    fn feed<'a>(&mut self, chunk: &'a [u8]) -> (Option<u32>, std::borrow::Cow<'a, [u8]>) {
        use std::borrow::Cow;
        if self.done {
            return (None, Cow::Borrowed(chunk));
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(end) => {
                self.line.extend_from_slice(&chunk[..end]);
                self.done = true;
                let pid = std::str::from_utf8(&self.line)
                    .ok()
                    .and_then(|line| line.trim().parse().ok());
                if pid.is_some() {
                    (pid, Cow::Borrowed(&chunk[end + 1..]))
                } else {
                    let mut all = std::mem::take(&mut self.line);
                    all.extend_from_slice(&chunk[end..]);
                    (None, Cow::Owned(all))
                }
            }
            None if self.line.len() + chunk.len() > Self::MAX_LINE => {
                self.done = true;
                let mut all = std::mem::take(&mut self.line);
                all.extend_from_slice(chunk);
                (None, Cow::Owned(all))
            }
            None => {
                self.line.extend_from_slice(chunk);
                (None, Cow::Borrowed(&[]))
            }
        }
    }
}

/// A [`Sandbox`] running each session in its own Docker container, as
/// [`container_config`] describes.
///
/// agentd's data directory holds the volumes. It must be the same
/// directory the Docker daemon sees at [`SandboxConfig::host_data_dir`]
/// (or at the same path, when that is unset), and agentd must run as the
/// sandbox user or as root, because agent-writable directories are given
/// to the sandbox user.
#[derive(Debug, Clone)]
pub struct DockerSandbox {
    docker: Docker,
    config: SandboxConfig,
    layout: Layout,
}

impl DockerSandbox {
    /// Connects to the Docker daemon named by `DOCKER_HOST` (the local
    /// socket by default) and negotiates the API version.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Config`] for an invalid configuration,
    /// [`SandboxError::InvalidSpec`] for a bad `data_dir`, and
    /// [`SandboxError::Docker`] if the daemon can't be reached.
    pub async fn connect(
        store: Store,
        data_dir: impl Into<PathBuf>,
        config: SandboxConfig,
    ) -> Result<Self> {
        let docker = Docker::connect_with_defaults()
            .map_err(docker_err("connect", true))?
            .negotiate_version()
            .await
            .map_err(docker_err("version", true))?;
        Self::new(docker, store, data_dir, config)
    }

    /// A Docker sandbox over an existing client.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Config`] for an invalid configuration, and
    /// [`SandboxError::InvalidSpec`] unless `data_dir` is absolute without
    /// `..`.
    pub fn new(
        docker: Docker,
        store: Store,
        data_dir: impl Into<PathBuf>,
        config: SandboxConfig,
    ) -> Result<Self> {
        config.validate()?;
        let data_dir = data_dir.into();
        if !config::is_plain_absolute(&data_dir) {
            return Err(SandboxError::InvalidSpec(
                "the data directory must be absolute without `..`",
            ));
        }
        Ok(Self {
            layout: Layout {
                store,
                data_dir,
                owner: Some((config.uid, config.gid)),
                cleanup_period_days: config.cleanup_period_days,
            },
            docker,
            config,
        })
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::default().force(true).build();
        match self.docker.remove_container(id, Some(options)).await {
            Err(err) if !matches!(status_of(&err), Some(404 | 409)) => {
                Err(docker_err("remove container", true)(err))
            }
            _ => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for DockerSandbox {
    async fn ensure_volume(&self, key: &VolumeKey) -> Result<VolumeRef> {
        self.layout.ensure_volume(key).await
    }

    async fn start(&self, spec: &SessionSpec) -> Result<Container> {
        let body = container_config(&self.config, &self.layout.data_dir, spec)?;
        let dir = self
            .layout
            .prepare_session_dirs(&spec.volume, spec.session)
            .await?;
        if spec.skills_dir.is_some() {
            let layout = self.layout.clone();
            let mount_point = dir.join("claude").join("skills");
            tokio::task::spawn_blocking(move || layout.repair_dir(&mount_point))
                .await
                .map_err(|_| SandboxError::Io {
                    what: "filesystem task",
                    source: std::io::Error::other("the task panicked or was cancelled"),
                })??;
        }
        let created = self
            .docker
            .create_container(None, body)
            .await
            .map_err(docker_err("create container", true))?;
        if let Err(err) = self.docker.start_container(&created.id, None).await {
            let _ = self.remove(&created.id).await;
            return Err(docker_err("start container", true)(err));
        }
        tracing::debug!(container = %created.id, session = %spec.session, "sandbox started");
        Ok(Container {
            id: ContainerId(created.id),
            session: spec.session,
            paths: container_paths(spec),
        })
    }

    async fn exec(
        &self,
        container: &Container,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<ChildIo> {
        check_exec(argv, env)?;
        let cmd: Vec<String> = EXEC_WRAPPER
            .iter()
            .map(|arg| arg.to_string())
            .chain(argv.iter().cloned())
            .collect();
        let user = format!("{}:{}", self.config.uid, self.config.gid);
        let options = CreateExecOptions {
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(false),
            tty: Some(false),
            env: Some(env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            cmd: Some(cmd),
            user: Some(user.clone()),
            working_dir: Some(container.paths.work.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let exec = self
            .docker
            .create_exec(&container.id.0, options)
            .await
            .map_err(|err| match status_of(&err) {
                Some(404 | 409) => SandboxError::NotFound,
                _ => docker_err("create exec", false)(err),
            })?;
        let started = self
            .docker
            .start_exec(&exec.id, Some(StartExecOptions::default()))
            .await
            .map_err(docker_err("start exec", false))?;
        let StartExecResults::Attached { mut output, input } = started else {
            return Err(SandboxError::Docker {
                op: "start exec",
                status: None,
                message: Some("not attached".into()),
            });
        };
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        let (pid_tx, pid_rx) = watch::channel(None);
        let pump = tokio::spawn(async move {
            let mut splitter = PidSplitter::default();
            while let Some(Ok(item)) = output.next().await {
                let LogOutput::StdOut { message } = item else {
                    continue;
                };
                let (pid, rest) = splitter.feed(&message);
                if pid.is_some() {
                    let _ = pid_tx.send(pid);
                }
                if writer.write_all(&rest).await.is_err() {
                    break;
                }
            }
        });
        Ok(ChildIo {
            stdin: input,
            stdout: Box::pin(reader),
            child: ChildHandle(ChildInner::Docker(DockerChild {
                docker: self.docker.clone(),
                exec_id: exec.id,
                container: container.id.0.clone(),
                user,
                pid: pid_rx,
                pump: Some(pump),
            })),
        })
    }

    async fn ip(&self, container: &ContainerId) -> Result<IpAddr> {
        let inspected = self
            .docker
            .inspect_container(&container.0, None)
            .await
            .map_err(|err| match status_of(&err) {
                Some(404) => SandboxError::NotFound,
                _ => docker_err("inspect container", true)(err),
            })?;
        if !inspected
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false)
        {
            return Err(SandboxError::NotFound);
        }
        inspected
            .network_settings
            .and_then(|settings| settings.networks)
            .and_then(|mut networks| networks.remove(&self.config.network))
            .and_then(|endpoint| endpoint.ip_address)
            .and_then(|ip| ip.parse().ok())
            .ok_or(SandboxError::NoAddress)
    }

    async fn stop(&self, container: &ContainerId) -> Result<()> {
        let timeout = i32::try_from(self.config.stop_timeout_secs).unwrap_or(i32::MAX);
        let options = StopContainerOptionsBuilder::default().t(timeout).build();
        match self
            .docker
            .stop_container(&container.0, Some(options))
            .await
        {
            Err(err) if status_of(&err) == Some(404) => return Ok(()),
            Err(err) if status_of(&err) != Some(304) => {
                return Err(docker_err("stop container", true)(err));
            }
            _ => {}
        }
        self.remove(&container.0).await
    }

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>> {
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&managed_filters(&self.config))
            .build();
        let summaries = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(docker_err("list containers", true))?;
        Ok(summaries
            .into_iter()
            .filter_map(|summary| {
                let session = summary
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(LABEL_SESSION))
                    .and_then(|session| session.parse().ok());
                Some(ManagedContainer {
                    id: ContainerId(summary.id?),
                    session,
                    running: summary.state == Some(ContainerSummaryStateEnum::RUNNING),
                })
            })
            .collect())
    }

    fn events(&self) -> BoxStream<'static, Result<ContainerEvent>> {
        let mut filters = managed_filters(&self.config);
        filters.insert("type".into(), vec!["container".into()]);
        filters.insert("event".into(), vec!["die".into()]);
        let options = EventsOptionsBuilder::default()
            .since(&since(std::time::SystemTime::now()))
            .filters(&filters)
            .build();
        let events = self.docker.events(Some(options)).boxed();
        let state = Some((events, self.config.clone()));
        stream::unfold(state, |state| async move {
            let (mut events, config) = state?;
            loop {
                match events.next().await {
                    Some(Ok(message)) => {
                        if let Some(event) = died_event(&config, message) {
                            return Some((Ok(event), Some((events, config))));
                        }
                    }
                    Some(Err(_)) | None => return Some((Err(SandboxError::EventsMissed), None)),
                }
            }
        })
        .boxed()
    }
}

/// A process exec'd into a container, the [`DockerSandbox`] side of a
/// [`ChildHandle`].
#[derive(Debug)]
pub(crate) struct DockerChild {
    docker: Docker,
    exec_id: String,
    container: String,
    user: String,
    pid: watch::Receiver<Option<u32>>,
    pump: Option<JoinHandle<()>>,
}

impl DockerChild {
    pub(crate) async fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(pump) = self.pump.take() {
            let _ = pump.await;
        }
        let deadline = tokio::time::Instant::now() + EXIT_POLL;
        loop {
            let inspected = self
                .docker
                .inspect_exec(&self.exec_id)
                .await
                .map_err(docker_err("inspect exec", false))?;
            if inspected.running != Some(true) {
                return Ok(ExitStatus {
                    code: inspected
                        .exit_code
                        .and_then(|code| i32::try_from(code).ok()),
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(ExitStatus { code: None });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        let pid = tokio::time::timeout(PID_WAIT, self.pid.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(|pid| pid.ok().and_then(|pid| *pid));
        let Some(pid) = pid else {
            return Err(SandboxError::Docker {
                op: "kill",
                status: None,
                message: Some("the process's pid is unknown".into()),
            });
        };
        let options = CreateExecOptions {
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".into(),
                "kill -s KILL \"$1\"".into(),
                "sh".into(),
                pid.to_string(),
            ]),
            user: Some(self.user.clone()),
            ..Default::default()
        };
        let exec = self
            .docker
            .create_exec(&self.container, options)
            .await
            .map_err(docker_err("create exec", false))?;
        self.docker
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    detach: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(docker_err("start exec", false))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
