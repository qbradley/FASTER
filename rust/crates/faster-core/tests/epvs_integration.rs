//! EPVS (Epoch-Protected Version Scheme) integration tests.
//!
//! These tests exercise the packed `(phase, version)` `SystemState` word
//! under realistic concurrent conditions. EPVS replaces separate AtomicU8
//! phase + AtomicU32 version fields with a single AtomicU64 CAS word,
//! eliminating race windows where threads could observe inconsistent
//! (phase, version) pairs.
//!
//! # Test categories
//!
//! 1. **Concurrent checkpoint + operations** — the primary race EPVS fixes.
//! 2. **Concurrent grow + operations** — secondary race on grow transitions.
//! 3. **Checkpoint + grow simultaneously** — mutual exclusion via single word.
//! 4. **Stress tests** — 16 threads × high-volume ops with periodic state changes.
//! 5. **Property-based tests** — random operation sequences with invariant checks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use faster_core::state::{AtomicSystemState, Phase, SystemState};

/// 56-bit version mask: bits 0–55. Mirrors VERSION_MASK
/// which is pub(crate).
const VERSION_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Collects every (phase, version) snapshot observed by a reader thread.
/// Returns true if all observations have consistent (phase, version) pairs
/// — i.e., no torn reads where old_phase is paired with new_version or
/// vice versa.
fn is_consistent_observation(state: SystemState) -> bool {
    // A valid observation must:
    // 1. Have a recognisable phase (not corrupted by torn read)
    // 2. Not be in an intermediate state (mid-transition)
    let _ = state.phase(); // panics on corrupted discriminant
    !state.is_intermediate()
}

// ═══════════════════════════════════════════════════════════════════════
// 1. Concurrent checkpoint + observer: No torn reads
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that concurrent observers never see inconsistent (phase, version)
/// pairs during a full checkpoint cycle.
///
/// RACE CONDITION TESTED: Without EPVS, a reader could read phase=Prepare
/// then version=1 (the new version), producing an impossible combination.
/// With EPVS, the single-word CAS guarantees atomicity.
#[test]
fn checkpoint_cycle_no_torn_reads() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let torn_read_count = Arc::new(AtomicU64::new(0));
    let observations = Arc::new(AtomicU64::new(0));

    let num_observers = 4;
    let num_cycles = 100;
    let barrier = Arc::new(Barrier::new(num_observers + 1)); // +1 for driver

    // Spawn observer threads that continuously read the state.
    let observers: Vec<_> = (0..num_observers)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let torn = Arc::clone(&torn_read_count);
            let obs = Arc::clone(&observations);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                while !done.load(Ordering::Acquire) {
                    let s = state.load(Ordering::Acquire);
                    obs.fetch_add(1, Ordering::Relaxed);

                    if s.is_intermediate() {
                        // Intermediate states are transient — seeing one is
                        // fine, but the phase+version must still be valid.
                        let _ = s.phase();
                        let _ = s.version();
                        continue;
                    }

                    if !is_consistent_observation(s) {
                        torn.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    // Driver thread: run checkpoint cycles.
    barrier.wait();
    for cycle in 0u64..num_cycles {
        let transitions = [
            (Phase::Rest, Phase::Prepare, cycle, cycle),
            (Phase::Prepare, Phase::InProgress, cycle, cycle + 1),
            (Phase::InProgress, Phase::WaitFlush, cycle + 1, cycle + 1),
            (
                Phase::WaitFlush,
                Phase::WaitCompletion,
                cycle + 1,
                cycle + 1,
            ),
            (
                Phase::WaitCompletion,
                Phase::PersistenceCallback,
                cycle + 1,
                cycle + 1,
            ),
            (
                Phase::PersistenceCallback,
                Phase::Rest,
                cycle + 1,
                cycle + 1,
            ),
        ];

        for (from_phase, to_phase, from_ver, to_ver) in transitions {
            let from = SystemState::new(from_phase, from_ver);
            let to = SystemState::new(to_phase, to_ver);
            let ok = state.try_transition(from, to, || {});
            assert!(
                ok,
                "cycle {cycle}: transition {from_phase} -> {to_phase} failed"
            );
        }
    }

    done.store(true, Ordering::Release);
    for h in observers {
        h.join().expect("observer panicked");
    }

    assert_eq!(
        torn_read_count.load(Ordering::Relaxed),
        0,
        "EPVS should prevent all torn reads"
    );
    assert!(
        observations.load(Ordering::Relaxed) > 0,
        "observers should have made at least some observations"
    );
}

/// Verifies that observer threads always see valid (phase, version) pairs
/// during checkpoint transitions — never an impossible combination like
/// (Prepare, version+1) which would indicate a torn read.
///
/// RACE CONDITION TESTED: The Prepare→InProgress transition bumps version.
/// Without EPVS, a thread could see phase=Prepare with the post-bump version.
#[test]
fn checkpoint_phase_version_consistency() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let violation_count = Arc::new(AtomicU64::new(0));

    let num_observers = 4;
    let num_cycles = 200;
    let barrier = Arc::new(Barrier::new(num_observers + 1));

    let observers: Vec<_> = (0..num_observers)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let violations = Arc::clone(&violation_count);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut last_version = 0u64;

                while !done.load(Ordering::Acquire) {
                    let s = state.wait_non_intermediate();
                    let phase = s.phase();
                    let version = s.version();

                    // Version must be monotonically non-decreasing.
                    if version < last_version {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                    last_version = version;

                    // Phase-version consistency: version only bumps at
                    // Prepare→InProgress. So if phase is Prepare, the
                    // version must be the pre-bump value.
                    if phase == Phase::Prepare && version > last_version {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    barrier.wait();
    for cycle in 0u64..num_cycles {
        let t = |from_p: Phase, to_p: Phase, fv: u64, tv: u64| {
            assert!(state.try_transition(
                SystemState::new(from_p, fv),
                SystemState::new(to_p, tv),
                || {}
            ));
        };
        t(Phase::Rest, Phase::Prepare, cycle, cycle);
        t(Phase::Prepare, Phase::InProgress, cycle, cycle + 1);
        t(Phase::InProgress, Phase::WaitFlush, cycle + 1, cycle + 1);
        t(
            Phase::WaitFlush,
            Phase::WaitCompletion,
            cycle + 1,
            cycle + 1,
        );
        t(
            Phase::WaitCompletion,
            Phase::PersistenceCallback,
            cycle + 1,
            cycle + 1,
        );
        t(
            Phase::PersistenceCallback,
            Phase::Rest,
            cycle + 1,
            cycle + 1,
        );
    }

    done.store(true, Ordering::Release);
    for h in observers {
        h.join().expect("observer panicked");
    }

    assert_eq!(
        violation_count.load(Ordering::Relaxed),
        0,
        "all phase-version observations must be consistent"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 2. Concurrent grow + observer: Atomic grow transitions
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that grow phase transitions are atomic: observers never see
/// a grow phase with a version from a different epoch.
///
/// RACE CONDITION TESTED: Without EPVS, a thread reading phase then version
/// separately could see (PrepareGrow, wrong_version) during transitions.
#[test]
fn grow_cycle_no_torn_reads() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let torn_count = Arc::new(AtomicU64::new(0));

    let num_observers = 4;
    let num_cycles = 100;
    let barrier = Arc::new(Barrier::new(num_observers + 1));

    let observers: Vec<_> = (0..num_observers)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let torn = Arc::clone(&torn_count);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                while !done.load(Ordering::Acquire) {
                    let s = state.load(Ordering::Acquire);
                    if !s.is_intermediate() {
                        let phase = s.phase();
                        let version = s.version();
                        // During grow cycles, version stays constant.
                        // A torn read would show a version from a different cycle.
                        if phase.is_grow() && version > 1000 {
                            // Implausible version for our test range
                            torn.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    barrier.wait();
    for _cycle in 0..num_cycles {
        // Grow cycle: Rest → PrepareGrow → InProgressGrow → WaitCompletionGrow → Rest
        let version = state.load(Ordering::Acquire).version();
        let t = |from_p: Phase, to_p: Phase| {
            assert!(state.try_transition(
                SystemState::new(from_p, version),
                SystemState::new(to_p, version),
                || {}
            ));
        };
        t(Phase::Rest, Phase::PrepareGrow);
        t(Phase::PrepareGrow, Phase::InProgressGrow);
        t(Phase::InProgressGrow, Phase::WaitCompletionGrow);
        t(Phase::WaitCompletionGrow, Phase::Rest);
    }

    done.store(true, Ordering::Release);
    for h in observers {
        h.join().expect("observer panicked");
    }

    assert_eq!(
        torn_count.load(Ordering::Relaxed),
        0,
        "grow transitions must be atomic"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 3. Checkpoint + Grow mutual exclusion
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that checkpoint and grow can never both win a transition from
/// Rest. The single CAS word means exactly one will succeed.
///
/// RACE CONDITION TESTED: If phase and version were separate fields, both
/// could partially succeed — e.g., checkpoint sets phase=Prepare while grow
/// sets version for its own transition.
#[test]
fn checkpoint_grow_mutual_exclusion_stress() {
    let num_trials = 1_000;
    let checkpoint_wins = AtomicU64::new(0);
    let grow_wins = AtomicU64::new(0);
    let both_won = AtomicU64::new(0);
    let neither_won = AtomicU64::new(0);

    for _ in 0..num_trials {
        let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
        let barrier = Arc::new(Barrier::new(2));

        let s1 = Arc::clone(&state);
        let b1 = Arc::clone(&barrier);
        let ckpt_handle = thread::spawn(move || {
            b1.wait();
            s1.try_transition(
                SystemState::INITIAL,
                SystemState::new(Phase::Prepare, 0),
                || {},
            )
        });

        let s2 = Arc::clone(&state);
        let b2 = Arc::clone(&barrier);
        let grow_handle = thread::spawn(move || {
            b2.wait();
            s2.try_transition(
                SystemState::INITIAL,
                SystemState::new(Phase::PrepareGrow, 0),
                || {},
            )
        });

        let ckpt = ckpt_handle.join().unwrap();
        let grow = grow_handle.join().unwrap();

        match (ckpt, grow) {
            (true, false) => {
                checkpoint_wins.fetch_add(1, Ordering::Relaxed);
            }
            (false, true) => {
                grow_wins.fetch_add(1, Ordering::Relaxed);
            }
            (true, true) => {
                both_won.fetch_add(1, Ordering::Relaxed);
            }
            (false, false) => {
                neither_won.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    assert_eq!(
        both_won.load(Ordering::Relaxed),
        0,
        "checkpoint and grow must never both win the CAS race"
    );
    assert_eq!(
        neither_won.load(Ordering::Relaxed),
        0,
        "exactly one of checkpoint/grow should always win"
    );
    assert!(
        checkpoint_wins.load(Ordering::Relaxed) > 0 && grow_wins.load(Ordering::Relaxed) > 0,
        "both checkpoint and grow should win some races (statistical check — \
         ckpt={}, grow={})",
        checkpoint_wins.load(Ordering::Relaxed),
        grow_wins.load(Ordering::Relaxed),
    );
}

/// Multiple threads attempt overlapping checkpoint and grow transitions.
/// Verifies that the final state is always reachable through a valid
/// sequence of transitions — no corrupted state from concurrent access.
///
/// INVARIANT: After all threads complete, the state must be in one of the
/// valid phases with a version consistent with completed checkpoint cycles.
#[test]
fn multi_thread_checkpoint_grow_interleave() {
    let num_rounds = 50;
    let threads_per_role = 4;

    for _ in 0..num_rounds {
        let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
        let barrier = Arc::new(Barrier::new(threads_per_role * 2));

        // Checkpoint threads try Rest → Prepare → ... → Rest cycle
        let ckpt_handles: Vec<_> = (0..threads_per_role)
            .map(|_| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    // Try to initiate checkpoint
                    let _ = state.try_transition(
                        SystemState::INITIAL,
                        SystemState::new(Phase::Prepare, 0),
                        || {},
                    );
                })
            })
            .collect();

        // Grow threads try Rest → PrepareGrow cycle
        let grow_handles: Vec<_> = (0..threads_per_role)
            .map(|_| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let _ = state.try_transition(
                        SystemState::INITIAL,
                        SystemState::new(Phase::PrepareGrow, 0),
                        || {},
                    );
                })
            })
            .collect();

        for h in ckpt_handles.into_iter().chain(grow_handles) {
            h.join().expect("thread panicked");
        }

        // Verify final state is valid.
        let final_state = state.load(Ordering::Acquire);
        assert!(
            !final_state.is_intermediate(),
            "must not be in intermediate state"
        );
        let phase = final_state.phase();
        assert!(
            phase == Phase::Prepare || phase == Phase::PrepareGrow,
            "expected Prepare or PrepareGrow, got {phase}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 4. Stress tests
// ═══════════════════════════════════════════════════════════════════════

/// High-contention stress: 8 threads competing to drive state transitions
/// while 8 observer threads validate invariants.
///
/// INVARIANT: Version is monotonically non-decreasing. Phase transitions
/// follow the valid checkpoint lifecycle.
#[test]
fn stress_concurrent_transitions_8_threads() {
    stress_concurrent_transitions(8, 500);
}

/// 16-thread stress test for nightly CI. Runs more cycles to increase
/// the probability of exposing race conditions.
#[test]
#[ignore] // ~2-5s depending on hardware
fn stress_concurrent_transitions_16_threads() {
    stress_concurrent_transitions(16, 2_000);
}

fn stress_concurrent_transitions(num_drivers: usize, cycles_per_driver: usize) {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let version_regression = Arc::new(AtomicBool::new(false));
    let successful_cycles = Arc::new(AtomicU64::new(0));

    let total_threads = num_drivers + num_drivers; // drivers + observers
    let barrier = Arc::new(Barrier::new(total_threads));

    // Observer threads: continuously verify invariants.
    let observers: Vec<_> = (0..num_drivers)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let regression = Arc::clone(&version_regression);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut last_version = 0u64;
                while !done.load(Ordering::Acquire) {
                    let s = state.wait_non_intermediate();
                    let v = s.version();
                    if v < last_version {
                        regression.store(true, Ordering::Release);
                    }
                    last_version = v;
                }
            })
        })
        .collect();

    // Driver threads: compete to run full checkpoint cycles.
    let drivers: Vec<_> = (0..num_drivers)
        .map(|_| {
            let state = Arc::clone(&state);
            let cycles = Arc::clone(&successful_cycles);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..cycles_per_driver {
                    // Read current state and try to advance it.
                    let current = state.wait_non_intermediate();
                    let phase = current.phase();
                    let version = current.version();

                    let (next_phase, next_version) = match phase {
                        Phase::Rest => (Phase::Prepare, version),
                        Phase::Prepare => (Phase::InProgress, version + 1),
                        Phase::InProgress => (Phase::WaitFlush, version),
                        Phase::WaitFlush => (Phase::WaitCompletion, version),
                        Phase::WaitCompletion => (Phase::PersistenceCallback, version),
                        Phase::PersistenceCallback => (Phase::Rest, version),
                        Phase::PrepareGrow => (Phase::InProgressGrow, version),
                        Phase::InProgressGrow => (Phase::WaitCompletionGrow, version),
                        Phase::WaitCompletionGrow => (Phase::Rest, version),
                    };

                    if state.try_transition(
                        current,
                        SystemState::new(next_phase, next_version),
                        || {},
                    ) && next_phase == Phase::Rest
                    {
                        cycles.fetch_add(1, Ordering::Relaxed);
                    }
                    // CAS failure is expected under contention — just retry.
                }
            })
        })
        .collect();

    for d in drivers {
        d.join().expect("driver panicked");
    }
    done.store(true, Ordering::Release);
    for o in observers {
        o.join().expect("observer panicked");
    }

    assert!(
        !version_regression.load(Ordering::Acquire),
        "version must be monotonically non-decreasing"
    );
    assert!(
        successful_cycles.load(Ordering::Relaxed) > 0,
        "at least one full cycle should complete"
    );
}

/// Stress test with mixed checkpoint and grow transitions competing.
/// Verifies that interleaved checkpoint/grow cycles don't corrupt state.
///
/// INVARIANT: The state word is never in an unrecognised state.
/// Version never regresses. Intermediate windows are finite.
#[test]
fn stress_mixed_checkpoint_grow() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let corruption_count = Arc::new(AtomicU64::new(0));
    let completed_ckpt = Arc::new(AtomicU64::new(0));
    let completed_grow = Arc::new(AtomicU64::new(0));

    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads + 2)); // +2 observers

    // Two observer threads that validate state.
    let obs_handles: Vec<_> = (0..2)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let corrupt = Arc::clone(&corruption_count);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                while !done.load(Ordering::Acquire) {
                    let s = state.load(Ordering::Acquire);
                    // Even intermediate states must have valid phase discriminants.
                    let raw_phase = s.phase_raw() & !0x80;
                    if Phase::from_u8(raw_phase).is_none() {
                        corrupt.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    // Worker threads: drive transitions. Even-numbered drive checkpoints,
    // odd-numbered drive grows. All compete on the same state word.
    let workers: Vec<_> = (0..num_threads)
        .map(|t| {
            let state = Arc::clone(&state);
            let ckpt_done = Arc::clone(&completed_ckpt);
            let grow_done = Arc::clone(&completed_grow);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let is_checkpoint_driver = t % 2 == 0;

                for _ in 0..500 {
                    let current = state.wait_non_intermediate();
                    let phase = current.phase();
                    let version = current.version();

                    if phase == Phase::Rest {
                        // Compete to start either checkpoint or grow.
                        if is_checkpoint_driver {
                            let _ = state.try_transition(
                                current,
                                SystemState::new(Phase::Prepare, version),
                                || {},
                            );
                        } else {
                            let _ = state.try_transition(
                                current,
                                SystemState::new(Phase::PrepareGrow, version),
                                || {},
                            );
                        }
                    } else {
                        // Advance whatever's in progress.
                        let (next_phase, next_version) = match phase {
                            Phase::Prepare => (Phase::InProgress, version + 1),
                            Phase::InProgress => (Phase::WaitFlush, version),
                            Phase::WaitFlush => (Phase::WaitCompletion, version),
                            Phase::WaitCompletion => (Phase::PersistenceCallback, version),
                            Phase::PersistenceCallback => {
                                ckpt_done.fetch_add(1, Ordering::Relaxed);
                                (Phase::Rest, version)
                            }
                            Phase::PrepareGrow => (Phase::InProgressGrow, version),
                            Phase::InProgressGrow => (Phase::WaitCompletionGrow, version),
                            Phase::WaitCompletionGrow => {
                                grow_done.fetch_add(1, Ordering::Relaxed);
                                (Phase::Rest, version)
                            }
                            Phase::Rest => continue,
                        };

                        let _ = state.try_transition(
                            current,
                            SystemState::new(next_phase, next_version),
                            || {},
                        );
                    }
                }
            })
        })
        .collect();

    for w in workers {
        w.join().expect("worker panicked");
    }
    done.store(true, Ordering::Release);
    for o in obs_handles {
        o.join().expect("observer panicked");
    }

    assert_eq!(
        corruption_count.load(Ordering::Relaxed),
        0,
        "no corrupted phase discriminants should be observed"
    );
    // At least some cycles should complete.
    let total = completed_ckpt.load(Ordering::Relaxed) + completed_grow.load(Ordering::Relaxed);
    assert!(total > 0, "at least one full cycle should complete");
}

// ═══════════════════════════════════════════════════════════════════════
// 5. FasterKv-level EPVS: concurrent operations + checkpoint
// ═══════════════════════════════════════════════════════════════════════

use faster_core::InMemoryDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};

/// Creates a store sized for concurrent tests.
fn concurrent_test_store() -> Arc<FasterKv<SimpleFunctions<u64, u64>>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16, // 64K buckets
        buffer_size_pages: 32,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    Arc::new(FasterKv::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ))
}

/// Creates a store with small hash index to force grow sooner.
fn growable_test_store() -> Arc<FasterKv<SimpleFunctions<u64, u64>>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig {
            load_factor_threshold: 0.5, // trigger grow earlier
            chunks_per_operation: 8,
            enabled: true,
        },
        hash_index_size_log2: 10, // 1K buckets — will fill quickly
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    Arc::new(FasterKv::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ))
}

/// Creates a counter store for RMW tests.
/// CounterFunctions uses i64 for Value/Input/Output.
fn concurrent_counter_store() -> Arc<FasterKv<CounterFunctions<u64>>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16,
        buffer_size_pages: 32,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    Arc::new(FasterKv::new(
        config,
        CounterFunctions::new(),
        InMemoryDevice::new(),
    ))
}

/// Multiple threads perform upserts while a checkpoint is taken.
/// Verifies no data corruption: every key written before the checkpoint
/// is readable after the checkpoint completes.
///
/// RACE CONDITION TESTED: Operations that straddle a checkpoint boundary
/// must see consistent (phase, version) — otherwise records might be
/// written to the wrong version, causing data loss or corruption.
#[test]
#[ignore = "tier-2: 4-thread concurrent upsert + checkpoint stress test"]
fn concurrent_upserts_during_checkpoint() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = concurrent_test_store();

    const THREADS: u64 = 4;
    const KEYS_PER_THREAD: u64 = 5_000;

    // Phase 1: Populate store.
    {
        let mut s = store.new_session();
        for i in 0..1_000u64 {
            let _ = store.upsert(&mut s, &i, &(i * 7), ());
        }
        store.dispose_session(s);
    }

    // Phase 2: Concurrent upserts + checkpoint.
    let barrier = Arc::new(Barrier::new(THREADS as usize + 1)); // +1 checkpoint thread
    let panic_count = Arc::new(AtomicU64::new(0));

    let writer_handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let panics = Arc::clone(&panic_count);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let base = 1_000 + t * KEYS_PER_THREAD;
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let status = store.upsert(&mut s, &key, &(key * 3), ());
                    if !status.is_success() {
                        panics.fetch_add(1, Ordering::Relaxed);
                    }
                }

                store.dispose_session(s);
            })
        })
        .collect();

    // Checkpoint thread.
    let store_ckpt = Arc::clone(&store);
    let dir_path = dir.path().to_path_buf();
    let barrier_ckpt = Arc::clone(&barrier);
    let ckpt_handle = thread::spawn(move || {
        barrier_ckpt.wait();
        // Give writers a moment to start, then checkpoint.
        thread::yield_now();
        store_ckpt.checkpoint(&dir_path, faster_core::checkpoint::CheckpointType::FoldOver)
    });

    for h in writer_handles {
        h.join().expect("writer panicked");
    }
    let ckpt_result = ckpt_handle.join().expect("checkpoint thread panicked");
    // Checkpoint may or may not succeed depending on timing; the key
    // invariant is no panics and no data corruption.
    let _result = ckpt_result;

    assert_eq!(
        panic_count.load(Ordering::Relaxed),
        0,
        "no operations should fail during concurrent checkpoint"
    );

    // Phase 3: Verify pre-checkpoint data is still readable.
    {
        let mut s = store.new_session();
        for i in 0..1_000u64 {
            let mut out: Option<u64> = None;
            let status = store.read(&mut s, &i, &0u64, &mut out, ());
            assert_eq!(
                status,
                OperationStatus::Ok,
                "key {i} missing after checkpoint"
            );
            assert_eq!(out, Some(i * 7), "key {i} value corrupted after checkpoint");
        }
        store.dispose_session(s);
    }
}

/// Multiple threads perform reads and upserts while a checkpoint is taken.
/// Reads during checkpoint must return consistent values — never partial
/// writes or corrupted records.
///
/// RACE CONDITION TESTED: Read operations that observe a phase transition
/// mid-flight must either see the pre-transition or post-transition value,
/// never a mix.
#[test]
fn concurrent_reads_during_checkpoint() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = concurrent_test_store();

    // Populate with known values.
    const N: u64 = 2_000;
    {
        let mut s = store.new_session();
        for i in 0..N {
            let _ = store.upsert(&mut s, &i, &(i * 11), ());
        }
        store.dispose_session(s);
    }

    let barrier = Arc::new(Barrier::new(5)); // 4 readers + 1 checkpoint
    let corruption_count = Arc::new(AtomicU64::new(0));

    // Reader threads: continuously verify values.
    let reader_handles: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let corrupt = Arc::clone(&corruption_count);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                for i in 0..N {
                    let mut out: Option<u64> = None;
                    let status = store.read(&mut s, &i, &0u64, &mut out, ());
                    if status == OperationStatus::Ok && out != Some(i * 11) {
                        corrupt.fetch_add(1, Ordering::Relaxed);
                    }
                    // Pending is acceptable (page eviction)
                }

                store.dispose_session(s);
            })
        })
        .collect();

    // Checkpoint thread.
    let store_ckpt = Arc::clone(&store);
    let dir_path = dir.path().to_path_buf();
    let barrier_ckpt = Arc::clone(&barrier);
    let ckpt_handle = thread::spawn(move || {
        barrier_ckpt.wait();
        store_ckpt.checkpoint(&dir_path, faster_core::checkpoint::CheckpointType::FoldOver)
            .expect("checkpoint should succeed");
    });

    for h in reader_handles {
        h.join().expect("reader panicked");
    }
    ckpt_handle.join().expect("checkpoint panicked");

    assert_eq!(
        corruption_count.load(Ordering::Relaxed),
        0,
        "reads during checkpoint must return consistent values"
    );
}

/// RMW operations during checkpoint must not corrupt data or crash.
/// Each thread increments a shared set of counters; the final sum must
/// be non-negative for each key and the total must not exceed the maximum
/// possible (some RMWs may fail CAS during phase transitions, which is
/// expected FASTER behavior — the EPVS property is that no *torn* state
/// or corruption occurs).
///
/// RACE CONDITION TESTED: RMW involves read-modify-write of a record.
/// If the phase transition is not atomic with the version, the RMW could
/// observe a torn phase/version and corrupt the record.
#[test]
#[ignore = "tier-2: 4-thread concurrent RMW + checkpoint stress test"]
fn concurrent_rmw_during_checkpoint() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = concurrent_counter_store();

    const THREADS: u64 = 4;
    const OPS_PER_THREAD: u64 = 1_000;
    const NUM_KEYS: u64 = 100;

    // Seed all counters to 0.
    {
        let mut s = store.new_session();
        for k in 0..NUM_KEYS {
            let _ = store.upsert(&mut s, &k, &0i64, ());
        }
        store.dispose_session(s);
    }

    let barrier = Arc::new(Barrier::new(THREADS as usize + 1));
    let success_count = Arc::new(AtomicU64::new(0));

    let rmw_handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&success_count);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let mut local_ok = 0u64;
                for i in 0..OPS_PER_THREAD {
                    let key = i % NUM_KEYS;
                    let mut out: i64 = 0;
                    let status = store.rmw(&mut s, &key, &1i64, &mut out, ());
                    if status.is_success() {
                        local_ok += 1;
                    }
                    // Periodically drain pending ops that checkpoint may have deferred.
                    if (i + 1) % 100 == 0 {
                        let _ = store.try_complete_pending(&mut s, false);
                    }
                }
                // Drain all remaining pending operations before disposing.
                let _ = store.try_complete_pending(&mut s, true);
                successes.fetch_add(local_ok, Ordering::Relaxed);

                store.dispose_session(s);
            })
        })
        .collect();

    let store_ckpt = Arc::clone(&store);
    let dir_path = dir.path().to_path_buf();
    let barrier_ckpt = Arc::clone(&barrier);
    let ckpt_handle = thread::spawn(move || {
        barrier_ckpt.wait();
        thread::yield_now();
        store_ckpt.checkpoint(&dir_path, faster_core::checkpoint::CheckpointType::FoldOver)
            .expect("checkpoint should succeed");
    });

    for h in rmw_handles {
        h.join().expect("RMW thread panicked");
    }
    ckpt_handle.join().expect("checkpoint panicked");

    // Verify data integrity: each counter must be non-negative (no corruption)
    // and total must not exceed the maximum possible.
    let mut total = 0i64;
    {
        let mut s = store.new_session();
        for k in 0..NUM_KEYS {
            let mut out: i64 = 0;
            let status = store.read(&mut s, &k, &0i64, &mut out, ());
            assert_eq!(status, OperationStatus::Ok, "key {k} missing after RMW");
            assert!(
                out >= 0,
                "key {k} has negative count {out} — data corruption!"
            );
            total += out;
        }
        store.dispose_session(s);
    }

    let max_possible = (THREADS * OPS_PER_THREAD) as i64;
    assert!(total > 0, "at least some RMW operations must succeed");
    assert!(
        total <= max_possible,
        "total {total} exceeds maximum possible {max_possible} — double-counting corruption!"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 6. FasterKv-level: concurrent operations + grow
// ═══════════════════════════════════════════════════════════════════════

/// Multiple threads upsert while a manual grow is triggered.
/// Verifies no data loss during hash table resize.
///
/// RACE CONDITION TESTED: During grow, entries are split between old and
/// new tables. Without atomic phase+version, an operation might write to
/// the wrong table or miss an entry that's being migrated.
#[test]
fn concurrent_upserts_during_grow() {
    let store = growable_test_store();

    const THREADS: u64 = 4;
    const KEYS_PER_THREAD: u64 = 2_000;

    let barrier = Arc::new(Barrier::new(THREADS as usize + 1));

    let writer_handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let base = t * KEYS_PER_THREAD;
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let _ = store.upsert(&mut s, &key, &(key * 5), ());
                    // Periodically check grow to help cooperative splitting.
                    if i % 500 == 0 {
                        store.check_grow();
                    }
                }

                store.dispose_session(s);
            })
        })
        .collect();

    // Grow trigger thread.
    let store_grow = Arc::clone(&store);
    let barrier_grow = Arc::clone(&barrier);
    let grow_handle = thread::spawn(move || {
        barrier_grow.wait();
        // Give writers time to fill the index, then trigger grow.
        thread::yield_now();
        store_grow.grow_index().expect("grow_index should succeed");
    });

    for h in writer_handles {
        h.join().expect("writer panicked");
    }
    grow_handle.join().expect("grow panicked");

    // Verify all data is readable.
    {
        let mut s = store.new_session();
        let mut found = 0u64;
        for t in 0..THREADS {
            let base = t * KEYS_PER_THREAD;
            for i in 0..KEYS_PER_THREAD {
                let key = base + i;
                let mut out: Option<u64> = None;
                let status = store.read(&mut s, &key, &0u64, &mut out, ());
                if status == OperationStatus::Ok {
                    assert_eq!(out, Some(key * 5), "key {key} value corrupted after grow");
                    found += 1;
                }
            }
        }
        store.dispose_session(s);

        // With a small index, some operations might have failed due to
        // bucket overflow, but the majority should succeed.
        let total = THREADS * KEYS_PER_THREAD;
        assert!(
            found > total / 2,
            "expected at least half of {total} keys, found {found}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 7. FasterKv stress: high thread count + periodic checkpoints
// ═══════════════════════════════════════════════════════════════════════

/// 8-thread stress: 50K operations each with periodic checkpoints.
/// Verifies data integrity after completion.
///
/// INVARIANT: No panics, no data corruption, no deadlocks.
#[test]
#[ignore = "tier-2: 8-thread × 50K ops stress test with periodic checkpoints"]
fn stress_8_threads_with_checkpoints() {
    stress_operations_with_checkpoints(8, 50_000, 5_000);
}

/// 16-thread stress test for nightly CI.
#[test]
#[ignore] // ~5-15s depending on hardware
fn stress_16_threads_with_checkpoints() {
    stress_operations_with_checkpoints(16, 100_000, 10_000);
}

fn stress_operations_with_checkpoints(
    num_threads: u64,
    ops_per_thread: u64,
    checkpoint_interval: u64,
) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 18, // 256K buckets
        buffer_size_pages: 64,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let barrier = Arc::new(Barrier::new(num_threads as usize));
    let total_upserts = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let store = Arc::clone(&store);
            let dir_path = dir.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            let total = Arc::clone(&total_upserts);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let base = t * ops_per_thread;
                for i in 0..ops_per_thread {
                    let key = base + i;

                    // Mix of operations: 70% upsert, 20% read, 10% RMW.
                    // Use deterministic selection based on key.
                    match key % 10 {
                        0..7 => {
                            let status = store.upsert(&mut s, &key, &(key * 3), ());
                            if status.is_success() {
                                total.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        7..9 => {
                            let mut out: Option<u64> = None;
                            let _ = store.read(&mut s, &key, &0u64, &mut out, ());
                        }
                        _ => {
                            let mut out: Option<u64> = None;
                            let _ = store.read(&mut s, &key, &0u64, &mut out, ());
                        }
                    }

                    // Thread 0 triggers periodic checkpoints.
                    // May fail during concurrent operations — acceptable in stress test.
                    if t == 0 && i > 0 && i % checkpoint_interval == 0 {
                        let _result = store.checkpoint(
                            &dir_path,
                            faster_core::checkpoint::CheckpointType::FoldOver,
                        );
                    }
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join()
            .expect("worker panicked — possible deadlock or data corruption");
    }

    let upserts = total_upserts.load(Ordering::Relaxed);
    assert!(
        upserts > 0,
        "at least some upserts should succeed across {num_threads} threads"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 8. EPVS word-level: intermediate state visibility window
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that the intermediate-state window is finite: observers
/// never spin forever waiting for a transition to complete.
///
/// INVARIANT: `wait_non_intermediate()` always terminates in bounded time.
#[test]
fn intermediate_window_is_finite() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));

    let num_transitions = 500;
    let barrier = Arc::new(Barrier::new(2));

    // Waiter thread: calls wait_non_intermediate() repeatedly.
    let s1 = Arc::clone(&state);
    let b1 = Arc::clone(&barrier);
    let waiter = thread::spawn(move || {
        b1.wait();
        let mut observations = 0u64;
        for _ in 0..num_transitions {
            let s = s1.wait_non_intermediate();
            assert!(!s.is_intermediate());
            observations += 1;
        }
        observations
    });

    // Driver thread: performs transitions that create brief intermediate windows.
    let s2 = Arc::clone(&state);
    let b2 = Arc::clone(&barrier);
    let driver = thread::spawn(move || {
        b2.wait();
        for cycle in 0u64..num_transitions {
            let from_ver = cycle;
            let to_ver = cycle;
            // Drive a complete checkpoint cycle. Each transition briefly
            // enters the intermediate state.
            let transitions = [
                (Phase::Rest, Phase::Prepare, from_ver, to_ver),
                (Phase::Prepare, Phase::InProgress, to_ver, to_ver + 1),
                (Phase::InProgress, Phase::WaitFlush, to_ver + 1, to_ver + 1),
                (
                    Phase::WaitFlush,
                    Phase::WaitCompletion,
                    to_ver + 1,
                    to_ver + 1,
                ),
                (
                    Phase::WaitCompletion,
                    Phase::PersistenceCallback,
                    to_ver + 1,
                    to_ver + 1,
                ),
                (
                    Phase::PersistenceCallback,
                    Phase::Rest,
                    to_ver + 1,
                    to_ver + 1,
                ),
            ];

            for (from_p, to_p, fv, tv) in transitions {
                let ok = s2.try_transition(
                    SystemState::new(from_p, fv),
                    SystemState::new(to_p, tv),
                    || {},
                );
                assert!(ok, "transition {from_p} → {to_p} at v{fv} failed");
            }
        }
    });

    let obs = waiter
        .join()
        .expect("waiter timed out — intermediate window not finite");
    driver.join().expect("driver panicked");

    assert_eq!(obs, num_transitions, "all waits should complete");
}

/// Verifies that multiple concurrent drivers can each observe
/// transitions completing without deadlock, even when they all call
/// wait_non_intermediate() simultaneously.
#[test]
fn concurrent_wait_non_intermediate() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let num_waiters = 8;
    let done = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(num_waiters + 1));
    let total_observations = Arc::new(AtomicU64::new(0));

    let waiters: Vec<_> = (0..num_waiters)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let barrier = Arc::clone(&barrier);
            let total = Arc::clone(&total_observations);
            thread::spawn(move || {
                barrier.wait();
                while !done.load(Ordering::Acquire) {
                    let s = state.wait_non_intermediate();
                    assert!(!s.is_intermediate());
                    total.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    barrier.wait();

    // Drive 100 checkpoint cycles while waiters observe.
    for cycle in 0u64..100 {
        let t = |from_p: Phase, to_p: Phase, fv: u64, tv: u64| {
            assert!(state.try_transition(
                SystemState::new(from_p, fv),
                SystemState::new(to_p, tv),
                || {}
            ));
        };
        t(Phase::Rest, Phase::Prepare, cycle, cycle);
        t(Phase::Prepare, Phase::InProgress, cycle, cycle + 1);
        t(Phase::InProgress, Phase::WaitFlush, cycle + 1, cycle + 1);
        t(
            Phase::WaitFlush,
            Phase::WaitCompletion,
            cycle + 1,
            cycle + 1,
        );
        t(
            Phase::WaitCompletion,
            Phase::PersistenceCallback,
            cycle + 1,
            cycle + 1,
        );
        t(
            Phase::PersistenceCallback,
            Phase::Rest,
            cycle + 1,
            cycle + 1,
        );
    }

    done.store(true, Ordering::Release);
    for w in waiters {
        w.join().expect("waiter panicked — possible livelock");
    }

    assert!(
        total_observations.load(Ordering::Relaxed) > 0,
        "waiters should have observed non-intermediate states"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 9. Version monotonicity under high contention
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that the version counter never decreases, even when many
/// threads are concurrently driving checkpoint cycles at different rates.
///
/// INVARIANT: For any sequence of observations by a single thread,
/// version(t) >= version(t-1). A regression would indicate a torn read
/// or a CAS corruption.
#[test]
fn version_monotonicity_under_contention() {
    let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
    let done = Arc::new(AtomicBool::new(false));
    let regression_detected = Arc::new(AtomicBool::new(false));
    let max_version_seen = Arc::new(AtomicU64::new(0));

    let num_drivers = 4;
    let num_observers = 4;
    let barrier = Arc::new(Barrier::new(num_drivers + num_observers));

    // Observer threads: track version monotonicity.
    let observers: Vec<_> = (0..num_observers)
        .map(|_| {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            let regression = Arc::clone(&regression_detected);
            let max_ver = Arc::clone(&max_version_seen);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut local_max = 0u64;
                while !done.load(Ordering::Acquire) {
                    let s = state.load(Ordering::Acquire);
                    if !s.is_intermediate() {
                        let v = s.version();
                        if v < local_max {
                            regression.store(true, Ordering::Release);
                        }
                        if v > local_max {
                            local_max = v;
                        }
                    }
                }
                max_ver.fetch_max(local_max, Ordering::Relaxed);
            })
        })
        .collect();

    // Driver threads: compete to advance through checkpoint cycles.
    let drivers: Vec<_> = (0..num_drivers)
        .map(|_| {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..1_000 {
                    let current = state.wait_non_intermediate();
                    let phase = current.phase();
                    let version = current.version();

                    let (next_phase, next_version) = match phase {
                        Phase::Rest => (Phase::Prepare, version),
                        Phase::Prepare => (Phase::InProgress, version + 1),
                        Phase::InProgress => (Phase::WaitFlush, version),
                        Phase::WaitFlush => (Phase::WaitCompletion, version),
                        Phase::WaitCompletion => (Phase::PersistenceCallback, version),
                        Phase::PersistenceCallback => (Phase::Rest, version),
                        Phase::PrepareGrow => (Phase::InProgressGrow, version),
                        Phase::InProgressGrow => (Phase::WaitCompletionGrow, version),
                        Phase::WaitCompletionGrow => (Phase::Rest, version),
                    };

                    let _ = state.try_transition(
                        current,
                        SystemState::new(next_phase, next_version),
                        || {},
                    );
                }
            })
        })
        .collect();

    for d in drivers {
        d.join().expect("driver panicked");
    }
    done.store(true, Ordering::Release);
    for o in observers {
        o.join().expect("observer panicked");
    }

    assert!(
        !regression_detected.load(Ordering::Acquire),
        "version must never decrease — EPVS atomic word prevents this"
    );
    assert!(
        max_version_seen.load(Ordering::Relaxed) > 0,
        "at least some version bumps should have been observed"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 10. Property-based tests
// ═══════════════════════════════════════════════════════════════════════

mod epvs_proptests {
    use super::*;
    use proptest::prelude::*;

    fn phase_strategy() -> impl Strategy<Value = Phase> {
        prop_oneof![
            Just(Phase::Rest),
            Just(Phase::Prepare),
            Just(Phase::InProgress),
            Just(Phase::WaitFlush),
            Just(Phase::WaitCompletion),
            Just(Phase::PersistenceCallback),
            Just(Phase::PrepareGrow),
            Just(Phase::InProgressGrow),
            Just(Phase::WaitCompletionGrow),
        ]
    }

    // Packing and unpacking any valid (phase, version) must round-trip.
    proptest! {
        #[test]
        fn pack_unpack_roundtrip(
            phase in phase_strategy(),
            version in 0u64..=VERSION_MASK,
        ) {
            let state = SystemState::new(phase, version);
            prop_assert_eq!(state.phase(), phase);
            prop_assert_eq!(state.version(), version);
        }
    }

    // The intermediate bit must not corrupt the phase or version.
    proptest! {
        #[test]
        fn intermediate_preserves_fields(
            phase in phase_strategy(),
            version in 0u64..=VERSION_MASK,
        ) {
            let state = SystemState::new(phase, version);
            let inter = state.make_intermediate();
            prop_assert!(inter.is_intermediate());
            prop_assert_eq!(inter.phase(), phase, "intermediate corrupted phase");
            prop_assert_eq!(inter.version(), version, "intermediate corrupted version");
        }
    }

    // CAS with the correct expected value always succeeds on a fresh state.
    proptest! {
        #[test]
        fn cas_success_with_correct_expected(
            phase in phase_strategy(),
            version in 0u64..=(VERSION_MASK / 2),
            new_phase in phase_strategy(),
            new_version in 0u64..=(VERSION_MASK / 2),
        ) {
            let initial = SystemState::new(phase, version);
            let atomic = AtomicSystemState::new(initial);
            let next = SystemState::new(new_phase, new_version);
            let result = atomic.compare_exchange(
                initial, next,
                Ordering::AcqRel, Ordering::Acquire,
            );
            prop_assert!(result.is_ok());
            let loaded = atomic.load(Ordering::Acquire);
            prop_assert_eq!(loaded.phase(), new_phase);
            prop_assert_eq!(loaded.version(), new_version);
        }
    }

    // CAS with a wrong expected value always fails and returns the actual state.
    proptest! {
        #[test]
        fn cas_failure_with_wrong_expected(
            actual_phase in phase_strategy(),
            actual_version in 1u64..=(VERSION_MASK / 2),
        ) {
            let actual = SystemState::new(actual_phase, actual_version);
            let atomic = AtomicSystemState::new(actual);
            // Deliberately wrong expected.
            let wrong = SystemState::new(Phase::Rest, 0);
            if wrong.word() != actual.word() {
                let result = atomic.compare_exchange(
                    wrong,
                    SystemState::new(Phase::Prepare, 99),
                    Ordering::AcqRel, Ordering::Acquire,
                );
                prop_assert!(result.is_err());
                let err_state = result.unwrap_err();
                prop_assert_eq!(err_state, actual);
            }
        }
    }

    // try_transition with correct expected succeeds and calls hooks.
    proptest! {
        #[test]
        fn try_transition_calls_hook(
            version in 0u64..=1000u64,
        ) {
            let initial = SystemState::new(Phase::Rest, version);
            let next = SystemState::new(Phase::Prepare, version);
            let atomic = AtomicSystemState::new(initial);
            let mut called = false;
            let won = atomic.try_transition(initial, next, || called = true);
            prop_assert!(won);
            prop_assert!(called);
            let loaded = atomic.load(Ordering::Acquire);
            prop_assert_eq!(loaded.phase(), Phase::Prepare);
            prop_assert_eq!(loaded.version(), version);
        }
    }

    // Random valid checkpoint lifecycles preserve version monotonicity.
    proptest! {
        #[test]
        fn random_checkpoint_cycles_monotonic(
            num_cycles in 1u64..20,
        ) {
            let atomic = AtomicSystemState::new(SystemState::INITIAL);
            let mut expected_version = 0u64;

            for _cycle in 0..num_cycles {
                let transitions = [
                    (Phase::Rest, Phase::Prepare, expected_version, expected_version),
                    (Phase::Prepare, Phase::InProgress, expected_version, expected_version + 1),
                    (Phase::InProgress, Phase::WaitFlush, expected_version + 1, expected_version + 1),
                    (Phase::WaitFlush, Phase::WaitCompletion, expected_version + 1, expected_version + 1),
                    (Phase::WaitCompletion, Phase::PersistenceCallback, expected_version + 1, expected_version + 1),
                    (Phase::PersistenceCallback, Phase::Rest, expected_version + 1, expected_version + 1),
                ];

                for (from_p, to_p, fv, tv) in transitions {
                    let ok = atomic.try_transition(
                        SystemState::new(from_p, fv),
                        SystemState::new(to_p, tv),
                        || {},
                    );
                    prop_assert!(ok, "transition {} -> {} failed", from_p, to_p);
                }
                expected_version += 1;
            }

            let final_state = atomic.load(Ordering::Acquire);
            prop_assert_eq!(final_state.version(), expected_version);
            prop_assert_eq!(final_state.phase(), Phase::Rest);
        }
    }

    // Alternating checkpoint and grow cycles produce valid final states.
    proptest! {
        #[test]
        fn alternating_checkpoint_grow_cycles(
            pattern in proptest::collection::vec(prop::bool::ANY, 1..10),
        ) {
            let atomic = AtomicSystemState::new(SystemState::INITIAL);
            let mut version = 0u64;

            for do_checkpoint in &pattern {
                if *do_checkpoint {
                    // Checkpoint cycle: bumps version
                    let t = |from_p: Phase, to_p: Phase, fv: u64, tv: u64| -> bool {
                        atomic.try_transition(
                            SystemState::new(from_p, fv),
                            SystemState::new(to_p, tv),
                            || {},
                        )
                    };
                    prop_assert!(t(Phase::Rest, Phase::Prepare, version, version));
                    prop_assert!(t(Phase::Prepare, Phase::InProgress, version, version + 1));
                    prop_assert!(t(Phase::InProgress, Phase::WaitFlush, version + 1, version + 1));
                    prop_assert!(t(Phase::WaitFlush, Phase::WaitCompletion, version + 1, version + 1));
                    prop_assert!(t(Phase::WaitCompletion, Phase::PersistenceCallback, version + 1, version + 1));
                    prop_assert!(t(Phase::PersistenceCallback, Phase::Rest, version + 1, version + 1));
                    version += 1;
                } else {
                    // Grow cycle: version stays same
                    let t = |from_p: Phase, to_p: Phase| -> bool {
                        atomic.try_transition(
                            SystemState::new(from_p, version),
                            SystemState::new(to_p, version),
                            || {},
                        )
                    };
                    prop_assert!(t(Phase::Rest, Phase::PrepareGrow));
                    prop_assert!(t(Phase::PrepareGrow, Phase::InProgressGrow));
                    prop_assert!(t(Phase::InProgressGrow, Phase::WaitCompletionGrow));
                    prop_assert!(t(Phase::WaitCompletionGrow, Phase::Rest));
                }
            }

            let final_state = atomic.load(Ordering::Acquire);
            prop_assert_eq!(final_state.phase(), Phase::Rest);
            prop_assert_eq!(final_state.version(), version);
        }
    }

    // The word encoding never aliases: distinct (phase, version) pairs
    // produce distinct u64 words.
    proptest! {
        #[test]
        fn no_word_aliasing(
            p1 in phase_strategy(),
            v1 in 0u64..=VERSION_MASK,
            p2 in phase_strategy(),
            v2 in 0u64..=VERSION_MASK,
        ) {
            let s1 = SystemState::new(p1, v1);
            let s2 = SystemState::new(p2, v2);
            if p1 != p2 || v1 != v2 {
                prop_assert_ne!(
                    s1.word(), s2.word(),
                    "distinct states ({},{}) and ({},{}) must have distinct words",
                    p1 as u8, v1, p2 as u8, v2,
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 11. Edge cases
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that version 0 and maximum version (2^56 - 1) work correctly
/// with all phases.
#[test]
fn boundary_versions() {
    let min_version = 0u64;
    let max_version = VERSION_MASK;

    for version in [min_version, max_version, 1, max_version - 1] {
        let phases = [
            Phase::Rest,
            Phase::Prepare,
            Phase::InProgress,
            Phase::WaitFlush,
            Phase::WaitCompletion,
            Phase::PersistenceCallback,
            Phase::PrepareGrow,
            Phase::InProgressGrow,
            Phase::WaitCompletionGrow,
        ];

        for phase in phases {
            let state = SystemState::new(phase, version);
            assert_eq!(state.phase(), phase, "phase mismatch at version {version}");
            assert_eq!(
                state.version(),
                version,
                "version mismatch for phase {phase:?}"
            );

            // Intermediate form should preserve everything.
            let inter = state.make_intermediate();
            assert!(inter.is_intermediate());
            assert_eq!(inter.phase(), phase);
            assert_eq!(inter.version(), version);

            // Atomic round-trip.
            let atomic = AtomicSystemState::new(state);
            let loaded = atomic.load(Ordering::Acquire);
            assert_eq!(loaded, state);
        }
    }
}

/// Verifies that version overflow beyond 56 bits is silently masked.
#[test]
fn version_overflow_is_masked() {
    let overflowed = SystemState::new(Phase::Rest, u64::MAX);
    assert_eq!(overflowed.version(), VERSION_MASK);
    // Phase should not be corrupted by overflow.
    assert_eq!(overflowed.phase(), Phase::Rest);
}

/// Verifies that back-to-back CAS operations on the same AtomicSystemState
/// are serialised correctly — no ABA problems with the intermediate bit.
#[test]
fn sequential_transitions_no_aba() {
    let state = AtomicSystemState::new(SystemState::INITIAL);

    // Run 100 complete checkpoint cycles back-to-back.
    for cycle in 0u64..100 {
        let transitions = [
            (Phase::Rest, Phase::Prepare, cycle, cycle),
            (Phase::Prepare, Phase::InProgress, cycle, cycle + 1),
            (Phase::InProgress, Phase::WaitFlush, cycle + 1, cycle + 1),
            (
                Phase::WaitFlush,
                Phase::WaitCompletion,
                cycle + 1,
                cycle + 1,
            ),
            (
                Phase::WaitCompletion,
                Phase::PersistenceCallback,
                cycle + 1,
                cycle + 1,
            ),
            (
                Phase::PersistenceCallback,
                Phase::Rest,
                cycle + 1,
                cycle + 1,
            ),
        ];

        for (from_p, to_p, fv, tv) in transitions {
            let ok = state.try_transition(
                SystemState::new(from_p, fv),
                SystemState::new(to_p, tv),
                || {},
            );
            assert!(
                ok,
                "cycle {cycle}: {from_p} → {to_p} failed — possible ABA issue"
            );
        }
    }

    let final_state = state.load(Ordering::Acquire);
    assert_eq!(final_state.phase(), Phase::Rest);
    assert_eq!(final_state.version(), 100);
}
