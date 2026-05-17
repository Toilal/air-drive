//! Daemon orchestration: event loop, single-instance lock, control socket, shutdown.
//!
//! The Phase 4 [`run`] entry point ties together the watcher, the change poller,
//! the reconciler, and the op dispatcher behind a single
//! [`tokio_util::sync::CancellationToken`]. SIGTERM / SIGINT both flip the
//! token, the loops drain whatever's in flight, and the function returns
//! cleanly. Tests use this to validate the continuous-sync user story (T041–T049).

pub mod in_flight;
pub mod lock;
pub mod runtime;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::in_flight::InFlightOps;
use crate::drive::changes::{self, RemoteChange};
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
}

/// Run the daemon's continuous sync loop until `cancel` fires (SIGTERM/SIGINT or
/// an external trigger). Returns once every spawned task has drained.
pub async fn run(ctx: DaemonContext, cancel: CancellationToken) -> Result<()> {
    // Pre-flight: clear any stale partial downloads from a previous crash
    // (FR-010, T034b).
    staging::cleanup_orphans(&ctx.local_root)?;

    // Notifier the reconciler signals on every enqueue so the dispatcher
    // wakes immediately instead of waiting POLL_INTERVAL.
    let wake = Arc::new(Notify::new());

    // Shared "operations in progress" registry. The dispatcher marks a
    // Drive file id in-flight around every engine.upload/update call; the
    // remote-side reconciler short-circuits change events for ids in the
    // registry so we never enqueue a Download for the echo of our own write.
    let in_flight = InFlightOps::new();

    let (raw_tx, raw_rx) = mpsc::channel::<WatchEvent>(1024);
    let (debounced_tx, mut debounced_rx) = mpsc::channel::<WatchEvent>(1024);
    let (remote_tx, mut remote_rx) = mpsc::channel::<RemoteChange>(1024);

    // 1. Local watcher (notify) → raw events channel.
    let (_watcher_keepalive, watcher_rx) = watch::Watcher::start(&ctx.local_root)?;
    let raw_forwarder = forward_channel(watcher_rx, raw_tx, cancel.clone());

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
        let wake_local = wake.clone();
        let cancel_local = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_local.cancelled() => return,
                    maybe = debounced_rx.recv() => {
                        let Some(ev) = maybe else { return; };
                        if let Err(e) = continuous::apply_local(ev, &db, mapping_id, &local_root).await {
                            tracing::warn!(error = %e, "apply_local failed");
                        } else {
                            wake_local.notify_one();
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
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_remote.cancelled() => return,
                    maybe = remote_rx.recv() => {
                        let Some(change) = maybe else { return; };
                        if let Err(e) = continuous::apply_remote(change, &db, mapping_id, &local_root, &in_flight_remote).await {
                            tracing::warn!(error = %e, "apply_remote failed");
                        } else {
                            wake_remote.notify_one();
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
            )
            .await
            {
                tracing::error!(error = %e, "dispatcher exited with error");
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
    let _ = dispatcher_task.await;
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
