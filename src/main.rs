//! air-drive — open source Google Drive sync daemon for Linux.
//!
//! This binary exposes the `air-drive` CLI: `link`, `map`, `start`, `pause`, `resume`,
//! `status`, `unlink`, and the `setup` orchestrator. See `specs/001-minimal-sync-daemon/`
//! for the feature specification, implementation plan, and tasks.

#![forbid(unsafe_code)]

mod cli;
mod config;
mod daemon;
mod drive;
mod engine;
mod error;
mod reconcile;
mod state;
mod watch;

fn main() {
    println!("air-drive — not yet wired up. See specs/001-minimal-sync-daemon/.");
}
