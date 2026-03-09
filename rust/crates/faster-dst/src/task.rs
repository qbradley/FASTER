//! Task model for deterministic cooperative scheduling.
//!
//! Each task is a step-function ([`FnMut`]) that the scheduler calls repeatedly.
//! Every invocation executes one logical "step" and returns a [`TaskAction`]
//! telling the scheduler what to do next.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

// ── TaskId ────────────────────────────────────────────────────────────────

/// Unique, opaque identifier for a scheduled task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub(crate) u64);

impl TaskId {
    /// The raw numeric identifier (useful for tracing / display).
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

// ── TaskAction ────────────────────────────────────────────────────────────

/// Result of executing one step of a task.
#[derive(Debug)]
pub enum TaskAction {
    /// Task completed — remove from the scheduler.
    Complete,
    /// Voluntary yield — return to the ready queue.
    Yield,
    /// Block until unparked — move to the blocked queue.
    Park(BlockReason),
}

// ── BlockReason ───────────────────────────────────────────────────────────

/// Why a task is blocked.
#[derive(Debug, Clone)]
pub enum BlockReason {
    /// Waiting for a channel message.
    Channel,
    /// Waiting for an I/O operation to complete.
    IoCompletion,
    /// Sleeping until a simulated deadline (absolute duration from epoch).
    Time(Duration),
}

impl fmt::Display for BlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel => write!(f, "Channel"),
            Self::IoCompletion => write!(f, "IoCompletion"),
            Self::Time(d) => write!(f, "Time({d:?})"),
        }
    }
}

// ── TaskContext ────────────────────────────────────────────────────────────

/// Per-step context handed to every task invocation.
///
/// Provides the task's identity and a type-keyed local-storage map that
/// persists across steps (useful for e.g. epoch thread identity in Phase 3).
pub struct TaskContext {
    task_id: TaskId,
    locals: HashMap<TypeId, Box<dyn Any>>,
}

impl TaskContext {
    /// Create a fresh context for `task_id`.
    pub(crate) fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            locals: HashMap::new(),
        }
    }

    /// The identity of the currently executing task.
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// Store a value in task-local storage, keyed by its concrete type.
    ///
    /// Overwrites any previous value of the same type.
    pub fn set_local<T: 'static>(&mut self, value: T) {
        self.locals.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Retrieve a reference to a task-local value by type.
    pub fn get_local<T: 'static>(&self) -> Option<&T> {
        self.locals
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
    }
}

impl fmt::Debug for TaskContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskContext")
            .field("task_id", &self.task_id)
            .field("locals_count", &self.locals.len())
            .finish()
    }
}

// ── TaskState ─────────────────────────────────────────────────────────────

/// Internal bookkeeping state of a task.
#[derive(Debug)]
pub(crate) enum TaskState {
    Ready,
    #[allow(dead_code)] // stored for Debug output; scheduler uses blocked_tasks map
    Blocked(BlockReason),
    Complete,
}

// ── SimTask ───────────────────────────────────────────────────────────────

/// A cooperative task managed by the [`DeterministicScheduler`](crate::DeterministicScheduler).
pub struct SimTask {
    /// Unique task identifier.
    pub(crate) id: TaskId,
    /// Human-readable label (for tracing / debugging).
    pub(crate) name: String,
    /// The step function called each time this task is selected to run.
    pub(crate) step_fn: Box<dyn FnMut(&mut TaskContext) -> TaskAction>,
    /// Per-task context surviving across steps.
    pub(crate) context: TaskContext,
    /// Current lifecycle state.
    pub(crate) state: TaskState,
}

impl SimTask {
    /// Create a new task.
    pub(crate) fn new(
        id: TaskId,
        name: String,
        step_fn: Box<dyn FnMut(&mut TaskContext) -> TaskAction>,
    ) -> Self {
        Self {
            context: TaskContext::new(id),
            id,
            name,
            step_fn,
            state: TaskState::Ready,
        }
    }
}

impl fmt::Debug for SimTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimTask")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", &self.state)
            .finish()
    }
}

// ── SchedulerResult ───────────────────────────────────────────────────────

/// Outcome of running the scheduler to quiescence.
#[derive(Debug)]
#[must_use]
pub enum SchedulerResult {
    /// Every spawned task returned [`TaskAction::Complete`].
    AllComplete,
    /// No more progress is possible: all remaining tasks are blocked and
    /// nothing can unpark them.
    Deadlock {
        /// The blocked tasks and their respective reasons.
        blocked_tasks: Vec<(TaskId, BlockReason)>,
    },
}

impl fmt::Display for SchedulerResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllComplete => write!(f, "AllComplete"),
            Self::Deadlock { blocked_tasks } => {
                write!(f, "Deadlock({} blocked tasks:", blocked_tasks.len())?;
                for (id, reason) in blocked_tasks {
                    write!(f, " {id}={reason}")?;
                }
                write!(f, ")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_context_local_storage() {
        let mut ctx = TaskContext::new(TaskId(0));
        assert!(ctx.get_local::<u64>().is_none());

        ctx.set_local(42u64);
        assert_eq!(ctx.get_local::<u64>(), Some(&42));

        // Different type doesn't conflict
        ctx.set_local("hello");
        assert_eq!(ctx.get_local::<u64>(), Some(&42));
        assert_eq!(ctx.get_local::<&str>(), Some(&"hello"));

        // Overwrite
        ctx.set_local(99u64);
        assert_eq!(ctx.get_local::<u64>(), Some(&99));
    }

    #[test]
    fn task_id_display() {
        let id = TaskId(7);
        assert_eq!(format!("{id}"), "TaskId(7)");
    }

    #[test]
    fn scheduler_result_display() {
        let r = SchedulerResult::AllComplete;
        assert_eq!(format!("{r}"), "AllComplete");

        let r = SchedulerResult::Deadlock {
            blocked_tasks: vec![(TaskId(1), BlockReason::Channel)],
        };
        assert!(format!("{r}").contains("Deadlock"));
    }
}
