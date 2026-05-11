# Data Model — Phase 1

The MVP persists state in three places, each chosen for its constraints:

1. **SQLite** (`$XDG_CONFIG_HOME/air-drive/state.db`) — the change ledger, sync items,
   conflicts, the pending-operation queue, the Drive `pageToken`, and the schema version.
2. **TOML config file** (`$XDG_CONFIG_HOME/air-drive/config.toml`) — user-editable settings
   (account label, folder mapping, OAuth client override, log file path).
3. **JSON token file** (`$XDG_CONFIG_HOME/air-drive/tokens.json`, mode `0600`) — OAuth
   refresh/access tokens managed by `yup-oauth2`.

The config and tokens are split because `yup-oauth2` owns the tokens file's format and
rotation, while the config is human-edited.

## Entities

### Account

Represents the linked Google Drive account. Exactly one row in the MVP (FR-001).

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK | Always `1` in this MVP. Future: per-account row. |
| `email` | TEXT NOT NULL | Captured from Drive's `about.user.emailAddress` after first auth. Display only. |
| `created_at` | INTEGER NOT NULL | Unix epoch seconds. |
| `linked_at` | INTEGER NOT NULL | Last successful OAuth consent. Touched on relink. |

OAuth tokens are NOT stored here — they live in `tokens.json` (managed by `yup-oauth2`).

### Folder Mapping

The local↔remote pair to keep in sync. Exactly one row in the MVP (FR-002).

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK | Always `1` in this MVP. |
| `account_id` | INTEGER NOT NULL | FK → `account.id`. |
| `local_path` | TEXT NOT NULL | Absolute, canonicalised. |
| `remote_folder_id` | TEXT NOT NULL | Drive file ID (immune to renames). |
| `remote_folder_name` | TEXT | Cached display name; rehydrated on each `status`. |
| `created_at` | INTEGER NOT NULL | Unix epoch seconds. |

### Sync Item

A logical file or folder currently tracked. One row per item; the row IS the source of truth
for "what was synced last time".

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK AUTOINCREMENT | |
| `mapping_id` | INTEGER NOT NULL | FK → `folder_mapping.id`. |
| `relative_path` | TEXT NOT NULL | POSIX-style path relative to `local_path` (and to `remote_folder_id`). |
| `kind` | TEXT NOT NULL CHECK(kind IN ('file','dir')) | |
| `remote_id` | TEXT | Drive file ID. NULL during the brief window where a local create is queued for upload. |
| `size` | INTEGER | Bytes for files; NULL for dirs. |
| `md5` | TEXT | Hex MD5 for files; NULL for dirs and for items with no Drive MD5 (skipped — see FR-011). |
| `local_inode` | INTEGER | For optimistic cache lookups; not authoritative. |
| `last_synced_at` | INTEGER NOT NULL | Unix epoch seconds of last successful sync. |
| `state` | TEXT NOT NULL CHECK(state IN ('synced','pending_local','pending_remote','conflict','skipped')) | See state diagram. |

Unique index on `(mapping_id, relative_path)`.

### Pending Operation

A queued atomic op to be executed by the `SyncEngine`. Drained in submission order with
per-item serialisation (we never have two ops in flight for the same `sync_item_id`).

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK AUTOINCREMENT | |
| `sync_item_id` | INTEGER NOT NULL | FK → `sync_item.id`. |
| `op` | TEXT NOT NULL CHECK(op IN ('upload','download','delete_local','delete_remote','rename_local','rename_remote','create_dir_local','create_dir_remote','mark_conflict')) | |
| `payload` | TEXT | Op-specific JSON: e.g. `{"new_relative_path": "…"}` for renames. |
| `attempts` | INTEGER NOT NULL DEFAULT 0 | |
| `next_attempt_at` | INTEGER NOT NULL | Unix epoch seconds. Set by the backoff layer when retrying. |
| `last_error` | TEXT | One-line error message for the most recent failure (status surfaces it). |
| `enqueued_at` | INTEGER NOT NULL | Unix epoch seconds. |

Index on `(next_attempt_at)` for the dispatch worker.

### Conflict Record

Surfaces an unresolved conflict (FR-006). One row per conflict event; lives until the user
resolves manually (no automatic cleanup in this feature).

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK AUTOINCREMENT | |
| `sync_item_id` | INTEGER NOT NULL | FK → `sync_item.id` of the file kept under the canonical name (the remote version, per the Q2 clarification). |
| `original_relative_path` | TEXT NOT NULL | The canonical name, for status output. |
| `conflict_relative_path` | TEXT NOT NULL | The `.conflict-<UTC-ts>.<ext>` name where the local version was preserved. |
| `detected_at` | INTEGER NOT NULL | Unix epoch seconds. |

### Drive Change Cursor

Tracks the `pageToken` we hand to `changes.list` next. Singleton row.

| Field | Type | Notes |
|---|---|---|
| `id` | INTEGER PK | Always `1`. |
| `mapping_id` | INTEGER NOT NULL | FK → `folder_mapping.id`. |
| `page_token` | TEXT NOT NULL | Opaque value from Drive. |
| `updated_at` | INTEGER NOT NULL | Unix epoch seconds. |

### Schema Version

| Field | Type | Notes |
|---|---|---|
| `version` | INTEGER PK | Single row, set to `1` for this feature. Forward-only migrations increment this. |
| `applied_at` | INTEGER NOT NULL | |

## State diagram for a Sync Item

```text
                          (local create)              (remote create)
                          ┌────────────────┐         ┌────────────────┐
                          ▼                │         ▼                │
        ┌──────────┐  upload   ┌──────────────┐  download   ┌──────────────┐
        │ pending  │ ─────────▶│   synced     │◀──────────  │   pending    │
        │  local   │           │              │             │   remote     │
        └──────────┘           └──────┬───────┘             └──────┬───────┘
              ▲                       │                            ▲
              │       (local edit)    │      (remote edit)         │
              └───────────────────────┴────────────────────────────┘
                                      │
                                      │  (edited on both sides since last sync)
                                      ▼
                              ┌──────────────┐
                              │  conflict    │  ──────▶  user resolves manually
                              └──────────────┘
                                      │
                              ┌──────────────┐
                              │  skipped     │  (Google-native doc, symlink, etc.)
                              └──────────────┘
```

Transitions are written in a single SQL transaction with the corresponding `PendingOperation`
insert, so a crash mid-op cannot leave the item in an inconsistent state.

## Config file format (`config.toml`)

```toml
# OAuth configuration (optional override of the embedded defaults — Q1 clarification)
[oauth]
# Override the project-owned OAuth client_id with your own Google Cloud project.
# Remove this section entirely to use the embedded default.
# client_id = "your-own.apps.googleusercontent.com"

[mapping]
local_path = "/home/alice/Drive"
remote_folder_name = "alice@gmail.com / My Drive / Sync"   # display only
# remote_folder_id is stored in the SQLite DB, NOT here

[daemon]
remote_poll_interval_seconds = 30        # 10..60, default 30
safety_net_interval_seconds = 300        # default 300 (5 min)
log_file = ""                            # empty → stderr only
```

`mapping.remote_folder_id` is intentionally NOT in this file — it's an opaque ID with no
value to the user; it lives in `state.db` alongside the cached `remote_folder_name`.

## Token file format (`tokens.json`, mode `0600`)

Owned by `yup-oauth2` — schema is the crate's `DiskTokenStorage` format. We do not specify
it here; we only specify that the file MUST be created with `0600` permissions and that the
daemon refuses to start if the permissions are looser.
