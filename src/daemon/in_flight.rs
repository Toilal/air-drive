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
//! we don't need for a HashSet behind a few-microseconds critical section.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Shared "in-flight" registry. Cloneable handle backed by an `Arc<Mutex<_>>`.
#[derive(Clone, Debug, Default)]
pub struct InFlightOps {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl InFlightOps {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `remote_id` as in-flight and return a RAII guard that removes it
    /// when dropped. Held across the `engine.update / engine.upload` await so
    /// the poller can't observe the post-engine, pre-fingerprint window.
    pub fn mark(&self, remote_id: &str) -> InFlightGuard {
        // `Result::ok` swallows a poisoned mutex — we'd rather miss one
        // dedupe than kill the daemon over a recoverable poisoning. Same
        // reasoning in [`InFlightGuard::drop`].
        if let Ok(mut set) = self.inner.lock() {
            set.insert(remote_id.to_owned());
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
            .map(|set| set.contains(remote_id))
            .unwrap_or(false)
    }
}

/// RAII guard returned by [`InFlightOps::mark`]. Drops the entry from the
/// registry when this value goes out of scope (i.e. when the op completes,
/// successfully or not).
pub struct InFlightGuard {
    inner: Arc<Mutex<HashSet<String>>>,
    remote_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.inner.lock() {
            set.remove(&self.remote_id);
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
    fn cloned_handle_observes_the_same_set() {
        let a = InFlightOps::new();
        let b = a.clone();
        let _g = a.mark("R1");
        assert!(b.contains("R1"));
    }
}
