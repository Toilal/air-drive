//! Local filesystem watcher built on the `notify` crate.
//!
//! [`Watcher`] wraps `notify::RecommendedWatcher` (inotify on Linux), maps raw
//! `notify::Event`s into the daemon-internal [`WatchEvent`] enum, applies the
//! `[watch].symlinks` policy ([`classify_local`]) and drops special files, and
//! forwards everything to a tokio mpsc channel that the debouncer
//! (`super::debounce`) consumes.
//!
//! The watcher holds the inotify file descriptor for the entire lifetime of the
//! returned struct — dropping it cancels the subscription.

pub mod debounce;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc;

use crate::config::SymlinkPolicy;
use crate::error::{Error, Result};

/// How long a buffered rename `From` half-event waits for its matching `To`
/// before being treated as a move *out* of the watched tree. Linux inotify
/// delivers `From`, `To` and `Both` for a within-tree rename in the same read
/// batch, so the match is effectively instant (sub-millisecond); a `From` that
/// lingers past this TTL had no `To` follow it, i.e. the file was moved out of
/// the tree (most commonly into the desktop trash) — equivalent to a deletion,
/// so it is emitted as [`WatchEvent::Deleted`] rather than silently dropped.
///
/// This value is also the latency floor for detecting such a move, so it is kept
/// short. 1 s is a ~100–1000× margin over the real correlation gap while making
/// "send to trash" feel near-instant; too short risks a within-tree rename whose
/// `To` is delayed under load being mis-emitted as a delete-then-create.
const RENAME_CORRELATION_TTL: Duration = Duration::from_secs(1);

/// How often the rename reaper task scans for `From` halves that have outlived
/// [`RENAME_CORRELATION_TTL`]. Bounds how late a move-out-of-tree deletion is
/// emitted on an otherwise quiet tree (worst case TTL + this interval).
const RENAME_REAP_INTERVAL: Duration = Duration::from_millis(250);

/// Rename `From` halves buffered by tracker (cookie), shared between the notify
/// callback (which inserts/correlates) and the reaper task (which times out
/// uncorrelated halves into deletions). A plain `std::sync::Mutex` is enough:
/// every critical section is a quick map operation with no `.await` inside.
type SharedRenames = Arc<Mutex<HashMap<usize, (PathBuf, Instant)>>>;

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

/// Owns the `notify` watcher handle. Dropping it stops the inotify subscription
/// and aborts the rename reaper task.
pub struct Watcher {
    _inner: RecommendedWatcher,
    reaper: tokio::task::JoinHandle<()>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.reaper.abort();
    }
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
        symlinks: SymlinkPolicy,
    ) -> Result<(Self, mpsc::Receiver<WatchEvent>)> {
        let (tx, rx) = mpsc::channel::<WatchEvent>(1024);
        let root = Arc::new(local_root.to_path_buf());

        let handler_root = root.clone();
        let handler_ignore = ignore.clone();
        // Buffer correlating inotify rename half-events by tracker (cookie),
        // shared with the reaper task so a `From` with no `To` (a move out of
        // the tree) is timed out into a deletion even on an otherwise quiet tree.
        let pending_renames: SharedRenames = Arc::new(Mutex::new(HashMap::new()));
        let reaper_renames = pending_renames.clone();
        let reaper_tx = tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let ev = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "notify error");
                    return;
                }
            };
            // Hold the lock only for the conversion; send outside it.
            let converted = {
                let mut pending = match pending_renames.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                convert_event(&ev, &handler_root, &handler_ignore, symlinks, &mut pending)
            };
            for ev in converted {
                // `try_send` is non-blocking. Capacity > 1024 should swallow
                // every realistic burst, but if it doesn't we'd rather drop a
                // single event than block the inotify thread.
                if let Err(e) = tx.try_send(ev) {
                    tracing::warn!(error = %e, "watch channel full or closed");
                }
            }
        };

        let mut inner = RecommendedWatcher::new(handler, notify::Config::default())
            .map_err(|e| Error::Config(format!("notify init: {e}")))?;
        inner
            .watch(local_root, RecursiveMode::Recursive)
            .map_err(|e| Error::Config(format!("notify watch({}): {e}", local_root.display())))?;

        // Reaper: times out uncorrelated `From` halves into `Deleted` events.
        let reaper = tokio::spawn(reap_renames(
            reaper_renames,
            reaper_tx,
            RENAME_CORRELATION_TTL,
            RENAME_REAP_INTERVAL,
        ));
        Ok((
            Self {
                _inner: inner,
                reaper,
            },
            rx,
        ))
    }
}

/// Convert a `notify::Event` to zero or more [`WatchEvent`]s. Filters out:
///
/// - paths rejected by the `symlinks` policy / special files ([`classify_local`])
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
/// - a lone `From` (move out of the tree) is buffered until [`RENAME_CORRELATION_TTL`]
///   passes with no matching `To`, then emitted as [`WatchEvent::Deleted`] — by the
///   next event's sweep, or the [`reap_renames`] task on a quiet tree. Moving a file
///   to the desktop trash is exactly this case, so such deletes propagate promptly
///   rather than waiting for the safety-net reconcile.
fn convert_event(
    ev: &notify::Event,
    root: &Path,
    ignore: &GlobSet,
    symlinks: SymlinkPolicy,
    pending_renames: &mut HashMap<usize, (PathBuf, Instant)>,
) -> Vec<WatchEvent> {
    let mut out = Vec::new();
    let stage_dir = root.join(crate::engine::staging::PARTIAL_DIR);

    let kept: Vec<&PathBuf> = ev
        .paths
        .iter()
        .filter(|p| !p.starts_with(&stage_dir))
        .filter(|p| accept_local_file(p, ignore, root, symlinks))
        .collect();
    let tracker = ev.attrs.tracker();

    match ev.kind {
        EventKind::Create(_) => {
            for p in kept {
                out.push(WatchEvent::Created(p.clone()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            // Any `From` that has outlived the TTL was a move out of the tree:
            // surface it as a deletion before buffering the new one.
            for p in drain_stale_renames(pending_renames, Instant::now(), RENAME_CORRELATION_TTL) {
                out.push(WatchEvent::Deleted(p));
            }
            // Buffer the source; the matching `To` (same tracker) resolves it.
            if let (Some(t), Some(p)) = (tracker, kept.first()) {
                pending_renames.insert(t, ((*p).clone(), Instant::now()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            // Sweep stale `From` halves here too, not only when a fresh `From`
            // arrives — surfacing each as a deletion.
            for p in drain_stale_renames(pending_renames, Instant::now(), RENAME_CORRELATION_TTL) {
                out.push(WatchEvent::Deleted(p));
            }
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

/// Remove and return the source paths of buffered rename `From` halves older
/// than `ttl`. A `From` with no following `To` within the TTL was a move out of
/// the watched tree (commonly into the desktop trash); each returned path is a
/// deletion the caller must propagate. Pure and time-injectable for testing.
fn drain_stale_renames(
    pending: &mut HashMap<usize, (PathBuf, Instant)>,
    now: Instant,
    ttl: Duration,
) -> Vec<PathBuf> {
    let stale: Vec<usize> = pending
        .iter()
        .filter(|(_, (_, seen))| now.duration_since(*seen) >= ttl)
        .map(|(t, _)| *t)
        .collect();
    stale
        .into_iter()
        .filter_map(|t| pending.remove(&t).map(|(p, _)| p))
        .collect()
}

/// Long-lived task that times out uncorrelated rename `From` halves into
/// [`WatchEvent::Deleted`] events. Without it, a file moved out of the watched
/// tree on an otherwise quiet tree (no later fs event to trigger a sweep) would
/// linger buffered until the next event — its deletion reaching Drive only via
/// the slow safety-net reconcile. Exits once the receiver is gone.
async fn reap_renames(
    pending: SharedRenames,
    tx: mpsc::Sender<WatchEvent>,
    ttl: Duration,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        if tx.is_closed() {
            return;
        }
        let drained = {
            let mut guard = match pending.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            drain_stale_renames(&mut guard, Instant::now(), ttl)
        };
        for path in drained {
            if tx.send(WatchEvent::Deleted(path)).await.is_err() {
                return;
            }
        }
    }
}

/// What a local path resolves to for sync purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    /// A regular file (or, under [`SymlinkPolicy::Follow`], a symlink to one).
    File,
    /// A directory (or, under [`SymlinkPolicy::Follow`], a symlink to one).
    Dir,
}

/// Classify `path` (an entry under `root`) per the symlink `policy`, returning
/// the effective kind to sync, or `None` to skip.
///
/// Non-symlinks map straight through: a regular file → [`LocalKind::File`], a
/// directory → [`LocalKind::Dir`], anything else (FIFO, socket, device) →
/// `None`. A symlink is skipped under [`SymlinkPolicy::Skip`]; under
/// [`SymlinkPolicy::Follow`] it is resolved to its target via `canonicalize`
/// (which chases the whole chain and fails on broken / cyclic `ELOOP` links —
/// both correctly skipped) and then skipped if the target is a special file or
/// resolves **outside** `root` (an escape guard so a stray link can't pull in
/// unrelated files). This is the single source of truth shared by the live
/// watcher filter and the tree walkers in `reconcile`.
pub fn classify_local(path: &Path, root: &Path, policy: SymlinkPolicy) -> Option<LocalKind> {
    let lmeta = std::fs::symlink_metadata(path).ok()?;
    if lmeta.file_type().is_symlink() {
        if policy == SymlinkPolicy::Skip {
            return None;
        }
        let target = std::fs::canonicalize(path).ok()?;
        let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !target.starts_with(&canon_root) {
            tracing::info!(
                path = %path.display(),
                "skipping symlink whose target resolves outside the watched root"
            );
            return None;
        }
        let tmeta = std::fs::metadata(&target).ok()?;
        if tmeta.is_dir() {
            return Some(LocalKind::Dir);
        }
        if tmeta.is_file() {
            return Some(LocalKind::File);
        }
        return None;
    }
    if lmeta.is_dir() {
        Some(LocalKind::Dir)
    } else if lmeta.is_file() {
        Some(LocalKind::File)
    } else {
        None
    }
}

/// Returns `true` if the watcher should forward an event for `path`. A path that
/// no longer exists (a Delete event) is accepted — the reconciler decides what
/// to do. Otherwise the path must classify to a syncable file/dir under the
/// `symlinks` policy ([`classify_local`]). Files whose name matches one of the
/// user-configurable `watch.ignore_patterns` globs are rejected first — these
/// are typically editor / OS scratch files (`.foo.swp`, `.DS_Store`, …).
fn accept_local_file(path: &Path, ignore: &GlobSet, root: &Path, policy: SymlinkPolicy) -> bool {
    if let Some(name) = path.file_name()
        && ignore.is_match(Path::new(name))
    {
        tracing::debug!(
            path = %path.display(),
            "ignoring file (matches watch.ignore_patterns)"
        );
        return false;
    }
    // Path gone (likely a Delete) — don't filter it out; the reconciler handles
    // a Deleted on a path that no longer exists.
    if std::fs::symlink_metadata(path).is_err() {
        return true;
    }
    if classify_local(path, root, policy).is_some() {
        return true;
    }
    tracing::debug!(
        path = %path.display(),
        "ignoring path (symlink policy or non-regular file)"
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

    // --- classify_local: symlink policy (Skip vs Follow) + guards ------------

    #[test]
    fn classify_plain_file_and_dir_regardless_of_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("f.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        for policy in [SymlinkPolicy::Skip, SymlinkPolicy::Follow] {
            assert_eq!(
                classify_local(&root.join("f.txt"), root, policy),
                Some(LocalKind::File)
            );
            assert_eq!(
                classify_local(&root.join("d"), root, policy),
                Some(LocalKind::Dir)
            );
        }
    }

    #[test]
    fn classify_symlink_to_file_follows_or_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("target.txt"), b"payload").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(root.join("target.txt"), &link).unwrap();

        assert_eq!(classify_local(&link, root, SymlinkPolicy::Skip), None);
        assert_eq!(
            classify_local(&link, root, SymlinkPolicy::Follow),
            Some(LocalKind::File)
        );
    }

    #[test]
    fn classify_symlink_to_dir_follows_or_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("realdir")).unwrap();
        let link = root.join("linkdir");
        std::os::unix::fs::symlink(root.join("realdir"), &link).unwrap();

        assert_eq!(classify_local(&link, root, SymlinkPolicy::Skip), None);
        assert_eq!(
            classify_local(&link, root, SymlinkPolicy::Follow),
            Some(LocalKind::Dir)
        );
    }

    #[test]
    fn classify_skips_symlink_escaping_the_root_even_when_following() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"nope").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let link = root.join("escape.txt");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).unwrap();

        // Follow must NOT pull in a target that resolves outside the watched root.
        assert_eq!(classify_local(&link, root, SymlinkPolicy::Follow), None);
    }

    #[test]
    fn classify_skips_broken_symlink_when_following() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let link = root.join("dangling.txt");
        std::os::unix::fs::symlink(root.join("does-not-exist"), &link).unwrap();
        assert_eq!(classify_local(&link, root, SymlinkPolicy::Follow), None);
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
        let (_w, mut rx) =
            Watcher::start(tmp.path(), default_matcher(), SymlinkPolicy::Skip).unwrap();
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

        assert_eq!(
            convert_event(&e_from, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![]
        );
        assert_eq!(
            convert_event(&e_to, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![WatchEvent::Renamed {
                from: from.clone(),
                to: to.clone()
            }]
        );
        assert_eq!(
            convert_event(&e_both, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![]
        );
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
            convert_event(&e_to, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![WatchEvent::Created(to)]
        );
    }

    /// A move *out* of the tree is a lone `From`: buffered, no event emitted
    /// immediately (it still awaits a possible `To`), and TTL-draining later.
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
        assert_eq!(
            convert_event(&e_from, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![]
        );
        assert_eq!(pending.len(), 1, "the From half is buffered");
    }

    /// `drain_stale_renames` returns only the `From` halves older than the TTL
    /// and removes them, leaving fresh ones buffered.
    #[test]
    fn drain_stale_renames_returns_only_expired() {
        let mut pending: HashMap<usize, (PathBuf, Instant)> = HashMap::new();
        let now = Instant::now();
        let old = PathBuf::from("/x/old.txt");
        let fresh = PathBuf::from("/x/fresh.txt");
        pending.insert(1, (old.clone(), now - Duration::from_secs(10)));
        pending.insert(2, (fresh.clone(), now));

        let drained = drain_stale_renames(&mut pending, now, RENAME_CORRELATION_TTL);

        assert_eq!(drained, vec![old]);
        assert_eq!(pending.len(), 1, "fresh From stays buffered");
        assert!(pending.contains_key(&2));
    }

    /// A stale, never-correlated `From` (a move out of the tree) is surfaced as a
    /// `Deleted` when the next rename event triggers a sweep.
    #[test]
    fn convert_sweeps_expired_from_into_deleted() {
        let m = default_matcher();
        let root = Path::new("/x");
        let mut pending: HashMap<usize, (PathBuf, Instant)> = HashMap::new();
        let gone = root.join("gone.txt");
        // A From from 10s ago that never got a To (moved to trash).
        pending.insert(99, (gone.clone(), Instant::now() - Duration::from_secs(10)));

        // An unrelated fresh `From` arrives and triggers the sweep.
        let other = root.join("other.txt");
        let e_from = name_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            Some(1),
            &[&other],
        );
        let out = convert_event(&e_from, root, &m, SymlinkPolicy::Skip, &mut pending);

        assert_eq!(out, vec![WatchEvent::Deleted(gone)]);
        assert!(pending.contains_key(&1), "the fresh From is now buffered");
        assert!(!pending.contains_key(&99), "the expired From was drained");
    }

    /// On a quiet tree (no further fs events), the reaper task times an
    /// uncorrelated `From` out into a `Deleted` on its own.
    #[tokio::test]
    async fn reaper_times_out_uncorrelated_from_into_deleted() {
        let pending: SharedRenames = Arc::new(Mutex::new(HashMap::new()));
        let gone = PathBuf::from("/x/gone.txt");
        pending
            .lock()
            .unwrap()
            .insert(7, (gone.clone(), Instant::now()));

        let (tx, mut rx) = mpsc::channel::<WatchEvent>(8);
        // ttl = 0 → the entry is stale on the first tick.
        let task = tokio::spawn(reap_renames(
            pending.clone(),
            tx,
            Duration::ZERO,
            Duration::from_millis(10),
        ));

        let ev = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("reaper should emit within the timeout")
            .expect("channel open");
        assert_eq!(ev, WatchEvent::Deleted(gone));
        assert!(
            pending.lock().unwrap().is_empty(),
            "the drained entry is removed from the buffer"
        );
        task.abort();
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
        assert_eq!(
            convert_event(&e_from, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![]
        );
        assert_eq!(
            convert_event(&e_both, root, &m, SymlinkPolicy::Skip, &mut pending),
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
            convert_event(&create, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![WatchEvent::Created(p.clone())]
        );
        let modify = name_event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            None,
            &[&p],
        );
        assert_eq!(
            convert_event(&modify, root, &m, SymlinkPolicy::Skip, &mut pending),
            vec![WatchEvent::Modified(p.clone())]
        );
        let remove = name_event(
            EventKind::Remove(notify::event::RemoveKind::File),
            None,
            &[&p],
        );
        assert_eq!(
            convert_event(&remove, root, &m, SymlinkPolicy::Skip, &mut pending),
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
        let (_w, mut rx) =
            Watcher::start(tmp.path(), default_matcher(), SymlinkPolicy::Skip).unwrap();
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
        let (_w, mut rx) =
            Watcher::start(tmp.path(), default_matcher(), SymlinkPolicy::Skip).unwrap();
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

        let (_w, mut rx) =
            Watcher::start(tmp.path(), default_matcher(), SymlinkPolicy::Skip).unwrap();
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
