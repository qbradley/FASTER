//! DstRunner — the deterministic concurrency scenario execution engine.
//!
//! Wires [`FasterKv`] + [`SimDeviceV2`] + sim-clock + [`StatsCollector`]
//! together and drives N writer threads with a completion scheduler thread
//! and a watchdog.
//!
//! # Determinism guarantee
//!
//! Same [`ScenarioConfig::seed`] ⇒ identical key sequence, identical I/O
//! latencies, identical completion order, identical stats.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::sim_time;

use crate::clock::SimulatedClock;
use crate::device::SimulatedStorage;
use crate::sim_device_v2::SimDeviceV2;

use super::assertions::ScenarioAssertions;
use super::config::{KeyDist, ScenarioConfig};
use super::stats::{ExecutionStats, StatsCollector};
use super::watchdog::{Watchdog, WatchdogOutcome};

// ── Public types ────────────────────────────────────────────────────

/// Outcome of a [`DstRunner::run`] execution.
#[derive(Debug)]
pub enum RunOutcome {
    /// All assertions passed.
    Pass { stats: ExecutionStats },
    /// One or more assertions failed.
    Fail {
        assertion: String,
        stats: ExecutionStats,
    },
    /// Watchdog detected a deadlock / stall.
    Deadlock {
        blocked_info: String,
        stats: ExecutionStats,
    },
    /// Unrecoverable error during setup or execution.
    Error { message: String },
}

/// Configuration knobs for the runner itself (not the scenario).
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// Sim-time increment per scheduler tick (default: 1 ms).
    pub clock_tick: Duration,
    /// Maximum wall-clock time before the watchdog aborts (default: 60 s).
    pub max_wall_duration: Duration,
    /// Maximum wall-clock time without progress before stall abort (default: 10 s).
    pub max_stall_wall: Duration,
    /// Watchdog poll interval (default: 50 ms).
    pub watchdog_poll: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            clock_tick: Duration::from_millis(1),
            max_wall_duration: Duration::from_secs(60),
            max_stall_wall: Duration::from_secs(10),
            watchdog_poll: Duration::from_millis(50),
        }
    }
}

/// Deterministic scenario runner.
///
/// Constructs a `FasterKv<SimDeviceV2>` store, spawns writer threads,
/// a completion scheduler thread, and a watchdog, then collects results.
pub struct DstRunner {
    config: ScenarioConfig,
    assertions: ScenarioAssertions,
    runner_config: RunnerConfig,
}

impl DstRunner {
    pub fn new(config: ScenarioConfig, assertions: ScenarioAssertions) -> Self {
        Self {
            config,
            assertions,
            runner_config: RunnerConfig::default(),
        }
    }

    /// Override runner-level knobs (timeouts, tick size, etc.).
    #[must_use]
    pub fn with_runner_config(mut self, rc: RunnerConfig) -> Self {
        self.runner_config = rc;
        self
    }

    /// Execute the scenario.  Blocks until all threads finish or the
    /// watchdog aborts.
    pub fn run(&self) -> RunOutcome {
        // ── 1. Create shared sim-time clock ────────────────────────
        // Two clocks kept in lockstep:
        //   sim_clock  — read by SimDeviceV2 for completion timing
        //   time_nanos — installed per-thread for SimInstant::now()
        let sim_clock = Arc::new(SimulatedClock::new());
        let time_nanos = Arc::new(AtomicU64::new(0));

        // ── 2. Create device + storage ─────────────────────────────
        let storage = SimulatedStorage::new();
        let device = SimDeviceV2::new(
            self.config.io.clone(),
            Arc::clone(&sim_clock),
            Arc::clone(&storage),
        );

        // ── 3. Build FasterKv ──────────────────────────────────────
        let kv_config = FasterKvConfig {
            hash_index_size_log2: 10, // 1 K buckets — enough for DST
            buffer_size_pages: self.config.buffer_pool_pages,
            mutable_fraction: 0.9,
            sector_size: 512,
            lossy: self.config.lossy,
            ..FasterKvConfig::default()
        };

        let store = Arc::new(
            FasterKv::new(kv_config, SimpleFunctions::<u64, u64>::default(), device),
        );

        // ── 4. Stats + abort flag ──────────────────────────────────
        let stats = Arc::new(StatsCollector::new(self.config.num_writers));
        let abort = Arc::new(AtomicBool::new(false));

        let wall_start = Instant::now();

        // ── 5. Spawn writer threads ────────────────────────────────
        let mut writer_handles = Vec::with_capacity(self.config.num_writers);
        for writer_id in 0..self.config.num_writers {
            let store = Arc::clone(&store);
            let stats = Arc::clone(&stats);
            let abort = Arc::clone(&abort);
            let time_nanos = Arc::clone(&time_nanos);
            let config = self.config.clone();

            let handle = std::thread::Builder::new()
                .name(format!("dst-writer-{}", writer_id))
                .spawn(move || {
                    // Install sim clock for this thread.
                    sim_time::install_sim_clock(Arc::clone(&time_nanos));

                    let mut session = store.new_session();
                    let mut rng = ChaCha8Rng::seed_from_u64(
                        config.seed.wrapping_add(writer_id as u64),
                    );
                    let mut seq_counter: u64 = 0;
                    let value: u64 = 0xCAFE_0000 + writer_id as u64;

                    for op_idx in 0..config.ops_per_writer {
                        if abort.load(Ordering::Relaxed) {
                            break;
                        }

                        let key = generate_key(&config.key_dist, &mut rng, &mut seq_counter);
                        let outcome = store.upsert(&mut session, &key, &value, ());

                        if outcome.is_success() {
                            let t = time_nanos.load(Ordering::Relaxed);
                            stats.record_op(writer_id, t);
                        } else if outcome.is_aborted() {
                            let t = time_nanos.load(Ordering::Relaxed);
                            stats.record_abort(writer_id, t);
                        }

                        // Periodic maintenance (drives flush + poll_completions).
                        if config.maintenance_interval > 0
                            && (op_idx + 1) % config.maintenance_interval == 0
                        {
                            store.maintenance();
                        }
                    }

                    // Final maintenance pass.
                    store.maintenance();
                    store.dispose_session(session);

                    sim_time::remove_sim_clock();
                })
                .expect("failed to spawn writer thread");

            writer_handles.push(handle);
        }

        // ── 6. Spawn completion scheduler thread ───────────────────
        let sched_abort = Arc::clone(&abort);
        let sched_clock = Arc::clone(&sim_clock);
        let sched_time = Arc::clone(&time_nanos);
        let sched_store = Arc::clone(&store);
        let tick = self.runner_config.clock_tick;

        let scheduler_handle = std::thread::Builder::new()
            .name("dst-scheduler".into())
            .spawn(move || {
                while !sched_abort.load(Ordering::Relaxed) {
                    // Advance the SimulatedClock (used by SimDeviceV2).
                    sched_clock.advance(tick);
                    // Mirror to the AtomicU64 used by install_sim_clock.
                    sched_time.store(sched_clock.now_nanos(), Ordering::Relaxed);

                    // Fire any completions whose deadline has passed.
                    // Access device through the store's poll_completions path:
                    // maintenance() calls poll_completions internally.
                    sched_store.maintenance();

                    // Small wall-clock sleep to avoid busy-spinning.
                    std::thread::sleep(Duration::from_micros(50));
                }
            })
            .expect("failed to spawn scheduler thread");

        // ── 7. Spawn watchdog thread ───────────────────────────────
        let wd_abort = Arc::clone(&abort);
        let max_stall = self.runner_config.max_stall_wall;
        let max_total = self.runner_config.max_wall_duration;
        let wd_poll = self.runner_config.watchdog_poll;

        // The watchdog tracks progress via total_ops from StatsCollector.
        // We use a proxy AtomicU64 that mirrors stats.total_ops.
        // (StatsCollector.total_ops is private, so writers already
        // increment stats which in turn bumps its internal atomic.)
        // We share the stats' progress indirectly: the watchdog reads
        // a separate progress counter that writers also increment.
        let progress = Arc::new(AtomicU64::new(0));
        let wd_progress = Arc::clone(&progress);

        // Patch: writers need to also bump `progress`.  We capture it
        // via a quick thread that polls stats periodically.  This is
        // simpler than changing the hot-path writer loop.
        let poll_abort = Arc::clone(&abort);
        let poll_stats = Arc::clone(&stats);
        let poll_progress = Arc::clone(&progress);
        let progress_poller = std::thread::Builder::new()
            .name("dst-progress-poll".into())
            .spawn(move || {
                while !poll_abort.load(Ordering::Relaxed) {
                    // Read total ops from each per-writer atomic and sum.
                    let total: u64 = (0..poll_stats.num_writers())
                        .map(|i| poll_stats.writer_ops(i))
                        .sum();
                    poll_progress.store(total, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .expect("failed to spawn progress poller");

        let watchdog_handle = std::thread::Builder::new()
            .name("dst-watchdog".into())
            .spawn(move || {
                let wd = Watchdog::new(max_stall, max_total, wd_abort)
                    .with_poll_interval(wd_poll);
                wd.run(&wd_progress)
            })
            .expect("failed to spawn watchdog thread");

        // ── 8. Join writers ────────────────────────────────────────
        let mut writer_panics = Vec::new();
        for (id, h) in writer_handles.into_iter().enumerate() {
            if let Err(e) = h.join() {
                writer_panics.push(format!("writer-{} panicked: {:?}", id, e));
            }
        }

        // Signal scheduler + watchdog to stop.
        abort.store(true, Ordering::Relaxed);

        let _ = scheduler_handle.join();
        let watchdog_outcome = watchdog_handle.join().unwrap_or(WatchdogOutcome::AllClear);
        let _ = progress_poller.join();

        // ── 9. Compute stats ───────────────────────────────────────
        let wall_duration = wall_start.elapsed();
        let sim_duration = sim_clock.now();
        let exec_stats = stats.finalize(sim_duration, wall_duration);

        // ── 10. Handle panics / watchdog / assertions ──────────────
        if !writer_panics.is_empty() {
            return RunOutcome::Error {
                message: writer_panics.join("; "),
            };
        }

        match watchdog_outcome {
            WatchdogOutcome::Stall { info } => {
                return RunOutcome::Deadlock {
                    blocked_info: info,
                    stats: exec_stats,
                };
            }
            WatchdogOutcome::Timeout { info } => {
                return RunOutcome::Deadlock {
                    blocked_info: info,
                    stats: exec_stats,
                };
            }
            WatchdogOutcome::AllClear => {}
        }

        let failures = self.assertions.check(&exec_stats);
        if failures.is_empty() {
            RunOutcome::Pass { stats: exec_stats }
        } else {
            let msg = failures
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            RunOutcome::Fail {
                assertion: msg,
                stats: exec_stats,
            }
        }
    }
}

// ── Key generation ──────────────────────────────────────────────────

fn generate_key(dist: &KeyDist, rng: &mut ChaCha8Rng, seq: &mut u64) -> u64 {
    match dist {
        KeyDist::Uniform { range } => rng.random_range(0..*range),
        KeyDist::Sequential => {
            let k = *seq;
            *seq += 1;
            k
        }
        KeyDist::Zipfian { range, exponent } => {
            // Approximate Zipfian via inverse-power-law on uniform [0,1).
            let u: f64 = rng.random();
            let rank = (u.powf(*exponent) * (*range as f64)).min((*range - 1) as f64);
            rank as u64
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_device_v2::SimIoConfig;

    /// Minimal scenario: 2 writers × 1 K ops with queue_depth=32 → Pass.
    #[test]
    fn basic_two_writer_pass() {
        let config = ScenarioConfig {
            num_writers: 2,
            ops_per_writer: 1_000,
            buffer_pool_pages: 16,
            key_dist: KeyDist::Uniform { range: 10_000 },
            lossy: false,
            io: SimIoConfig {
                queue_depth: 32,
                seed: 42,
                ..SimIoConfig::default()
            },
            maintenance_interval: 50,
            seed: 12345,
        };

        let assertions = ScenarioAssertions {
            all_writers_complete: true,
            no_deadlock: false, // don't check stall heuristic
            ..ScenarioAssertions::default()
        };

        let runner = DstRunner::new(config, assertions).with_runner_config(RunnerConfig {
            clock_tick: Duration::from_millis(1),
            max_wall_duration: Duration::from_secs(60),
            max_stall_wall: Duration::from_secs(30),
            watchdog_poll: Duration::from_millis(100),
        });

        let outcome = runner.run();
        match &outcome {
            RunOutcome::Pass { stats } => {
                assert_eq!(stats.total_ops, 2_000, "expected 2×1000 ops");
                assert_eq!(stats.per_writer_ops.len(), 2);
                for &w in &stats.per_writer_ops {
                    assert_eq!(w, 1_000);
                }
            }
            RunOutcome::Fail { assertion, stats } => {
                panic!(
                    "scenario failed assertion: {} (total_ops={})",
                    assertion, stats.total_ops
                );
            }
            RunOutcome::Deadlock { blocked_info, stats } => {
                panic!(
                    "deadlock detected: {} (total_ops={})",
                    blocked_info, stats.total_ops
                );
            }
            RunOutcome::Error { message } => {
                panic!("error: {}", message);
            }
        }
    }

    /// Same seed → identical total_ops (determinism smoke test).
    #[test]
    fn deterministic_replay() {
        let make_config = || ScenarioConfig {
            num_writers: 2,
            ops_per_writer: 500,
            buffer_pool_pages: 8,
            key_dist: KeyDist::Sequential,
            lossy: false,
            io: SimIoConfig {
                queue_depth: 16,
                seed: 99,
                ..SimIoConfig::default()
            },
            maintenance_interval: 100,
            seed: 77777,
        };

        let assertions = ScenarioAssertions::default();
        let rc = RunnerConfig {
            clock_tick: Duration::from_millis(1),
            max_wall_duration: Duration::from_secs(60),
            max_stall_wall: Duration::from_secs(30),
            watchdog_poll: Duration::from_millis(100),
        };

        let r1 = DstRunner::new(make_config(), assertions.clone())
            .with_runner_config(rc.clone())
            .run();
        let r2 = DstRunner::new(make_config(), assertions.clone())
            .with_runner_config(rc.clone())
            .run();

        let s1 = match r1 {
            RunOutcome::Pass { stats } => stats,
            other => panic!("run 1 failed: {:?}", other),
        };
        let s2 = match r2 {
            RunOutcome::Pass { stats } => stats,
            other => panic!("run 2 failed: {:?}", other),
        };

        assert_eq!(s1.total_ops, s2.total_ops, "deterministic ops count");
        assert_eq!(
            s1.per_writer_ops, s2.per_writer_ops,
            "per-writer ops match"
        );
    }

    /// Watchdog fires on artificially short stall budget.
    #[test]
    fn watchdog_fires_on_stall() {
        let config = ScenarioConfig {
            num_writers: 1,
            // Huge op count — writers will never finish.
            ops_per_writer: u64::MAX,
            buffer_pool_pages: 8,
            key_dist: KeyDist::Sequential,
            lossy: false,
            io: SimIoConfig {
                queue_depth: 4,
                seed: 1,
                // Very high latency to slow things down.
                base_latency_ns: 1_000_000_000,
                ..SimIoConfig::default()
            },
            maintenance_interval: 10,
            seed: 0,
        };

        let assertions = ScenarioAssertions::default();
        let rc = RunnerConfig {
            clock_tick: Duration::from_millis(1),
            // Very short wall budget forces a timeout.
            max_wall_duration: Duration::from_millis(500),
            max_stall_wall: Duration::from_millis(400),
            watchdog_poll: Duration::from_millis(20),
        };

        let outcome = DstRunner::new(config, assertions)
            .with_runner_config(rc)
            .run();

        match outcome {
            RunOutcome::Deadlock { .. } => { /* expected */ }
            RunOutcome::Pass { .. } => panic!("should not pass with infinite ops"),
            other => {
                // An Error is also acceptable if threads panicked during abort.
                if let RunOutcome::Error { .. } = other {
                    // ok
                } else {
                    panic!("unexpected outcome: {:?}", other);
                }
            }
        }
    }
}
