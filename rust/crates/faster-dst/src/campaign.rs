//! Seed exploration campaign engine.
//!
//! [`SeedCampaign`] sweeps thousands of seed/fault combinations across
//! scenario templates, executing in parallel and collecting failures with
//! reproduction commands.

use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use faster_core::checkpoint::CheckpointType;
use faster_core::sim_hooks::SimulatedCrash;

use crate::crash::{install_schedule, remove_schedule};
use crate::harness::SimulationHarness;
use crate::report::{CampaignReport, ScenarioFailure};
use crate::scenario::ScenarioTemplate;
use crate::workload::Workload;

/// Campaign engine that runs scenario templates across many seeds in parallel.
///
/// # Example
///
/// ```rust,no_run
/// use faster_dst::campaign::SeedCampaign;
/// use faster_dst::scenario::ScenarioTemplate;
/// use faster_dst::workload::CrudWorkload;
///
/// let template = ScenarioTemplate::builder("basic")
///     .workload(|seed| CrudWorkload::new(seed, 50))
///     .check_committed_recoverable()
///     .build();
///
/// let mut campaign = SeedCampaign::new();
/// campaign.add_scenario(template).seed_range(0..10);
/// let report = campaign.run();
/// assert!(report.all_passed());
/// ```
pub struct SeedCampaign {
    scenarios: Vec<Arc<ScenarioTemplate>>,
    seed_range: Range<u64>,
    thread_count: usize,
}

impl SeedCampaign {
    /// Create a campaign with default settings (seeds 0..100, auto thread count).
    pub fn new() -> Self {
        Self {
            scenarios: Vec::new(),
            seed_range: 0..100,
            thread_count: default_thread_count(),
        }
    }

    /// Add a scenario template to the campaign.
    pub fn add_scenario(&mut self, template: ScenarioTemplate) -> &mut Self {
        self.scenarios.push(Arc::new(template));
        self
    }

    /// Set the seed range to explore.
    pub fn seed_range(&mut self, range: Range<u64>) -> &mut Self {
        self.seed_range = range;
        self
    }

    /// Set the number of worker threads.
    pub fn thread_count(&mut self, count: usize) -> &mut Self {
        self.thread_count = count;
        self
    }

    /// Execute the campaign, returning a structured report.
    ///
    /// Each (scenario, seed) pair runs independently on a worker thread with
    /// its own temp directory. Failures are collected without aborting the
    /// campaign.
    pub fn run(&self) -> CampaignReport {
        let start = Instant::now();

        // Build the work queue: (scenario_index, seed).
        let work_items: Vec<(usize, u64)> = self
            .scenarios
            .iter()
            .enumerate()
            .flat_map(|(si, _)| self.seed_range.clone().map(move |seed| (si, seed)))
            .collect();

        let total = work_items.len();
        if total == 0 {
            return CampaignReport {
                total: 0,
                passed: 0,
                failed: 0,
                elapsed: start.elapsed(),
                failures: vec![],
            };
        }

        let next_idx = AtomicUsize::new(0);
        let results: Mutex<Vec<Result<(), ScenarioFailure>>> =
            Mutex::new(Vec::with_capacity(total));
        let thread_count = self.thread_count.min(total).max(1);

        std::thread::scope(|scope| {
            for _ in 0..thread_count {
                scope.spawn(|| {
                    loop {
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= work_items.len() {
                            break;
                        }
                        let (scenario_idx, seed) = work_items[idx];
                        let result = run_scenario(&self.scenarios[scenario_idx], seed);
                        results.lock().unwrap().push(result);
                    }
                });
            }
        });

        let all_results = results.into_inner().unwrap();
        let failures: Vec<ScenarioFailure> =
            all_results.into_iter().filter_map(|r| r.err()).collect();

        CampaignReport {
            total,
            passed: total - failures.len(),
            failed: failures.len(),
            elapsed: start.elapsed(),
            failures,
        }
    }
}

impl Default for SeedCampaign {
    fn default() -> Self {
        Self::new()
    }
}

/// Execute a single (scenario, seed) pair.
///
/// Phases:
/// 1. Write workload records and take a checkpoint.
/// 2. (If crash schedule) install schedule and attempt recovery (crash may fire).
/// 3. Clean recovery and invariant verification.
fn run_scenario(scenario: &ScenarioTemplate, seed: u64) -> Result<(), ScenarioFailure> {
    let harness = SimulationHarness::new(seed);
    let mut workload = scenario.create_workload(seed);
    let invariants = scenario.create_invariants(&workload);

    // Phase 1: Execute workload and checkpoint.
    let token = {
        let store = harness.create_file_store();
        let mut session = store.new_session();
        workload.execute(&store, &mut session);
        let tok = store
            .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
            .map_err(|e| ScenarioFailure {
                scenario_name: scenario.name.clone(),
                seed,
                invariant_violated: "checkpoint".to_string(),
                error_message: format!("checkpoint failed: {e:?}"),
            })?;
        store.dispose_session(session);
        tok
    }; // store dropped — simulates process crash boundary

    // Phase 2: Crash injection (if configured).
    if let Some(schedule) = scenario.create_crash_schedule(seed) {
        let _schedule_ref = install_schedule(schedule);
        let crash_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut store = harness.create_file_store();
            // Crash injection may interrupt recovery.
            let _result = store.recover(harness.checkpoint_dir(), Some(token));
        }));
        remove_schedule();
        // Distinguish SimulatedCrash (expected) from real panics (bugs).
        match crash_result {
            Ok(_) => {}
            Err(payload) => {
                if payload.downcast_ref::<SimulatedCrash>().is_none() {
                    std::panic::resume_unwind(payload);
                }
            }
        }
    }

    // Phase 3: Clean recovery and invariant verification.
    let mut store = harness.create_file_store();
    store
        .recover(harness.checkpoint_dir(), Some(token))
        .map_err(|e| ScenarioFailure {
            scenario_name: scenario.name.clone(),
            seed,
            invariant_violated: "recovery".to_string(),
            error_message: format!("clean recovery failed: {e:?}"),
        })?;

    let mut session = store.new_session();
    for inv in &invariants {
        inv.check(&store, &mut session)
            .map_err(|msg| ScenarioFailure {
                scenario_name: scenario.name.clone(),
                seed,
                invariant_violated: msg.clone(),
                error_message: msg,
            })?;
    }
    store.dispose_session(session);

    Ok(())
}

fn default_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::CrudWorkload;

    #[test]
    fn empty_campaign_produces_empty_report() {
        let campaign = SeedCampaign::new();
        let report = campaign.run();
        assert_eq!(report.total, 0);
        assert!(report.all_passed());
    }

    #[test]
    fn single_scenario_no_crash() {
        let t = ScenarioTemplate::builder("basic")
            .workload(|seed| CrudWorkload::new(seed, 10))
            .check_committed_recoverable()
            .build();

        let mut c = SeedCampaign::new();
        c.add_scenario(t).seed_range(0..5).thread_count(1);
        let r = c.run();
        assert_eq!(r.total, 5);
        assert!(r.all_passed());
    }
}
