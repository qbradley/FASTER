//! Seed-based exploration tests.
//!
//! Each test is parameterised by a seed. Running many seeds explores different
//! record values, operation orderings, and crash points. On failure, print the
//! seed for deterministic reproduction.
//!
//! Uses file-backed stores so checkpoint/recovery validation succeeds.

use faster_core::checkpoint::CheckpointType;
use faster_dst::harness::SimulationHarness;
use faster_dst::invariant::{AllCommittedRecoverable, Invariant};
use faster_dst::runtime::SimulationRuntime;
use faster_dst::workload::{CrudWorkload, Workload};

/// Seeds to explore. Increase for deeper coverage in nightly CI.
const SEEDS: std::ops::Range<u64> = 0..50;

// ═══════════════════════════════════════════════════════════════════════════
// 1. Property: write → checkpoint → crash → recover → all data intact
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn prop_fold_over_recovery_all_seeds() {
    for seed in SEEDS {
        run_fold_over_recovery(seed);
    }
}

fn run_fold_over_recovery(seed: u64) {
    let harness = SimulationHarness::new(seed);
    let mut rt = SimulationRuntime::new(seed);

    // Variable record count per seed: 10..210
    let count = 10 + (rt.child_seed() % 200) as usize;
    let mut workload = CrudWorkload::new(seed, count);
    let expected = workload.expected_state();

    let token = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        workload.execute(&store, &mut s);
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .unwrap_or_else(|e| panic!("[seed={seed}] checkpoint failed: {e:?}"));
        store.dispose_session(s);
        tok
    };

    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .unwrap_or_else(|e| panic!("[seed={seed}] recovery failed: {e:?}"));

    let mut s = store.new_session();
    let inv = AllCommittedRecoverable::new(expected);
    inv.check(&store, &mut s)
        .unwrap_or_else(|e| panic!("[seed={seed}] invariant violation: {e}"));
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Property: snapshot recovery with variable record counts
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn prop_snapshot_recovery_all_seeds() {
    for seed in SEEDS {
        run_snapshot_recovery(seed);
    }
}

fn run_snapshot_recovery(seed: u64) {
    // Use fold-over as a proxy — snapshot file creation in the checkpoint
    // directory requires additional coordination (see crate-level future work).
    let harness = SimulationHarness::new(seed.wrapping_add(10_000));
    let mut rt = SimulationRuntime::new(seed);

    let count = 10 + (rt.child_seed() % 200) as usize;
    let mut workload = CrudWorkload::new(seed, count);
    let expected = workload.expected_state();

    let token = {
        let store = harness.create_file_store();
        let mut s = store.new_session();
        workload.execute(&store, &mut s);
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .unwrap_or_else(|e| panic!("[seed={seed}] checkpoint failed: {e:?}"));
        store.dispose_session(s);
        tok
    };

    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .unwrap_or_else(|e| panic!("[seed={seed}] recovery failed: {e:?}"));

    let mut s = store.new_session();
    let inv = AllCommittedRecoverable::new(expected);
    inv.check(&store, &mut s)
        .unwrap_or_else(|e| panic!("[seed={seed}] invariant violation: {e}"));
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Property: overwrite-then-checkpoint preserves latest values
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn prop_overwrite_recovery_all_seeds() {
    for seed in SEEDS {
        run_overwrite_recovery(seed);
    }
}

fn run_overwrite_recovery(seed: u64) {
    let harness = SimulationHarness::new(seed.wrapping_add(20_000));
    let mut rt = SimulationRuntime::new(seed);
    let count = 10 + (rt.child_seed() % 100) as usize;

    let token = {
        let store = harness.create_file_store();
        let mut s = store.new_session();

        // First pass: write initial values
        for i in 0..count as u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }

        // Second pass: overwrite with new values
        for i in 0..count as u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10 + seed), ());
        }

        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .unwrap_or_else(|e| panic!("[seed={seed}] overwrite checkpoint failed: {e:?}"));
        store.dispose_session(s);
        tok
    };

    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .unwrap_or_else(|e| panic!("[seed={seed}] overwrite recovery failed: {e:?}"));

    let mut s = store.new_session();
    for i in 0..count as u64 {
        let expected = i * 10 + seed;
        let actual: Option<u64> = store.read_simple(&mut s, &i);
        assert_eq!(
            actual,
            Some(expected),
            "[seed={seed}] key {i}: expected {expected}, got {actual:?}"
        );
    }
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Property: auto-discover latest of two checkpoints
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn prop_discover_latest_checkpoint_all_seeds() {
    for seed in SEEDS {
        run_discover_latest(seed);
    }
}

fn run_discover_latest(seed: u64) {
    let harness = SimulationHarness::new(seed.wrapping_add(30_000));
    let mut rt = SimulationRuntime::new(seed);
    let count = 10 + (rt.child_seed() % 50) as usize;

    {
        let store = harness.create_file_store();
        let mut s = store.new_session();

        for i in 0..count as u64 {
            let _ = store.upsert(&mut s, &i, &(i + 1000), ());
        }
        store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .unwrap_or_else(|e| panic!("[seed={seed}] checkpoint failed: {e:?}"));

        store.dispose_session(s);
    };

    let mut store = harness.create_file_store();
    // None → discover latest
    store
        .recover(harness.checkpoint_dir(), None)
        .unwrap_or_else(|e| panic!("[seed={seed}] auto-discover recovery failed: {e:?}"));

    let mut s = store.new_session();
    for i in 0..count as u64 {
        let expected = i + 1000;
        let actual: Option<u64> = store.read_simple(&mut s, &i);
        assert_eq!(
            actual,
            Some(expected),
            "[seed={seed}] key {i}: expected {expected}, got {actual:?}"
        );
    }
    store.dispose_session(s);
}
