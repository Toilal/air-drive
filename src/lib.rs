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
