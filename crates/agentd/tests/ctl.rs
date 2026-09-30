//! The agentctl API on agentd's ctl listener, and the startup purge.

mod common;

use std::net::SocketAddr;

use agentd::ctl::{ProcessInfo, ProcessToken, STAGING_DIR};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{AgentId, ScopeKey, SessionId, VolumeKey};
use secrecy::ExposeSecret as _;
use tokio::sync::oneshot;

use common::{CONFIG, Response, TempDir, env};

async fn post(addr: SocketAddr, token: Option<&ProcessToken>) -> Response {
    let token = token.map(|token| token.secret().expose_secret().to_owned());
    tokio::task::spawn_blocking(move || {
        common::post(addr, "/v1/history", token.as_deref(), "{}").unwrap()
    })
    .await
    .unwrap()
}

async fn issue(app: &App, ip: &str) -> ProcessToken {
    let agent = AgentId::new_v4();
    app.ctl()
        .issue_process_token(ProcessInfo {
            session: SessionId::new_v4(),
            agent,
            volume: VolumeKey {
                agent,
                scope: ScopeKey::Private,
            },
            container_ip: ip.parse().unwrap(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn the_ctl_listener_serves_the_agentctl_api_by_source_address() {
    let app = App::open(Config::parse(CONFIG, env()).unwrap())
        .await
        .unwrap();
    let server = Server::bind(app.clone(), Routers::new(&app)).await.unwrap();
    let ctl = server.addrs().ctl;
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));

    let response = post(ctl, None).await;
    assert_eq!(response.status, 401, "{response:?}");
    assert!(response.body.contains("\"unauthorized\""), "{response:?}");

    let elsewhere = issue(&app, "127.0.0.2").await;
    assert_eq!(post(ctl, Some(&elsewhere)).await.status, 401);

    let local = issue(&app, "127.0.0.1").await;
    let response = post(ctl, Some(&local)).await;
    assert_eq!(response.status, 409, "{response:?}");
    assert!(response.body.contains("\"no_turn\""), "{response:?}");

    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn startup_deletes_tokens_and_staged_files_from_before() {
    let dir = TempDir::new();
    let text = CONFIG
        .replace(
            "sqlite::memory:",
            &format!("sqlite://{}", dir.path().join("agentd.db").display()),
        )
        .replace("/nonexistent/agentd", &dir.path().display().to_string());
    let config = || Config::parse(&text, env()).unwrap();

    let app = App::open(config()).await.unwrap();
    let token = issue(&app, "127.0.0.1").await;
    let staged = dir.path().join(STAGING_DIR).join("left-over");
    std::fs::create_dir_all(&staged).unwrap();
    app.store().close().await;

    let app = App::open(config()).await.unwrap();
    assert!(!dir.path().join(STAGING_DIR).exists());
    let server = Server::bind(app.clone(), Routers::new(&app)).await.unwrap();
    let ctl = server.addrs().ctl;
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));
    assert_eq!(post(ctl, Some(&token)).await.status, 401);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
