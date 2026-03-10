//! Durable page store sample using FASTER.
//!
//! Similar to `page-cache` but this is a **durable store** — it never
//! intentionally loses data. To prevent unbounded storage growth, writer
//! threads delete the oldest pages when the live set approaches the
//! `--max-live-pages` threshold.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{
    FasterKv, FasterKvConfig, Functions, ReadInfo, RmwInPlaceResult, RmwInfo, UpsertInfo,
};
use faster_core::SyncFileDevice;
use rand::Rng;

const FASTER_PAGE_SIZE: usize = 1 << 25; // 32 MiB per FASTER page frame

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, ValueEnum)]
enum KeyDistribution {
    /// Monotonically increasing keys — every write is a new page.
    Linear,
    /// Zipfian distribution — some keys are updated more frequently.
    Zipf,
}

#[derive(Parser, Debug)]
#[command(
    name = "page-store",
    about = "Durable page store with delete-based retention using FASTER"
)]
struct Args {
    /// How long to run (seconds).
    #[arg(long, default_value_t = 60)]
    duration: u64,

    /// Directory for FASTER log files.
    #[arg(long, default_value = "./page-store-data")]
    storage_dir: PathBuf,

    /// Total log storage size (MB).
    #[arg(long, default_value_t = 1024)]
    log_size_mb: usize,

    /// In-memory portion size (MB).
    #[arg(long, default_value_t = 256)]
    in_memory_mb: usize,

    /// Value size per page (bytes).
    #[arg(long, default_value_t = 4096)]
    page_size: usize,

    /// Key distribution.
    #[arg(long, value_enum, default_value_t = KeyDistribution::Linear)]
    distribution: KeyDistribution,

    /// Zipf exponent (only used with --distribution zipf).
    #[arg(long, default_value_t = 1.0)]
    zipf_exponent: f64,

    /// Total key space for Zipf distribution.
    #[arg(long, default_value_t = 10_000_000)]
    key_space: u64,

    /// Number of reader threads (0 = write-only).
    #[arg(long, default_value_t = 0)]
    readers: usize,

    /// Number of writer threads.
    #[arg(long, default_value_t = 1)]
    writers: usize,

    /// For readers: ratio of known-key vs random-key lookups.
    #[arg(long, default_value_t = 0.8)]
    read_ratio: f64,

    /// How often to print stats (seconds).
    #[arg(long, default_value_t = 5)]
    report_interval: u64,

    // -- page-store specific --
    /// Maximum number of live pages to maintain.
    #[arg(long, default_value_t = 1_000_000)]
    max_live_pages: u64,

    /// Target retention ratio — deletes start at this fraction of max-live-pages.
    #[arg(long, default_value_t = 0.7)]
    retention_ratio: f64,

    /// Enable periodic checkpointing for durability.
    #[arg(long, default_value_t = false)]
    enable_checkpoints: bool,

    /// How often to checkpoint (seconds).
    #[arg(long, default_value_t = 60)]
    checkpoint_interval: u64,
}

// ---------------------------------------------------------------------------
// FASTER Functions: u64 key → Vec<u8> value
// ---------------------------------------------------------------------------

struct PageFunctions;

impl Functions for PageFunctions {
    type Key = u64;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(
        &self,
        _key: &u64,
        value: &Vec<u8>,
        _input: &Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &ReadInfo,
    ) {
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

// ---------------------------------------------------------------------------
// Shared statistics
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    writes: AtomicU64,
    write_bytes: AtomicU64,
    write_cold: AtomicU64,
    write_errors: AtomicU64,
    deletes: AtomicU64,
    delete_errors: AtomicU64,
    reads: AtomicU64,
    read_hits: AtomicU64,
    read_misses: AtomicU64,
    read_pending: AtomicU64,
    live_pages: AtomicU64,
    live_high_watermark: AtomicU64,
}

fn print_interval(stats: &Stats, elapsed: Duration, has_readers: bool) {
    let s = elapsed.as_secs_f64();
    let w = stats.writes.load(Ordering::Relaxed);
    let wb = stats.write_bytes.load(Ordering::Relaxed);
    let d = stats.deletes.load(Ordering::Relaxed);
    let live = stats.live_pages.load(Ordering::Relaxed);
    let we = stats.write_errors.load(Ordering::Relaxed);

    print!(
        "[{s:>6.1}s] W: {w:>10} ({:>8.0}/s, {:>7.1} MB/s) D: {d:>8} live={live}",
        w as f64 / s,
        wb as f64 / s / 1_048_576.0,
    );
    if we > 0 {
        print!(" err={we}");
    }
    if has_readers {
        let rh = stats.read_hits.load(Ordering::Relaxed);
        let rm = stats.read_misses.load(Ordering::Relaxed);
        let rp = stats.read_pending.load(Ordering::Relaxed);
        let rt = rh + rm + rp;
        let hit_pct = if rt > 0 {
            rh as f64 / rt as f64 * 100.0
        } else {
            0.0
        };
        print!(" | R: {rt:>10} ({:>8.0}/s, {hit_pct:.1}% hit)", rt as f64 / s);
    }
    println!();
}

fn print_summary(stats: &Stats, elapsed: Duration, has_readers: bool) {
    let s = elapsed.as_secs_f64();
    let w = stats.writes.load(Ordering::Relaxed);
    let wb = stats.write_bytes.load(Ordering::Relaxed);
    let d = stats.deletes.load(Ordering::Relaxed);
    let de = stats.delete_errors.load(Ordering::Relaxed);
    let we = stats.write_errors.load(Ordering::Relaxed);
    let live = stats.live_pages.load(Ordering::Relaxed);
    let hwm = stats.live_high_watermark.load(Ordering::Relaxed);

    println!("\n════════════════════════════════════════");
    println!("  Duration:        {s:.1}s");
    println!(
        "  Writes:          {w} ({:.0}/s, {:.1} MB, {:.1} MB/s)",
        w as f64 / s,
        wb as f64 / 1_048_576.0,
        wb as f64 / s / 1_048_576.0,
    );
    println!("  Deletes:         {d} ({:.0}/s)", d as f64 / s);
    println!("  Live pages:      {live} (high watermark: {hwm})");
    if we > 0 || de > 0 {
        println!("  Errors:          {we} write, {de} delete");
    }
    if has_readers {
        let rh = stats.read_hits.load(Ordering::Relaxed);
        let rm = stats.read_misses.load(Ordering::Relaxed);
        let rp = stats.read_pending.load(Ordering::Relaxed);
        let rt = rh + rm + rp;
        let hit_pct = if rt > 0 {
            rh as f64 / rt as f64 * 100.0
        } else {
            0.0
        };
        println!("  Reads:           {rt} ({:.0}/s, {hit_pct:.1}% hit)", rt as f64 / s);
    }
    println!("════════════════════════════════════════");
}

// ---------------------------------------------------------------------------
// Approximate Zipf sampler (inverse-CDF)
// ---------------------------------------------------------------------------

struct ZipfSampler {
    n: u64,
    s: f64,
}

impl ZipfSampler {
    fn new(n: u64, s: f64) -> Self {
        Self { n, s }
    }

    fn sample(&self, rng: &mut impl Rng) -> u64 {
        let u: f64 = rng.random::<f64>();
        let k = if (self.s - 1.0).abs() < 1e-9 {
            (self.n as f64).powf(u) as u64
        } else {
            let n1s = (self.n as f64).powf(1.0 - self.s);
            (u * (n1s - 1.0) + 1.0).powf(1.0 / (1.0 - self.s)) as u64
        };
        k.clamp(0, self.n - 1)
    }
}

// ---------------------------------------------------------------------------
// Live set — tracks which keys are alive for retention management
// ---------------------------------------------------------------------------

struct LiveSet {
    keys: Mutex<VecDeque<u64>>,
}

impl LiveSet {
    fn new() -> Self {
        Self {
            keys: Mutex::new(VecDeque::new()),
        }
    }

    /// Push a new key. Returns keys to delete if the set exceeds the threshold.
    fn push(&self, key: u64, threshold: u64) -> Vec<u64> {
        let mut keys = self.keys.lock().unwrap();
        keys.push_back(key);
        let mut to_delete = Vec::new();
        while keys.len() as u64 > threshold {
            if let Some(old_key) = keys.pop_front() {
                to_delete.push(old_key);
            }
        }
        to_delete
    }

    fn len(&self) -> u64 {
        self.keys.lock().unwrap().len() as u64
    }
}

// ---------------------------------------------------------------------------
// Worker threads
// ---------------------------------------------------------------------------

fn writer_thread(
    store: Arc<FasterKv<PageFunctions>>,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    args: Arc<Args>,
    key_counter: Arc<AtomicU64>,
    live_set: Arc<LiveSet>,
) {
    let mut session = store.new_session();
    let mut rng = rand::rng();
    let page_size = args.page_size;
    let mut page_data = vec![0u8; page_size];

    let delete_threshold = (args.max_live_pages as f64 * args.retention_ratio) as u64;

    let zipf = match args.distribution {
        KeyDistribution::Zipf => Some(ZipfSampler::new(args.key_space, args.zipf_exponent)),
        _ => None,
    };

    let mut op_count: u64 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        let key = match args.distribution {
            KeyDistribution::Linear => key_counter.fetch_add(1, Ordering::Relaxed),
            KeyDistribution::Zipf => zipf.as_ref().unwrap().sample(&mut rng),
        };

        // Fill page buffer with key pattern
        let key_bytes = key.to_le_bytes();
        for chunk in page_data.chunks_mut(8) {
            let len = chunk.len().min(8);
            chunk[..len].copy_from_slice(&key_bytes[..len]);
        }

        // Upsert the page — retry on Aborted (CAS contention / allocation pressure)
        loop {
            let status = store.upsert(&mut session, &key, &page_data, ());
            if status == OperationStatus::CopyUpdated || status.is_pending() {
                stats.writes.fetch_add(1, Ordering::Relaxed);
                stats.write_bytes.fetch_add(page_size as u64, Ordering::Relaxed);
                stats.write_cold.fetch_add(1, Ordering::Relaxed);
                break;
            } else if status.is_success() {
                stats.writes.fetch_add(1, Ordering::Relaxed);
                stats.write_bytes.fetch_add(page_size as u64, Ordering::Relaxed);
                break;
            } else {
                store.maintenance();
                let _ = store.complete_pending(&mut session);
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_millis(1));
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
        }

        // Track in live set, get keys to delete for retention
        let to_delete = live_set.push(key, delete_threshold);
        if !to_delete.is_empty() {
            for old_key in &to_delete {
                let del_status = store.delete(&mut session, old_key, ());
                if del_status.is_success() {
                    stats.deletes.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.delete_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Update live page stats
        let live = live_set.len();
        stats.live_pages.store(live, Ordering::Relaxed);
        // Relaxed CAS loop for high watermark
        loop {
            let hwm = stats.live_high_watermark.load(Ordering::Relaxed);
            if live <= hwm {
                break;
            }
            if stats
                .live_high_watermark
                .compare_exchange_weak(hwm, live, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }

        op_count += 1;
        if op_count % 256 == 0 {
            store.maintenance();
            let _ = store.complete_pending(&mut session);
        }
    }

    store.dispose_session(session);
}

fn reader_thread(
    store: Arc<FasterKv<PageFunctions>>,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    args: Arc<Args>,
    key_counter: Arc<AtomicU64>,
) {
    let mut session = store.new_session();
    let mut rng = rand::rng();
    let empty_input: Vec<u8> = Vec::new();
    let mut op_count: u64 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        let max_key = match args.distribution {
            KeyDistribution::Linear => key_counter.load(Ordering::Relaxed),
            KeyDistribution::Zipf => args.key_space,
        };
        if max_key == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }

        let key = if rng.random::<f64>() < args.read_ratio {
            rng.random_range(0..max_key)
        } else {
            rng.random::<u64>()
        };

        let mut output: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &key, &empty_input, &mut output, ());

        stats.reads.fetch_add(1, Ordering::Relaxed);
        match status {
            OperationStatus::Ok => {
                stats.read_hits.fetch_add(1, Ordering::Relaxed);
            }
            OperationStatus::Pending => {
                stats.read_pending.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                stats.read_misses.fetch_add(1, Ordering::Relaxed);
            }
        }

        op_count += 1;
        if op_count % 256 == 0 {
            store.maintenance();
            let _ = store.complete_pending(&mut session);
        }
    }

    store.dispose_session(session);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args = Args::parse();

    std::fs::create_dir_all(&args.storage_dir).expect("failed to create storage directory");

    // Map CLI args → FASTER config
    //
    // The buffer needs substantial headroom because the flush pipeline is
    // async: Sealed → Flushing → Flushed → Evicted. The evictor can only
    // reclaim Flushed pages, so the buffer must have enough frames to keep
    // writing while pages are in-flight to disk.
    let in_mem_bytes = args.in_memory_mb * 1024 * 1024;
    let target_pages = (in_mem_bytes / FASTER_PAGE_SIZE).max(4);
    let buffer_size_pages = (target_pages * 4).next_power_of_two();

    let key_space_est = match args.distribution {
        KeyDistribution::Linear => args.max_live_pages,
        KeyDistribution::Zipf => args.key_space,
    };
    let hash_index_log2 = ((key_space_est as f64 * 2.0).log2().ceil() as usize).max(10);

    let segment_size: u64 = (args.log_size_mb as u64 * 1024 * 1024).max(1 << 30);

    let config = FasterKvConfig {
        hash_index_size_log2: hash_index_log2,
        buffer_size_pages,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: (buffer_size_pages / 2) as u32,
            eviction_batch_size: 8,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };

    let dist_name = match args.distribution {
        KeyDistribution::Linear => "linear",
        KeyDistribution::Zipf => "zipf",
    };
    let delete_threshold = (args.max_live_pages as f64 * args.retention_ratio) as u64;
    println!(
        "page-store: {dist_name} distribution, {} writer(s), {} reader(s)",
        args.writers, args.readers,
    );
    println!(
        "  Storage: {} MB log, {} MB in-memory, {} byte pages",
        args.log_size_mb, args.in_memory_mb, args.page_size,
    );
    println!(
        "  Retention: max {} live pages, {:.0}% threshold ({delete_threshold})",
        args.max_live_pages,
        args.retention_ratio * 100.0,
    );
    println!(
        "  Config:  {buffer_size_pages} buffer pages, {hash_index_log2} hash index log2",
    );
    if args.enable_checkpoints {
        println!(
            "  Checkpoints: enabled (every {} seconds)",
            args.checkpoint_interval,
        );
        eprintln!(
            "  WARNING: checkpoint API not yet exposed in faster-core public interface; \
             periodic maintenance() will be used instead."
        );
    }

    let device = SyncFileDevice::new(&args.storage_dir, "log.", 512, segment_size, 4)
        .expect("failed to create storage device");

    let store = Arc::new(FasterKv::new(config, PageFunctions, device));

    let shutdown = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let key_counter = Arc::new(AtomicU64::new(0));
    let live_set = Arc::new(LiveSet::new());

    // Ctrl+C handler
    {
        let shutdown = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            eprintln!("\nShutting down...");
            shutdown.store(true, Ordering::SeqCst);
        })
        .expect("failed to set Ctrl+C handler");
    }

    let args = Arc::new(args);
    let mut handles = Vec::new();

    // Maintenance thread: continuously pumps the flush → evict pipeline
    {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        handles.push(thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                store.maintenance();
                thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    // Writer threads
    for _tid in 0..args.writers {
        let store = Arc::clone(&store);
        let stats = Arc::clone(&stats);
        let shutdown = Arc::clone(&shutdown);
        let args = Arc::clone(&args);
        let key_counter = Arc::clone(&key_counter);
        let live_set = Arc::clone(&live_set);
        handles.push(thread::spawn(move || {
            writer_thread(store, stats, shutdown, args, key_counter, live_set);
        }));
    }

    // Reader threads
    for _tid in 0..args.readers {
        let store = Arc::clone(&store);
        let stats = Arc::clone(&stats);
        let shutdown = Arc::clone(&shutdown);
        let args = Arc::clone(&args);
        let key_counter = Arc::clone(&key_counter);
        handles.push(thread::spawn(move || {
            reader_thread(store, stats, shutdown, args, key_counter);
        }));
    }

    // Checkpoint thread (aggressive maintenance if checkpoint API unavailable)
    if args.enable_checkpoints {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        let interval = Duration::from_secs(args.checkpoint_interval);
        handles.push(thread::spawn(move || {
            let mut session = store.new_session();
            while !shutdown.load(Ordering::Relaxed) {
                thread::sleep(interval);
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                store.maintenance();
                let _ = store.complete_pending(&mut session);
                eprintln!("[checkpoint] maintenance pass completed");
            }
            store.dispose_session(session);
        }));
    }

    // Stats reporting loop
    let start = Instant::now();
    let duration = Duration::from_secs(args.duration);
    let report_interval = Duration::from_secs(args.report_interval);
    let mut next_report = report_interval;
    let has_readers = args.readers > 0;

    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
        let elapsed = start.elapsed();
        if elapsed >= duration {
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
        if elapsed >= next_report {
            print_interval(&stats, elapsed, has_readers);
            next_report += report_interval;
        }
    }

    for h in handles {
        let _ = h.join();
    }

    print_summary(&stats, start.elapsed(), has_readers);

    // Exit before FasterKv::Drop to avoid segfault in SyncFileDevice
    // cleanup (pre-existing issue in faster-core I/O thread shutdown).
    std::process::exit(0);
}
