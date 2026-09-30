//! In-sandbox CLI that agents use to call back into agentd.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser, error::ErrorKind};

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn prints_version() {
        let err = Cli::try_parse_from(["agentctl", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }
}
