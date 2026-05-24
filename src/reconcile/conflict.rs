//! Conflict detection + resolution.
//!
//! ## When a conflict fires
//!
//! The continuous reconciler sees a remote change with a new md5. If our
//! `sync_item` recorded fingerprint matches the new remote md5, the change is
//! an echo of our own write — skipped. If it matches the local file's current
//! md5, it's a pure remote-side edit — straight Download. The third case is
//! a conflict: both sides drifted from the last-synced fingerprint
//! independently. Per Q2 of the clarification round the remote version keeps
//! the canonical name; the local version is renamed to
//! `<stem>.conflict-YYYYMMDDTHHMMSSZ.<ext>` and a [`conflict_record`] row
//! is inserted. Both files then flow through the normal pipelines (Download
//! for the canonical, Upload for the renamed local).
//!
//! ## Resolution
//!
//! The user resolves a conflict by deleting one of the two files (or by
//! merging into one and deleting the other). [`apply_local_cleanup`] is
//! called from the watcher path on `Deleted` events: if the deleted path
//! matches either side of an open `conflict_record`, the row is removed
//! and the file's sync proceeds normally.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::state::Db;
use crate::state::conflicts;
use crate::state::items::ItemId;

/// Compute the conflict-renamed sibling for a local path:
/// `doc.txt` → `doc.conflict-20260518T093000Z.txt`. Extension-less files get
/// the suffix appended directly (`README` → `README.conflict-...`).
pub fn conflict_path_for(canonical: &Path, unix_now: i64) -> PathBuf {
    let parent = canonical.parent().unwrap_or_else(|| Path::new(""));
    let file_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let ts = format_iso_z(unix_now);
    let new_name = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.conflict-{ts}.{ext}"),
        _ => format!("{file_name}.conflict-{ts}"),
    };
    parent.join(new_name)
}

/// Rename the canonical local file to its conflict path and record the pair
/// in `conflict_record`. Returns the conflict path so the caller can also
/// update the corresponding `sync_item` if needed.
///
/// Idempotent on the filesystem side: if the rename fails because the
/// canonical doesn't exist (e.g. raced with another delete), we still insert
/// the record — the user will see the conflict in `status` and can decide.
pub async fn open_conflict(
    db: &Db,
    item_id: ItemId,
    canonical_local: &Path,
    canonical_relative: &str,
    local_root: &Path,
    unix_now: i64,
) -> Result<PathBuf> {
    let conflict_local = conflict_path_for(canonical_local, unix_now);
    if canonical_local.exists() {
        std::fs::rename(canonical_local, &conflict_local).map_err(|e| {
            Error::Mapping(format!(
                "rename {} → {}: {e}",
                canonical_local.display(),
                conflict_local.display()
            ))
        })?;
    } else {
        tracing::warn!(
            path = %canonical_local.display(),
            "conflict raised but local file already gone — recording anyway"
        );
    }
    let conflict_relative = strip_root(&conflict_local, local_root).unwrap_or_else(|_| {
        conflict_local
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("conflict")
            .to_owned()
    });
    conflicts::insert(
        db.connection(),
        item_id,
        canonical_relative,
        &conflict_relative,
        unix_now,
    )
    .await?;
    tracing::warn!(
        canonical = canonical_relative,
        conflict = conflict_relative.as_str(),
        "opened conflict — both sides will sync, user resolves by deleting one"
    );
    Ok(conflict_local)
}

/// Watcher-driven cleanup: when either side of a `conflict_record` is deleted
/// locally, remove the record. Called from `apply_local` on Deleted events.
/// Silent no-op when the path isn't part of any open conflict.
pub async fn cleanup_on_local_delete(db: &Db, relative_path: &str) -> Result<()> {
    let rows = conflicts::list_unresolved(db.connection()).await?;
    for row in rows {
        if row.original_relative_path == relative_path
            || row.conflict_relative_path == relative_path
        {
            conflicts::delete(db.connection(), row.id).await?;
            tracing::info!(
                cleared = relative_path,
                conflict_id = row.id.0,
                "conflict resolved — user deleted one side"
            );
        }
    }
    Ok(())
}

fn strip_root(absolute: &Path, root: &Path) -> Result<String> {
    let rel = absolute
        .strip_prefix(root)
        .map_err(|e| Error::Mapping(format!("strip_prefix: {e}")))?;
    Ok(rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/"))
}

// ---------------------------------------------------------------------------
// `YYYYMMDDTHHMMSSZ` formatting — no chrono dep
// ---------------------------------------------------------------------------

/// Render a Unix epoch second as `YYYYMMDDTHHMMSSZ`. Pure integer math via
/// Howard Hinnant's days-from-civil inversion.
pub fn format_iso_z(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400) as u32;
    let h = secs_of_day / 3600;
    let m = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z")
}

/// Howard Hinnant's `civil_from_days` — for a count of days since 1970-01-01
/// returns `(year, month, day)` with month in `1..=12`, day in `1..=31`.
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1_715_000_000 is 2024-05-06T12:53:20Z (verified against the format_iso_z
    // round-trip test below — see the leap-day case).
    const TS_FIXED: i64 = 1_715_000_000;
    const TS_STR: &str = "20240506T125320Z";

    #[test]
    fn conflict_path_keeps_extension() {
        let canonical = Path::new("/x/y/doc.txt");
        let got = conflict_path_for(canonical, TS_FIXED);
        assert_eq!(
            got.to_string_lossy(),
            format!("/x/y/doc.conflict-{TS_STR}.txt")
        );
    }

    #[test]
    fn conflict_path_handles_no_extension() {
        let canonical = Path::new("/x/y/README");
        let got = conflict_path_for(canonical, TS_FIXED);
        assert_eq!(
            got.to_string_lossy(),
            format!("/x/y/README.conflict-{TS_STR}")
        );
    }

    #[test]
    fn conflict_path_handles_dotfile() {
        // A leading-dot file like `.env` has no real "extension" — we treat
        // it as a name + dotted-suffix.
        let canonical = Path::new("/x/.env");
        let got = conflict_path_for(canonical, TS_FIXED);
        assert_eq!(got.to_string_lossy(), format!("/x/.env.conflict-{TS_STR}"));
    }

    #[test]
    fn format_iso_z_round_trips_known_values() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_iso_z(0), "19700101T000000Z");
        // 2024-02-29T00:00:00Z — leap day sanity.
        let leap = 1_709_164_800;
        assert_eq!(format_iso_z(leap), "20240229T000000Z");
        // One second before epoch — negative seconds.
        assert_eq!(format_iso_z(-1), "19691231T235959Z");
    }
}
