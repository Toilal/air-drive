//! air-drive — open source Google Drive sync daemon for Linux.
//!
//! This crate ships both a library (`air_drive`) and a binary (`air-drive`). The library
//! groups the building blocks (config, state, sync engine, drive client, watcher,
//! reconciler) so they can be exercised by integration tests independently of the CLI
//! surface. The binary is a thin clap dispatcher over the library — see `src/main.rs`.
//!
//! See `specs/001-minimal-sync-daemon/` for the feature specification, plan, and tasks.

#![forbid(unsafe_code)]

pub mod cli;
pub mod config;
pub mod daemon;
pub mod drive;
pub mod engine;
pub mod error;
pub mod observability;
pub mod reconcile;
pub mod state;
pub mod watch;

pub use error::{Error, Result};

/// The crate version string, git-aware when the binary was built from a
/// checkout. Matches `CARGO_PKG_VERSION` on a clean tag, appends the commit
/// count + short SHA otherwise (e.g. `0.1.1-12-gff7bba8`, with a trailing
/// `-dirty` if the working tree had uncommitted changes at build time).
/// Falls back to `CARGO_PKG_VERSION` when built outside git (release
/// tarball).
pub const VERSION: &str = match option_env!("AIR_DRIVE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};
