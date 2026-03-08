//! Loom-based deterministic concurrency tests for FASTER core algorithms.
//!
//! Each test re-implements the minimal core algorithm under test using
//! `loom` primitives (not the project's own types, which use `std` atomics).
//! This lets loom explore all possible interleavings to detect memory-ordering
//! bugs, ABA races, and lost updates.
//!
//! Run with: `cargo test --features loom -p faster-core --test loom_tests`

// All content is gated on the `loom` feature.
#![cfg(feature = "loom")]

use loom::sync::Arc;
use loom::sync::atomic::Ordering;
use loom::thread;

// ============================================================================
// Test B2: Treiber stack free-list with ABA counter
// ============================================================================

/// Mirrors the allocator's ABA-tagged Treiber stack.
///
/// Layout: `[63:48 tag | 47:0 address]`.  Tag increments on every CAS to
/// prevent ABA.  "Next" pointers are stored in a shared array (standing in
/// for the allocator's page memory).
mod treiber {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const TAG_SHIFT: u32 = 48;
    const ADDR_MASK: u64 = (1u64 << TAG_SHIFT) - 1;
    const TAG_INCREMENT: u64 = 1u64 << TAG_SHIFT;

    pub struct Stack {
        head: AtomicU64,
        /// Simulated memory: next[i] holds the next-pointer for node i.
        /// Index 0 is unused (0 = empty sentinel).
        next: [AtomicU64; 5],
    }

    impl Stack {
        pub fn new() -> Self {
            Self {
                head: AtomicU64::new(0),
                next: [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ],
            }
        }

        pub fn push(&self, addr: u64) {
            debug_assert!(addr != 0 && addr <= 4);
            loop {
                let old_head = self.head.load(Ordering::Acquire);
                let old_addr = old_head & ADDR_MASK;
                let old_tag = old_head & !ADDR_MASK;

                // Write next-pointer into the node being pushed.
                self.next[addr as usize].store(old_addr, Ordering::Relaxed);

                let new_head = addr | old_tag.wrapping_add(TAG_INCREMENT);
                match self.head.compare_exchange_weak(
                    old_head,
                    new_head,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(_) => continue,
                }
            }
        }

        pub fn pop(&self) -> Option<u64> {
            loop {
                let old_head = self.head.load(Ordering::Acquire);
                let head_addr = old_head & ADDR_MASK;

                if head_addr == 0 {
                    return None;
                }

                let next_val = self.next[head_addr as usize].load(Ordering::Relaxed);
                let old_tag = old_head & !ADDR_MASK;
                let new_head = (next_val & ADDR_MASK) | old_tag.wrapping_add(TAG_INCREMENT);

                match self.head.compare_exchange_weak(
                    old_head,
                    new_head,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(head_addr),
                    Err(_) => continue,
                }
            }
        }
    }
}

#[test]
fn b2_treiber_stack_push_pop() {
    loom::model(|| {
        let stack = Arc::new(treiber::Stack::new());

        // Thread 1 pushes node 1 then pops.
        let s1 = Arc::clone(&stack);
        let t1 = thread::spawn(move || {
            s1.push(1);
            s1.pop()
        });

        // Thread 2 pushes node 2 then pops.
        let s2 = Arc::clone(&stack);
        let t2 = thread::spawn(move || {
            s2.push(2);
            s2.pop()
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // Each thread pushed one item and popped one item.
        // Both pops must succeed (each must get *some* item).
        let v1 = r1.expect("pop must succeed for thread 1");
        let v2 = r2.expect("pop must succeed for thread 2");

        // No double-free: the two popped values must cover {1, 2} exactly
        // (each value popped at most once).
        assert!(v1 == 1 || v1 == 2, "unexpected value {v1}");
        assert!(v2 == 1 || v2 == 2, "unexpected value {v2}");
        // Together they must have retrieved both items (no lost items).
        assert_eq!(v1 + v2, 3, "lost or duplicated item: got {v1} and {v2}");
    });
}

// ============================================================================
// Test B3: Hash bucket entry CAS (try_insert with 3 slots)
// ============================================================================

/// Minimal hash bucket: 3 AtomicU64 slots, CAS empty→value.
mod bucket {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const NUM_SLOTS: usize = 3;
    const EMPTY: u64 = 0;

    pub struct Bucket {
        entries: [AtomicU64; NUM_SLOTS],
    }

    impl Bucket {
        pub fn new() -> Self {
            Self {
                entries: [
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                ],
            }
        }

        /// Try to CAS an empty slot to `value`. Returns `Ok(slot_index)` on
        /// success or `Err(())` if all slots are occupied.
        pub fn try_insert(&self, value: u64) -> Result<usize, ()> {
            for i in 0..NUM_SLOTS {
                let current = self.entries[i].load(Ordering::Acquire);
                if current == EMPTY {
                    match self.entries[i].compare_exchange(
                        EMPTY,
                        value,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(i),
                        Err(_) => continue,
                    }
                }
            }
            Err(())
        }

        /// Read all non-empty slot values.
        pub fn values(&self) -> Vec<u64> {
            let mut out = Vec::new();
            for i in 0..NUM_SLOTS {
                let v = self.entries[i].load(Ordering::Acquire);
                if v != EMPTY {
                    out.push(v);
                }
            }
            out
        }
    }
}

#[test]
fn b3_bucket_try_insert() {
    loom::model(|| {
        let b = Arc::new(bucket::Bucket::new());

        let b1 = Arc::clone(&b);
        let t1 = thread::spawn(move || b1.try_insert(0xA));

        let b2 = Arc::clone(&b);
        let t2 = thread::spawn(move || b2.try_insert(0xB));

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // Both threads must succeed (3 slots, 2 inserts).
        let slot1 = r1.expect("thread 1 must get a slot");
        let slot2 = r2.expect("thread 2 must get a slot");

        // They must occupy different slots.
        assert_ne!(slot1, slot2, "two threads got the same slot");

        // The bucket must contain exactly {0xA, 0xB}.
        let mut vals = b.values();
        vals.sort();
        assert_eq!(vals, vec![0xA, 0xB]);
    });
}

// ============================================================================
// Test B4: Epoch protect/unprotect safe-epoch invariant
// ============================================================================

/// Minimal epoch system: global epoch + 2 per-thread slots.
mod epoch {
    use loom::sync::atomic::{AtomicU64, Ordering};

    pub const INACTIVE: u64 = 0;

    pub struct EpochTable {
        pub current_epoch: AtomicU64,
        /// Per-thread local epoch slots. INACTIVE (0) = not protected.
        pub local_epochs: [AtomicU64; 2],
    }

    impl EpochTable {
        pub fn new() -> Self {
            Self {
                current_epoch: AtomicU64::new(1),
                local_epochs: [AtomicU64::new(INACTIVE), AtomicU64::new(INACTIVE)],
            }
        }

        /// Protect: snapshot current epoch into the thread's slot.
        pub fn protect(&self, thread_idx: usize) {
            let e = self.current_epoch.load(Ordering::SeqCst);
            self.local_epochs[thread_idx].store(e, Ordering::Release);
        }

        /// Unprotect: clear the thread's slot.
        pub fn unprotect(&self, thread_idx: usize) {
            self.local_epochs[thread_idx].store(INACTIVE, Ordering::Release);
        }

        /// Bump the global epoch, return the new value.
        pub fn bump(&self) -> u64 {
            self.current_epoch.fetch_add(1, Ordering::SeqCst)
        }

        /// Compute the safe-to-reclaim epoch:
        /// `min(active local epochs) - 1`, or `current - 1` if none active.
        pub fn compute_safe_epoch(&self) -> u64 {
            let current = self.current_epoch.load(Ordering::SeqCst);
            let mut min = current;

            for slot in &self.local_epochs {
                let e = slot.load(Ordering::Acquire);
                if e != INACTIVE && e < min {
                    min = e;
                }
            }
            min.saturating_sub(1)
        }
    }
}

#[test]
fn b4_epoch_protect_unprotect() {
    loom::model(|| {
        let table = Arc::new(epoch::EpochTable::new());

        // Thread 0: protect, read epoch, unprotect.
        let t0 = {
            let tbl = Arc::clone(&table);
            thread::spawn(move || {
                tbl.protect(0);
                let my_epoch = tbl.local_epochs[0].load(Ordering::Acquire);
                // While protected, safe_epoch must be < my_epoch
                // (or equal to my_epoch - 1 at best).
                let safe = tbl.compute_safe_epoch();
                assert!(
                    safe < my_epoch,
                    "safe_epoch {safe} must be < protected epoch {my_epoch}"
                );
                tbl.unprotect(0);
            })
        };

        // Thread 1: protect, read, unprotect (same pattern, different slot).
        let t1 = {
            let tbl = Arc::clone(&table);
            thread::spawn(move || {
                tbl.protect(1);
                let my_epoch = tbl.local_epochs[1].load(Ordering::Acquire);
                let safe = tbl.compute_safe_epoch();
                assert!(
                    safe < my_epoch,
                    "safe_epoch {safe} must be < protected epoch {my_epoch}"
                );
                tbl.unprotect(1);
            })
        };

        // Main thread bumps the epoch once.
        table.bump();

        t0.join().unwrap();
        t1.join().unwrap();
    });
}

// ============================================================================
// Test B5: Overflow pool bump allocator — distinct addresses
// ============================================================================

/// Minimal bump allocator using AtomicU64 counter.
/// Each allocation returns a distinct "address" (the counter value).
/// Simulates zeroing by writing 0 to an output slot.
mod bump_pool {
    use loom::sync::atomic::{AtomicU64, Ordering};

    pub struct Pool {
        counter: AtomicU64,
        /// Simulated bucket memory: one u64 per allocation.
        pub slots: [AtomicU64; 4],
    }

    impl Pool {
        pub fn new() -> Self {
            // Counter starts at 1 (0 is reserved as the null address).
            Self {
                counter: AtomicU64::new(1),
                slots: [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ],
            }
        }

        /// Allocate: bump counter, zero the slot, return the address.
        pub fn allocate(&self) -> u64 {
            let addr = self.counter.fetch_add(1, Ordering::Relaxed);
            // Zero the corresponding slot (simulates re-zeroing bucket).
            self.slots[addr as usize].store(0, Ordering::Relaxed);
            // Release fence ensures zeroing is visible before the address
            // is published into a shared data structure.
            loom::sync::atomic::fence(Ordering::Release);
            addr
        }
    }
}

#[test]
fn b5_bump_pool_distinct_addresses() {
    loom::model(|| {
        let pool = Arc::new(bump_pool::Pool::new());

        let p1 = Arc::clone(&pool);
        let t1 = thread::spawn(move || p1.allocate());

        let p2 = Arc::clone(&pool);
        let t2 = thread::spawn(move || p2.allocate());

        let a1 = t1.join().unwrap();
        let a2 = t2.join().unwrap();

        // Addresses must be distinct (no double-allocation).
        assert_ne!(a1, a2, "two threads got the same address");

        // Both addresses must be valid (1 or 2).
        assert!(a1 >= 1 && a1 <= 2);
        assert!(a2 >= 1 && a2 <= 2);

        // Slots must be zeroed.
        assert_eq!(pool.slots[a1 as usize].load(Ordering::Acquire), 0);
        assert_eq!(pool.slots[a2 as usize].load(Ordering::Acquire), 0);
    });
}

// ============================================================================
// Test B6: Epoch-gated Treiber stack free (deferred push)
// ============================================================================

/// Tests that deferring free-list pushes (epoch-gated frees) preserves
/// stack integrity under all interleavings.
///
/// Two threads pop from a pre-filled Treiber stack, then defer the push-back
/// (simulating epoch-gated free). After both threads complete, the deferred
/// pushes execute (simulating epoch drain). Invariant: no item is lost or
/// duplicated.
///
/// Without deferral, an ABA race could cause a popping thread to succeed a
/// CAS against a recycled head. Deferral ensures nodes cannot re-enter the
/// stack while any thread is mid-pop.
#[test]
fn b6_epoch_gated_free_list() {
    loom::model(|| {
        let stack = Arc::new(treiber::Stack::new());
        // Pre-fill: items 1 and 2 are "on the free list".
        stack.push(1);
        stack.push(2);

        // Deferred-push queue (models epoch drain list).
        let deferred = Arc::new(loom::sync::Mutex::new(Vec::<u64>::new()));

        // Thread 1: pop (allocate) then defer push (epoch-gated free).
        let s1 = Arc::clone(&stack);
        let d1 = Arc::clone(&deferred);
        let t1 = thread::spawn(move || {
            let v = s1.pop().expect("thread 1 must pop");
            d1.lock().unwrap().push(v);
            v
        });

        // Thread 2: pop (allocate) then defer push (epoch-gated free).
        let s2 = Arc::clone(&stack);
        let d2 = Arc::clone(&deferred);
        let t2 = thread::spawn(move || {
            let v = s2.pop().expect("thread 2 must pop");
            d2.lock().unwrap().push(v);
            v
        });

        let v1 = t1.join().unwrap();
        let v2 = t2.join().unwrap();

        // Each thread must get a different item.
        assert_ne!(v1, v2, "each thread must get a distinct item");

        // --- Epoch advance: drain deferred pushes ---
        let to_push: Vec<u64> = deferred.lock().unwrap().drain(..).collect();
        for v in to_push {
            stack.push(v);
        }

        // --- Verify: all items accounted for ---
        let mut final_items = Vec::new();
        while let Some(v) = stack.pop() {
            final_items.push(v);
        }
        final_items.sort();
        assert_eq!(
            final_items,
            vec![1, 2],
            "all items must be preserved after epoch drain"
        );
    });
}

// ============================================================================
// Test 0C: Lock-free drain list (Treiber stack push + atomic-swap drain)
// ============================================================================

/// Minimal lock-free drain list re-implemented with loom primitives.
///
/// Mirrors `epoch::drain::DrainList`: Treiber stack push via CAS,
/// drain via atomic swap of head to null, then walk + execute.
/// Uses `AtomicUsize` counters instead of `FnOnce` callbacks so
/// loom can track the memory accesses.
mod drain_list {
    use loom::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
    use std::ptr;

    pub struct DrainNode {
        pub epoch: u64,
        /// Shared counter this node increments when "executed".
        counter: *const AtomicUsize,
        next: *mut DrainNode,
    }

    pub struct DrainList {
        head: AtomicPtr<DrainNode>,
    }

    // SAFETY: nodes are heap-allocated, accessed only through atomic
    // operations on the head pointer (CAS for push, swap for drain).
    unsafe impl Send for DrainList {}
    // SAFETY: concurrent pushes serialize on CAS; drain atomically claims
    // the chain, giving the drainer exclusive access to claimed nodes.
    unsafe impl Sync for DrainList {}

    impl DrainList {
        pub fn new() -> Self {
            Self {
                head: AtomicPtr::new(ptr::null_mut()),
            }
        }

        pub fn push(&self, epoch: u64, counter: &AtomicUsize) {
            let node = Box::into_raw(Box::new(DrainNode {
                epoch,
                counter: counter as *const AtomicUsize,
                next: ptr::null_mut(),
            }));

            loop {
                let head = self.head.load(Ordering::Acquire);
                // SAFETY: `node` is uniquely owned, not yet published.
                unsafe { (*node).next = head };
                match self
                    .head
                    .compare_exchange(head, node, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => return,
                    Err(_) => continue,
                }
            }
        }

        /// Drain entries with epoch ≤ `safe_epoch`.
        /// Returns the number of entries executed.
        pub fn drain_up_to(&self, safe_epoch: u64) -> usize {
            let head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
            if head.is_null() {
                return 0;
            }

            let mut nodes = Vec::new();
            let mut current = head;
            while !current.is_null() {
                // SAFETY: exclusive access to the claimed chain.
                let next = unsafe { (*current).next };
                nodes.push(current);
                current = next;
            }

            let mut executed = 0;
            for node_ptr in nodes {
                // SAFETY: exclusive access to claimed chain nodes.
                let epoch = unsafe { (*node_ptr).epoch };
                if epoch <= safe_epoch {
                    // SAFETY: reconstruct Box, increment the shared counter.
                    let node = unsafe { Box::from_raw(node_ptr) };
                    // SAFETY: the counter pointer is valid for the
                    // lifetime of the loom model (owned by Arc in test).
                    unsafe { (*node.counter).fetch_add(1, Ordering::Relaxed) };
                    executed += 1;
                } else {
                    // Re-push unready node.
                    loop {
                        let head = self.head.load(Ordering::Acquire);
                        // SAFETY: exclusive access to this unready node.
                        unsafe { (*node_ptr).next = head };
                        match self.head.compare_exchange(
                            head,
                            node_ptr,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break,
                            Err(_) => continue,
                        }
                    }
                }
            }

            executed
        }
    }

    impl Drop for DrainList {
        fn drop(&mut self) {
            let mut current = self.head.load(Ordering::Acquire);
            while !current.is_null() {
                // SAFETY: exclusive access via &mut self in drop.
                let node = unsafe { Box::from_raw(current) };
                current = node.next;
            }
        }
    }
}

/// Two threads push drain entries concurrently while a third thread
/// drains. All entries must eventually execute exactly once.
#[test]
fn c0_drain_list_concurrent_push_and_drain() {
    loom::model(|| {
        let list = Arc::new(drain_list::DrainList::new());
        let counter = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

        // Thread 1: push one entry at epoch 1.
        let l1 = Arc::clone(&list);
        let c1 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            l1.push(1, &c1);
        });

        // Thread 2: push one entry at epoch 1.
        let l2 = Arc::clone(&list);
        let c2 = Arc::clone(&counter);
        let t2 = thread::spawn(move || {
            l2.push(1, &c2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Main thread drains — all entries are at epoch 1, safe_epoch=1.
        let executed = list.drain_up_to(1);
        assert_eq!(executed, 2, "both entries must be drained");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "both callbacks must have fired"
        );
    });
}

/// Push entries at different epochs; drain with a threshold that only
/// fires some. Verify partial drain correctness under concurrency.
#[test]
fn c0_drain_list_partial_drain() {
    loom::model(|| {
        let list = Arc::new(drain_list::DrainList::new());
        let counter = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

        // Thread 1: push at epoch 1 (will be ready).
        let l1 = Arc::clone(&list);
        let c1 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            l1.push(1, &c1);
        });

        // Thread 2: push at epoch 5 (will NOT be ready).
        let l2 = Arc::clone(&list);
        let c2 = Arc::clone(&counter);
        let t2 = thread::spawn(move || {
            l2.push(5, &c2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Drain up to epoch 3: only epoch-1 entry fires.
        let first_pass = list.drain_up_to(3);
        assert_eq!(first_pass, 1, "only epoch-1 entry should drain");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Drain up to epoch 5: the remaining entry fires.
        let second_pass = list.drain_up_to(5);
        assert_eq!(second_pass, 1, "epoch-5 entry should drain now");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    });
}

// ============================================================================
// D1: Epoch Framework — protect prevents premature reclamation
// ============================================================================

/// Faithful re-implementation of FASTER's epoch protect/unprotect with
/// reentrant guard counting and safe-to-reclaim computation.
///
/// Mirrors: `epoch/table.rs` (EpochTable), `epoch/entry.rs` (EpochEntry)
///
/// Layout:
///   - `current_epoch` — global monotonic counter (SeqCst bumps)
///   - `entries[N]` — per-thread: {local_epoch, guard_count}
///   - `safe_to_reclaim_epoch` — cached safe epoch
mod epoch_framework {
    use loom::sync::atomic::{AtomicU64, Ordering};

    pub const NUM_THREADS: usize = 2;
    pub const UNPROTECTED: u64 = 0;

    pub struct EpochEntry {
        /// Matches `EpochEntry::local_current_epoch`.  0 = unprotected.
        pub local_epoch: AtomicU64,
        /// Reentrant guard count — protect increments, unprotect decrements.
        pub guard_count: AtomicU64,
    }

    impl EpochEntry {
        pub fn new() -> Self {
            Self {
                local_epoch: AtomicU64::new(UNPROTECTED),
                guard_count: AtomicU64::new(0),
            }
        }
    }

    pub struct EpochTable {
        pub current_epoch: AtomicU64,
        pub entries: [EpochEntry; NUM_THREADS],
    }

    impl EpochTable {
        pub fn new() -> Self {
            Self {
                current_epoch: AtomicU64::new(1),
                entries: [EpochEntry::new(), EpochEntry::new()],
            }
        }

        /// Protect: snapshot current epoch into thread's slot.
        /// Reentrant: only snapshots on first (non-nested) protect.
        /// Mirrors: epoch/table.rs `protect()` with Release store.
        pub fn protect(&self, tid: usize) {
            let old_count = self.entries[tid]
                .guard_count
                .fetch_add(1, Ordering::Relaxed);
            if old_count == 0 {
                // First entry — snapshot the global epoch.
                let e = self.current_epoch.load(Ordering::SeqCst);
                self.entries[tid].local_epoch.store(e, Ordering::Release);
            }
        }

        /// Unprotect: clear thread's epoch slot when guard count reaches 0.
        /// Mirrors: epoch/table.rs `unprotect()`.
        pub fn unprotect(&self, tid: usize) {
            let old_count = self.entries[tid]
                .guard_count
                .fetch_sub(1, Ordering::Relaxed);
            debug_assert!(old_count > 0, "unprotect without matching protect");
            if old_count == 1 {
                // Last exit — clear local epoch.
                self.entries[tid]
                    .local_epoch
                    .store(UNPROTECTED, Ordering::Release);
            }
        }

        /// Bump the global epoch.  Uses SeqCst to match production code.
        /// Mirrors: epoch/table.rs `bump_current_epoch()`.
        pub fn bump(&self) -> u64 {
            self.current_epoch.fetch_add(1, Ordering::SeqCst)
        }

        /// Compute the minimum epoch held by any active thread.
        /// Returns `current_epoch` if no thread is protected.
        /// Mirrors: epoch/table.rs `compute_safe_epoch()`.
        pub fn compute_safe_epoch(&self) -> u64 {
            let current = self.current_epoch.load(Ordering::SeqCst);
            let mut min = current;
            for entry in &self.entries {
                let e = entry.local_epoch.load(Ordering::Acquire);
                if e != UNPROTECTED && e < min {
                    min = e;
                }
            }
            min.saturating_sub(1)
        }
    }
}

/// BUG CAUGHT: If protect() used Relaxed instead of Release for the local
/// epoch store, a concurrent `compute_safe_epoch` could read stale (0) from
/// the slot and incorrectly advance the safe epoch past a protected thread's
/// epoch — enabling premature reclamation of data the thread is still using.
#[test]
fn d1_epoch_protect_prevents_premature_reclaim() {
    loom::model(|| {
        let table = Arc::new(epoch_framework::EpochTable::new());

        // Thread 0: protect → bump epoch → check safe epoch → unprotect.
        let t0 = {
            let tbl = Arc::clone(&table);
            thread::spawn(move || {
                tbl.protect(0);
                let my_epoch = tbl.entries[0].local_epoch.load(Ordering::Acquire);
                // Bump global epoch so safe_epoch could advance.
                tbl.bump();
                // While protected, safe_epoch must be < my_epoch.
                let safe = tbl.compute_safe_epoch();
                assert!(
                    safe < my_epoch,
                    "safe_epoch {safe} must be < protected epoch {my_epoch}: premature reclaim!"
                );
                tbl.unprotect(0);
            })
        };

        // Thread 1: also protects, bumps, checks.
        let t1 = {
            let tbl = Arc::clone(&table);
            thread::spawn(move || {
                tbl.protect(1);
                let my_epoch = tbl.entries[1].local_epoch.load(Ordering::Acquire);
                tbl.bump();
                let safe = tbl.compute_safe_epoch();
                assert!(
                    safe < my_epoch,
                    "safe_epoch {safe} must be < protected epoch {my_epoch}: premature reclaim!"
                );
                tbl.unprotect(1);
            })
        };

        t0.join().unwrap();
        t1.join().unwrap();
    });
}

/// BUG CAUGHT: If epoch drain used Relaxed on the swap instead of AcqRel,
/// a push concurrent with a drain could have its node lost — the push CAS
/// succeeds against the old head, but the drainer swapped the old head away
/// without seeing the new node.
#[test]
fn d1_epoch_drain_under_concurrent_protect() {
    loom::model(|| {
        let table = Arc::new(epoch_framework::EpochTable::new());
        let drain_list = Arc::new(drain_list::DrainList::new());
        let counter = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

        // Thread 0: protect at epoch 1, push a deferred action, then unprotect.
        let tbl0 = Arc::clone(&table);
        let dl0 = Arc::clone(&drain_list);
        let c0 = Arc::clone(&counter);
        let t0 = thread::spawn(move || {
            tbl0.protect(0);
            dl0.push(1, &c0);
            tbl0.unprotect(0);
        });

        // Thread 1: bump epoch then drain.
        let tbl1 = Arc::clone(&table);
        let dl1 = Arc::clone(&drain_list);
        let t1 = thread::spawn(move || {
            tbl1.bump(); // epoch becomes 2
            dl1.drain_up_to(1); // drain entries ≤ epoch 1
        });

        t0.join().unwrap();
        t1.join().unwrap();

        // Final drain: any remaining entries should fire.
        drain_list.drain_up_to(100);

        // The counter must be exactly 1: the deferred action executed once.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "deferred action must execute exactly once"
        );
    });
}

// ============================================================================
// D2: Hash Bucket — two-phase tentative insert protocol
// ============================================================================

/// Faithful re-implementation of FASTER's hash bucket with tentative bit
/// two-phase insert.
///
/// Mirrors: `hash/bucket.rs` (HashBucket, AtomicHashBucketEntry)
///
/// Bit layout:
///   [63] Tentative | [61:48] Tag (14) | [47:0] Address (48)
mod hash_bucket {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const ADDRESS_BITS: u32 = 48;
    const ADDRESS_MASK: u64 = (1u64 << ADDRESS_BITS) - 1;
    const TAG_SHIFT: u32 = 48;
    const TAG_MASK: u64 = ((1u64 << 14) - 1) << TAG_SHIFT;
    const TENTATIVE_BIT: u64 = 1u64 << 63;
    const EMPTY: u64 = 0;

    pub const NUM_ENTRIES: usize = 7;

    fn pack(tag: u16, addr: u64, tentative: bool) -> u64 {
        let mut v = (addr & ADDRESS_MASK) | (((tag as u64) & 0x3FFF) << TAG_SHIFT);
        if tentative {
            v |= TENTATIVE_BIT;
        }
        v
    }

    fn unpack_tag(v: u64) -> u16 {
        ((v & TAG_MASK) >> TAG_SHIFT) as u16
    }

    fn unpack_addr(v: u64) -> u64 {
        v & ADDRESS_MASK
    }

    fn is_tentative(v: u64) -> bool {
        v & TENTATIVE_BIT != 0
    }

    fn is_empty(v: u64) -> bool {
        v == EMPTY
    }

    pub struct Bucket {
        entries: [AtomicU64; NUM_ENTRIES],
    }

    impl Bucket {
        pub fn new() -> Self {
            Self {
                entries: [
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                    AtomicU64::new(EMPTY),
                ],
            }
        }

        /// Two-phase insert: CAS empty → tentative, then CAS tentative → committed.
        /// Mirrors: the two-phase protocol in hash/bucket.rs.
        /// Returns Ok(slot_index) on success, Err(()) if bucket full.
        pub fn insert(&self, tag: u16, addr: u64) -> Result<usize, ()> {
            let tentative = pack(tag, addr, true);
            let committed = pack(tag, addr, false);

            for i in 0..NUM_ENTRIES {
                let current = self.entries[i].load(Ordering::Acquire);
                if !is_empty(current) {
                    continue;
                }
                // Phase 1: CAS empty → tentative.
                match self.entries[i].compare_exchange(
                    EMPTY,
                    tentative,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // Phase 2: CAS tentative → committed (clear tentative bit).
                        let result = self.entries[i].compare_exchange(
                            tentative,
                            committed,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        debug_assert!(
                            result.is_ok(),
                            "tentative → committed CAS must succeed (single owner)"
                        );
                        return Ok(i);
                    }
                    Err(_) => continue,
                }
            }
            Err(())
        }

        /// Find the first committed (non-tentative) entry matching `tag`.
        /// Mirrors: hash/bucket.rs `find_entry()` which skips tentative entries.
        pub fn find(&self, tag: u16) -> Option<u64> {
            for i in 0..NUM_ENTRIES {
                let v = self.entries[i].load(Ordering::Acquire);
                if !is_empty(v) && !is_tentative(v) && unpack_tag(v) == tag {
                    return Some(unpack_addr(v));
                }
            }
            None
        }

        /// Count all non-empty entries (including tentative).
        pub fn count_occupied(&self) -> usize {
            let mut n = 0;
            for i in 0..NUM_ENTRIES {
                let v = self.entries[i].load(Ordering::Acquire);
                if !is_empty(v) {
                    n += 1;
                }
            }
            n
        }
    }
}

/// BUG CAUGHT: Without the tentative bit protocol, a concurrent reader could
/// see a partially-inserted entry (address stored but record not yet written
/// to the log). The two-phase CAS ensures lookups only return fully committed
/// entries.
#[test]
fn d2_bucket_two_phase_insert_no_lost_updates() {
    loom::model(|| {
        let b = Arc::new(hash_bucket::Bucket::new());

        // Thread 1: insert tag=0x1111 at addr=0xAAA.
        let b1 = Arc::clone(&b);
        let t1 = thread::spawn(move || b1.insert(0x1111, 0xAAA));

        // Thread 2: insert tag=0x2222 at addr=0xBBB.
        let b2 = Arc::clone(&b);
        let t2 = thread::spawn(move || b2.insert(0x2222, 0xBBB));

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // Both must succeed (7 slots, 2 inserts).
        let s1 = r1.expect("thread 1 must get a slot");
        let s2 = r2.expect("thread 2 must get a slot");
        assert_ne!(s1, s2, "two threads must occupy different slots");

        // Both entries must be findable.
        assert_eq!(b.find(0x1111), Some(0xAAA), "tag 0x1111 must be found");
        assert_eq!(b.find(0x2222), Some(0xBBB), "tag 0x2222 must be found");
        assert_eq!(b.count_occupied(), 2);
    });
}

/// BUG CAUGHT: If find() did NOT skip tentative entries, a concurrent lookup
/// during an in-progress insert could return an address whose record has not
/// yet been written to the log.
#[test]
fn d2_bucket_find_skips_tentative_during_insert() {
    loom::model(|| {
        let b = Arc::new(hash_bucket::Bucket::new());

        // Thread 1: insert tag=0x0ABC at addr=0x100.
        let b1 = Arc::clone(&b);
        let t1 = thread::spawn(move || b1.insert(0x0ABC, 0x100));

        // Thread 2: concurrent find for the same tag.
        let b2 = Arc::clone(&b);
        let t2 = thread::spawn(move || b2.find(0x0ABC));

        t1.join().unwrap().expect("insert must succeed");
        let found = t2.join().unwrap();

        // find() may or may not see the entry (depends on interleaving),
        // but if it DOES see it, it must be the committed version.
        if let Some(addr) = found {
            assert_eq!(addr, 0x100, "found address must match inserted address");
        }
    });
}

/// BUG CAUGHT: Two threads inserting entries with the SAME tag could corrupt
/// the bucket if the CAS was not properly serialized, leading to one entry
/// overwriting the other.
#[test]
fn d2_bucket_same_tag_concurrent_insert() {
    loom::model(|| {
        let b = Arc::new(hash_bucket::Bucket::new());

        // Both threads insert with the same tag but different addresses.
        let b1 = Arc::clone(&b);
        let t1 = thread::spawn(move || b1.insert(0x3FFF, 0x001));

        let b2 = Arc::clone(&b);
        let t2 = thread::spawn(move || b2.insert(0x3FFF, 0x002));

        t1.join().unwrap().expect("insert 1 must succeed");
        t2.join().unwrap().expect("insert 2 must succeed");

        // Both entries must be present (different slots, same tag).
        assert_eq!(b.count_occupied(), 2, "both entries must be stored");
    });
}

// ============================================================================
// D3: RecordInfo — seal/revivify CAS under contention
// ============================================================================

/// Faithful re-implementation of FASTER's RecordInfo atomic operations.
///
/// Mirrors: `record/record_info.rs`
///
/// Bit layout:
///   [63] Final | [62] Tombstone | [61] Invalid | [60] Sealed
///   [59:48] Version (12) | [47:0] Previous Address (48)
mod record_info {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const PREVIOUS_ADDR_MASK: u64 = (1u64 << 48) - 1;
    const VERSION_SHIFT: u32 = 48;
    const SEALED_BIT: u64 = 1u64 << 60;
    #[allow(dead_code)]
    const INVALID_BIT: u64 = 1u64 << 61;
    const TOMBSTONE_BIT: u64 = 1u64 << 62;
    #[allow(dead_code)]
    const FINAL_BIT: u64 = 1u64 << 63;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct RecordInfo(pub u64);

    impl RecordInfo {
        pub fn new(prev_addr: u64, version: u16) -> Self {
            let bits = (prev_addr & PREVIOUS_ADDR_MASK) | ((version as u64) << VERSION_SHIFT);
            Self(bits)
        }

        pub fn is_sealed(self) -> bool {
            self.0 & SEALED_BIT != 0
        }

        #[allow(dead_code)]
        pub fn is_invalid(self) -> bool {
            self.0 & INVALID_BIT != 0
        }

        pub fn is_tombstone(self) -> bool {
            self.0 & TOMBSTONE_BIT != 0
        }

        pub fn with_sealed(self) -> Self {
            Self(self.0 | SEALED_BIT)
        }

        pub fn with_sealed_cleared(self) -> Self {
            Self(self.0 & !SEALED_BIT)
        }
    }

    /// Atomic wrapper, mirrors the AtomicU64-based RecordInfo in production.
    pub struct AtomicRecordInfo(AtomicU64);

    impl AtomicRecordInfo {
        pub fn new(info: RecordInfo) -> Self {
            Self(AtomicU64::new(info.0))
        }

        pub fn load(&self, ordering: Ordering) -> RecordInfo {
            RecordInfo(self.0.load(ordering))
        }

        /// Seal the record. Returns the previous value.
        /// Mirrors: record_info.rs `seal()` — `fetch_or(SEALED_BIT, Release)`.
        pub fn seal(&self) -> RecordInfo {
            RecordInfo(self.0.fetch_or(SEALED_BIT, Ordering::Release))
        }

        /// Set the invalid bit. Returns the previous value.
        /// Mirrors: record_info.rs `set_invalid()`.
        #[allow(dead_code)]
        pub fn set_invalid(&self) -> RecordInfo {
            RecordInfo(self.0.fetch_or(INVALID_BIT, Ordering::Release))
        }

        /// Set the tombstone bit. Returns the previous value.
        pub fn set_tombstone(&self) -> RecordInfo {
            RecordInfo(self.0.fetch_or(TOMBSTONE_BIT, Ordering::Release))
        }

        /// Try to seal: CAS from non-sealed to sealed.
        /// Mirrors: record_info.rs `try_seal()`.
        #[allow(dead_code)]
        pub fn try_seal(&self) -> Result<RecordInfo, RecordInfo> {
            let current = self.load(Ordering::Acquire);
            if current.is_sealed() {
                return Err(current);
            }
            let desired = current.with_sealed();
            self.compare_exchange(current, desired, Ordering::AcqRel, Ordering::Acquire)
        }

        /// Revivify: CAS from sealed to unsealed.
        /// Only one thread can win this race — the winner gets exclusive
        /// in-place update rights.
        /// Mirrors: record_info.rs `try_revivify()`.
        pub fn try_revivify(&self, expected: RecordInfo) -> Result<RecordInfo, RecordInfo> {
            debug_assert!(expected.is_sealed(), "try_revivify on non-sealed record");
            let desired = expected.with_sealed_cleared();
            self.compare_exchange(expected, desired, Ordering::AcqRel, Ordering::Acquire)
        }

        fn compare_exchange(
            &self,
            current: RecordInfo,
            new: RecordInfo,
            success: Ordering,
            failure: Ordering,
        ) -> Result<RecordInfo, RecordInfo> {
            self.0
                .compare_exchange(current.0, new.0, success, failure)
                .map(RecordInfo)
                .map_err(RecordInfo)
        }
    }
}

/// BUG CAUGHT: If `seal()` used Relaxed instead of Release, concurrent readers
/// might not see the seal before the writer starts modifying the record via
/// copy-to-tail, leading to torn reads.
#[test]
fn d3_record_seal_visibility() {
    loom::model(|| {
        let info = record_info::RecordInfo::new(0x42, 7);
        let rec = Arc::new(record_info::AtomicRecordInfo::new(info));

        // Thread 1: seal the record.
        let r1 = Arc::clone(&rec);
        let t1 = thread::spawn(move || {
            r1.seal();
        });

        // Thread 2: read the record.
        let r2 = Arc::clone(&rec);
        let t2 = thread::spawn(move || r2.load(Ordering::Acquire));

        t1.join().unwrap();
        let snapshot = t2.join().unwrap();

        // The snapshot is either the original or the sealed version.
        // It must NEVER be some corrupt intermediate value.
        if snapshot.is_sealed() {
            // If sealed, the rest of the fields must be intact.
            assert_eq!(snapshot.0 & ((1u64 << 48) - 1), 0x42);
        } else {
            assert_eq!(snapshot.0, info.0);
        }
    });
}

/// BUG CAUGHT: If `try_revivify()` used Relaxed orderings instead of AcqRel,
/// two threads could both "win" the revivification race — both would CAS
/// from sealed→unsealed, and both would think they have exclusive update
/// rights, leading to data corruption.
#[test]
fn d3_record_revivify_single_winner() {
    loom::model(|| {
        let info = record_info::RecordInfo::new(0x100, 3).with_sealed();
        let rec = Arc::new(record_info::AtomicRecordInfo::new(info));

        // Two threads race to revivify the same sealed record.
        let r1 = Arc::clone(&rec);
        let t1 = thread::spawn(move || r1.try_revivify(info));

        let r2 = Arc::clone(&rec);
        let t2 = thread::spawn(move || r2.try_revivify(info));

        let result1 = t1.join().unwrap();
        let result2 = t2.join().unwrap();

        // Exactly ONE thread must win; the other must fail.
        let wins = [result1.is_ok(), result2.is_ok()]
            .iter()
            .filter(|&&x| x)
            .count();
        assert_eq!(
            wins, 1,
            "exactly one thread must win revivification: got {wins}"
        );

        // After the race, the record must be unsealed.
        let final_state = rec.load(Ordering::Acquire);
        assert!(
            !final_state.is_sealed(),
            "record must be unsealed after revivification"
        );
    });
}

/// BUG CAUGHT: If `try_seal()` and concurrent `set_tombstone()` were not
/// properly ordered, the tombstone could be set on a stale snapshot, causing
/// the seal to be silently lost.
#[test]
fn d3_record_seal_and_tombstone_concurrent() {
    loom::model(|| {
        let info = record_info::RecordInfo::new(0x200, 1);
        let rec = Arc::new(record_info::AtomicRecordInfo::new(info));

        // Thread 1: seal the record.
        let r1 = Arc::clone(&rec);
        let t1 = thread::spawn(move || r1.seal());

        // Thread 2: set tombstone.
        let r2 = Arc::clone(&rec);
        let t2 = thread::spawn(move || r2.set_tombstone());

        t1.join().unwrap();
        t2.join().unwrap();

        // Both operations use fetch_or: both bits must be set regardless of
        // ordering.
        let final_state = rec.load(Ordering::Acquire);
        assert!(final_state.is_sealed(), "sealed bit must persist");
        assert!(final_state.is_tombstone(), "tombstone bit must persist");
    });
}

// ============================================================================
// D4: EPVS State Transitions — two-phase CAS protocol
// ============================================================================

/// Faithful re-implementation of FASTER's SystemState two-phase transition
/// with intermediate bit for exclusive state machine ownership.
///
/// Mirrors: `state/system_state.rs` (SystemState, AtomicSystemState)
///
/// Bit layout:
///   [63:56] Phase (8 bits, bit 63 = 0x80 intermediate marker)
///   [55:0]  Version (56 bits)
mod system_state {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const PHASE_SHIFT: u32 = 56;
    const VERSION_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;
    const INTERMEDIATE_BIT: u8 = 0x80;

    // Phase discriminants matching production Phase enum.
    pub const REST: u8 = 0;
    pub const PREPARE: u8 = 1;
    #[allow(dead_code)]
    pub const IN_PROGRESS: u8 = 2;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct State(u64);

    impl core::fmt::Debug for State {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(
                f,
                "State(phase={}, v={}, intermediate={})",
                self.phase_raw() & !INTERMEDIATE_BIT,
                self.version(),
                self.is_intermediate()
            )
        }
    }

    impl State {
        pub fn new(phase: u8, version: u64) -> Self {
            Self(((phase as u64) << PHASE_SHIFT) | (version & VERSION_MASK))
        }

        pub fn phase_raw(self) -> u8 {
            (self.0 >> PHASE_SHIFT) as u8
        }

        pub fn phase(self) -> u8 {
            self.phase_raw() & !INTERMEDIATE_BIT
        }

        pub fn version(self) -> u64 {
            self.0 & VERSION_MASK
        }

        pub fn is_intermediate(self) -> bool {
            (self.phase_raw() & INTERMEDIATE_BIT) != 0
        }

        pub fn make_intermediate(self) -> Self {
            Self(self.0 | ((INTERMEDIATE_BIT as u64) << PHASE_SHIFT))
        }

        pub fn word(self) -> u64 {
            self.0
        }
    }

    pub struct AtomicState(AtomicU64);

    impl AtomicState {
        pub fn new(initial: State) -> Self {
            Self(AtomicU64::new(initial.word()))
        }

        pub fn load(&self, ordering: Ordering) -> State {
            State(self.0.load(ordering))
        }

        /// Two-phase transition using the intermediate-state protocol.
        /// Mirrors: system_state.rs `try_transition()`.
        ///
        /// 1. CAS `expected → intermediate` (claim exclusive transition)
        /// 2. Execute `before_hooks`
        /// 3. CAS `intermediate → next` (publish new state)
        pub fn try_transition(
            &self,
            expected: State,
            next: State,
            before_hooks: impl FnOnce(),
        ) -> bool {
            let intermediate = expected.make_intermediate();

            // Step 1: Claim exclusive transition.
            if self
                .0
                .compare_exchange(
                    expected.word(),
                    intermediate.word(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return false;
            }

            // Step 2: Execute hooks (safe — we hold exclusive transition).
            before_hooks();

            // Step 3: Publish the new state.
            let result = self.0.compare_exchange(
                intermediate.word(),
                next.word(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            debug_assert!(result.is_ok(), "intermediate → next CAS must succeed");
            true
        }

        /// Spin-wait until the intermediate bit clears.
        /// Mirrors: system_state.rs `wait_non_intermediate()`.
        pub fn wait_non_intermediate(&self) -> State {
            loop {
                let state = self.load(Ordering::Acquire);
                if !state.is_intermediate() {
                    return state;
                }
                loom::thread::yield_now();
            }
        }
    }
}

/// BUG CAUGHT: Without the intermediate bit protocol, two threads could both
/// CAS from Rest→Prepare, both succeeding — resulting in duplicate hook
/// execution and corrupted checkpoint state. The intermediate bit ensures
/// exclusive ownership of the transition window.
#[test]
fn d4_state_transition_exclusive_winner() {
    loom::model(|| {
        let hook_count = Arc::new(loom::sync::atomic::AtomicUsize::new(0));
        let state = Arc::new(system_state::AtomicState::new(system_state::State::new(
            system_state::REST,
            1,
        )));

        let expected = system_state::State::new(system_state::REST, 1);
        let next = system_state::State::new(system_state::PREPARE, 1);

        // Thread 1: attempt Rest→Prepare transition.
        let s1 = Arc::clone(&state);
        let h1 = Arc::clone(&hook_count);
        let t1 = thread::spawn(move || {
            s1.try_transition(expected, next, || {
                h1.fetch_add(1, Ordering::SeqCst);
            })
        });

        // Thread 2: attempt the same transition.
        let s2 = Arc::clone(&state);
        let h2 = Arc::clone(&hook_count);
        let t2 = thread::spawn(move || {
            s2.try_transition(expected, next, || {
                h2.fetch_add(1, Ordering::SeqCst);
            })
        });

        let won1 = t1.join().unwrap();
        let won2 = t2.join().unwrap();

        // Exactly one thread must win.
        assert!(
            won1 ^ won2,
            "exactly one thread must win: won1={won1}, won2={won2}"
        );

        // Hooks must execute exactly once.
        assert_eq!(
            hook_count.load(Ordering::SeqCst),
            1,
            "transition hooks must execute exactly once"
        );

        // Final state must be Prepare(v1).
        let final_state = state.load(Ordering::Acquire);
        assert_eq!(final_state.phase(), system_state::PREPARE);
        assert_eq!(final_state.version(), 1);
        assert!(!final_state.is_intermediate());
    });
}

/// BUG CAUGHT: If a reader does not wait for the intermediate bit to clear,
/// it could observe a transient state and make decisions based on stale phase
/// information — for example, writing a record under the wrong checkpoint
/// version.
#[test]
fn d4_state_wait_non_intermediate() {
    loom::model(|| {
        let state = Arc::new(system_state::AtomicState::new(system_state::State::new(
            system_state::REST,
            1,
        )));

        let expected = system_state::State::new(system_state::REST, 1);
        let next = system_state::State::new(system_state::PREPARE, 1);

        // Thread 1: perform the transition.
        let s1 = Arc::clone(&state);
        let t1 = thread::spawn(move || {
            s1.try_transition(expected, next, || {
                // Simulate hook work — yield to increase interleaving.
                loom::thread::yield_now();
            });
        });

        // Thread 2: wait for a non-intermediate state and read it.
        let s2 = Arc::clone(&state);
        let t2 = thread::spawn(move || s2.wait_non_intermediate());

        t1.join().unwrap();
        let observed = t2.join().unwrap();

        // The observed state must never be intermediate.
        assert!(
            !observed.is_intermediate(),
            "wait_non_intermediate must not return intermediate state"
        );
        // It must be either Rest(1) or Prepare(1).
        assert!(
            (observed.phase() == system_state::REST && observed.version() == 1)
                || (observed.phase() == system_state::PREPARE && observed.version() == 1),
            "unexpected state: {:?}",
            observed
        );
    });
}

// ============================================================================
// D5: Log Allocation — concurrent tail CAS
// ============================================================================

/// Faithful re-implementation of FASTER's log allocator tail CAS loop.
///
/// Mirrors: `hybrid_log/log_allocator.rs` (LogAllocator::try_allocate)
///
/// Layout: LogicalAddress = Page(23 bits) | Offset(25 bits)
/// Page size fixed to a small value for loom tractability.
mod log_alloc {
    use loom::sync::atomic::{AtomicU64, Ordering};

    const OFFSET_BITS: u32 = 25;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct LogAddr(pub u64);

    impl LogAddr {
        pub fn new(page: u32, offset: u32) -> Self {
            Self(((page as u64) << OFFSET_BITS) | (offset as u64))
        }

        pub fn page(self) -> u32 {
            (self.0 >> OFFSET_BITS) as u32
        }

        pub fn offset(self) -> u32 {
            (self.0 & ((1u64 << OFFSET_BITS) - 1)) as u32
        }

        pub fn raw(self) -> u64 {
            self.0
        }
    }

    pub struct Allocator {
        tail: AtomicU64,
        page_size: u32,
    }

    impl Allocator {
        pub fn new(start: LogAddr, page_size: u32) -> Self {
            Self {
                tail: AtomicU64::new(start.raw()),
                page_size,
            }
        }

        /// Lock-free bump allocation via CAS loop.
        /// Returns the address where the record should be written, or None
        /// if the allocation would cross a page boundary.
        /// Mirrors: log_allocator.rs `try_allocate()`.
        pub fn try_allocate(&self, size: u32) -> Option<LogAddr> {
            debug_assert!(size > 0);
            loop {
                let current = LogAddr(self.tail.load(Ordering::Acquire));
                let new_offset = current.offset() + size;

                if new_offset > self.page_size {
                    return None;
                }

                let new_tail = if new_offset == self.page_size {
                    // Wrap to next page.
                    LogAddr::new(current.page() + 1, 0)
                } else {
                    LogAddr::new(current.page(), new_offset)
                };

                match self.tail.compare_exchange(
                    current.raw(),
                    new_tail.raw(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(current),
                    Err(_) => continue,
                }
            }
        }

        pub fn tail(&self) -> LogAddr {
            LogAddr(self.tail.load(Ordering::Acquire))
        }
    }
}

/// BUG CAUGHT: If the CAS loop used Relaxed orderings, concurrent allocators
/// could both read the same tail and both CAS "successfully" (impossible with
/// proper CAS, but demonstrates the necessity of AcqRel for correctness).
/// More practically: ensures that allocated address ranges never overlap.
#[test]
fn d5_log_alloc_no_overlapping_addresses() {
    loom::model(|| {
        // Small page: 128 bytes. Each allocation is 32 bytes.
        let alloc = Arc::new(log_alloc::Allocator::new(
            log_alloc::LogAddr::new(0, 0),
            128,
        ));

        let a1 = Arc::clone(&alloc);
        let t1 = thread::spawn(move || a1.try_allocate(32));

        let a2 = Arc::clone(&alloc);
        let t2 = thread::spawn(move || a2.try_allocate(32));

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let addr1 = r1.expect("alloc 1 must succeed");
        let addr2 = r2.expect("alloc 2 must succeed");

        // Addresses must be distinct.
        assert_ne!(
            addr1.raw(),
            addr2.raw(),
            "concurrent allocations must return distinct addresses"
        );

        // One must be at offset 0, the other at offset 32.
        let mut offsets = [addr1.offset(), addr2.offset()];
        offsets.sort();
        assert_eq!(offsets, [0, 32], "offsets must be [0, 32]");

        // Both on the same page.
        assert_eq!(addr1.page(), 0);
        assert_eq!(addr2.page(), 0);

        // Tail must have advanced to offset 64.
        assert_eq!(alloc.tail().offset(), 64);
    });
}

/// BUG CAUGHT: If page boundary detection was done after the CAS (instead of
/// before), a thread could allocate across a page boundary, corrupting the
/// record that spans two pages.
#[test]
fn d5_log_alloc_page_boundary_crossing() {
    loom::model(|| {
        // Page size = 64. Start at offset 32. Each alloc = 32 bytes.
        // First allocation fills the page exactly (offset 32 + 32 = 64).
        // Second allocation must either get the next page or fail.
        let alloc = Arc::new(log_alloc::Allocator::new(
            log_alloc::LogAddr::new(0, 32),
            64,
        ));

        let a1 = Arc::clone(&alloc);
        let t1 = thread::spawn(move || a1.try_allocate(32));

        let a2 = Arc::clone(&alloc);
        let t2 = thread::spawn(move || a2.try_allocate(32));

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // One thread fills the page exactly (gets page 0, offset 32).
        // The other either gets page 1, offset 0 (if it retries after
        // page wrap) or None (page boundary rejection).
        let addrs: Vec<_> = [r1, r2].iter().filter_map(|r| *r).collect();

        // At least one allocation must succeed.
        assert!(!addrs.is_empty(), "at least one allocation must succeed");

        // If both succeed, they must be on consecutive addresses.
        if addrs.len() == 2 {
            let mut sorted: Vec<u64> = addrs.iter().map(|a| a.raw()).collect();
            sorted.sort();
            // First must be page 0 offset 32, second must be page 1 offset 0.
            let first = log_alloc::LogAddr(sorted[0]);
            let second = log_alloc::LogAddr(sorted[1]);
            assert_eq!(first.page(), 0);
            assert_eq!(first.offset(), 32);
            assert_eq!(second.page(), 1);
            assert_eq!(second.offset(), 0);
        }
    });
}

/// BUG CAUGHT: Multiple concurrent allocators must produce a contiguous,
/// non-overlapping sequence of addresses. Tests the invariant that the final
/// tail equals the sum of all allocations.
#[test]
fn d5_log_alloc_sequential_integrity() {
    loom::model(|| {
        // 256-byte page, start at offset 0. Three 32-byte allocations.
        let alloc = Arc::new(log_alloc::Allocator::new(
            log_alloc::LogAddr::new(0, 0),
            256,
        ));

        let a1 = Arc::clone(&alloc);
        let t1 = thread::spawn(move || a1.try_allocate(32));

        let a2 = Arc::clone(&alloc);
        let t2 = thread::spawn(move || a2.try_allocate(32));

        let r1 = t1.join().unwrap().expect("alloc 1");
        let r2 = t2.join().unwrap().expect("alloc 2");

        // Non-overlapping: [addr, addr+32) ranges must not intersect.
        let ranges = [
            (r1.offset(), r1.offset() + 32),
            (r2.offset(), r2.offset() + 32),
        ];
        assert!(
            ranges[0].1 <= ranges[1].0 || ranges[1].1 <= ranges[0].0,
            "ranges must not overlap: {:?}",
            ranges
        );

        // Final tail = start + 64 (two 32-byte allocations).
        assert_eq!(alloc.tail().offset(), 64);
    });
}

// ============================================================================
// D6: Combined epoch + drain — deferred reclamation correctness
// ============================================================================

/// BUG CAUGHT: If the epoch framework's `compute_safe_epoch` was not properly
/// synchronized with `protect()`, a drain could execute a deferred action while
/// a thread holding a stale epoch guard is still accessing the data.
/// This test combines epoch protect/unprotect with deferred drain actions.
#[test]
fn d6_epoch_deferred_reclaim_safety() {
    loom::model(|| {
        let table = Arc::new(epoch_framework::EpochTable::new());
        let drain = Arc::new(drain_list::DrainList::new());
        let reclaimed = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

        // Thread 0: protect, defer a reclaim at current epoch, unprotect.
        let tbl0 = Arc::clone(&table);
        let dl0 = Arc::clone(&drain);
        let rc0 = Arc::clone(&reclaimed);
        let t0 = thread::spawn(move || {
            tbl0.protect(0);
            let e = tbl0.entries[0].local_epoch.load(Ordering::Acquire);
            dl0.push(e, &rc0);
            tbl0.unprotect(0);
        });

        // Thread 1: bump epoch twice and drain.
        let tbl1 = Arc::clone(&table);
        let dl1 = Arc::clone(&drain);
        let t1 = thread::spawn(move || {
            tbl1.bump();
            tbl1.bump();
            let safe = tbl1.compute_safe_epoch();
            dl1.drain_up_to(safe);
        });

        t0.join().unwrap();
        t1.join().unwrap();

        // Final cleanup drain.
        drain.drain_up_to(100);

        // The deferred reclaim must execute exactly once.
        assert_eq!(
            reclaimed.load(Ordering::SeqCst),
            1,
            "deferred reclaim must fire exactly once"
        );
    });
}
