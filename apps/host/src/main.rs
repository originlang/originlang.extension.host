//! `ol-host` — the independent host service.
//!
//! This binary owns the server entry point and the transport. It is the only
//! thing a client (CLI, HomeWorld, Desktop) needs to download and run; it does
//! not require the `bootidea` CLI to be installed. `ol-host` is the sole
//! official service entry; the previous name `oext-host` is not shipped as an
//! alias, so consumers must start the new name (see the migration note in
//! `docs/service-contract.md` §1.2).
//!
//! Contract: see `docs/service-contract.md` (H01) and `docs/release-contract.md`
//! (H02). The behaviour here is defined by that contract, not the reverse.

mod config;
mod provider;
mod serve;

use std::process::ExitCode;

/// Exit codes defined by the service contract (§9.3).
mod exit {
    /// Clean shutdown (request, EOF, signal).
    pub const OK: u8 = 0;
    /// Unclassified failure, or graceful-period timeout force-kill.
    pub const FAILURE: u8 = 1;
    /// Argument or configuration error.
    pub const USAGE: u8 = 2;
    /// Same-root instance conflict, or listen-bind failure.
    pub const CONFLICT: u8 = 3;
    /// Fatal protocol error on stdio.
    pub const PROTOCOL: u8 = 4;
}

fn main() -> ExitCode {
    let args = match config::Args::parse() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("ol-host: {error}");
            return ExitCode::from(exit::USAGE);
        }
    };

    if args.version {
        println!("{}", config::HOST_VERSION);
        return ExitCode::from(exit::OK);
    }
    if args.help {
        config::print_usage();
        return ExitCode::from(exit::OK);
    }

    let settings = match config::load(&args) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("ol-host: {error}");
            return ExitCode::from(exit::USAGE);
        }
    };

    let outcome = serve::serve(&settings);
    match outcome {
        Ok(()) => ExitCode::from(exit::OK),
        Err(serve::ServeError::Conflict(message)) => {
            eprintln!("ol-host: {message}");
            ExitCode::from(exit::CONFLICT)
        }
        Err(serve::ServeError::Protocol(message)) => {
            eprintln!("ol-host: {message}");
            ExitCode::from(exit::PROTOCOL)
        }
        Err(serve::ServeError::Failure(message)) => {
            eprintln!("ol-host: {message}");
            ExitCode::from(exit::FAILURE)
        }
    }
}
