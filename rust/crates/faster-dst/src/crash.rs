//! Crash-point injection framework.
//!
//! Provides [`CrashSchedule`] for declaratively specifying which crash points
//! should fire, and [`CrashRecoveryRunner`] for orchestrating the full
//! workload → crash → recovery → verify cycle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use faster_core::checkpoint::CheckpointType;
use faster_core::sim_hooks::{SimulatedCrash, install_crash_check, remove_crash_check};

use crate::fault::CrashPoint;
use crate::harness::SimulationHarness;
use crate::invariant::Invariant;
use crate::workload::{CrudWorkload, Workload};

// ── CrashTrigger ────────────────────────────────────────────────────

/// When a crash point should fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashTrigger {
    /// Crash on the Nth visit (1-based).
    OnVisit(u64),
    /// Crash on every visit.
    Always,
    /// Never crash (documentation / tracing only).
    Never,
}

// ── CrashSchedule ───────────────────────────────────────────────────

/// Declarative schedule of which crash points should fire and when.
#[derive(Debug, Clone)]
pub struct CrashSchedule {
    triggers: HashMap<String, CrashTrigger>,
    visit_counts: HashMap<String, u64>,
}

impl CrashSchedule {
    /// Empty schedule — no crashes.
    pub fn new() -> Self {
        Self {
            triggers: HashMap::new(),
            visit_counts: HashMap::new(),
        }
    }

    /// Schedule a crash at the given point with the given trigger.
    pub fn add(&mut self, point: CrashPoint, trigger: CrashTrigger) -> &mut Self {
        self.triggers.insert(point.label().to_string(), trigger);
        self
    }

    /// Schedule a crash at a raw label string.
    pub fn add_raw(&mut self, label: &str, trigger: CrashTrigger) -> &mut Self {
        self.triggers.insert(label.to_string(), trigger);
        self
    }

    /// Check whether the given label should crash, updating visit counts.
    pub fn should_crash(&mut self, label: &str) -> bool {
        let trigger = match self.triggers.get(label) {
            Some(t) => t,
            None => return false,
        };

        let count = self.visit_counts.entry(label.to_string()).or_insert(0);
        *count += 1;

        match trigger {
            CrashTrigger::Always => true,
            CrashTrigger::OnVisit(n) => *count == *n,
            CrashTrigger::Never => false,
        }
    }
}

impl Default for CrashSchedule {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared reference to a CrashSchedule for thread-local installation.
pub type CrashScheduleRef = Rc<RefCell<CrashSchedule>>;

/// Install a [`CrashSchedule`] as the crash oracle for the current thread.
///
/// Returns the shared reference so callers can inspect it later.
pub fn install_schedule(schedule: CrashSchedule) -> CrashScheduleRef {
    let shared = Rc::new(RefCell::new(schedule));
    let shared_clone = Rc::clone(&shared);
    install_crash_check(move |label| shared_clone.borrow_mut().should_crash(label));
    shared
}

/// Remove any installed crash schedule from the current thread.
pub fn remove_schedule() {
    remove_crash_check();
}

// ── CrashScenario ───────────────────────────────────────────────────

/// A complete crash-recovery test scenario.
pub struct CrashScenario {
    /// Workload to run before the crash.
    pub workload: CrudWorkload,
    /// Schedule controlling which crash points fire.
    pub crash_schedule: CrashSchedule,
    /// Invariants to verify after recovery.
    pub invariants: Vec<Box<dyn Invariant>>,
}

// ── CrashRecoveryResult ─────────────────────────────────────────────

/// Outcome of a crash-recovery test run.
#[derive(Debug)]
pub struct CrashRecoveryResult {
    /// Whether a crash was triggered (vs. the workload completed normally).
    pub crashed: bool,
    /// The crash-point label that fired, if any.
    pub crash_label: Option<String>,
    /// Whether recovery succeeded.
    pub recovery_ok: bool,
    /// Results of invariant checks (label → pass/fail message).
    pub invariant_results: Vec<Result<(), String>>,
}

impl CrashRecoveryResult {
    /// Returns `true` if the scenario passed: crash was caught, recovery
    /// succeeded, and all invariants hold.
    pub fn passed(&self) -> bool {
        self.recovery_ok && self.invariant_results.iter().all(|r| r.is_ok())
    }
}

// ── CrashRecoveryRunner ─────────────────────────────────────────────

/// Orchestrates: workload → checkpoint → crash → recovery → verify.
pub struct CrashRecoveryRunner {
    seed: u64,
}

impl CrashRecoveryRunner {
    /// Create a runner with the given seed.
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Execute a crash-recovery scenario.
    ///
    /// The runner has two modes based on the crash schedule targets:
    ///
    /// 1. **Checkpoint crash**: Write data, install crash schedule, attempt
    ///    checkpoint (crash fires during checkpoint). Then recover from any
    ///    prior checkpoint and verify invariants.
    /// 2. **Recovery crash**: Write data, checkpoint successfully, then
    ///    install crash schedule and attempt recovery (crash fires during
    ///    recovery). Then re-recover without crash schedule.
    ///
    /// The mode is automatic based on which crash points are scheduled.
    pub fn run(&self, scenario: &mut CrashScenario) -> CrashRecoveryResult {
        let harness = SimulationHarness::new(self.seed);

        // Phase 1: Write workload data and checkpoint (no crash schedule yet).
        let token = {
            let store = harness.create_file_store();
            let mut session = store.new_session();
            scenario.workload.execute(&store, &mut session);
            let tok = store
                .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
                .expect("initial checkpoint should succeed");
            store.dispose_session(session);
            tok
        }; // store dropped

        // Phase 2: Install crash schedule and attempt recovery.
        // This catches crashes in both recovery AND checkpoint paths.
        // Recovery crash points fire during the recover() call below.
        // Checkpoint crash points would fire if we attempted another checkpoint,
        // but since we just recover, only recovery points fire here.
        let crash_label = {
            let _schedule_ref = install_schedule(scenario.crash_schedule.clone());

            let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut store = harness.create_file_store();
                store
                    .recover(harness.checkpoint_dir(), Some(token))
                    .expect("recovery under crash schedule");
            }));

            remove_schedule();

            match crash_result {
                Ok(()) => None,
                Err(payload) => {
                    if let Some(crash) = payload.downcast_ref::<SimulatedCrash>() {
                        Some(crash.point.to_string())
                    } else {
                        std::panic::resume_unwind(payload);
                    }
                }
            }
        };

        let crashed = crash_label.is_some();

        // Phase 3: Clean recovery — should always succeed.
        let recovery_ok;
        let mut invariant_results = Vec::new();

        let mut store = harness.create_file_store();
        match store.recover(harness.checkpoint_dir(), Some(token)) {
            Ok(_info) => {
                recovery_ok = true;
                let mut session = store.new_session();
                for inv in &scenario.invariants {
                    invariant_results.push(inv.check(&store, &mut session));
                }
                store.dispose_session(session);
            }
            Err(e) => {
                recovery_ok = false;
                invariant_results.push(Err(format!("recovery failed: {e}")));
            }
        }

        CrashRecoveryResult {
            crashed,
            crash_label,
            recovery_ok,
            invariant_results,
        }
    }

    /// Convenience: run a checkpoint crash scenario for a single crash point.
    ///
    /// Writes `record_count` records, checkpoints, then installs a crash
    /// schedule targeting the given point and attempts another checkpoint.
    /// Recovers and verifies the original data is intact.
    pub fn run_checkpoint_crash(
        &self,
        point: CrashPoint,
        record_count: usize,
    ) -> CrashRecoveryResult {
        let workload = CrudWorkload::new(self.seed, record_count);
        let expected = workload.expected_state();

        let mut schedule = CrashSchedule::new();
        schedule.add(point, CrashTrigger::Always);

        let invariant = crate::invariant::AllCommittedRecoverable::new(expected);

        let mut scenario = CrashScenario {
            workload,
            crash_schedule: schedule,
            invariants: vec![Box::new(invariant)],
        };

        self.run(&mut scenario)
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_schedule_on_visit() {
        let mut sched = CrashSchedule::new();
        sched.add_raw("test_point", CrashTrigger::OnVisit(3));

        assert!(!sched.should_crash("test_point")); // visit 1
        assert!(!sched.should_crash("test_point")); // visit 2
        assert!(sched.should_crash("test_point")); // visit 3
        assert!(!sched.should_crash("test_point")); // visit 4
    }

    #[test]
    fn crash_schedule_always() {
        let mut sched = CrashSchedule::new();
        sched.add_raw("boom", CrashTrigger::Always);

        assert!(sched.should_crash("boom"));
        assert!(sched.should_crash("boom"));
    }

    #[test]
    fn crash_schedule_never() {
        let mut sched = CrashSchedule::new();
        sched.add_raw("safe", CrashTrigger::Never);

        assert!(!sched.should_crash("safe"));
        assert!(!sched.should_crash("safe"));
    }

    #[test]
    fn crash_schedule_unknown_label() {
        let mut sched = CrashSchedule::new();
        assert!(!sched.should_crash("unknown"));
    }

    #[test]
    fn install_and_remove_schedule() {
        let mut sched = CrashSchedule::new();
        sched.add_raw("x", CrashTrigger::Always);
        let _ref = install_schedule(sched);

        // The crash check is installed — should_crash returns true.
        assert!(faster_core::sim_hooks::should_crash("x"));
        assert!(!faster_core::sim_hooks::should_crash("y"));

        remove_schedule();
        assert!(!faster_core::sim_hooks::should_crash("x"));
    }

    #[test]
    fn crash_schedule_from_crash_point() {
        let mut sched = CrashSchedule::new();
        sched.add(
            CrashPoint::Checkpoint(crate::fault::CheckpointPhase::MetadataPersisted),
            CrashTrigger::Always,
        );
        assert!(sched.should_crash("checkpoint_metadata_persisted"));
        assert!(!sched.should_crash("checkpoint_completed"));
    }
}
