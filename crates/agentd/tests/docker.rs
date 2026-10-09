//! Docker tests: they need a Docker daemon, so they are ignored by default.
//! CI runs them with `cargo test --workspace -- --ignored docker_`.
//!
//! `docker_startup_reaps_only_this_instances_sandboxes` plants containers
//! from `debian:stable-slim`, which it pulls if it is missing, and checks that
//! [`connect_docker`](agentd::pipeline::connect_docker) stops only the one
//! labeled as this instance's sandbox. It removes what it planted when it
//! ends, also when it fails.
//!
//! `docker_real_claude_starts` runs the real `claude`, from the sandbox
//! image built from `images/sandbox/Dockerfile`, which CI builds first.
//!
//! The image is `agent-core/sandbox:dev`, or `AGENT_CORE_SANDBOX_IMAGE`.
//! Sessions run in it through agentd's hooks and the sandbox crate's
//! container configuration, on an internal network of the test's own. The
//! test process serves the credential proxy on that network's gateway
//! address, which the host holds on an internal network's bridge, and
//! forwards to `fake_anthropic()`, so no account is needed. The containers,
//! the network and the data directory are removed when the test ends, also
//! when it fails.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentd::consents::ConsentSettings;
use agentd::ctl::{Ctl, CtlSettings, NoSurfaces};
use agentd::pipeline::{Hooks, connect_docker};
use agentd::{App, Config};
use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, NetworkCreateRequest};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
};
use core_types::{
    AgentId, ConvRef, CredentialRef, Hop, MemberId, MemberKey, MessageId, Requester, ScopeKey,
    SessionId, Side, SurfaceKind, ThreadKey, TurnId, TurnKind, VolumeKey,
};
use cred_proxy::{CredProxy, FixedKey, Observation, ProxyObserver, Registry};
use futures::StreamExt as _;
use runner::{
    PoolConfig, ProcessConfig, SessionConfig, SessionManager, SessionStart, TurnOutcome,
    TurnRequest,
};
use sandbox::{DockerSandbox, Sandbox, SandboxConfig};
use secrecy::SecretString;
use store::{Sealer, Store};
use testkit::anthropic::DEFAULT_REPLY;
use testkit::{TempDir, fake_anthropic};
use tokio::net::TcpListener;

const DEFAULT_IMAGE: &str = "agent-core/sandbox:dev";
const PLANTED_IMAGE: &str = "debian:stable-slim";
const COMMUNITY_KEY: &str = "sk-ant-community-test-key";

/// What the test leaves on the Docker host and on disk, removed on drop.
struct Leftovers {
    network: String,
    instance: String,
    dir: PathBuf,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        let network = self.network.clone();
        let instance = self.instance.clone();
        let cleanup = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let docker = connect().await;
                    for id in containers(&docker, &instance).await {
                        let options = RemoveContainerOptionsBuilder::default().force(true).build();
                        let _ = docker.remove_container(&id, Some(options)).await;
                    }
                    let _ = docker.remove_network(&network).await;
                });
        });
        let _ = cleanup.join();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Containers a test planted, removed on drop.
#[derive(Default)]
struct Planted(Vec<String>);

impl Drop for Planted {
    fn drop(&mut self) {
        let ids = std::mem::take(&mut self.0);
        let cleanup = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let docker = connect().await;
                    for id in ids {
                        let options = RemoveContainerOptionsBuilder::default().force(true).build();
                        let _ = docker.remove_container(&id, Some(options)).await;
                    }
                });
        });
        let _ = cleanup.join();
    }
}

impl Planted {
    /// Starts a container from [`PLANTED_IMAGE`] with `labels`.
    async fn plant(&mut self, docker: &Docker, labels: HashMap<String, String>) -> String {
        let created = docker
            .create_container(
                None,
                ContainerCreateBody {
                    image: Some(PLANTED_IMAGE.into()),
                    cmd: Some(vec!["sleep".into(), "600".into()]),
                    labels: Some(labels),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        self.0.push(created.id.clone());
        docker.start_container(&created.id, None).await.unwrap();
        created.id
    }
}

async fn connect() -> Docker {
    Docker::connect_with_defaults()
        .unwrap()
        .negotiate_version()
        .await
        .unwrap()
}

/// The ids of the containers labeled with `instance`.
async fn containers(docker: &Docker, instance: &str) -> Vec<String> {
    let filters = std::collections::HashMap::from([(
        "label".to_owned(),
        vec![format!("{}={instance}", sandbox::LABEL_INSTANCE)],
    )]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    docker
        .list_containers(Some(options))
        .await
        .unwrap()
        .into_iter()
        .filter_map(|container| container.id)
        .collect()
}

struct NoTokens;

#[async_trait]
impl TokenSource for NoTokens {
    async fn access_token(&self, _member: MemberId) -> Result<SecretString, AuthError> {
        Err(AuthError::NotLinked)
    }
}

/// Counts the requests the proxy forwarded.
#[derive(Default)]
struct Forwarded(AtomicUsize);

impl ProxyObserver for Forwarded {
    fn observe(&self, _observation: &Observation) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        turn: TurnId::new_v4(),
        message: "Say hello.".into(),
        credential: CredentialRef::Community,
        model: None,
        requester: Requester {
            member: None,
            key: MemberKey {
                surface: SurfaceKind::RocketChat,
                team: "chat.example".into(),
                user: "alice".into(),
            },
            outside: None,
        },
        hop: Hop::ZERO,
        side: Side::Public,
        kind: TurnKind::Normal,
        trigger: Some(MessageId::new("m1")),
    }
}

fn expect_reply(outcome: &TurnOutcome) {
    let TurnOutcome::Finished(result) = outcome else {
        panic!("the turn didn't finish: {outcome:?}");
    };
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.result.as_deref(), Some(DEFAULT_REPLY), "{result:?}");
}

/// The CLI's running total for its process, from a finished turn.
fn process_total(outcome: &TurnOutcome) -> f64 {
    match outcome {
        TurnOutcome::Finished(result) => result.process_total_cost_usd.unwrap(),
        other => panic!("{other:?}"),
    }
}

/// A finished turn's own cost, as the runner bills it.
fn turn_cost(outcome: &TurnOutcome) -> f64 {
    match outcome {
        TurnOutcome::Finished(result) => result.cost_usd.unwrap(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_startup_reaps_only_this_instances_sandboxes() {
    let docker = connect().await;
    if docker.inspect_image(PLANTED_IMAGE).await.is_err() {
        let (from_image, tag) = PLANTED_IMAGE.split_once(':').unwrap();
        let options = CreateImageOptionsBuilder::default()
            .from_image(from_image)
            .tag(tag)
            .build();
        let mut progress = docker.create_image(Some(options), None, None);
        while let Some(item) = progress.next().await {
            item.unwrap();
        }
    }
    let instance = format!("test-{}", uuid::Uuid::new_v4().simple());
    let mut planted = Planted::default();
    let left = planted
        .plant(
            &docker,
            HashMap::from([
                (sandbox::LABEL_INSTANCE.to_owned(), instance.clone()),
                (
                    sandbox::LABEL_SESSION.to_owned(),
                    SessionId::new_v4().to_string(),
                ),
            ]),
        )
        .await;
    let stranger = planted.plant(&docker, HashMap::new()).await;
    assert_eq!(containers(&docker, &instance).await, vec![left.clone()]);

    let dir = TempDir::new("agentd-test");
    let text = format!(
        "{}\n[sandbox]\nimage = \"{PLANTED_IMAGE}\"\ninstance = \"{instance}\"\nstop_timeout_secs = 1\n",
        common::CONFIG
            .replace(
                "proxy_listen = \"127.0.0.2:0\"",
                "proxy_listen = \"127.0.0.2:8080\""
            )
            .replace(
                "ctl_listen = \"127.0.0.2:0\"",
                "ctl_listen = \"127.0.0.2:8081\""
            )
            .replace("/nonexistent/agentd", &dir.path().display().to_string()),
    );
    let config = Config::parse(&text, common::env()).unwrap();
    let store = Store::open_in_memory(config.sealer().unwrap())
        .await
        .unwrap();
    let app = App::new(config, store, None).unwrap();
    assert!(connect_docker(&app).await.unwrap().is_some());

    assert!(containers(&docker, &instance).await.is_empty());
    assert!(docker.inspect_container(&left, None).await.is_err());
    let kept = docker.inspect_container(&stranger, None).await.unwrap();
    assert_eq!(kept.state.and_then(|state| state.running), Some(true));
    drop(planted);
}

#[tokio::test]
#[ignore = "needs docker and the sandbox image"]
async fn docker_real_claude_starts() {
    let image = std::env::var("AGENT_CORE_SANDBOX_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.into());
    let docker = connect().await;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let leftovers = Leftovers {
        network: format!("agentd-claude-test-{tag}"),
        instance: format!("test-{tag}"),
        dir: std::env::temp_dir().join(format!("agentd-claude-{tag}")),
    };
    docker
        .create_network(NetworkCreateRequest {
            name: leftovers.network.clone(),
            internal: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    let gateway: IpAddr = docker
        .inspect_network(&leftovers.network, None)
        .await
        .unwrap()
        .ipam
        .and_then(|ipam| ipam.config)
        .and_then(|config| config.into_iter().next())
        .and_then(|config| config.gateway)
        .unwrap()
        .parse()
        .unwrap();
    std::fs::create_dir(&leftovers.dir).unwrap();
    let data_dir = leftovers.dir.clone();

    let fake = fake_anthropic().await;
    let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
    let store = Store::open_in_memory(sealer).await.unwrap();
    let registry = Registry::new();
    let forwarded = Arc::new(Forwarded::default());
    let proxy = CredProxy::new(
        &fake.uri(),
        registry.clone(),
        Arc::new(NoTokens),
        Arc::new(FixedKey::new(SecretString::from(COMMUNITY_KEY))),
    )
    .unwrap()
    .with_observer(forwarded.clone())
    .into_router();
    let listener = TcpListener::bind((gateway, 0)).await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            proxy.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let mut config = SandboxConfig::new(&image);
    config.network = leftovers.network.clone();
    config.instance = leftovers.instance.clone();
    config.memory_mb = 1024;
    config.stop_timeout_secs = 1;
    let me = std::fs::metadata(&data_dir).unwrap();
    if me.uid() != 0 {
        config.uid = me.uid();
        config.gid = me.gid();
    }
    let sandbox = DockerSandbox::new(docker.clone(), store.clone(), &data_dir, config).unwrap();
    let sandbox: Arc<dyn Sandbox> = Arc::new(sandbox);
    let ctl = Ctl::new(
        store.clone(),
        CtlSettings {
            staging_dir: data_dir.join("ctl-outbox"),
            attach_max_bytes: 1024,
            lease_ttl: Duration::from_secs(30),
            consents: ConsentSettings::in_data_dir(&data_dir),
        },
        Arc::new(NoSurfaces),
    );
    let no_proxy = gateway.to_string();
    let hooks = Hooks::new(
        registry,
        ctl,
        "http://agentctl.internal:8081",
        BTreeMap::from([
            ("NO_PROXY".to_owned(), no_proxy.clone()),
            ("no_proxy".to_owned(), no_proxy),
        ]),
    );
    let sessions = SessionManager::new(
        store.clone(),
        Arc::clone(&sandbox),
        hooks,
        SessionConfig {
            process: ProcessConfig {
                anthropic_base_url: format!("http://{proxy_addr}"),
                turn_timeout_secs: 180,
                ..ProcessConfig::default()
            },
            pool: PoolConfig::default(),
            image,
            data_dir: data_dir.clone(),
        },
    )
    .unwrap();

    let agent = AgentId::new_v4();
    runner::write_persona(&data_dir, agent, "You are a test agent. Answer briefly.\n")
        .await
        .unwrap();
    let thread = ThreadKey {
        conv: ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example".into(),
            conversation: "GENERAL".into(),
        },
        root: Some(MessageId::new("m1")),
    };
    let scope = ScopeKey::Channel(thread.conv.clone());
    let session = sessions
        .lookup_or_create(agent, &thread, &scope)
        .await
        .unwrap();

    let first = sessions.run_turn(session.id, request()).await.unwrap();
    assert_eq!(first.process_start, Some(SessionStart::New));
    expect_reply(&first.outcome);
    assert!(first.finished.unwrap().is_some());

    let running = containers(&docker, &leftovers.instance).await;
    assert_eq!(running.len(), 1);
    let inspected = docker.inspect_container(&running[0], None).await.unwrap();
    let host = inspected.host_config.unwrap();
    assert_eq!(host.readonly_rootfs, Some(true));
    assert_eq!(
        host.network_mode.as_deref(),
        Some(leftovers.network.as_str())
    );

    sessions.stop(session.id).await;
    assert!(containers(&docker, &leftovers.instance).await.is_empty());

    let second = sessions.run_turn(session.id, request()).await.unwrap();
    assert_eq!(second.process_start, Some(SessionStart::Resume));
    expect_reply(&second.outcome);
    let (first_total, second_total) = (
        process_total(&first.outcome),
        process_total(&second.outcome),
    );
    assert!(
        (second_total - 2.0 * first_total).abs() < first_total / 100.0,
        "a resumed process restores the session's total cost: {first_total} then {second_total}"
    );
    let second_cost = turn_cost(&second.outcome);
    assert!(
        (second_cost - first_total).abs() < first_total / 100.0,
        "the runner takes the restored total off the resumed turn's cost: {second_cost}"
    );
    sessions.stop(session.id).await;

    let volume = sandbox
        .ensure_volume(&VolumeKey { agent, scope })
        .await
        .unwrap();
    let id = session.id.to_string();
    let transcript = volume
        .session_dir(session.id)
        .join("claude/projects")
        .join(&id)
        .join(format!("{id}.jsonl"));
    let text = std::fs::read_to_string(&transcript)
        .unwrap_or_else(|err| panic!("{}: {err}", transcript.display()));
    assert!(
        text.matches(DEFAULT_REPLY).count() >= 2,
        "the transcript holds both replies"
    );

    let seen = fake.requests().await;
    assert!(fake.message_requests().await.len() >= 2);
    assert_eq!(
        seen.len(),
        forwarded.0.load(Ordering::SeqCst),
        "every request the fake saw came through the proxy"
    );
    for request in &seen {
        assert_eq!(
            request
                .headers
                .get("x-api-key")
                .map(|v| v.to_str().unwrap()),
            Some(COMMUNITY_KEY),
            "{} {}",
            request.method,
            request.url
        );
    }
    drop(sessions);
    drop(leftovers);
}
