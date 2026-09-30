//! The turn pipeline: from the messages surfaces deliver to the agents'
//! replies, through sessions, sandboxes and `claude` processes.
//!
//! - [`Pipeline`]: routes each message to the agents it addresses, runs
//!   their turns and delivers what they made.
//! - [`StoreSurfaces`]: the surface each agent's bot posts through.
//! - [`Hooks`]: agentd's [`TurnHooks`](runner::TurnHooks). Each process
//!   gets a placeholder from the [`Registry`](cred_proxy::Registry) the
//!   credential proxy checks, and an agentctl token from [`Ctl`], and each
//!   turn points the one and records itself on the other.
//! - [`Turns`]: the runner's [`SessionManager`] over a [`Sandbox`], with
//!   those hooks.
//! - [`connect_docker`]: the [`DockerSandbox`] `[sandbox]` names, with the
//!   containers a previous run left stopped.
//!
//! [`Ctl`]: crate::ctl::Ctl

mod billing;
mod hooks;
mod message;
mod run;
mod surfaces;
mod view;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use runner::{PoolConfig, ProcessConfig, SessionConfig, SessionManager};
use sandbox::{DockerSandbox, Sandbox};
use time::OffsetDateTime;

use crate::app::App;
use crate::commands::SessionControl;
use crate::config::Config;
use crate::policy::Limits;

pub use billing::{
    COMMUNITY_KEY_REFUSED_TEXT, COMMUNITY_USAGE_LIMIT_TEXT, FAILURE_DM_INTERVAL,
    LOGIN_EXPIRED_TEXT, USAGE_LIMIT_TEXT,
};
pub use hooks::{AGENTCTL_TOKEN_VAR, AGENTCTL_URL_VAR, Hooks, ProcessHandle};
pub use message::HISTORY_LIMIT;
pub use run::{
    DEFAULT_MAX_PENDING, DEFAULT_MAX_PENDING_PER_OWNER, DEFAULT_QUEUE_PER_THREAD,
    DEFAULT_WORKING_EMOJI, DELIVERY_FAILED_TEXT, FAILED_TEXT, Pipeline, PipelineSettings,
    REFUSAL_DM_INTERVAL, RESTARTING_TEXT, TIMED_OUT_TEXT, TRUNCATED_NOTE, UNCONFIRMED_TEXT,
};
pub use surfaces::StoreSurfaces;

/// The agentctl API as sandboxes reach it: agentd's name on the sandbox
/// network and the ctl listener's port. It is in the egress environment's
/// `NO_PROXY`, so agentctl reaches it directly.
pub const AGENTCTL_URL: &str = "http://agentctl.internal:8081";

/// How [`Turns`] runs processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSettings {
    /// How processes are started. Its `anthropic_base_url` is where the
    /// credential proxy is, as sandboxes reach it.
    pub process: ProcessConfig,
    /// How containers are kept warm.
    pub pool: PoolConfig,
    /// The sandbox image.
    pub image: String,
    /// agentd's data directory, which holds the agents' persona files.
    pub data_dir: PathBuf,
    /// The agentctl API, as sandboxes reach it.
    pub agentctl_url: String,
    /// Added to every process's environment after the egress proxy's
    /// variables, overriding them. It holds no secret. agentd sets nothing
    /// here; tests running `fake-claude` name its script and reach agentd
    /// by address.
    pub env: BTreeMap<String, String>,
}

impl TurnSettings {
    /// The settings `config` gives, or `None` without a `[sandbox]`
    /// section: processes reach the credential proxy at
    /// [`cred_proxy::PROXY_URL`] and the agentctl API at [`AGENTCTL_URL`].
    pub fn from_config(config: &Config) -> Option<Self> {
        let sandbox = config.sandbox.as_ref()?;
        Some(Self {
            process: config.runner.process(),
            pool: config.runner.pool(),
            image: sandbox.image.clone(),
            data_dir: config.store.data_dir.clone(),
            agentctl_url: AGENTCTL_URL.to_owned(),
            env: BTreeMap::new(),
        })
    }
}

/// The runner's sessions, over a sandbox, with agentd's [`Hooks`].
///
/// Cloning is cheap: every clone shares the same sessions. The idle reaper
/// and the sandbox event follower run until the last clone is dropped.
#[derive(Debug, Clone)]
pub struct Turns {
    sessions: Arc<SessionManager<Hooks>>,
}

impl Turns {
    /// Starts the runner over `sandbox`, with hooks over `app`'s registry
    /// and agentctl API, and hands its sessions to `app`'s commands, for
    /// `sessions` and `reset`, for as long as the runner runs.
    ///
    /// It spawns the idle reaper and the event follower, so it must be
    /// called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// If `settings` are invalid.
    pub fn start(
        app: &App,
        sandbox: Arc<dyn Sandbox>,
        settings: TurnSettings,
    ) -> anyhow::Result<Self> {
        let hooks = Hooks::new(
            app.registry().clone(),
            app.ctl().clone(),
            settings.agentctl_url,
            settings.env,
        );
        let sessions = SessionManager::new(
            app.store().clone(),
            sandbox,
            hooks,
            SessionConfig {
                process: settings.process,
                pool: settings.pool,
                image: settings.image,
                data_dir: settings.data_dir,
            },
        )
        .context("starting the runner")?;
        let sessions = Arc::new(sessions);
        let control: Arc<dyn SessionControl> = sessions.clone();
        app.commands().use_sessions(Arc::downgrade(&control));
        Ok(Self { sessions })
    }

    /// The sessions.
    pub fn sessions(&self) -> &SessionManager<Hooks> {
        &self.sessions
    }
}

impl PipelineSettings {
    /// The settings `app` gives: its data directory, its manager bots,
    /// `[community]`'s admins, `[runner]`'s working emoji and models, and
    /// `[limits]`' caps, with the default queue bounds and the system
    /// clock.
    pub fn from_app(app: &App) -> Self {
        let managers = app
            .rocketchat()
            .map(|manager| manager.binding.bot.clone())
            .into_iter()
            .chain(app.slack().map(|slack| slack.bot()))
            .collect();
        let runner = &app.config().runner;
        Self {
            data_dir: app.config().store.data_dir.clone(),
            managers,
            admins: app.config().community.admins.clone(),
            working_emoji: runner.working_emoji.clone(),
            models: runner.models.clone(),
            queue_per_thread: DEFAULT_QUEUE_PER_THREAD,
            max_pending: DEFAULT_MAX_PENDING,
            max_pending_per_owner: DEFAULT_MAX_PENDING_PER_OWNER,
            limits: Limits::from_config(&app.config().limits),
            now: OffsetDateTime::now_utc,
        }
    }
}

impl Pipeline {
    /// The pipeline of `app`, running turns on `turns`: posting through
    /// `app`'s surfaces and prompting for links through its manager bots.
    pub fn for_app(app: &App, turns: Turns) -> Self {
        Self::new(
            app.store().clone(),
            turns,
            Arc::clone(app.surfaces()),
            app.commands().replies().clone(),
            PipelineSettings::from_app(app),
        )
    }
}

/// Connects to the Docker daemon for the sandboxes `config`'s `[sandbox]`
/// section describes, and stops every container a previous run of this
/// agentd instance left: their placeholders and agentctl tokens died with
/// it. `None` without the section.
///
/// # Errors
///
/// If the daemon can't be reached, or a left container can't be stopped.
pub async fn connect_docker(app: &App) -> anyhow::Result<Option<Arc<dyn Sandbox>>> {
    let Some(settings) = app.config().sandbox.clone() else {
        return Ok(None);
    };
    let docker = DockerSandbox::connect(
        app.store().clone(),
        app.config().store.data_dir.clone(),
        settings,
    )
    .await
    .context("connecting to the Docker daemon for [sandbox]")?;
    let reaped = docker
        .reap_orphans()
        .await
        .context("stopping the sandboxes left from before the restart")?;
    tracing::info!(reaped, "stopped the sandboxes left from before the restart");
    Ok(Some(Arc::new(docker)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{MINIMAL, env, sandboxed};

    #[test]
    fn settings_come_from_the_sandbox_and_runner_sections() {
        let plain = Config::parse(MINIMAL, env()).unwrap();
        assert_eq!(TurnSettings::from_config(&plain), None);

        let text = format!(
            "{}\n[sandbox]\nimage = \"img:1\"\n[runner]\nturn_timeout_secs = 5\n",
            sandboxed()
        );
        let config = Config::parse(&text, env()).unwrap();
        let settings = TurnSettings::from_config(&config).unwrap();
        assert_eq!(settings.image, "img:1");
        assert_eq!(settings.process.turn_timeout_secs, 5);
        assert_eq!(settings.process.anthropic_base_url, cred_proxy::PROXY_URL);
        assert_eq!(settings.agentctl_url, AGENTCTL_URL);
        assert_eq!(settings.pool, PoolConfig::default());
        assert_eq!(settings.data_dir, config.store.data_dir);
        assert!(settings.env.is_empty());
        let (_, no_proxy) = cred_proxy::EGRESS_ENV
            .iter()
            .find(|(name, _)| *name == "NO_PROXY")
            .unwrap();
        for url in [cred_proxy::PROXY_URL, AGENTCTL_URL] {
            let host = url
                .strip_prefix("http://")
                .and_then(|rest| rest.split(':').next())
                .unwrap();
            assert!(no_proxy.split(',').any(|name| name == host), "{url}");
        }
    }

    #[tokio::test]
    async fn no_sandbox_section_means_no_docker() {
        let config = Config::parse(MINIMAL, env()).unwrap();
        let store = store::Store::open_in_memory(config.sealer().unwrap())
            .await
            .unwrap();
        let app = App::new(config, store, None).unwrap();
        assert!(connect_docker(&app).await.unwrap().is_none());
    }
}
