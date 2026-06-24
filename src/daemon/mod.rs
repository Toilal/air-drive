//! Daemon orchestration: event loop, single-instance lock, control socket, shutdown.
//!
//! The [`run`] entry point ties together the watcher, the change poller,
//! the reconciler, and the op dispatcher behind a single
//! [`tokio_util::sync::CancellationToken`]. SIGTERM / SIGINT both flip the
//! token, the loops drain whatever's in flight, and the function returns
//! cleanly. Tests use this to validate the continuous-sync user story.

pub mod file_status;
pub mod in_flight;
pub mod lock;
pub mod pause;
pub mod runtime;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::in_flight::InFlightOps;
use crate::daemon::pause::{PauseState, run_control_server};
use crate::drive::changes::{self, RemoteBatch};
use crate::drive::http::DriveHttp;
use crate::engine::SyncEngine;
use crate::engine::staging;
use crate::error::Result;
use crate::reconcile::continuous;
use crate::state::Db;
use crate::state::mapping::MappingId;
use crate::watch::{self, WatchEvent, debounce};

/// Inputs the daemon needs to start.
pub struct DaemonContext {
    /// State DB handle.
    pub db: Db,
    /// Sync engine (rclone or HTTP, picked at the CLI boundary).
    pub engine: Arc<dyn SyncEngine>,
    /// Drive REST client (shared by reconciler + dispatcher).
    pub http: DriveHttp,
    /// ID of the mapping row in `folder_mapping`.
    pub mapping_id: MappingId,
    /// Absolute local watched folder.
    pub local_root: PathBuf,
    /// Drive folder ID that maps to `local_root`.
    pub remote_root_id: String,
    /// Poll interval for `changes.list`, clamped to `[10, 60]` upstream.
    pub remote_poll_interval: Duration,
    /// Run the offline-local catch-up scan at startup. Set on a restart (a prior
    /// session converged the tree); `false` on the first sync, where the initial
    /// reconciliation already covered everything.
    pub catch_up_offline_local: bool,
    /// XDG runtime dir — where the control socket (`control.sock`) lives.
    pub runtime_dir: PathBuf,
    /// File-name glob patterns the watcher ignores (from `[watch].ignore_patterns`).
    pub watch_ignore_patterns: Vec<String>,
    /// How symlinks under the watched root are handled (from `[watch].symlinks`).
    pub symlinks: crate::config::SymlinkPolicy,
}

/// How long a tombstone — a trashed file's row kept so a restore re-links instead
/// of duplicating (#8) — is retained before the start-up GC reclaims it (30 days).
const TOMBSTONE_RETENTION_SECS: i64 = 30 * 24 * 3600;

/// Run the daemon's continuous sync loop until `cancel` fires (SIGTERM/SIGINT or
/// an external trigger). Returns once every spawned task has drained.
pub async fn run(ctx: DaemonContext, cancel: CancellationToken) -> Result<()> {
    // Pre-flight: clear any stale partial downloads from a previous crash.
    staging::cleanup_orphans(&ctx.local_root)?;

    // Pre-flight: reclaim tombstones older than the retention window so the
    // sync_item table doesn't grow unbounded with long-trashed files (#8).
    let cutoff = crate::state::unix_now() - TOMBSTONE_RETENTION_SECS;
    match crate::state::items::gc_tombstones(ctx.db.connection(), cutoff).await {
        Ok(n) if n > 0 => tracing::info!(reclaimed = n, "garbage-collected expired tombstones"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "tombstone GC failed; continuing"),
    }

    // Notifier the reconciler signals on every enqueue so the dispatcher
    // wakes immediately instead of waiting POLL_INTERVAL.
    let wake = Arc::new(Notify::new());

    // Shared "operations in progress" registry. The dispatcher marks a
    // Drive file id in-flight around every engine.upload/update call; the
    // remote-side reconciler short-circuits change events for ids in the
    // registry so we never enqueue a Download for the echo of our own write.
    let in_flight = InFlightOps::new();

    // Shared pause flag, manipulated by the control-socket server, read by
    // the dispatcher loop (which sleeps cooperatively on it).
    let pause_state = PauseState::new();

    // Sync-activity signal: pulsed whenever a file's state changes (a local or
    // remote event reconciled, or an op completed). The control socket's
    // `subscribe` command forwards it so the desktop overlay refreshes emblems
    // live. Bounded; a lagging subscriber just gets a coalesced refresh.
    let (activity_tx, _) = tokio::sync::broadcast::channel::<()>(64);

    let (raw_tx, raw_rx) = mpsc::channel::<WatchEvent>(1024);
    let (debounced_tx, mut debounced_rx) = mpsc::channel::<WatchEvent>(1024);
    let (remote_tx, mut remote_rx) = mpsc::channel::<RemoteBatch>(1024);

    // 1. Local watcher (notify) → raw events channel.
    let ignore_matcher = Arc::new(watch::build_ignore_matcher(&ctx.watch_ignore_patterns)?);
    let (_watcher_keepalive, watcher_rx) =
        watch::Watcher::start(&ctx.local_root, ignore_matcher, ctx.symlinks)?;
    let raw_forwarder = forward_channel(watcher_rx, raw_tx, cancel.clone());

    // 1b. Startup catch-up: replay any local change made while the daemon was
    //     stopped (inotify wasn't running then). The watcher is already live, so
    //     no concurrent change is lost; remote-side offline changes are recovered
    //     separately by the change poller from the persisted cursor. Skipped on
    //     the first sync, where the initial reconciliation already converged
    //     everything (running it there would just race that fresh state).
    if ctx.catch_up_offline_local {
        match crate::reconcile::startup_local_scan(
            &ctx.http,
            &ctx.db,
            ctx.mapping_id,
            &ctx.local_root,
            &ctx.watch_ignore_patterns,
            ctx.symlinks,
        )
        .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(replayed = n, "startup scan: replayed offline local changes"),
            Err(e) => tracing::warn!(error = %e, "startup local scan failed; continuing"),
        }
    }

    // 2. Debounce.
    let debounce_task = tokio::spawn(debounce::run(
        raw_rx,
        debounced_tx,
        debounce::DEFAULT_WINDOW,
    ));

    // 3. Reconcile local events.
    let reconcile_local_task = {
        let db = ctx.db.clone();
        let local_root = ctx.local_root.clone();
        let mapping_id = ctx.mapping_id;
        let symlinks = ctx.symlinks;
        let wake_local = wake.clone();
        let cancel_local = cancel.clone();
        let activity_local = activity_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_local.cancelled() => return,
                    maybe = debounced_rx.recv() => {
                        let Some(ev) = maybe else { return; };
                        if let Err(e) = continuous::apply_local(ev, &db, mapping_id, &local_root, symlinks).await {
                            tracing::warn!(error = %e, "apply_local failed");
                        } else {
                            wake_local.notify_one();
                            // A local change set an item pending → refresh emblems.
                            let _ = activity_local.send(());
                        }
                    }
                }
            }
        })
    };

    // 4. Drive change poller.
    let poller_task = {
        let http = ctx.http.clone();
        let db = ctx.db.clone();
        let tx = remote_tx.clone();
        let cancel_poll = cancel.clone();
        let mapping_id = ctx.mapping_id;
        let root_id = ctx.remote_root_id.clone();
        let interval = ctx.remote_poll_interval;
        tokio::spawn(async move {
            if let Err(e) =
                changes::run(http, db, mapping_id, root_id, tx, interval, cancel_poll).await
            {
                tracing::error!(error = %e, "change poller exited with error");
            }
        })
    };

    // 5. Reconcile remote events.
    let reconcile_remote_task = {
        let db = ctx.db.clone();
        let local_root = ctx.local_root.clone();
        let mapping_id = ctx.mapping_id;
        let wake_remote = wake.clone();
        let cancel_remote = cancel.clone();
        let in_flight_remote = in_flight.clone();
        let activity_remote = activity_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_remote.cancelled() => return,
                    maybe = remote_rx.recv() => {
                        let Some(batch) = maybe else { return; };
                        let mut all_applied = true;
                        let mut any_applied = false;
                        for change in batch.changes {
                            match continuous::apply_remote(change, &db, mapping_id, &local_root, &in_flight_remote).await {
                                Ok(()) => any_applied = true,
                                Err(e) => {
                                    all_applied = false;
                                    tracing::warn!(error = %e, "apply_remote failed; cursor will not advance past this batch");
                                }
                            }
                        }
                        if any_applied {
                            wake_remote.notify_one();
                            // Remote changes touched item states → refresh emblems.
                            let _ = activity_remote.send(());
                        }
                        // Advance the cursor ONLY when the whole batch applied —
                        // otherwise the poller re-fetches it next tick and the
                        // failed change is retried (apply_remote is idempotent on
                        // re-delivery) instead of being silently lost.
                        if all_applied {
                            if let Err(e) = crate::state::cursor::set(db.connection(), mapping_id, &batch.new_token, crate::state::unix_now()).await {
                                tracing::warn!(error = %e, "failed to persist new cursor");
                            }
                        }
                    }
                }
            }
        })
    };

    // 6. Op dispatcher.
    let dispatcher_task = {
        let db = ctx.db.clone();
        let engine = ctx.engine.clone();
        let http = ctx.http.clone();
        let local_root = ctx.local_root.clone();
        let remote_root_id = ctx.remote_root_id.clone();
        let wake_dispatch = wake.clone();
        let cancel_dispatch = cancel.clone();
        let in_flight_dispatch = in_flight.clone();
        let pause_dispatch = pause_state.clone();
        let activity_dispatch = activity_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = runtime::run(
                db,
                engine,
                http,
                local_root,
                remote_root_id,
                wake_dispatch,
                cancel_dispatch,
                in_flight_dispatch,
                pause_dispatch,
                activity_dispatch,
            )
            .await
            {
                tracing::error!(error = %e, "dispatcher exited with error");
            }
        })
    };

    // 6b. Control-socket server (pause / resume / status-snapshot / status-path).
    let control_task = {
        let cancel_ctl = cancel.clone();
        let state = pause_state.clone();
        let runtime_dir = ctx.runtime_dir.clone();
        let db = ctx.db.clone();
        let mapping_id = ctx.mapping_id;
        let local_root = ctx.local_root.clone();
        let activity_ctl = activity_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_control_server(
                runtime_dir,
                state,
                db,
                mapping_id,
                local_root,
                activity_ctl,
                cancel_ctl,
            )
            .await
            {
                tracing::error!(error = %e, "control socket server exited with error");
            }
        })
    };

    // 7. Signal handler. SIGTERM and SIGINT flip the cancellation token.
    let signal_task = {
        let cancel_signal = cancel.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("shutdown signal received");
            cancel_signal.cancel();
        })
    };

    // Wait for all tasks. The signal task may complete first (triggers cancel);
    // the others drain and return after.
    let _ = signal_task.await;
    let _ = raw_forwarder.await;
    let _ = debounce_task.await;
    let _ = reconcile_local_task.await;
    let _ = poller_task.await;
    let _ = reconcile_remote_task.await;
    // The dispatcher may be mid-transfer (an rclone subprocess). All loops are
    // cancel-guarded between ops, so it returns promptly when idle; bound the
    // wait so an in-flight transfer can't hold shutdown open indefinitely. On
    // timeout, abort the task — dropping its future kills the rclone child
    // (`kill_on_drop`); the op stays `pending` and re-runs (idempotently) next
    // start.
    let mut dispatcher_task = dispatcher_task;
    if tokio::time::timeout(Duration::from_secs(20), &mut dispatcher_task)
        .await
        .is_err()
    {
        tracing::warn!("dispatcher still draining at shutdown — aborting in-flight transfer");
        dispatcher_task.abort();
    }
    let _ = control_task.await;
    Ok(())
}

/// Spawn a small task that pumps events from the watcher's mpsc into the
/// debouncer's mpsc. Pure plumbing — the watcher returns its own channel
/// because `notify` doesn't know about tokio.
fn forward_channel(
    mut rx: mpsc::Receiver<WatchEvent>,
    tx: mpsc::Sender<WatchEvent>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                maybe = rx.recv() => {
                    let Some(ev) = maybe else { return; };
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

/// Wait for either `SIGTERM` or `SIGINT`. Returns as soon as one fires.
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install SIGTERM handler");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install SIGINT handler");
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}
