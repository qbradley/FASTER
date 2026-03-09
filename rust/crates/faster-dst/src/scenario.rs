//! Scenario templates for campaign-based seed exploration.
//!
//! A [`ScenarioTemplate`] is a declarative specification combining a workload
//! factory, fault profile, crash schedule, and invariant factories. The
//! [`SeedCampaign`](crate::campaign::SeedCampaign) engine instantiates one
//! concrete scenario per (template, seed) pair.

use std::sync::Arc;

use crate::crash::CrashSchedule;
use crate::fault::FaultConfig;
use crate::invariant::{AllCommittedRecoverable, Invariant};
use crate::workload::{CrudWorkload, Workload};

/// Factory that creates an [`Invariant`] from a workload reference.
///
/// The workload is passed so that invariants can inspect the expected state
/// (e.g. `AllCommittedRecoverable` needs the committed key→value map).
pub type InvariantFactory = Arc<dyn Fn(&CrudWorkload) -> Box<dyn Invariant> + Send + Sync>;

/// A declarative specification combining workload, fault profile, crash
/// schedule, and invariants for campaign-based seed exploration.
///
/// Templates are designed to be shared across threads via [`Arc`]. All
/// factories are `Fn + Send + Sync` so the campaign engine can call them
/// from any worker thread.
///
/// # Example
///
/// ```rust
/// use faster_dst::scenario::ScenarioTemplate;
/// use faster_dst::workload::CrudWorkload;
/// use faster_dst::invariant::AllCommittedRecoverable;
///
/// let template = ScenarioTemplate::builder("basic_recovery")
///     .workload(|seed| CrudWorkload::new(seed, 50))
///     .check_committed_recoverable()
///     .build();
///
/// assert_eq!(template.name, "basic_recovery");
/// ```
pub struct ScenarioTemplate {
    /// Human-readable name for display and reproduction commands.
    pub name: String,
    /// Factory that creates a deterministic workload from a seed.
    workload_factory: Arc<dyn Fn(u64) -> CrudWorkload + Send + Sync>,
    /// Fault injection configuration for the scenario.
    pub fault_config: FaultConfig,
    /// Optional factory that creates a crash schedule from a seed.
    crash_schedule_factory: Option<Arc<dyn Fn(u64) -> CrashSchedule + Send + Sync>>,
    /// Factories that create invariants from the workload.
    invariant_factories: Vec<InvariantFactory>,
}

impl ScenarioTemplate {
    /// Start building a new scenario template.
    pub fn builder(name: impl Into<String>) -> ScenarioTemplateBuilder {
        ScenarioTemplateBuilder {
            name: name.into(),
            workload_factory: None,
            fault_config: FaultConfig::default(),
            crash_schedule_factory: None,
            invariant_factories: Vec::new(),
        }
    }

    /// Create a workload for the given seed.
    pub fn create_workload(&self, seed: u64) -> CrudWorkload {
        (self.workload_factory)(seed)
    }

    /// Create a crash schedule for the given seed, if configured.
    pub fn create_crash_schedule(&self, seed: u64) -> Option<CrashSchedule> {
        self.crash_schedule_factory.as_ref().map(|f| f(seed))
    }

    /// Create invariants by invoking each factory with the workload.
    pub fn create_invariants(&self, workload: &CrudWorkload) -> Vec<Box<dyn Invariant>> {
        self.invariant_factories
            .iter()
            .map(|f| f(workload))
            .collect()
    }
}

impl std::fmt::Debug for ScenarioTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScenarioTemplate")
            .field("name", &self.name)
            .field("fault_config", &self.fault_config)
            .field("has_crash_schedule", &self.crash_schedule_factory.is_some())
            .field("invariant_count", &self.invariant_factories.len())
            .finish()
    }
}

/// Builder for [`ScenarioTemplate`].
pub struct ScenarioTemplateBuilder {
    name: String,
    workload_factory: Option<Arc<dyn Fn(u64) -> CrudWorkload + Send + Sync>>,
    fault_config: FaultConfig,
    crash_schedule_factory: Option<Arc<dyn Fn(u64) -> CrashSchedule + Send + Sync>>,
    invariant_factories: Vec<InvariantFactory>,
}

impl ScenarioTemplateBuilder {
    /// Set the workload factory. Required.
    pub fn workload(mut self, f: impl Fn(u64) -> CrudWorkload + Send + Sync + 'static) -> Self {
        self.workload_factory = Some(Arc::new(f));
        self
    }

    /// Set the fault injection configuration.
    pub fn fault_config(mut self, config: FaultConfig) -> Self {
        self.fault_config = config;
        self
    }

    /// Set the crash schedule factory.
    pub fn crash_schedule(
        mut self,
        f: impl Fn(u64) -> CrashSchedule + Send + Sync + 'static,
    ) -> Self {
        self.crash_schedule_factory = Some(Arc::new(f));
        self
    }

    /// Add an invariant factory.
    ///
    /// The factory receives a reference to the workload so it can build
    /// invariants that depend on the expected state.
    pub fn invariant(
        mut self,
        f: impl Fn(&CrudWorkload) -> Box<dyn Invariant> + Send + Sync + 'static,
    ) -> Self {
        self.invariant_factories.push(Arc::new(f));
        self
    }

    /// Convenience: add the standard [`AllCommittedRecoverable`] invariant.
    ///
    /// This checks that every key written by the workload is present with
    /// the correct value after recovery.
    pub fn check_committed_recoverable(self) -> Self {
        self.invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
    }

    /// Convenience: add a [`NoPhantomReads`] invariant for keys in
    /// `[phantom_start, phantom_start + count)`.
    ///
    /// These keys should NOT appear after recovery — their presence would
    /// indicate uncommitted data leaked across a crash boundary.
    pub fn check_no_phantom_reads(self, phantom_start: u64, count: u64) -> Self {
        self.invariant(move |_wl| {
            let keys: Vec<u64> = (phantom_start..phantom_start + count).collect();
            Box::new(crate::invariant::NoPhantomReads::new(keys))
        })
    }

    /// Build the scenario template.
    ///
    /// # Panics
    ///
    /// Panics if no workload factory was set.
    pub fn build(self) -> ScenarioTemplate {
        ScenarioTemplate {
            name: self.name,
            workload_factory: self.workload_factory.expect("workload factory is required"),
            fault_config: self.fault_config,
            crash_schedule_factory: self.crash_schedule_factory,
            invariant_factories: self.invariant_factories,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_produces_template() {
        let t = ScenarioTemplate::builder("test")
            .workload(|seed| CrudWorkload::new(seed, 10))
            .check_committed_recoverable()
            .build();

        assert_eq!(t.name, "test");
        let wl = t.create_workload(42);
        assert_eq!(wl.records().len(), 10);
        assert!(t.create_crash_schedule(42).is_none());
        assert_eq!(t.create_invariants(&wl).len(), 1);
    }

    #[test]
    fn builder_with_crash_schedule() {
        let t = ScenarioTemplate::builder("crash")
            .workload(|seed| CrudWorkload::new(seed, 5))
            .crash_schedule(|_seed| CrashSchedule::new())
            .build();

        assert!(t.create_crash_schedule(0).is_some());
    }

    #[test]
    fn template_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ScenarioTemplate>();
    }

    #[test]
    fn workload_deterministic_across_calls() {
        let t = ScenarioTemplate::builder("det")
            .workload(|seed| CrudWorkload::new(seed, 20))
            .build();

        let w1 = t.create_workload(99);
        let w2 = t.create_workload(99);
        assert_eq!(w1.records(), w2.records());
    }
}
