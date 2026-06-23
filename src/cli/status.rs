//! `air-drive status [--json]`.
//!
//! Reads the on-disk state DB and renders a snapshot in either human-readable
//! form (default) or as a JSON document matching `contracts/status.schema.json`.
//!
//! Liveness detection: we try to acquire the single-instance lock
//! non-blockingly. If [`crate::daemon::lock::Lock::acquire`] returns
//! [`crate::error::Error::Lock`] the daemon is alive and the lock file
//! carries its PID. If the acquire SUCCEEDS we release immediately (the guard
//! drops at end of scope) — no daemon is running.
//!
//! "syncing" vs "idle" comes from the `pending_operation` queue: any pending
//! op flips the state to syncing. The richer states (`paused`, `blocked`,
//! `error`) require the daemon to signal them via the control socket — for
//! now we surface `idle`/`syncing` only.

use std::path::Path;

use serde_json::{Value, json};

use crate::cli::{ExitCode, runtime};
use crate::daemon::lock::Lock;
use crate::error::{Error, Result};
use crate::state::Db;
use crate::state::meta::{self, BlockedKind};
use crate::state::ops::Operation;
use crate::state::{accounts, conflicts, items, mapping, ops};

/// `air-drive status [--json]` entry point.
pub async fn run(config_dir_override: Option<&Path>, as_json: bool) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;

    // Probe liveness: try to acquire; if it fails with Lock { pid } the daemon
    // is alive on that pid. Release immediately when our guard drops.
    let pid = match Lock::acquire(paths.config()) {
        Ok(_guard) => None,
        Err(Error::Lock { pid }) => pid,
        Err(e) => return Err(e),
    };

    let db = runtime::open_state(&paths).await?;
    let snapshot = collect(&db, pid).await?;

    if as_json {
        let s = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| Error::Config(format!("status JSON serialise: {e}")))?;
        println!("{s}");
    } else {
        print_human(&snapshot);
    }
    Ok(ExitCode::Ok)
}

/// Assemble the status snapshot from the state DB. Public so the integration
/// tests can call it without spawning the binary.
pub async fn collect(db: &Db, pid: Option<u32>) -> Result<Value> {
    let account = accounts::get_single(db.connection()).await?;
    let mapping_row = mapping::get_single(db.connection()).await?;
    let op_counts = ops::count_by_op(db.connection()).await?;
    let conflict_rows = conflicts::list_unresolved(db.connection()).await?;

    let blocked = meta::get_blocked(db.connection()).await?;
    let (last_sync_at, items_uploaded, items_downloaded) = meta::last_sync(db.connection()).await?;

    // Native Google Docs are represented as local shortcut files and tracked as
    // `skipped` items (issue #3). Surface them so they are visible, not invisible.
    let skipped_items = match &mapping_row {
        Some(m) => items::list_skipped(db.connection(), m.id).await?,
        None => Vec::new(),
    };

    let total_pending: i64 = op_counts.values().sum();
    let state = if blocked.is_some() {
        "blocked"
    } else if pid.is_some() && total_pending > 0 {
        "syncing"
    } else if pid.is_some() {
        "idle"
    } else {
        // No daemon running. Surface "idle" — the user can re-start to act on
        // any backlog. We don't go through "error" until the control-socket
        // path lands and we have a way to surface persistent failures.
        "idle"
    };

    let account_json = match account {
        Some(a) => json!({
            "email": a.email,
            "linked_at": a.linked_at,
        }),
        None => Value::Null,
    };
    let mapping_json = match mapping_row {
        Some(m) => {
            let mut obj = serde_json::Map::new();
            obj.insert("local_path".into(), Value::String(m.local_path));
            obj.insert(
                "remote_folder_name".into(),
                Value::String(
                    m.remote_folder_name
                        .unwrap_or_else(|| m.remote_folder_id.clone()),
                ),
            );
            obj.insert("remote_folder_id".into(), Value::String(m.remote_folder_id));
            Value::Object(obj)
        }
        None => Value::Null,
    };

    let pending = json!({
        "uploads": op_counts.get(&Operation::Upload).copied().unwrap_or(0),
        "downloads": op_counts.get(&Operation::Download).copied().unwrap_or(0),
        "renames": op_counts.get(&Operation::RenameRemote).copied().unwrap_or(0)
            + op_counts.get(&Operation::RenameLocal).copied().unwrap_or(0),
        "deletes": op_counts.get(&Operation::DeleteRemote).copied().unwrap_or(0)
            + op_counts.get(&Operation::DeleteLocal).copied().unwrap_or(0),
    });

    let conflicts_json: Vec<Value> = conflict_rows
        .into_iter()
        .map(|c| {
            json!({
                "original_path": c.original_relative_path,
                "conflict_path": c.conflict_relative_path,
                "detected_at": c.detected_at,
            })
        })
        .collect();

    let mut snapshot = serde_json::Map::new();
    snapshot.insert("schema_version".into(), json!(1));
    snapshot.insert("state".into(), json!(state));
    snapshot.insert("account".into(), account_json);
    snapshot.insert("mapping".into(), mapping_json);
    snapshot.insert("pending".into(), pending);
    snapshot.insert(
        "last_sync".into(),
        match last_sync_at {
            Some(at) => json!({
                "timestamp": at,
                "items_uploaded": items_uploaded,
                "items_downloaded": items_downloaded,
            }),
            None => Value::Null,
        },
    );
    snapshot.insert(
        "last_error".into(),
        match &blocked {
            Some(b) => json!({
                "message": b.message,
                "at": b.at,
                "kind": match b.kind {
                    BlockedKind::Auth => "auth",
                    BlockedKind::Remote => "remote",
                    BlockedKind::Mapping => "mapping",
                    BlockedKind::Transient => "transient",
                },
            }),
            None => Value::Null,
        },
    );
    snapshot.insert("conflicts".into(), Value::Array(conflicts_json));

    // Skipped items — native Google Docs surfaced as local shortcut files (issue #3).
    let skipped_paths: Vec<Value> = skipped_items
        .iter()
        .map(|i| Value::String(i.relative_path.clone()))
        .collect();
    snapshot.insert(
        "skipped".into(),
        json!({
            "count": skipped_items.len(),
            "paths": skipped_paths,
        }),
    );

    snapshot.insert("pid".into(), pid.map(|p| json!(p)).unwrap_or(Value::Null));
    snapshot.insert("rclone".into(), Value::Null);

    Ok(Value::Object(snapshot))
}

/// Pretty-print the snapshot as ASCII tables of one column. Kept deliberately
/// terse — the JSON form is the source of truth.
fn print_human(s: &Value) {
    println!("state          : {}", s["state"].as_str().unwrap_or("?"));
    match &s["account"] {
        Value::Object(o) => println!(
            "account        : {} (linked {})",
            o["email"].as_str().unwrap_or("?"),
            o.get("linked_at")
                .and_then(|v| v.as_i64())
                .map(format_unix)
                .unwrap_or_else(|| "?".into())
        ),
        _ => println!("account        : (none — run `air-drive link`)"),
    }
    match &s["mapping"] {
        Value::Object(o) => println!(
            "mapping        : {} ↔ {}",
            o["local_path"].as_str().unwrap_or("?"),
            o["remote_folder_name"].as_str().unwrap_or("?")
        ),
        _ => println!("mapping        : (none — run `air-drive map`)"),
    }
    let p = &s["pending"];
    println!(
        "pending        : {} upload, {} download, {} rename, {} delete",
        p["uploads"].as_i64().unwrap_or(0),
        p["downloads"].as_i64().unwrap_or(0),
        p["renames"].as_i64().unwrap_or(0),
        p["deletes"].as_i64().unwrap_or(0),
    );
    let conflicts = s["conflicts"].as_array().map(|a| a.len()).unwrap_or(0);
    if conflicts > 0 {
        println!("conflicts      : {conflicts} unresolved");
        for c in s["conflicts"].as_array().unwrap_or(&Vec::new()) {
            println!(
                "  - {} → {}",
                c["original_path"].as_str().unwrap_or("?"),
                c["conflict_path"].as_str().unwrap_or("?"),
            );
        }
    }
    let skipped = s["skipped"]["count"].as_i64().unwrap_or(0);
    if skipped > 0 {
        println!("skipped        : {skipped} native Google Docs (as shortcut files)");
        for p in s["skipped"]["paths"].as_array().unwrap_or(&Vec::new()) {
            println!("  - {}", p.as_str().unwrap_or("?"));
        }
    }
    match s["pid"].as_i64() {
        Some(p) => println!("daemon         : running (pid {p})"),
        None => println!("daemon         : not running"),
    }
}

fn format_unix(t: i64) -> String {
    // No chrono dep; just print the seconds. The JSON output carries the
    // unix epoch verbatim — pretty formatting is a follow-up.
    format!("@{t}")
}
