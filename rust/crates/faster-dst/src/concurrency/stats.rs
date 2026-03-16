use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Aggregated statistics from a completed scenario execution.
#[derive(Clone, Debug)]
pub struct ExecutionStats {
    /// Total operations across all writers.
    pub total_ops: u64,
    /// Total aborted operations across all writers.
    pub total_aborts: u64,
    /// Simulated time elapsed.
    pub sim_duration: Duration,
    /// Wall-clock time elapsed.
    pub wall_duration: Duration,
    /// Average throughput (ops/sec) over the entire run.
    pub throughput_avg: f64,
    /// Minimum throughput in any 1-second sim-time bucket.
    pub throughput_min_1s: f64,
    /// Longest gap between consecutive operations (sim-time).
    pub max_stall: Duration,
    /// Operations completed by each writer thread.
    pub per_writer_ops: Vec<u64>,
}

/// Thread-safe statistics collector shared across writer threads.
///
/// Writers call [`record_op`] and [`record_abort`] concurrently.
/// After the run, the orchestrator calls [`finalize`] to produce
/// [`ExecutionStats`].
pub struct StatsCollector {
    total_ops: AtomicU64,
    total_aborts: AtomicU64,
    per_writer_ops: Vec<AtomicU64>,
    per_writer_aborts: Vec<AtomicU64>,
    // Timestamps stored per-writer to avoid lock contention on the hot path.
    // Each writer appends to its own Vec under a mutex (contention is only
    // between record_op and finalize on the *same* writer, which doesn't
    // happen concurrently by design).
    timestamps: Vec<Mutex<Vec<u64>>>,
}

// Safety: AtomicU64 and Mutex<Vec<u64>> are both Send + Sync.
// The struct is designed for concurrent access from multiple writer threads.
unsafe impl Send for StatsCollector {}
unsafe impl Sync for StatsCollector {}

impl std::fmt::Debug for StatsCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsCollector")
            .field("total_ops", &self.total_ops.load(Ordering::Relaxed))
            .field("total_aborts", &self.total_aborts.load(Ordering::Relaxed))
            .field("num_writers", &self.per_writer_ops.len())
            .finish()
    }
}

impl StatsCollector {
    /// Create a new collector for the given number of writers.
    pub fn new(num_writers: usize) -> Self {
        let per_writer_ops = (0..num_writers).map(|_| AtomicU64::new(0)).collect();
        let per_writer_aborts = (0..num_writers).map(|_| AtomicU64::new(0)).collect();
        let timestamps = (0..num_writers).map(|_| Mutex::new(Vec::new())).collect();
        Self {
            total_ops: AtomicU64::new(0),
            total_aborts: AtomicU64::new(0),
            per_writer_ops,
            per_writer_aborts,
            timestamps,
        }
    }

    /// Number of writer slots.
    pub fn num_writers(&self) -> usize {
        self.per_writer_ops.len()
    }

    /// Current ops count for a single writer (relaxed read).
    pub fn writer_ops(&self, writer_id: usize) -> u64 {
        self.per_writer_ops[writer_id].load(Ordering::Relaxed)
    }

    /// Record a successful operation for a writer at the given sim-time.
    pub fn record_op(&self, writer_id: usize, sim_time_ns: u64) {
        self.total_ops.fetch_add(1, Ordering::Relaxed);
        self.per_writer_ops[writer_id].fetch_add(1, Ordering::Relaxed);
        self.timestamps[writer_id]
            .lock()
            .expect("timestamp lock poisoned")
            .push(sim_time_ns);
    }

    /// Record an aborted operation for a writer at the given sim-time.
    pub fn record_abort(&self, writer_id: usize, sim_time_ns: u64) {
        self.total_aborts.fetch_add(1, Ordering::Relaxed);
        self.per_writer_aborts[writer_id].fetch_add(1, Ordering::Relaxed);
        self.timestamps[writer_id]
            .lock()
            .expect("timestamp lock poisoned")
            .push(sim_time_ns);
    }

    /// Consume collected data and produce final [`ExecutionStats`].
    ///
    /// `sim_duration` and `wall_duration` are provided by the orchestrator
    /// since the collector only tracks per-op timestamps.
    pub fn finalize(&self, sim_duration: Duration, wall_duration: Duration) -> ExecutionStats {
        let total_ops = self.total_ops.load(Ordering::Relaxed);
        let total_aborts = self.total_aborts.load(Ordering::Relaxed);
        let per_writer_ops: Vec<u64> = self
            .per_writer_ops
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();

        // Merge all timestamps across writers and sort
        let mut all_ts: Vec<u64> = Vec::new();
        for ts_lock in &self.timestamps {
            let ts = ts_lock.lock().expect("timestamp lock poisoned");
            all_ts.extend(ts.iter());
        }
        all_ts.sort_unstable();

        // Max stall: largest gap between consecutive sorted timestamps
        let max_stall_ns = if all_ts.len() < 2 {
            0u64
        } else {
            all_ts
                .windows(2)
                .map(|w| w[1].saturating_sub(w[0]))
                .max()
                .unwrap_or(0)
        };

        // 1-second bucket throughput: divide timeline into 1-second (1e9 ns) buckets
        let throughput_min_1s = if all_ts.is_empty() {
            0.0
        } else {
            let bucket_ns: u64 = 1_000_000_000;
            let first = all_ts[0];
            let last = *all_ts.last().unwrap();
            let num_buckets = ((last - first) / bucket_ns) + 1;

            if num_buckets == 0 {
                total_ops as f64
            } else {
                let mut buckets = vec![0u64; num_buckets as usize];
                for &ts in &all_ts {
                    let idx = ((ts - first) / bucket_ns) as usize;
                    buckets[idx] += 1;
                }
                *buckets.iter().min().unwrap_or(&0) as f64
            }
        };

        let sim_secs = sim_duration.as_secs_f64();
        let throughput_avg = if sim_secs > 0.0 {
            total_ops as f64 / sim_secs
        } else {
            0.0
        };

        ExecutionStats {
            total_ops,
            total_aborts,
            sim_duration,
            wall_duration,
            throughput_avg,
            throughput_min_1s,
            max_stall: Duration::from_nanos(max_stall_ns),
            per_writer_ops,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throughput_min_1s_calculation() {
        // 3 buckets: [0s-1s], [1s-2s], [2s-3s]
        // Bucket 0: 10 ops at 0..1s, Bucket 1: 3 ops at 1..2s, Bucket 2: 7 ops at 2..3s
        let collector = StatsCollector::new(1);

        // Bucket 0: 10 ops spread in [0ns, 999_999_999ns]
        for i in 0..10u64 {
            collector.record_op(0, i * 100_000_000); // every 100ms
        }
        // Bucket 1: 3 ops
        for i in 0..3u64 {
            collector.record_op(0, 1_000_000_000 + i * 300_000_000);
        }
        // Bucket 2: 7 ops
        for i in 0..7u64 {
            collector.record_op(0, 2_000_000_000 + i * 100_000_000);
        }

        let stats = collector.finalize(
            Duration::from_secs(3),
            Duration::from_millis(100),
        );

        assert_eq!(stats.total_ops, 20);
        // Minimum bucket has 3 ops
        assert!((stats.throughput_min_1s - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn max_stall_detection() {
        let collector = StatsCollector::new(2);

        // Writer 0: ops at 0s, 0.5s, 1s
        collector.record_op(0, 0);
        collector.record_op(0, 500_000_000);
        collector.record_op(0, 1_000_000_000);

        // Writer 1: ops at 1s then 4s (3-second gap)
        collector.record_op(1, 1_000_000_000);
        collector.record_op(1, 4_000_000_000);

        let stats = collector.finalize(
            Duration::from_secs(5),
            Duration::from_millis(50),
        );

        // Max gap: 4s - 1s = 3s
        assert_eq!(stats.max_stall, Duration::from_secs(3));
        assert_eq!(stats.total_ops, 5);
        assert_eq!(stats.per_writer_ops, vec![3, 2]);
    }

    #[test]
    fn abort_tracking() {
        let collector = StatsCollector::new(2);
        collector.record_op(0, 100);
        collector.record_abort(0, 200);
        collector.record_op(1, 300);
        collector.record_abort(1, 400);
        collector.record_abort(1, 500);

        let stats = collector.finalize(
            Duration::from_nanos(500),
            Duration::from_nanos(500),
        );

        assert_eq!(stats.total_ops, 2);
        assert_eq!(stats.total_aborts, 3);
    }

    #[test]
    fn empty_collector() {
        let collector = StatsCollector::new(4);
        let stats = collector.finalize(
            Duration::from_secs(0),
            Duration::from_secs(0),
        );
        assert_eq!(stats.total_ops, 0);
        assert_eq!(stats.throughput_min_1s, 0.0);
        assert_eq!(stats.max_stall, Duration::from_nanos(0));
        assert_eq!(stats.per_writer_ops, vec![0, 0, 0, 0]);
    }

    #[test]
    fn stats_collector_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StatsCollector>();
    }
}
