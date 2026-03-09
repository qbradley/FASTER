//! Structured execution trace for deterministic simulation runs.
//!
//! [`SimulationTrace`] records every scheduling decision, park/unpark event,
//! and time advancement, satisfying FR-017 (execution trace).  Traces are
//! useful both for debugging failed seeds and for asserting determinism
//! (same seed → identical trace).

use std::time::Duration;

use crate::task::{BlockReason, TaskId};

// ── IoOp ──────────────────────────────────────────────────────────────────

/// Descriptor for an I/O operation recorded in the trace.
#[derive(Debug, Clone)]
pub enum IoOp {
    /// A read at the given byte offset and length.
    Read { offset: u64, len: usize },
    /// A write at the given byte offset and length.
    Write { offset: u64, len: usize },
}

// ── TraceEvent ────────────────────────────────────────────────────────────

/// A single event in the simulation trace.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// A new task was added to the scheduler.
    TaskSpawned { task_id: TaskId, name: String },
    /// The scheduler selected this task to run its next step.
    TaskSelected { task_id: TaskId, step: u64 },
    /// The task voluntarily yielded back to the ready queue.
    TaskYielded { task_id: TaskId },
    /// The task parked itself (moved to blocked queue).
    TaskParked {
        task_id: TaskId,
        reason: BlockReason,
    },
    /// A previously blocked task was unparked (moved to ready queue).
    TaskUnparked { task_id: TaskId },
    /// The task completed.
    TaskComplete { task_id: TaskId },
    /// The simulated clock was advanced.
    TimeAdvanced { to: Duration },
    /// An I/O operation was issued by a task.
    IoIssued { task_id: TaskId, op: IoOp },
    /// An I/O operation completed for a task.
    IoCompleted { task_id: TaskId, op: IoOp },
    /// A message was sent on a channel by a task.
    ChannelSend { task_id: TaskId },
    /// A message was received on a channel by a task.
    ChannelRecv { task_id: TaskId },
}

// ── SimulationTrace ───────────────────────────────────────────────────────

/// Append-only log of [`TraceEvent`]s produced during a simulation run.
///
/// Enabled by default.  Use [`SimulationTrace::disabled`] to suppress
/// recording (the API is still callable but events are silently dropped).
#[derive(Debug)]
pub struct SimulationTrace {
    events: Vec<TraceEvent>,
    enabled: bool,
}

impl SimulationTrace {
    /// Create a new, enabled trace.
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            enabled: true,
        }
    }

    /// Create a trace that silently discards all events.
    pub fn disabled() -> Self {
        Self {
            events: Vec::new(),
            enabled: false,
        }
    }

    /// Append an event to the trace (no-op if disabled).
    pub fn record(&mut self, event: TraceEvent) {
        if self.enabled {
            self.events.push(event);
        }
    }

    /// All recorded events in order.
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// Discard all recorded events.
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// `true` if no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for SimulationTrace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_trace_records() {
        let mut trace = SimulationTrace::new();
        assert!(trace.is_empty());

        trace.record(TraceEvent::TaskSpawned {
            task_id: TaskId(1),
            name: "t1".into(),
        });
        assert_eq!(trace.len(), 1);
        assert!(!trace.is_empty());
    }

    #[test]
    fn disabled_trace_drops() {
        let mut trace = SimulationTrace::disabled();
        trace.record(TraceEvent::TaskComplete { task_id: TaskId(0) });
        assert!(trace.is_empty());
    }

    #[test]
    fn clear_removes_events() {
        let mut trace = SimulationTrace::new();
        trace.record(TraceEvent::TaskComplete { task_id: TaskId(0) });
        trace.clear();
        assert!(trace.is_empty());
    }
}
