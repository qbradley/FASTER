//! Concurrency testing infrastructure: scenario configuration, assertion
//! checking, statistics collection, seed management, scenario library,
//! campaign runner, and the DstRunner execution engine.

pub mod assertions;
pub mod campaign;
pub mod config;
pub mod runner;
pub mod scenarios;
pub mod seeds;
pub mod stats;
pub mod watchdog;

pub use assertions::{AssertionFailure, ScenarioAssertions};
pub use campaign::{CampaignFailure, ConcurrencyCampaignReport, run_concurrency_campaign};
pub use config::{KeyDist, ScenarioConfig};
pub use runner::{DstRunner, RunOutcome, RunnerConfig};
pub use scenarios::{ALL_SCENARIOS, ScenarioFactory};
pub use seeds::exploration_seeds;
pub use stats::{ExecutionStats, StatsCollector};
pub use watchdog::{Watchdog, WatchdogOutcome};
