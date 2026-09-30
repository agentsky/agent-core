//! A Docker test: it needs a Docker daemon and a route to github.com, so
//! it is ignored by default. CI runs it with
//! `cargo test --workspace -- --ignored docker_`.
//!
//! A container with git sits on an internal network of its own, like a
//! sandbox. The test process serves the egress proxy on the network's
//! gateway address, which the host holds on an internal network's bridge,
//! and the container reaches it as `cred-proxy.internal:8080` with the
//! sandbox's proxy environment. The container is removed, and the network
//! with it, when the test ends, also when it fails.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest};
use bollard::query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder};
use core_types::{CredentialKind, MemberId, SessionId};
use cred_proxy::{CredProxy, EGRESS_ENV, EgressPolicy, EgressProxy, FixedKey, Registry};
use futures::StreamExt as _;
use secrecy::SecretString;
use tokio::net::TcpListener;

const IMAGE: &str = "alpine/git";
const TAG: &str = "2.54.0";
const ALLOWED_REPO: &str = "https://github.com/octocat/Hello-World.git";
const OTHER_REPO: &str = "https://gitlab.com/gitlab-org/gitlab-test.git";

struct NoTokens;

#[async_trait]
impl TokenSource for NoTokens {
    async fn access_token(&self, _member: MemberId) -> Result<SecretString, AuthError> {
        Err(AuthError::NotLinked)
    }
}

/// What the test leaves on the Docker host, removed on drop.
struct Leftovers {
    network: String,
    container: Option<String>,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        let network = self.network.clone();
        let container = self.container.clone();
        let cleanup = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let docker = connect().await;
                    if let Some(container) = container {
                        let options = RemoveContainerOptionsBuilder::default().force(true).build();
                        let _ = docker.remove_container(&container, Some(options)).await;
                    }
                    let _ = docker.remove_network(&network).await;
                });
        });
        let _ = cleanup.join();
    }
}

async fn connect() -> Docker {
    Docker::connect_with_defaults()
        .unwrap()
        .negotiate_version()
        .await
        .unwrap()
}

/// Runs `argv` in `container` with `env`, and returns its exit code and
/// output.
async fn run(docker: &Docker, container: &str, argv: &[&str], env: &[String]) -> (i64, String) {
    let exec = docker
        .create_exec(
            container,
            CreateExecOptions {
                cmd: Some(argv.iter().map(|arg| (*arg).to_owned()).collect()),
                env: Some(env.to_vec()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut text = String::new();
    if let StartExecResults::Attached { mut output, .. } =
        docker.start_exec(&exec.id, None).await.unwrap()
    {
        while let Some(chunk) = output.next().await {
            text.push_str(&chunk.unwrap().to_string());
        }
    }
    let code = docker.inspect_exec(&exec.id).await.unwrap().exit_code;
    (code.unwrap_or(-1), text)
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_a_sandbox_clones_from_an_allowed_host_only() {
    let docker = connect().await;
    let options = CreateImageOptionsBuilder::default()
        .from_image(IMAGE)
        .tag(TAG)
        .build();
    let mut pull = docker.create_image(Some(options), None, None);
    while let Some(progress) = pull.next().await {
        progress.expect("pulling the git image");
    }

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let mut leftovers = Leftovers {
        network: format!("agentd-egress-test-{tag}"),
        container: None,
    };
    docker
        .create_network(NetworkCreateRequest {
            name: leftovers.network.clone(),
            internal: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    let ipam = docker
        .inspect_network(&leftovers.network, None)
        .await
        .unwrap()
        .ipam
        .and_then(|ipam| ipam.config)
        .and_then(|config| config.into_iter().next())
        .unwrap();
    let gateway: IpAddr = ipam.gateway.unwrap().parse().unwrap();
    let subnet = ipam.subnet.unwrap().parse().unwrap();

    let registry = Registry::new();
    let policy = EgressPolicy::new(
        vec!["github.com".parse().unwrap()],
        Vec::new(),
        vec![subnet],
    )
    .unwrap();
    let router = CredProxy::new(
        "http://127.0.0.1:9",
        registry.clone(),
        Arc::new(NoTokens),
        Arc::new(FixedKey::new(SecretString::from("unused"))),
    )
    .unwrap()
    .with_egress(EgressProxy::new(policy))
    .into_router();
    let listener = TcpListener::bind((gateway, 8080)).await.unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let created = docker
        .create_container(
            None,
            ContainerCreateBody {
                image: Some(format!("{IMAGE}:{TAG}")),
                entrypoint: Some(vec!["sleep".to_owned()]),
                cmd: Some(vec!["600".to_owned()]),
                host_config: Some(HostConfig {
                    network_mode: Some(leftovers.network.clone()),
                    extra_hosts: Some(vec![format!("cred-proxy.internal:{gateway}")]),
                    init: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    leftovers.container = Some(created.id.clone());
    docker.start_container(&created.id, None).await.unwrap();
    let ip: IpAddr = docker
        .inspect_container(&created.id, None)
        .await
        .unwrap()
        .network_settings
        .and_then(|settings| settings.networks)
        .and_then(|mut networks| networks.remove(&leftovers.network))
        .and_then(|endpoint| endpoint.ip_address)
        .unwrap()
        .parse()
        .unwrap();
    registry
        .mint(SessionId::new_v4(), ip, CredentialKind::Subscription)
        .unwrap();
    let env: Vec<String> = EGRESS_ENV
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();

    let (code, output) = run(
        &docker,
        &created.id,
        &["git", "clone", "--depth=1", ALLOWED_REPO, "/tmp/allowed"],
        &env,
    )
    .await;
    assert_eq!(code, 0, "{output}");
    let (code, output) = run(
        &docker,
        &created.id,
        &["test", "-f", "/tmp/allowed/README"],
        &[],
    )
    .await;
    assert_eq!(code, 0, "{output}");

    let (code, output) = run(
        &docker,
        &created.id,
        &["git", "clone", "--depth=1", OTHER_REPO, "/tmp/other"],
        &env,
    )
    .await;
    assert_ne!(code, 0, "{output}");
    assert!(output.contains("403"), "{output}");

    let (code, output) = run(
        &docker,
        &created.id,
        &["git", "clone", "--depth=1", ALLOWED_REPO, "/tmp/direct"],
        &[],
    )
    .await;
    assert_ne!(code, 0, "cloned without the proxy: {output}");
}
