//! `omp-deck-gateway`: see [`deck_service::gateway::cli`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let code = deck_service::gateway::cli::main(std::env::args().skip(1));
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
