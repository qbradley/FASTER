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

use loom::sync::atomic::Ordering;
use loom::sync::Arc;
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
                entries: [AtomicU64::new(EMPTY), AtomicU64::new(EMPTY), AtomicU64::new(EMPTY)],
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
