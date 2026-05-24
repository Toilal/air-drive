//! 200 ms event debounce per logical path.
//!
//! Why: editor saves are a typical source of bursts. `vim` for instance writes
//! through a tempfile + rename, producing `Create` + `Remove` + `Create` on the
//! same logical path within a few ms. Re-uploading three times would be wasteful
//! and would briefly leave the daemon's view of the file out of sync. The
//! debouncer waits 200 ms after the *last* event for a path before forwarding
//! its final state.
//!
//! Algorithm: a single tokio task owns a `HashMap<PathBuf, (event, deadline)>`.
//! On each incoming event, the entry's deadline is bumped to `now + window`.
//! A `sleep_until` of the earliest deadline wakes the task to drain ready
//! entries.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::watch::WatchEvent;

/// Default coalesce window — matches the spec's promise to react within 200 ms
/// of the last event in a burst.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(200);

/// Long-lived debounce task. Returns when the input channel closes (watcher
/// dropped). Drops the rest of any unflushed entries on close.
pub async fn run(
    mut rx: mpsc::Receiver<WatchEvent>,
    tx: mpsc::Sender<WatchEvent>,
    window: Duration,
) {
    let mut pending: HashMap<PathBuf, (WatchEvent, Instant)> = HashMap::new();
    loop {
        let next_deadline = pending.values().map(|(_, d)| *d).min();
        let sleep_target =
            next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(60));

        tokio::select! {
            biased;
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(ev) => {
                        let key = key_of(&ev);
                        pending.insert(key, (ev, Instant::now() + window));
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(sleep_target) => {
                let now = Instant::now();
                let due: Vec<PathBuf> = pending
                    .iter()
                    .filter_map(|(k, (_, d))| if *d <= now { Some(k.clone()) } else { None })
                    .collect();
                for k in due {
                    if let Some((ev, _)) = pending.remove(&k) {
                        if tx.send(ev).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Choose the key under which to store a debounced event.
///
/// For rename events we key on the **destination** path. Successive events on
/// the same path (e.g. `Created` then a quick `Modified`) coalesce because they
/// share the key.
fn key_of(ev: &WatchEvent) -> PathBuf {
    match ev {
        WatchEvent::Created(p) | WatchEvent::Modified(p) | WatchEvent::Deleted(p) => p.clone(),
        WatchEvent::Renamed { to, .. } => to.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coalesces_three_burst_events_to_one() {
        let (raw_tx, raw_rx) = mpsc::channel::<WatchEvent>(32);
        let (out_tx, mut out_rx) = mpsc::channel::<WatchEvent>(32);
        let task = tokio::spawn(run(raw_rx, out_tx, Duration::from_millis(50)));

        let p = PathBuf::from("/tmp/burst.txt");
        raw_tx.send(WatchEvent::Created(p.clone())).await.unwrap();
        raw_tx.send(WatchEvent::Deleted(p.clone())).await.unwrap();
        raw_tx.send(WatchEvent::Created(p.clone())).await.unwrap();

        let ev = tokio::time::timeout(Duration::from_millis(300), out_rx.recv())
            .await
            .expect("timeout")
            .expect("channel open");
        assert_eq!(ev, WatchEvent::Created(p.clone()));

        // No second event should arrive in the next 50 ms (only one final state).
        let extra = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(extra.is_err(), "got unexpected second event: {extra:?}");

        drop(raw_tx);
        let _ = task.await;
    }

    #[tokio::test]
    async fn distinct_paths_each_get_one_event() {
        let (raw_tx, raw_rx) = mpsc::channel::<WatchEvent>(32);
        let (out_tx, mut out_rx) = mpsc::channel::<WatchEvent>(32);
        let task = tokio::spawn(run(raw_rx, out_tx, Duration::from_millis(50)));

        let a = PathBuf::from("/tmp/a.txt");
        let b = PathBuf::from("/tmp/b.txt");
        raw_tx.send(WatchEvent::Created(a.clone())).await.unwrap();
        raw_tx.send(WatchEvent::Created(b.clone())).await.unwrap();

        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(
                tokio::time::timeout(Duration::from_millis(300), out_rx.recv())
                    .await
                    .expect("timeout")
                    .expect("channel open"),
            );
        }
        assert!(seen.iter().any(|e| e.path() == a));
        assert!(seen.iter().any(|e| e.path() == b));

        drop(raw_tx);
        let _ = task.await;
    }
}
