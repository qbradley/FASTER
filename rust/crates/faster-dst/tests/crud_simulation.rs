//! Integration tests for deterministic CRUD scheduling.
//!
//! These tests exercise [`SimulatedFasterKv`] and [`spawn_crud_worker`]
//! under the deterministic scheduler, verifying reproducibility and
//! correctness of FASTER operations.

use std::rc::Rc;

use faster_core::status::OperationStatus;
use faster_dst::harness::SimulationHarness;
use faster_dst::scheduler::DeterministicScheduler;
use faster_dst::sim_store::{CrudOp, CrudResult, SimulatedFasterKv, spawn_crud_worker};
use faster_dst::task::{SchedulerResult, TaskAction};

// ── Helpers ──────────────────────────────────────────────────────────────

/// Run a full simulation with the given seed and return collected results
/// for each worker.
fn run_simulation(seed: u64, workers: Vec<(&str, Vec<CrudOp>)>) -> Vec<Vec<CrudResult>> {
    let harness = SimulationHarness::new(seed);
    let store = Rc::new(SimulatedFasterKv::new(&harness));

    let mut sched = DeterministicScheduler::new(seed);
    let handles: Vec<_> = workers
        .into_iter()
        .map(|(name, ops)| spawn_crud_worker(&mut sched, Rc::clone(&store), name, ops))
        .collect();

    let outcome = sched.run_until_complete();
    assert!(
        matches!(outcome, SchedulerResult::AllComplete),
        "scheduler did not complete: {outcome}"
    );

    handles
        .into_iter()
        .map(|(_id, results)| results.borrow().clone())
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Same seed must produce identical results across 100 runs.
#[test]
fn concurrent_upserts_deterministic() {
    let seed = 0xDEAD_BEEF;
    let make_workers = || {
        vec![
            (
                "w0",
                vec![
                    CrudOp::Upsert { key: 1, value: 10 },
                    CrudOp::Upsert { key: 2, value: 20 },
                    CrudOp::Read { key: 1 },
                ],
            ),
            (
                "w1",
                vec![
                    CrudOp::Upsert { key: 1, value: 99 },
                    CrudOp::Read { key: 2 },
                    CrudOp::Read { key: 1 },
                ],
            ),
        ]
    };

    let baseline = run_simulation(seed, make_workers());
    for _ in 0..100 {
        let run = run_simulation(seed, make_workers());
        assert_eq!(run, baseline, "non-deterministic result detected!");
    }
}

/// Different seeds should (with high probability) produce different
/// interleaving results.
#[test]
fn different_seeds_explore_interleavings() {
    let make_workers = || {
        vec![
            (
                "w0",
                vec![
                    CrudOp::Upsert { key: 1, value: 10 },
                    CrudOp::Read { key: 1 },
                ],
            ),
            (
                "w1",
                vec![
                    CrudOp::Upsert { key: 1, value: 99 },
                    CrudOp::Read { key: 1 },
                ],
            ),
        ]
    };

    let mut distinct = std::collections::HashSet::new();
    for seed in 0..50 {
        let results = run_simulation(seed, make_workers());
        // Hash the debug representation as a rough equality key.
        distinct.insert(format!("{results:?}"));
    }

    // We expect at least 2 distinct outcomes across 50 seeds.
    assert!(
        distinct.len() >= 2,
        "expected different interleavings across seeds but got only {}: {distinct:#?}",
        distinct.len(),
    );
}

/// A read-after-write must see the written value when running sequentially
/// in a single worker.
#[test]
fn read_after_write_consistency() {
    let workers = vec![(
        "single",
        vec![
            CrudOp::Upsert { key: 42, value: 7 },
            CrudOp::Read { key: 42 },
        ],
    )];

    let results = run_simulation(1, workers);
    assert_eq!(
        results[0][1],
        CrudResult::Read(OperationStatus::Ok, Some(7)),
    );
}

/// RMW should produce correct results.
#[test]
fn rmw_correctness() {
    // With SimpleFunctions<u64, u64> RMW does a full-value replacement,
    // so the final value equals the last RMW input.
    let workers = vec![(
        "rmw-worker",
        vec![
            CrudOp::Upsert { key: 1, value: 0 },
            CrudOp::Rmw { key: 1, value: 10 },
            CrudOp::Rmw { key: 1, value: 20 },
            CrudOp::Read { key: 1 },
        ],
    )];

    let results = run_simulation(42, workers);
    // Final read should see value 20 (last RMW).
    match &results[0][3] {
        CrudResult::Read(OperationStatus::Ok, Some(v)) => {
            assert_eq!(*v, 20, "expected 20 from last RMW, got {v}");
        }
        other => panic!("expected Read(Ok, Some(20)), got {other:?}"),
    }
}

/// Delete then read must return NotFound.
#[test]
fn delete_then_read() {
    let workers = vec![(
        "del",
        vec![
            CrudOp::Upsert { key: 5, value: 50 },
            CrudOp::Delete { key: 5 },
            CrudOp::Read { key: 5 },
        ],
    )];

    let results = run_simulation(99, workers);
    match &results[0][2] {
        CrudResult::Read(status, _) => {
            // After deletion, read should return NotFound.
            assert_eq!(
                *status,
                OperationStatus::NotFound,
                "expected NotFound after delete, got {status:?}"
            );
        }
        other => panic!("expected Read result, got {other:?}"),
    }
}

/// Epoch protection must not panic: many concurrent workers touching
/// overlapping keys.
#[test]
fn epoch_protection_invariant() {
    let workers: Vec<(&str, Vec<CrudOp>)> = (0..8)
        .map(|i| {
            let ops: Vec<CrudOp> = (0..10)
                .flat_map(|j| {
                    vec![
                        CrudOp::Upsert {
                            key: j % 4,
                            value: i * 100 + j,
                        },
                        CrudOp::Read { key: j % 4 },
                    ]
                })
                .collect();
            let name: &str = Box::leak(format!("w{i}").into_boxed_str());
            (name, ops)
        })
        .collect();

    // Must not panic — epoch protection is sound.
    let _results = run_simulation(0xCAFE, workers);
}

/// A single worker should complete sequentially.
#[test]
fn single_worker_sequential() {
    let ops = vec![
        CrudOp::Upsert { key: 1, value: 1 },
        CrudOp::Upsert { key: 2, value: 2 },
        CrudOp::Read { key: 1 },
        CrudOp::Read { key: 2 },
    ];

    let results = run_simulation(7, vec![("solo", ops)]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].len(), 4);
    assert_eq!(
        results[0][2],
        CrudResult::Read(OperationStatus::Ok, Some(1)),
    );
    assert_eq!(
        results[0][3],
        CrudResult::Read(OperationStatus::Ok, Some(2)),
    );
}

/// Stress test with many workers and many ops.
#[test]
fn many_workers_stress() {
    let workers: Vec<(&str, Vec<CrudOp>)> = (0..16)
        .map(|i| {
            let ops: Vec<CrudOp> = (0..20)
                .map(|j| CrudOp::Upsert {
                    key: j,
                    value: i * 1000 + j,
                })
                .collect();
            let name: &str = Box::leak(format!("stress-{i}").into_boxed_str());
            (name, ops)
        })
        .collect();

    let _results = run_simulation(123, workers);
    // If we got here without a panic, the scheduler handled it.
}

/// Verify that the scheduler records trace events for CRUD steps.
#[test]
fn scheduler_traces_crud_steps() {
    let harness = SimulationHarness::new(42);
    let store = Rc::new(SimulatedFasterKv::new(&harness));

    let mut sched = DeterministicScheduler::new(42);
    let ops = vec![
        CrudOp::Upsert { key: 1, value: 10 },
        CrudOp::Read { key: 1 },
    ];
    let _ = spawn_crud_worker(&mut sched, Rc::clone(&store), "tracer", ops);

    let outcome = sched.run_until_complete();
    assert!(matches!(outcome, SchedulerResult::AllComplete));

    // The scheduler should have recorded at least 2 yield steps + 1 complete.
    let trace = sched.trace();
    assert!(
        trace.len() >= 3,
        "expected at least 3 trace events, got {}",
        trace.len(),
    );
}

/// Spawn workers via the closure-based API pattern.
#[test]
fn spawn_from_closure_pattern() {
    let harness = SimulationHarness::new(42);
    let store = Rc::new(SimulatedFasterKv::new(&harness));
    let mut sched = DeterministicScheduler::new(42);

    // Directly use scheduler.spawn with a custom closure that wraps CRUD.
    let store_ref = Rc::clone(&store);
    let manual_results = Rc::new(std::cell::RefCell::new(Vec::new()));
    let manual_results_handle = Rc::clone(&manual_results);

    let _id = sched.spawn("manual", {
        let mut step = 0u32;
        let mut session = None;
        move |_ctx| {
            if session.is_none() {
                session = Some(store_ref.new_session());
            }
            let s = session.as_mut().unwrap();
            step += 1;
            match step {
                1 => {
                    let st = store_ref.store().upsert(s, &100u64, &200u64, ()).status();
                    manual_results.borrow_mut().push(CrudResult::Upsert(st));
                    TaskAction::Yield
                }
                2 => {
                    let mut out: Option<u64> = None;
                    let st = store_ref
                        .store()
                        .read(s, &100u64, &0u64, &mut out, ())
                        .status();
                    manual_results.borrow_mut().push(CrudResult::Read(st, out));
                    TaskAction::Yield
                }
                _ => {
                    if let Some(s) = session.take() {
                        store_ref.dispose_session(s);
                    }
                    TaskAction::Complete
                }
            }
        }
    });

    let outcome = sched.run_until_complete();
    assert!(matches!(outcome, SchedulerResult::AllComplete));

    let res = manual_results_handle.borrow();
    assert_eq!(res.len(), 2);
    assert_eq!(res[1], CrudResult::Read(OperationStatus::Ok, Some(200)),);
}
