//! On-disk schema for the state DB.
//!
//! Two SQL blocks are exposed:
//!
//! - [`BOOTSTRAP`] is applied unconditionally on every open before anything else. It
//!   creates the `schema_version` table if missing — this lets `Db::open` always read
//!   `MAX(version)` without first checking that the table exists (no more fragile string
//!   matching on "no such table" errors).
//! - [`MIGRATIONS`] is the forward-only ladder. `MIGRATIONS[i]` is applied when moving
//!   from version `i` to `i + 1`. The MVP ships only `MIGRATIONS[0]` ([`V1_SCHEMA`]).
//!
//! The full table layout is documented in
//! `specs/001-minimal-sync-daemon/data-model.md`.

/// Latest schema version this binary knows how to apply.
pub const LATEST_VERSION: i64 = 2;

/// Unconditional bootstrap: ensures `schema_version` exists so the migration runner can
/// always read it. Idempotent.
pub const BOOTSTRAP: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);
"#;

/// Forward-only migrations. Index `i` is the SQL block to apply when moving from schema
/// version `i` to `i+1`. The bootstrap step above already created `schema_version` so
/// migrations only carry the **schema additions** of their version.
pub const MIGRATIONS: &[&str] = &[V1_SCHEMA, V2_SCHEMA];

const V1_SCHEMA: &str = r#"
-- linked Google Drive account (single row in MVP, id=1)
CREATE TABLE account (
    id         INTEGER PRIMARY KEY,
    email      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    linked_at  INTEGER NOT NULL
);

-- folder mapping (single row in MVP, id=1)
CREATE TABLE folder_mapping (
    id                 INTEGER PRIMARY KEY,
    account_id         INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    local_path         TEXT NOT NULL,
    remote_folder_id   TEXT NOT NULL,
    remote_folder_name TEXT,
    created_at         INTEGER NOT NULL
);

-- one row per known file or folder under the mapped subtree
CREATE TABLE sync_item (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    mapping_id     INTEGER NOT NULL REFERENCES folder_mapping(id) ON DELETE CASCADE,
    relative_path  TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK(kind IN ('file','dir')),
    remote_id      TEXT,
    size           INTEGER,
    md5            TEXT,
    local_inode    INTEGER,
    last_synced_at INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK(state IN ('synced','pending_local','pending_remote','conflict','skipped'))
);

CREATE UNIQUE INDEX sync_item_mapping_relpath
    ON sync_item(mapping_id, relative_path);

-- queued atomic operations waiting for the dispatcher
CREATE TABLE pending_operation (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    sync_item_id    INTEGER NOT NULL REFERENCES sync_item(id) ON DELETE CASCADE,
    op              TEXT NOT NULL CHECK(op IN (
                        'upload','download',
                        'delete_local','delete_remote',
                        'rename_local','rename_remote',
                        'create_dir_local','create_dir_remote',
                        'mark_conflict'
                    )),
    payload         TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    enqueued_at     INTEGER NOT NULL
);

CREATE INDEX pending_operation_next_attempt
    ON pending_operation(next_attempt_at);

-- open conflicts: one row per file modified on both sides
CREATE TABLE conflict_record (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    sync_item_id           INTEGER NOT NULL REFERENCES sync_item(id) ON DELETE CASCADE,
    original_relative_path TEXT NOT NULL,
    conflict_relative_path TEXT NOT NULL,
    detected_at            INTEGER NOT NULL
);

-- Drive `changes.list` page token (singleton, id=1)
CREATE TABLE drive_change_cursor (
    id         INTEGER PRIMARY KEY,
    mapping_id INTEGER NOT NULL REFERENCES folder_mapping(id) ON DELETE CASCADE,
    page_token TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;

/// v2 — single-row `state_meta` for surfaceable daemon state. Today we use the
/// `blocked_*` triple (FR-009, FR-020) and the `last_sync_*` triple (FR-008
/// section "last_sync"). Future state can be added as new columns without
/// breaking compat.
const V2_SCHEMA: &str = r#"
CREATE TABLE state_meta (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    blocked_kind     TEXT CHECK (blocked_kind IN ('auth', 'remote', 'mapping')),
    blocked_message  TEXT,
    blocked_at       INTEGER,
    last_sync_at     INTEGER,
    items_uploaded   INTEGER NOT NULL DEFAULT 0,
    items_downloaded INTEGER NOT NULL DEFAULT 0
);
INSERT INTO state_meta (id) VALUES (1);
"#;
