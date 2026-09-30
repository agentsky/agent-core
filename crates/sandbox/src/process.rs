//! [`ProcessSandbox`]: sessions as local processes, for tests and
//! Docker-less development.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use core_types::{SessionId, VolumeKey};
use futures::stream::{self, BoxStream, StreamExt};
use store::Store;
use tokio::sync::broadcast;

use crate::layout::{Layout, SkillsEntry};
use crate::{
    ChildHandle, ChildInner, ChildIo, Container, ContainerEvent, ContainerId, ExitStatus,
    ManagedContainer, PERSONA_FILE, Result, Sandbox, SandboxError, SessionPaths, SessionSpec,
    VolumeRef, check_exec, check_spec, config,
};

/// A [`Sandbox`] whose "containers" are the session directories on the
/// host, under the data directory the caller gives (a temp directory in
/// tests), and whose processes are local children.
///
/// **It isolates nothing.** Processes run as agentd's own user with full
/// access to the host and the network. `shared/` is writable whatever the
/// spec says, and nothing hides other sessions' directories. It exists so
/// the runner and the turn pipeline can be tested end to end without
/// Docker, and must never run untrusted agents.
///
/// What it does keep from the Docker sandbox:
///
/// - The same directory layout, volumes table and `settings.json`, from
///   the same code.
/// - [`SessionPaths`] point at the host directories; the skills directory
///   appears as a symlink at `claude/skills`.
/// - `exec` clears the environment and sets `HOME`, `TMPDIR`, the spec's
///   environment and then `exec`'s, runs in the work directory in a new
///   process group, and passes `LLVM_PROFILE_FILE` through from agentd's
///   environment (unless `exec`'s `env` sets it), so instrumented test
///   binaries keep their coverage. No `PATH` is set unless given.
/// - Spawns are serialized across all process sandboxes, so a child's
///   pipes are open only in the child and in agentd: once its stdin is
///   closed, a write to it fails.
/// - [`ip`](Sandbox::ip) is `127.0.0.1` while the container runs.
/// - [`stop`](Sandbox::stop) kills each process group it started whose
///   leader hasn't been reaped, and reports the container
///   [`Died`](ContainerEvent::Died).
#[derive(Debug, Clone)]
pub struct ProcessSandbox {
    layout: Layout,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    containers: Mutex<HashMap<ContainerId, Record>>,
    events: broadcast::Sender<ContainerEvent>,
}

#[derive(Debug)]
struct Record {
    session: SessionId,
    env: BTreeMap<String, String>,
    children: Vec<Arc<ChildState>>,
}

#[derive(Debug)]
struct ChildState {
    pgid: u32,
    reaped: AtomicBool,
}

impl ProcessSandbox {
    /// A process sandbox keeping volumes under `data_dir/volumes` and
    /// recording them in `store`, with the default `cleanupPeriodDays`.
    ///
    /// # Errors
    ///
    /// [`SandboxError::InvalidSpec`] unless `data_dir` is absolute without
    /// `..`.
    pub fn new(store: Store, data_dir: impl Into<PathBuf>) -> Result<Self> {
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
                owner: None,
                cleanup_period_days: config::DEFAULT_CLEANUP_PERIOD_DAYS,
            },
            inner: Arc::new(Inner {
                containers: Mutex::new(HashMap::new()),
                events: broadcast::channel(1024).0,
            }),
        })
    }

    /// Sets the `cleanupPeriodDays` written to `settings.json`.
    pub fn with_cleanup_period_days(mut self, days: u32) -> Self {
        self.layout.cleanup_period_days = days;
        self
    }

    fn containers(&self) -> MutexGuard<'_, HashMap<ContainerId, Record>> {
        self.inner
            .containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Sandbox for ProcessSandbox {
    async fn ensure_volume(&self, key: &VolumeKey) -> Result<VolumeRef> {
        self.layout.ensure_volume(key).await
    }

    async fn start(&self, spec: &SessionSpec) -> Result<Container> {
        check_spec(spec)?;
        let dir = self
            .layout
            .prepare_session_dirs(
                &spec.volume,
                spec.session,
                SkillsEntry::Link(spec.skills_dir.clone()),
            )
            .await?;
        let paths = SessionPaths {
            work: dir.join("work"),
            claude_config: dir.join("claude"),
            home: dir.join("home"),
            tmp: dir.join("tmp"),
            persona_file: spec.persona_dir.join(PERSONA_FILE),
            shared: spec.volume.shared_dir(),
            memory: spec.memory.then(|| spec.volume.memory_dir()).flatten(),
        };
        let mut env = spec.env.clone();
        env.insert("HOME".into(), paths.home.to_string_lossy().into_owned());
        env.insert("TMPDIR".into(), paths.tmp.to_string_lossy().into_owned());
        let id = ContainerId(format!("process-{}", uuid::Uuid::new_v4()));
        self.containers().insert(
            id.clone(),
            Record {
                session: spec.session,
                env,
                children: Vec::new(),
            },
        );
        Ok(Container {
            id,
            session: spec.session,
            paths,
        })
    }

    async fn exec(
        &self,
        container: &Container,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<ChildIo> {
        check_exec(argv, env)?;
        let mut containers = self.containers();
        let record = containers
            .get_mut(&container.id)
            .ok_or(SandboxError::NotFound)?;
        let mut command = tokio::process::Command::new(&argv[0]);
        command.args(&argv[1..]).env_clear().envs(&record.env);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
            .envs(env)
            .current_dir(&container.paths.work)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let spawned = {
            let _spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
            command.spawn()
        };
        let mut child = spawned.map_err(|source| SandboxError::Io {
            what: "spawning a process",
            source,
        })?;
        let (Some(stdin), Some(stdout), Some(pgid)) =
            (child.stdin.take(), child.stdout.take(), child.id())
        else {
            return Err(SandboxError::Io {
                what: "spawning a process",
                source: std::io::Error::other("the child has no pipes"),
            });
        };
        let state = Arc::new(ChildState {
            pgid,
            reaped: AtomicBool::new(false),
        });
        record
            .children
            .retain(|child| !child.reaped.load(Ordering::SeqCst));
        record.children.push(Arc::clone(&state));
        Ok(ChildIo {
            stdin: Box::pin(ClosingStdin(Some(stdin))),
            stdout: Box::pin(stdout),
            child: ChildHandle(ChildInner::Process(ProcessChild { child, state })),
        })
    }

    async fn ip(&self, container: &ContainerId) -> Result<IpAddr> {
        if self.containers().contains_key(container) {
            Ok(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            Err(SandboxError::NotFound)
        }
    }

    async fn stop(&self, container: &ContainerId) -> Result<()> {
        let Some(record) = self.containers().remove(container) else {
            return Ok(());
        };
        for child in &record.children {
            if !child.reaped.load(Ordering::SeqCst) {
                kill_group(child.pgid);
            }
        }
        let _ = self.inner.events.send(ContainerEvent::Died {
            container: container.clone(),
            session: Some(record.session),
        });
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>> {
        let mut found: Vec<ManagedContainer> = self
            .containers()
            .iter()
            .map(|(id, record)| ManagedContainer {
                id: id.clone(),
                session: Some(record.session),
                running: true,
            })
            .collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(found)
    }

    fn events(&self) -> BoxStream<'static, Result<ContainerEvent>> {
        let receiver = self.inner.events.subscribe();
        stream::unfold(Some(receiver), |receiver| async move {
            let mut receiver = receiver?;
            match receiver.recv().await {
                Ok(event) => Some((Ok(event), Some(receiver))),
                Err(_) => Some((Err(SandboxError::EventsMissed), None)),
            }
        })
        .fuse()
        .boxed()
    }
}

/// Held around every spawn, by every process sandbox in the process.
///
/// A child starts with a copy of each of agentd's descriptors and holds it
/// until its exec closes it. Without the lock, a spawn on another thread
/// could fork a child while this spawn's pipes were open, or while a file
/// was open for writing, and a child that waited to be scheduled held them
/// on into the life of the process spawned here: a write to a process that
/// had closed its stdin found a reader and succeeded, and an executable
/// just written failed to start with `ETXTBSY`. A spawn returns only once
/// its child has exec'd, so with one spawn at a time no pre-exec child is
/// left holding another's pipes or files.
static SPAWNING: Mutex<()> = Mutex::new(());

/// Sends SIGKILL to process group `pgid`. It is a plain `kill(2)`, which
/// returns at once, so `Drop` can call it too.
fn kill_group(pgid: u32) {
    if let Some(group) = i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

/// A child's stdin whose shutdown closes the pipe, as shutting down a
/// Docker exec's stdin does. A bare `ChildStdin` closes only when dropped.
struct ClosingStdin(Option<tokio::process::ChildStdin>);

impl tokio::io::AsyncWrite for ClosingStdin {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.0.as_mut() {
            Some(stdin) => std::pin::Pin::new(stdin).poll_write(cx, buf),
            None => std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.0.as_mut() {
            Some(stdin) => std::pin::Pin::new(stdin).poll_flush(cx),
            None => std::task::Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let flushed = self.as_mut().poll_flush(cx);
        if flushed.is_ready() {
            self.0 = None;
        }
        flushed
    }
}

/// A local child process, the [`ProcessSandbox`] side of a
/// [`ChildHandle`].
#[derive(Debug)]
pub(crate) struct ProcessChild {
    child: tokio::process::Child,
    state: Arc<ChildState>,
}

impl ProcessChild {
    pub(crate) async fn wait(&mut self) -> Result<ExitStatus> {
        let status = self.child.wait().await.map_err(|source| SandboxError::Io {
            what: "waiting for a process",
            source,
        })?;
        self.state.reaped.store(true, Ordering::SeqCst);
        Ok(ExitStatus {
            code: status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal)),
        })
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        if !self.state.reaped.load(Ordering::SeqCst) {
            kill_group(self.state.pgid);
        }
        let _ = self.child.start_kill();
        Ok(())
    }
}

impl Drop for ProcessChild {
    fn drop(&mut self) {
        if !self.state.reaped.swap(true, Ordering::SeqCst)
            && matches!(self.child.try_wait(), Ok(None))
        {
            kill_group(self.state.pgid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use core_types::{AgentId, ScopeKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::SharedAccess;
    use crate::test_util::{TempDir, awkward_channel, memory_store};

    async fn sandbox(dir: &TempDir) -> ProcessSandbox {
        ProcessSandbox::new(memory_store().await, dir.0.clone()).unwrap()
    }

    async fn started(sandbox: &ProcessSandbox, dir: &TempDir, scope: ScopeKey) -> Container {
        let volume = sandbox
            .ensure_volume(&VolumeKey {
                agent: AgentId::new_v4(),
                scope,
            })
            .await
            .unwrap();
        let spec = SessionSpec::new(
            SessionId::new_v4(),
            volume,
            "unused",
            dir.0.join("agents/a1"),
        );
        sandbox.start(&spec).await.unwrap()
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    async fn run(
        sandbox: &ProcessSandbox,
        container: &Container,
        script: &str,
        env: &BTreeMap<String, String>,
        stdin: &str,
    ) -> (ExitStatus, String) {
        let mut io = sandbox
            .exec(container, &argv(&["/bin/sh", "-c", script]), env)
            .await
            .unwrap();
        io.stdin.write_all(stdin.as_bytes()).await.unwrap();
        io.stdin.shutdown().await.unwrap();
        assert!(io.stdin.write_all(b"late").await.is_err());
        let mut out = String::new();
        io.stdout.read_to_string(&mut out).await.unwrap();
        (io.child.wait().await.unwrap(), out)
    }

    #[tokio::test]
    async fn a_relative_data_dir_is_refused() {
        assert!(matches!(
            ProcessSandbox::new(memory_store().await, "data"),
            Err(SandboxError::InvalidSpec(_))
        ));
    }

    #[tokio::test]
    async fn two_agents_in_one_channel_get_two_volumes() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let scope = awkward_channel();
        let a = VolumeKey {
            agent: AgentId::new_v4(),
            scope: scope.clone(),
        };
        let b = VolumeKey {
            agent: AgentId::new_v4(),
            scope,
        };
        let va = sandbox.ensure_volume(&a).await.unwrap();
        let vb = sandbox.ensure_volume(&b).await.unwrap();
        assert_ne!(va.path(), vb.path());
        assert_ne!(va.shared_dir(), vb.shared_dir());
        assert_eq!(va.path().file_name(), vb.path().file_name());
        let rows = [
            sandbox.layout.store.volume(&a).await.unwrap().unwrap(),
            sandbox.layout.store.volume(&b).await.unwrap().unwrap(),
        ];
        assert_ne!(rows[0].path, rows[1].path);
    }

    #[tokio::test]
    async fn the_session_layout_and_settings() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await.with_cleanup_period_days(9);
        let volume = sandbox
            .ensure_volume(&VolumeKey {
                agent: AgentId::new_v4(),
                scope: ScopeKey::Private,
            })
            .await
            .unwrap();
        let spec = SessionSpec::new(
            SessionId::new_v4(),
            volume.clone(),
            "unused",
            dir.0.join("agents/a1"),
        );
        sandbox.start(&spec).await.unwrap();
        let session_dir = volume.session_dir(spec.session);
        for sub in ["work", "claude", "home", "tmp"] {
            assert!(session_dir.join(sub).is_dir(), "{sub}");
        }
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(session_dir.join("claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings, serde_json::json!({"cleanupPeriodDays": 9}));

        let default = ProcessSandbox::new(memory_store().await, dir.0.clone()).unwrap();
        default.start(&spec).await.unwrap();
        let settings = std::fs::read_to_string(session_dir.join("claude/settings.json")).unwrap();
        assert!(settings.contains("3650"), "{settings}");
    }

    #[tokio::test]
    async fn start_gives_host_paths_and_links_skills() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let skills = dir.0.join("skills/a1");
        std::fs::create_dir_all(skills.join("s1")).unwrap();
        std::fs::write(skills.join("s1/SKILL.md"), "skill").unwrap();
        let volume = sandbox
            .ensure_volume(&VolumeKey {
                agent: AgentId::new_v4(),
                scope: ScopeKey::Private,
            })
            .await
            .unwrap();
        let mut spec = SessionSpec::new(
            SessionId::new_v4(),
            volume.clone(),
            "unused",
            dir.0.join("agents/a1"),
        );
        spec.skills_dir = Some(skills.clone());
        spec.memory = true;
        spec.shared = SharedAccess::ReadWrite;
        let container = sandbox.start(&spec).await.unwrap();
        let session_dir = volume.session_dir(spec.session);
        assert_eq!(container.session(), spec.session);
        assert_eq!(
            container.paths(),
            &SessionPaths {
                work: session_dir.join("work"),
                claude_config: session_dir.join("claude"),
                home: session_dir.join("home"),
                tmp: session_dir.join("tmp"),
                persona_file: dir.0.join("agents/a1/persona.md"),
                shared: volume.shared_dir(),
                memory: volume.memory_dir(),
            }
        );
        let skill = container.paths().claude_config.join("skills/s1/SKILL.md");
        assert_eq!(std::fs::read_to_string(skill).unwrap(), "skill");

        spec.skills_dir = None;
        spec.memory = false;
        let again = sandbox.start(&spec).await.unwrap();
        assert!(!again.paths().claude_config.join("skills").exists());
        assert_eq!(again.paths().memory, None);
        assert_ne!(again.id(), container.id());
    }

    #[tokio::test]
    async fn exec_pipes_stdio_with_only_the_given_environment() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let container = started(&sandbox, &dir, ScopeKey::Private).await;
        let env = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);
        let (status, out) = run(
            &sandbox,
            &container,
            "echo \"$HOME|$TMPDIR|$FOO|${CARGO_PKG_NAME-unset}|$(pwd -P)\"; cat",
            &env,
            "from stdin\n",
        )
        .await;
        assert!(status.success());
        let paths = container.paths();
        let work = std::fs::canonicalize(&paths.work).unwrap();
        assert_eq!(
            out,
            format!(
                "{}|{}|bar|unset|{}\nfrom stdin\n",
                paths.home.display(),
                paths.tmp.display(),
                work.display()
            )
        );
        let (status, _) = run(&sandbox, &container, "exit 3", &BTreeMap::new(), "").await;
        assert_eq!(status.code, Some(3));
    }

    #[tokio::test]
    async fn exec_passes_llvm_profile_file_through() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let container = started(&sandbox, &dir, awkward_channel()).await;
        let script = "echo \"${LLVM_PROFILE_FILE-unset}\"";
        let (_, out) = run(&sandbox, &container, script, &BTreeMap::new(), "").await;
        let expected = std::env::var("LLVM_PROFILE_FILE").unwrap_or_else(|_| "unset".into());
        assert_eq!(out.trim_end(), expected);
        let env = BTreeMap::from([("LLVM_PROFILE_FILE".to_string(), "given".to_string())]);
        let (_, out) = run(&sandbox, &container, script, &env, "").await;
        assert_eq!(out.trim_end(), "given");
    }

    #[tokio::test]
    async fn fake_claude_runs_at_the_container_paths_and_keeps_its_coverage() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        std::fs::create_dir_all(dir.0.join("agents/a1")).unwrap();
        std::fs::write(dir.0.join("agents/a1/persona.md"), "persona").unwrap();
        let container = started(&sandbox, &dir, ScopeKey::Private).await;
        let paths = container.paths();
        let session = container.session().to_string();
        let mut argv = vec![testkit::fake_claude_path().to_string_lossy().into_owned()];
        argv.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--append-system-prompt-file",
                &paths.persona_file.to_string_lossy(),
                "--session-id",
                &session,
            ]
            .map(String::from),
        );
        let env = BTreeMap::from([
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "http://127.0.0.1:9".to_string(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                paths.claude_config.to_string_lossy().into_owned(),
            ),
            ("CLAUDE_CODE_PROJECT_DIR_NAME".to_string(), session.clone()),
            (
                testkit::claude::SCRIPT_ENV.to_string(),
                dir.0.join("script.json").to_string_lossy().into_owned(),
            ),
        ]);
        let mut io = sandbox.exec(&container, &argv, &env).await.unwrap();
        io.stdin.shutdown().await.unwrap();
        let mut out = Vec::new();
        io.stdout.read_to_end(&mut out).await.unwrap();
        assert!(io.child.wait().await.unwrap().success());
        let stray: Vec<_> = std::fs::read_dir(&paths.work)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    #[tokio::test]
    async fn kill_ends_the_process_group() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let container = started(&sandbox, &dir, ScopeKey::Private).await;
        let mut io = sandbox
            .exec(
                &container,
                &argv(&["/bin/sh", "-c", "sleep 30 & echo started; wait"]),
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        let mut line = [0u8; 8];
        io.stdout.read_exact(&mut line).await.unwrap();
        io.child.kill().await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), io.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code, Some(128 + 9));
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(10), io.stdout.read_to_end(&mut rest));
        read.await.unwrap().unwrap();
        io.child.kill().await.unwrap();
    }

    fn is_gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z')),
        }
    }

    #[test]
    fn dropping_a_child_on_a_current_thread_runtime_kills_its_group() {
        let dir = TempDir::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let grandchild = runtime.block_on(async {
            let sandbox = sandbox(&dir).await;
            let container = started(&sandbox, &dir, ScopeKey::Private).await;
            let mut io = sandbox
                .exec(
                    &container,
                    &argv(&["/bin/sh", "-c", "sleep 30 & echo $!; wait"]),
                    &BTreeMap::new(),
                )
                .await
                .unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while io.stdout.read_exact(&mut byte).await.is_ok() && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            drop(io);
            String::from_utf8(line).unwrap()
        });
        drop(runtime);
        assert!(!grandchild.is_empty());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !is_gone(&grandchild) {
            assert!(
                std::time::Instant::now() < deadline,
                "process {grandchild} survived"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[tokio::test]
    async fn stop_kills_processes_and_reports_the_death() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let mut events = sandbox.events();
        let container = started(&sandbox, &dir, ScopeKey::Private).await;
        assert_eq!(
            sandbox.ip(container.id()).await.unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        let mut io = sandbox
            .exec(
                &container,
                &argv(&["/bin/sh", "-c", "sleep 30 & echo started; wait"]),
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        let mut line = [0u8; 8];
        io.stdout.read_exact(&mut line).await.unwrap();
        sandbox.stop(container.id()).await.unwrap();
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(10), io.stdout.read_to_end(&mut rest));
        read.await.unwrap().unwrap();
        assert_eq!(io.child.wait().await.unwrap().code, Some(128 + 9));
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            ContainerEvent::Died {
                container: container.id().clone(),
                session: Some(container.session()),
            }
        );
        assert!(matches!(
            sandbox.ip(container.id()).await,
            Err(SandboxError::NotFound)
        ));
        assert!(matches!(
            sandbox
                .exec(&container, &argv(&["/bin/true"]), &BTreeMap::new())
                .await,
            Err(SandboxError::NotFound)
        ));
        sandbox.stop(container.id()).await.unwrap();
    }

    #[tokio::test]
    async fn events_end_only_after_events_missed() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let mut events = sandbox.events();
        drop(sandbox);
        let items: Vec<_> = events.by_ref().collect().await;
        assert!(
            matches!(items.as_slice(), [Err(SandboxError::EventsMissed)]),
            "{items:?}"
        );
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn reap_orphans_stops_every_container() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let a = started(&sandbox, &dir, ScopeKey::Private).await;
        let b = started(&sandbox, &dir, awkward_channel()).await;
        let listed = sandbox.list_managed().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|found| found.running));
        let sessions: Vec<_> = listed.iter().map(|found| found.session).collect();
        assert!(sessions.contains(&Some(a.session())));
        assert!(sessions.contains(&Some(b.session())));
        assert_eq!(sandbox.reap_orphans().await.unwrap(), 2);
        assert!(sandbox.list_managed().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_spawn_failure_is_an_io_error() {
        let dir = TempDir::new();
        let sandbox = sandbox(&dir).await;
        let container = started(&sandbox, &dir, ScopeKey::Private).await;
        let err = sandbox
            .exec(
                &container,
                &argv(&["/nonexistent/binary"]),
                &BTreeMap::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Io { .. }), "{err:?}");
        let mut spec = SessionSpec::new(
            SessionId::new_v4(),
            sandbox
                .ensure_volume(&VolumeKey {
                    agent: AgentId::new_v4(),
                    scope: awkward_channel(),
                })
                .await
                .unwrap(),
            "unused",
            dir.0.join("agents/a1"),
        );
        spec.memory = true;
        assert!(matches!(
            sandbox.start(&spec).await,
            Err(SandboxError::InvalidSpec(_))
        ));
    }
}
