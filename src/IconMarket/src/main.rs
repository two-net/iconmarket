//! IconMarket: a village marketplace run entirely through a LINE Official Account.
//!
//! One process (NFR-MNT-01) organised by the modules of SRS §2.1.

mod admin;
mod ai;
mod catalog;
mod config;
mod dispatcher;
mod http;
mod logging;
mod notifications;
mod orders;
mod payments;
mod persistence;
mod scheduler;
mod shutdown;
mod startup;
mod users;
mod webhook;

use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    // Before any thread exists, see its docs
    persistence::clear_libpq_env();

    // Load and validate the config before logging exists, so problems go to stderr
    let path = config::path_from_args(std::env::args_os());
    let config = match config::load(&path).and_then(|config| {
        config::validate(&config)?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("icon-market: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    // Held until exit so buffered log lines are flushed
    let _log_guard = match logging::init(&config.logging) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("icon-market: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(startup::run(config));
    // Tasks abandoned by an expired drain must not keep the process alive
    runtime.shutdown_timeout(Duration::from_secs(1));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "exiting after a fatal error");
            ExitCode::FAILURE
        }
    }
}
