//! Crate-wide error type.
//!
//! Every fallible operation in the daemon returns [`Result<T>`] — an alias for
//! `std::result::Result<T, Error>`. The [`Error`] enum is the canonical sum of every
//! way an operation can fail; lower layers map their native errors into it via `From`
//! impls (mostly auto-derived by `thiserror`'s `#[from]`).

use std::path::PathBuf;

/// Top-level error type for `air-drive`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O error (filesystem, network sockets, sub-process pipes).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite error from the embedded state DB.
    #[error("state DB error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Google Drive REST API error that isn't tied to an HTTP status (missing
    /// field, malformed JSON, …).
    #[error("drive API error: {0}")]
    Drive(String),

    /// Google Drive returned a non-success HTTP status. Carrying the numeric
    /// `status` (and the `reason` Drive puts in the body) lets retry/404
    /// classification be type-driven instead of string-matching the message.
    #[error("drive API error: HTTP {status}: {body}")]
    DriveHttp {
        /// HTTP status code (e.g. 404, 429, 500, 503).
        status: u16,
        /// Response body (Drive's JSON error, kept for the `reason` + diagnostics).
        body: String,
    },

    /// Connection-level failure reaching Drive (DNS, TCP, TLS, timeout). Always
    /// retry-eligible — the request never got a status back.
    #[error("network error: {0}")]
    Network(String),

    /// OAuth / token error (refresh failure, invalid `client_id`, revoked grant).
    #[error("OAuth error: {0}")]
    Oauth(String),

    /// Configuration error (TOML parse failure, invalid section, schema upgrade required).
    #[error("config error: {0}")]
    Config(String),

    /// `rclone` subprocess error. `stderr` is captured for diagnostics.
    #[error("rclone error: {stderr}")]
    Rclone {
        /// Stderr captured from the failed `rclone` invocation.
        stderr: String,
    },

    /// Single-instance lock is held by another live daemon (CLI exit code 6).
    #[error("another daemon is already running (pid {pid:?})")]
    Lock {
        /// PID of the running daemon if it could be read from the lock file.
        pid: Option<u32>,
    },

    /// Folder mapping problem: missing local path, unresolvable remote, etc.
    #[error("mapping error: {0}")]
    Mapping(String),

    /// TOML (de)serialisation error coming from the `toml` crate.
    #[error("TOML error: {0}")]
    Toml(String),

    /// File permissions are too loose (tokens MUST be `0600`).
    #[error("file {path} has insecure permissions: got {got:o}, want {want:o}")]
    InsecurePermissions {
        /// Offending file.
        path: PathBuf,
        /// Mode found on disk (POSIX, octal).
        got: u32,
        /// Required mode (POSIX, octal).
        want: u32,
    },
}

impl From<toml::de::Error> for Error {
    fn from(value: toml::de::Error) -> Self {
        Error::Toml(value.to_string())
    }
}

impl From<toml::ser::Error> for Error {
    fn from(value: toml::ser::Error) -> Self {
        Error::Toml(value.to_string())
    }
}

impl From<tokio_rusqlite::Error> for Error {
    fn from(value: tokio_rusqlite::Error) -> Self {
        match value {
            tokio_rusqlite::Error::Rusqlite(e) => Error::Sqlite(e),
            other => Error::Config(format!("state DB connection error: {other}")),
        }
    }
}

/// Crate-wide `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_permissions_displays() {
        let err = Error::InsecurePermissions {
            path: PathBuf::from("/tmp/probe"),
            got: 0o644,
            want: 0o600,
        };
        let s = format!("{err}");
        assert!(s.contains("/tmp/probe"));
        assert!(s.contains("644"));
        assert!(s.contains("600"));
    }
}
