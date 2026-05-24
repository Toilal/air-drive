//! Tracing init + standardised operation-span fields.
//!
//! Every operation log line MUST include the fields `event`, `op_id`, `item_id` (when
//! applicable), and `relative_path` (when applicable). Use the [`op_span!`] macro at the
//! caller site to enter an instrumented span carrying these fields.

use std::path::Path;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::{Error, Result};

/// Initialise the global `tracing` subscriber.
///
/// Verbosity:
/// - `0` → `warn` (default)
/// - `1` → `info`
/// - `2` → `debug`
/// - `3+` → `trace`
///
/// `RUST_LOG` is honoured when set and overrides the verbosity argument.
///
/// When `log_file` is `Some`, log records are duplicated to that file in addition to
/// `stderr`. The file is opened in append mode.
pub fn init_tracing(verbose: u8, log_file: Option<&Path>) -> Result<()> {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("air_drive={default_level}")));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer);

    match log_file {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| {
                    Error::Config(format!("cannot open log file {}: {e}", path.display()))
                })?;
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .with_target(true);
            registry.with(file_layer).try_init().map_err(|e| {
                Error::Config(format!("tracing subscriber already initialised: {e}"))
            })?;
        }
        None => {
            registry.try_init().map_err(|e| {
                Error::Config(format!("tracing subscriber already initialised: {e}"))
            })?;
        }
    }

    Ok(())
}

/// Build an `INFO`-level span carrying the standard operation fields.
///
/// Two arities:
///
/// ```ignore
/// // Minimal: just the event name and the operation id.
/// let _g = op_span!("upload", op_id).entered();
///
/// // Full: event + op_id + item_id + relative_path.
/// let _g = op_span!("upload", op_id, item_id, "Documents/notes.md").entered();
/// ```
///
/// Both forms expand to `tracing::info_span!(...)` with the canonical field names.
#[macro_export]
macro_rules! op_span {
    ($event:expr, $op_id:expr) => {
        ::tracing::info_span!($event, op_id = $op_id)
    };
    ($event:expr, $op_id:expr, $item_id:expr, $relative_path:expr) => {
        ::tracing::info_span!(
            $event,
            op_id = $op_id,
            item_id = $item_id,
            relative_path = $relative_path
        )
    };
}
