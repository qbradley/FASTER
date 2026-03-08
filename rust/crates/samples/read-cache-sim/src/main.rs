//! Read-cache simulation using FASTER.
//!
//! Continuously writes blocks to different keys in a FASTER store, simulating
//! a read cache with a fixed working set. Supports both compaction and
//! non-compaction modes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use faster_core::SyncFileDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, Functions, ReadInfo, RmwInPlaceResult, RmwInfo, UpsertInfo};
use rand::Rng;

// ── FASTER internal page size (2^25 = 32 MiB) ──────────────────────────

const FASTER_PAGE_SIZE: usize = 1 << 25;

// ── CLI ─────────────────────────────────────────────────────────────────

/// Read-cache simulation using FASTER.
///
/// Continuously writes blocks to different keys in a FASTER store,
/// simulating a read cache with a fixed working set.
#[derive(Parser, Debug)]
#[command(name = "read-cache-sim")]
struct Args {
    /// Number of worker threads (default: number of CPUs).
    #[arg(long, default_value_t = default_threads())]
    threads: usize,

    /// Fixed block size in bytes.
    #[arg(long, default_value_t = 4096)]
    block_size: usize,

    /// Enable variable block sizes (random between 512..block-size).
    #[arg(long)]
    variable_blocks: bool,

    /// Number of unique keys (working set size).
    #[arg(long, default_value_t = 100_000)]
    key_space: u64,

    /// Run for this many seconds then stop (default: run forever).
    #[arg(long)]
    duration: Option<u64>,

    /// FASTER in-memory size in bytes (default: 128 MiB).
    /// Determines the number of in-memory pages (page size is fixed at 32 MiB).
    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    memory_size: usize,

    /// FASTER segment/file size in bytes for on-disk storage.
    #[arg(long, default_value_t = 1 << 30)]
    segment_size: u64,

    /// Enable compaction mode.
    #[arg(long)]
    compact: bool,

    /// Space amplification threshold for compaction.
    #[arg(long, default_value_t = 2.0)]
    compact_threshold: f64,

    /// Statistics reporting interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    stats_interval: u64,

    /// Percentage of operations that are reads vs writes (0-100).
    #[arg(long, default_value_t = 50)]
    read_pct: u64,

    /// Storage directory.
    #[arg(long, default_value = "./read-cache-sim-data")]
    dir: PathBuf,
}

fn default_threads() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

// ── BlockFunctions ──────────────────────────────────────────────────────

/// Custom Functions implementation for byte-block keys and values.
///
/// Keys are `u64`, values are `Vec<u8>` blocks. Input is `Vec<u8>` (the
/// block data to write). Output is `Option<Vec<u8>>` (the read result).
struct BlockFunctions;

impl Functions for BlockFunctions {
    type Key = u64;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(&self, _key: &u64, value: &Vec<u8>, _input: &Vec<u8>, output: &mut Option<Vec<u8>>, _info: &ReadInfo) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut Vec<u8>,
        input: &Vec<u8>,
        _old_value: Option<&Vec<u8>>,
        _output: &mut Option<Vec<u8>>,
        _info: &UpsertInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_initial(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_in_place(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        *value = input.clone();
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        _old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        *new_value = input.clone();
    }
}

// ── Shared stats ────────────────────────────────────────────────────────

struct Stats {
    reads: AtomicU64,
    writes: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    read_found: AtomicU64,
    read_not_found: AtomicU64,
    read_pending: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            read_found: AtomicU64::new(0),
            read_not_found: AtomicU64::new(0),
            read_pending: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            read_found: self.read_found.load(Ordering::Relaxed),
            read_not_found: self.read_not_found.load(Ordering::Relaxed),
            read_pending: self.read_pending.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
struct StatsSnapshot {
    reads: u64,
    writes: u64,
    read_bytes: u64,
    write_bytes: u64,
    read_found: u64,
    read_not_found: u64,
    read_pending: u64,
}

impl StatsSnapshot {
    fn total_ops(&self) -> u64 {
        self.reads + self.writes
    }
}

// ── Formatting helpers ──────────────────────────────────────────────────

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_rate(bytes_per_sec: f64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;

    if bytes_per_sec >= GIB {
        format!("{:.2} GiB/s", bytes_per_sec / GIB)
    } else {
        format!("{:.2} MiB/s", bytes_per_sec / MIB)
    }
}

fn format_ops(ops_per_sec: f64) -> String {
    if ops_per_sec >= 1_000_000.0 {
        format!("{:.2}M ops/s", ops_per_sec / 1_000_000.0)
    } else if ops_per_sec >= 1_000.0 {
        format!("{:.2}K ops/s", ops_per_sec / 1_000.0)
    } else {
        format!("{:.0} ops/s", ops_per_sec)
    }
}

// ── Stats reporter ──────────────────────────────────────────────────────

fn stats_reporter(
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
    start: Instant,
    live_data_estimate: u64,
) {
    let mut prev_snap = stats.snapshot();
    let mut prev_time = start;

    // Print header
    eprintln!(
        "{:>8}  {:>12}  {:>14}  {:>12}  {:>12}  {:>10}  {:>10}  {:>10}",
        "elapsed",
        "total_ops",
        "inst_ops/s",
        "write_tput",
        "read_tput",
        "hit_rate",
        "pending",
        "live_est"
    );
    eprintln!("{}", "-".repeat(106));

    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(interval);

        let now = Instant::now();
        let snap = stats.snapshot();
        let dt = now.duration_since(prev_time).as_secs_f64();
        let elapsed = now.duration_since(start).as_secs_f64();

        if dt < 0.001 {
            prev_snap = snap;
            prev_time = now;
            continue;
        }

        let delta_ops = snap.total_ops().saturating_sub(prev_snap.total_ops());
        let delta_write_bytes = snap.write_bytes.saturating_sub(prev_snap.write_bytes);
        let delta_read_bytes = snap.read_bytes.saturating_sub(prev_snap.read_bytes);

        let inst_ops = delta_ops as f64 / dt;
        let write_tput = delta_write_bytes as f64 / dt;
        let read_tput = delta_read_bytes as f64 / dt;

        let total_reads = snap.read_found + snap.read_not_found + snap.read_pending;
        let hit_rate = if total_reads > 0 {
            format!(
                "{:.1}%",
                snap.read_found as f64 / total_reads as f64 * 100.0
            )
        } else {
            "N/A".to_string()
        };

        eprintln!(
            "{:>7.1}s  {:>12}  {:>14}  {:>12}  {:>12}  {:>10}  {:>10}  {:>10}",
            elapsed,
            snap.total_ops(),
            format_ops(inst_ops),
            format_rate(write_tput),
            format_rate(read_tput),
            hit_rate,
            snap.read_pending,
            format_bytes(live_data_estimate),
        );

        prev_snap = snap;
        prev_time = now;
    }
}

// ── Worker thread ───────────────────────────────────────────────────────

fn worker(
    store: Arc<FasterKv<BlockFunctions>>,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    thread_id: usize,
    args: Arc<Args>,
) {
    let mut session = store.new_session();
    let mut rng = rand::rng();
    let mut op_count: u64 = 0;

    // Pre-generate a reusable block buffer
    let max_block = args.block_size;
    let block_buf: Vec<u8> = (0..max_block).map(|i| (i & 0xFF) as u8).collect();

    let empty_input: Vec<u8> = Vec::new();

    while !shutdown.load(Ordering::Relaxed) {
        let key: u64 = rng.random_range(0..args.key_space);
        let is_read = rng.random_range(0..100) < args.read_pct;

        if is_read {
            let mut output: Option<Vec<u8>> = None;
            let status = store.read(&mut session, &key, &empty_input, &mut output, ());

            stats.reads.fetch_add(1, Ordering::Relaxed);
            match status {
                OperationStatus::Ok => {
                    stats.read_found.fetch_add(1, Ordering::Relaxed);
                    if let Some(ref data) = output {
                        stats
                            .read_bytes
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                }
                OperationStatus::Pending => {
                    stats.read_pending.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    stats.read_not_found.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            let block_size = if args.variable_blocks {
                rng.random_range(512..=max_block)
            } else {
                max_block
            };

            let block = block_buf[..block_size].to_vec();
            let _ = store.upsert(&mut session, &key, &block, ());
            stats.writes.fetch_add(1, Ordering::Relaxed);
            stats
                .write_bytes
                .fetch_add(block_size as u64, Ordering::Relaxed);
        }

        op_count += 1;

        // Periodic refresh: allow epoch to advance and call maintenance
        if op_count % 256 == 0 {
            store.maintenance();
        }
    }

    store.dispose_session(session);
    eprintln!("  thread {thread_id}: {op_count} ops completed");
}

// ── Compaction thread ───────────────────────────────────────────────────

fn compaction_worker(
    store: Arc<FasterKv<BlockFunctions>>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
) {
    // NOTE: FASTER's compact() requires FixedSizeKey + FixedSizeValue, which
    // Vec<u8> does not satisfy. For variable-length values we rely on aggressive
    // maintenance() (flush + evict) to reclaim memory. True log compaction for
    // variable-length records will be supported in a future FASTER release.
    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(interval);
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Aggressive maintenance: shift read-only boundary, flush, evict.
        store.maintenance();
    }
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    // Validate
    assert!(args.threads > 0, "threads must be > 0");
    assert!(args.block_size > 0, "block-size must be > 0");
    assert!(args.key_space > 0, "key-space must be > 0");
    assert!(args.read_pct <= 100, "read-pct must be 0..=100");

    // Compute FASTER config
    let buffer_size_pages = (args.memory_size / FASTER_PAGE_SIZE)
        .max(4)
        .next_power_of_two();
    let hash_index_log2 = ((args.key_space as f64 * 2.0).log2().ceil() as usize).max(10);

    eprintln!("=== Read-Cache Simulation ===");
    eprintln!("  threads:        {}", args.threads);
    eprintln!("  block size:     {}", format_bytes(args.block_size as u64));
    if args.variable_blocks {
        eprintln!(
            "  variable:       512..{}",
            format_bytes(args.block_size as u64)
        );
    }
    eprintln!("  key space:      {}", args.key_space);
    eprintln!("  read pct:       {}%", args.read_pct);
    eprintln!(
        "  memory size:    {} ({buffer_size_pages} pages x 32 MiB)",
        format_bytes(args.memory_size as u64)
    );
    eprintln!("  segment size:   {}", format_bytes(args.segment_size));
    eprintln!(
        "  compaction:     {}",
        if args.compact {
            format!("enabled (threshold: {:.1}x)", args.compact_threshold)
        } else {
            "disabled".to_string()
        }
    );
    eprintln!("  storage dir:    {}", args.dir.display());
    if let Some(d) = args.duration {
        eprintln!("  duration:       {d}s");
    } else {
        eprintln!("  duration:       unlimited (Ctrl+C to stop)");
    }

    let live_data_estimate = args.key_space * args.block_size as u64;
    eprintln!("  est. live data: {}", format_bytes(live_data_estimate));
    eprintln!();

    // Create storage directory
    std::fs::create_dir_all(&args.dir).expect("failed to create storage directory");

    // Build the FASTER store
    let config = FasterKvConfig {
        hash_index_size_log2: hash_index_log2,
        buffer_size_pages,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: args.compact,
    };

    let device = SyncFileDevice::new(&args.dir, "log", 512, args.segment_size, 4)
        .expect("failed to create storage device");

    let store = Arc::new(FasterKv::new(config, BlockFunctions, device));

    // Shutdown signal
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_ctrlc = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        eprintln!("\nShutting down...");
        shutdown_ctrlc.store(true, Ordering::SeqCst);
    })
    .expect("failed to set Ctrl+C handler");

    // Duration timer
    if let Some(secs) = args.duration {
        let shutdown_timer = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            eprintln!("\nDuration limit reached ({secs}s). Shutting down...");
            shutdown_timer.store(true, Ordering::SeqCst);
        });
    }

    let start = Instant::now();
    let stats = Arc::new(Stats::new());
    let args = Arc::new(args);

    // Stats reporter thread
    let stats_reporter_handle = {
        let stats = Arc::clone(&stats);
        let shutdown = Arc::clone(&shutdown);
        let interval = Duration::from_millis(args.stats_interval);
        thread::spawn(move || {
            stats_reporter(stats, shutdown, interval, start, live_data_estimate);
        })
    };

    // Compaction thread (if enabled)
    let compaction_handle = if args.compact {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        Some(thread::spawn(move || {
            compaction_worker(store, shutdown, Duration::from_secs(5));
        }))
    } else {
        None
    };

    // Worker threads
    let mut worker_handles = Vec::with_capacity(args.threads);
    for tid in 0..args.threads {
        let store = Arc::clone(&store);
        let stats = Arc::clone(&stats);
        let shutdown = Arc::clone(&shutdown);
        let args = Arc::clone(&args);
        worker_handles.push(thread::spawn(move || {
            worker(store, stats, shutdown, tid, args);
        }));
    }

    // Wait for workers
    for h in worker_handles {
        let _ = h.join();
    }

    // Signal shutdown for auxiliary threads
    shutdown.store(true, Ordering::SeqCst);
    let _ = stats_reporter_handle.join();
    if let Some(h) = compaction_handle {
        let _ = h.join();
    }

    // Final summary
    let elapsed = start.elapsed();
    let snap = stats.snapshot();
    let elapsed_secs = elapsed.as_secs_f64();

    eprintln!();
    eprintln!("=== Final Summary ===");
    eprintln!("  elapsed:        {:.2}s", elapsed_secs);
    eprintln!("  total ops:      {}", snap.total_ops());
    eprintln!("  reads:          {}", snap.reads);
    eprintln!("  writes:         {}", snap.writes);
    if elapsed_secs > 0.0 {
        eprintln!(
            "  avg ops/s:      {}",
            format_ops(snap.total_ops() as f64 / elapsed_secs)
        );
        eprintln!(
            "  write throughput: {}",
            format_rate(snap.write_bytes as f64 / elapsed_secs)
        );
        eprintln!(
            "  read throughput:  {}",
            format_rate(snap.read_bytes as f64 / elapsed_secs)
        );
    }
    let total_reads = snap.read_found + snap.read_not_found + snap.read_pending;
    if total_reads > 0 {
        eprintln!(
            "  read hit rate:  {:.1}%",
            snap.read_found as f64 / total_reads as f64 * 100.0
        );
    }
    eprintln!("  read found:     {}", snap.read_found);
    eprintln!("  read not found: {}", snap.read_not_found);
    eprintln!("  read pending:   {}", snap.read_pending);
    eprintln!("  bytes written:  {}", format_bytes(snap.write_bytes));
    eprintln!("  bytes read:     {}", format_bytes(snap.read_bytes));
}
