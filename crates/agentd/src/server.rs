//! The HTTP listeners, the background tasks, and graceful shutdown.
//!
//! agentd has three listeners, each bound to its own address from the
//! configuration (never `0.0.0.0`):
//!
//! | Listener | Key | Serves |
//! | --- | --- | --- |
//! | public | `server.listen` | `/healthz`, the Slack request URLs, and later OAuth callbacks |
//! | proxy | `internal.proxy_listen` | the credential proxy and the egress proxy ([`cred_proxy`]) |
//! | ctl | `internal.ctl_listen` | the agentctl API ([`ctl`](crate::ctl)) |
//!
//! The public listener also refuses connections from
//! `internal.sandbox_subnet`. Every request carries the peer address as
//! [`ConnectInfo<SocketAddr>`](axum::extract::ConnectInfo); the internal
//! listeners identify sandboxes by it.
//!
//! [`Worker`]s run next to the listeners, such as the queue behind the
//! Slack routes, and are drained with them on shutdown.

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use auth::TokenSource;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::serve::Listener;
use core_types::Sender;
use cred_proxy::CredProxy;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tower::Service as _;

use crate::agents::{Acknowledge, Supervisor};
use crate::app::App;
use crate::commands::intake::{CommandIntake, CommandSubmitter};
use crate::commands::relink::{RELINK_SWEEP_INTERVAL, RelinkNotifier};
use crate::commands::rocketchat::{self, CommandFeed, StoreDedup};
use crate::commands::slack_tokens::{ConfigTokenRotator, ROTATION_INTERVAL};
use crate::net::RefuseSubnet;
use crate::pipeline::{NoCommunityKey, Turns};
use crate::slack;
use crate::sweeper::{self, SWEEP_INTERVAL};

/// How long `/healthz` waits for the store before reporting it unavailable.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// The routes each listener serves, and the workers behind them.
#[derive(Debug)]
pub struct Routers {
    /// The public listener's routes.
    pub public: Router,
    /// The proxy listener's routes.
    pub proxy: Router,
    /// The ctl listener's routes.
    pub ctl: Router,
    /// Tasks that run as long as the listeners, such as the queue the Slack
    /// routes fill.
    pub workers: Vec<Worker>,
    /// The intake that runs every surface's commands. The Slack queue holds
    /// a submitter into it.
    pub intake: CommandIntake,
    /// A submitter into [`intake`](Self::intake), for the Rocket.Chat
    /// manager bot's connection. [`Server::run`] holds it until shutdown,
    /// so the intake runs as long as the listeners.
    pub commands: CommandSubmitter,
}

impl Routers {
    /// The routes agentd serves: `/healthz` and the Slack request URLs on
    /// the public listener, with the Slack queue as a worker handing
    /// commands to the command intake; the credential proxy on the proxy
    /// listener, forwarding to `proxy.upstream` with the placeholders in
    /// [`App::registry`] and answering `CONNECT` with the egress proxy
    /// `[proxy]` describes; and the agentctl API on the ctl listener.
    ///
    /// # Errors
    ///
    /// If the credential proxy can't be built.
    pub fn new(app: &App) -> anyhow::Result<Self> {
        let (slack_routes, slack_queue) = slack::routes(app);
        let (intake, commands) = CommandIntake::new(app.commands().clone());
        let inbound = slack::Inbound::new(
            app.store().clone(),
            app.slack().map(|slack| slack.identity().clone()),
            commands.clone(),
        );
        let tokens: Arc<dyn TokenSource> = app.auth().clone();
        let proxy = CredProxy::new(
            &app.config().proxy.upstream,
            app.registry().clone(),
            tokens,
            Arc::new(NoCommunityKey),
        )
        .context("proxy.upstream")?
        .with_egress(app.config().egress_proxy()?);
        Ok(Self {
            public: public_router(app.clone()).merge(slack_routes),
            proxy: proxy.into_router(),
            ctl: app.ctl().router(),
            workers: vec![Worker::new(
                "Slack queue",
                slack::run_queue(slack_queue, app.store().clone(), Sender::new(inbound)),
            )],
            intake,
            commands,
        })
    }
}

/// A task [`Server::run`] runs next to the listeners.
///
/// It must keep running while agentd serves: one that ends before shutdown
/// stops agentd with an error. On shutdown it gets the same drain timeout as
/// in-flight requests. A worker that consumes what a router produces should
/// end once that router is dropped, which happens after its listener has
/// stopped and its connections have finished.
pub struct Worker {
    name: &'static str,
    task: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Worker {
    /// A worker named `name` in logs and errors, running `task`.
    pub fn new(name: &'static str, task: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            name,
            task: Box::pin(task),
        }
    }
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("name", &self.name)
            .finish_non_exhaustive()
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
    turns: Option<Turns>,
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
            turns: None,
        })
    }

    /// The bound addresses.
    pub fn addrs(&self) -> Addrs {
        self.addrs
    }

    /// Runs turns with `turns` while serving. Without it agentd runs none.
    pub fn with_turns(mut self, turns: Turns) -> Self {
        self.turns = Some(turns);
        self
    }

    /// Serves until `shutdown` completes, then shuts down gracefully:
    ///
    /// 1. Every listener stops accepting, and idle connections are closed.
    /// 2. In-flight requests, the workers and the sweeper get
    ///    `server.drain_timeout_secs` to finish. Whatever is still running
    ///    then is dropped. If `abort` completes first, as a second shutdown
    ///    signal does, it is dropped at once instead.
    /// 3. The store is closed.
    ///
    /// The sweeper runs alongside, every [`SWEEP_INTERVAL`], and so do the
    /// routers' [`Worker`]s, the [`CommandIntake`], the relink notifier,
    /// with the Slack manager app the configuration token rotator, and with
    /// `[rocketchat]` the manager bot's connection and the [`Supervisor`] of
    /// the agents' connections. Every connection feeds the commands it hears
    /// to the intake like the Slack queue does, and passes other messages to
    /// [`Acknowledge`]. The intake finishes the commands it received once
    /// they all stop.
    ///
    /// # Errors
    ///
    /// If a listener, a worker or the sweeper stops before `shutdown` does.
    /// The others are still shut down gracefully first.
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
            turns,
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
        for worker in routers.workers {
            tasks.spawn(async move {
                worker.task.await;
                worker.name
            });
        }
        let store = app.store().clone();
        let sweeping = stopping.clone();
        tasks.spawn(async move {
            sweeper::run(store, SWEEP_INTERVAL, sweeping).await;
            "sweeper"
        });
        let notifier = RelinkNotifier::new(app.store().clone(), app.commands().replies().clone());
        let wake = app.auth().take_relink_notices();
        let notifying = stopping.clone();
        tasks.spawn(async move {
            notifier.run(wake, RELINK_SWEEP_INTERVAL, notifying).await;
            "relink notifier"
        });
        let intake = routers.intake;
        tasks.spawn(async move {
            intake.run().await;
            "command intake"
        });
        let commands = routers.commands;
        let holding = stopping.clone();
        if let Some(slack) = app.slack() {
            let rotator = ConfigTokenRotator::new(
                app.store().clone(),
                slack.client().clone(),
                app.commands().replies().clone(),
            );
            let rotating = stopping.clone();
            tasks.spawn(async move {
                rotator.run(ROTATION_INTERVAL, rotating).await;
                "Slack configuration token rotator"
            });
        }
        if let Some(manager) = app.rocketchat() {
            let feed = CommandFeed::new(commands.clone(), manager.binding.clone());
            let onward = Sender::new(Acknowledge::new(
                manager.agents.clone(),
                manager.binding.bot.clone(),
            ));
            let supervisor = Supervisor::new(
                manager.agents.clone(),
                manager.surface_config.clone(),
                Arc::new(StoreDedup(app.store().clone())),
                manager.bots.clone(),
                feed.clone(),
                Some(onward.clone()),
            );
            let supervising = stopping.clone();
            tasks.spawn(async move {
                supervisor.run(supervising).await;
                "Rocket.Chat agent supervisor"
            });
            let connection = rocketchat::listen(
                manager.surface.clone(),
                manager.binding.clone(),
                feed.into_sender(Some(onward)),
                stopping,
            );
            tasks.spawn(async move {
                if let Err(err) = connection.await {
                    tracing::error!(error = %err, "the Rocket.Chat manager bot's connection ended");
                }
                "Rocket.Chat manager bot's connection"
            });
        }
        tasks.spawn(async move {
            stopped(holding).await;
            drop(commands);
            "command submitter"
        });
        tracing::info!(
            public = %addrs.public,
            proxy = %addrs.proxy,
            ctl = %addrs.ctl,
            turns = turns.is_some(),
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
            tracing::warn!(unfinished = tasks.len(), "{reason}");
            tasks.shutdown().await;
        }
        drop(turns);
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
