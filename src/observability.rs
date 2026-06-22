//! Tracing init + standardised operation-span fields.
//!
//! Every operation log line MUST include the fields `event`, `op_id`, `item_id` (when
//! applicable), and `relative_path` (when applicable). Use the [`op_span!`] macro at the
//! caller site to enter an instrumented span carrying these fields.

use std::io::IsTerminal;
use std::path::Path;

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::{LogColor, LogFormat};
use crate::error::{Error, Result};

/// Inputs that drive the global `tracing` subscriber, merged from the CLI and
/// `config.toml`.
#[derive(Debug, Clone, Copy)]
pub struct LogOptions<'a> {
    /// `-v` count from the CLI (`0` = none). Any value `> 0` overrides
    /// [`Self::level`] but not `RUST_LOG`.
    pub verbose: u8,
    /// When `Some`, log records are duplicated to that file in addition to
    /// `stderr`. The file is opened in append mode, always colour-free.
    pub log_file: Option<&'a Path>,
    /// Persistent log level from `[daemon].log_level`. Empty means "unset".
    pub level: &'a str,
    /// Log record format for both layers.
    pub format: LogFormat,
    /// ANSI colour policy for the stderr layer.
    pub color: LogColor,
}

/// Initialise the global `tracing` subscriber from [`LogOptions`].
///
/// Level precedence (highest first):
/// 1. `RUST_LOG` environment variable.
/// 2. `-v` flags: `1` → `info`, `2` → `debug`, `3+` → `trace`.
/// 3. `[daemon].log_level` from `config.toml` (bare level applies to the
///    `air_drive` target; a value containing `=` is a full filter directive).
/// 4. Built-in default `air_drive=warn`.
pub fn init_tracing(opts: &LogOptions<'_>) -> Result<()> {
    let env_filter = resolve_filter(opts)?;

    let ansi = match opts.color {
        LogColor::Always => true,
        LogColor::Never => false,
        LogColor::Auto => std::io::stderr().is_terminal(),
    };

    let mut layers: Vec<Box<dyn Layer<_> + Send + Sync>> = Vec::new();
    layers.push(fmt_layer(std::io::stderr, ansi, opts.format));

    if let Some(path) = opts.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| Error::Config(format!("cannot open log file {}: {e}", path.display())))?;
        layers.push(fmt_layer(file, false, opts.format));
    }

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layers)
        .try_init()
        .map_err(|e| Error::Config(format!("tracing subscriber already initialised: {e}")))?;

    Ok(())
}

/// Resolve the [`EnvFilter`] following the documented precedence.
fn resolve_filter(opts: &LogOptions<'_>) -> Result<EnvFilter> {
    // RUST_LOG always wins when set, with its full native semantics.
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return Ok(filter);
    }
    let directive = config_directive(opts.verbose, opts.level)?;
    // Already validated by `config_directive`, so `new` cannot drop directives.
    Ok(EnvFilter::new(directive))
}

/// Decide the filter directive from the `-v` count and the persistent
/// `log_level`, ignoring `RUST_LOG` (handled separately by [`resolve_filter`]).
///
/// Precedence: an explicit `-v` overrides the config level; an empty config
/// level falls back to `air_drive=warn`. A bare level applies to the
/// `air_drive` target; a value containing `=` is passed through as a full
/// filter directive. The result is validated before being returned.
fn config_directive(verbose: u8, level: &str) -> Result<String> {
    if verbose > 0 {
        let level = match verbose {
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        return Ok(format!("air_drive={level}"));
    }
    let level = level.trim();
    if level.is_empty() {
        return Ok("air_drive=warn".to_string());
    }
    let directive = if level.contains('=') {
        level.to_string()
    } else {
        format!("air_drive={level}")
    };
    EnvFilter::try_new(&directive)
        .map_err(|e| Error::Config(format!("invalid log_level {level:?}: {e}")))?;
    Ok(directive)
}

/// Build a type-erased `fmt` layer for `writer`, honouring the chosen format
/// and ANSI policy.
fn fmt_layer<S, W>(writer: W, ansi: bool, format: LogFormat) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true)
        .with_ansi(ansi);
    match format {
        LogFormat::Text => layer.boxed(),
        LogFormat::Json => layer.json().boxed(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_overrides_config_level() {
        // Any -v wins over a config level, mapping count → level.
        assert_eq!(config_directive(1, "trace").unwrap(), "air_drive=info");
        assert_eq!(config_directive(2, "").unwrap(), "air_drive=debug");
        assert_eq!(config_directive(3, "warn").unwrap(), "air_drive=trace");
        assert_eq!(config_directive(9, "").unwrap(), "air_drive=trace");
    }

    #[test]
    fn empty_config_level_defaults_to_warn() {
        assert_eq!(config_directive(0, "").unwrap(), "air_drive=warn");
        // Whitespace-only is treated as empty.
        assert_eq!(config_directive(0, "  ").unwrap(), "air_drive=warn");
    }

    #[test]
    fn bare_config_level_targets_air_drive() {
        assert_eq!(config_directive(0, "debug").unwrap(), "air_drive=debug");
        // Surrounding whitespace is trimmed.
        assert_eq!(config_directive(0, " info ").unwrap(), "air_drive=info");
    }

    #[test]
    fn directive_config_level_passes_through() {
        let d = config_directive(0, "air_drive=debug,rclone=warn").unwrap();
        assert_eq!(d, "air_drive=debug,rclone=warn");
    }

    #[test]
    fn invalid_config_level_is_rejected() {
        let err = config_directive(0, "notalevel").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }
}
