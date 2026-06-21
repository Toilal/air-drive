# State schema

All sync state is persisted to a single SQLite file, `state.db`, in the config
directory (`~/.config/air-drive/state.db` by default). Nothing that matters
survives only in memory — `pageToken`, mapping, pending operations, conflicts,
and accounts are all on disk, so a crash or restart resumes cleanly.

The schema lives in `src/state/schema.rs`; typed accessors per table live in the
sibling `src/state/*.rs` files.

## Versioning and migrations

- A `schema_version` table records applied versions. A `BOOTSTRAP` block creates
  it unconditionally on every open, so the migration runner can always read
  `MAX(version)` without fragile "no such table" checks.
- `MIGRATIONS` is a **forward-only** ladder: `MIGRATIONS[i]` moves the DB from
  version `i` to `i + 1`. Each migration carries only the schema additions of
  its version.
- The current `LATEST_VERSION` is **4**.

| Version | Adds                                                                                  |
| ------- | ------------------------------------------------------------------------------------- |
| v1      | Core tables: `account`, `folder_mapping`, `sync_item`, `pending_operation`, `conflict_record`, `drive_change_cursor`. |
| v2      | `state_meta` — single-row surfaceable daemon state (blocked + last-sync counters).    |
| v3      | `folder_mapping.remote_folder_spec` — the original CLI `<remote-folder>` spec, kept so the daemon can re-resolve / recreate the remote root if it was trashed on Drive between runs. |
| v4      | `sync_item.trashed_at` (nullable epoch) — tombstone marker: a file trashed on Drive keeps its row (and `remote_id`) so a restore re-links instead of duplicating (#8). A start-up GC reclaims tombstones older than 30 days. |

To add a schema change: append a new `Vn_SCHEMA` constant, push it onto
`MIGRATIONS`, and bump `LATEST_VERSION`. Never edit a shipped migration in place.

## Tables

> The MVP is single-account / single-mapping (rows `id = 1`), but the data model
> is multi-account by design — `sync_item`, `pending_operation`, etc. all key off
> `mapping_id` / `account_id` so N accounts and mappings need no schema change.

### `account`

The linked Google Drive account.

| Column       | Type    | Notes                          |
| ------------ | ------- | ------------------------------ |
| `id`         | INTEGER | PK (single row, `id = 1` MVP). |
| `email`      | TEXT    | Account email.                 |
| `created_at` | INTEGER | Unix epoch.                    |
| `linked_at`  | INTEGER | Unix epoch.                    |

### `folder_mapping`

One local↔remote folder pair.

| Column               | Type    | Notes                                                |
| -------------------- | ------- | ---------------------------------------------------- |
| `id`                 | INTEGER | PK.                                                  |
| `account_id`         | INTEGER | FK → `account(id)` `ON DELETE CASCADE`.              |
| `local_path`         | TEXT    | Absolute watched local path.                         |
| `remote_folder_id`   | TEXT    | Authoritative Drive folder ID.                       |
| `remote_folder_name` | TEXT    | Display name (nullable).                             |
| `created_at`         | INTEGER | Unix epoch.                                          |
| `remote_folder_spec` | TEXT    | (v3) Original CLI spec, for re-resolution/recreation. |

### `sync_item`

One row per known file or folder under the mapped subtree.

| Column           | Type    | Notes                                                                           |
| ---------------- | ------- | ------------------------------------------------------------------------------- |
| `id`             | INTEGER | PK (autoincrement).                                                             |
| `mapping_id`     | INTEGER | FK → `folder_mapping(id)` `ON DELETE CASCADE`.                                   |
| `relative_path`  | TEXT    | Path relative to the root (`/`-separated).                                       |
| `kind`           | TEXT    | `file` \| `dir`.                                                                 |
| `remote_id`      | TEXT    | Drive ID (nullable).                                                             |
| `size`           | INTEGER | Bytes (nullable).                                                                |
| `md5`            | TEXT    | Content hash (nullable).                                                         |
| `local_inode`    | INTEGER | For rename detection (nullable).                                                 |
| `last_synced_at` | INTEGER | Unix epoch.                                                                      |
| `state`          | TEXT    | `synced` \| `pending_local` \| `pending_remote` \| `conflict` \| `skipped`.      |
| `trashed_at`     | INTEGER | (v4) Tombstone marker, nullable. Non-null = the file was trashed on Drive and removed locally, but the row is kept for restore de-duplication (#8). |

Unique index on `(mapping_id, relative_path)`.

### `pending_operation`

Queued atomic operations awaiting the dispatcher.

| Column            | Type    | Notes                                                                 |
| ----------------- | ------- | --------------------------------------------------------------------- |
| `id`              | INTEGER | PK (autoincrement).                                                   |
| `sync_item_id`    | INTEGER | FK → `sync_item(id)` `ON DELETE CASCADE`.                             |
| `op`              | TEXT    | `upload`, `download`, `delete_local`, `delete_remote`, `rename_local`, `rename_remote`, `create_dir_local`, `create_dir_remote`, `mark_conflict`. |
| `payload`         | TEXT    | Op-specific JSON (nullable).                                          |
| `attempts`        | INTEGER | Retry counter (default 0).                                            |
| `next_attempt_at` | INTEGER | Unix epoch; indexed for due-row scans.                               |
| `last_error`      | TEXT    | Last failure message (nullable).                                     |
| `enqueued_at`     | INTEGER | Unix epoch.                                                          |

### `conflict_record`

One row per file modified on both sides.

| Column                   | Type    | Notes                                       |
| ------------------------ | ------- | ------------------------------------------- |
| `id`                     | INTEGER | PK (autoincrement).                         |
| `sync_item_id`           | INTEGER | FK → `sync_item(id)` `ON DELETE CASCADE`.  |
| `original_relative_path` | TEXT    | The path that diverged.                     |
| `conflict_relative_path` | TEXT    | The renamed sibling preserving the loser.   |
| `detected_at`            | INTEGER | Unix epoch.                                 |

### `drive_change_cursor`

The Drive `changes.list` page token (singleton, `id = 1`).

| Column       | Type    | Notes                                          |
| ------------ | ------- | ---------------------------------------------- |
| `id`         | INTEGER | PK.                                            |
| `mapping_id` | INTEGER | FK → `folder_mapping(id)` `ON DELETE CASCADE`. |
| `page_token` | TEXT    | Current `pageToken`.                           |
| `updated_at` | INTEGER | Unix epoch.                                    |

### `state_meta`

Single-row (`id = 1`) surfaceable daemon state, read by `air-drive status`. New
state can be added as columns without breaking compat.

| Column             | Type    | Notes                                          |
| ------------------ | ------- | ---------------------------------------------- |
| `id`               | INTEGER | PK, `CHECK (id = 1)`.                           |
| `blocked_kind`     | TEXT    | `auth` \| `remote` \| `mapping` (nullable).    |
| `blocked_message`  | TEXT    | Human-readable block reason (nullable).        |
| `blocked_at`       | INTEGER | Unix epoch (nullable).                          |
| `last_sync_at`     | INTEGER | Unix epoch (nullable).                          |
| `items_uploaded`   | INTEGER | Counter (default 0).                            |
| `items_downloaded` | INTEGER | Counter (default 0).                            |
