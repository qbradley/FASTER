//! Drain list for deferred epoch-keyed callbacks.
//!
//! When the global epoch is bumped, a callback may be queued to execute
//! once no thread references the prior epoch. The [`DrainList`] stores
//! these deferred actions and executes them when the safe-to-reclaim
//! epoch advances past their tagged epoch.

use std::sync::Mutex;

/// A single deferred action associated with an epoch.
struct DrainAction {
    /// The epoch at which this action was queued (the prior epoch before
    /// the bump that created it).
    epoch: u64,
    /// The callback to execute when the epoch becomes safe.
    action: Box<dyn FnOnce() + Send>,
}

/// A thread-safe queue of deferred callbacks keyed by epoch.
///
/// # Design Choice: Mutex + Vec
///
/// A lock-free structure is unnecessary here because:
/// - `push` happens only on `bump_current_epoch` (relatively rare — not
///   on every read/write operation)
/// - `drain_up_to` happens on `try_drain` (also rare)
/// - The critical section is short (just Vec manipulation, no I/O)
///
/// Actions are collected under the lock, then executed after releasing it.
/// This prevents deadlocks if a callback calls `push()` or
/// `bump_current_epoch()`.
pub(crate) struct DrainList {
    actions: Mutex<Vec<DrainAction>>,
}

impl DrainList {
    /// Creates an empty drain list.
    pub(crate) fn new() -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
        }
    }

    /// Enqueues a deferred action to execute when `safe_epoch >= epoch`.
    pub(crate) fn push(&self, epoch: u64, action: Box<dyn FnOnce() + Send>) {
        let mut actions = self.actions.lock().expect("drain list lock poisoned");
        actions.push(DrainAction { epoch, action });
    }

    /// Executes and removes all actions whose epoch is ≤ `safe_epoch`.
    ///
    /// Actions are collected under the lock, then executed after releasing
    /// it. This prevents deadlocks if a callback enqueues more drain actions.
    /// Execution order preserves insertion order for actions at the same epoch.
    pub(crate) fn drain_up_to(&self, safe_epoch: u64) {
        let to_execute = {
            let mut actions = self.actions.lock().expect("drain list lock poisoned");
            if actions.is_empty() {
                return;
            }
            // Partition: extract ready actions, keep the rest.
            // drain(..) empties the vec; partition splits into two vecs.
            let (ready, remaining): (Vec<_>, Vec<_>) =
                actions.drain(..).partition(|a| a.epoch <= safe_epoch);
            *actions = remaining;
            ready
        };

        // Execute outside the lock, preserving insertion order.
        for action in to_execute {
            (action.action)();
        }
    }

    /// Returns the number of pending (not yet drained) actions.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.actions.lock().expect("drain list lock poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn push_and_drain_single() {
        let list = DrainList::new();
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        list.push(5, Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        }));

        assert_eq!(list.pending_count(), 1);

        // Epoch 4 — not safe yet
        list.drain_up_to(4);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert_eq!(list.pending_count(), 1);

        // Epoch 5 — now safe
        list.drain_up_to(5);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(list.pending_count(), 0);
    }

    #[test]
    fn drain_multiple_epochs() {
        let list = DrainList::new();
        let counter = Arc::new(AtomicU64::new(0));

        for epoch in 1..=5 {
            let c = Arc::clone(&counter);
            list.push(epoch, Box::new(move || {
                c.fetch_add(epoch, Ordering::Relaxed);
            }));
        }

        // Drain up to epoch 3: actions 1, 2, 3 fire
        list.drain_up_to(3);
        assert_eq!(counter.load(Ordering::Relaxed), 1 + 2 + 3);
        assert_eq!(list.pending_count(), 2);

        // Drain up to epoch 5: actions 4, 5 fire
        list.drain_up_to(5);
        assert_eq!(counter.load(Ordering::Relaxed), 1 + 2 + 3 + 4 + 5);
        assert_eq!(list.pending_count(), 0);
    }

    #[test]
    fn drain_empty_is_noop() {
        let list = DrainList::new();
        list.drain_up_to(100); // should not panic
        assert_eq!(list.pending_count(), 0);
    }

    #[test]
    fn callback_can_observe_shared_state() {
        let list = DrainList::new();
        let data = Arc::new(Mutex::new(Vec::new()));
        let d = Arc::clone(&data);

        list.push(1, Box::new(move || {
            d.lock().unwrap().push("first");
        }));

        let d = Arc::clone(&data);
        list.push(1, Box::new(move || {
            d.lock().unwrap().push("second");
        }));

        list.drain_up_to(1);
        let result = data.lock().unwrap();
        // Insertion order preserved
        assert_eq!(&*result, &["first", "second"]);
    }
}
