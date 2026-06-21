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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// How long a buffered rename `From` half-event waits for its matching `To`
/// before being discarded. Linux inotify delivers `From`, `To` and `Both` for a
/// within-tree rename in the same batch, so the match is effectively instant; a
/// `From` that lingers past this TTL was a move *out* of the watched tree (no
/// `To` follows), whose deletion the periodic safety-net reconcile reconciles.
const RENAME_CORRELATION_TTL: Duration = Duration::from_secs(5);

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
        // Per-watcher buffer correlating inotify rename half-events by tracker
        // (cookie). Owned by the `FnMut` handler, so access is serialised by the
        // single notify callback thread — no lock needed.
        let mut pending_renames: HashMap<usize, (PathBuf, Instant)> = HashMap::new();
        let handler = move |res: notify::Result<notify::Event>| {
            let ev = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "notify error");
                    return;
                }
            };
            for converted in
                convert_event(&ev, &handler_root, &handler_ignore, &mut pending_renames)
            {
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
/// ## Rename correlation
///
/// Linux inotify reports a within-tree rename as **three** events sharing one
/// tracker (cookie): `Modify(Name(From))`, `Modify(Name(To))`, then
/// `Modify(Name(Both))`. A move *across* the watch boundary is a single lone half:
/// `From` only (moved out) or `To` only (moved in). To tell these apart we buffer
/// each `From` by its tracker in `pending_renames`; the matching `To` resolves the
/// pair into a single [`WatchEvent::Renamed`]:
///
/// - `From` (tracker T) → buffer T, emit nothing.
/// - `To` (tracker T) → if T was buffered, emit `Renamed{from, to}`; else it is a
///   move into the tree, emit `Created(to)`.
/// - `Both` → on Linux the `To` already emitted the rename and cleared T, so this
///   is dropped; if T is still buffered (a backend that skips the separate `To`),
///   resolve it here.
/// - a lone `From` (move out of the tree) stays buffered until [`RENAME_CORRELATION_TTL`]
///   evicts it; the deletion is then caught by the periodic safety-net reconcile.
fn convert_event(
    ev: &notify::Event,
    root: &Path,
    ignore: &GlobSet,
    pending_renames: &mut HashMap<usize, (PathBuf, Instant)>,
) -> Vec<WatchEvent> {
    let mut out = Vec::new();
    let stage_dir = root.join(crate::engine::staging::PARTIAL_DIR);

    let kept: Vec<&PathBuf> = ev
        .paths
        .iter()
        .filter(|p| !p.starts_with(&stage_dir))
        .filter(|p| accept_local_file(p, ignore))
        .collect();
    let tracker = ev.attrs.tracker();

    match ev.kind {
        EventKind::Create(_) => {
            for p in kept {
                out.push(WatchEvent::Created(p.clone()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            // Buffer the source; the matching `To` (same tracker) resolves it.
            if let (Some(t), Some(p)) = (tracker, kept.first()) {
                evict_stale_renames(pending_renames);
                pending_renames.insert(t, ((*p).clone(), Instant::now()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            let from = tracker
                .and_then(|t| pending_renames.remove(&t))
                .map(|(p, _)| p);
            if let Some(to) = kept.first() {
                match from {
                    Some(from) => out.push(WatchEvent::Renamed {
                        from,
                        to: (*to).clone(),
                    }),
                    // No buffered source: this is a move *into* the watched tree.
                    None => out.push(WatchEvent::Created((*to).clone())),
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() == 2 => {
            // Linux emits `Both` *after* `From`+`To`, so the `To` arm has already
            // emitted the Renamed and cleared the tracker — drop the redundant
            // `Both`. If the source is still buffered (a backend that emits
            // `From`+`Both` without a separate `To`), resolve the rename here.
            if let Some(t) = tracker
                && pending_renames.remove(&t).is_some()
            {
                out.push(WatchEvent::Renamed {
                    from: ev.paths[0].clone(),
                    to: ev.paths[1].clone(),
                });
            }
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

/// Drop buffered rename `From` halves older than [`RENAME_CORRELATION_TTL`] — a
/// `From` with no following `To` was a move out of the watched tree, so its slot
/// would otherwise leak. Called whenever a fresh `From` is buffered.
fn evict_stale_renames(pending: &mut HashMap<usize, (PathBuf, Instant)>) {
    let now = Instant::now();
    pending.retain(|_, (_, seen)| now.duration_since(*seen) < RENAME_CORRELATION_TTL);
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

    // --- convert_event: simulate the exact inotify sequences (see the probe in
    // the commit message) without touching the filesystem or a real watcher. ---

    fn name_event(kind: EventKind, tracker: Option<usize>, paths: &[&Path]) -> notify::Event {
        let mut ev = notify::Event::new(kind);
        for p in paths {
            ev = ev.add_path(p.to_path_buf());
        }
        if let Some(t) = tracker {
            ev = ev.set_tracker(t);
        }
        ev
    }

    /// Within-tree rename: Linux emits From, To, then Both (same tracker). Exactly
    /// one `Renamed` must come out — not the `Modified`/`Created` that the old
    /// catch-all produced (which made the reconciler delete+recreate the folder).
    #[test]
    fn convert_within_tree_rename_emits_single_renamed() {
        let m = default_matcher();
        let root = Path::new("/x");
        let from = root.join("a.txt");
        let to = root.join("b.txt");
        let mut pending = HashMap::new();

        let e_from = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            Some(7),
            &[&from],
        );
        let e_to = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            Some(7),
            &[&to],
        );
        let e_both = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            Some(7),
            &[&from, &to],
        );

        assert_eq!(convert_event(&e_from, root, &m, &mut pending), vec![]);
        assert_eq!(
            convert_event(&e_to, root, &m, &mut pending),
            vec![WatchEvent::Renamed {
                from: from.clone(),
                to: to.clone()
            }]
        );
        assert_eq!(convert_event(&e_both, root, &m, &mut pending), vec![]);
        assert!(pending.is_empty(), "the tracker buffer must be drained");
    }

    /// A move *into* the tree is a lone `To` (no buffered `From`) → a fresh create.
    #[test]
    fn convert_move_into_tree_emits_created() {
        let m = default_matcher();
        let root = Path::new("/x");
        let to = root.join("incoming.txt");
        let mut pending = HashMap::new();
        let e_to = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            Some(11),
            &[&to],
        );
        assert_eq!(
            convert_event(&e_to, root, &m, &mut pending),
            vec![WatchEvent::Created(to)]
        );
    }

    /// A move *out* of the tree is a lone `From`: buffered, no event now (the
    /// safety-net reconcile handles the deletion), and TTL-evictable.
    #[test]
    fn convert_move_out_of_tree_buffers_from_silently() {
        let m = default_matcher();
        let root = Path::new("/x");
        let from = root.join("leaving.txt");
        let mut pending = HashMap::new();
        let e_from = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            Some(13),
            &[&from],
        );
        assert_eq!(convert_event(&e_from, root, &m, &mut pending), vec![]);
        assert_eq!(pending.len(), 1, "the From half is buffered");
    }

    /// Defensive: a backend that emits `From` + `Both` without a separate `To`
    /// still resolves to a single `Renamed` via the `Both` arm.
    #[test]
    fn convert_from_then_both_without_to_resolves_via_both() {
        let m = default_matcher();
        let root = Path::new("/x");
        let from = root.join("a.txt");
        let to = root.join("b.txt");
        let mut pending = HashMap::new();
        let e_from = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            Some(21),
            &[&from],
        );
        let e_both = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            Some(21),
            &[&from, &to],
        );
        assert_eq!(convert_event(&e_from, root, &m, &mut pending), vec![]);
        assert_eq!(
            convert_event(&e_both, root, &m, &mut pending),
            vec![WatchEvent::Renamed { from, to }]
        );
    }

    /// Plain create / modify / delete still map straight through.
    #[test]
    fn convert_create_modify_delete_pass_through() {
        use notify::event::{DataChange, ModifyKind};
        let m = default_matcher();
        let root = Path::new("/x");
        let p = root.join("f.txt");
        let mut pending = HashMap::new();

        let create = name_event(
            EventKind::Create(notify::event::CreateKind::File),
            None,
            &[&p],
        );
        assert_eq!(
            convert_event(&create, root, &m, &mut pending),
            vec![WatchEvent::Created(p.clone())]
        );
        let modify = name_event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            None,
            &[&p],
        );
        assert_eq!(
            convert_event(&modify, root, &m, &mut pending),
            vec![WatchEvent::Modified(p.clone())]
        );
        let remove = name_event(
            EventKind::Remove(notify::event::RemoveKind::File),
            None,
            &[&p],
        );
        assert_eq!(
            convert_event(&remove, root, &m, &mut pending),
            vec![WatchEvent::Deleted(p)]
        );
    }

    /// End-to-end against the real inotify backend: a within-tree rename must
    /// surface as `Renamed`, not `Created`/`Modified`. This is the regression
    /// guard for the folder-rename bug the e2e suite caught (#7).
    #[tokio::test]
    async fn watcher_emits_renamed_for_within_tree_rename() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        let (_w, mut rx) = Watcher::start(tmp.path(), default_matcher()).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::rename(tmp.path().join("a.txt"), tmp.path().join("b.txt")).unwrap();

        let verdict = async {
            loop {
                match rx.recv().await {
                    Some(WatchEvent::Renamed { from, to })
                        if from.ends_with("a.txt") && to.ends_with("b.txt") =>
                    {
                        return true;
                    }
                    // The bug manifested as a spurious Created/Modified on the
                    // destination instead of a Renamed.
                    Some(WatchEvent::Created(p)) | Some(WatchEvent::Modified(p))
                        if p.ends_with("b.txt") =>
                    {
                        return false;
                    }
                    Some(_) => continue,
                    None => return false,
                }
            }
        };
        let ok = tokio::time::timeout(Duration::from_secs(2), verdict)
            .await
            .unwrap_or(false);
        assert!(
            ok,
            "within-tree rename must surface as Renamed, not Created/Modified"
        );
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
