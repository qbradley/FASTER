//! Cross-implementation YCSB benchmark suite for FASTER.
//!
//! Runs standardized YCSB workloads against the Rust FASTER implementation,
//! producing results comparable to the C# and C++ benchmark runners in the
//! same repository.
//!
//! # Workloads
//!
//! | Code | Mix | Description |
//! |------|-----|-------------|
//! | A | 50/50 read/update | Update-heavy |
//! | B | 95/5 read/update | Read-heavy |
//! | C | 100% read | Read-only |
//! | F | 50/50 read/RMW | Read-modify-write |
//!
//! # Usage
//!
//! ```bash
//! cargo run -p cross-impl-bench --release -- --help
//! cargo run -p cross-impl-bench --release -- --workloads A,B,C,F --threads 1,4,8
//! ```

mod distribution;
mod report;
mod stats;
mod workload;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use faster_core::NullDevice;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::record::Value;
use faster_core::status::OperationStatus;
use faster_core::store::{
    DeleteInfo, FasterKv, FasterKvConfig, Functions, ReadInfo, RmwInPlaceResult, RmwInfo,
    SimpleFunctions, UpsertInfo,
};

use distribution::{Distribution, KeyGenerator};
use report::{BenchmarkResult, Reporter};
use stats::LatencyHistogram;
use workload::{Op, WorkloadSpec};

// ── CLI ──────────────────────────────────────────────────────────────

/// Cross-implementation YCSB benchmark suite for FASTER.
///
/// Runs standardized workloads and reports ops/sec, latency percentiles,
/// and throughput in formats comparable to the C# and C++ benchmarks.
#[derive(Parser, Debug)]
#[command(name = "cross-impl-bench", version)]
struct Cli {
    /// YCSB workloads to run (comma-separated: A,B,C,F or "all")
    #[arg(short, long, default_value = "all")]
    workloads: String,

    /// Thread counts to test (comma-separated: 1,2,4,8,16)
    #[arg(short, long, default_value = "1,4,8,16")]
    threads: String,

    /// Key distribution: uniform or zipfian
    #[arg(short, long, default_value = "uniform")]
    distribution: String,

    /// Zipfian theta parameter (skew factor)
    #[arg(long, default_value = "0.99")]
    zipf_theta: f64,

    /// Value sizes in bytes (comma-separated: 8,100,1024)
    #[arg(short = 's', long, default_value = "8")]
    value_sizes: String,

    /// Number of keys to pre-load
    #[arg(short = 'n', long, default_value = "1000000")]
    num_keys: u64,

    /// Measurement duration in seconds per run
    #[arg(long, default_value = "10")]
    run_secs: u64,

    /// Warm-up duration in seconds before measurement
    #[arg(long, default_value = "3")]
    warmup_secs: u64,

    /// Number of iterations per configuration (best of N)
    #[arg(short, long, default_value = "3")]
    iterations: usize,

    /// Epoch refresh interval (operations between refresh calls)
    #[arg(long, default_value = "64")]
    refresh_interval: u64,

    /// Output CSV file path (in addition to console output)
    #[arg(long)]
    csv: Option<String>,

    /// Use safe (per-op epoch) context instead of UnsafeContext
    #[arg(long, default_value = "false")]
    safe_context: bool,

    /// Latency sampling rate (1 in N operations measured for latency)
    #[arg(long, default_value = "100")]
    latency_sample_rate: u64,
}

// ── Padded value types for different record sizes ────────────────────

#[derive(Clone, Copy)]
#[repr(C)]
struct Value100([u8; 100]);

impl Default for Value100 {
    fn default() -> Self {
        Self([0u8; 100])
    }
}

impl Value for Value100 {
    fn serialized_size(&self) -> usize {
        100
    }
    fn serialize(&self, buf: &mut [u8]) -> usize {
        buf[..100].copy_from_slice(&self.0);
        100
    }
    fn deserialize(buf: &[u8]) -> Self {
        let mut v = [0u8; 100];
        v.copy_from_slice(&buf[..100]);
        Self(v)
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
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

/// Custom Functions for padded value types.
struct PaddedFunctions<V>(std::marker::PhantomData<V>);

impl<V> Default for PaddedFunctions<V> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

// Safety: contains no mutable state
unsafe impl<V: Send> Send for PaddedFunctions<V> {}
unsafe impl<V: Sync> Sync for PaddedFunctions<V> {}

impl<V: Value + Copy + 'static> Functions for PaddedFunctions<V> {
    type Key = u64;
    type Value = V;
    type Input = V;
    type Output = Option<V>;
    type Context = ();

    fn read(&self, _key: &u64, value: &V, _input: &V, output: &mut Option<V>, _info: &ReadInfo) {
        *output = Some(*value);
    }

    fn read_completion(
        &self,
        _key: &u64,
        _output: &Option<V>,
        _ctx: &(),
        _status: OperationStatus,
    ) {
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut V,
        input: &V,
        _old: Option<&V>,
        _output: &mut Option<V>,
        _info: &UpsertInfo,
    ) {
        *value = *input;
    }

    fn rmw_in_place(
        &self,
        _key: &u64,
        input: &V,
        value: &mut V,
        _output: &mut Option<V>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        *value = *input;
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &V,
        _old: &V,
        new_value: &mut V,
        _output: &mut Option<V>,
        _info: &RmwInfo,
    ) {
        *new_value = *input;
    }

    fn rmw_initial(
        &self,
        _key: &u64,
        input: &V,
        value: &mut V,
        _output: &mut Option<V>,
        _info: &RmwInfo,
    ) {
        *value = *input;
    }

    fn delete(&self, _key: &u64, _value: &mut V, _info: &DeleteInfo) {}
}

// ── Store construction ───────────────────────────────────────────────

fn make_config(n: usize, buffer_pages: usize) -> FasterKvConfig {
    let log2 = ((n as f64 * 2.0).log2().ceil() as usize).max(16);
    FasterKvConfig {
        hash_index_size_log2: log2,
        buffer_size_pages: buffer_pages,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: faster_core::grow::GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    }
}

// ── Generic benchmark runner ─────────────────────────────────────────

struct BenchConfig {
    workload: WorkloadSpec,
    dist: Distribution,
    num_keys: u64,
    value_size: usize,
    threads: usize,
    run_duration: Duration,
    warmup_duration: Duration,
    refresh_interval: u64,
    safe_context: bool,
    latency_sample_rate: u64,
}

fn run_bench<F>(store: &Arc<FasterKv<F>>, cfg: &BenchConfig) -> BenchmarkResult
where
    F: Functions<Key = u64, Context = ()>,
    F::Value: Copy + Default,
    F::Input: Copy + Default,
    F::Output: Default,
{
    let workload = cfg.workload;
    let dist = cfg.dist;
    let num_keys = cfg.num_keys;
    let value_size = cfg.value_size;
    let threads = cfg.threads;
    let run_duration = cfg.run_duration;
    let warmup_duration = cfg.warmup_duration;
    let refresh_interval = cfg.refresh_interval;
    let safe_context = cfg.safe_context;
    let latency_sample_rate = cfg.latency_sample_rate;

    let handles: Vec<_> = (0..threads)
        .map(|tid| {
            let store = Arc::clone(store);
            std::thread::spawn(move || {
                let mut keygen = KeyGenerator::new(dist, num_keys, tid as u64);
                let mut rng_seed: u64 = 0xDEAD_BEEF_u64.wrapping_add(tid as u64 * 0x1337);
                let mut histogram = LatencyHistogram::new();
                let mut ops: u64 = 0;
                let mut reads: u64 = 0;
                let mut writes: u64 = 0;
                let mut rmw_count: u64 = 0;
                let mut output = F::Output::default();
                let input = F::Input::default();

                // Warm-up
                let warmup_start = Instant::now();
                if safe_context {
                    let mut session = store.new_session();
                    while warmup_start.elapsed() < warmup_duration {
                        for _ in 0..1000 {
                            let k = keygen.next_key();
                            match workload.next_op(&mut rng_seed) {
                                Op::Read => {
                                    let _ = store.read(&mut session, &k, &input, &mut output, ());
                                }
                                Op::Upsert => {
                                    let _ = store.upsert(&mut session, &k, &input, ());
                                }
                                Op::Rmw => {
                                    let _ = store.rmw(&mut session, &k, &input, &mut output, ());
                                }
                            }
                        }
                    }
                    store.dispose_session(session);
                } else {
                    let mut session = store.new_session();
                    {
                        let mut ctx = store.unsafe_context(&mut session);
                        let mut op_count = 0u64;
                        while warmup_start.elapsed() < warmup_duration {
                            for _ in 0..1000 {
                                let k = keygen.next_key();
                                match workload.next_op(&mut rng_seed) {
                                    Op::Read => {
                                        let _ = ctx.read(&store, &k, &input, &mut output, ());
                                    }
                                    Op::Upsert => {
                                        let _ = ctx.upsert(&store, &k, &input, ());
                                    }
                                    Op::Rmw => {
                                        let _ = ctx.rmw(&store, &k, &input, &mut output, ());
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
                }

                // Measurement
                let measure_start = Instant::now();
                if safe_context {
                    let mut session = store.new_session();
                    while measure_start.elapsed() < run_duration {
                        for _ in 0..1000 {
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
                                    let _ = store.read(&mut session, &k, &input, &mut output, ());
                                    reads += 1;
                                }
                                Op::Upsert => {
                                    let _ = store.upsert(&mut session, &k, &input, ());
                                    writes += 1;
                                }
                                Op::Rmw => {
                                    let _ = store.rmw(&mut session, &k, &input, &mut output, ());
                                    rmw_count += 1;
                                }
                            }
                            if sample {
                                histogram.record(op_start.elapsed());
                            }
                            ops += 1;
                        }
                    }
                    store.dispose_session(session);
                } else {
                    let mut session = store.new_session();
                    {
                        let mut ctx = store.unsafe_context(&mut session);
                        while measure_start.elapsed() < run_duration {
                            for _ in 0..1000 {
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
                                        let _ = ctx.read(&store, &k, &input, &mut output, ());
                                        reads += 1;
                                    }
                                    Op::Upsert => {
                                        let _ = ctx.upsert(&store, &k, &input, ());
                                        writes += 1;
                                    }
                                    Op::Rmw => {
                                        let _ = ctx.rmw(&store, &k, &input, &mut output, ());
                                        rmw_count += 1;
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
                    store.dispose_session(session);
                }

                let elapsed = measure_start.elapsed();
                (ops, reads, writes, rmw_count, elapsed, histogram)
            })
        })
        .collect();

    let mut total_ops: u64 = 0;
    let mut total_reads: u64 = 0;
    let mut total_writes: u64 = 0;
    let mut total_rmws: u64 = 0;
    let mut max_elapsed = Duration::ZERO;
    let mut merged_histogram = LatencyHistogram::new();

    for h in handles {
        let (thread_ops, r, w, rmw, elapsed, histogram) = h.join().expect("thread panicked");
        total_ops += thread_ops;
        total_reads += r;
        total_writes += w;
        total_rmws += rmw;
        if elapsed > max_elapsed {
            max_elapsed = elapsed;
        }
        merged_histogram.merge(&histogram);
    }

    let ops_per_sec = total_ops as f64 / max_elapsed.as_secs_f64();
    let record_size = 8 + value_size as u64;
    let throughput_mb_s = (ops_per_sec * record_size as f64) / (1024.0 * 1024.0);

    BenchmarkResult {
        workload: workload.name().to_string(),
        threads,
        distribution: dist.name().to_string(),
        value_size: value_size as u64,
        total_ops,
        reads: total_reads,
        writes: total_writes,
        rmws: total_rmws,
        elapsed: max_elapsed,
        ops_per_sec,
        throughput_mb_s,
        p50_ns: merged_histogram.percentile(50.0),
        p99_ns: merged_histogram.percentile(99.0),
        p999_ns: merged_histogram.percentile(99.9),
        safe_context,
    }
}

fn populate<F>(store: &FasterKv<F>, n: u64)
where
    F: Functions<Key = u64, Context = ()>,
    F::Value: Copy + Default,
    F::Input: Copy + Default,
    F::Output: Default,
{
    let input = F::Input::default();
    let mut session = store.new_session();
    {
        let mut ctx = store.unsafe_context(&mut session);
        for k in 0..n {
            let _ = ctx.upsert(store, &k, &input, ());
            if k % 64 == 0 {
                ctx.refresh();
            }
        }
    }
    store.dispose_session(session);
}

// ── Main ─────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let workloads: Vec<WorkloadSpec> = if cli.workloads == "all" {
        vec![
            WorkloadSpec::A,
            WorkloadSpec::B,
            WorkloadSpec::C,
            WorkloadSpec::F,
        ]
    } else {
        cli.workloads
            .split(',')
            .map(|w| match w.trim().to_uppercase().as_str() {
                "A" => WorkloadSpec::A,
                "B" => WorkloadSpec::B,
                "C" => WorkloadSpec::C,
                "F" => WorkloadSpec::F,
                other => {
                    eprintln!("Unknown workload: {other}");
                    std::process::exit(1);
                }
            })
            .collect()
    };

    let available_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let thread_counts: Vec<usize> = cli
        .threads
        .split(',')
        .filter_map(|t| t.trim().parse::<usize>().ok())
        .filter(|&t| t <= available_cpus && t > 0)
        .collect();

    let value_sizes: Vec<usize> = cli
        .value_sizes
        .split(',')
        .filter_map(|s| s.trim().parse::<usize>().ok())
        .filter(|&s| s > 0)
        .collect();

    let dist = match cli.distribution.to_lowercase().as_str() {
        "uniform" => Distribution::Uniform,
        "zipfian" | "zipf" => Distribution::Zipfian(cli.zipf_theta),
        other => {
            eprintln!("Unknown distribution: {other}");
            std::process::exit(1);
        }
    };

    let run_duration = Duration::from_secs(cli.run_secs);
    let warmup_duration = Duration::from_secs(cli.warmup_secs);
    let context_mode = if cli.safe_context {
        "SafeContext"
    } else {
        "UnsafeContext"
    };

    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║         FASTER Rust — Cross-Implementation Benchmark           ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
    print_system_info();
    println!("  Keys:          {}", cli.num_keys);
    println!(
        "  Distribution:  {} {}",
        dist.name(),
        if matches!(dist, Distribution::Zipfian(_)) {
            format!("(theta={:.2})", cli.zipf_theta)
        } else {
            String::new()
        }
    );
    println!(
        "  Run duration:  {}s (warmup: {}s)",
        cli.run_secs, cli.warmup_secs
    );
    println!("  Iterations:    {} (best of N)", cli.iterations);
    println!("  Context:       {context_mode}");
    println!("  Refresh:       every {} ops", cli.refresh_interval);
    println!("  Value sizes:   {:?} bytes", value_sizes);
    println!("  Thread counts: {:?}", thread_counts);
    println!(
        "  Workloads:     {:?}",
        workloads.iter().map(|w| w.name()).collect::<Vec<_>>()
    );
    println!();

    let mut all_results: Vec<BenchmarkResult> = Vec::new();
    let total_configs = workloads.len() * thread_counts.len() * value_sizes.len();
    let mut config_num = 0;

    for &value_size in &value_sizes {
        for &workload in &workloads {
            for &threads in &thread_counts {
                config_num += 1;
                println!(
                    "--- [{config_num}/{total_configs}] Workload {}, {threads}T, {value_size}B values, {} ---",
                    workload.name(),
                    dist.name()
                );

                let mut best_result: Option<BenchmarkResult> = None;

                for iter in 1..=cli.iterations {
                    print!("  Iteration {iter}/{}: ", cli.iterations);

                    let result = match value_size {
                        v if v <= 8 => {
                            let store = Arc::new(FasterKv::new(
                                make_config(cli.num_keys as usize, 256),
                                SimpleFunctions::default(),
                                NullDevice::new(),
                            ));
                            print!("populating... ");
                            populate::<SimpleFunctions<u64, u64>>(&store, cli.num_keys);
                            print!("running... ");
                            let bench_cfg = BenchConfig {
                                workload,
                                dist,
                                num_keys: cli.num_keys,
                                value_size: 8,
                                threads,
                                run_duration,
                                warmup_duration,
                                refresh_interval: cli.refresh_interval,
                                safe_context: cli.safe_context,
                                latency_sample_rate: cli.latency_sample_rate,
                            };
                            run_bench(&store, &bench_cfg)
                        }
                        100 => {
                            let store = Arc::new(FasterKv::new(
                                make_config(cli.num_keys as usize, 512),
                                PaddedFunctions::<Value100>::default(),
                                NullDevice::new(),
                            ));
                            print!("populating... ");
                            populate::<PaddedFunctions<Value100>>(&store, cli.num_keys);
                            print!("running... ");
                            let bench_cfg = BenchConfig {
                                workload,
                                dist,
                                num_keys: cli.num_keys,
                                value_size: 100,
                                threads,
                                run_duration,
                                warmup_duration,
                                refresh_interval: cli.refresh_interval,
                                safe_context: cli.safe_context,
                                latency_sample_rate: cli.latency_sample_rate,
                            };
                            run_bench(&store, &bench_cfg)
                        }
                        1024 => {
                            let store = Arc::new(FasterKv::new(
                                make_config(cli.num_keys as usize, 1024),
                                PaddedFunctions::<Value1024>::default(),
                                NullDevice::new(),
                            ));
                            print!("populating... ");
                            populate::<PaddedFunctions<Value1024>>(&store, cli.num_keys);
                            print!("running... ");
                            let bench_cfg = BenchConfig {
                                workload,
                                dist,
                                num_keys: cli.num_keys,
                                value_size: 1024,
                                threads,
                                run_duration,
                                warmup_duration,
                                refresh_interval: cli.refresh_interval,
                                safe_context: cli.safe_context,
                                latency_sample_rate: cli.latency_sample_rate,
                            };
                            run_bench(&store, &bench_cfg)
                        }
                        other => {
                            eprintln!("Unsupported value size: {other}. Supported: 8, 100, 1024");
                            std::process::exit(1);
                        }
                    };

                    println!(
                        "{:.2}M ops/sec (P50={:.0}ns P99={:.0}ns P99.9={:.0}ns)",
                        result.ops_per_sec / 1_000_000.0,
                        result.p50_ns,
                        result.p99_ns,
                        result.p999_ns
                    );

                    if best_result
                        .as_ref()
                        .is_none_or(|b| result.ops_per_sec > b.ops_per_sec)
                    {
                        best_result = Some(result);
                    }
                }

                if let Some(best) = best_result {
                    println!(
                        "  >> Best: {:.2}M ops/sec | {:.1} MB/s | P50={:.0}ns P99={:.0}ns P99.9={:.0}ns",
                        best.ops_per_sec / 1_000_000.0,
                        best.throughput_mb_s,
                        best.p50_ns,
                        best.p99_ns,
                        best.p999_ns
                    );
                    all_results.push(best);
                }
                println!();
            }
        }
    }

    Reporter::print_summary_table(&all_results);
    Reporter::print_scaling_table(&all_results);

    if let Some(csv_path) = &cli.csv {
        match Reporter::write_csv(&all_results, csv_path) {
            Ok(()) => println!("\nCSV written to: {csv_path}"),
            Err(e) => eprintln!("\nFailed to write CSV: {e}"),
        }
    }
}

fn print_system_info() {
    println!("-- System ----------------------------------------------------------");
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(line) = cpuinfo.lines().find(|l| l.starts_with("model name")) {
            if let Some(name) = line.split(':').nth(1) {
                println!("  CPU:           {}", name.trim());
            }
        }
    }
    if let Ok(n) = std::thread::available_parallelism() {
        println!("  Cores:         {}", n.get());
    }
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        println!("  Kernel:        {}", release.trim());
    }
    println!(
        "  Rust:          {} (edition 2024)",
        env!("CARGO_PKG_RUST_VERSION")
    );
    println!();
}
