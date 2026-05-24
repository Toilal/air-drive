//! Download staging — atomic rename + orphan cleanup.
//!
//! Both [`super::http::HttpEngine`] and [`super::rclone::RcloneEngine`] stage downloads
//! into `<local_root>/.air-drive-partial/<op-id>` and only `rename(2)` them into the
//! final location once the bytes are fully written. The rename is atomic on the same
//! filesystem (POSIX guarantee), which means a crash mid-download never leaves a
//! truncated file at the destination path.
//!
//! On daemon startup, [`cleanup_orphans`] sweeps any leftovers under
//! `.air-drive-partial/` from a previous crash.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Subdirectory name (relative to the watched local root) used for partial downloads.
pub const PARTIAL_DIR: &str = ".air-drive-partial";

/// Compute a unique staging path for the given local root and op id. The directory is
/// created on the fly; callers do not need to pre-create it.
pub fn stage_path(local_root: &Path, op_id: &str) -> Result<PathBuf> {
    let dir = local_root.join(PARTIAL_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(op_id))
}

/// Atomically promote `staging` → `final_path`. Creates `final_path`'s parent directory
/// if missing. If the destination already exists, it is overwritten by `rename`'s
/// standard POSIX semantics.
pub fn promote(staging: &Path, final_path: &Path) -> Result<()> {
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(staging, final_path)?;
    Ok(())
}

/// Delete the staging file if it exists. Used to clean up on download failure so we
/// don't leak bytes into `.air-drive-partial/`.
pub fn discard(staging: &Path) -> Result<()> {
    match std::fs::remove_file(staging) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Remove every leftover entry under `<local_root>/.air-drive-partial/`. Logs a warning
/// for each cleared file via [`tracing`]. Missing partial dir is a no-op.
pub fn cleanup_orphans(local_root: &Path) -> Result<usize> {
    let dir = local_root.join(PARTIAL_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %e, "could not remove orphan partial");
                continue;
            }
            removed += 1;
            tracing::info!(path = %path.display(), "removed orphan partial download");
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_path_creates_partial_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = stage_path(tmp.path(), "op-1").unwrap();
        assert!(p.parent().unwrap().is_dir());
        assert_eq!(p.parent().unwrap().file_name().unwrap(), PARTIAL_DIR);
        assert_eq!(p.file_name().unwrap(), "op-1");
    }

    #[test]
    fn promote_atomically_moves_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = stage_path(tmp.path(), "op-2").unwrap();
        std::fs::write(&staging, b"hello").unwrap();
        let dest = tmp.path().join("sub/dir/out.bin");
        promote(&staging, &dest).unwrap();
        assert!(!staging.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn discard_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = stage_path(tmp.path(), "op-3").unwrap();
        std::fs::write(&staging, b"x").unwrap();
        discard(&staging).unwrap();
        discard(&staging).unwrap(); // already gone — must not error
    }

    #[test]
    fn cleanup_orphans_clears_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["op-1", "op-2", "op-3"] {
            let p = stage_path(tmp.path(), name).unwrap();
            std::fs::write(&p, b"leftover").unwrap();
        }
        let removed = cleanup_orphans(tmp.path()).unwrap();
        assert_eq!(removed, 3);
        let leftover = std::fs::read_dir(tmp.path().join(PARTIAL_DIR))
            .unwrap()
            .count();
        assert_eq!(leftover, 0);
    }

    #[test]
    fn cleanup_orphans_noop_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let removed = cleanup_orphans(tmp.path()).unwrap();
        assert_eq!(removed, 0);
    }
}
