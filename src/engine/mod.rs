//! Pluggable sync engine: the [`SyncEngine`] trait and its initial implementation.
//!
//! The trait is the only door the reconciler walks through when it needs to talk to the
//! remote side. Its **steady-state** surface is deliberately **atomic and per-file**
//! (upload, download, move, delete): that granularity is what gives us the event-driven
//! loop required by constitution principle II and the freedom to swap the rclone-backed
//! implementation for a native Rust engine later (constitution principle IV). The
//! continuous-sync loop MUST only ever use these per-file operations.
//!
//! The single exception is **bootstrap**: [`SyncEngine::bulk_download`] and
//! [`SyncEngine::bulk_upload`] move a pre-computed *set* of files in one batched
//! transfer, used **only** by the one-shot initial reconciliation
//! ([`crate::reconcile::initial`]) where O(files) per-file process spawns are the
//! dominant cost. They are not a "bisync the tree" call — the reconciler still owns all
//! policy (what to sync, ignore patterns, conflicts, native-Doc shortcuts, state
//! population); the engine only moves the bytes for the exact relative paths it is
//! handed. `RcloneEngine` implements them as a single `rclone copy --files-from`;
//! `HttpEngine` as a per-file loop over the same list.
//!
//! Implementations: [`rclone::RcloneEngine`] (production) and [`http::HttpEngine`] (the
//! in-process engine the integration suite drives).

pub mod http;
pub mod rclone;
pub mod rclone_download;
pub mod rclone_path;
pub mod staging;

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
    /// are skipped before they ever reach the engine.
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
    /// and atomically rename into place, enforced by `RcloneEngine`).
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

/// One file to fetch in a bootstrap [`SyncEngine::bulk_download`]. Carries both
/// the Drive id (so an id-addressed engine like [`http::HttpEngine`] can fetch
/// directly) and the path relative to the remote root (so a path-addressed
/// engine like [`rclone::RcloneEngine`] can list it in a `--files-from`); each
/// engine uses whichever field fits. Resolved by the reconciler from its remote
/// walk, so no engine has to re-walk the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkDownload {
    /// Drive file ID.
    pub remote_id: String,
    /// Destination path relative to the local root (and source path relative to
    /// the remote root — they mirror each other).
    pub rel_path: String,
}

/// One file to push in a bootstrap [`SyncEngine::bulk_upload`]. Carries the
/// already-resolved Drive parent-folder id + name (for an id-addressed engine)
/// and the path relative to the local root (for a path-addressed engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkUpload {
    /// Source path relative to the local root (mirrors the remote path).
    pub rel_path: String,
    /// Drive ID of the destination parent folder (already created by the
    /// reconciler's directory pass).
    pub remote_parent_id: String,
    /// File name on Drive (last segment of `rel_path`).
    pub name: String,
}

/// The pluggable sync engine. Implementations live under [`mod@self`].
#[async_trait::async_trait]
pub trait SyncEngine: Send + Sync + 'static {
    /// Upload a local file to Drive.
    async fn upload(&self, local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile>;

    /// Replace the content of an existing remote file in place. Preserves the Drive
    /// `remote_id` (Drive comments, sharing settings, etc. survive). Used by the
    /// continuous reconciler when a local `Modified` event fires for a file the
    /// daemon already knows.
    async fn update(&self, remote_id: &str, local: &Path) -> Result<RemoteFile>;

    /// Fetch a Drive file to a local path. The implementation MUST stage the bytes
    /// under `<local_root>/.air-drive-partial/<op-id>` and atomically rename into
    /// `local` only after the bytes are fully written. `local_root`
    /// is the watched folder root — passing it explicitly means staged downloads of
    /// nested files (`dir/sub/file.txt`) still land in the single root-level
    /// `.air-drive-partial/` rather than scattered through the tree where the
    /// orphan-sweep can't find them on the next start-up.
    async fn download(&self, remote_id: &str, local: &Path, local_root: &Path) -> Result<()>;

    /// Move and/or rename a remote file (no re-upload on rename).
    async fn move_remote(&self, remote_id: &str, new_parent_id: &str, new_name: &str)
    -> Result<()>;

    /// Delete (trash) a remote file.
    async fn delete_remote(&self, remote_id: &str) -> Result<()>;

    /// Create an empty folder on Drive under `remote_parent_id` and return its
    /// metadata (the `size`/`md5` of the returned [`RemoteFile`] are not
    /// meaningful for a folder). Used to propagate empty directories and to
    /// anchor folder renames/moves.
    async fn create_dir_remote(&self, remote_parent_id: &str, name: &str) -> Result<RemoteFile>;

    /// Remove a remote folder (Drive trash) by id. Callers MUST delete a
    /// folder's children first; this only removes the (expected-empty) folder.
    async fn remove_dir_remote(&self, remote_id: &str) -> Result<()>;

    /// **Bootstrap-only.** Download `items` into `local_root`, recreating
    /// intermediate directories as needed. `remote_root_id` scopes the remote
    /// for path-addressed engines. Native Google Docs are excluded by the caller
    /// and MUST NOT be fetched here. An empty `items` is a no-op.
    ///
    /// This is a batched accelerator for [`crate::reconcile::initial`], never
    /// used by the continuous loop. Implementations SHOULD move the whole set in
    /// as few round-trips as possible and report progress to the `rclone`/engine
    /// tracing target.
    async fn bulk_download(
        &self,
        items: &[BulkDownload],
        remote_root_id: &str,
        local_root: &Path,
    ) -> Result<()>;

    /// **Bootstrap-only.** Upload `items` (paths relative to `local_root`) under
    /// `remote_root_id`, creating intermediate remote folders as needed. An empty
    /// `items` is a no-op.
    ///
    /// Counterpart of [`Self::bulk_download`]; same constraints and intent.
    async fn bulk_upload(
        &self,
        items: &[BulkUpload],
        local_root: &Path,
        remote_root_id: &str,
    ) -> Result<()>;
}
