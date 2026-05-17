//! `RcloneEngine` — the initial [`SyncEngine`] implementation.
//!
//! Drives a resolved `rclone` binary via `tokio::process::Command`. Actual subprocess
//! invocation will be wired in T034 (Phase 3, US1). This module currently exposes the
//! struct skeleton so the reconciler can be written against the trait.

use std::path::{Path, PathBuf};

use crate::engine::{RemoteFile, SyncEngine};
use crate::error::{Error, Result};

/// Where the `rclone` binary the engine drives came from. Surfaced via
/// `air-drive status --json` under `rclone.source` so the user can audit which binary
/// is actually in use (cf. `research.md §5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcloneSource {
    /// `[rclone].path` from `config.toml`.
    Config,
    /// First `rclone` found on `$PATH`.
    Path,
    /// Cached binary at `$XDG_CACHE_HOME/air-drive/bin/rclone`.
    Cache,
    /// Downloaded by the daemon from `downloads.rclone.org`.
    Downloaded,
}

/// Resolved rclone binary handle.
#[derive(Debug, Clone)]
pub struct RcloneBinary {
    /// Absolute path to the binary.
    pub path: PathBuf,
    /// Version string as reported by `rclone version` (e.g. `"1.65.2"`).
    pub version: String,
    /// Where the binary came from.
    pub source: RcloneSource,
}

/// rclone-backed sync engine.
#[derive(Debug, Clone)]
pub struct RcloneEngine {
    binary: RcloneBinary,
}

impl RcloneEngine {
    /// Build a new engine around an already-resolved [`RcloneBinary`].
    ///
    /// Resolution (config → PATH → cache → download) is performed in
    /// `engine::rclone_path` (T033 — Phase 3 US1).
    pub fn new(binary: RcloneBinary) -> Self {
        Self { binary }
    }

    /// Borrow the resolved binary descriptor (for status output).
    pub fn binary(&self) -> &RcloneBinary {
        &self.binary
    }
}

#[async_trait::async_trait]
impl SyncEngine for RcloneEngine {
    async fn upload(
        &self,
        _local: &Path,
        _remote_parent_id: &str,
        _name: &str,
    ) -> Result<RemoteFile> {
        // T034 (Phase 3 US1) — wire `rclone copyto <local> drive:<parent>/<name>`.
        Err(Error::Rclone {
            stderr: "RcloneEngine::upload not implemented (pending T034)".into(),
        })
    }

    async fn download(&self, _remote_id: &str, _local: &Path) -> Result<()> {
        // T034 — wire `rclone copyto drive:<remote_id> <staging>` then atomic rename.
        Err(Error::Rclone {
            stderr: "RcloneEngine::download not implemented (pending T034)".into(),
        })
    }

    async fn move_remote(
        &self,
        _remote_id: &str,
        _new_parent_id: &str,
        _new_name: &str,
    ) -> Result<()> {
        // T034 — wire `rclone moveto drive:<id> drive:<parent>/<name>`.
        Err(Error::Rclone {
            stderr: "RcloneEngine::move_remote not implemented (pending T034)".into(),
        })
    }

    async fn delete_remote(&self, _remote_id: &str) -> Result<()> {
        // T034 — wire `rclone delete drive:<id>` (or REST `files.update` with trashed=true).
        Err(Error::Rclone {
            stderr: "RcloneEngine::delete_remote not implemented (pending T034)".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dummy_binary() -> RcloneBinary {
        RcloneBinary {
            path: PathBuf::from("/usr/bin/rclone"),
            version: "1.65.0".into(),
            source: RcloneSource::Path,
        }
    }

    #[test]
    fn engine_exposes_resolved_binary() {
        let engine = RcloneEngine::new(dummy_binary());
        assert_eq!(engine.binary().version, "1.65.0");
        assert_eq!(engine.binary().source, RcloneSource::Path);
    }

    #[tokio::test]
    async fn methods_return_not_implemented_for_now() {
        // Sanity: the trait can be instantiated and called. The "not implemented"
        // errors are placeholders until T034 (Phase 3 US1).
        let engine: Arc<dyn SyncEngine> = Arc::new(RcloneEngine::new(dummy_binary()));
        let err = engine
            .download("rid", Path::new("/tmp/x"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Rclone { .. }));
    }
}
