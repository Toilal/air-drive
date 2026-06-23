//! Shared registry of "operations the daemon is currently performing against
//! Drive" — used to suppress the spurious echo a `changes.list` poll would
//! otherwise produce.
//!
//! ## Race we close
//!
//! Without this registry, a local-modify event flows like:
//!
//! 1. `apply_local` enqueues an Upload op for `(item, remote_id=R)`.
//! 2. Dispatcher pulls the op, hashes the file, calls `engine.update(R, ...)`.
//! 3. Drive registers the change in its change feed.
//! 4. Dispatcher persists the new fingerprint to `sync_item.md5`.
//!
//! If the change poller fires between (3) and (4), it sees `R` on Drive with
//! the new md5 while `sync_item.md5` still holds the old value. The
//! reconciler treats it as a remote-side divergence and enqueues a Download
//! op — pure waste, since the local bytes already match. With this registry
//! the dispatcher marks `R` as in-flight before (2) and clears it after (4);
//! the poller's `apply_remote` short-circuits whenever the file id is in
//! the registry.
//!
//! ## Design note
//!
//! Use [`std::sync::Mutex`], not `tokio::sync::Mutex`. The guard is acquired
//! for one map mutation at a time and never held across an `await`. Going
//! async here would only buy us blocking semantics on lock contention, which
//! we don't need for a map behind a few-microseconds critical section.
//!
//! The map is **ref-counted** (`HashMap<id, usize>`), not a set: the same Drive
//! id can be marked by two overlapping ops (e.g. an Upload-update and a
//! RenameRemote). With a plain set the first guard to drop would clear the id
//! while the second op was still in flight, re-opening the echo window. The
//! count is incremented on `mark` and decremented on drop; the id is removed
//! only when it hits zero.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared "in-flight" registry. Cloneable handle backed by an `Arc<Mutex<_>>`.
#[derive(Clone, Debug, Default)]
pub struct InFlightOps {
    inner: Arc<Mutex<HashMap<String, usize>>>,
}

impl InFlightOps {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `remote_id` as in-flight and return a RAII guard that decrements its
    /// count when dropped. Held across the `engine.update / engine.upload` await
    /// so the poller can't observe the post-engine, pre-fingerprint window.
    #[must_use = "the guard clears the in-flight mark on drop; dropping it immediately defeats echo suppression"]
    pub fn mark(&self, remote_id: &str) -> InFlightGuard {
        // `Result::ok` swallows a poisoned mutex — we'd rather miss one
        // dedupe than kill the daemon over a recoverable poisoning. Same
        // reasoning in [`InFlightGuard::drop`].
        if let Ok(mut map) = self.inner.lock() {
            *map.entry(remote_id.to_owned()).or_insert(0) += 1;
        }
        InFlightGuard {
            inner: self.inner.clone(),
            remote_id: remote_id.to_owned(),
        }
    }

    /// `true` when the given Drive file ID is currently being modified by the
    /// daemon. Used by `apply_remote` to skip self-induced change events.
    pub fn contains(&self, remote_id: &str) -> bool {
        self.inner
            .lock()
            .map(|map| map.contains_key(remote_id))
            .unwrap_or(false)
    }
}

/// RAII guard returned by [`InFlightOps::mark`]. Decrements the entry's count
/// when this value goes out of scope (i.e. when the op completes, successfully
/// or not), removing the id once no op holds it.
pub struct InFlightGuard {
    inner: Arc<Mutex<HashMap<String, usize>>>,
    remote_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.inner.lock()
            && let std::collections::hash_map::Entry::Occupied(mut e) =
                map.entry(self.remote_id.clone())
        {
            if *e.get() <= 1 {
                e.remove();
            } else {
                *e.get_mut() -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_then_drop_clears_the_id() {
        let reg = InFlightOps::new();
        {
            let _g = reg.mark("R1");
            assert!(reg.contains("R1"));
        }
        assert!(!reg.contains("R1"));
    }

    #[test]
    fn multiple_marks_track_independently() {
        let reg = InFlightOps::new();
        let g1 = reg.mark("R1");
        let g2 = reg.mark("R2");
        assert!(reg.contains("R1") && reg.contains("R2"));
        drop(g1);
        assert!(!reg.contains("R1") && reg.contains("R2"));
        drop(g2);
        assert!(!reg.contains("R2"));
    }

    #[test]
    fn nested_marks_of_same_id_refcount() {
        let reg = InFlightOps::new();
        let g1 = reg.mark("R1");
        let g2 = reg.mark("R1"); // same id, overlapping op
        assert!(reg.contains("R1"));
        drop(g1);
        // Still in-flight: the second op holds it. A plain HashSet would have
        // cleared it here, re-opening the echo window.
        assert!(reg.contains("R1"));
        drop(g2);
        assert!(!reg.contains("R1"));
    }

    #[test]
    fn cloned_handle_observes_the_same_set() {
        let a = InFlightOps::new();
        let b = a.clone();
        let _g = a.mark("R1");
        assert!(b.contains("R1"));
    }
}
