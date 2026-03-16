//! Concurrency testing infrastructure: scenario configuration, assertion
//! checking, statistics collection, seed management, and the DstRunner
//! execution engine.

pub mod assertions;
pub mod config;
pub mod runner;
pub mod seeds;
pub mod stats;
pub mod watchdog;

pub use assertions::{AssertionFailure, ScenarioAssertions};
pub use config::{KeyDist, ScenarioConfig};
pub use runner::{DstRunner, RunOutcome, RunnerConfig};
pub use seeds::exploration_seeds;
pub use stats::{ExecutionStats, StatsCollector};
pub use watchdog::{Watchdog, WatchdogOutcome};
