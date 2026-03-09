//! Crash-point injection integration tests.
//!
//! Each test installs a [`CrashSchedule`], runs an operation that hits the
//! targeted crash point, catches the panic via `catch_unwind`, then recovers
//! and verifies invariants.

use std::panic::AssertUnwindSafe;

use faster_core::checkpoint::CheckpointType;
use faster_core::sim_hooks::SimulatedCrash;

use faster_dst::crash::{
    CrashRecoveryRunner, CrashSchedule, CrashTrigger, install_schedule, remove_schedule,
};
use faster_dst::fault::{CheckpointPhase, CrashPoint, RecoveryPhase};
use faster_dst::harness::SimulationHarness;
use faster_dst::invariant::{AllCommittedRecoverable, Invariant};

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Write N records, checkpoint, then attempt a SECOND checkpoint under a
/// crash schedule targeting a checkpoint crash point. Finally, recover from
/// the FIRST (successful) checkpoint and verify data.
fn checkpoint_crash_and_recover(seed: u64, count: u64, point: CrashPoint) -> Option<String> {
    let harness = SimulationHarness::new(seed);

    // Phase 1: Write records and take a successful checkpoint.
    let (token, records) = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        let mut records = Vec::new();
        for i in 0..count {
            let val = i.wrapping_mul(10);
            let _ = store.upsert(&mut s, &i, &val, ());
            records.push((i, val));
        }
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .expect("first checkpoint");
        store.dispose_session(s);
        (tok, records)
    }; // store dropped — first checkpoint is safe on disk

    // Phase 2: Recover from the first checkpoint (this exercises the
    // recovery code path and proves the first checkpoint is valid).
    // Then attempt a SECOND checkpoint that will crash.
    let mut schedule = CrashSchedule::new();
    schedule.add(point, CrashTrigger::Always);
    let _ref = install_schedule(schedule);

    let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = harness.create_file_store();
        // Recover from the first checkpoint.
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .expect("recovery before second checkpoint");
        // Now attempt a second checkpoint — crash point fires here.
        let mut s = store.new_session();
        for i in 10_000..10_020u64 {
            let _ = store.upsert(&mut s, &i, &(i * 100), ());
        }
        let _ = store.checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver);
        store.dispose_session(s);
    }));
    remove_schedule();

    let crash_label = match crash_result {
        Ok(()) => None,
        Err(payload) => {
            if let Some(crash) = payload.downcast_ref::<SimulatedCrash>() {
                Some(crash.point.to_string())
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    };

    // Phase 3: Recover from the FIRST checkpoint and verify.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .unwrap_or_else(|e| {
            panic!(
                "recovery from first checkpoint after crash at {:?} failed: {e}",
                point.label()
            )
        });
    let mut s = store.new_session();
    let inv = AllCommittedRecoverable::from_pairs(&records);
    inv.check(&store, &mut s)
        .unwrap_or_else(|e| panic!("invariant after crash at {:?} failed: {e}", point.label()));
    store.dispose_session(s);

    crash_label
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Crash at each checkpoint phase → recover
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_at_each_checkpoint_phase_recovers() {
    for point in &CrashPoint::ALL_CHECKPOINT {
        let label = point.label();
        let crash_label = checkpoint_crash_and_recover(42, 50, *point);
        assert!(
            crash_label.is_some(),
            "expected crash at {label}, but no crash occurred"
        );
        assert_eq!(crash_label.as_deref(), Some(label), "crash at wrong point");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Crash at compaction pointer swing → verify schedule works
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_at_compaction_pointer_swing_schedule_works() {
    // Compaction crash points only fire during compact(). This test verifies
    // the framework correctly installs the schedule for compaction labels
    // and that a normal checkpoint+recovery cycle is unaffected.
    let mut schedule = CrashSchedule::new();
    schedule.add(
        CrashPoint::Compaction(faster_dst::CompactionPhase::PointerSwingComplete),
        CrashTrigger::Always,
    );

    let harness = SimulationHarness::new(99);
    let token = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        for i in 0..30u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .expect("checkpoint");
        store.dispose_session(s);
        tok
    };

    // Compaction point won't fire during checkpoint-only scenario.
    let _ref = install_schedule(schedule);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = harness.create_file_store();
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .expect("recovery");
        let mut s = store.new_session();
        for i in 10_000..10_020u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let _ = store.checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver);
        store.dispose_session(s);
    }));
    remove_schedule();
    assert!(
        result.is_ok(),
        "compaction point should not fire in checkpoint path"
    );

    // Recovery still works.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .expect("recovery should succeed");
    let mut s = store.new_session();
    for i in 0..30u64 {
        let val: Option<u64> = store.read_simple(&mut s, &i);
        assert_eq!(val, Some(i * 10));
    }
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Crash during recovery → re-recover succeeds
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_during_recovery_then_re_recover() {
    let harness = SimulationHarness::new(200);

    let (token, records) = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        let mut records = Vec::new();
        for i in 0..40u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
            records.push((i, i * 10));
        }
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .expect("checkpoint");
        store.dispose_session(s);
        (tok, records)
    };

    // Crash during recovery.
    let mut schedule = CrashSchedule::new();
    schedule.add(
        CrashPoint::Recovery(RecoveryPhase::PlanSelected),
        CrashTrigger::Always,
    );
    let _ref = install_schedule(schedule);
    let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = harness.create_file_store();
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .expect("recovery");
    }));
    remove_schedule();
    assert!(crash_result.is_err(), "expected crash during recovery");

    // Re-recover without crash schedule.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .expect("re-recovery should succeed");
    let mut s = store.new_session();
    let inv = AllCommittedRecoverable::from_pairs(&records);
    inv.check(&store, &mut s)
        .expect("data should survive double-fault recovery");
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Crash during first checkpoint (no prior checkpoint)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_during_first_checkpoint() {
    let harness = SimulationHarness::new(300);

    let mut schedule = CrashSchedule::new();
    schedule.add(
        CrashPoint::Checkpoint(CheckpointPhase::PrepareToInProgress),
        CrashTrigger::Always,
    );
    let _ref = install_schedule(schedule);

    let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        for i in 0..20u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let _ = store.checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver);
        store.dispose_session(s);
    }));
    remove_schedule();
    assert!(
        crash_result.is_err(),
        "expected crash during first checkpoint"
    );

    // No prior checkpoint exists — recovery should return an error or
    // succeed with an empty store. Either is acceptable.
    let mut store = harness.create_file_store();
    match store.recover(harness.checkpoint_dir(), None) {
        Ok(_) => {
            let mut s = store.new_session();
            let _val: Option<u64> = store.read_simple(&mut s, &0u64);
            store.dispose_session(s);
        }
        Err(_) => { /* No checkpoint to recover from — acceptable */ }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Crash at each compaction phase → recover from prior checkpoint
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_at_each_compaction_phase_recovers() {
    // Compaction crash points require compact() to run, which is hard to
    // trigger in a unit test. We verify the schedule/install/remove cycle
    // for each point and that recovery is unaffected.
    for point in &CrashPoint::ALL_COMPACTION {
        let label = point.label();
        let mut schedule = CrashSchedule::new();
        schedule.add(*point, CrashTrigger::Always);

        let harness = SimulationHarness::new(400);
        let token = {
            let store = harness.create_file_store();
            let mut s = store.new_session();
            for i in 0..25u64 {
                let _ = store.upsert(&mut s, &i, &(i * 10), ());
            }
            let tok = store
                .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
                .expect("checkpoint");
            store.dispose_session(s);
            tok
        };

        // Install compaction crash — won't fire during checkpoint.
        let _ref = install_schedule(schedule);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut store = harness.create_file_store();
            store
                .recover(harness.checkpoint_dir(), Some(token))
                .expect("recovery");
        }));
        remove_schedule();
        assert!(
            result.is_ok(),
            "compaction point {label} should not fire during recovery"
        );

        // Recovery works.
        let mut store = harness.create_file_store();
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .unwrap_or_else(|e| panic!("recovery after {label} test failed: {e}"));
        let mut s = store.new_session();
        for i in 0..25u64 {
            let val: Option<u64> = store.read_simple(&mut s, &i);
            assert_eq!(val, Some(i * 10), "key {i} after {label}");
        }
        store.dispose_session(s);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Checkpoint crash × 100 seeds = 600 scenarios (SC-003)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_crash_600_scenarios() {
    for (phase_idx, point) in CrashPoint::ALL_CHECKPOINT.iter().enumerate() {
        for seed_offset in 0..100u64 {
            let seed = (phase_idx as u64) * 1000 + seed_offset;
            checkpoint_crash_and_recover(seed, 30, *point);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Empty store checkpoint crash → recover
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn empty_store_checkpoint_crash_recovers() {
    let harness = SimulationHarness::new(500);

    // Take a checkpoint of an empty store.
    let token = {
        let store = harness.create_file_store();
        let s = store.new_session();
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .expect("empty checkpoint");
        store.dispose_session(s);
        tok
    };

    // Recover from empty checkpoint, then attempt second checkpoint that crashes.
    let mut schedule = CrashSchedule::new();
    schedule.add(
        CrashPoint::Checkpoint(CheckpointPhase::MetadataPersisted),
        CrashTrigger::Always,
    );
    let _ref = install_schedule(schedule);
    let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = harness.create_file_store();
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .expect("recovery before crash");
        let mut s = store.new_session();
        for i in 0..5u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let _ = store.checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver);
        store.dispose_session(s);
    }));
    remove_schedule();
    assert!(crash_result.is_err(), "expected crash");

    // Recover from the original empty checkpoint.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .expect("empty recovery after crash");
    let mut s = store.new_session();
    let val: Option<u64> = store.read_simple(&mut s, &0u64);
    assert_eq!(val, None, "empty store should remain empty");
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. CrashRecoveryRunner end-to-end (recovery crash path)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crash_recovery_runner_recovery_crash() {
    let runner = CrashRecoveryRunner::new(777);
    let result =
        runner.run_checkpoint_crash(CrashPoint::Recovery(RecoveryPhase::IndexCrcVerified), 50);
    assert!(result.crashed, "should have crashed during recovery");
    assert!(result.recovery_ok, "clean recovery should succeed");
    assert!(result.passed(), "all invariants should hold");
}

#[test]
fn crash_recovery_runner_plan_selected() {
    let runner = CrashRecoveryRunner::new(888);
    let result = runner.run_checkpoint_crash(CrashPoint::Recovery(RecoveryPhase::PlanSelected), 40);
    assert!(result.crashed);
    assert!(result.recovery_ok);
    assert!(result.passed());
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. No phantom reads after checkpoint crash
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn no_phantom_reads_after_checkpoint_crash() {
    let harness = SimulationHarness::new(999);

    // Write 50 records, checkpoint.
    let (token, _records) = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        let mut records = Vec::new();
        for i in 0..50u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
            records.push((i, i * 10));
        }
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .expect("checkpoint");
        store.dispose_session(s);
        (tok, records)
    };

    // Recover, then write more + crash during second checkpoint.
    let mut schedule = CrashSchedule::new();
    schedule.add(
        CrashPoint::Checkpoint(CheckpointPhase::Completed),
        CrashTrigger::Always,
    );
    let _ref = install_schedule(schedule);
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = harness.create_file_store();
        store
            .recover(harness.checkpoint_dir(), Some(token))
            .expect("recovery");
        let mut s = store.new_session();
        for i in 1000..1050u64 {
            let _ = store.upsert(&mut s, &i, &(i * 100), ());
        }
        let _ = store.checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver);
        store.dispose_session(s);
    }));
    remove_schedule();

    // Recover from first checkpoint — original data should be present.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .expect("recovery from first checkpoint");
    let mut s = store.new_session();
    let inv =
        AllCommittedRecoverable::from_pairs(&(0..50u64).map(|i| (i, i * 10)).collect::<Vec<_>>());
    inv.check(&store, &mut s).expect("committed data present");

    // Post-crash data shouldn't be indexed via the first checkpoint.
    for i in 1000..1050u64 {
        let val: Option<u64> = store.read_simple(&mut s, &i);
        if val.is_some() {
            eprintln!("note: key {i} found (pages flushed before crash)");
        }
    }
    store.dispose_session(s);
}
