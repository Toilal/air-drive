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
pub const LATEST_VERSION: i64 = 6;

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
pub const MIGRATIONS: &[&str] = &[
    V1_SCHEMA, V2_SCHEMA, V3_SCHEMA, V4_SCHEMA, V5_SCHEMA, V6_SCHEMA,
];

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
/// `blocked_*` triple and the `last_sync_*` triple. Future state can be added
/// as new columns without breaking compat.
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

/// v4 — tombstone support: `sync_item.trashed_at` (Unix epoch seconds, nullable).
/// A non-null value marks a row whose file was trashed on Drive and removed
/// locally, kept (with its `remote_id`) so a restore re-links to the original path
/// instead of duplicating. A retention GC reclaims old tombstones (issue #8).
const V4_SCHEMA: &str = r#"
ALTER TABLE sync_item ADD COLUMN trashed_at INTEGER;
"#;

/// v5 — add the `write_shortcut` operation. Native Google Docs are now materialised
/// as local shortcut files (issue #3); the dispatcher needs a dedicated op to write
/// them. SQLite cannot alter a CHECK constraint in place, so `pending_operation` is
/// rebuilt with the extended `op` set. Nothing foreign-key-references the table, so
/// the drop/rename is safe inside the migration transaction. Existing queued rows are
/// copied across verbatim and the due-row index is recreated.
const V5_SCHEMA: &str = r#"
CREATE TABLE pending_operation_v5 (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    sync_item_id    INTEGER NOT NULL REFERENCES sync_item(id) ON DELETE CASCADE,
    op              TEXT NOT NULL CHECK(op IN (
                        'upload','download',
                        'delete_local','delete_remote',
                        'rename_local','rename_remote',
                        'create_dir_local','create_dir_remote',
                        'write_shortcut',
                        'mark_conflict'
                    )),
    payload         TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    enqueued_at     INTEGER NOT NULL
);

INSERT INTO pending_operation_v5
    (id, sync_item_id, op, payload, attempts, next_attempt_at, last_error, enqueued_at)
SELECT id, sync_item_id, op, payload, attempts, next_attempt_at, last_error, enqueued_at
FROM pending_operation;

DROP TABLE pending_operation;
ALTER TABLE pending_operation_v5 RENAME TO pending_operation;

CREATE INDEX pending_operation_next_attempt
    ON pending_operation(next_attempt_at);
"#;

/// v6 — extend `state_meta.blocked_kind` with the recoverable `transient` kind
/// (Drive briefly unreachable; cleared on the next successful call). SQLite can't
/// alter a CHECK constraint in place, so the singleton table is rebuilt with the
/// widened set and its one row copied across. Nothing foreign-key-references it.
const V6_SCHEMA: &str = r#"
CREATE TABLE state_meta_v6 (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    blocked_kind     TEXT CHECK (blocked_kind IN ('auth', 'remote', 'mapping', 'transient')),
    blocked_message  TEXT,
    blocked_at       INTEGER,
    last_sync_at     INTEGER,
    items_uploaded   INTEGER NOT NULL DEFAULT 0,
    items_downloaded INTEGER NOT NULL DEFAULT 0
);

INSERT INTO state_meta_v6
    (id, blocked_kind, blocked_message, blocked_at, last_sync_at, items_uploaded, items_downloaded)
SELECT id, blocked_kind, blocked_message, blocked_at, last_sync_at, items_uploaded, items_downloaded
FROM state_meta;

DROP TABLE state_meta;
ALTER TABLE state_meta_v6 RENAME TO state_meta;
"#;

/// v3 — persist the original `<remote-folder>` CLI spec on the mapping row.
/// The daemon needs it at startup to re-resolve (and optionally recreate) the
/// remote root if the user trashed it on Drive between two runs. Without the
/// spec, the stored Drive ID becomes useless for recreation since we cannot
/// guess the parent + name from an ID alone.
const V3_SCHEMA: &str = r#"
ALTER TABLE folder_mapping ADD COLUMN remote_folder_spec TEXT;
"#;
