//! Pluggable sync engine: the [`SyncEngine`] trait and its initial implementation.
//!
//! The trait is the only door the reconciler walks through when it needs to talk to the
//! remote side. It exposes **atomic, per-file** operations (upload, download, move,
//! delete) — never a high-level "bisync the tree" call. That granularity is what gives
//! us the event-driven loop required by constitution principle II and the freedom to
//! swap the rclone-backed implementation for a native Rust engine later (constitution
//! principle IV).
//!
//! The MVP ships exactly one implementation: [`rclone::RcloneEngine`].

pub mod rclone;

use std::path::Path;

use crate::error::Result;

/// Metadata returned by the remote side after a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// Drive file ID assigned by the remote.
    pub id: String,
    /// MIME type as reported by Drive. Set to a sentinel for non-Drive engines that
    /// don't know it.
    pub mime: String,
    /// Size in bytes.
    pub size: i64,
    /// Hex MD5 if the remote exposes it. Native Google Docs return `None` here, and
    /// per FR-011 those are skipped before they ever reach the engine.
    pub md5: Option<String>,
}

/// Atomic, side-effectful operations the reconciler asks the engine to perform.
///
/// Each variant carries every piece of data the engine needs to act without consulting
/// the state DB. Payloads serialise to JSON in `pending_operation.payload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Push a local file at `local` into Drive under `remote_parent_id` as `name`.
    Upload {
        /// Absolute path on the local filesystem.
        local: std::path::PathBuf,
        /// Drive ID of the destination folder.
        remote_parent_id: String,
        /// File name on Drive (last segment of the relative path).
        name: String,
    },
    /// Fetch a remote file to `local` (the engine must stage to a temporary location
    /// and atomically rename into place — FR-010, enforced by `RcloneEngine`).
    Download {
        /// Drive file ID to fetch.
        remote_id: String,
        /// Final on-disk destination.
        local: std::path::PathBuf,
    },
    /// Move and/or rename a remote file: change its parent folder and/or its name.
    MoveRemote {
        /// Drive file ID to move.
        remote_id: String,
        /// New parent folder ID. Same as the current one when only renaming.
        new_parent_id: String,
        /// New display name.
        new_name: String,
    },
    /// Delete a remote file (Drive trash, not permanent).
    DeleteRemote {
        /// Drive file ID to delete.
        remote_id: String,
    },
}

/// The pluggable sync engine. Implementations live under [`mod@self`].
#[async_trait::async_trait]
pub trait SyncEngine: Send + Sync + 'static {
    /// Upload a local file to Drive.
    async fn upload(&self, local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile>;

    /// Fetch a Drive file to a local path. The implementation MUST stage the bytes
    /// somewhere temporary and atomically rename into `local` only after verification
    /// (FR-010).
    async fn download(&self, remote_id: &str, local: &Path) -> Result<()>;

    /// Move and/or rename a remote file. Used for FR-005 (no re-upload on rename).
    async fn move_remote(&self, remote_id: &str, new_parent_id: &str, new_name: &str)
    -> Result<()>;

    /// Delete (trash) a remote file.
    async fn delete_remote(&self, remote_id: &str) -> Result<()>;
}
