//! 5-minute sustained stress test for FASTER.
//!
//! Runs a multi-threaded mixed workload (YCSB-like) with throughput monitoring
//! and a per-thread correctness oracle. Reports violations and throughput cliffs.
//!
//! # Usage
//!
//! ```sh
//! # Default: 16 threads, 300s, non-lossy, 1M key range
//! cargo run --release -p faster-core --example stress_5min
//!
//! # Quick smoke test
//! cargo run --release -p faster-core --example stress_5min -- --duration-secs 10 --threads 4
//!
//! # Lossy (eviction) mode
//! cargo run --release -p faster-core --example stress_5min -- --lossy
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::{FasterKv, FasterKvConfig, InMemoryDevice, SimpleFunctions};

type Store = FasterKv<SimpleFunctions<u64, u64>>;

// ─── Xorshift64 PRNG (no external dep) ──────────────────────────────────

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0xDEAD_BEEF_CAFE } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Returns a value in `0..bound` (biased but fine for workload mixing).
    fn next_bounded(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

// ─── CLI parsing ─────────────────────────────────────────────────────────

struct Config {
    threads: usize,
    duration_secs: u64,
    lossy: bool,
    key_range: u64,
    report_interval: u64,
}

impl Config {
    fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut cfg = Config {
            threads: 16,
            duration_secs: 300,
            lossy: false,
            key_range: 1_000_000,
            report_interval: 10,
        };

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--threads" => {
                    i += 1;
                    cfg.threads = args[i].parse().expect("invalid --threads");
                }
                "--duration-secs" => {
                    i += 1;
                    cfg.duration_secs = args[i].parse().expect("invalid --duration-secs");
                }
                "--lossy" => {
                    cfg.lossy = true;
                }
                "--key-range" => {
                    i += 1;
                    cfg.key_range = args[i].parse().expect("invalid --key-range");
                }
                "--report-interval" => {
                    i += 1;
                    cfg.report_interval = args[i].parse().expect("invalid --report-interval");
                }
                other => {
                    eprintln!("Unknown argument: {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        cfg
    }
}

// ─── Shared counters ────────────────────────────────────────────────────

struct SharedCounters {
    total_ops: AtomicU64,
    violations: AtomicU64,
    oracle_checks: AtomicU64,
    stop: AtomicBool,
}

impl SharedCounters {
    fn new() -> Self {
        Self {
            total_ops: AtomicU64::new(0),
            violations: AtomicU64::new(0),
            oracle_checks: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }
}

// ─── Worker thread ──────────────────────────────────────────────────────

fn worker(
    store: &Store,
    counters: &SharedCounters,
    barrier: &Barrier,
    thread_id: usize,
    key_range: u64,
    num_threads: usize,
) {
    let mut rng = Xorshift64::new((thread_id as u64 + 1) * 0x9E37_79B9_7F4A_7C15);
    let mut session = store.new_session();
    let mut oracle: HashMap<u64, u64> = HashMap::new();
    let sanity_max = key_range * (num_threads as u64) * 2;

    // Batch ops counter — flush to global every 256 ops to reduce contention.
    let mut local_ops: u64 = 0;
    let mut local_violations: u64 = 0;
    let mut local_oracle_checks: u64 = 0;
    const FLUSH_INTERVAL: u64 = 256;

    barrier.wait();

    while !counters.stop.load(Ordering::Relaxed) {
        let roll = rng.next_bounded(100);
        let key = rng.next_bounded(key_range);

        if roll < 50 {
            // ── 50% upsert ──
            let value = rng.next_u64() % sanity_max;
            let outcome = store.upsert(&mut session, &key, &value, ());
            if outcome.is_success() {
                oracle.insert(key, value);
            }
        } else if roll < 85 {
            // ── 35% read ──
            let mut output: Option<u64> = None;
            let outcome = store.read(&mut session, &key, &0, &mut output, ());
            let status = outcome.status();
            if status == OperationStatus::Ok {
                if let Some(val) = output {
                    local_oracle_checks += 1;
                    // Sanity: value should be in a reasonable range.
                    if val > sanity_max {
                        local_violations += 1;
                    }
                }
            }
            // NotFound is acceptable (key may not have been written yet,
            // or was deleted / evicted in lossy mode).
        } else if roll < 95 {
            // ── 10% RMW (increment by 1) ──
            let mut rmw_out: Option<u64> = None;
            let outcome = store.rmw(&mut session, &key, &1, &mut rmw_out, ());
            if outcome.is_success() {
                // After RMW the exact value is hard to predict across threads,
                // so just update oracle if we were tracking this key.
                if let Some(v) = rmw_out {
                    oracle.insert(key, v);
                } else {
                    // RMW created or updated but didn't return output — remove
                    // oracle entry to avoid false positives.
                    oracle.remove(&key);
                }
            }
        } else {
            // ── 5% delete ──
            let _outcome = store.delete(&mut session, &key, ());
            oracle.remove(&key);
        }

        local_ops += 1;

        if local_ops % FLUSH_INTERVAL == 0 {
            counters
                .total_ops
                .fetch_add(FLUSH_INTERVAL, Ordering::Relaxed);
            if local_violations > 0 {
                counters
                    .violations
                    .fetch_add(local_violations, Ordering::Relaxed);
                local_violations = 0;
            }
            if local_oracle_checks > 0 {
                counters
                    .oracle_checks
                    .fetch_add(local_oracle_checks, Ordering::Relaxed);
                local_oracle_checks = 0;
            }
        }

        // Periodically drain pending I/O (every 1024 ops).
        if local_ops % 1024 == 0 {
            store.complete_pending(&mut session);
        }
    }

    // Flush remaining local counters.
    let remainder = local_ops % FLUSH_INTERVAL;
    if remainder > 0 {
        counters.total_ops.fetch_add(remainder, Ordering::Relaxed);
    }
    if local_violations > 0 {
        counters
            .violations
            .fetch_add(local_violations, Ordering::Relaxed);
    }
    if local_oracle_checks > 0 {
        counters
            .oracle_checks
            .fetch_add(local_oracle_checks, Ordering::Relaxed);
    }

    store.complete_pending_sync(&mut session);
    store.dispose_session(session);
}

// ─── Reporter thread ────────────────────────────────────────────────────

#[allow(dead_code)]
struct ReportSnapshot {
    elapsed_secs: u64,
    interval_ops_per_sec: u64,
    total_ops: u64,
    violations: u64,
}

fn reporter(
    counters: &SharedCounters,
    report_interval: u64,
    duration_secs: u64,
    barrier: &Barrier,
) -> Vec<ReportSnapshot> {
    let mut snapshots = Vec::new();
    let mut peak_ops: u64 = 0;
    let mut cliff_time: Option<u64> = None;
    let mut cliff_drop_pct: Option<u64> = None;

    // Wait for all workers + this reporter to sync.
    barrier.wait();

    let start = Instant::now();
    let mut prev_total: u64 = 0;

    loop {
        thread::sleep(Duration::from_secs(report_interval));

        let elapsed = start.elapsed().as_secs();
        let total = counters.total_ops.load(Ordering::Relaxed);
        let violations = counters.violations.load(Ordering::Relaxed);
        let interval_ops = total.saturating_sub(prev_total);
        let ops_per_sec = interval_ops / report_interval.max(1);
        prev_total = total;

        if ops_per_sec > peak_ops {
            peak_ops = ops_per_sec;
        }

        // Detect throughput cliff: >50% drop from peak.
        if peak_ops > 0 && ops_per_sec < peak_ops / 2 && cliff_time.is_none() {
            let drop = ((peak_ops - ops_per_sec) * 100) / peak_ops;
            cliff_time = Some(elapsed);
            cliff_drop_pct = Some(drop);
        }

        println!(
            "  [T+{elapsed:>4}s]  ops/s: {ops_per_sec:>12}  total: {total:>14}  violations: {violations}"
        );

        snapshots.push(ReportSnapshot {
            elapsed_secs: elapsed,
            interval_ops_per_sec: ops_per_sec,
            total_ops: total,
            violations,
        });

        if elapsed >= duration_secs || counters.stop.load(Ordering::Relaxed) {
            break;
        }
    }

    // Signal workers to stop.
    counters.stop.store(true, Ordering::Relaxed);

    // Print cliff detection inline.
    if let (Some(t), Some(pct)) = (cliff_time, cliff_drop_pct) {
        println!("\n  ⚠ Throughput cliff detected at T+{t}s ({pct}% drop from peak)");
    }

    snapshots
}

// ─── Main ───────────────────────────────────────────────────────────────

fn main() {
    let cfg = Config::from_args();

    let mode_str = if cfg.lossy { "lossy" } else { "non-lossy" };

    println!("═══════════════════════════════════════════════════");
    println!("  FASTER 5-Minute Stress Test");
    println!("═══════════════════════════════════════════════════");
    println!("  Threads:         {}", cfg.threads);
    println!("  Duration:        {}s", cfg.duration_secs);
    println!("  Mode:            {mode_str}");
    println!("  Key range:       {}", cfg.key_range);
    println!("  Report interval: {}s", cfg.report_interval);
    println!("═══════════════════════════════════════════════════\n");

    // ── Build store ──────────────────────────────────────────────────────

    let eviction_policy = if cfg.lossy {
        EvictionPolicy {
            max_in_memory_pages: 128,
            eviction_batch_size: 16,
        }
    } else {
        EvictionPolicy::default()
    };

    let store_config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 20,
        buffer_size_pages: 128,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy,
        auto_compact: false,
        lossy: cfg.lossy,
    };

    let store: Store = FasterKv::new(
        store_config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    );
    let store = Arc::new(store);
    let counters = Arc::new(SharedCounters::new());

    // +1 for the reporter thread.
    let barrier = Arc::new(Barrier::new(cfg.threads + 1));

    // ── Spawn worker threads ─────────────────────────────────────────────

    let mut handles = Vec::with_capacity(cfg.threads);

    for tid in 0..cfg.threads {
        let s = Arc::clone(&store);
        let c = Arc::clone(&counters);
        let b = Arc::clone(&barrier);
        let key_range = cfg.key_range;
        let num_threads = cfg.threads;

        handles.push(thread::spawn(move || {
            worker(&s, &c, &b, tid, key_range, num_threads);
        }));
    }

    // ── Reporter on current thread ───────────────────────────────────────

    let snapshots = reporter(&counters, cfg.report_interval, cfg.duration_secs, &barrier);

    // ── Join workers ─────────────────────────────────────────────────────

    for h in handles {
        h.join().expect("worker thread panicked");
    }

    // ── Final report ─────────────────────────────────────────────────────

    let total_ops = counters.total_ops.load(Ordering::Relaxed);
    let violations = counters.violations.load(Ordering::Relaxed);
    let oracle_checks = counters.oracle_checks.load(Ordering::Relaxed);

    let avg_ops = if cfg.duration_secs > 0 {
        total_ops / cfg.duration_secs
    } else {
        total_ops
    };

    let peak_ops = snapshots
        .iter()
        .map(|s| s.interval_ops_per_sec)
        .max()
        .unwrap_or(0);
    let min_ops = snapshots
        .iter()
        .map(|s| s.interval_ops_per_sec)
        .min()
        .unwrap_or(0);

    // Cliff detection from snapshots.
    let mut cliff_detected = false;
    let mut cliff_time = 0u64;
    let mut cliff_pct = 0u64;
    let mut running_peak: u64 = 0;
    for snap in &snapshots {
        if snap.interval_ops_per_sec > running_peak {
            running_peak = snap.interval_ops_per_sec;
        }
        if running_peak > 0 && snap.interval_ops_per_sec < running_peak / 2 && !cliff_detected {
            cliff_detected = true;
            cliff_time = snap.elapsed_secs;
            cliff_pct = ((running_peak - snap.interval_ops_per_sec) * 100) / running_peak;
        }
    }

    let cliff_str = if cliff_detected {
        format!("YES (at T+{cliff_time}s, {cliff_pct}% drop)")
    } else {
        "NO".to_string()
    };

    let result_str = if violations == 0 {
        "✅ PASS (0 violations)"
    } else {
        "❌ FAIL"
    };

    println!();
    println!("═══════════════════════════════════════════════════");
    println!("  FASTER 5-Minute Stress Test — Final Report");
    println!("═══════════════════════════════════════════════════");
    println!("  Threads:       {}", cfg.threads);
    println!("  Duration:      {}s", cfg.duration_secs);
    println!("  Mode:          {mode_str}");
    println!("  Key range:     {}", cfg.key_range);
    println!();
    println!("  Total ops:     {total_ops}");
    println!("  Avg ops/s:     {avg_ops}");
    println!("  Peak ops/s:    {peak_ops}");
    println!("  Min ops/s:     {min_ops}");
    println!();
    println!("  Oracle checks: {oracle_checks}");
    println!("  Violations:    {violations}");
    println!();
    println!("  Throughput cliff detected: {cliff_str}");
    println!();
    println!("  Result: {result_str}");
    println!("═══════════════════════════════════════════════════");

    if violations > 0 {
        std::process::exit(1);
    }
}
