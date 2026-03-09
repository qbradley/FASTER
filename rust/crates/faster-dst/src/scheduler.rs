//! Cooperative, single-threaded, deterministic task scheduler.
//!
//! [`DeterministicScheduler`] replaces OS thread scheduling with a
//! seed-controlled PRNG that selects which ready task runs next.  Every
//! task is a step-function ([`FnMut`]) called repeatedly on the **same**
//! thread — there are no OS threads involved.
//!
//! # Determinism guarantee
//!
//! Given the same seed and the same set of spawned tasks (in the same
//! order), the scheduler will always execute exactly the same interleaving
//! of task steps.

use std::collections::HashMap;

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};

use crate::clock::SimulatedClock;
use crate::task::{
    BlockReason, SchedulerResult, SimTask, TaskAction, TaskContext, TaskId, TaskState,
};
use crate::trace::{SimulationTrace, TraceEvent};

/// Cooperative, single-threaded, deterministic scheduler.
///
/// Tasks are step-functions executed on the calling thread.  The PRNG seed
/// controls task-selection order, guaranteeing reproducibility.
#[derive(Debug)]
pub struct DeterministicScheduler {
    rng: SmallRng,
    seed: u64,
    tasks: HashMap<TaskId, SimTask>,
    ready_queue: Vec<TaskId>,
    blocked_tasks: HashMap<TaskId, BlockReason>,
    next_task_id: u64,
    clock: SimulatedClock,
    trace: SimulationTrace,
    step_count: u64,
}

impl DeterministicScheduler {
    /// Create a scheduler seeded with `seed`.
    ///
    /// The same seed always produces the same scheduling decisions.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: SmallRng::seed_from_u64(seed),
            seed,
            tasks: HashMap::new(),
            ready_queue: Vec::new(),
            blocked_tasks: HashMap::new(),
            next_task_id: 0,
            clock: SimulatedClock::new(),
            trace: SimulationTrace::new(),
            step_count: 0,
        }
    }

    /// The seed this scheduler was created with.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Spawn a new task and return its [`TaskId`].
    ///
    /// The task is immediately placed on the ready queue.
    pub fn spawn(
        &mut self,
        name: impl Into<String>,
        step_fn: impl FnMut(&mut TaskContext) -> TaskAction + 'static,
    ) -> TaskId {
        let id = TaskId(self.next_task_id);
        self.next_task_id += 1;

        let name = name.into();
        self.trace.record(TraceEvent::TaskSpawned {
            task_id: id,
            name: name.clone(),
        });

        let task = SimTask::new(id, name, Box::new(step_fn));
        self.tasks.insert(id, task);
        self.ready_queue.push(id);
        id
    }

    /// Run all tasks until every one has completed or a deadlock is detected.
    pub fn run_until_complete(&mut self) -> SchedulerResult {
        loop {
            if !self.run_step() {
                return self.current_result();
            }
        }
    }

    /// Run until no task is ready (all are blocked or complete).
    ///
    /// Unlike [`run_until_complete`](Self::run_until_complete), this will
    /// **not** attempt to advance simulated time to unblock sleeping tasks.
    pub fn run_until_idle(&mut self) -> SchedulerResult {
        while !self.ready_queue.is_empty() {
            self.run_one_step();
        }
        self.current_result()
    }

    /// Unpark a blocked task, moving it back to the ready queue.
    ///
    /// This is the mechanism by which channels, I/O completions, and the
    /// test harness wake tasks.
    ///
    /// # Panics
    ///
    /// Panics if `task_id` is not currently blocked.
    pub fn unpark(&mut self, task_id: TaskId) {
        let removed = self.blocked_tasks.remove(&task_id);
        assert!(
            removed.is_some(),
            "unpark called on {task_id} which is not blocked"
        );

        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.state = TaskState::Ready;
        }
        self.ready_queue.push(task_id);

        self.trace.record(TraceEvent::TaskUnparked { task_id });
    }

    /// The execution trace.
    pub fn trace(&self) -> &SimulationTrace {
        &self.trace
    }

    /// The simulated clock.
    pub fn clock(&self) -> &SimulatedClock {
        &self.clock
    }

    /// Total number of task steps executed so far.
    pub fn step_count(&self) -> u64 {
        self.step_count
    }

    /// Number of tasks currently on the ready queue.
    pub fn ready_count(&self) -> usize {
        self.ready_queue.len()
    }

    /// Number of currently blocked tasks.
    pub fn blocked_count(&self) -> usize {
        self.blocked_tasks.len()
    }

    // ── internals ─────────────────────────────────────────────────────────

    /// Attempt one scheduling step.  Returns `true` if progress was made.
    fn run_step(&mut self) -> bool {
        if self.ready_queue.is_empty() {
            // Try to unblock time-sleeping tasks.
            self.advance_time_if_needed();
            if self.ready_queue.is_empty() {
                return false;
            }
        }
        self.run_one_step();
        true
    }

    /// Execute exactly one task step, assuming the ready queue is non-empty.
    fn run_one_step(&mut self) {
        debug_assert!(!self.ready_queue.is_empty());

        // Deterministic task selection via PRNG.
        let idx = rand_range_usize(&mut self.rng, self.ready_queue.len());
        let task_id = self.ready_queue.swap_remove(idx);

        self.trace.record(TraceEvent::TaskSelected {
            task_id,
            step: self.step_count,
        });

        // Execute one step of the selected task.
        let task = self
            .tasks
            .get_mut(&task_id)
            .expect("ready-queue references a missing task");
        let action = (task.step_fn)(&mut task.context);
        self.step_count += 1;

        match action {
            TaskAction::Complete => {
                task.state = TaskState::Complete;
                self.trace.record(TraceEvent::TaskComplete { task_id });
            }
            TaskAction::Yield => {
                task.state = TaskState::Ready;
                self.ready_queue.push(task_id);
                self.trace.record(TraceEvent::TaskYielded { task_id });
            }
            TaskAction::Park(reason) => {
                task.state = TaskState::Blocked(reason.clone());
                self.blocked_tasks.insert(task_id, reason.clone());
                self.trace
                    .record(TraceEvent::TaskParked { task_id, reason });
            }
        }
    }

    /// If all ready tasks are gone and some tasks are blocked on time,
    /// advance the simulated clock to the earliest deadline and unpark
    /// all tasks whose deadline has been reached.
    fn advance_time_if_needed(&mut self) {
        // Find the earliest time deadline among blocked tasks.
        let earliest = self
            .blocked_tasks
            .iter()
            .filter_map(|(id, reason)| match reason {
                BlockReason::Time(deadline) => Some((*id, *deadline)),
                _ => None,
            })
            .min_by_key(|(_, d)| *d);

        if let Some((_, target_time)) = earliest {
            self.clock.set(target_time);
            self.trace
                .record(TraceEvent::TimeAdvanced { to: target_time });

            // Collect all tasks whose deadline has been reached.
            let to_unpark: Vec<TaskId> = self
                .blocked_tasks
                .iter()
                .filter_map(|(id, reason)| match reason {
                    BlockReason::Time(d) if *d <= target_time => Some(*id),
                    _ => None,
                })
                .collect();

            for id in to_unpark {
                self.blocked_tasks.remove(&id);
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.state = TaskState::Ready;
                }
                self.ready_queue.push(id);
                self.trace.record(TraceEvent::TaskUnparked { task_id: id });
            }
        }
    }

    /// Build a [`SchedulerResult`] based on current state.
    fn current_result(&self) -> SchedulerResult {
        if self.blocked_tasks.is_empty() {
            SchedulerResult::AllComplete
        } else {
            SchedulerResult::Deadlock {
                blocked_tasks: self
                    .blocked_tasks
                    .iter()
                    .map(|(id, reason)| (*id, reason.clone()))
                    .collect(),
            }
        }
    }
}

/// Generate a uniform `usize` in `[0, upper)`.
fn rand_range_usize(rng: &mut SmallRng, upper: usize) -> usize {
    debug_assert!(upper > 0);
    (rng.next_u64() as usize) % upper
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scheduler_completes_immediately() {
        let mut sched = DeterministicScheduler::new(1);
        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        assert_eq!(sched.step_count(), 0);
    }

    #[test]
    fn single_task_runs_to_completion() {
        let mut sched = DeterministicScheduler::new(1);
        sched.spawn("once", |_ctx| TaskAction::Complete);

        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        assert_eq!(sched.step_count(), 1);
    }

    #[test]
    fn yielding_task_gets_rescheduled() {
        let mut sched = DeterministicScheduler::new(1);
        sched.spawn("yield-3", {
            let mut n = 0;
            move |_ctx| {
                n += 1;
                if n < 3 {
                    TaskAction::Yield
                } else {
                    TaskAction::Complete
                }
            }
        });

        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
        assert_eq!(sched.step_count(), 3);
    }

    #[test]
    fn unpark_wakes_blocked_task() {
        let mut sched = DeterministicScheduler::new(1);
        let id = sched.spawn("blocker", {
            let mut step = 0;
            move |_ctx| {
                step += 1;
                if step == 1 {
                    TaskAction::Park(BlockReason::Channel)
                } else {
                    TaskAction::Complete
                }
            }
        });

        // Run until idle — task will park.
        let result = sched.run_until_idle();
        assert!(matches!(result, SchedulerResult::Deadlock { .. }));
        assert_eq!(sched.blocked_count(), 1);

        // Unpark and run to completion.
        sched.unpark(id);
        let result = sched.run_until_complete();
        assert!(matches!(result, SchedulerResult::AllComplete));
    }
}
