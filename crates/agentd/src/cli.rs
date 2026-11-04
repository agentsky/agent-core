//! The command line: `agentd serve`, `agentd migrate` and `agentd gen-key`.

use std::ffi::OsString;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret as _;
use store::Sealer;
use tokio::sync::watch;

use crate::app::{self, App};
use crate::config::Config;
use crate::pipeline::{self, Pipeline, TurnSettings, Turns};
use crate::server::{Routers, Server};
use crate::telemetry;

/// The agent-core daemon.
#[derive(Debug, Parser)]
#[command(name = "agentd", version, about)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// agentd's subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon until SIGTERM or SIGINT. A second signal stops it
    /// without waiting for in-flight requests.
    Serve {
        /// The configuration file.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Apply pending database migrations, then exit.
    Migrate {
        /// The configuration file.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Print a new random master key for AGENTD_MASTER_KEY.
    GenKey,
}

/// Runs agentd with the process's arguments and environment, and returns
/// the exit status. Errors are printed to standard error.
pub fn main<A, T, E, K, V>(args: A, env: E) -> ExitCode
where
    A: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli, env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agentd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run<E, K, V>(cli: Cli, env: E) -> anyhow::Result<()>
where
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    match cli.command {
        Command::GenKey => gen_key(&mut std::io::stdout().lock()),
        Command::Serve { config } => {
            let config = load(&config, env)?;
            runtime()?.block_on(async {
                let signals = ShutdownSignals::install()?;
                serve(config, signals.first(), signals.second()).await
            })
        }
        Command::Migrate { config } => {
            let config = load(&config, env)?;
            runtime()?.block_on(migrate(config))
        }
    }
}

/// Loads the configuration, sets up logging, and warns about each unknown
/// `AGENTD_` variable the configuration ignored.
fn load<E, K, V>(path: &Path, env: E) -> anyhow::Result<Config>
where
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let config = Config::load(path, env)?;
    telemetry::init(&config.server.log_filter)?;
    for name in &config.unknown_env {
        tracing::warn!(variable = %name, "ignoring an unknown AGENTD_ environment variable");
    }
    Ok(config)
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")
}

/// `agentd gen-key`: writes a new master key and a newline to `out`.
///
/// # Errors
///
/// If the operating system's random number generator or `out` fails.
pub fn gen_key(out: &mut impl Write) -> anyhow::Result<()> {
    let key = Sealer::generate_key().context("generating a key")?;
    writeln!(out, "{}", key.expose_secret()).context("writing the key")?;
    out.flush().context("writing the key")
}

/// `agentd migrate`: opens the store, which applies pending migrations, and
/// closes it.
///
/// # Errors
///
/// If the store can't be opened or migrated.
pub async fn migrate(config: Config) -> anyhow::Result<()> {
    let store = app::open_store(&config).await?;
    store.close().await;
    tracing::info!("the store is migrated");
    Ok(())
}

/// `agentd serve`: opens the store, connects to the Docker daemon for
/// `[sandbox]` and stops the sandboxes left from before, binds the
/// listeners, starts the runner, and serves until `shutdown` completes, then
/// shuts down gracefully, cutting the drain short if `abort` completes (see
/// [`Server::run`]). Without `[sandbox]` it runs no turns.
///
/// # Errors
///
/// If the store can't be opened, the Docker daemon can't be reached, a
/// listener can't be bound, or a listener fails while serving.
pub async fn serve<F, G>(config: Config, shutdown: F, abort: G) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send,
    G: Future<Output = ()> + Send,
{
    let app = App::open(config).await?;
    let sandbox = pipeline::connect_docker(&app).await?;
    let mut server = Server::bind(app.clone(), Routers::new(&app)?).await?;
    match (sandbox, TurnSettings::from_config(app.config())) {
        (Some(sandbox), Some(settings)) => {
            let turns = Turns::start(&app, sandbox, settings)?;
            server = server.with_pipeline(Pipeline::for_app(&app, turns));
        }
        _ => tracing::warn!("no [sandbox] section: agentd runs no turns"),
    }
    server.run(shutdown, abort).await
}

/// SIGTERM and SIGINT (Ctrl-C elsewhere), counted: the
/// [`first`](Self::first) asks for a graceful shutdown, and the
/// [`second`](Self::second) cuts its drain short.
#[derive(Debug, Clone)]
pub struct ShutdownSignals {
    received: watch::Receiver<u32>,
}

impl ShutdownSignals {
    /// Installs the signal handlers and starts counting. The handlers are
    /// installed before this returns, so a signal that arrives during
    /// startup is not lost.
    ///
    /// # Errors
    ///
    /// If a signal handler can't be installed.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime.
    pub fn install() -> anyhow::Result<Self> {
        let mut source = SignalSource::install()?;
        let (count, received) = watch::channel(0);
        tokio::spawn(async move {
            let mut seen = 0;
            while let Some(signal) = source.next().await {
                seen += 1;
                count.send_replace(seen);
                if seen == 1 {
                    tracing::info!(signal, "received a shutdown signal");
                } else {
                    tracing::warn!(
                        signal,
                        "received a second shutdown signal; dropping in-flight work"
                    );
                    break;
                }
            }
        });
        Ok(Self { received })
    }

    /// Completes on the first signal.
    pub fn first(&self) -> impl Future<Output = ()> + Send + 'static {
        self.nth(1)
    }

    /// Completes on the second signal.
    pub fn second(&self) -> impl Future<Output = ()> + Send + 'static {
        self.nth(2)
    }

    fn nth(&self, n: u32) -> impl Future<Output = ()> + Send + 'static {
        let mut received = self.received.clone();
        async move {
            if received.wait_for(|count| *count >= n).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The installed SIGTERM and SIGINT handlers.
#[cfg(unix)]
struct SignalSource {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl SignalSource {
    fn install() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            terminate: signal(SignalKind::terminate()).context("installing the SIGTERM handler")?,
            interrupt: signal(SignalKind::interrupt()).context("installing the SIGINT handler")?,
        })
    }

    /// The next signal's name, or `None` once no more can arrive.
    async fn next(&mut self) -> Option<&'static str> {
        tokio::select! {
            Some(()) = self.terminate.recv() => Some("SIGTERM"),
            Some(()) = self.interrupt.recv() => Some("SIGINT"),
            else => None,
        }
    }
}

/// Ctrl-C, where there are no Unix signals.
#[cfg(not(unix))]
struct SignalSource;

#[cfg(not(unix))]
impl SignalSource {
    fn install() -> anyhow::Result<Self> {
        Ok(Self)
    }

    /// The next Ctrl-C, or `None` if it can't be listened for.
    async fn next(&mut self) -> Option<&'static str> {
        tokio::signal::ctrl_c().await.ok().map(|()| "ctrl-c")
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;
    use clap::error::ErrorKind;
    use secrecy::SecretString;

    use super::*;

    const NO_ENV: [(&str, &str); 0] = [];

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn prints_version() {
        let err = Cli::try_parse_from(["agentd", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert_eq!(main(["agentd", "--version"], NO_ENV), ExitCode::SUCCESS);
    }

    #[test]
    fn parses_the_subcommands() {
        let cli = Cli::try_parse_from(["agentd", "serve", "--config", "/etc/agentd.toml"]).unwrap();
        assert!(
            matches!(cli.command, Command::Serve { config } if config == Path::new("/etc/agentd.toml"))
        );
        let cli = Cli::try_parse_from(["agentd", "migrate", "--config", "a.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Migrate { .. }));
        let cli = Cli::try_parse_from(["agentd", "gen-key"]).unwrap();
        assert!(matches!(cli.command, Command::GenKey));
    }

    #[test]
    fn usage_errors_exit_with_2() {
        assert_eq!(main(["agentd"], NO_ENV), ExitCode::from(2));
        assert_eq!(main(["agentd", "serve"], NO_ENV), ExitCode::from(2));
        assert_eq!(main(["agentd", "bogus"], NO_ENV), ExitCode::from(2));
    }

    #[test]
    fn config_errors_exit_with_1() {
        for command in ["serve", "migrate"] {
            let code = main(
                ["agentd", command, "--config", "/nonexistent/agentd.toml"],
                NO_ENV,
            );
            assert_eq!(code, ExitCode::FAILURE, "{command}");
        }
    }

    #[test]
    fn gen_key_prints_a_usable_key() {
        let mut out = Vec::new();
        gen_key(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 1);
        Sealer::from_base64(&SecretString::from(text.clone())).unwrap();

        let mut other = Vec::new();
        gen_key(&mut other).unwrap();
        assert_ne!(other, text.into_bytes());
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("closed"))
        }
    }

    #[cfg(unix)]
    fn send_self(signal: &str) {
        let status = std::process::Command::new("kill")
            .args([signal, &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_signal_shuts_down_and_the_second_forces_it() {
        let signals = ShutdownSignals::install().unwrap();
        let (first, second) = (signals.first(), signals.second());
        tokio::pin!(first, second);
        let brief = std::time::Duration::from_millis(100);
        let patient = std::time::Duration::from_secs(10);
        assert!(tokio::time::timeout(brief, &mut first).await.is_err());

        send_self("-INT");
        tokio::time::timeout(patient, &mut first).await.unwrap();
        assert!(
            tokio::time::timeout(brief, &mut second).await.is_err(),
            "one signal is not two"
        );

        send_self("-TERM");
        tokio::time::timeout(patient, &mut second).await.unwrap();
        tokio::time::timeout(patient, signals.first())
            .await
            .unwrap();
    }

    #[test]
    fn gen_key_reports_a_broken_output() {
        let err = gen_key(&mut Broken).unwrap_err();
        assert!(format!("{err:#}").contains("writing the key"), "{err:#}");
    }
}
