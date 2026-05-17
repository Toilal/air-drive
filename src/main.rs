//! `air-drive` binary entry point.
//!
//! All real logic lives in the library crate (`air_drive`). This file parses the CLI,
//! initialises tracing, dispatches to the subcommand handler, and translates the
//! returned [`ExitCode`] into the process exit status the user observes.

#![forbid(unsafe_code)]

use std::process::ExitCode as StdExitCode;

use clap::Parser;

use air_drive::cli::{Cli, ExitCode, dispatch, fallback_exit_code};
use air_drive::observability::init_tracing;

fn main() -> StdExitCode {
    let cli = Cli::parse();
    if let Err(e) = init_tracing(cli.verbose, cli.log_file.as_deref()) {
        eprintln!("failed to initialise tracing: {e}");
        return StdExitCode::from(ExitCode::GenericError as u8);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build tokio runtime");
            return StdExitCode::from(ExitCode::GenericError as u8);
        }
    };
    let code = rt.block_on(async {
        match dispatch(cli).await {
            Ok(code) => code,
            Err(e) => {
                tracing::error!(error = %e, "command failed");
                eprintln!("Error: {e}");
                fallback_exit_code(&e)
            }
        }
    });
    StdExitCode::from(code as u8)
}
