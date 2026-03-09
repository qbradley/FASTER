//! Scheduler integration tests.
//!
//! These tests verify determinism, interleaving diversity, deadlock detection,
//! channel ordering, time advancement, trace completeness, and task-local
//! storage across the [`DeterministicScheduler`] and supporting types.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use faster_dst::{
    BlockReason, DeterministicScheduler, SchedulerResult, SimChannel, TaskAction, TraceEvent,
};

// ── Helpers ───────────────────────────────────────────────────────────────

/// Collect the sequence of `TaskSelected` task-ids from the trace.
fn selected_sequence(sched: &DeterministicScheduler) -> Vec<u64> {
    sched
        .trace()
        .events()
        .iter()
        .filter_map(|e| match e {
            TraceEvent::TaskSelected { task_id, .. } => Some(task_id.as_u64()),
            _ => None,
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[test]
fn same_seed_identical_interleaving() {
    let reference = {
        let mut sched = DeterministicScheduler::new(42);
        for i in 0..4 {
            sched.spawn(format!("task-{i}"), {
                let mut count = 0;
                move |_ctx| {
                    count += 1;
                    if count < 4 {
                        TaskAction::Yield
                    } else {
                        TaskAction::Complete
                    }
                }
            });
        }
        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        selected_sequence(&sched)
    };

    for run in 0..100 {
        let mut sched = DeterministicScheduler::new(42);
        for i in 0..4 {
            sched.spawn(format!("task-{i}"), {
                let mut count = 0;
                move |_ctx| {
                    count += 1;
                    if count < 4 {
                        TaskAction::Yield
                    } else {
                        TaskAction::Complete
                    }
                }
            });
        }
        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        let seq = selected_sequence(&sched);
        assert_eq!(
            seq, reference,
            "run {run} diverged from reference interleaving"
        );
    }
}

#[test]
fn different_seeds_different_interleavings() {
    let mut patterns: HashSet<Vec<u64>> = HashSet::new();

    for seed in 0..10u64 {
        let mut sched = DeterministicScheduler::new(seed * 1_000_003);
        for i in 0..4 {
            sched.spawn(format!("task-{i}"), {
                let mut count = 0;
                move |_ctx| {
                    count += 1;
                    if count < 4 {
                        TaskAction::Yield
                    } else {
                        TaskAction::Complete
                    }
                }
            });
        }
        let _ = sched.run_until_complete();
        patterns.insert(selected_sequence(&sched));
    }

    assert!(
        patterns.len() >= 3,
        "expected at least 3 distinct interleavings from 10 seeds, got {}",
        patterns.len()
    );
}

#[test]
fn channel_deterministic() {
    // Two runs with the same seed must produce the same delivery order.
    fn run_with_seed(seed: u64) -> Vec<u64> {
        let mut sched = DeterministicScheduler::new(seed);
        let channel: Rc<RefCell<SimChannel<u64>>> = Rc::new(RefCell::new(SimChannel::new()));
        let received: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

        // Producer: sends 0..5 then completes.
        let ch = Rc::clone(&channel);
        sched.spawn("producer", {
            let mut n = 0u64;
            move |_ctx| {
                if n < 5 {
                    let mut ch = ch.borrow_mut();
                    ch.send(n);
                    n += 1;
                    TaskAction::Yield
                } else {
                    TaskAction::Complete
                }
            }
        });

        // Consumer: tries to recv; yields if empty, completes after 5 items.
        let ch = Rc::clone(&channel);
        let recv = Rc::clone(&received);
        sched.spawn("consumer", {
            move |_ctx| {
                let mut ch = ch.borrow_mut();
                if let Some(v) = ch.try_recv() {
                    recv.borrow_mut().push(v);
                    if recv.borrow().len() >= 5 {
                        return TaskAction::Complete;
                    }
                }
                TaskAction::Yield
            }
        });

        let _ = sched.run_until_complete();
        received.borrow().clone()
    }

    let ref_run = run_with_seed(99);
    for _ in 0..50 {
        assert_eq!(run_with_seed(99), ref_run);
    }
}

#[test]
fn deadlock_detection() {
    let mut sched = DeterministicScheduler::new(1);
    sched.spawn("blocker-a", |_ctx| TaskAction::Park(BlockReason::Channel));
    sched.spawn("blocker-b", |_ctx| {
        TaskAction::Park(BlockReason::IoCompletion)
    });

    let result = sched.run_until_complete();
    match result {
        SchedulerResult::Deadlock { blocked_tasks } => {
            assert_eq!(blocked_tasks.len(), 2, "expected 2 blocked tasks");
        }
        SchedulerResult::AllComplete => panic!("should have detected deadlock"),
    }
}

#[test]
fn time_advancement_deterministic() {
    fn run_with_seed(seed: u64) -> (Vec<u64>, Duration) {
        let mut sched = DeterministicScheduler::new(seed);
        let log: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

        for i in 0..3u64 {
            let log = Rc::clone(&log);
            // Each task sleeps for (i+1)*100 ms then completes.
            sched.spawn(format!("sleeper-{i}"), {
                let mut step = 0;
                move |_ctx| {
                    step += 1;
                    if step == 1 {
                        let deadline = Duration::from_millis((i + 1) * 100);
                        TaskAction::Park(BlockReason::Time(deadline))
                    } else {
                        log.borrow_mut().push(i);
                        TaskAction::Complete
                    }
                }
            });
        }

        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        let clock = sched.clock().now();
        (log.borrow().clone(), clock)
    }

    let (ref_log, ref_clock) = run_with_seed(77);
    for _ in 0..50 {
        let (log, clock) = run_with_seed(77);
        assert_eq!(log, ref_log);
        assert_eq!(clock, ref_clock);
    }
    // Clock should have advanced to at least the largest deadline.
    assert!(ref_clock >= Duration::from_millis(300));
}

#[test]
fn interleaving_diversity() {
    let mut patterns: HashSet<Vec<u64>> = HashSet::new();

    for seed in 0..10u64 {
        let mut sched = DeterministicScheduler::new(seed.wrapping_mul(6_700_417));
        for i in 0..4 {
            sched.spawn(format!("task-{i}"), {
                let mut count = 0;
                move |_ctx| {
                    count += 1;
                    if count < 6 {
                        TaskAction::Yield
                    } else {
                        TaskAction::Complete
                    }
                }
            });
        }
        let _ = sched.run_until_complete();
        patterns.insert(selected_sequence(&sched));
    }

    assert!(
        patterns.len() >= 3,
        "expected at least 3 distinct orderings, got {}",
        patterns.len()
    );
}

#[test]
fn trace_records_all_events() {
    let mut sched = DeterministicScheduler::new(42);
    sched.spawn("only-task", {
        let mut step = 0;
        move |_ctx| {
            step += 1;
            if step < 3 {
                TaskAction::Yield
            } else {
                TaskAction::Complete
            }
        }
    });

    let _ = sched.run_until_complete();
    let events = sched.trace().events();

    // Should contain exactly: Spawned, Selected, Yielded, Selected, Yielded, Selected, Complete
    let spawned = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::TaskSpawned { .. }))
        .count();
    let selected = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::TaskSelected { .. }))
        .count();
    let yielded = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::TaskYielded { .. }))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::TaskComplete { .. }))
        .count();

    assert_eq!(spawned, 1);
    assert_eq!(selected, 3); // 2 yields + 1 complete
    assert_eq!(yielded, 2);
    assert_eq!(completed, 1);
}

#[test]
fn task_local_storage() {
    let mut sched = DeterministicScheduler::new(1);
    let observed: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

    let obs = Rc::clone(&observed);
    sched.spawn("local-user", {
        let mut step = 0u64;
        move |ctx| {
            step += 1;
            match step {
                1 => {
                    // Store a value in task-local storage.
                    ctx.set_local(42u64);
                    TaskAction::Yield
                }
                2 => {
                    // Read it back — should persist across steps.
                    let v = ctx.get_local::<u64>().copied().unwrap_or(0);
                    obs.borrow_mut().push(v);
                    TaskAction::Yield
                }
                3 => {
                    // Overwrite.
                    ctx.set_local(99u64);
                    let v = ctx.get_local::<u64>().copied().unwrap_or(0);
                    obs.borrow_mut().push(v);
                    TaskAction::Complete
                }
                _ => unreachable!(),
            }
        }
    });

    let _ = sched.run_until_complete();
    assert_eq!(*observed.borrow(), vec![42, 99]);
}
