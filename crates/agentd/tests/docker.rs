//! A Docker test with the real `claude`: it needs a Docker daemon and the
//! sandbox image built from `images/sandbox/Dockerfile`, so it is ignored
//! by default. CI builds the image, then runs it with
//! `cargo test --workspace -- --ignored docker_`.
//!
//! The image is `agent-core/sandbox:dev`, or `AGENT_CORE_SANDBOX_IMAGE`.
//! Sessions run in it through agentd's hooks and the sandbox crate's
//! container configuration, on an internal network of the test's own. The
//! test process serves the credential proxy on that network's gateway
//! address, which the host holds on an internal network's bridge, and
//! forwards to `fake_anthropic()`, so no account is needed. The containers,
//! the network and the data directory are removed when the test ends, also
//! when it fails.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentd::ctl::{Ctl, CtlSettings, NoSurfaces};
use agentd::pipeline::Hooks;
use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use bollard::Docker;
use bollard::models::NetworkCreateRequest;
use bollard::query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder};
use core_types::{
    AgentId, ConvRef, CredentialRef, Hop, MemberId, MemberKey, MessageId, Requester, ScopeKey,
    Side, SurfaceKind, ThreadKey, TurnId, TurnKind, VolumeKey,
};
use cred_proxy::{CredProxy, FixedKey, Observation, ProxyObserver, Registry};
use runner::{
    PoolConfig, ProcessConfig, SessionConfig, SessionManager, SessionStart, TurnOutcome,
    TurnRequest,
};
use sandbox::{DockerSandbox, Sandbox, SandboxConfig};
use secrecy::SecretString;
use store::{Sealer, Store};
use testkit::anthropic::DEFAULT_REPLY;
use testkit::fake_anthropic;
use tokio::net::TcpListener;

const DEFAULT_IMAGE: &str = "agent-core/sandbox:dev";
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
