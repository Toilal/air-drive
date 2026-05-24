//! Local filesystem watcher built on the `notify` crate.
//!
//! [`Watcher`] wraps `notify::RecommendedWatcher` (inotify on Linux), maps raw
//! `notify::Event`s into the daemon-internal [`WatchEvent`] enum, filters out
//! symlinks + special files, and forwards everything to a tokio mpsc channel
//! that the debouncer (`super::debounce`) consumes.
//!
//! The watcher holds the inotify file descriptor for the entire lifetime of the
//! returned struct — dropping it cancels the subscription.

pub mod debounce;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Compile a list of glob patterns into a matcher. Each pattern is matched
/// against the **file name** (not the full path) at runtime — see
/// [`config::default_ignore_patterns`](crate::config::default_ignore_patterns)
/// for the seeded defaults.
///
/// Returns [`Error::Config`] on the first invalid pattern with a message
/// identifying it, so a typo in `config.toml` surfaces at daemon startup
/// rather than as a silent miss at runtime.
pub fn build_ignore_matcher(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p)
            .map_err(|e| Error::Config(format!("invalid ignore pattern '{p}': {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| Error::Config(format!("ignore pattern set build: {e}")))
}

/// Daemon-level filesystem event. `notify::Event` is collapsed into one of these
/// variants before reaching the debouncer / reconciler, so downstream code never
/// has to deal with the platform-specific `notify::EventKind` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// New regular file or directory.
    Created(PathBuf),
    /// Content modified (write closed).
    Modified(PathBuf),
    /// File or directory removed.
    Deleted(PathBuf),
    /// Rename / move within the watched tree.
    Renamed {
        /// Source path (no longer exists after the event).
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },
}

impl WatchEvent {
    /// Primary path the event refers to. For `Renamed`, the **destination**.
    pub fn path(&self) -> &Path {
        match self {
            WatchEvent::Created(p) | WatchEvent::Modified(p) | WatchEvent::Deleted(p) => p,
            WatchEvent::Renamed { to, .. } => to,
        }
    }
}

/// Owns the `notify` watcher handle. Dropping it stops the inotify subscription.
pub struct Watcher {
    _inner: RecommendedWatcher,
}

impl Watcher {
    /// Start watching `local_root` recursively. Events arrive on the returned
    /// channel; the daemon's debouncer reads from it. `ignore` matches against
    /// the **file name** of every event path; matches are dropped before
    /// reaching the channel.
    ///
    /// Channel capacity is generous (1024) so a burst of editor saves doesn't
    /// drop events. If the channel ever fills, the bridge thread logs a
    /// `tracing::warn` and continues — losing one watch event is preferable to
    /// blocking the inotify thread.
    pub fn start(
        local_root: &Path,
        ignore: Arc<GlobSet>,
    ) -> Result<(Self, mpsc::Receiver<WatchEvent>)> {
        let (tx, rx) = mpsc::channel::<WatchEvent>(1024);
        let root = Arc::new(local_root.to_path_buf());

        let handler_root = root.clone();
        let handler_ignore = ignore.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let ev = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "notify error");
                    return;
                }
            };
            for converted in convert_event(&ev, &handler_root, &handler_ignore) {
                // `try_send` is non-blocking. Capacity > 1024 should swallow
                // every realistic burst, but if it doesn't we'd rather drop a
                // single event than block the inotify thread.
                if let Err(e) = tx.try_send(converted) {
                    tracing::warn!(error = %e, "watch channel full or closed");
                }
            }
        };

        let mut inner = RecommendedWatcher::new(handler, notify::Config::default())
            .map_err(|e| Error::Config(format!("notify init: {e}")))?;
        inner
            .watch(local_root, RecursiveMode::Recursive)
            .map_err(|e| Error::Config(format!("notify watch({}): {e}", local_root.display())))?;
        Ok((Self { _inner: inner }, rx))
    }
}

/// Convert a `notify::Event` to zero or more [`WatchEvent`]s. Filters out:
///
/// - symlinks and special files (lstat + reject non-regular non-directory)
/// - paths under `<root>/.air-drive-partial/` — those are our own staging artefacts
///
/// Only one event is produced per logical path; for compound kinds (e.g. some
/// inotify `Modify` flavours) we still emit a single `Modified`.
fn convert_event(ev: &notify::Event, root: &Path, ignore: &GlobSet) -> Vec<WatchEvent> {
    let mut out = Vec::new();
    let stage_dir = root.join(crate::engine::staging::PARTIAL_DIR);

    let kept: Vec<&PathBuf> = ev
        .paths
        .iter()
        .filter(|p| !p.starts_with(&stage_dir))
        .filter(|p| accept_local_file(p, ignore))
        .collect();

    match ev.kind {
        EventKind::Create(_) => {
            for p in kept {
                out.push(WatchEvent::Created(p.clone()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() == 2 => {
            // Many platforms emit a single Modify event with [from, to] for renames.
            // We only emit one Renamed even if either side is filtered out — the
            // reconciler handles "renamed to / from outside" the watched tree as
            // a delete + create separately.
            out.push(WatchEvent::Renamed {
                from: ev.paths[0].clone(),
                to: ev.paths[1].clone(),
            });
        }
        EventKind::Modify(_) => {
            for p in kept {
                out.push(WatchEvent::Modified(p.clone()));
            }
        }
        EventKind::Remove(_) => {
            for p in kept {
                out.push(WatchEvent::Deleted(p.clone()));
            }
        }
        // Access / Any / Other events are ignored.
        _ => {}
    }
    out
}

/// Returns `true` if the path either doesn't exist (deletes get filtered to None
/// metadata) OR is a regular file / directory. Symlinks and special files
/// (FIFO, socket, block/char device) are rejected with a one-line `tracing::info`.
/// Files whose name matches one of the user-configurable `watch.ignore_patterns`
/// globs are also rejected — these are typically editor / OS scratch files
/// (`.foo.swp`, `.DS_Store`, …) the user never wants synced.
fn accept_local_file(path: &Path, ignore: &GlobSet) -> bool {
    if let Some(name) = path.file_name()
        && ignore.is_match(Path::new(name))
    {
        tracing::debug!(
            path = %path.display(),
            "ignoring file (matches watch.ignore_patterns)"
        );
        return false;
    }
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // File no longer exists — likely a Delete event. Don't filter it out
        // here; the reconciler decides what to do when it sees a Deleted on a
        // path that doesn't exist anymore.
        Err(_) => return true,
    };
    let ft = md.file_type();
    if ft.is_file() || ft.is_dir() {
        return true;
    }
    tracing::info!(
        path = %path.display(),
        "ignoring non-regular file (symlink, fifo, socket, device)"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn default_matcher() -> Arc<GlobSet> {
        let patterns: Vec<String> = crate::config::default_ignore_patterns()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        Arc::new(build_ignore_matcher(&patterns).expect("default patterns compile"))
    }

    fn matches(name: &str) -> bool {
        let m = default_matcher();
        m.is_match(Path::new(name))
    }

    #[test]
    fn default_patterns_match_known_temps() {
        // Regular files MUST NOT match.
        assert!(!matches("foo.txt"));
        assert!(!matches("foo.md"));
        assert!(!matches(".hidden"));

        // vim swap files + atomic-save sentinel.
        assert!(matches(".foo.txt.swp"));
        assert!(matches(".foo.txt.swo"));
        assert!(matches(".bar.swx"));
        assert!(matches(".bar.swn"));
        assert!(matches("4913"));

        // emacs.
        assert!(matches("#foo.txt#"));
        assert!(matches(".#foo.txt"));
        assert!(matches("foo.txt~"));

        // gedit / nautilus.
        assert!(matches(".goutputstream-AB12CD"));

        // LibreOffice.
        assert!(matches(".~lock.report.odt#"));

        // MS Office.
        assert!(matches("~$report.docx"));

        // macOS / Windows.
        assert!(matches(".DS_Store"));
        assert!(matches("._appledouble"));
        assert!(matches("Thumbs.db"));
        assert!(matches("desktop.ini"));

        // JetBrains.
        assert!(matches("foo.___jb_tmp___"));
        assert!(matches("foo.___jb_old___"));
    }

    #[test]
    fn invalid_pattern_surfaces_as_config_error() {
        let bad = vec!["[unclosed".to_string()];
        let err = build_ignore_matcher(&bad).unwrap_err();
        assert!(
            matches!(err, Error::Config(msg) if msg.contains("invalid ignore pattern")),
            "expected Config error"
        );
    }

    #[tokio::test]
    async fn watcher_emits_created_for_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (_w, mut rx) = Watcher::start(tmp.path(), default_matcher()).unwrap();
        // Slight pause so the watcher's setup completes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event arrives")
            .expect("channel open");
        match ev {
            WatchEvent::Created(p) | WatchEvent::Modified(p) => {
                assert!(p.ends_with("a.txt"), "got {p:?}");
            }
            other => panic!("expected Created/Modified, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn watcher_emits_deleted_when_file_removed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.txt"), b"hi").unwrap();
        let (_w, mut rx) = Watcher::start(tmp.path(), default_matcher()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::remove_file(tmp.path().join("b.txt")).unwrap();
        // Drain until we see Deleted (the platform may emit Modified first).
        let saw_delete = async {
            loop {
                match rx.recv().await {
                    Some(WatchEvent::Deleted(p)) if p.ends_with("b.txt") => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
        };
        let ok = tokio::time::timeout(Duration::from_secs(2), saw_delete)
            .await
            .unwrap_or(false);
        assert!(ok, "expected Deleted event for b.txt");
    }

    #[tokio::test]
    async fn watcher_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, b"t").unwrap();
        let link = tmp.path().join("link.txt");

        let (_w, mut rx) = Watcher::start(tmp.path(), default_matcher()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a symlink — the create event should be filtered out.
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Anything we receive within 500 ms MUST be for `target.txt` (created
        // before the watcher started) or unrelated. The symlink path itself
        // must NOT appear.
        let result = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                match rx.recv().await {
                    Some(ev) => {
                        if ev.path().ends_with("link.txt") {
                            return Some(ev);
                        }
                    }
                    None => return None,
                }
            }
        })
        .await;
        assert!(
            result.is_err() || result.unwrap().is_none(),
            "symlink event should be filtered out"
        );
    }
}
