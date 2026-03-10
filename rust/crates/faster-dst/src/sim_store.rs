//! [`SimulatedFasterKv`] — FASTER store configured for deterministic simulation.
//!
//! Wraps [`FasterKv`] in an `Rc` for shared
//! single-threaded ownership and provides [`spawn_crud_worker`] to schedule
//! a sequence of CRUD operations as scheduler steps.

use std::rc::Rc;

use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, FasterSession, SimpleFunctions};

use crate::harness::SimulationHarness;
use crate::scheduler::DeterministicScheduler;
use crate::task::{TaskAction, TaskId};

// ── CrudOp / CrudResult ─────────────────────────────────────────────────

/// A CRUD operation to be executed by a simulation worker.
#[derive(Debug, Clone)]
pub enum CrudOp {
    /// Insert or replace `(key, value)`.
    Upsert {
        /// Key.
        key: u64,
        /// Value.
        value: u64,
    },
    /// Read the value for `key`.
    Read {
        /// Key.
        key: u64,
    },
    /// Read-modify-write: apply `value` to the record at `key`.
    Rmw {
        /// Key.
        key: u64,
        /// Input value for the RMW callback.
        value: u64,
    },
    /// Delete the record at `key`.
    Delete {
        /// Key.
        key: u64,
    },
}

/// Outcome of a single [`CrudOp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrudResult {
    /// Result of an [`CrudOp::Upsert`].
    Upsert(OperationStatus),
    /// Result of a [`CrudOp::Read`].
    Read(OperationStatus, Option<u64>),
    /// Result of a [`CrudOp::Rmw`].
    Rmw(OperationStatus, Option<u64>),
    /// Result of a [`CrudOp::Delete`].
    Delete(OperationStatus),
}

// ── SimulatedFasterKv ───────────────────────────────────────────────────

/// A [`FasterKv`] store configured for deterministic simulation.
///
/// All operations run on a single OS thread, so `Rc` is used instead of
/// `Arc`.  Each worker task creates its own [`FasterSession`].
pub struct SimulatedFasterKv {
    store: FasterKv<SimpleFunctions<u64, u64>>,
}

impl SimulatedFasterKv {
    /// Create a new simulation store backed by an in-memory device.
    pub fn new(harness: &SimulationHarness) -> Self {
        Self {
            store: harness.create_store(),
        }
    }

    /// Create a new simulation store with a custom configuration.
    pub fn new_with_config(harness: &SimulationHarness, config: FasterKvConfig) -> Self {
        Self {
            store: FasterKv::new(config, SimpleFunctions::default(), harness.create_device()),
        }
    }

    /// Create a file-backed simulation store (for checkpoint/recovery tests).
    pub fn new_file_backed(harness: &SimulationHarness) -> Self {
        Self {
            store: harness.create_file_store(),
        }
    }

    /// Reference to the underlying [`FasterKv`] store.
    pub fn store(&self) -> &FasterKv<SimpleFunctions<u64, u64>> {
        &self.store
    }

    /// Create a new session.
    pub fn new_session(&self) -> FasterSession<SimpleFunctions<u64, u64>> {
        self.store.new_session()
    }

    /// Dispose a session.
    pub fn dispose_session(&self, session: FasterSession<SimpleFunctions<u64, u64>>) {
        self.store.dispose_session(session);
    }
}

impl std::fmt::Debug for SimulatedFasterKv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatedFasterKv").finish_non_exhaustive()
    }
}

// ── spawn_crud_worker ───────────────────────────────────────────────────

/// Spawn a CRUD worker task on the scheduler.
///
/// The worker executes `operations` in sequence, yielding after each one.
/// Each operation is one "step" from the scheduler's perspective.
///
/// Lifecycle:
/// 1. First step — creates a session.
/// 2. Subsequent steps — one CRUD operation each (yields between them).
/// 3. Final step — disposes the session and completes.
///
/// Returns the [`TaskId`] and a shared handle to the result vector
/// (populated as the worker runs).
pub fn spawn_crud_worker(
    scheduler: &mut DeterministicScheduler,
    store: Rc<SimulatedFasterKv>,
    name: impl Into<String>,
    operations: Vec<CrudOp>,
) -> (TaskId, Rc<std::cell::RefCell<Vec<CrudResult>>>) {
    let results: Rc<std::cell::RefCell<Vec<CrudResult>>> = Rc::new(std::cell::RefCell::new(
        Vec::with_capacity(operations.len()),
    ));
    let results_handle = Rc::clone(&results);

    let id = scheduler.spawn(name, {
        let mut ops_iter = operations.into_iter();
        let mut session: Option<FasterSession<SimpleFunctions<u64, u64>>> = None;

        move |_ctx| {
            // Lazily create the session on the first step.
            if session.is_none() {
                session = Some(store.store().new_session());
            }
            let sess = session.as_mut().unwrap();

            if let Some(op) = ops_iter.next() {
                let result = execute_op(store.store(), sess, &op);
                results.borrow_mut().push(result);
                TaskAction::Yield
            } else {
                // All operations done — dispose session and complete.
                if let Some(s) = session.take() {
                    store.store().dispose_session(s);
                }
                TaskAction::Complete
            }
        }
    });

    (id, results_handle)
}

/// Execute a single CRUD operation and return the result.
fn execute_op(
    store: &FasterKv<SimpleFunctions<u64, u64>>,
    session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    op: &CrudOp,
) -> CrudResult {
    match *op {
        CrudOp::Upsert { key, value } => {
            let outcome = store.upsert(session, &key, &value, ());
            CrudResult::Upsert(outcome.status())
        }
        CrudOp::Read { key } => {
            let mut output: Option<u64> = None;
            let outcome = store.read(session, &key, &0u64, &mut output, ());
            CrudResult::Read(outcome.status(), output)
        }
        CrudOp::Rmw { key, value } => {
            let mut output: Option<u64> = None;
            let outcome = store.rmw(session, &key, &value, &mut output, ());
            CrudResult::Rmw(outcome.status(), output)
        }
        CrudOp::Delete { key } => {
            let outcome = store.delete(session, &key, ());
            CrudResult::Delete(outcome.status())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_store_basic_crud() {
        let harness = SimulationHarness::new(42);
        let sim = SimulatedFasterKv::new(&harness);
        let mut s = sim.new_session();

        let status = sim.store().upsert(&mut s, &1u64, &100u64, ());
        assert!(status.is_success());

        let val: Option<u64> = sim.store().read_simple(&mut s, &1u64);
        assert_eq!(val, Some(100));

        sim.dispose_session(s);
    }

    #[test]
    fn spawn_worker_executes_all_ops() {
        let harness = SimulationHarness::new(42);
        let store = Rc::new(SimulatedFasterKv::new(&harness));

        let ops = vec![
            CrudOp::Upsert { key: 1, value: 10 },
            CrudOp::Upsert { key: 2, value: 20 },
            CrudOp::Read { key: 1 },
        ];

        let mut sched = DeterministicScheduler::new(42);
        let (_id, results) = spawn_crud_worker(&mut sched, Rc::clone(&store), "w0", ops);

        let outcome = sched.run_until_complete();
        assert!(
            matches!(outcome, crate::task::SchedulerResult::AllComplete),
            "scheduler did not complete: {outcome}"
        );

        let res = results.borrow();
        assert_eq!(res.len(), 3);
        assert!(matches!(
            res[2],
            CrudResult::Read(OperationStatus::Ok, Some(10))
        ));
    }
}
