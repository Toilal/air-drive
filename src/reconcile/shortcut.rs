//! Native Google Docs → local shortcut files.
//!
//! Native Google formats (`application/vnd.google-apps.document`, `.spreadsheet`,
//! `.presentation`, …) have no `md5Checksum` and no byte stream Drive will hand us,
//! so they cannot be synced as opaque files like everything else. Rather than leave
//! them invisible, the daemon writes a small **shortcut file** next to where the doc
//! would live: a JSON pointer carrying the doc's web URL and Drive id, mirroring the
//! Google Drive desktop client's `.gdoc`/`.gsheet`/`.gslides` files closely enough
//! that the same tooling opens it in the browser.
//!
//! Shortcuts are **one-directional**: the daemon writes and renames/removes them to
//! track the remote doc, but never uploads a shortcut back to Drive. Their
//! `sync_item` rows carry [`crate::state::items::ItemState::Skipped`], which both
//! marks them for `air-drive status` and tells the local watcher path
//! ([`crate::reconcile::continuous::apply_local`]) not to treat the on-disk pointer
//! as a regular file to upload.
//!
//! This module holds only pure helpers (mime → extension / URL / path / body) plus a
//! thin file writer; the reconciler decides *when* to call them and the dispatcher
//! performs the queued write (`Operation::WriteShortcut`).

use std::path::Path;

use crate::drive::metadata::FOLDER_MIME;
use crate::error::{Error, Result};

/// Mime prefix shared by every native Google app type (Docs, Sheets, folders, …).
const NATIVE_PREFIX: &str = "application/vnd.google-apps.";

/// `true` for a native Google format we represent as a shortcut. Folders share the
/// `vnd.google-apps.*` prefix but are real containers handled elsewhere, so they are
/// explicitly excluded.
pub fn is_native(mime: &str) -> bool {
    mime.starts_with(NATIVE_PREFIX) && mime != FOLDER_MIME
}

/// File extension (without the leading dot) for a native doc's shortcut. Unknown
/// native types fall back to the generic `glink`.
pub fn extension(mime: &str) -> &'static str {
    match mime {
        "application/vnd.google-apps.document" => "gdoc",
        "application/vnd.google-apps.spreadsheet" => "gsheet",
        "application/vnd.google-apps.presentation" => "gslides",
        "application/vnd.google-apps.drawing" => "gdraw",
        "application/vnd.google-apps.form" => "gform",
        "application/vnd.google-apps.script" => "gscript",
        "application/vnd.google-apps.site" => "gsite",
        "application/vnd.google-apps.jam" => "gjam",
        "application/vnd.google-apps.map" => "gmap",
        _ => "glink",
    }
}

/// Browser URL that opens the doc. Drive's web app uses a stable per-type path, so we
/// build it from the mime type and id rather than depending on a `webViewLink` field
/// (which would force an extra metadata field on every `files.list`).
pub fn web_url(mime: &str, id: &str) -> String {
    match mime {
        "application/vnd.google-apps.document" => {
            format!("https://docs.google.com/document/d/{id}/edit")
        }
        "application/vnd.google-apps.spreadsheet" => {
            format!("https://docs.google.com/spreadsheets/d/{id}/edit")
        }
        "application/vnd.google-apps.presentation" => {
            format!("https://docs.google.com/presentation/d/{id}/edit")
        }
        "application/vnd.google-apps.drawing" => {
            format!("https://docs.google.com/drawings/d/{id}/edit")
        }
        "application/vnd.google-apps.form" => format!("https://docs.google.com/forms/d/{id}/edit"),
        "application/vnd.google-apps.script" => format!("https://script.google.com/d/{id}/edit"),
        // Any other native type (site, jam, map, …): the generic Drive opener resolves
        // it to the right editor server-side.
        _ => format!("https://drive.google.com/open?id={id}"),
    }
}

/// Append the shortcut extension to a native doc's base relative path. Native docs
/// have no file extension on Drive, so `"Notes"` (a Google Doc) → `"Notes.gdoc"`.
pub fn relative_path(base: &str, mime: &str) -> String {
    format!("{base}.{}", extension(mime))
}

/// The JSON body written to the shortcut file. Built through `serde_json` so any
/// special characters in the id/mime are escaped correctly; a trailing newline keeps
/// it tidy for `cat`/editors.
pub fn content(mime: &str, id: &str) -> String {
    let body = serde_json::json!({
        "url": web_url(mime, id),
        "doc_id": id,
        "mime_type": mime,
        "resource_key": serde_json::Value::Null,
    });
    let mut s = serde_json::to_string_pretty(&body).unwrap_or_default();
    s.push('\n');
    s
}

/// Write a shortcut file at `path`, creating parent directories as needed. Shared by
/// the initial reconciliation pass (synchronous) and the dispatcher's
/// `Operation::WriteShortcut` path.
pub async fn write(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
    }
    tokio::fs::write(path, body).await.map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_native_excludes_folders_and_regular_files() {
        assert!(is_native("application/vnd.google-apps.document"));
        assert!(is_native("application/vnd.google-apps.spreadsheet"));
        assert!(is_native("application/vnd.google-apps.unknown-future-type"));
        assert!(!is_native(FOLDER_MIME));
        assert!(!is_native("text/plain"));
        assert!(!is_native("application/pdf"));
    }

    #[test]
    fn extension_maps_known_types_and_falls_back() {
        assert_eq!(extension("application/vnd.google-apps.document"), "gdoc");
        assert_eq!(
            extension("application/vnd.google-apps.spreadsheet"),
            "gsheet"
        );
        assert_eq!(
            extension("application/vnd.google-apps.presentation"),
            "gslides"
        );
        assert_eq!(extension("application/vnd.google-apps.whatever"), "glink");
    }

    #[test]
    fn web_url_uses_per_type_paths() {
        assert_eq!(
            web_url("application/vnd.google-apps.document", "ABC"),
            "https://docs.google.com/document/d/ABC/edit"
        );
        assert_eq!(
            web_url("application/vnd.google-apps.spreadsheet", "ABC"),
            "https://docs.google.com/spreadsheets/d/ABC/edit"
        );
        assert_eq!(
            web_url("application/vnd.google-apps.site", "ABC"),
            "https://drive.google.com/open?id=ABC"
        );
    }

    #[test]
    fn relative_path_appends_extension() {
        assert_eq!(
            relative_path("Notes", "application/vnd.google-apps.document"),
            "Notes.gdoc"
        );
        assert_eq!(
            relative_path("sub/Budget", "application/vnd.google-apps.spreadsheet"),
            "sub/Budget.gsheet"
        );
    }

    #[test]
    fn content_is_valid_json_with_url_and_id() {
        let s = content("application/vnd.google-apps.document", "DOC123");
        assert!(s.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["url"].as_str().unwrap(),
            "https://docs.google.com/document/d/DOC123/edit"
        );
        assert_eq!(v["doc_id"].as_str().unwrap(), "DOC123");
        assert_eq!(
            v["mime_type"].as_str().unwrap(),
            "application/vnd.google-apps.document"
        );
        assert!(v["resource_key"].is_null());
    }

    #[tokio::test]
    async fn write_creates_parents_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/Notes.gdoc");
        write(&path, "hello\n").await.unwrap();
        let got = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(got, "hello\n");
    }
}
