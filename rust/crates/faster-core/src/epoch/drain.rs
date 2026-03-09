//! Lock-free drain list for deferred epoch-keyed callbacks.
//!
//! When the global epoch is bumped, a callback may be queued to execute
//! once no thread references the prior epoch. The [`DrainList`] stores
//! these deferred actions and executes them when the safe-to-reclaim
//! epoch advances past their tagged epoch.
//!
//! # Design Choice: Lock-Free Treiber Stack
//!
//! The previous `Mutex<Vec<>>` implementation serialized all threads on
//! push. Under heavy epoch drain (e.g., page eviction during compaction),
//! this mutex becomes a bottleneck. A Treiber stack eliminates contention
//! on the push path — each thread only needs a single successful CAS to
//! enqueue its callback.
//!
//! - **Push**: lock-free CAS loop on the head pointer. No ABA concern
//!   because nodes are heap-allocated and never recycled into the list.
//! - **Drain**: atomic swap of head to null (claims the entire chain),
//!   then a linear walk to partition ready vs. unready nodes.
//! - **Order**: the stack is LIFO, but we reverse the claimed chain
//!   before execution to preserve FIFO insertion order.

use crate::sync::{AtomicPtr, AtomicU64, Ordering};
use std::ptr;

/// A single deferred action in the lock-free drain list.
///
/// Heap-allocated via [`Box`], linked through raw `next` pointers.
/// Nodes are allocated on push and freed on drain (or when [`DrainList`]
/// is dropped). They are never recycled, so there is no ABA concern.
struct DrainNode {
    /// The epoch at which this action was queued (the prior epoch before
    /// the bump that created it).
    epoch: u64,
    /// The callback to execute when the epoch becomes safe.
    /// Wrapped in `Option` so we can `take()` it out for `FnOnce` consumption.
    action: Option<Box<dyn FnOnce() + Send>>,
    /// Next node in the stack (toward the bottom). Null for the last node.
    next: *mut DrainNode,
}

/// A lock-free queue of deferred callbacks keyed by epoch.
///
/// Implemented as a Treiber stack (LIFO linked list) with:
/// - **Push**: CAS loop on the head pointer (lock-free, minimal contention)
/// - **Drain**: atomic swap of head to null, then linear walk
///
/// # Memory Safety
///
/// Nodes are heap-allocated via `Box::into_raw` on push and reclaimed
/// via `Box::from_raw` on drain or drop. Each node is consumed exactly
/// once. The `Drop` impl walks any remaining nodes to prevent leaks.
pub(crate) struct DrainList {
    head: AtomicPtr<DrainNode>,
    /// Number of pending (not yet drained) actions. Used as a fast-path
    /// signal in `try_drain`: when zero, the expensive epoch table scan
    /// is skipped entirely.
    drain_count: AtomicU64,
}

// SAFETY: `DrainList` is safe to send between threads. The raw
// `*mut DrainNode` inside `AtomicPtr` is only accessed through atomic
// operations (CAS on push, swap on drain). Node ownership transfers
// cleanly: `Box::into_raw` on push, `Box::from_raw` on drain/drop.
unsafe impl Send for DrainList {}

// SAFETY: `DrainList` is safe to share between threads. Concurrent
// pushes are serialized by CAS on the head pointer. Drain atomically
// claims the entire list via swap, after which only the draining thread
// accesses the claimed nodes.
unsafe impl Sync for DrainList {}

impl DrainList {
    /// Creates an empty drain list.
    pub(crate) fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            drain_count: AtomicU64::new(0),
        }
    }

    // -------------------------------------------------------------------
    // Private unsafe helpers — all raw-pointer manipulation is here
    // -------------------------------------------------------------------

    /// CAS-loop to push a node onto the head of the Treiber stack.
    ///
    /// # Safety
    ///
    /// - `node` must be a valid, uniquely-owned heap pointer allocated via
    ///   `Box::into_raw(Box::new(DrainNode { .. }))`.
    /// - The caller must not access `node` after this call (ownership is
    ///   transferred to the list).
    unsafe fn link_and_cas_push(&self, node: *mut DrainNode) {
        loop {
            let head = self.head.load(Ordering::Acquire);
            // SAFETY: Caller guarantees `node` is valid and uniquely owned.
            // No other thread can see it until the CAS publishes it.
            unsafe { (*node).next = head };

            match self
                .head
                .compare_exchange_weak(head, node, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Walk a raw chain starting from `head`, reconstitute each node as a
    /// `Box<DrainNode>`, and return them in FIFO (insertion) order.
    ///
    /// The Treiber stack is LIFO, so this reverses the chain.
    ///
    /// # Safety
    ///
    /// - `head` must be non-null and point to a valid chain of `DrainNode`s
    ///   that were allocated via `Box::into_raw`.
    /// - The caller must have exclusive access to the entire chain (e.g.,
    ///   via an atomic swap of the head pointer).
    #[allow(clippy::vec_box)] // Intentional: reconstituting Box ownership from raw pointers
    unsafe fn claim_chain(head: *mut DrainNode) -> Vec<Box<DrainNode>> {
        // H1/C-4: Pre-allocate for common chain lengths to reduce
        // allocation overhead on the hot drain path.
        let mut nodes = Vec::with_capacity(16);
        let mut current = head;
        while !current.is_null() {
            // SAFETY: Caller guarantees each node in the chain is a valid
            // heap allocation from Box::into_raw. We read `next` before
            // reconstituting the Box (which would invalidate the pointer).
            let next = unsafe { (*current).next };
            // SAFETY: `current` is a valid node allocated via Box::into_raw
            // in push(). Exclusive access guaranteed by the caller.
            nodes.push(unsafe { Box::from_raw(current) });
            current = next;
        }
        nodes.reverse();
        nodes
    }

    // -------------------------------------------------------------------
    // Public API — safe wrappers over the private unsafe helpers
    // -------------------------------------------------------------------

    /// Enqueues a deferred action to execute when `safe_epoch >= epoch`.
    ///
    /// Lock-free: uses a CAS loop on the head pointer. Each push
    /// allocates a fresh heap node, so there is no ABA concern.
    pub(crate) fn push(&self, epoch: u64, action: Box<dyn FnOnce() + Send>) {
        let node = Box::into_raw(Box::new(DrainNode {
            epoch,
            action: Some(action),
            next: ptr::null_mut(),
        }));

        // SAFETY: `node` was just allocated via Box::into_raw above.
        // It is valid, uniquely owned, and not yet published.
        unsafe { self.link_and_cas_push(node) };
        self.drain_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Executes and removes all actions whose epoch is ≤ `safe_epoch`.
    ///
    /// Atomically swaps the head to null (claiming the entire chain),
    /// reverses it to restore FIFO insertion order, then partitions into
    /// ready and unready nodes. Ready callbacks are executed; unready
    /// nodes are pushed back for future drain passes.
    ///
    /// Callbacks are executed after the chain is fully claimed, so a
    /// callback that calls `push()` will not deadlock — it simply pushes
    /// to the (now-empty or partially-repopulated) head.
    ///
    /// # Performance (H1/C-4)
    ///
    /// The `Vec` uses a pre-allocated capacity hint (16 nodes) to reduce
    /// allocation overhead in typical cases. For high-throughput scenarios
    /// the allocator amortises cost over multiple drains.
    pub(crate) fn drain_up_to(&self, safe_epoch: u64) {
        trace_span!("epoch_drain");
        let head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);

        if head.is_null() {
            return;
        }

        // SAFETY: We atomically claimed the entire chain via swap (head
        // replaced with null). Each node was allocated via Box::into_raw
        // in push(). We have exclusive ownership of the claimed chain.
        let nodes = unsafe { Self::claim_chain(head) };

        let mut drained = 0u64;
        for mut node in nodes {
            if node.epoch <= safe_epoch {
                if let Some(action) = node.action.take() {
                    action();
                }
                drained += 1;
            } else {
                // Re-push unready nodes so they survive until a future drain.
                let raw = Box::into_raw(node);
                // SAFETY: `raw` was just obtained from Box::into_raw.
                // The node is valid and we have exclusive ownership.
                unsafe { self.link_and_cas_push(raw) };
            }
        }
        if drained > 0 {
            self.drain_count.fetch_sub(drained, Ordering::Relaxed);
        }
    }

    /// Returns `true` if there are pending (not yet drained) actions.
    ///
    /// O(1) check via atomic counter. In pure-read workloads drain_count
    /// stays at zero, letting `try_drain` skip the epoch table scan.
    #[inline]
    pub(crate) fn has_pending(&self) -> bool {
        self.drain_count.load(Ordering::Relaxed) > 0
    }

    /// Returns the number of pending (not yet drained) actions.
    ///
    /// Traverses the entire linked list — O(n). Only for use in
    /// single-threaded test contexts; not safe to call concurrently
    /// with `drain_up_to`.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        let mut count = 0;
        let mut current = self.head.load(Ordering::Acquire);
        while !current.is_null() {
            count += 1;
            // SAFETY: `current` is a valid node in the list, reachable
            // from head. We only read the `next` pointer (no mutation,
            // no ownership transfer). This is safe in single-threaded
            // test contexts where no concurrent drain can free nodes
            // underneath us.
            current = unsafe { (*current).next };
        }
        count
    }
}

impl Drop for DrainList {
    fn drop(&mut self) {
        // `get_mut`: exclusive access guaranteed by `&mut self`.
        let head = *self.head.get_mut();
        if head.is_null() {
            return;
        }
        // SAFETY: exclusive access via `&mut self` in drop. Each node
        // was allocated via `Box::into_raw` in `push` and has not been
        // freed. `claim_chain` reconstitutes them as `Box<DrainNode>`.
        let nodes = unsafe { Self::claim_chain(head) };
        drop(nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn push_and_drain_single() {
        let list = DrainList::new();
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        list.push(
            5,
            Box::new(move || {
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );

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
            list.push(
                epoch,
                Box::new(move || {
                    c.fetch_add(epoch, Ordering::Relaxed);
                }),
            );
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

        list.push(
            1,
            Box::new(move || {
                d.lock().unwrap().push("first");
            }),
        );

        let d = Arc::clone(&data);
        list.push(
            1,
            Box::new(move || {
                d.lock().unwrap().push("second");
            }),
        );

        list.drain_up_to(1);
        let result = data.lock().unwrap();
        // Insertion order preserved
        assert_eq!(&*result, &["first", "second"]);
    }
}
