//! Daemon orchestration: event loop, single-instance lock, control socket, shutdown.
//!
//! Phase 2c lands only the [`lock`] primitive (FR-017). The actual event loop, control
//! socket, and shutdown handling are written in Phase 4 (US2) and Phase 5 (US3).

pub mod lock;
