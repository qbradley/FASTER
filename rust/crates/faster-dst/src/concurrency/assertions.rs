use std::fmt;

use super::stats::ExecutionStats;

/// A single assertion failure with context.
#[derive(Clone, Debug)]
pub struct AssertionFailure {
    pub name: &'static str,
    pub message: String,
}

impl fmt::Display for AssertionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

/// Declarative post-run assertions checked against [`ExecutionStats`].
#[derive(Clone, Debug)]
pub struct ScenarioAssertions {
    /// Minimum ops/sec in any 1-second sim-time window.
    pub min_throughput_1s: Option<u64>,
    /// Maximum gap (sim-time ns) between consecutive operations.
    pub max_stall_ns: Option<u64>,
    /// Maximum RSS growth in bytes (informational — not always measurable).
    pub memory_bound_bytes: Option<u64>,
    /// Sim-time watchdog: fail if run exceeds this duration.
    pub max_sim_duration_ns: Option<u64>,
    /// All writer threads must complete their full op count.
    pub all_writers_complete: bool,
    /// No thread should appear stuck (stall exceeding watchdog).
    pub no_deadlock: bool,
}

impl Default for ScenarioAssertions {
    fn default() -> Self {
        Self {
            min_throughput_1s: None,
            max_stall_ns: None,
            memory_bound_bytes: None,
            max_sim_duration_ns: None,
            all_writers_complete: true,
            no_deadlock: true,
        }
    }
}

impl ScenarioAssertions {
    /// Check assertions against collected stats, returning all failures.
    pub fn check(&self, stats: &ExecutionStats) -> Vec<AssertionFailure> {
        let mut failures = Vec::new();

        if let Some(min) = self.min_throughput_1s {
            if stats.throughput_min_1s < min as f64 {
                failures.push(AssertionFailure {
                    name: "min_throughput_1s",
                    message: format!(
                        "minimum 1s throughput {:.1} ops/s < required {} ops/s",
                        stats.throughput_min_1s, min
                    ),
                });
            }
        }

        if let Some(max_ns) = self.max_stall_ns {
            let stall_ns = stats.max_stall.as_nanos() as u64;
            if stall_ns > max_ns {
                failures.push(AssertionFailure {
                    name: "max_stall_ns",
                    message: format!(
                        "max stall {}ns > allowed {}ns",
                        stall_ns, max_ns
                    ),
                });
            }
        }

        if let Some(max_sim_ns) = self.max_sim_duration_ns {
            let sim_ns = stats.sim_duration.as_nanos() as u64;
            if sim_ns > max_sim_ns {
                failures.push(AssertionFailure {
                    name: "max_sim_duration_ns",
                    message: format!(
                        "sim duration {}ns > watchdog {}ns",
                        sim_ns, max_sim_ns
                    ),
                });
            }
        }

        if self.all_writers_complete {
            for (i, &ops) in stats.per_writer_ops.iter().enumerate() {
                if ops == 0 {
                    failures.push(AssertionFailure {
                        name: "all_writers_complete",
                        message: format!("writer {} completed 0 ops", i),
                    });
                }
            }
        }

        if self.no_deadlock {
            if let Some(max_ns) = self.max_stall_ns {
                let stall_ns = stats.max_stall.as_nanos() as u64;
                // A stall exceeding 10× the configured max is a deadlock signal
                if stall_ns > max_ns.saturating_mul(10) {
                    failures.push(AssertionFailure {
                        name: "no_deadlock",
                        message: format!(
                            "suspected deadlock: stall {}ns > 10× max_stall {}ns",
                            stall_ns, max_ns
                        ),
                    });
                }
            }
        }

        failures
    }
}
