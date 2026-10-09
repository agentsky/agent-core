//! `serve` in-process: the listeners, `/healthz`, and graceful shutdown.

mod common;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd::server::{Addrs, Routers, Server, Worker};
use agentd::{App, Config};
use axum::routing;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use common::{CONFIG, Response, env};

fn config(text: &str) -> Config {
    Config::parse(text, env()).unwrap()
}

struct Running {
    app: App,
    addrs: Addrs,
    stop: oneshot::Sender<()>,
    abort: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn start(text: &str, routes: impl FnOnce(Routers) -> Routers) -> Self {
        let app = App::open(config(text)).await.unwrap();
        let server = Server::bind(app.clone(), routes(Routers::new(&app).unwrap()))
            .await
            .unwrap();
        let addrs = server.addrs();
        let (stop, stopped) = oneshot::channel();
        let (abort, aborted) = oneshot::channel();
        let task = tokio::spawn(server.run(
            async {
                let _ = stopped.await;
            },
            async {
                if aborted.await.is_err() {
                    std::future::pending::<()>().await;
                }
            },
        ));
        Self {
            app,
            addrs,
            stop,
            abort,
            task,
        }
    }

    async fn stop(self) -> (App, anyhow::Result<()>) {
        self.stop.send(()).unwrap();
        let result = within(Duration::from_secs(10), self.task).await.unwrap();
        (self.app, result)
    }

    /// Asks for a graceful shutdown, then, once it has stopped accepting,
    /// forces it, as a second signal does.
    async fn stop_then_force(self) -> (App, anyhow::Result<()>) {
        let public = self.addrs.public;
        self.stop.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(public).is_ok() {
            assert!(Instant::now() < deadline, "still accepting after shutdown");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.abort.send(()).unwrap();
        let result = within(Duration::from_secs(10), self.task).await.unwrap();
        (self.app, result)
    }
}

async fn within<F: Future>(limit: Duration, future: F) -> F::Output {
    tokio::time::timeout(limit, future)
        .await
        .expect("timed out")
}

async fn get(addr: SocketAddr, path: &'static str) -> Option<Response> {
    tokio::task::spawn_blocking(move || common::get(addr, path))
        .await
        .unwrap()
}

#[tokio::test]
async fn serve_answers_healthz_and_shuts_down_cleanly() {
    let running = Running::start(CONFIG, |routers| routers).await;
    let addrs = running.addrs;
    assert_eq!(addrs.public.ip(), std::net::Ipv4Addr::LOCALHOST);
    assert_ne!(addrs.public.port(), 0);

    let health = get(addrs.public, "/healthz").await.unwrap();
    assert_eq!(health.status, 200, "{health:?}");
    assert_eq!(health.body, "ok\n");
    assert_eq!(get(addrs.public, "/nope").await.unwrap().status, 404);
    let proxied = get(addrs.proxy, "/healthz").await.unwrap();
    assert_eq!(proxied.status, 403, "{proxied:?}");
    assert!(proxied.body.contains(r#""type":"error""#), "{proxied:?}");
    assert_eq!(get(addrs.ctl, "/healthz").await.unwrap().status, 404);

    let (app, result) = running.stop().await;
    result.unwrap();
    app.store().ping().await.unwrap_err();
    for addr in [addrs.public, addrs.proxy, addrs.ctl] {
        assert!(
            std::net::TcpStream::connect(addr).is_err(),
            "{addr} still accepts"
        );
    }
}

#[tokio::test]
async fn cli_serve_runs_until_shutdown() {
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(agentd::cli::serve(
        config(CONFIG),
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!task.is_finished());
    stop.send(()).unwrap();
    within(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn healthz_is_503_when_the_store_is_down() {
    let running = Running::start(CONFIG, |routers| routers).await;
    running.app.store().close().await;
    let health = get(running.addrs.public, "/healthz").await.unwrap();
    assert_eq!(health.status, 503, "{health:?}");
    assert_eq!(health.body, "store unavailable\n");
    running.stop().await.1.unwrap();
}

#[tokio::test]
async fn shutdown_stops_accepting_and_waits_for_in_flight_requests() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (notify, released) = (started.clone(), release.clone());
    let running = Running::start(CONFIG, move |mut routers| {
        routers.public = routers.public.route(
            "/slow",
            routing::get(move || async move {
                notify.notify_one();
                released.notified().await;
                "done"
            }),
        );
        routers
    })
    .await;
    let public = running.addrs.public;
    let request = tokio::spawn(get(public, "/slow"));
    within(Duration::from_secs(5), started.notified()).await;

    let stop = tokio::spawn(running.stop());
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(public).is_ok() {
        assert!(Instant::now() < deadline, "still accepting after shutdown");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    release.notify_one();

    let response = within(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((response.status, response.body.as_str()), (200, "done"));
    stop.await.unwrap().1.unwrap();
}

#[tokio::test]
async fn shutdown_drops_requests_still_running_after_the_drain_timeout() {
    let text = CONFIG.replace("drain_timeout_secs = 5", "drain_timeout_secs = 1");
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let running = Running::start(&text, move |mut routers| {
        routers.public = routers.public.route(
            "/hang",
            routing::get(move || async move {
                notify.notify_one();
                std::future::pending::<()>().await;
            }),
        );
        routers
    })
    .await;
    let request = tokio::spawn(get(running.addrs.public, "/hang"));
    within(Duration::from_secs(5), started.notified()).await;

    let begun = Instant::now();
    let (app, result) = running.stop().await;
    result.unwrap();
    let took = begun.elapsed();
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(
        within(Duration::from_secs(5), request)
            .await
            .unwrap()
            .is_none()
    );
    app.store().ping().await.unwrap_err();
}

#[tokio::test]
async fn a_worker_that_stops_early_stops_agentd_with_an_error() {
    let running = Running::start(CONFIG, |mut routers| {
        routers.workers.push(Worker::new("test worker", async {}));
        routers
    })
    .await;
    let result = within(Duration::from_secs(10), running.task).await.unwrap();
    let err = result.unwrap_err();
    assert!(
        err.to_string()
            .contains("the test worker stopped unexpectedly"),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_worker_still_running_after_the_drain_timeout_is_dropped() {
    let text = CONFIG.replace("drain_timeout_secs = 5", "drain_timeout_secs = 1");
    let (done, finished) = oneshot::channel::<()>();
    let running = Running::start(&text, move |mut routers| {
        let listed = format!("{:?}", routers.workers);
        assert!(listed.contains("Slack queue"), "{listed}");
        routers.workers.push(Worker::new("test worker", async move {
            std::future::pending::<()>().await;
            let _ = done.send(());
        }));
        routers
    })
    .await;
    let (_, result) = running.stop().await;
    result.unwrap();
    assert!(finished.await.is_err(), "the pending worker finished");
}

#[tokio::test]
async fn a_forced_shutdown_drops_in_flight_work_without_waiting_for_the_drain() {
    let text = CONFIG.replace("drain_timeout_secs = 5", "drain_timeout_secs = 3600");
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let (done, finished) = oneshot::channel::<()>();
    let running = Running::start(&text, move |mut routers| {
        routers.public = routers.public.route(
            "/hang",
            routing::get(move || async move {
                notify.notify_one();
                std::future::pending::<()>().await;
            }),
        );
        routers.workers.push(Worker::new("test worker", async move {
            std::future::pending::<()>().await;
            let _ = done.send(());
        }));
        routers
    })
    .await;
    let request = tokio::spawn(get(running.addrs.public, "/hang"));
    within(Duration::from_secs(5), started.notified()).await;

    let begun = Instant::now();
    let (app, result) = running.stop_then_force().await;
    result.unwrap();
    let took = begun.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(
        within(Duration::from_secs(5), request)
            .await
            .unwrap()
            .is_none()
    );
    assert!(finished.await.is_err(), "the pending worker finished");
    app.store().ping().await.unwrap_err();
}

#[tokio::test]
async fn an_address_in_use_is_named() {
    let taken = std::net::TcpListener::bind("127.0.0.2:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let text = CONFIG.replacen(
        "proxy_listen = \"127.0.0.2:0\"",
        &format!("proxy_listen = \"127.0.0.2:{port}\""),
        1,
    );
    let app = App::open(config(&text)).await.unwrap();
    let err = Server::bind(app.clone(), Routers::new(&app).unwrap())
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains(&format!("internal.proxy_listen (127.0.0.2:{port})")),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_bad_store_url_is_named() {
    let text = CONFIG.replace("sqlite::memory:", "sqlite:///nonexistent-dir/agentd.db");
    let err = App::open(config(&text)).await.unwrap_err();
    assert!(format!("{err:#}").contains("store.url"), "{err:#}");
}

#[tokio::test]
async fn the_proxy_listener_hands_connect_to_the_egress_proxy() {
    let running = Running::start(CONFIG, |routers| routers).await;
    let proxy = running.addrs.proxy;
    let answer = tokio::task::spawn_blocking(move || {
        let mut stream =
            std::net::TcpStream::connect_timeout(&proxy, Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        std::io::Write::write_all(
            &mut stream,
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
        common::read_response(&mut stream)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer.status, 403, "{answer:?}");
    assert!(
        answer.body.contains("This address has no sandbox session."),
        "{answer:?}"
    );
    running.stop().await.1.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_public_listener_refuses_the_sandbox_subnet() {
    let running = Running::start(CONFIG, |routers| routers).await;
    let public = running.addrs.public;
    let proxy = running.addrs.proxy;
    let refused = tokio::task::spawn_blocking(move || {
        let socket = socket_from("127.0.0.2");
        let mut stream = socket.connect(public).ok()?;
        std::io::Write::write_all(&mut stream, b"GET /healthz HTTP/1.1\r\nHost: a\r\n\r\n").ok()?;
        common::read_response(&mut stream)
    });
    assert!(
        refused.await.unwrap().is_none(),
        "the sandbox subnet got an answer"
    );

    let allowed = tokio::task::spawn_blocking(move || {
        let mut stream = socket_from("127.0.0.3").connect(public).unwrap();
        std::io::Write::write_all(
            &mut stream,
            b"GET /healthz HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
        common::read_response(&mut stream)
    });
    assert_eq!(allowed.await.unwrap().unwrap().status, 200);
    assert_eq!(get(proxy, "/").await.unwrap().status, 403);
    running.stop().await.1.unwrap();
}

#[cfg(target_os = "linux")]
fn socket_from(ip: &str) -> SocketFrom {
    SocketFrom(ip.parse().unwrap())
}

#[cfg(target_os = "linux")]
struct SocketFrom(std::net::IpAddr);

#[cfg(target_os = "linux")]
impl SocketFrom {
    fn connect(&self, to: SocketAddr) -> std::io::Result<std::net::TcpStream> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let stream = runtime.block_on(async {
            let socket = tokio::net::TcpSocket::new_v4()?;
            socket.bind(SocketAddr::new(self.0, 0))?;
            socket.connect(to).await
        })?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(stream)
    }
}
