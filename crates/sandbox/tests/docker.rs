//! Docker tests: they need a Docker daemon, so they are ignored by default.
//! CI runs them with `cargo test --workspace -- --ignored docker_`.
//!
//! They use `debian:stable-slim` and run as the test process's own uid
//! (10001 when that is root), since they check mounts and isolation, not
//! the CLI. Each test uses its own `instance` label and its own internal
//! network, so tests running in parallel leave each other alone. A
//! fixture removes its containers, network and directory when dropped,
//! also when the test fails.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

use bollard::Docker;
use bollard::models::NetworkCreateRequest;
use bollard::query_parameters::{
    CreateImageOptionsBuilder, KillContainerOptionsBuilder, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder,
};
use core_types::{AgentId, ConvRef, ScopeKey, SessionId, SurfaceKind, VolumeKey};
use futures::StreamExt;
use sandbox::{
    Container, ContainerEvent, DockerSandbox, Sandbox, SandboxConfig, SessionSpec, SharedAccess,
    VolumeRef,
};
use store::{Sealer, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;

const IMAGE: &str = "debian:stable-slim";

struct Fixture {
    docker: Docker,
    sandbox: DockerSandbox,
    config: SandboxConfig,
    network: String,
    owns_network: bool,
    dir: PathBuf,
}

/// The network a fixture's sandboxes attach to.
enum Network<'a> {
    /// A new internal network, like the real `sandbox` network.
    Internal,
    /// A new network with a route out.
    Open,
    /// Another fixture's network.
    Shared(&'a str),
}

async fn connect() -> Docker {
    Docker::connect_with_defaults()
        .unwrap()
        .negotiate_version()
        .await
        .unwrap()
}

async fn pull_image(docker: &Docker) {
    static PULLED: OnceCell<()> = OnceCell::const_new();
    PULLED
        .get_or_init(|| async {
            let options = CreateImageOptionsBuilder::default()
                .from_image("debian")
                .tag("stable-slim")
                .build();
            let mut progress = docker.create_image(Some(options), None, None);
            while let Some(item) = progress.next().await {
                item.expect("pulling debian:stable-slim");
            }
        })
        .await;
}

impl Fixture {
    async fn new_on(network: Network<'_>) -> Self {
        let docker = connect().await;
        pull_image(&docker).await;
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (network, internal) = match network {
            Network::Shared(name) => (name.to_string(), None),
            Network::Internal => (format!("agentd-test-{tag}"), Some(true)),
            Network::Open => (format!("agentd-test-open-{tag}"), Some(false)),
        };
        if internal.is_some() {
            docker
                .create_network(NetworkCreateRequest {
                    name: network.clone(),
                    internal,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let dir = std::env::temp_dir().join(format!("sandbox-docker-{tag}"));
        std::fs::create_dir(&dir).unwrap();
        let me = std::fs::metadata(&dir).unwrap();
        let mut config = SandboxConfig::new(IMAGE);
        config.network = network.clone();
        config.instance = format!("test-{tag}");
        config.memory_mb = 256;
        config.cpus = 0.5;
        config.pids_limit = 128;
        config.stop_timeout_secs = 1;
        if me.uid() != 0 {
            config.uid = me.uid();
            config.gid = me.gid();
        }
        let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
        let store = Store::open_in_memory(sealer).await.unwrap();
        let sandbox = DockerSandbox::new(docker.clone(), store, &dir, config.clone()).unwrap();
        std::fs::create_dir_all(dir.join("agents/a1")).unwrap();
        std::fs::write(dir.join("agents/a1/persona.md"), "You are a test.\n").unwrap();
        std::fs::create_dir_all(dir.join("skills/a1/s1")).unwrap();
        std::fs::write(dir.join("skills/a1/s1/SKILL.md"), "skill\n").unwrap();
        Self {
            docker,
            sandbox,
            config,
            network,
            owns_network: internal.is_some(),
            dir,
        }
    }

    async fn new() -> Self {
        Self::new_on(Network::Internal).await
    }

    async fn volume(&self, agent: AgentId, scope: ScopeKey) -> VolumeRef {
        self.sandbox
            .ensure_volume(&VolumeKey { agent, scope })
            .await
            .unwrap()
    }

    fn spec(&self, volume: &VolumeRef) -> SessionSpec {
        let mut spec = SessionSpec::new(
            SessionId::new_v4(),
            volume.clone(),
            IMAGE,
            self.dir.join("agents/a1"),
        );
        spec.skills_dir = Some(self.dir.join("skills/a1"));
        spec.shared = SharedAccess::ReadWrite;
        spec
    }

    async fn start(&self, spec: &SessionSpec) -> Container {
        self.sandbox.start(spec).await.unwrap()
    }

    /// Runs `script` with `sh -c` in the container, feeding it `stdin`.
    async fn run_with(&self, container: &Container, script: &str, stdin: &str) -> (i32, String) {
        let argv = ["sh", "-c", script].map(String::from);
        let env = BTreeMap::from([("TEST_VAR".to_string(), "set".to_string())]);
        let mut io = self.sandbox.exec(container, &argv, &env).await.unwrap();
        io.stdin.write_all(stdin.as_bytes()).await.unwrap();
        io.stdin.shutdown().await.unwrap();
        let mut out = String::new();
        io.stdout.read_to_string(&mut out).await.unwrap();
        let status = io.child.wait().await.unwrap();
        (status.code.unwrap_or(-1), out)
    }

    async fn run(&self, container: &Container, script: &str) -> (i32, String) {
        self.run_with(container, script, "").await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let instance = self.config.instance.clone();
        let network = self.owns_network.then(|| self.network.clone());
        let cleanup = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(remove_leftovers(&instance, network.as_deref()));
        });
        let _ = cleanup.join();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Force-removes every container of `instance`, then `network`. It runs on
/// its own runtime, since a fixture may be dropped while its test's runtime
/// unwinds.
async fn remove_leftovers(instance: &str, network: Option<&str>) {
    let docker = connect().await;
    let filters = HashMap::from([(
        "label".to_string(),
        vec![format!("{}={instance}", sandbox::LABEL_INSTANCE)],
    )]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    for container in docker
        .list_containers(Some(options))
        .await
        .unwrap_or_default()
    {
        if let Some(id) = container.id {
            let options = RemoveContainerOptionsBuilder::default().force(true).build();
            let _ = docker.remove_container(&id, Some(options)).await;
        }
    }
    if let Some(network) = network {
        let _ = docker.remove_network(network).await;
    }
}

fn channel() -> ScopeKey {
    ScopeKey::Channel(ConvRef {
        surface: SurfaceKind::RocketChat,
        team: "chat.example.org:3000".into(),
        conversation: "room%2Fx:y".into(),
    })
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_a_session_cannot_see_another_sessions_directory() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let spec_a = fx.spec(&volume);
    let spec_b = fx.spec(&volume);
    let a = fx.start(&spec_a).await;
    let b = fx.start(&spec_b).await;
    assert_eq!(fx.run(&a, "echo secret > note").await.0, 0);
    let (code, out) = fx.run(&a, "ls /volume/sessions").await;
    assert_eq!((code, out.trim()), (0, spec_a.session.to_string().as_str()));
    let (code, _) = fx
        .run(&b, &format!("test -e /volume/sessions/{}", spec_a.session))
        .await;
    assert_ne!(code, 0);
    let (code, out) = fx.run(&b, "ls /volume/sessions").await;
    assert_eq!((code, out.trim()), (0, spec_b.session.to_string().as_str()));
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_shared_is_visible_across_sessions() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let a = fx.start(&fx.spec(&volume)).await;
    let b = fx.start(&fx.spec(&volume)).await;
    assert_eq!(
        fx.run(&a, "echo hello > /volume/shared/greeting").await.0,
        0
    );
    assert_eq!(
        fx.run(&b, "cat /volume/shared/greeting").await,
        (0, "hello\n".into())
    );
    let other_agent = fx.volume(AgentId::new_v4(), channel()).await;
    let c = fx.start(&fx.spec(&other_agent)).await;
    assert_ne!(fx.run(&c, "test -e /volume/shared/greeting").await.0, 0);
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_skills_and_persona_are_read_only() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let spec = fx.spec(&volume);
    let container = fx.start(&spec).await;
    let paths = container.paths();
    let persona = paths.persona_file.display();
    let skills = paths.claude_config.join("skills");
    let skills = skills.display();
    assert_eq!(
        fx.run(&container, &format!("cat {persona}")).await,
        (0, "You are a test.\n".into())
    );
    assert_eq!(
        fx.run(&container, &format!("cat {skills}/s1/SKILL.md"))
            .await,
        (0, "skill\n".into())
    );
    for target in [
        "/agent/new".to_string(),
        format!("{skills}/new"),
        format!("{skills}/s1/SKILL.md"),
    ] {
        let (code, _) = fx.run(&container, &format!("echo x >> {target}")).await;
        assert_ne!(code, 0, "{target}");
    }
    let settings = paths.claude_config.join("settings.json");
    let (code, out) = fx
        .run(&container, &format!("cat {}", settings.display()))
        .await;
    assert_eq!(code, 0);
    assert!(out.contains("\"cleanupPeriodDays\": 3650"), "{out}");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_shared_can_be_read_only_and_memory_is_absent_unless_requested() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), ScopeKey::Private).await;
    let mut owner = fx.spec(&volume);
    owner.memory = true;
    let owner = fx.start(&owner).await;
    assert_eq!(
        fx.run(
            &owner,
            "echo dm > /volume/memory/m && echo s > /volume/shared/s"
        )
        .await
        .0,
        0
    );
    let mut task = fx.spec(&volume);
    task.shared = SharedAccess::ReadOnly;
    let task = fx.start(&task).await;
    assert_eq!(
        fx.run(&task, "cat /volume/shared/s").await,
        (0, "s\n".into())
    );
    assert_ne!(fx.run(&task, "echo x > /volume/shared/t").await.0, 0);
    assert_ne!(fx.run(&task, "test -e /volume/memory").await.0, 0);
    assert_eq!(task.paths().memory, None);
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_runs_as_a_non_root_user_on_a_read_only_root() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let container = fx.start(&fx.spec(&volume)).await;
    let (code, out) = fx.run(&container, "id -u; id -g").await;
    assert_eq!(code, 0);
    assert_eq!(out, format!("{}\n{}\n", fx.config.uid, fx.config.gid));
    assert_ne!(fx.config.uid, 0);
    assert_ne!(fx.run(&container, "touch /etc/x").await.0, 0);
    assert_ne!(fx.run(&container, "touch /x").await.0, 0);
    let (code, out) = fx
        .run(
            &container,
            "grep -E '^(CapEff|NoNewPrivs)' /proc/self/status",
        )
        .await;
    assert_eq!(code, 0);
    assert!(out.contains("CapEff:\t0000000000000000"), "{out}");
    assert!(out.contains("NoNewPrivs:\t1"), "{out}");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_home_and_tmp_are_writable_and_scripts_run_there() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let container = fx.start(&fx.spec(&volume)).await;
    let script = r#"
        set -e
        test "$HOME" = "$(cd ~ && pwd)"
        echo x > "$HOME/.config-test"
        for dir in /tmp "$TMPDIR" "$HOME"; do
            printf '#!/bin/sh\necho ran\n' > "$dir/s.sh"
            chmod +x "$dir/s.sh"
            "$dir/s.sh"
        done
        echo "$TEST_VAR"
        pwd
    "#;
    let (code, out) = fx.run(&container, script).await;
    let paths = container.paths();
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        format!("ran\nran\nran\nset\n{}\n", paths.work.display())
    );
    let (_, out) = fx.run(&container, "echo \"$HOME $TMPDIR\"").await;
    assert_eq!(
        out.trim(),
        format!("{} {}", paths.home.display(), paths.tmp.display())
    );
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_the_internet_is_unreachable() {
    let by_name = "timeout 10 bash -c 'echo > /dev/tcp/example.com/443'";
    let by_address = "timeout 10 bash -c 'echo > /dev/tcp/1.1.1.1/443'";
    let open = Fixture::new_on(Network::Open).await;
    let volume = open.volume(AgentId::new_v4(), channel()).await;
    let container = open.start(&open.spec(&volume)).await;
    let (code, _) = open.run(&container, by_name).await;
    assert_eq!(code, 0, "the probe must work where there is a route out");
    drop(open);

    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let container = fx.start(&fx.spec(&volume)).await;
    assert_ne!(fx.run(&container, by_name).await.0, 0);
    assert_ne!(fx.run(&container, by_address).await.0, 0);
    let ip = fx.sandbox.ip(container.id()).await.unwrap();
    assert!(!ip.is_loopback() && !ip.is_unspecified(), "{ip}");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_exec_pipes_stdio_and_kill_stops_the_process() {
    let fx = Fixture::new().await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let container = fx.start(&fx.spec(&volume)).await;
    assert_eq!(
        fx.run_with(&container, "cat; echo done", "line one\nline two\n")
            .await,
        (0, "line one\nline two\ndone\n".into())
    );
    assert_eq!(fx.run(&container, "exit 7").await.0, 7);

    let argv = ["sleep", "600"].map(String::from);
    let mut io = fx
        .sandbox
        .exec(&container, &argv, &BTreeMap::new())
        .await
        .unwrap();
    io.child.kill().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), io.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code, Some(137));
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_reap_orphans_stops_only_this_instances_containers() {
    let fx = Fixture::new().await;
    let other = Fixture::new_on(Network::Shared(&fx.network)).await;
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let a = fx.start(&fx.spec(&volume)).await;
    let b = fx.start(&fx.spec(&volume)).await;
    let other_volume = other.volume(AgentId::new_v4(), channel()).await;
    let kept = other.start(&other.spec(&other_volume)).await;

    let listed = fx.sandbox.list_managed().await.unwrap();
    let mut ids: Vec<_> = listed.iter().map(|found| found.id.clone()).collect();
    ids.sort();
    let mut expected = vec![a.id().clone(), b.id().clone()];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(
        listed
            .iter()
            .all(|found| found.running && found.session.is_some())
    );

    assert_eq!(fx.sandbox.reap_orphans().await.unwrap(), 2);
    assert!(fx.sandbox.list_managed().await.unwrap().is_empty());
    assert!(fx.sandbox.ip(a.id()).await.is_err());
    let still = other.sandbox.list_managed().await.unwrap();
    assert_eq!(still.len(), 1);
    assert_eq!(&still[0].id, kept.id());
    fx.sandbox.stop(a.id()).await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_a_killed_container_produces_a_die_event() {
    let fx = Fixture::new().await;
    let mut events = fx.sandbox.events();
    let volume = fx.volume(AgentId::new_v4(), channel()).await;
    let spec = fx.spec(&volume);
    let container = fx.start(&spec).await;
    let options = KillContainerOptionsBuilder::default()
        .signal("KILL")
        .build();
    fx.docker
        .kill_container(&container.id().0, Some(options))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(30), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        ContainerEvent::Died {
            container: container.id().clone(),
            session: Some(spec.session),
        }
    );
}
