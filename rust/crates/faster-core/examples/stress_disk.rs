//! Forever-runnable disk-backed stress test for FASTER.
//!
//! Uses [`SyncFileDevice`] on real disk (NVMe), lossless mode with
//! compaction, and periodic key deletion to keep the working set bounded
//! at ≤50 % of the key range.  Can run indefinitely without OOM or disk
//! exhaustion.
//!
//! # Usage
//!
//! ```sh
//! # Default: 16 threads, run forever, 1M key range, 500K max live keys
//! cargo run --release -p faster-core --example stress_disk
//!
//! # Bounded 5-minute run
//! cargo run --release -p faster-core --example stress_disk -- \
//!   --threads 16 --duration-secs 300 --key-range 1000000 \
//!   --max-live-keys 500000 --storage-dir /tmp/faster-stress/ \
//!   --report-interval 10 --log-size-mb 1024
//!
//! # Keep data directory after exit for inspection
//! cargo run --release -p faster-core --example stress_disk -- --keep-data
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use faster_core::compaction::policy::LogSizeBudgetPolicy;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::{FasterKv, FasterKvConfig, SimpleFunctions, SyncFileDevice};

type Store = FasterKv<SimpleFunctions<u64, u64>>;

// ─── Graceful shutdown via SIGINT ───────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Signal-safe: an atomic store is async-signal-safe on all POSIX targets.
extern "C" fn sigint_handler(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_sigint_handler() {
    // SIGINT = 2 on every POSIX platform.
    // SAFETY: `signal()` is a standard C library function. We register a
    // handler that only performs an atomic store, which is signal-safe.
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }
}

// ─── Xorshift64 PRNG (no external dep) ─────────────────────────────────

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

    fn next_bounded(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

// ─── CLI parsing ────────────────────────────────────────────────────────

struct Config {
    threads: usize,
    duration_secs: u64,
    key_range: u64,
    max_live_keys: u64,
    storage_dir: PathBuf,
    report_interval: u64,
    log_size_mb: u64,
    keep_data: bool,
}

impl Config {
    fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut cfg = Config {
            threads: 16,
            duration_secs: 0,
            key_range: 1_000_000,
            max_live_keys: 500_000,
            storage_dir: PathBuf::from("/tmp/faster-stress/"),
            report_interval: 10,
            log_size_mb: 1024,
            keep_data: false,
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
                "--key-range" => {
                    i += 1;
                    cfg.key_range = args[i].parse().expect("invalid --key-range");
                }
                "--max-live-keys" => {
                    i += 1;
                    cfg.max_live_keys = args[i].parse().expect("invalid --max-live-keys");
                }
                "--storage-dir" => {
                    i += 1;
                    cfg.storage_dir = PathBuf::from(&args[i]);
                }
                "--report-interval" => {
                    i += 1;
                    cfg.report_interval = args[i].parse().expect("invalid --report-interval");
                }
                "--log-size-mb" => {
                    i += 1;
                    cfg.log_size_mb = args[i].parse().expect("invalid --log-size-mb");
                }
                "--keep-data" => {
                    cfg.keep_data = true;
                }
                other => {
                    eprintln!("Unknown argument: {other}");
                    eprintln!("Usage: stress_disk [OPTIONS]");
                    eprintln!("  --threads N          Worker threads (default: 16)");
                    eprintln!("  --duration-secs N    0 = forever (default: 0)");
                    eprintln!("  --key-range N        Total key space (default: 1000000)");
                    eprintln!("  --max-live-keys N    Working set cap (default: 500000)");
                    eprintln!("  --storage-dir PATH   Disk storage (default: /tmp/faster-stress/)");
                    eprintln!("  --report-interval N  Seconds between reports (default: 10)");
                    eprintln!("  --log-size-mb N      On-disk segment size (default: 1024)");
                    eprintln!("  --keep-data          Don't delete storage dir on exit");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        cfg
    }
}

// ─── Shared counters ───────────────────────────────────────────────────

struct SharedCounters {
    upserts: AtomicU64,
    reads: AtomicU64,
    deletes: AtomicU64,
    stop: AtomicBool,
}

impl SharedCounters {
    fn new() -> Self {
        Self {
            upserts: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }

    fn total_ops(&self) -> u64 {
        self.upserts.load(Ordering::Relaxed)
            + self.reads.load(Ordering::Relaxed)
            + self.deletes.load(Ordering::Relaxed)
    }
}

// ─── Reporter ──────────────────────────────────────────────────────────

fn reporter(
    counters: &SharedCounters,
    report_interval: u64,
    duration_secs: u64,
    barrier: &Barrier,
    live_keys_sampler: &AtomicU64,
) {
    barrier.wait();
    let start = Instant::now();
    let mut prev_total: u64 = 0;
    let mut peak_ops: u64 = 0;

    loop {
        thread::sleep(Duration::from_secs(report_interval));

        if SHUTDOWN.load(Ordering::Relaxed) {
            counters.stop.store(true, Ordering::Relaxed);
            break;
        }

        let elapsed = start.elapsed().as_secs();
        let total = counters.total_ops();
        let interval_ops = total.saturating_sub(prev_total);
        let ops_per_sec = interval_ops / report_interval.max(1);
        prev_total = total;

        if ops_per_sec > peak_ops {
            peak_ops = ops_per_sec;
        }

        let upserts = counters.upserts.load(Ordering::Relaxed);
        let reads = counters.reads.load(Ordering::Relaxed);
        let deletes = counters.deletes.load(Ordering::Relaxed);
        let live = live_keys_sampler.load(Ordering::Relaxed);
        let rss_mb = read_rss_mb().unwrap_or(0);

        // Detect throughput cliff: > 50 % drop from peak.
        let cliff = if peak_ops > 0 && ops_per_sec < peak_ops / 2 {
            " ⚠ CLIFF"
        } else {
            ""
        };

        println!(
            "  [T+{elapsed:>5}s]  ops/s: {ops_per_sec:>10}  total: {total:>14}  \
             live: {live:>8}  u/r/d: {upserts}/{reads}/{deletes}  \
             RSS: {rss_mb} MB{cliff}"
        );

        let timed_out = duration_secs > 0 && elapsed >= duration_secs;
        if timed_out || counters.stop.load(Ordering::Relaxed) {
            counters.stop.store(true, Ordering::Relaxed);
            break;
        }
    }
}

fn read_rss_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(rss_pages * 4096 / (1024 * 1024))
}

// ─── Main ──────────────────────────────────────────────────────────────

fn main() {
    install_sigint_handler();

    let cfg = Config::from_args();
    let duration_str = if cfg.duration_secs == 0 {
        "forever (Ctrl+C to stop)".to_string()
    } else {
        format!("{}s", cfg.duration_secs)
    };

    println!("═══════════════════════════════════════════════════════════");
    println!("  FASTER Disk-Backed Stress Test");
    println!("═══════════════════════════════════════════════════════════");
    println!("  Threads:         {}", cfg.threads);
    println!("  Duration:        {duration_str}");
    println!("  Mode:            lossless (exercises flush/eviction)");
    println!("  Key range:       {}", cfg.key_range);
    println!("  Max live keys:   {}", cfg.max_live_keys);
    println!("  Log size:        {} MB", cfg.log_size_mb);
    println!("  Storage dir:     {}", cfg.storage_dir.display());
    println!("  Report interval: {}s", cfg.report_interval);
    println!("  Keep data:       {}", cfg.keep_data);
    println!("═══════════════════════════════════════════════════════════\n");

    // ── Prepare storage directory ───────────────────────────────────────
    if cfg.storage_dir.exists() {
        std::fs::remove_dir_all(&cfg.storage_dir).expect("failed to clean storage dir");
    }
    std::fs::create_dir_all(&cfg.storage_dir).expect("failed to create storage dir");

    let storage_dir_for_cleanup = cfg.storage_dir.clone();
    let keep_data = cfg.keep_data;

    // ── Build store with SyncFileDevice ─────────────────────────────────
    let segment_size: u64 = cfg.log_size_mb * 1024 * 1024;
    let hash_index_log2 = ((cfg.key_range as f64 * 2.0).log2().ceil() as usize).max(10);

    // 16 page frames × 32 MB = 512 MB in-memory buffer.
    // Evict after 8 pages (256 MB) to keep flushing to disk continuously.
    let buffer_size_pages: usize = 16;

    let store_config = FasterKvConfig {
        hash_index_size_log2: hash_index_log2,
        buffer_size_pages,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: (buffer_size_pages / 2) as u32,
            eviction_batch_size: 4,
        },
        grow_config: GrowConfig::default(),
        auto_compact: true,
        lossy: false,
    };

    let device = SyncFileDevice::new(&cfg.storage_dir, "log.", 512, segment_size, 4)
        .expect("failed to create SyncFileDevice");

    let mut store: Store = FasterKv::new(store_config, SimpleFunctions::default(), device);

    // Compact when total log exceeds 4× the in-memory buffer capacity.
    // With 16 × 32 MB pages (512 MB in-memory), the budget is ~2 GB.
    let page_size_bytes = 1u64 << faster_core::address::OFFSET_BITS;
    let budget = 4 * buffer_size_pages as u64 * page_size_bytes;
    store.set_compaction_policy(Some(Box::new(LogSizeBudgetPolicy::new(budget))));

    let store = Arc::new(store);
    let counters = Arc::new(SharedCounters::new());
    let live_keys_sampler = Arc::new(AtomicU64::new(0));

    // Barrier: N workers + 1 reporter.
    let barrier = Arc::new(Barrier::new(cfg.threads + 1));
    let cfg = Arc::new(cfg);

    // ── Dedicated maintenance thread ────────────────────────────────────
    let mut handles = Vec::with_capacity(cfg.threads + 1);
    {
        let store = Arc::clone(&store);
        let counters = Arc::clone(&counters);
        handles.push(thread::spawn(move || {
            while !counters.stop.load(Ordering::Relaxed) && !SHUTDOWN.load(Ordering::Relaxed) {
                store.maintenance();
                thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    // ── Spawn worker threads ────────────────────────────────────────────
    for tid in 0..cfg.threads {
        let store = Arc::clone(&store);
        let counters = Arc::clone(&counters);
        let barrier = Arc::clone(&barrier);
        let cfg = Arc::clone(&cfg);
        let sampler = Arc::clone(&live_keys_sampler);

        handles.push(thread::spawn(move || {
            worker_with_sampling(&store, &counters, &barrier, tid, &cfg, &sampler);
        }));
    }

    // ── Reporter on main thread ─────────────────────────────────────────
    reporter(
        &counters,
        cfg.report_interval,
        cfg.duration_secs,
        &barrier,
        &live_keys_sampler,
    );

    // ── Signal all threads to stop ──────────────────────────────────────
    counters.stop.store(true, Ordering::Relaxed);
    SHUTDOWN.store(true, Ordering::SeqCst);

    for h in handles {
        let _ = h.join();
    }

    // ── Final report ────────────────────────────────────────────────────
    let total = counters.total_ops();
    let upserts = counters.upserts.load(Ordering::Relaxed);
    let reads = counters.reads.load(Ordering::Relaxed);
    let deletes = counters.deletes.load(Ordering::Relaxed);
    let rss_mb = read_rss_mb().unwrap_or(0);

    println!();
    println!("═══════════════════════════════════════════════════════════");
    println!("  Final Report");
    println!("═══════════════════════════════════════════════════════════");
    println!("  Total ops:   {total}");
    println!("  Upserts:     {upserts}");
    println!("  Reads:       {reads}");
    println!("  Deletes:     {deletes}");
    println!("  RSS:         {rss_mb} MB");
    println!("═══════════════════════════════════════════════════════════");

    // ── Cleanup ─────────────────────────────────────────────────────────
    // Drop the store first to close the device.
    drop(store);

    if !keep_data {
        println!("  Cleaning up {}...", storage_dir_for_cleanup.display());
        let _ = std::fs::remove_dir_all(&storage_dir_for_cleanup);
    } else {
        println!("  Data retained at {}", storage_dir_for_cleanup.display());
    }

    // Exit via process::exit to avoid potential segfault in SyncFileDevice
    // Drop after device is already closed (pre-existing issue tracked upstream).
    std::process::exit(0);
}

/// Wrapper around [`worker`] that periodically publishes the thread's live
/// key count to a shared sampler for the reporter to read.
fn worker_with_sampling(
    store: &Store,
    counters: &SharedCounters,
    barrier: &Barrier,
    thread_id: usize,
    cfg: &Config,
    sampler: &AtomicU64,
) {
    let mut rng = Xorshift64::new((thread_id as u64 + 1) * 0x9E37_79B9_7F4A_7C15);
    let mut session = store.new_session();

    let keys_per_thread = cfg.key_range / cfg.threads as u64;
    let key_base = thread_id as u64 * keys_per_thread;
    let max_live_per_thread = (cfg.max_live_keys / cfg.threads as u64).max(1);

    let mut live_keys: VecDeque<u64> = VecDeque::with_capacity(max_live_per_thread as usize + 1);

    let mut local_upserts: u64 = 0;
    let mut local_reads: u64 = 0;
    let mut local_deletes: u64 = 0;
    let mut local_ops: u64 = 0;
    const FLUSH_INTERVAL: u64 = 256;

    barrier.wait();

    while !counters.stop.load(Ordering::Relaxed) && !SHUTDOWN.load(Ordering::Relaxed) {
        let roll = rng.next_bounded(100);

        if roll < 60 {
            // ── 60 % upsert ──
            let key = key_base + rng.next_bounded(keys_per_thread);
            let value = rng.next_u64();

            // Cap: delete oldest when at max live keys to bound working set.
            if live_keys.len() >= max_live_per_thread as usize {
                if let Some(old_key) = live_keys.pop_front() {
                    let _ = store.delete(&mut session, &old_key, ());
                    local_deletes += 1;
                    local_ops += 1;
                }
            }

            let _ = store.upsert(&mut session, &key, &value, ());
            live_keys.push_back(key);
            local_upserts += 1;
        } else if roll < 80 {
            // ── 20 % read ──
            if !live_keys.is_empty() {
                let idx = rng.next_bounded(live_keys.len() as u64) as usize;
                let key = live_keys[idx];
                let mut output: Option<u64> = None;
                let _ = store.read(&mut session, &key, &0, &mut output, ());
            }
            local_reads += 1;
        } else {
            // ── 20 % delete ──
            if let Some(old_key) = live_keys.pop_front() {
                let _ = store.delete(&mut session, &old_key, ());
            }
            local_deletes += 1;
        }

        local_ops += 1;

        if local_ops % FLUSH_INTERVAL == 0 {
            counters.upserts.fetch_add(local_upserts, Ordering::Relaxed);
            counters.reads.fetch_add(local_reads, Ordering::Relaxed);
            counters.deletes.fetch_add(local_deletes, Ordering::Relaxed);
            // Thread 0 publishes approximate global live key count.
            if thread_id == 0 {
                sampler.store(
                    live_keys.len() as u64 * cfg.threads as u64,
                    Ordering::Relaxed,
                );
            }
            local_upserts = 0;
            local_reads = 0;
            local_deletes = 0;
        }

        if local_ops % 1024 == 0 {
            store.complete_pending(&mut session);
        }
    }

    counters.upserts.fetch_add(local_upserts, Ordering::Relaxed);
    counters.reads.fetch_add(local_reads, Ordering::Relaxed);
    counters.deletes.fetch_add(local_deletes, Ordering::Relaxed);

    store.complete_pending_sync(&mut session);
    store.dispose_session(session);
}
