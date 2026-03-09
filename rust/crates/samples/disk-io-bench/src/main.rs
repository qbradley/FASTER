//! Disk I/O benchmark comparing FASTER device implementations.
//!
//! Runs disk-bound workloads against SyncFileDevice, UringDevice, and
//! TokioFileDevice with both sync and Tokio client modes. Designed for
//! execution on a remote VM with local NVMe storage.
//!
//! # Usage
//!
//! ```bash
//! cargo run -p disk-io-bench --release -- --help
//! cargo run -p disk-io-bench --release -- --device sync --workload overcommit --threads 4
//! cargo run -p disk-io-bench --release -- --run-all --output json --output-file results.json
//! cargo run -p disk-io-bench --release -- --force-disk --iterations 5
//! ```
//!
//! # Memory Budget
//!
//! For true disk I/O, the dataset (`num_keys * (8 + value_size)`) must
//! exceed `buffer_pages * 32 KiB`. On a 32 GB VM with 10M keys and 1 KiB
//! values (~10 GB), use buffer_pages=16 (512 KiB window) to force the
//! vast majority of reads through the device layer.

mod distribution;
mod stats;
mod workload;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use faster_core::SyncFileDevice;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::record::Value;
use faster_core::status::OperationStatus;
use faster_core::store::{
    DeleteInfo, FasterKv, FasterKvConfig, Functions, ReadInfo, RmwInPlaceResult, RmwInfo,
    UpsertInfo,
};
use serde::Serialize;

use distribution::{Distribution, KeyGenerator};
use stats::{IterationSummary, LatencyHistogram};
use workload::{Op, WorkloadSpec};

// ── CLI ──────────────────────────────────────────────────────────────

/// Disk I/O benchmark for FASTER device comparison.
///
/// Compares SyncFileDevice, UringDevice, and TokioFileDevice under
/// disk-bound workloads with both sync and Tokio client modes.
///
/// # Memory Budget
///
/// For true disk I/O, the dataset (`num_keys * (8 + value_size)`) must
/// exceed `buffer_pages * 32 KiB`. On a 32 GB VM with default 10M keys
/// and 1 KiB values (~10 GB), buffer_pages=16 gives a 512 KiB in-memory
/// window — forcing the vast majority of reads to go through the device.
#[derive(Parser, Debug, Clone)]
#[command(name = "disk-io-bench", version)]
struct Cli {
    /// Device type: sync, uring, tokio
    #[arg(short, long, default_value = "sync")]
    device: String,

    /// Client type: sync (std::thread), tokio (async tasks)
    #[arg(short, long, default_value = "sync")]
    client: String,

    /// Workload: overcommit, write-heavy, mixed, scan
    #[arg(short, long, default_value = "mixed")]
    workload: String,

    /// Number of worker threads/tasks
    #[arg(short, long, default_value = "4")]
    threads: usize,

    /// Number of keys to pre-load
    #[arg(short, long, default_value = "10000000")]
    num_keys: u64,

    /// Value size in bytes
    #[arg(short = 's', long, default_value = "1024")]
    value_size: usize,

    /// In-memory buffer pages (controls memory pressure). Each page is 32 KiB.
    /// Lower values force more disk I/O. Use --force-disk to auto-tune.
    #[arg(short, long, default_value = "16")]
    buffer_pages: usize,

    /// Measurement duration in seconds
    #[arg(long, default_value = "30")]
    duration: u64,

    /// Warmup duration in seconds
    #[arg(long, default_value = "10")]
    warmup: u64,

    /// Storage directory for device files
    #[arg(long, default_value = "/tmp/faster-bench-data")]
    data_dir: PathBuf,

    /// Epoch refresh interval (ops between refresh)
    #[arg(long, default_value = "64")]
    refresh_interval: u64,

    /// io_uring queue depth
    #[arg(long, default_value = "256")]
    queue_depth: u32,

    /// Enable O_DIRECT for uring device
    #[arg(long)]
    direct_io: bool,

    /// Output format: table, json, csv
    #[arg(long, default_value = "table")]
    output: String,

    /// Write results to file
    #[arg(long)]
    output_file: Option<PathBuf>,

    /// Run full benchmark matrix (all device x client x workload x thread combos)
    #[arg(long)]
    run_all: bool,

    /// Latency sampling rate (1 in N operations)
    #[arg(long, default_value = "100")]
    latency_sample_rate: u64,

    /// Number of io threads for SyncFileDevice
    #[arg(long, default_value = "4")]
    io_threads: usize,

    /// Number of measurement iterations for statistical rigor (min 1)
    #[arg(long, default_value = "5")]
    iterations: usize,

    /// Auto-tune buffer_pages to guarantee disk-bound behavior.
    /// Starts at buffer_pages=16 and halves until pending_rate > 0%.
    #[arg(long)]
    force_disk: bool,
}

// ── Value types ──────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[repr(C, align(8))]
struct Value1024([u8; 1024]);

impl Default for Value1024 {
    fn default() -> Self {
        Self([0u8; 1024])
    }
}

impl Value for Value1024 {
    fn serialized_size(&self) -> usize {
        1024
    }
    fn serialize(&self, buf: &mut [u8]) -> usize {
        buf[..1024].copy_from_slice(&self.0);
        1024
    }
    fn deserialize(buf: &[u8]) -> Self {
        let mut v = [0u8; 1024];
        v.copy_from_slice(&buf[..1024]);
        Self(v)
    }
}

struct BenchFunctions;
impl Default for BenchFunctions {
    fn default() -> Self {
        Self
    }
}
unsafe impl Send for BenchFunctions {}
unsafe impl Sync for BenchFunctions {}

impl Functions for BenchFunctions {
    type Key = u64;
    type Value = Value1024;
    type Input = Value1024;
    type Output = Option<Value1024>;
    type Context = ();

    fn read(
        &self,
        _key: &u64,
        value: &Value1024,
        _input: &Value1024,
        output: &mut Option<Value1024>,
        _info: &ReadInfo,
    ) {
        *output = Some(*value);
    }

    fn read_completion(
        &self,
        _key: &u64,
        _output: &Option<Value1024>,
        _ctx: &(),
        _status: OperationStatus,
    ) {
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut Value1024,
        input: &Value1024,
        _old: Option<&Value1024>,
        _output: &mut Option<Value1024>,
        _info: &UpsertInfo,
    ) {
        *value = *input;
    }

    fn rmw_in_place(
        &self,
        _key: &u64,
        input: &Value1024,
        value: &mut Value1024,
        _output: &mut Option<Value1024>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        *value = *input;
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &Value1024,
        _old: &Value1024,
        new_value: &mut Value1024,
        _output: &mut Option<Value1024>,
        _info: &RmwInfo,
    ) {
        *new_value = *input;
    }

    fn rmw_initial(
        &self,
        _key: &u64,
        input: &Value1024,
        value: &mut Value1024,
        _output: &mut Option<Value1024>,
        _info: &RmwInfo,
    ) {
        *value = *input;
    }

    fn delete(&self, _key: &u64, _value: &mut Value1024, _info: &DeleteInfo) {}
}

// ── Configuration types ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeviceType {
    Sync,
    Uring,
    Tokio,
}

impl DeviceType {
    fn name(&self) -> &'static str {
        match self {
            DeviceType::Sync => "sync",
            DeviceType::Uring => "uring",
            DeviceType::Tokio => "tokio",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClientType {
    Sync,
    Tokio,
}

impl ClientType {
    fn name(&self) -> &'static str {
        match self {
            ClientType::Sync => "sync",
            ClientType::Tokio => "tokio",
        }
    }
}

// ── Benchmark result ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct BenchmarkConfig {
    device: String,
    client: String,
    workload: String,
    threads: usize,
    num_keys: u64,
    value_size: usize,
    buffer_pages: usize,
    duration_secs: u64,
    iterations: usize,
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkResults {
    ops_per_sec: f64,
    throughput_mb_s: f64,
    p50_ns: f64,
    p99_ns: f64,
    p999_ns: f64,
    pending_rate: f64,
    total_ops: u64,
    total_reads: u64,
    total_writes: u64,
    total_pending: u64,
    elapsed_secs: f64,
    // Statistical fields across iterations
    ops_per_sec_stddev: f64,
    ops_per_sec_ci95_lo: f64,
    ops_per_sec_ci95_hi: f64,
    high_variance: bool,
    /// "DISK-BOUND", "IN-MEMORY BASELINE", or "MIXED"
    io_classification: String,
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkReport {
    config: BenchmarkConfig,
    results: BenchmarkResults,
}

/// Results from a single iteration (no stats aggregation).
#[derive(Debug, Clone)]
struct SingleRunResults {
    ops_per_sec: f64,
    #[allow(dead_code)]
    throughput_mb_s: f64,
    p50_ns: f64,
    p99_ns: f64,
    p999_ns: f64,
    pending_rate: f64,
    total_ops: u64,
    total_reads: u64,
    total_writes: u64,
    total_pending: u64,
    elapsed_secs: f64,
}

// ── Store construction helpers ───────────────────────────────────────

fn make_config(num_keys: u64, buffer_pages: usize) -> FasterKvConfig {
    let n = num_keys as usize;
    let log2 = ((n as f64 * 2.0).log2().ceil() as usize).max(16);
    FasterKvConfig {
        hash_index_size_log2: log2,
        buffer_size_pages: buffer_pages,
        mutable_fraction: 0.9,
        sector_size: 4096,
        eviction_policy: EvictionPolicy::default(),
        grow_config: faster_core::grow::GrowConfig::default(),
        auto_compact: false,
    }
}

const SEGMENT_SIZE: u64 = 1 << 30; // 1 GiB segments

fn make_sync_device(data_dir: &std::path::Path, io_threads: usize) -> SyncFileDevice {
    SyncFileDevice::new(data_dir, "log.", 4096, SEGMENT_SIZE, io_threads)
        .expect("failed to create SyncFileDevice")
}

#[cfg(target_os = "linux")]
fn make_uring_device(
    data_dir: &std::path::Path,
    queue_depth: u32,
    direct_io: bool,
) -> faster_uring::UringDevice {
    use faster_uring::{BatchPolicy, UringConfig, UringDeviceConfig};
    let config = UringDeviceConfig {
        base_path: data_dir.to_path_buf(),
        prefix: "log.".to_string(),
        sector_size: 4096,
        segment_size: SEGMENT_SIZE,
        ring_config: UringConfig {
            queue_depth,
            sq_poll: false,
            direct_io,
        },
        batch_policy: BatchPolicy::default(),
    };
    faster_uring::UringDevice::new(config).expect("failed to create UringDevice")
}

// ── Population ───────────────────────────────────────────────────────

fn populate(store: &FasterKv<BenchFunctions>, num_keys: u64) {
    let input = Value1024::default();
    let chunk_size = 100_000u64;
    let mut session = store.new_session();
    {
        let mut ctx = store.unsafe_context(&mut session);
        for k in 0..num_keys {
            let _ = ctx.upsert(store, &k, &input, ());
            if k % 64 == 0 {
                ctx.refresh();
            }
            if k % chunk_size == 0 && k > 0 {
                drop(ctx);
                loop {
                    let r = store.try_complete_pending(&mut session, false);
                    if r.is_done() {
                        break;
                    }
                    std::thread::yield_now();
                }
                ctx = store.unsafe_context(&mut session);
                if k % (chunk_size * 10) == 0 {
                    eprint!(
                        "\r  Populated {:.1}M / {:.1}M keys...",
                        k as f64 / 1_000_000.0,
                        num_keys as f64 / 1_000_000.0
                    );
                }
            }
        }
    }
    loop {
        let r = store.try_complete_pending(&mut session, false);
        if r.is_done() {
            break;
        }
        std::thread::yield_now();
    }
    store.dispose_session(session);
    store.maintenance();
    eprintln!(
        "\r  Populated {:.1}M keys — done.                    ",
        num_keys as f64 / 1_000_000.0
    );
}

// ── Thread stats ─────────────────────────────────────────────────────

struct ThreadStats {
    ops: u64,
    reads: u64,
    writes: u64,
    pending_count: u64,
    histogram: LatencyHistogram,
    elapsed: Duration,
}

// ── Sync client benchmark ────────────────────────────────────────────

fn run_sync_worker(
    store: &Arc<FasterKv<BenchFunctions>>,
    workload: WorkloadSpec,
    dist: Distribution,
    num_keys: u64,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    latency_sample_rate: u64,
    tid: usize,
) -> ThreadStats {
    let input = Value1024::default();
    let mut output: Option<Value1024> = None;
    let mut keygen = KeyGenerator::new(dist, num_keys, tid as u64);
    let mut rng_seed: u64 = 0xDEAD_BEEF_u64.wrapping_add(tid as u64 * 0x1337);

    // Warmup phase
    let warmup_start = Instant::now();
    let mut session = store.new_session();
    {
        let mut ctx = store.unsafe_context(&mut session);
        let mut op_count = 0u64;
        while warmup_start.elapsed() < warmup_duration {
            for _ in 0..256 {
                let k = keygen.next_key();
                match workload.next_op(&mut rng_seed) {
                    Op::Read => {
                        let status = ctx.read(store, &k, &input, &mut output, ());
                        if status == OperationStatus::Pending {
                            drop(ctx);
                            complete_pending_sync(store, &mut session);
                            ctx = store.unsafe_context(&mut session);
                        }
                    }
                    Op::Upsert => {
                        let _ = ctx.upsert(store, &k, &input, ());
                    }
                    Op::Rmw => {
                        let _ = ctx.rmw(store, &k, &input, &mut output, ());
                    }
                }
                op_count += 1;
                if op_count % refresh_interval == 0 {
                    ctx.refresh();
                }
            }
        }
    }
    store.dispose_session(session);

    // Measurement phase
    let mut histogram = LatencyHistogram::new();
    let mut ops: u64 = 0;
    let mut reads: u64 = 0;
    let mut writes: u64 = 0;
    let mut pending_count: u64 = 0;

    let mut session = store.new_session();
    let measure_start = Instant::now();
    {
        let mut ctx = store.unsafe_context(&mut session);
        while measure_start.elapsed() < run_duration {
            for _ in 0..256 {
                let k = keygen.next_key();
                let op = workload.next_op(&mut rng_seed);
                let sample = ops % latency_sample_rate == 0;
                let op_start = if sample {
                    Instant::now()
                } else {
                    measure_start
                };

                match op {
                    Op::Read => {
                        let status = ctx.read(store, &k, &input, &mut output, ());
                        reads += 1;
                        if status == OperationStatus::Pending {
                            pending_count += 1;
                            drop(ctx);
                            complete_pending_sync(store, &mut session);
                            ctx = store.unsafe_context(&mut session);
                        }
                    }
                    Op::Upsert => {
                        let _ = ctx.upsert(store, &k, &input, ());
                        writes += 1;
                    }
                    Op::Rmw => {
                        let _ = ctx.rmw(store, &k, &input, &mut output, ());
                        writes += 1;
                    }
                }

                if sample {
                    histogram.record(op_start.elapsed());
                }
                ops += 1;
                if ops % refresh_interval == 0 {
                    ctx.refresh();
                }
            }
        }
    }
    loop {
        let r = store.try_complete_pending(&mut session, false);
        if r.is_done() {
            break;
        }
        std::thread::yield_now();
    }
    let elapsed = measure_start.elapsed();
    store.dispose_session(session);

    ThreadStats {
        ops,
        reads,
        writes,
        pending_count,
        histogram,
        elapsed,
    }
}

fn complete_pending_sync(
    store: &FasterKv<BenchFunctions>,
    session: &mut faster_core::store::FasterSession<BenchFunctions>,
) {
    loop {
        let r = store.try_complete_pending(session, false);
        if r.is_done() {
            break;
        }
        std::thread::yield_now();
    }
}

fn run_sync_bench(
    store: Arc<FasterKv<BenchFunctions>>,
    workload: WorkloadSpec,
    dist: Distribution,
    num_keys: u64,
    threads: usize,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    latency_sample_rate: u64,
) -> SingleRunResults {
    let handles: Vec<_> = (0..threads)
        .map(|tid| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                run_sync_worker(
                    &store,
                    workload,
                    dist,
                    num_keys,
                    run_duration,
                    warmup_duration,
                    refresh_interval,
                    latency_sample_rate,
                    tid,
                )
            })
        })
        .collect();

    aggregate_results(handles, 1024)
}

// ── Tokio client benchmark ───────────────────────────────────────────

fn run_tokio_bench(
    store: Arc<FasterKv<BenchFunctions>>,
    workload: WorkloadSpec,
    dist: Distribution,
    num_keys: u64,
    threads: usize,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    latency_sample_rate: u64,
) -> SingleRunResults {
    let handles: Vec<_> = (0..threads)
        .map(|tid| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime");
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    run_tokio_worker(
                        &store,
                        workload,
                        dist,
                        num_keys,
                        run_duration,
                        warmup_duration,
                        refresh_interval,
                        latency_sample_rate,
                        tid,
                    )
                    .await
                })
            })
        })
        .collect();

    aggregate_results(handles, 1024)
}

async fn run_tokio_worker(
    store: &FasterKv<BenchFunctions>,
    workload: WorkloadSpec,
    dist: Distribution,
    num_keys: u64,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    latency_sample_rate: u64,
    tid: usize,
) -> ThreadStats {
    use faster_tokio::AsyncSession;

    let input = Value1024::default();
    let mut output: Option<Value1024> = None;
    let mut keygen = KeyGenerator::new(dist, num_keys, tid as u64);
    let mut rng_seed: u64 = 0xDEAD_BEEF_u64.wrapping_add(tid as u64 * 0x1337);

    // Warmup
    let warmup_start = Instant::now();
    {
        let mut session = AsyncSession::new(store);
        let mut op_count = 0u64;
        while warmup_start.elapsed() < warmup_duration {
            for _ in 0..256 {
                let k = keygen.next_key();
                match workload.next_op(&mut rng_seed) {
                    Op::Read => {
                        let status = session.read(&k, &input, &mut output, ());
                        if status == OperationStatus::Pending {
                            session.complete_pending().await;
                        }
                    }
                    Op::Upsert => {
                        let _ = session.upsert(&k, &input, ());
                    }
                    Op::Rmw => {
                        let _ = session.rmw(&k, &input, &mut output, ());
                    }
                }
                op_count += 1;
                if op_count % refresh_interval == 0 {
                    let _ = session.refresh().await;
                }
            }
        }
    }

    // Measurement
    let mut histogram = LatencyHistogram::new();
    let mut ops: u64 = 0;
    let mut reads: u64 = 0;
    let mut writes: u64 = 0;
    let mut pending_count: u64 = 0;

    let measure_start = Instant::now();
    {
        let mut session = AsyncSession::new(store);
        while measure_start.elapsed() < run_duration {
            for _ in 0..256 {
                let k = keygen.next_key();
                let op = workload.next_op(&mut rng_seed);
                let sample = ops % latency_sample_rate == 0;
                let op_start = if sample {
                    Instant::now()
                } else {
                    measure_start
                };

                match op {
                    Op::Read => {
                        let status = session.read(&k, &input, &mut output, ());
                        reads += 1;
                        if status == OperationStatus::Pending {
                            pending_count += 1;
                            session.complete_pending().await;
                        }
                    }
                    Op::Upsert => {
                        let _ = session.upsert(&k, &input, ());
                        writes += 1;
                    }
                    Op::Rmw => {
                        let _ = session.rmw(&k, &input, &mut output, ());
                        writes += 1;
                    }
                }

                if sample {
                    histogram.record(op_start.elapsed());
                }
                ops += 1;
                if ops % refresh_interval == 0 {
                    let _ = session.refresh().await;
                }
            }
        }
        session.complete_pending().await;
    }
    let elapsed = measure_start.elapsed();

    ThreadStats {
        ops,
        reads,
        writes,
        pending_count,
        histogram,
        elapsed,
    }
}

// ── Result aggregation ───────────────────────────────────────────────

fn aggregate_results(
    handles: Vec<std::thread::JoinHandle<ThreadStats>>,
    value_size: usize,
) -> SingleRunResults {
    let mut total_ops: u64 = 0;
    let mut total_reads: u64 = 0;
    let mut total_writes: u64 = 0;
    let mut total_pending: u64 = 0;
    let mut max_elapsed = Duration::ZERO;
    let mut merged_histogram = LatencyHistogram::new();

    for h in handles {
        let stats = h.join().expect("worker thread panicked");
        total_ops += stats.ops;
        total_reads += stats.reads;
        total_writes += stats.writes;
        total_pending += stats.pending_count;
        if stats.elapsed > max_elapsed {
            max_elapsed = stats.elapsed;
        }
        merged_histogram.merge(&stats.histogram);
    }

    let elapsed_secs = max_elapsed.as_secs_f64();
    let ops_per_sec = total_ops as f64 / elapsed_secs;
    let record_size = (8 + value_size) as f64;
    let throughput_mb_s = (ops_per_sec * record_size) / (1024.0 * 1024.0);
    let pending_rate = if total_ops > 0 {
        total_pending as f64 / total_ops as f64
    } else {
        0.0
    };

    SingleRunResults {
        ops_per_sec,
        throughput_mb_s,
        p50_ns: merged_histogram.percentile(50.0),
        p99_ns: merged_histogram.percentile(99.0),
        p999_ns: merged_histogram.percentile(99.9),
        pending_rate,
        total_ops,
        total_reads,
        total_writes,
        total_pending,
        elapsed_secs,
    }
}

// ── I/O classification ───────────────────────────────────────────────

fn classify_io(pending_rate: f64) -> &'static str {
    if pending_rate < 0.001 {
        "IN-MEMORY BASELINE"
    } else if pending_rate < 0.05 {
        "MIXED"
    } else {
        "DISK-BOUND"
    }
}

// ── Single iteration executor ────────────────────────────────────────

fn run_single_iteration(spec: &BenchSpec) -> SingleRunResults {
    let bench_dir = spec.data_dir.join(format!(
        "{}_{}_{}",
        spec.device.name(),
        spec.client.name(),
        spec.workload.name()
    ));
    let _ = std::fs::remove_dir_all(&bench_dir);
    std::fs::create_dir_all(&bench_dir).expect("failed to create bench data dir");

    let config = make_config(spec.num_keys, spec.buffer_pages);
    let dist = spec.workload.distribution();

    let results = match (spec.device, spec.client) {
        (DeviceType::Sync, ClientType::Sync) => {
            let device = make_sync_device(&bench_dir, spec.io_threads);
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_sync_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
        (DeviceType::Sync, ClientType::Tokio) => {
            let device = make_sync_device(&bench_dir, spec.io_threads);
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_tokio_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
        #[cfg(target_os = "linux")]
        (DeviceType::Uring, ClientType::Sync) => {
            let device = make_uring_device(&bench_dir, spec.queue_depth, spec.direct_io);
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_sync_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
        #[cfg(target_os = "linux")]
        (DeviceType::Uring, ClientType::Tokio) => {
            let device = make_uring_device(&bench_dir, spec.queue_depth, spec.direct_io);
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_tokio_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
        #[cfg(not(target_os = "linux"))]
        (DeviceType::Uring, _) => {
            eprintln!("  Warning: UringDevice requires Linux -- skipping");
            return SingleRunResults {
                ops_per_sec: 0.0,
                throughput_mb_s: 0.0,
                p50_ns: 0.0,
                p99_ns: 0.0,
                p999_ns: 0.0,
                pending_rate: 0.0,
                total_ops: 0,
                total_reads: 0,
                total_writes: 0,
                total_pending: 0,
                elapsed_secs: 0.0,
            };
        }
        (DeviceType::Tokio, ClientType::Sync) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let _guard = rt.enter();
            let device = faster_tokio::TokioFileDevice::new(&bench_dir, "log.", 4096, SEGMENT_SIZE)
                .expect("failed to create TokioFileDevice");
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_sync_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
        (DeviceType::Tokio, ClientType::Tokio) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let _guard = rt.enter();
            let device = faster_tokio::TokioFileDevice::new(&bench_dir, "log.", 4096, SEGMENT_SIZE)
                .expect("failed to create TokioFileDevice");
            let store = Arc::new(FasterKv::new(config, BenchFunctions, device));
            populate(&store, spec.num_keys);
            run_tokio_bench(
                store,
                spec.workload,
                dist,
                spec.num_keys,
                spec.threads,
                spec.run_duration,
                spec.warmup_duration,
                spec.refresh_interval,
                spec.latency_sample_rate,
            )
        }
    };

    let _ = std::fs::remove_dir_all(&bench_dir);
    results
}

// ── Output formatting ────────────────────────────────────────────────

fn print_table(reports: &[BenchmarkReport]) {
    println!();
    println!(
        "╔═══════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════╗"
    );
    println!(
        "║                                          FASTER Disk I/O Benchmark Results                                                              ║"
    );
    println!(
        "╠════════╤════════╤═══════════╤═════╤═══════════════╤══════════╤══════════╤══════════╤═══════════╤══════════╤═══════════════╤═══════════════╣"
    );
    println!(
        "║ Device │ Client │ Workload  │ Thr │ Ops/sec (mean)│ +Stddev  │ P50 (ns) │ P99 (ns) │ P99.9(ns) │ Pending% │ 95% CI        │ I/O Class     ║"
    );
    println!(
        "╠════════╪════════╪═══════════╪═════╪═══════════════╪══════════╪══════════╪══════════╪═══════════╪══════════╪═══════════════╪═══════════════╣"
    );
    for r in reports {
        let variance_flag = if r.results.high_variance { " !" } else { "" };
        let ci_str = format!(
            "[{},{}]",
            format_ops(r.results.ops_per_sec_ci95_lo),
            format_ops(r.results.ops_per_sec_ci95_hi),
        );
        println!(
            "║ {:6} │ {:6} │ {:9} │ {:>3} │ {:>13} │ {:>8} │ {:>8.0} │ {:>8.0} │ {:>9.0} │ {:>7.2}% │ {:>13} │ {:>13} ║{}",
            r.config.device,
            r.config.client,
            r.config.workload,
            r.config.threads,
            format_ops(r.results.ops_per_sec),
            format_ops(r.results.ops_per_sec_stddev),
            r.results.p50_ns,
            r.results.p99_ns,
            r.results.p999_ns,
            r.results.pending_rate * 100.0,
            ci_str,
            r.results.io_classification,
            variance_flag,
        );
    }
    println!(
        "╚════════╧════════╧═══════════╧═════╧═══════════════╧══════════╧══════════╧══════════╧═══════════╧══════════╧═══════════════╧═══════════════╝"
    );
    println!();
    println!("  Legend: ! = HIGH VARIANCE (stddev > 10% of mean)");
    println!("  I/O Class: DISK-BOUND (pending>=5%) | MIXED (0.1-5%) | IN-MEMORY BASELINE (<0.1%)");
    println!(
        "  Memory budget: buffer_pages x 32 KiB = in-memory window. Reduce to force disk I/O."
    );
}

fn print_json(reports: &[BenchmarkReport]) {
    println!("{}", serde_json::to_string_pretty(reports).unwrap());
}

fn print_csv(reports: &[BenchmarkReport]) {
    println!(
        "device,client,workload,threads,num_keys,value_size,buffer_pages,iterations,\
         ops_per_sec,ops_per_sec_stddev,ops_per_sec_ci95_lo,ops_per_sec_ci95_hi,\
         throughput_mb_s,p50_ns,p99_ns,p999_ns,pending_rate,\
         total_ops,total_reads,total_writes,total_pending,elapsed_secs,\
         high_variance,io_classification"
    );
    for r in reports {
        println!(
            "{},{},{},{},{},{},{},{},\
             {:.0},{:.0},{:.0},{:.0},\
             {:.1},{:.0},{:.0},{:.0},{:.4},\
             {},{},{},{},{:.3},\
             {},{}",
            r.config.device,
            r.config.client,
            r.config.workload,
            r.config.threads,
            r.config.num_keys,
            r.config.value_size,
            r.config.buffer_pages,
            r.config.iterations,
            r.results.ops_per_sec,
            r.results.ops_per_sec_stddev,
            r.results.ops_per_sec_ci95_lo,
            r.results.ops_per_sec_ci95_hi,
            r.results.throughput_mb_s,
            r.results.p50_ns,
            r.results.p99_ns,
            r.results.p999_ns,
            r.results.pending_rate,
            r.results.total_ops,
            r.results.total_reads,
            r.results.total_writes,
            r.results.total_pending,
            r.results.elapsed_secs,
            r.results.high_variance,
            r.results.io_classification,
        );
    }
}

fn format_ops(ops: f64) -> String {
    if ops >= 1_000_000_000.0 {
        format!("{:.2}B", ops / 1_000_000_000.0)
    } else if ops >= 1_000_000.0 {
        format!("{:.2}M", ops / 1_000_000.0)
    } else if ops >= 1_000.0 {
        format!("{:.1}K", ops / 1_000.0)
    } else {
        format!("{:.0}", ops)
    }
}

// ── Benchmark orchestration ──────────────────────────────────────────

#[derive(Clone)]
struct BenchSpec {
    device: DeviceType,
    client: ClientType,
    workload: WorkloadSpec,
    threads: usize,
    num_keys: u64,
    value_size: usize,
    buffer_pages: usize,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    latency_sample_rate: u64,
    data_dir: PathBuf,
    io_threads: usize,
    queue_depth: u32,
    direct_io: bool,
    iterations: usize,
    force_disk: bool,
}

fn run_single_bench(spec: &BenchSpec) -> BenchmarkReport {
    let mut effective_buffer_pages = spec.buffer_pages;

    // ── Force-disk auto-tuning ──────────────────────────────────────
    if spec.force_disk {
        eprintln!(
            "  [force-disk] Starting auto-tune with buffer_pages={effective_buffer_pages}..."
        );
        loop {
            let probe_spec = BenchSpec {
                buffer_pages: effective_buffer_pages,
                run_duration: Duration::from_secs(5),
                warmup_duration: Duration::from_secs(3),
                iterations: 1,
                force_disk: false,
                ..spec.clone()
            };
            eprint!("  [force-disk] Probing buffer_pages={effective_buffer_pages}... ");
            let probe = run_single_iteration(&probe_spec);
            let pr = probe.pending_rate * 100.0;
            eprintln!("pending_rate={pr:.2}%");

            if probe.pending_rate > 0.001 {
                eprintln!(
                    "  [force-disk] Achieved disk-bound behavior at \
                     buffer_pages={effective_buffer_pages} (pending={pr:.2}%)"
                );
                break;
            }
            if effective_buffer_pages <= 1 {
                eprintln!(
                    "  [force-disk] Cannot achieve disk I/O even at buffer_pages=1. \
                     The dataset may fit in OS page cache. Try increasing --num-keys."
                );
                break;
            }
            effective_buffer_pages = (effective_buffer_pages / 2).max(1);
        }
    }

    // ── Build effective spec with possibly tuned buffer_pages ────────
    let run_spec = BenchSpec {
        buffer_pages: effective_buffer_pages,
        force_disk: false,
        ..spec.clone()
    };

    let bench_config = BenchmarkConfig {
        device: spec.device.name().to_string(),
        client: spec.client.name().to_string(),
        workload: spec.workload.name().to_string(),
        threads: spec.threads,
        num_keys: spec.num_keys,
        value_size: spec.value_size,
        buffer_pages: effective_buffer_pages,
        duration_secs: spec.run_duration.as_secs(),
        iterations: spec.iterations,
    };

    // ── Run multiple iterations ─────────────────────────────────────
    let iterations = spec.iterations.max(1);
    let mut all_runs: Vec<SingleRunResults> = Vec::with_capacity(iterations);

    for i in 0..iterations {
        if iterations > 1 {
            eprint!("  Iteration {}/{}... ", i + 1, iterations);
        } else {
            eprint!("  Running... ");
        }
        let run = run_single_iteration(&run_spec);
        eprintln!(
            "{:.2}M ops/sec | pending={:.2}%",
            run.ops_per_sec / 1_000_000.0,
            run.pending_rate * 100.0,
        );
        all_runs.push(run);
    }

    // ── Aggregate across iterations ─────────────────────────────────
    let ops_values: Vec<f64> = all_runs.iter().map(|r| r.ops_per_sec).collect();
    let summary = IterationSummary::from_values(&ops_values);

    // Use the median run for latency percentiles
    let mut sorted_by_ops = all_runs.clone();
    sorted_by_ops.sort_by(|a, b| a.ops_per_sec.partial_cmp(&b.ops_per_sec).unwrap());
    let median_run = &sorted_by_ops[sorted_by_ops.len() / 2];

    let avg_pending_rate =
        all_runs.iter().map(|r| r.pending_rate).sum::<f64>() / all_runs.len() as f64;

    let io_class = classify_io(avg_pending_rate);

    // ── Pending-rate validation gate ────────────────────────────────
    if avg_pending_rate < 0.001 {
        eprintln!();
        eprintln!("  WARNING: All operations completing synchronously -- this is an in-memory");
        eprintln!("    benchmark, not disk-bound. Increase --num-keys or reduce --buffer-pages");
        eprintln!("    to force disk I/O. Use --force-disk for automatic tuning.");
        eprintln!("    Results tagged as: {io_class}");
        eprintln!();
    }

    if summary.high_variance {
        eprintln!(
            "  HIGH VARIANCE: stddev={:.2}M ({:.1}% of mean) across {} iterations",
            summary.stddev / 1_000_000.0,
            if summary.mean > 0.0 {
                summary.stddev / summary.mean * 100.0
            } else {
                0.0
            },
            iterations,
        );
    }

    let total_ops: u64 = all_runs.iter().map(|r| r.total_ops).sum();
    let total_reads: u64 = all_runs.iter().map(|r| r.total_reads).sum();
    let total_writes: u64 = all_runs.iter().map(|r| r.total_writes).sum();
    let total_pending: u64 = all_runs.iter().map(|r| r.total_pending).sum();
    let total_elapsed: f64 = all_runs.iter().map(|r| r.elapsed_secs).sum();

    let record_size = (8 + spec.value_size) as f64;
    let mean_throughput = (summary.mean * record_size) / (1024.0 * 1024.0);

    BenchmarkReport {
        config: bench_config,
        results: BenchmarkResults {
            ops_per_sec: summary.mean,
            throughput_mb_s: mean_throughput,
            p50_ns: median_run.p50_ns,
            p99_ns: median_run.p99_ns,
            p999_ns: median_run.p999_ns,
            pending_rate: avg_pending_rate,
            total_ops,
            total_reads,
            total_writes,
            total_pending,
            elapsed_secs: total_elapsed,
            ops_per_sec_stddev: summary.stddev,
            ops_per_sec_ci95_lo: summary.ci95_lo,
            ops_per_sec_ci95_hi: summary.ci95_hi,
            high_variance: summary.high_variance,
            io_classification: io_class.to_string(),
        },
    }
}

// ── System info ──────────────────────────────────────────────────────

fn print_system_info() {
    println!("-- System -----");
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(line) = cpuinfo.lines().find(|l| l.starts_with("model name")) {
            if let Some(name) = line.split(':').nth(1) {
                println!("  CPU:     {}", name.trim());
            }
        }
    }
    if let Ok(n) = std::thread::available_parallelism() {
        println!("  Cores:   {}", n.get());
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(line) = meminfo.lines().find(|l| l.starts_with("MemTotal")) {
            println!("  Memory:  {}", line.split(':').nth(1).unwrap_or("").trim());
        }
    }
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        println!("  Kernel:  {}", release.trim());
    }
    println!();
}

// ── Main ─────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    println!();
    println!("=== FASTER -- Disk I/O Device Benchmark ===");
    println!();
    print_system_info();

    let mut all_reports: Vec<BenchmarkReport> = Vec::new();

    if cli.run_all {
        let devices = vec![DeviceType::Sync, DeviceType::Uring, DeviceType::Tokio];
        let clients = vec![ClientType::Sync, ClientType::Tokio];
        let workloads = vec![
            WorkloadSpec::Overcommit,
            WorkloadSpec::WriteHeavy,
            WorkloadSpec::Mixed,
            WorkloadSpec::Scan,
        ];
        let thread_counts = vec![1, 4, 8, 16];

        let total = devices.len() * clients.len() * workloads.len() * thread_counts.len();
        let mut run_num = 0;

        for &device in &devices {
            for &client in &clients {
                for &workload in &workloads {
                    for &threads in &thread_counts {
                        run_num += 1;
                        println!(
                            "-- [{run_num}/{total}] {} + {} | {} | {threads}T --",
                            device.name(),
                            client.name(),
                            workload.name(),
                        );

                        let spec = BenchSpec {
                            device,
                            client,
                            workload,
                            threads,
                            num_keys: cli.num_keys,
                            value_size: cli.value_size,
                            buffer_pages: workload.default_buffer_pages(),
                            run_duration: Duration::from_secs(cli.duration),
                            warmup_duration: Duration::from_secs(cli.warmup),
                            refresh_interval: cli.refresh_interval,
                            latency_sample_rate: cli.latency_sample_rate,
                            data_dir: cli.data_dir.clone(),
                            io_threads: cli.io_threads,
                            queue_depth: cli.queue_depth,
                            direct_io: cli.direct_io,
                            iterations: cli.iterations,
                            force_disk: cli.force_disk,
                        };

                        let report = run_single_bench(&spec);
                        println!(
                            "  -> {:.2}M ops/sec (+/-{:.2}M) | P99={:.0}ns | \
                             pending={:.1}% | {}{}",
                            report.results.ops_per_sec / 1_000_000.0,
                            report.results.ops_per_sec_stddev / 1_000_000.0,
                            report.results.p99_ns,
                            report.results.pending_rate * 100.0,
                            report.results.io_classification,
                            if report.results.high_variance {
                                " HIGH VARIANCE"
                            } else {
                                ""
                            },
                        );
                        println!();
                        all_reports.push(report);
                    }
                }
            }
        }
    } else {
        let device = match cli.device.to_lowercase().as_str() {
            "sync" => DeviceType::Sync,
            "uring" | "io_uring" | "io-uring" => DeviceType::Uring,
            "tokio" => DeviceType::Tokio,
            other => {
                eprintln!("Unknown device type: {other}. Options: sync, uring, tokio");
                std::process::exit(1);
            }
        };

        let client = match cli.client.to_lowercase().as_str() {
            "sync" => ClientType::Sync,
            "tokio" | "async" => ClientType::Tokio,
            other => {
                eprintln!("Unknown client type: {other}. Options: sync, tokio");
                std::process::exit(1);
            }
        };

        let workload = match cli.workload.to_lowercase().as_str() {
            "overcommit" | "cold" => WorkloadSpec::Overcommit,
            "write-heavy" | "write" | "writeheavy" => WorkloadSpec::WriteHeavy,
            "mixed" => WorkloadSpec::Mixed,
            "scan" | "sequential" => WorkloadSpec::Scan,
            other => {
                eprintln!(
                    "Unknown workload: {other}. Options: overcommit, write-heavy, mixed, scan"
                );
                std::process::exit(1);
            }
        };

        let dataset_bytes = cli.num_keys as f64 * (8.0 + cli.value_size as f64);
        let buffer_bytes = cli.buffer_pages as f64 * 32.0 * 1024.0;

        println!("  Device:      {}", device.name());
        println!("  Client:      {}", client.name());
        println!(
            "  Workload:    {} -- {}",
            workload.name(),
            workload.description()
        );
        println!("  Threads:     {}", cli.threads);
        println!("  Keys:        {}", cli.num_keys);
        println!("  Value size:  {} bytes", cli.value_size);
        println!(
            "  Buffer:      {} pages ({} KiB in-memory)",
            cli.buffer_pages,
            cli.buffer_pages * 32
        );
        println!("  Duration:    {}s (warmup: {}s)", cli.duration, cli.warmup);
        println!("  Iterations:  {}", cli.iterations);
        println!("  Force disk:  {}", cli.force_disk);
        println!("  Data dir:    {}", cli.data_dir.display());
        println!(
            "  Dataset:     {:.1} GiB | Buffer: {:.1} MiB | Ratio: {:.0}:1",
            dataset_bytes / (1024.0 * 1024.0 * 1024.0),
            buffer_bytes / (1024.0 * 1024.0),
            if buffer_bytes > 0.0 {
                dataset_bytes / buffer_bytes
            } else {
                0.0
            },
        );
        println!();

        let spec = BenchSpec {
            device,
            client,
            workload,
            threads: cli.threads,
            num_keys: cli.num_keys,
            value_size: cli.value_size,
            buffer_pages: cli.buffer_pages,
            run_duration: Duration::from_secs(cli.duration),
            warmup_duration: Duration::from_secs(cli.warmup),
            refresh_interval: cli.refresh_interval,
            latency_sample_rate: cli.latency_sample_rate,
            data_dir: cli.data_dir.clone(),
            io_threads: cli.io_threads,
            queue_depth: cli.queue_depth,
            direct_io: cli.direct_io,
            iterations: cli.iterations,
            force_disk: cli.force_disk,
        };

        let report = run_single_bench(&spec);
        all_reports.push(report);
    }

    // Output results
    match cli.output.to_lowercase().as_str() {
        "table" => print_table(&all_reports),
        "json" => print_json(&all_reports),
        "csv" => print_csv(&all_reports),
        other => {
            eprintln!("Unknown output format: {other}. Options: table, json, csv");
            print_table(&all_reports);
        }
    }

    // Write to file if requested
    if let Some(ref path) = cli.output_file {
        let content = match cli.output.to_lowercase().as_str() {
            "json" => serde_json::to_string_pretty(&all_reports).unwrap(),
            "csv" => {
                let mut s = String::from(
                    "device,client,workload,threads,num_keys,value_size,buffer_pages,\
                     iterations,ops_per_sec,ops_per_sec_stddev,ops_per_sec_ci95_lo,\
                     ops_per_sec_ci95_hi,throughput_mb_s,p50_ns,p99_ns,p999_ns,\
                     pending_rate,total_ops,total_reads,total_writes,total_pending,\
                     elapsed_secs,high_variance,io_classification\n",
                );
                for r in &all_reports {
                    s.push_str(&format!(
                        "{},{},{},{},{},{},{},{},\
                         {:.0},{:.0},{:.0},{:.0},\
                         {:.1},{:.0},{:.0},{:.0},{:.4},\
                         {},{},{},{},{:.3},\
                         {},{}\n",
                        r.config.device,
                        r.config.client,
                        r.config.workload,
                        r.config.threads,
                        r.config.num_keys,
                        r.config.value_size,
                        r.config.buffer_pages,
                        r.config.iterations,
                        r.results.ops_per_sec,
                        r.results.ops_per_sec_stddev,
                        r.results.ops_per_sec_ci95_lo,
                        r.results.ops_per_sec_ci95_hi,
                        r.results.throughput_mb_s,
                        r.results.p50_ns,
                        r.results.p99_ns,
                        r.results.p999_ns,
                        r.results.pending_rate,
                        r.results.total_ops,
                        r.results.total_reads,
                        r.results.total_writes,
                        r.results.total_pending,
                        r.results.elapsed_secs,
                        r.results.high_variance,
                        r.results.io_classification,
                    ));
                }
                s
            }
            _ => serde_json::to_string_pretty(&all_reports).unwrap(),
        };
        std::fs::write(path, content).expect("failed to write output file");
        println!("\nResults written to: {}", path.display());
    }
}
