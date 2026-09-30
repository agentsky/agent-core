//! The command line: `agentd serve`, `agentd migrate` and `agentd gen-key`.

use std::ffi::OsString;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret as _;
use store::Sealer;

use crate::app::{self, App};
use crate::config::Config;
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
    /// Run the daemon until SIGTERM or SIGINT.
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
            let config = Config::load(&config, env)?;
            telemetry::init(&config.server.log_filter)?;
            runtime()?.block_on(async {
                let shutdown = shutdown_signal()?;
                serve(config, shutdown).await
            })
        }
        Command::Migrate { config } => {
            let config = Config::load(&config, env)?;
            telemetry::init(&config.server.log_filter)?;
            runtime()?.block_on(migrate(config))
        }
    }
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

/// `agentd serve`: opens the store, binds the listeners, and serves until
/// `shutdown` completes, then shuts down gracefully (see [`Server::run`]).
///
/// # Errors
///
/// If the store can't be opened, a listener can't be bound, or a listener
/// fails while serving.
pub async fn serve<F>(config: Config, shutdown: F) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let app = App::open(config).await?;
    let server = Server::bind(app.clone(), Routers::new(&app)).await?;
    server.run(shutdown).await
}

/// Completes on the first SIGTERM or SIGINT. The handlers are installed
/// before this returns, so a signal that arrives during startup is not lost.
///
/// # Errors
///
/// If a signal handler can't be installed.
#[cfg(unix)]
pub fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate =
        signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => tracing::info!(signal = "SIGTERM", "received a shutdown signal"),
            _ = interrupt.recv() => tracing::info!(signal = "SIGINT", "received a shutdown signal"),
        }
    })
}

/// Completes on Ctrl-C.
///
/// # Errors
///
/// Never; the `Result` matches the Unix version.
#[cfg(not(unix))]
pub fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()> + Send> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(signal = "ctrl-c", "received a shutdown signal");
    })
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;
    use clap::error::ErrorKind;
    use secrecy::SecretString;

    use std::path::Path;

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

    #[test]
    fn gen_key_reports_a_broken_output() {
        let err = gen_key(&mut Broken).unwrap_err();
        assert!(format!("{err:#}").contains("writing the key"), "{err:#}");
    }
}
