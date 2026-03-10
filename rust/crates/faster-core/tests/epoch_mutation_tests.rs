//! Mutation-testing gap-fill tests for `epoch/` module.
//!
//! Targets surviving mutants from the cargo-mutants campaign on drain.rs,
//! entry.rs, guard.rs, and table.rs.

use faster_core::epoch::EpochTable;
use std::sync::Arc;

// ===========================================================================
// entry.rs — is_occupied must reflect registration state
// ===========================================================================

/// Kill mutants: `is_occupied -> true`, `-> false`, `!= with ==`.
///
/// After registration, the entry should be occupied. After deregistration
/// (dropping EpochThread), it should not be occupied.
#[test]
fn entry_is_occupied_reflects_registration() {
    let table = Arc::new(EpochTable::new());

    // Before registration, no entries are occupied
    assert_eq!(
        table.registered_count(),
        0,
        "no entries should be occupied before registration"
    );

    // Register a thread — entry should become occupied
    let thread = table.register().expect("registration should succeed");
    let _idx = thread.entry_index();
    assert_eq!(table.registered_count(), 1);

    // Get the entry and check is_occupied
    // We can't directly access the entry, but registered_count tells us
    // someone is occupied. Let's verify via active_count.
    assert_eq!(table.active_count(), 0, "no one is in protected region yet");

    // Protect the thread to make it active
    let guard = thread.protect();
    assert_eq!(table.active_count(), 1, "one thread should be active");
    drop(guard);
    assert_eq!(table.active_count(), 0, "no one active after unprotect");

    // Drop the thread (deregistration)
    drop(thread);
    assert_eq!(
        table.registered_count(),
        0,
        "entry should be unoccupied after deregistration"
    );
}

/// Verify that entry reset clears all state.
///
/// Kill mutant: `entry.rs:86 reset -> ()`.
/// After registration and protection, dropping the thread should reset
/// the entry fully, making it available for reuse.
#[test]
fn entry_reset_clears_state_for_reuse() {
    let table = Arc::new(EpochTable::new());

    // Register, protect, unprotect, deregister
    let t1 = table.register().expect("first registration");
    let idx1 = t1.entry_index();
    {
        let _g = t1.protect();
        // Bump epoch while protected
        table.bump_current_epoch_no_callback();
    }
    drop(t1);

    // The slot should now be reusable
    let t2 = table.register().expect("second registration");
    let idx2 = t2.entry_index();
    // The slot from t1 should have been freed and reused
    assert_eq!(idx1, idx2, "deregistered slot should be reused");
    assert_eq!(table.registered_count(), 1);

    // The new registration should start with clean state
    assert_eq!(
        table.active_count(),
        0,
        "new thread should not be active initially"
    );
    drop(t2);
}

// ===========================================================================
// guard.rs — EpochThread::table() must return the correct table
// ===========================================================================

/// Kill mutant: `table() -> Box::leak(Default)`.
///
/// The returned table reference must be the one the thread is registered on.
#[test]
fn epoch_thread_table_returns_correct_table() {
    let table = Arc::new(EpochTable::new());

    // Bump epoch to give the table a distinguishable state
    table.bump_current_epoch_no_callback();
    table.bump_current_epoch_no_callback();
    let expected_epoch = table.current_epoch();
    assert!(expected_epoch > 1, "epoch should have been bumped");

    let thread = table.register().expect("registration should succeed");
    let returned_table = thread.table();

    // The returned table must report the same epoch as the original
    assert_eq!(
        returned_table.current_epoch(),
        expected_epoch,
        "table() must return the actual table, not a default"
    );

    // Default EpochTable starts at epoch 1, so this also catches the
    // Box::leak(Default::default()) mutant
    drop(thread);
}

// ===========================================================================
// table.rs — register thread_id sentinel (+ vs *)
// ===========================================================================

/// Kill mutant: `table.rs:148` — `+ with *` in register.
///
/// For slot index 0: `(0 + 1) = 1` (correct) vs `(0 * 1) = 0` (unoccupied).
/// The mutant would leave slot 0 marked as unoccupied after registration.
#[test]
fn register_slot_zero_is_occupied() {
    let table = Arc::new(EpochTable::new());

    // First registration always gets slot 0 (from free list)
    let thread = table.register().expect("registration should succeed");
    assert_eq!(
        thread.entry_index(),
        0,
        "first registration should be slot 0"
    );

    // Slot 0 must be occupied
    assert_eq!(
        table.registered_count(),
        1,
        "slot 0 must be marked as occupied after registration"
    );

    // Register a second thread to verify the first didn't silently fail
    let thread2 = table.register().expect("second registration");
    assert_ne!(
        thread2.entry_index(),
        0,
        "slot 0 should not be allocated twice"
    );
    assert_eq!(table.registered_count(), 2);

    drop(thread2);
    drop(thread);
}

// ===========================================================================
// drain.rs — performance-only mutants (triage: acceptable survivors)
// ===========================================================================

// drain.rs:198 (+= vs *=): drain_count bookkeeping only affects the
// has_pending() optimization. Actions still execute. Performance-only.
//
// drain.rs:207 (> vs ==, <, >=): Same reasoning — controls fetch_sub
// of drain_count which is an optimization counter.
//
// drain.rs:218 (has_pending -> true): Always returning true just means
// try_drain scans the epoch table every time. No correctness impact.
//
// drain.rs:218 (> vs >=): For u64, `> 0` and `>= 1` are equivalent.
// The mutant `>= 0` is always true = returns true always = performance-only.
//
// drain.rs:246 (Drop -> ()): Leak-only mutant. Tests can't detect memory
// leaks. Equivalent per SKILL.md.
//
// table.rs:364 (< vs <=): When epoch == min, setting min = epoch is a
// no-op. Equivalent mutant.

// ===========================================================================
// Integration: epoch lifecycle with deferred callbacks
// ===========================================================================

/// Verify that deferred callbacks fire after epoch advancement.
#[test]
fn deferred_callbacks_fire_after_epoch_advance() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let table = Arc::new(EpochTable::new());
    let fired = Arc::new(AtomicBool::new(false));

    let fired_clone = Arc::clone(&fired);
    table.defer(move || {
        fired_clone.store(true, Ordering::Release);
    });

    // Register, protect, bump, unprotect, bump again
    let thread = table.register().expect("registration");
    {
        let _g = thread.protect();
        table.bump_current_epoch_no_callback();
    }
    // After unprotect, epoch can advance past the defer point
    table.bump_current_epoch_no_callback();

    // Re-protect to trigger drain
    {
        let _g = thread.protect();
    }

    // The callback should have fired during drain
    assert!(
        fired.load(Ordering::Acquire),
        "deferred callback should fire after epoch advances"
    );

    drop(thread);
}

/// Multiple registrations and deregistrations cycle correctly.
#[test]
fn register_deregister_cycle() {
    let table = Arc::new(EpochTable::new());

    for i in 0..10 {
        let thread = table
            .register()
            .unwrap_or_else(|| panic!("registration {} should succeed", i));
        assert_eq!(table.registered_count(), 1);
        {
            let _g = thread.protect();
            assert_eq!(table.active_count(), 1);
        }
        assert_eq!(table.active_count(), 0);
        drop(thread);
        assert_eq!(table.registered_count(), 0);
    }
}
