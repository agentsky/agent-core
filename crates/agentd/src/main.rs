//! The agentd binary. Everything but reading the process's arguments and
//! environment lives in the library, in `agentd::cli`.

fn main() -> std::process::ExitCode {
    agentd::cli::main(std::env::args_os(), std::env::vars_os())
}
