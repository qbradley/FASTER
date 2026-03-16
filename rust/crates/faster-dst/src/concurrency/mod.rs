//! Concurrency testing infrastructure: scenario configuration, assertion
//! checking, statistics collection, and seed management.

pub mod assertions;
pub mod config;
pub mod seeds;
pub mod stats;

pub use assertions::{AssertionFailure, ScenarioAssertions};
pub use config::{KeyDist, ScenarioConfig};
pub use seeds::exploration_seeds;
pub use stats::{ExecutionStats, StatsCollector};
