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
use core_types::{Sender, Surface as _};
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
use crate::community::StoreCommunityKey;
use crate::net::RefuseSubnet;
use crate::pipeline::Pipeline;
use crate::skills::SkillHosts;
use crate::slack;
use crate::slack::agents::INSTALL_SWEEP_INTERVAL;
use crate::sweeper::{self, SWEEP_INTERVAL};

/// How long `/healthz` waits for the store before reporting it unavailable.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// The least time shutdown gives skill changes still running before it
/// closes the store, even once the drain timeout has passed.
pub const SKILL_DRAIN_FLOOR: Duration = Duration::from_secs(1);

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
    /// Where the Slack queue sends agents' messages, when agentd serves
    /// Slack. [`Server::with_pipeline`] connects it to the turn pipeline.
    pub slack_messages: Option<slack::Messages>,
}

impl Routers {
    /// The routes agentd serves: `/healthz`, the Slack request URLs and
    /// agent apps' OAuth callback on the public listener, with the Slack
    /// queue as a worker handing commands to the command intake and agents'
    /// messages to [`slack_messages`](Self::slack_messages); the credential proxy on the proxy
    /// listener, forwarding to `proxy.upstream` with the placeholders in
    /// [`App::registry`], members' tokens from [`App::auth`] and the
    /// community API key from the store ([`StoreCommunityKey`]), and
    /// answering `CONNECT` with the egress proxy `[proxy]` describes,
    /// extended for each session by its agent's skills' confirmed hosts
    /// ([`SkillHosts`]); and the agentctl API on the ctl listener.
    ///
    /// # Errors
    ///
    /// If the credential proxy can't be built.
    pub fn new(app: &App) -> anyhow::Result<Self> {
        let (slack_routes, slack_queue) = slack::routes(app);
        let (intake, commands) = CommandIntake::new(app.commands().clone());
        let mut inbound = slack::Inbound::new(
            app.store().clone(),
            app.slack().map(|slack| slack.identity().clone()),
            commands.clone(),
        );
        let mut slack_messages = None;
        let mut message_worker = None;
        if let Some(agents) = app.slack_agents() {
            let (messages, worker) = slack::Messages::new(agents.bots().clone());
            inbound = inbound.with_agents(messages.clone());
            slack_messages = Some(messages);
            message_worker = Some(Worker::new("Slack agents' messages", worker));
        }
        let tokens: Arc<dyn TokenSource> = app.auth().clone();
        let upstream = &app.config().proxy.upstream;
        if !cred_proxy::is_default_upstream(upstream) {
            tracing::warn!(
                %upstream,
                default = cred_proxy::DEFAULT_UPSTREAM,
                "the credential proxy forwards real credentials to proxy.upstream, not the default"
            );
        }
        let proxy = CredProxy::new(
            upstream,
            app.registry().clone(),
            tokens,
            Arc::new(StoreCommunityKey::new(app.store().clone())),
        )
        .context("proxy.upstream")?
        .with_egress(
            app.config()
                .egress_proxy()?
                .with_extension(Arc::new(SkillHosts(app.skills().clone()))),
        );
        Ok(Self {
            public: public_router(app.clone()).merge(slack_routes),
            proxy: proxy.into_router(),
            ctl: app.ctl().router(),
            workers: std::iter::once(Worker::new(
                "Slack queue",
                slack::run_queue(slack_queue, app.store().clone(), Sender::new(inbound)),
            ))
            .chain(message_worker)
            .collect(),
            intake,
            commands,
            slack_messages,
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
    pipeline: Option<Pipeline>,
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
            pipeline: None,
        })
    }

    /// The bound addresses.
    pub fn addrs(&self) -> Addrs {
        self.addrs
    }

    /// Passes the connections' messages, and those agents' Slack apps
    /// receive, to `pipeline`, which runs turns, while serving. Without it
    /// agentd runs none.
    pub fn with_pipeline(mut self, pipeline: Pipeline) -> Self {
        if let Some(messages) = &self.routers.slack_messages {
            messages.connect(pipeline.sink(surface_slack::surface::CAPS));
        }
        self.pipeline = Some(pipeline);
        self
    }

    /// Serves until `shutdown` completes, then shuts down gracefully:
    ///
    /// 1. The public listener stops accepting, idle connections are closed,
    ///    and the chat connections stop. The turn [`Pipeline`] takes no more
    ///    messages.
    /// 2. The turns already taken get `server.drain_timeout_secs` to finish,
    ///    while the proxy and ctl listeners still serve them. Those still
    ///    running then are dropped, their working emoji taken off and their
    ///    threads told to ask again ([`Pipeline::cut_short`]).
    /// 3. The proxy and ctl listeners stop accepting too, and in-flight
    ///    requests, the workers and the sweeper get what is left of the
    ///    same timeout to finish. Whatever is still running then is
    ///    dropped.
    /// 4. With turns, every warm session's process and container is
    ///    stopped, within what is left of the same timeout
    ///    ([`Pipeline::stop_sessions`]). A drain that was cut short, by the
    ///    timeout or by `abort`, or that left no time, skips it, and the
    ///    next start reaps what is left. A stop that runs out of time or is
    ///    cut short by `abort` leaves the sessions it already began stopping
    ///    to finish in the background: their agentctl token revocations
    ///    fail once the store is closed, which the runner logs as giving up,
    ///    and the idle reaper they keep alive runs until the process exits.
    ///    The next start purges the tokens and reaps the containers.
    /// 5. The pipeline is dropped. Skill changes still running, which an
    ///    aborted command leaves to finish in their own task, get what is
    ///    left of the same timeout, but at least
    ///    [`SKILL_DRAIN_FLOOR`], unless shutdown was forced
    ///    ([`Skills::drain`](crate::skills::Skills::drain)), and the store
    ///    is closed.
    ///
    /// If `abort` completes before the drain ends, as a second shutdown
    /// signal does, what is still running is dropped at once instead.
    ///
    /// The sweeper runs alongside, every [`SWEEP_INTERVAL`], and so do the
    /// routers' [`Worker`]s, the [`CommandIntake`], the relink notifier,
    /// with the Slack manager app the configuration token rotator and the
    /// sweeper of agents' apps (install reminders, stale creations), and with
    /// `[rocketchat]` the manager bot's connection and the [`Supervisor`] of
    /// the agents' connections. Every connection feeds the commands it hears
    /// to the intake like the Slack queue does, and passes other messages to
    /// the turn [`Pipeline`] when agentd runs turns
    /// ([`with_pipeline`](Self::with_pipeline)), or else to [`Acknowledge`]. The
    /// intake finishes the commands it received once they all stop.
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
            pipeline,
        } = self;
        drop(routers.slack_messages);
        let drain_timeout = app.config().server.drain_timeout();
        let (stop, stopping) = watch::channel(false);
        let (stop_internal, internal_stopping) = watch::channel(false);

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
            internal_stopping.clone(),
        ));
        tasks.spawn(serve_listener(
            "ctl listener",
            ctl,
            routers.ctl,
            internal_stopping,
        ));
        for worker in routers.workers {
            tasks.spawn(async move {
                worker.task.await;
                worker.name
            });
        }
        let store = app.store().clone();
        let skills = app.skills().clone();
        let sweeping = stopping.clone();
        tasks.spawn(async move {
            sweeper::run(store, skills, SWEEP_INTERVAL, sweeping).await;
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
        if let Some(agents) = app.slack_agents() {
            let agents = agents.clone();
            let sweeping = stopping.clone();
            tasks.spawn(async move {
                agents.run(INSTALL_SWEEP_INTERVAL, sweeping).await;
                "Slack agent app sweeper"
            });
        }
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
            let onward = match &pipeline {
                Some(pipeline) => pipeline.sink(manager.surface.caps()),
                None => Sender::new(Acknowledge::new(
                    manager.agents.clone(),
                    manager.binding.bot.clone(),
                )),
            };
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
            turns = pipeline.is_some(),
            "listening"
        );

        let mut abort = std::pin::pin!(abort);
        let mut failure = tokio::select! {
            () = shutdown => None,
            Some(joined) = tasks.join_next() => Some(stopped_early(joined)),
        };
        tracing::info!(
            drain_timeout_secs = drain_timeout.as_secs(),
            "shutting down: no longer accepting connections"
        );
        let deadline = tokio::time::Instant::now() + drain_timeout;
        stop.send_replace(true);

        let mut forced = false;
        if let Some(pipeline) = &pipeline {
            pipeline.close();
            let drained = tokio::select! {
                drained = tokio::time::timeout_at(deadline, pipeline.drain()) => drained.is_ok(),
                () = abort.as_mut() => {
                    forced = true;
                    false
                }
            };
            if !drained {
                tracing::warn!("turns still running at the drain's end; dropping them");
                pipeline.cut_short().await;
            }
        }
        stop_internal.send_replace(true);

        let cut_short = if forced {
            Some("shutdown forced; dropping in-flight work")
        } else {
            let drain = async {
                while let Some(joined) = tasks.join_next().await {
                    if let Err(err) = joined {
                        failure.get_or_insert(panicked(err));
                    }
                }
            };
            tokio::select! {
                drained = tokio::time::timeout_at(deadline, drain) => {
                    drained.is_err().then_some("drain timeout elapsed; dropping in-flight work")
                }
                () = abort.as_mut() => {
                    forced = true;
                    Some("shutdown forced; dropping in-flight work")
                }
            }
        };
        if let Some(reason) = cut_short {
            tracing::warn!(unfinished_tasks = tasks.len(), "{reason}");
            tasks.shutdown().await;
        }
        if let Some(pipeline) = &pipeline {
            if forced || tokio::time::Instant::now() >= deadline {
                tracing::warn!("leaving warm sandboxes for the next start to reap");
            } else {
                let left = tokio::select! {
                    stopped = tokio::time::timeout_at(deadline, pipeline.stop_sessions()) => {
                        stopped.is_err().then_some("drain timeout elapsed")
                    }
                    () = abort.as_mut() => {
                        forced = true;
                        Some("shutdown forced")
                    }
                };
                if let Some(reason) = left {
                    tracing::warn!("{reason}; leaving warm sandboxes for the next start to reap");
                }
            }
        }
        drop(pipeline);
        if !forced {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let drained = tokio::select! {
                drained = app.skills().drain(left.max(SKILL_DRAIN_FLOOR)) => drained,
                () = abort.as_mut() => false,
            };
            if !drained {
                tracing::warn!(
                    "skill changes still running at shutdown; the next change or start tidies what they leave"
                );
            }
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
    use crate::config::Config;
    use crate::config::tests::{MINIMAL, env};
    use crate::telemetry::tests::global_logs;

    async fn build_routers(upstream: Option<&str>) {
        let text = match upstream {
            Some(upstream) => format!("{MINIMAL}\n[proxy]\nupstream = \"{upstream}\"\n"),
            None => MINIMAL.to_owned(),
        };
        let config = Config::parse(&text, env()).unwrap();
        let store = store::Store::open_in_memory(config.sealer().unwrap())
            .await
            .unwrap();
        let app = App::new(config, store, None).unwrap();
        Routers::new(&app).unwrap();
    }

    #[tokio::test]
    async fn another_upstream_than_the_default_is_logged_as_a_warning() {
        let upstream = "https://llm-gateway.example.com";
        let logs = global_logs().tag();
        build_routers(Some(upstream)).await;
        let out = logs.snapshot();
        let warnings: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|line| matches!(line["level"].as_str(), Some("WARN" | "ERROR")))
            .collect();
        assert_eq!(warnings.len(), 1, "{out}");
        assert_eq!(warnings[0]["level"], "WARN");
        assert_eq!(warnings[0]["fields"]["upstream"], upstream);

        let slashed = "https://api.anthropic.com/";
        for quiet in [None, Some(cred_proxy::DEFAULT_UPSTREAM), Some(slashed)] {
            build_routers(quiet).await;
        }
        global_logs()
            .snapshot()
            .matching("forwards real credentials to proxy.upstream")
            .assert_has(&format!("\"upstream\":\"{upstream}\""))
            .assert_lacks(&format!(
                "\"upstream\":\"{}\"",
                cred_proxy::DEFAULT_UPSTREAM
            ))
            .assert_lacks(&format!("\"upstream\":\"{slashed}\""));
    }

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
