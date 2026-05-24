//! Automatic, lossless migration of `config.toml` on startup.
//!
//! When the daemon starts, [`migrate_on_disk`] reads the user's `config.toml`
//! and inserts any field present in the current [`Config`] schema but absent
//! from the file. Insertions carry a leading `# ...` comment so the user sees
//! what the option does without having to consult the changelog.
//!
//! What this step guarantees:
//!
//! - User-written comments, blank lines, key order, and formatting are
//!   preserved verbatim (we drive `toml_edit::DocumentMut`, not a re-render).
//! - User-modified values are never overwritten.
//! - Writes are atomic (write-to-temp + same-dir rename) so a crash mid-write
//!   cannot truncate the file.
//! - A no-op migration touches no bytes on disk.
//!
//! Out of scope: renames (need explicit per-version steps), removed fields
//! (left in place; [`Config::load`] would currently reject them, but that is
//! a separate decision tracked in the issue).

use std::path::Path;

use toml_edit::{DocumentMut, Item, Key, Table};

use crate::config::Config;
use crate::error::{Error, Result};

/// Human-readable description for every `(section, key)` exposed in the
/// schema. Used as a leading `# comment` line when a missing field is
/// inserted by [`migrate_on_disk`].
///
/// Keep this table in sync with the doc-comments in `config::mod`. The list
/// is small enough that a flat table is fine; if the field count explodes
/// later, parsing doc-comments at build time would be the next step.
const FIELD_DESCRIPTIONS: &[(&str, &str, &str)] = &[
    (
        "daemon",
        "remote_poll_interval_seconds",
        "Interval at which the daemon polls Drive `changes.list`, in seconds. Clamped to [10, 60].",
    ),
    (
        "daemon",
        "safety_net_interval_seconds",
        "Interval of the safety-net reconciliation cycle, in seconds. Must stay >= 300 (5 min).",
    ),
    (
        "daemon",
        "log_file",
        "Optional log file path; empty string disables file logging (stderr only).",
    ),
    (
        "watch",
        "ignore_patterns",
        "Glob patterns matched against the file name. Files whose name matches any pattern are never synced.",
    ),
    (
        "watch",
        "auto_create_root",
        "When true, the watched folder is created without prompting if missing. When false (default), the CLI prompts the user interactively on a TTY, or fails conservatively otherwise.",
    ),
    (
        "oauth",
        "client_id",
        "Override the embedded OAuth client_id with your own Google Cloud Desktop client.",
    ),
    (
        "oauth",
        "client_secret",
        "Companion client_secret for the Desktop OAuth client. Set together with client_id.",
    ),
    (
        "mapping",
        "local_path",
        "Absolute path of the watched local folder (display only; authoritative value lives in state.db).",
    ),
    (
        "mapping",
        "remote_folder_name",
        "Human-readable remote folder path (display only; authoritative value lives in state.db).",
    ),
    (
        "mapping",
        "auto_create_remote_root",
        "When true, `air-drive map` creates any missing folder under a path: notation target on Drive without prompting. When false (default), the CLI prompts the user interactively on a TTY, or fails conservatively otherwise. Only applies to path: notation — bare IDs and URLs reference a specific resource.",
    ),
    (
        "rclone",
        "path",
        "Absolute path to a user-provided rclone binary. When set, the daemon uses this instead of probing PATH / cache / downloading.",
    ),
    (
        "rclone",
        "min_version",
        "Minimum acceptable rclone version (informational; the binary check uses a compiled-in constant).",
    ),
];

fn description_for(section: &str, key: &str) -> Option<&'static str> {
    FIELD_DESCRIPTIONS
        .iter()
        .find(|(s, k, _)| *s == section && *k == key)
        .map(|(_, _, d)| *d)
}

/// Outcome of [`migrate_on_disk`]. Useful for tests and for the daemon to
/// `tracing::info!` only when something actually changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// `(section, key)` pairs inserted into the user's config.
    pub inserted: Vec<(String, String)>,
}

impl MigrationReport {
    /// Did the migration touch the file?
    pub fn changed(&self) -> bool {
        !self.inserted.is_empty()
    }
}

/// Walk the current schema and insert any missing field into the on-disk
/// `config.toml` at `path`. Comments, ordering, and user values are
/// preserved. Returns a [`MigrationReport`] describing what was inserted.
///
/// If `path` does not exist, this is a no-op — the user has not run
/// `air-drive init` yet, and the daemon will short-circuit elsewhere with a
/// proper error.
pub fn migrate_on_disk(path: &Path) -> Result<MigrationReport> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationReport::default());
        }
        Err(e) => return Err(e.into()),
    };

    let mut user_doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error::Toml(e.to_string()))?;

    let reference = build_reference_doc()?;

    let mut report = MigrationReport::default();
    merge_into(&reference, &mut user_doc, &mut report);

    if report.changed() {
        write_atomic(path, &user_doc.to_string())?;
    }
    Ok(report)
}

/// Render `Config::default()` through `toml_edit::ser` to get a doc that
/// carries the full current schema. Option fields that default to `None` are
/// naturally absent from this rendering — that is fine, the migration step
/// only adds keys that have a meaningful default value.
fn build_reference_doc() -> Result<DocumentMut> {
    let s = toml_edit::ser::to_string_pretty(&Config::default())
        .map_err(|e| Error::Toml(e.to_string()))?;
    s.parse::<DocumentMut>()
        .map_err(|e| Error::Toml(e.to_string()))
}

/// For every top-level table in `reference`, ensure the same table exists in
/// `user_doc`; then for every key in that reference table, ensure the same
/// key exists in the user's table. Insertions copy the reference value and
/// prefix it with a `# description` line drawn from [`FIELD_DESCRIPTIONS`].
fn merge_into(reference: &DocumentMut, user_doc: &mut DocumentMut, report: &mut MigrationReport) {
    for (section_name, ref_item) in reference.iter() {
        let Some(ref_section) = ref_item.as_table() else {
            continue;
        };

        if !user_doc.contains_table(section_name) {
            let mut t = Table::new();
            t.set_implicit(false);
            user_doc.insert(section_name, Item::Table(t));
        }

        let Some(user_section) = user_doc.get_mut(section_name).and_then(Item::as_table_mut) else {
            // A non-table value is sitting where we expected a section. Skip
            // rather than clobber; the user will see this when `Config::load`
            // parses the file.
            continue;
        };

        for (key_name, ref_value) in ref_section.iter() {
            if user_section.contains_key(key_name) {
                continue;
            }
            let mut key = Key::new(key_name);
            let prefix = match description_for(section_name, key_name) {
                Some(desc) if user_section.is_empty() => format!("# {desc}\n"),
                Some(desc) => format!("\n# {desc}\n"),
                None if user_section.is_empty() => String::new(),
                None => "\n".to_string(),
            };
            if !prefix.is_empty() {
                key.leaf_decor_mut().set_prefix(prefix);
            }
            user_section.insert_formatted(&key, ref_value.clone());
            report
                .inserted
                .push((section_name.to_string(), key_name.to_string()));
        }
    }
}

/// Write `contents` to `path` atomically: write to a sibling temp file in the
/// same directory, then `rename` over the target. `rename` is atomic on POSIX
/// when both paths sit on the same filesystem, so the user's config can never
/// be observed as truncated.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Config(format!("config path has no parent: {}", path.display())))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Config(format!("config path has no file name: {}", path.display())))?
        .to_string_lossy()
        .into_owned();
    let tmp_name = format!(".{file_name}.tmp");
    let tmp = dir.join(tmp_name);

    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn missing_file_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let report = migrate_on_disk(&path).unwrap();
        assert!(!report.changed());
        assert!(!path.exists());
    }

    #[test]
    fn missing_section_is_inserted_with_comment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // User-crafted config with custom values and comments, intentionally
        // missing the [daemon] and [watch] sections entirely.
        write(
            &path,
            "# my air-drive config\n\n[oauth]\nclient_id = \"x.apps.googleusercontent.com\"\n",
        );
        let report = migrate_on_disk(&path).unwrap();
        assert!(report.changed(), "report should mark a change");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("# my air-drive config"),
            "header lost: {body}"
        );
        assert!(
            body.contains("client_id = \"x.apps.googleusercontent.com\""),
            "user value lost: {body}"
        );
        assert!(body.contains("[daemon]"), "[daemon] not added: {body}");
        assert!(
            body.contains("remote_poll_interval_seconds"),
            "remote_poll_interval_seconds not added: {body}"
        );
        assert!(
            body.contains("# Interval at which the daemon polls"),
            "missing field description comment: {body}"
        );
    }

    #[test]
    fn existing_value_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // User has overridden remote_poll_interval_seconds to 45; the migration
        // must NOT roll it back to the default 30.
        write(&path, "[daemon]\nremote_poll_interval_seconds = 45\n");
        let report = migrate_on_disk(&path).unwrap();
        assert!(report.changed());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("remote_poll_interval_seconds = 45"),
            "user override clobbered: {body}"
        );
        // The two siblings should have been inserted.
        assert!(body.contains("safety_net_interval_seconds"));
        assert!(body.contains("log_file"));
    }

    #[test]
    fn idempotent_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // First call materialises everything.
        write(&path, "");
        let _ = migrate_on_disk(&path).unwrap();
        let body_after_first = std::fs::read_to_string(&path).unwrap();
        // Second call must be a no-op.
        let report = migrate_on_disk(&path).unwrap();
        assert!(!report.changed());
        let body_after_second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body_after_first, body_after_second);
    }

    #[test]
    fn comments_and_blank_lines_around_other_sections_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "\
# Top-of-file note from the user.

[oauth]
# Personal client.
client_id = \"x.apps.googleusercontent.com\"

# Below: nothing else for now.
";
        write(&path, original);
        let _ = migrate_on_disk(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Top-of-file note from the user."));
        assert!(body.contains("# Personal client."));
        assert!(body.contains("# Below: nothing else for now."));
        assert!(body.contains("client_id = \"x.apps.googleusercontent.com\""));
    }

    #[test]
    fn migrated_file_still_parses_with_strict_loader() {
        // After migration, Config::load (deny_unknown_fields) must still succeed:
        // every key we inserted is part of the current schema by construction.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        write(&path, "[daemon]\nremote_poll_interval_seconds = 45\n");
        let _ = migrate_on_disk(&path).unwrap();
        let cfg = Config::load(&path).expect("migrated file must parse");
        assert_eq!(cfg.daemon.remote_poll_interval_seconds, 45);
        assert_eq!(cfg.daemon.safety_net_interval_seconds, 300);
    }

    #[test]
    fn hand_crafted_upgrade_scenario_end_to_end() {
        // Mirrors the issue's acceptance criterion: a hand-crafted config.toml
        // with comments AND custom values, simulating an upgrade where one of
        // the schema's fields is "new" by being absent from the file. The
        // migration must add the missing field with a leading comment and
        // leave every comment + user value intact.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "\
# air-drive — my personal sync setup.
# Hand-edited by the user; this header MUST survive.

[oauth]
# Using my own GCP project because the embedded one hits quota.
client_id = \"123-abc.apps.googleusercontent.com\"
client_secret = \"GOCSPX-mysecret\"

[daemon]
# Polling every 20 s during testing.
remote_poll_interval_seconds = 20
# (safety_net_interval_seconds is intentionally MISSING — pretend it was
# added in a newer release.)
log_file = \"/tmp/air-drive.log\"
";
        write(&path, original);

        let report = migrate_on_disk(&path).unwrap();
        assert!(report.changed(), "migration should report changes");
        assert!(
            report.inserted.contains(&(
                "daemon".to_string(),
                "safety_net_interval_seconds".to_string()
            )),
            "expected safety_net_interval_seconds to be inserted, got {:?}",
            report.inserted
        );

        let body = std::fs::read_to_string(&path).unwrap();

        // User comments survive verbatim.
        assert!(body.contains("# air-drive — my personal sync setup."));
        assert!(body.contains("# Hand-edited by the user; this header MUST survive."));
        assert!(body.contains("# Using my own GCP project because the embedded one hits quota."));
        assert!(body.contains("# Polling every 20 s during testing."));
        assert!(body.contains("# (safety_net_interval_seconds is intentionally MISSING"));

        // User values survive verbatim.
        assert!(body.contains("client_id = \"123-abc.apps.googleusercontent.com\""));
        assert!(body.contains("client_secret = \"GOCSPX-mysecret\""));
        assert!(body.contains("remote_poll_interval_seconds = 20"));
        assert!(body.contains("log_file = \"/tmp/air-drive.log\""));

        // Newly-added field appears with its description comment.
        assert!(
            body.contains("safety_net_interval_seconds"),
            "missing field not inserted: {body}"
        );
        assert!(
            body.contains("# Interval of the safety-net reconciliation cycle"),
            "missing field comment not inserted: {body}"
        );

        // Strict loader still accepts the migrated file.
        let cfg = Config::load(&path).expect("migrated file must parse");
        assert_eq!(cfg.daemon.remote_poll_interval_seconds, 20);
        assert_eq!(cfg.daemon.safety_net_interval_seconds, 300);
        assert_eq!(cfg.daemon.log_file, "/tmp/air-drive.log");
        assert_eq!(
            cfg.oauth.client_id.as_deref(),
            Some("123-abc.apps.googleusercontent.com")
        );

        // Running the migration a second time is a no-op (idempotence).
        let body_before_second = body;
        let report2 = migrate_on_disk(&path).unwrap();
        assert!(!report2.changed(), "second pass should be a no-op");
        let body_after_second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body_before_second, body_after_second);
    }

    #[test]
    fn invalid_toml_surfaces_as_toml_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        write(&path, "this is = not = valid = toml = at all\n[[");
        let err = migrate_on_disk(&path).unwrap_err();
        assert!(matches!(err, Error::Toml(_)), "got: {err:?}");
    }
}
