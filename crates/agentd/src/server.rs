//! The HTTP listeners, the background tasks, and graceful shutdown.
//!
//! agentd has three listeners, each bound to its own address from the
//! configuration (never `0.0.0.0`):
//!
//! | Listener | Key | Serves |
//! | --- | --- | --- |
//! | public | `server.listen` | `/healthz`, and later Slack and OAuth routes |
//! | proxy | `internal.proxy_listen` | the credential proxy (placeholder) |
//! | ctl | `internal.ctl_listen` | the agentctl API (placeholder) |
//!
//! The public listener also refuses connections from
//! `internal.sandbox_subnet`. Every request carries the peer address as
//! [`ConnectInfo<SocketAddr>`](axum::extract::ConnectInfo); the internal
//! listeners identify sandboxes by it.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::serve::Listener;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tower::Service as _;

use crate::app::App;
use crate::net::RefuseSubnet;
use crate::sweeper::{self, SWEEP_INTERVAL};

/// How long `/healthz` waits for the store before reporting it unavailable.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// The routes each listener serves.
#[derive(Debug)]
pub struct Routers {
    /// The public listener's routes.
    pub public: Router,
    /// The proxy listener's routes.
    pub proxy: Router,
    /// The ctl listener's routes.
    pub ctl: Router,
}

impl Routers {
    /// The routes agentd serves: `/healthz` on the public listener. The
    /// internal listeners answer everything with 404 until the credential
    /// proxy and the agentctl API are added.
    pub fn new(app: &App) -> Self {
        Self {
            public: public_router(app.clone()),
            proxy: Router::new(),
            ctl: Router::new(),
        }
    }
}

/// The public listener's routes.
pub fn public_router(app: App) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(app)
}

/// `GET /healthz`: 200 when the store answers within [`HEALTH_TIMEOUT`], 503
/// otherwise. The reason is logged, not returned.
async fn healthz(State(app): State<App>) -> (StatusCode, &'static str) {
    match tokio::time::timeout(HEALTH_TIMEOUT, app.store().ping()).await {
        Ok(Ok(())) => (StatusCode::OK, "ok\n"),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "health check failed: the store returned an error");
            (StatusCode::SERVICE_UNAVAILABLE, "store unavailable\n")
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = HEALTH_TIMEOUT.as_millis(),
                "health check failed: the store did not answer"
            );
            (StatusCode::SERVICE_UNAVAILABLE, "store unavailable\n")
        }
    }
}

/// The addresses the listeners are bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addrs {
    /// The public listener.
    pub public: SocketAddr,
    /// The proxy listener.
    pub proxy: SocketAddr,
    /// The ctl listener.
    pub ctl: SocketAddr,
}

/// The bound listeners, ready to [`run`](Self::run).
#[derive(Debug)]
pub struct Server {
    app: App,
    routers: Routers,
    public: RefuseSubnet,
    proxy: TcpListener,
    ctl: TcpListener,
    addrs: Addrs,
}

impl Server {
    /// Binds the three listeners to their configured addresses. A port of 0
    /// picks a free port; [`addrs`](Self::addrs) says which.
    ///
    /// # Errors
    ///
    /// If an address can't be bound. The error names its key.
    pub async fn bind(app: App, routers: Routers) -> anyhow::Result<Self> {
        let config = app.config();
        let public = bind("server.listen", config.server.listen).await?;
        let proxy = bind("internal.proxy_listen", config.internal.proxy_listen).await?;
        let ctl = bind("internal.ctl_listen", config.internal.ctl_listen).await?;
        let addrs = Addrs {
            public: public.local_addr()?,
            proxy: proxy.local_addr()?,
            ctl: ctl.local_addr()?,
        };
        let public = RefuseSubnet::new(public, config.internal.sandbox_subnet);
        Ok(Self {
            app,
            routers,
            public,
            proxy,
            ctl,
            addrs,
        })
    }

    /// The bound addresses.
    pub fn addrs(&self) -> Addrs {
        self.addrs
    }

    /// Serves until `shutdown` completes, then shuts down gracefully:
    ///
    /// 1. Every listener stops accepting, and idle connections are closed.
    /// 2. In-flight requests and the sweeper get `server.drain_timeout_secs`
    ///    to finish. Whatever is still running then is dropped. If `abort`
    ///    completes first, as a second shutdown signal does, it is dropped
    ///    at once instead.
    /// 3. The store is closed.
    ///
    /// The sweeper runs alongside, every [`SWEEP_INTERVAL`].
    ///
    /// # Errors
    ///
    /// If a listener or the sweeper stops before `shutdown` does. The others
    /// are still shut down gracefully first.
    pub async fn run<F, G>(self, shutdown: F, abort: G) -> anyhow::Result<()>
    where
        F: Future<Output = ()> + Send,
        G: Future<Output = ()> + Send,
    {
        let Self {
            app,
            routers,
            public,
            proxy,
            ctl,
            addrs,
        } = self;
        let drain_timeout = app.config().server.drain_timeout();
        let (stop, stopping) = watch::channel(false);

        let mut tasks = JoinSet::new();
        tasks.spawn(serve_listener(
            "public listener",
            public,
            routers.public,
            stopping.clone(),
        ));
        tasks.spawn(serve_listener(
            "proxy listener",
            proxy,
            routers.proxy,
            stopping.clone(),
        ));
        tasks.spawn(serve_listener(
            "ctl listener",
            ctl,
            routers.ctl,
            stopping.clone(),
        ));
        let store = app.store().clone();
        tasks.spawn(async move {
            sweeper::run(store, SWEEP_INTERVAL, stopping).await;
            "sweeper"
        });
        tracing::info!(
            public = %addrs.public,
            proxy = %addrs.proxy,
            ctl = %addrs.ctl,
            "listening"
        );

        let mut failure = tokio::select! {
            () = shutdown => None,
            Some(joined) = tasks.join_next() => Some(stopped_early(joined)),
        };
        tracing::info!(
            drain_timeout_secs = drain_timeout.as_secs(),
            "shutting down: no longer accepting connections"
        );
        stop.send_replace(true);

        let drain = async {
            while let Some(joined) = tasks.join_next().await {
                if let Err(err) = joined {
                    failure.get_or_insert(panicked(err));
                }
            }
        };
        let cut_short = tokio::select! {
            drained = tokio::time::timeout(drain_timeout, drain) => {
                drained.is_err().then_some("drain timeout elapsed; dropping in-flight work")
            }
            () = abort => Some("shutdown forced; dropping in-flight work"),
        };
        if let Some(reason) = cut_short {
            tracing::warn!(unfinished_tasks = tasks.len(), "{reason}");
            tasks.shutdown().await;
        }
        app.store().close().await;
        tracing::info!("stopped");
        failure.map_or(Ok(()), Err)
    }
}

async fn bind(key: &str, addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {key} ({addr})"))
}

/// Completes once `stopping` becomes true, or its sender is dropped.
async fn stopped(mut stopping: watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Serves `router` on every connection `listener` accepts, until `stopping`
/// becomes true. Then it stops accepting, closes the listener, asks every
/// connection to finish its in-flight requests and close, and returns once
/// they have. Each request carries the peer's
/// [`ConnectInfo<SocketAddr>`](ConnectInfo).
///
/// The connections are tasks owned by this future: dropping or aborting it,
/// as the drain timeout does, closes them all.
async fn serve_listener<L>(
    name: &'static str,
    mut listener: L,
    router: Router,
    stopping: watch::Receiver<bool>,
) -> &'static str
where
    L: Listener<Addr = SocketAddr>,
{
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new());
    let graceful = GracefulShutdown::new();
    let mut connections = JoinSet::new();
    let stop = stopped(stopping);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            (io, peer) = listener.accept() => {
                let router = router.clone();
                let service = service_fn(move |mut request: Request<Incoming>| {
                    request.extensions_mut().insert(ConnectInfo(peer));
                    router.clone().call(request.map(Body::new))
                });
                let connection = builder
                    .serve_connection_with_upgrades(TokioIo::new(io), service)
                    .into_owned();
                connections.spawn(graceful.watch(connection));
            }
            Some(ended) = connections.join_next() => match ended {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::debug!(listener = name, error = %err, "a connection failed"),
                Err(err) => tracing::warn!(listener = name, error = %err, "a connection task panicked"),
            }
        }
    }
    drop(listener);
    graceful.shutdown().await;
    name
}

/// A task that ended while agentd was still serving: always an error.
fn stopped_early(joined: Result<&'static str, JoinError>) -> anyhow::Error {
    match joined {
        Ok(name) => anyhow!("the {name} stopped unexpectedly"),
        Err(err) => panicked(err),
    }
}

fn panicked(err: JoinError) -> anyhow::Error {
    anyhow::Error::new(err).context("a server task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_that_stops_early_is_an_error() {
        let err = stopped_early(Ok("sweeper"));
        assert_eq!(err.to_string(), "the sweeper stopped unexpectedly");
    }

    #[tokio::test]
    async fn a_panicking_task_is_reported() {
        let mut tasks: JoinSet<&'static str> = JoinSet::new();
        tasks.spawn(async { panic!("boom") });
        let joined = tasks.join_next().await.unwrap();
        let err = stopped_early(joined);
        assert!(err.to_string().contains("panicked"), "{err:#}");
    }
}
