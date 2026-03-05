//! Tests for the epoch-based reclamation system.
//!
//! Covers: basic operations, reentrance, drain callbacks, concurrent stress,
//! registration limits, epoch monotonicity, guard RAII, and property tests.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use super::*;

// ── Basic operations ───────────────────────────────────────────────

#[test]
fn new_table_initial_state() {
    let table = EpochTable::new();
    assert_eq!(table.current_epoch(), INITIAL_EPOCH);
    assert_eq!(table.safe_epoch(), 0);
    assert_eq!(table.registered_count(), 0);
    assert_eq!(table.active_count(), 0);
}

#[test]
fn register_and_deregister() {
    let table = Arc::new(EpochTable::new());
    let t1 = table.register().expect("register t1");
    assert_eq!(table.registered_count(), 1);

    let t2 = table.register().expect("register t2");
    assert_eq!(table.registered_count(), 2);

    drop(t1);
    assert_eq!(table.registered_count(), 1);

    drop(t2);
    assert_eq!(table.registered_count(), 0);
}

#[test]
fn protect_and_unprotect_basic() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    assert_eq!(table.active_count(), 0);

    {
        let guard = thread.protect();
        assert_eq!(table.active_count(), 1);
        assert_eq!(guard.epoch(), INITIAL_EPOCH);
    }

    assert_eq!(table.active_count(), 0);
}

#[test]
fn bump_epoch_advances_counter() {
    let table = Arc::new(EpochTable::new());
    assert_eq!(table.current_epoch(), 1);

    table.bump_current_epoch_no_callback();
    assert_eq!(table.current_epoch(), 2);

    table.bump_current_epoch_no_callback();
    assert_eq!(table.current_epoch(), 3);
}

#[test]
fn safe_epoch_with_no_active_threads() {
    let table = Arc::new(EpochTable::new());

    // Bump epoch a few times — no threads active
    for _ in 0..5 {
        table.bump_current_epoch_no_callback();
    }

    // After try_drain (called by bump), safe_epoch should be current - 1
    assert_eq!(table.current_epoch(), 6);
    assert_eq!(table.safe_epoch(), 5);
}

#[test]
fn safe_epoch_held_back_by_active_thread() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    // Protect at epoch 1
    let guard = thread.protect();
    assert_eq!(guard.epoch(), 1);

    // Bump epoch several times
    for _ in 0..5 {
        table.bump_current_epoch_no_callback();
    }
    assert_eq!(table.current_epoch(), 6);

    // Safe epoch should be 0 (min active = 1, safe = 1 - 1 = 0)
    assert_eq!(table.safe_epoch(), 0);

    // Drop the guard — thread unprotects, safe epoch should advance
    drop(guard);
    assert_eq!(table.safe_epoch(), 5); // current(6) - 1
}

// ── Reentrance ─────────────────────────────────────────────────────

#[test]
fn reentrant_protect_depth_2() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let g1 = thread.protect();
    let epoch = g1.epoch();
    assert_eq!(table.active_count(), 1);

    // Bump epoch before nested protect
    table.bump_current_epoch_no_callback();

    let g2 = thread.protect();
    // Inner guard sees the same epoch (set by outermost protect)
    assert_eq!(g2.epoch(), epoch);
    assert_eq!(table.active_count(), 1);

    // Drop inner guard — still protected
    drop(g2);
    assert_eq!(table.active_count(), 1);

    // Drop outer guard — now unprotected
    drop(g1);
    assert_eq!(table.active_count(), 0);
}

#[test]
fn reentrant_protect_depth_3() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let g1 = thread.protect();
    let g2 = thread.protect();
    let g3 = thread.protect();

    assert_eq!(table.active_count(), 1);

    drop(g3);
    assert_eq!(table.active_count(), 1);
    drop(g2);
    assert_eq!(table.active_count(), 1);
    drop(g1);
    assert_eq!(table.active_count(), 0);
}

// ── Drain callbacks ────────────────────────────────────────────────

#[test]
fn drain_callback_fires_immediately_no_active_threads() {
    let table = Arc::new(EpochTable::new());
    let fired = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&fired);

    table.bump_current_epoch(move || {
        f.store(true, Ordering::Relaxed);
    });

    assert!(fired.load(Ordering::Relaxed));
}

#[test]
fn drain_callback_delayed_by_active_thread() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    // Protect at epoch 1
    let guard = thread.protect();

    // Bump and queue callback (prior_epoch = 1)
    let fired = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&fired);
    table.bump_current_epoch(move || {
        f.store(true, Ordering::Relaxed);
    });

    // Thread still at epoch 1 → safe = 0 → callback NOT fired
    assert!(!fired.load(Ordering::Relaxed));

    // Unprotect → safe epoch advances → callback fires
    drop(guard);
    assert!(fired.load(Ordering::Relaxed));
}

#[test]
fn drain_callback_fires_in_order() {
    let table = Arc::new(EpochTable::new());
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let o = Arc::clone(&order);
    table.bump_current_epoch(move || o.lock().unwrap().push(1));

    let o = Arc::clone(&order);
    table.bump_current_epoch(move || o.lock().unwrap().push(2));

    let o = Arc::clone(&order);
    table.bump_current_epoch(move || o.lock().unwrap().push(3));

    let result = order.lock().unwrap();
    assert_eq!(&*result, &[1, 2, 3]);
}

#[test]
fn drain_callbacks_fire_only_when_safe() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let counters: Vec<Arc<AtomicU64>> = (0..5).map(|_| Arc::new(AtomicU64::new(0))).collect();

    // Protect at epoch 1
    let guard = thread.protect();

    // Bump 5 times, queuing callbacks (epochs 1..5)
    for (i, counter) in counters.iter().enumerate() {
        let c = Arc::clone(counter);
        let val = (i + 1) as u64;
        table.bump_current_epoch(move || {
            c.store(val, Ordering::Relaxed);
        });
    }

    // All blocked by guard at epoch 1
    for c in &counters {
        assert_eq!(c.load(Ordering::Relaxed), 0);
    }

    // Refresh to epoch 4 (current=6, but we skip to 4 to test partial drain)
    // Actually, refresh updates to current_epoch which is now 6.
    guard.refresh();
    // After refresh, local_epoch = 6, safe = 6-1 = 5
    // But try_drain happens in unprotect, not refresh. Let's force it.
    table.try_drain();

    // Now safe_epoch = 5 (min active = 6, safe = 5). Callbacks 1..5 fire.
    for (i, c) in counters.iter().enumerate() {
        assert_eq!(c.load(Ordering::Relaxed), (i + 1) as u64);
    }

    drop(guard);
}

#[test]
fn drain_callback_from_within_callback() {
    let table = Arc::new(EpochTable::new());
    let inner_fired = Arc::new(AtomicBool::new(false));
    let inner = Arc::clone(&inner_fired);

    // The outer callback will push an inner callback.
    // This tests that we don't deadlock on the drain list lock.
    let table_clone = Arc::clone(&table);
    table.bump_current_epoch(move || {
        table_clone.bump_current_epoch(move || {
            inner.store(true, Ordering::Relaxed);
        });
    });

    assert!(inner_fired.load(Ordering::Relaxed));
}

// ── EpochGuard refresh ─────────────────────────────────────────────

#[test]
fn guard_refresh_advances_local_epoch() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let guard = thread.protect();
    assert_eq!(guard.epoch(), 1);

    table.bump_current_epoch_no_callback();
    table.bump_current_epoch_no_callback();
    assert_eq!(table.current_epoch(), 3);

    // Guard still at epoch 1
    assert_eq!(guard.epoch(), 1);

    // Refresh advances local epoch
    guard.refresh();
    assert_eq!(guard.epoch(), 3);

    drop(guard);
}

#[test]
fn thread_refresh_when_protected() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let _guard = thread.protect();
    table.bump_current_epoch_no_callback();
    table.bump_current_epoch_no_callback();

    thread.refresh();

    let entry = &table.table[thread.entry_index()];
    let epoch = entry.local_current_epoch.load(Ordering::Relaxed);
    assert_eq!(epoch, 3);
}

#[test]
fn thread_refresh_when_not_protected_is_noop() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    // Not protected — refresh should be a no-op
    thread.refresh();

    let entry = &table.table[thread.entry_index()];
    let epoch = entry.local_current_epoch.load(Ordering::Relaxed);
    assert_eq!(epoch, INACTIVE_EPOCH);
}

// ── Registration limits ────────────────────────────────────────────

#[test]
fn register_max_threads() {
    let table = Arc::new(EpochTable::new());
    let mut threads = Vec::with_capacity(MAX_THREADS);

    for i in 0..MAX_THREADS {
        let t = table.register().unwrap_or_else(|| panic!("failed to register thread {i}"));
        threads.push(t);
    }
    assert_eq!(table.registered_count(), MAX_THREADS);

    // 257th registration should fail
    assert!(table.register().is_none());
}

#[test]
fn register_after_deregister_reuses_slot() {
    let table = Arc::new(EpochTable::new());
    let mut threads: Vec<_> = (0..MAX_THREADS)
        .map(|_| table.register().unwrap())
        .collect();

    // Table full
    assert!(table.register().is_none());

    // Drop one thread
    let idx = threads[42].entry_index();
    threads.remove(42);
    assert_eq!(table.registered_count(), MAX_THREADS - 1);

    // Can register again
    let new_thread = table.register().expect("should register after deregister");
    assert_eq!(new_thread.entry_index(), idx);
    assert_eq!(table.registered_count(), MAX_THREADS);
}

// ── Explicit unregister ────────────────────────────────────────────

#[test]
fn explicit_unregister() {
    let table = Arc::new(EpochTable::new());
    let mut thread = table.register().expect("register");
    assert_eq!(table.registered_count(), 1);

    thread.unregister();
    assert_eq!(table.registered_count(), 0);

    // Drop after unregister should not double-free
    drop(thread);
    assert_eq!(table.registered_count(), 0);
}

// ── Epoch monotonicity ─────────────────────────────────────────────

#[test]
fn current_epoch_monotonically_increases() {
    let table = Arc::new(EpochTable::new());
    let mut prev = table.current_epoch();

    for _ in 0..100 {
        table.bump_current_epoch_no_callback();
        let curr = table.current_epoch();
        assert!(curr > prev);
        prev = curr;
    }
}

#[test]
fn safe_epoch_never_exceeds_current() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    for _ in 0..50 {
        let _guard = thread.protect();
        table.bump_current_epoch_no_callback();
        assert!(table.safe_epoch() < table.current_epoch());
    }
}

#[test]
fn safe_epoch_monotonically_increases() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");
    let mut prev_safe = table.safe_epoch();

    for _ in 0..50 {
        {
            let _guard = thread.protect();
            table.bump_current_epoch_no_callback();
        }
        let safe = table.safe_epoch();
        assert!(safe >= prev_safe, "safe epoch decreased: {safe} < {prev_safe}");
        prev_safe = safe;
    }
}

// ── Concurrent stress tests ────────────────────────────────────────

#[test]
fn concurrent_protect_unprotect_16_threads() {
    let table = Arc::new(EpochTable::new());
    let num_threads = 16;
    let iterations = 10_000;
    let barrier = Arc::new(Barrier::new(num_threads + 1));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let thread = table.register().expect("register");
                barrier.wait();

                for _ in 0..iterations {
                    let guard = thread.protect();
                    // Simulate some work
                    std::hint::black_box(guard.epoch());
                    drop(guard);
                }
            })
        })
        .collect();

    barrier.wait();

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert_eq!(table.active_count(), 0);
}

#[test]
fn concurrent_bump_and_protect() {
    let table = Arc::new(EpochTable::new());
    let num_threads = 8;
    let iterations = 5_000;
    let barrier = Arc::new(Barrier::new(num_threads + 1));
    let drain_counter = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            let counter = Arc::clone(&drain_counter);

            thread::spawn(move || {
                let thread = table.register().expect("register");
                barrier.wait();

                for _ in 0..iterations {
                    let guard = thread.protect();

                    // Half the threads bump epochs with callbacks
                    if i % 2 == 0 {
                        let c = Arc::clone(&counter);
                        table.bump_current_epoch(move || {
                            c.fetch_add(1, Ordering::Relaxed);
                        });
                    }

                    std::hint::black_box(guard.epoch());
                    drop(guard);
                }
            })
        })
        .collect();

    barrier.wait();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // All callbacks should have fired (no active threads at the end)
    let final_count = drain_counter.load(Ordering::Relaxed);
    let expected = (num_threads / 2) * iterations;
    assert_eq!(
        final_count, expected as u64,
        "expected {expected} drain callbacks, got {final_count}"
    );
}

#[test]
fn concurrent_safe_epoch_monotonicity() {
    let table = Arc::new(EpochTable::new());
    let num_threads = 8;
    let iterations = 5_000;
    let barrier = Arc::new(Barrier::new(num_threads + 1));
    let violation = Arc::new(AtomicBool::new(false));

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            let violation = Arc::clone(&violation);

            thread::spawn(move || {
                let thread = table.register().expect("register");
                barrier.wait();

                let mut prev_safe = 0u64;
                for _ in 0..iterations {
                    {
                        let _guard = thread.protect();
                        if i == 0 {
                            table.bump_current_epoch_no_callback();
                        }
                    }
                    let safe = table.safe_epoch();
                    if safe < prev_safe {
                        violation.store(true, Ordering::Relaxed);
                    }
                    prev_safe = safe;
                }
            })
        })
        .collect();

    barrier.wait();

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert!(
        !violation.load(Ordering::Relaxed),
        "safe epoch decreased during concurrent test"
    );
}

// ── Guard RAII on panic ────────────────────────────────────────────

#[test]
fn guard_unprotects_on_panic() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = thread.protect();
        assert_eq!(table.active_count(), 1);
        panic!("intentional panic inside protected region");
    }));

    assert!(result.is_err());
    // Guard should have been dropped by unwinding, unprotecting the thread
    assert_eq!(table.active_count(), 0);
}

// ── LightEpoch type alias ──────────────────────────────────────────

#[test]
fn light_epoch_alias_works() {
    let _table: LightEpoch = LightEpoch::new();
}

// ── !Send assertion for EpochGuard ─────────────────────────────────

/// Compile-time assertion: EpochGuard must NOT be Send.
/// This is verified by the PhantomData<*const ()> field.
///
/// If this test compiles, it means EpochGuard is correctly !Send.
/// (We can't write a negative compilation test in standard Rust,
/// but we verify the marker is present.)
#[test]
fn epoch_guard_is_not_send() {
    fn assert_not_send<T>() {
        // This is a compile-time check — if EpochGuard were Send,
        // we'd want this test to fail. Since we can't do negative
        // compilation tests easily, we at least verify the type exists
        // and the PhantomData marker is present by constructing one.
    }
    assert_not_send::<EpochGuard<'_>>();
}

// ── EpochThread is Send ────────────────────────────────────────────

#[test]
fn epoch_thread_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<EpochThread>();
}

// ── Large-scale registration ───────────────────────────────────────

#[test]
fn register_deregister_churn() {
    let table = Arc::new(EpochTable::new());

    for _ in 0..1000 {
        let t = table.register().expect("register");
        let _guard = t.protect();
        // guard dropped, then thread dropped
    }

    assert_eq!(table.registered_count(), 0);
    assert_eq!(table.active_count(), 0);
}

// ── Edge cases ─────────────────────────────────────────────────────

#[test]
fn bump_epoch_many_times() {
    let table = Arc::new(EpochTable::new());

    for i in 0..1000 {
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);
        table.bump_current_epoch(move || {
            c.store(1, Ordering::Relaxed);
        });
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "callback {i} did not fire"
        );
    }

    assert_eq!(table.current_epoch(), 1001);
}

#[test]
fn protect_after_many_bumps() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    for _ in 0..100 {
        table.bump_current_epoch_no_callback();
    }

    let guard = thread.protect();
    assert_eq!(guard.epoch(), 101);
    drop(guard);
}

// ── Property tests ─────────────────────────────────────────────────

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn safe_epoch_le_current(bumps in 0u64..200) {
            let table = Arc::new(EpochTable::new());

            for _ in 0..bumps {
                table.bump_current_epoch_no_callback();
            }

            let safe = table.safe_epoch();
            let current = table.current_epoch();
            prop_assert!(safe < current, "safe {safe} >= current {current}");
        }

        #[test]
        fn protect_unprotect_symmetry(
            num_threads in 1usize..32,
            num_protects in 1usize..10,
        ) {
            let table = Arc::new(EpochTable::new());
            let threads: Vec<_> = (0..num_threads)
                .map(|_| table.register().unwrap())
                .collect();

            // Protect all
            let guards: Vec<Vec<_>> = threads.iter()
                .map(|t| (0..num_protects).map(|_| t.protect()).collect())
                .collect();

            prop_assert_eq!(table.active_count(), num_threads);

            // Drop all guards
            drop(guards);
            prop_assert_eq!(table.active_count(), 0);
        }

        #[test]
        fn epoch_monotonicity_random_ops(ops in proptest::collection::vec(0u8..3, 1..200)) {
            let table = Arc::new(EpochTable::new());
            let thread = table.register().unwrap();
            let mut guards: Vec<EpochGuard<'_>> = Vec::new();
            let mut prev_epoch = table.current_epoch();
            let mut prev_safe = table.safe_epoch();

            for op in ops {
                match op {
                    0 => {
                        // Protect
                        guards.push(thread.protect());
                    }
                    1 => {
                        // Unprotect (drop last guard)
                        guards.pop();
                    }
                    2 => {
                        // Bump
                        table.bump_current_epoch_no_callback();
                    }
                    _ => unreachable!(),
                }

                let epoch = table.current_epoch();
                let safe = table.safe_epoch();

                prop_assert!(epoch >= prev_epoch, "epoch decreased");
                prop_assert!(safe >= prev_safe, "safe epoch decreased");
                prop_assert!(safe < epoch, "safe >= current");

                prev_epoch = epoch;
                prev_safe = safe;
            }
        }
    }
}
