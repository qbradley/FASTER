//! io_uring stress test and comparison benchmark for FASTER.
//!
//! Exercises [`UringDevice`] under high-concurrency, variable-size workloads
//! and optionally compares it against [`SyncFileDevice`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io, thread};

use clap::Parser;
use faster_core::SyncFileDevice;
use faster_core::device::{Device, IoRequestResult, IoStatus};
use faster_uring::{BatchPolicy, UringConfig, UringDevice, UringDeviceConfig};
use rand::Rng;

// ── Constants ───────────────────────────────────────────────────────────

const SECTOR: u32 = 4096;
const SEGMENT: u64 = 256 * 1024 * 1024; // 256 MiB segments

// ── CLI ─────────────────────────────────────────────────────────────────

/// io_uring stress test and comparison benchmark for FASTER.
///
/// Exercises UringDevice under high-concurrency, variable-size workloads
/// and optionally compares against SyncFileDevice.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "uring-stress",
    about = "Battle-test the FASTER io_uring device"
)]
struct Args {
    /// Number of worker threads.
    #[arg(long, default_value_t = 4)]
    threads: usize,

    /// Total number of operations (writes + reads).
    #[arg(long, default_value_t = 10_000)]
    ops: usize,

    /// Value size in bytes (must be a multiple of 4096).
    #[arg(long, default_value_t = 4096)]
    value_size: usize,

    /// Use variable value sizes (64B→4KB range, rounded to sector).
    #[arg(long)]
    variable_sizes: bool,

    /// Percentage of operations that are reads (0-100).
    /// Applied during the mixed phase only.
    #[arg(long, default_value_t = 0)]
    read_pct: u64,

    /// Run comparison mode: UringDevice vs SyncFileDevice.
    #[arg(long)]
    compare: bool,

    /// Run recovery test: write, drop, reopen, verify.
    #[arg(long)]
    recovery: bool,

    /// Storage directory for test data.
    #[arg(long, default_value = "/tmp/uring-stress-data")]
    dir: PathBuf,

    /// io_uring queue depth.
    #[arg(long, default_value_t = 256)]
    queue_depth: u32,
}

// ── Callback infrastructure ─────────────────────────────────────────────

struct CallbackState {
    completed: AtomicBool,
    status: Mutex<Option<IoStatus>>,
    bytes: AtomicU64,
}

impl CallbackState {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            status: Mutex::new(None),
            bytes: AtomicU64::new(0),
        }
    }

    fn wait(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while !self.completed.load(Ordering::Acquire) {
            if start.elapsed() > timeout {
                return false;
            }
            thread::yield_now();
        }
        true
    }

    fn is_success(&self) -> bool {
        let status = self.status.lock().unwrap();
        matches!(*status, Some(IoStatus::Success))
    }
}

/// # Safety
///
/// `context` must point to a valid `Arc<CallbackState>` created by `Arc::into_raw`.
unsafe fn io_callback(context: *mut u8, status: IoStatus, bytes: u32) {
    // SAFETY: Caller guarantees `context` is a valid Arc<CallbackState> pointer.
    let state = unsafe { Arc::from_raw(context as *const CallbackState) };
    *state.status.lock().unwrap() = Some(status);
    state.bytes.store(bytes as u64, Ordering::Relaxed);
    state.completed.store(true, Ordering::Release);
}

fn make_callback() -> (Arc<CallbackState>, *mut u8) {
    let state = Arc::new(CallbackState::new());
    let ptr = Arc::into_raw(Arc::clone(&state)) as *mut u8;
    (state, ptr)
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Round up to sector alignment.
fn align_up(val: usize) -> usize {
    let sector = SECTOR as usize;
    (val + sector - 1) & !(sector - 1)
}

/// Generate a value buffer for a given key index and size.
fn make_value(key: usize, size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    let tag = (key as u64).to_le_bytes();
    let copy_len = tag.len().min(size);
    buf[..copy_len].copy_from_slice(&tag[..copy_len]);
    if size > 8 {
        for (i, byte) in buf.iter_mut().enumerate().skip(8) {
            *byte = ((key.wrapping_mul(7).wrapping_add(i)) % 256) as u8;
        }
    }
    buf
}

/// Verify a value buffer matches expected content for a key.
fn verify_value(key: usize, buf: &[u8], size: usize) -> bool {
    let expected = make_value(key, size);
    buf == expected.as_slice()
}

/// Pick value size (potentially variable).
fn pick_value_size(args: &Args, rng: &mut impl Rng) -> usize {
    if args.variable_sizes {
        // Random from {4096, 8192, 16384, 32768, 65536}
        let sizes = [
            SECTOR as usize,
            SECTOR as usize * 2,
            SECTOR as usize * 4,
            SECTOR as usize * 8,
            SECTOR as usize * 16,
        ];
        sizes[rng.random_range(0..sizes.len())]
    } else {
        align_up(args.value_size).max(SECTOR as usize)
    }
}

// ── Latency tracking ────────────────────────────────────────────────────

struct LatencyTracker {
    samples: Vec<Duration>,
}

impl LatencyTracker {
    fn percentile(&mut self, pct: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        self.samples.sort();
        let idx = ((pct / 100.0) * (self.samples.len() - 1) as f64) as usize;
        self.samples[idx.min(self.samples.len() - 1)]
    }
}

// ── Stress test ─────────────────────────────────────────────────────────

struct StressResult {
    write_ops: usize,
    write_elapsed: Duration,
    read_ops: usize,
    read_elapsed: Duration,
    total_bytes_written: u64,
    total_bytes_read: u64,
    write_latencies: Vec<Duration>,
    read_latencies: Vec<Duration>,
    errors: u64,
}

impl StressResult {
    fn print(&self) {
        println!(
            "  {} in {:.3}s",
            format_ops(self.write_ops, "writes"),
            self.write_elapsed.as_secs_f64()
        );
        println!(
            "  Write IOPS:    {:.0}",
            self.write_ops as f64 / self.write_elapsed.as_secs_f64()
        );
        println!(
            "  Write MB/s:    {:.1}",
            self.total_bytes_written as f64 / self.write_elapsed.as_secs_f64() / 1_048_576.0
        );

        let mut wl = LatencyTracker {
            samples: self.write_latencies.clone(),
        };
        println!(
            "  Write latency  p50: {:.3}ms  p99: {:.3}ms  p999: {:.3}ms",
            wl.percentile(50.0).as_secs_f64() * 1000.0,
            wl.percentile(99.0).as_secs_f64() * 1000.0,
            wl.percentile(99.9).as_secs_f64() * 1000.0
        );

        if self.read_ops > 0 {
            println!();
            println!(
                "  {} in {:.3}s",
                format_ops(self.read_ops, "reads"),
                self.read_elapsed.as_secs_f64()
            );
            println!(
                "  Read IOPS:     {:.0}",
                self.read_ops as f64 / self.read_elapsed.as_secs_f64()
            );
            println!(
                "  Read MB/s:     {:.1}",
                self.total_bytes_read as f64 / self.read_elapsed.as_secs_f64() / 1_048_576.0
            );

            let mut rl = LatencyTracker {
                samples: self.read_latencies.clone(),
            };
            println!(
                "  Read latency   p50: {:.3}ms  p99: {:.3}ms  p999: {:.3}ms",
                rl.percentile(50.0).as_secs_f64() * 1000.0,
                rl.percentile(99.0).as_secs_f64() * 1000.0,
                rl.percentile(99.9).as_secs_f64() * 1000.0
            );
        }

        if self.errors > 0 {
            println!("  ⚠ Errors: {}", self.errors);
        }
    }
}

fn format_ops(count: usize, label: &str) -> String {
    if count >= 1_000_000 {
        format!("{:.2}M {label}", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K {label}", count as f64 / 1_000.0)
    } else {
        format!("{count} {label}")
    }
}

fn run_stress_on_device(dev: Arc<dyn Device>, args: &Args) -> StressResult {
    let num_threads = args.threads;
    let total_ops = args.ops;
    let ops_per_thread = total_ops / num_threads;
    let timeout = Duration::from_secs(60);

    // ── Phase 1: Writes ─────────────────────────────────────
    let write_start = Instant::now();
    let total_bytes_written = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let all_write_latencies: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    // Track value sizes per key for verification
    let value_sizes: Arc<Mutex<Vec<(usize, usize)>>> =
        Arc::new(Mutex::new(Vec::with_capacity(total_ops)));

    let write_handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let dev = Arc::clone(&dev);
            let args = args.clone();
            let total_bytes = Arc::clone(&total_bytes_written);
            let errors = Arc::clone(&error_count);
            let latencies = Arc::clone(&all_write_latencies);
            let sizes = Arc::clone(&value_sizes);

            thread::spawn(move || {
                let mut rng = rand::rng();
                let mut local_latencies = Vec::with_capacity(ops_per_thread);
                let mut local_sizes = Vec::with_capacity(ops_per_thread);

                for i in 0..ops_per_thread {
                    let key = t * ops_per_thread + i;
                    let vsize = pick_value_size(&args, &mut rng);
                    let value = make_value(key, vsize);
                    let offset = key as u64 * SEGMENT; // Unique offset per key

                    local_sizes.push((key, vsize));

                    let (state, ctx) = make_callback();
                    let start = Instant::now();

                    // SAFETY: value is valid for vsize bytes until callback fires.
                    unsafe {
                        let r =
                            dev.write_async(value.as_ptr(), offset, vsize as u32, io_callback, ctx);
                        if !matches!(r, IoRequestResult::Submitted) {
                            errors.fetch_add(1, Ordering::Relaxed);
                            // Reclaim the Arc
                            drop(Arc::from_raw(ctx as *const CallbackState));
                            continue;
                        }
                    }

                    if !state.wait(timeout) || !state.is_success() {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    local_latencies.push(start.elapsed());
                    total_bytes.fetch_add(vsize as u64, Ordering::Relaxed);
                }

                latencies.lock().unwrap().extend(local_latencies);
                sizes.lock().unwrap().extend(local_sizes);
            })
        })
        .collect();

    for h in write_handles {
        h.join().unwrap();
    }
    let write_elapsed = write_start.elapsed();
    let total_written = total_bytes_written.load(Ordering::Relaxed);

    // ── Phase 2: Verification reads ─────────────────────────
    let read_start = Instant::now();
    let total_bytes_read = Arc::new(AtomicU64::new(0));
    let all_read_latencies: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let verified = Arc::new(AtomicU64::new(0));

    let sizes_snapshot = value_sizes.lock().unwrap().clone();
    let sizes_arc = Arc::new(sizes_snapshot);

    // Split verification across threads
    let chunk_size = sizes_arc.len().div_ceil(num_threads);
    let read_handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let dev = Arc::clone(&dev);
            let total_bytes = Arc::clone(&total_bytes_read);
            let errors = Arc::clone(&error_count);
            let latencies = Arc::clone(&all_read_latencies);
            let verified = Arc::clone(&verified);
            let sizes = Arc::clone(&sizes_arc);

            thread::spawn(move || {
                let start_idx = t * chunk_size;
                let end_idx = (start_idx + chunk_size).min(sizes.len());
                let mut local_latencies = Vec::with_capacity(end_idx - start_idx);

                for &(key, vsize) in &sizes[start_idx..end_idx] {
                    let offset = key as u64 * SEGMENT;
                    let mut rbuf = vec![0u8; vsize];

                    let (state, ctx) = make_callback();
                    let start = Instant::now();

                    // SAFETY: rbuf valid for vsize bytes until callback fires.
                    unsafe {
                        let r = dev.read_async(
                            offset,
                            rbuf.as_mut_ptr(),
                            vsize as u32,
                            io_callback,
                            ctx,
                        );
                        if !matches!(
                            r,
                            IoRequestResult::Submitted | IoRequestResult::CompletedSync
                        ) {
                            errors.fetch_add(1, Ordering::Relaxed);
                            // Reclaim the Arc
                            drop(Arc::from_raw(ctx as *const CallbackState));
                            continue;
                        }
                    }

                    if !state.wait(timeout) || !state.is_success() {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    local_latencies.push(start.elapsed());
                    total_bytes.fetch_add(vsize as u64, Ordering::Relaxed);

                    if verify_value(key, &rbuf, vsize) {
                        verified.fetch_add(1, Ordering::Relaxed);
                    } else {
                        eprintln!("  ✗ Verification failed for key {key}");
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }

                latencies.lock().unwrap().extend(local_latencies);
            })
        })
        .collect();

    for h in read_handles {
        h.join().unwrap();
    }
    let read_elapsed = read_start.elapsed();

    let verified_count = verified.load(Ordering::Relaxed);
    let expected_count = sizes_arc.len() as u64;
    if verified_count == expected_count {
        println!("  All {} values verified correct ✅", expected_count);
    } else {
        println!("  ⚠ Verified {}/{} values", verified_count, expected_count);
    }

    StressResult {
        write_ops: total_ops,
        write_elapsed,
        read_ops: sizes_arc.len(),
        read_elapsed,
        total_bytes_written: total_written,
        total_bytes_read: total_bytes_read.load(Ordering::Relaxed),
        write_latencies: all_write_latencies.lock().unwrap().clone(),
        read_latencies: all_read_latencies.lock().unwrap().clone(),
        errors: error_count.load(Ordering::Relaxed),
    }
}

// ── Device factories ────────────────────────────────────────────────────

fn make_uring_device(dir: &std::path::Path, queue_depth: u32) -> io::Result<UringDevice> {
    UringDevice::new(UringDeviceConfig {
        base_path: dir.to_path_buf(),
        prefix: "uring.".to_string(),
        sector_size: SECTOR,
        segment_size: SEGMENT,
        ring_config: UringConfig {
            queue_depth,
            sq_poll: false,
            direct_io: false,
        },
        batch_policy: BatchPolicy::default(),
    })
}

fn make_sync_device(dir: &std::path::Path) -> io::Result<SyncFileDevice> {
    SyncFileDevice::new(dir, "sync.", SECTOR, SEGMENT, 4)
}

// ── Modes ───────────────────────────────────────────────────────────────

fn run_stress(args: &Args) {
    println!("═══ FASTER io_uring Stress Test ═══");
    println!(
        "Config: threads={}, ops={}, value_size={}, variable={}",
        args.threads, args.ops, args.value_size, args.variable_sizes,
    );
    println!();

    let uring_dir = args.dir.join("uring");
    let _ = std::fs::remove_dir_all(&uring_dir);

    let dev = Arc::new(
        make_uring_device(&uring_dir, args.queue_depth).expect("failed to create UringDevice"),
    );

    println!("▶ Phase 1: Writes + Phase 2: Verification Reads");
    let result = run_stress_on_device(dev, args);
    result.print();
}

fn run_compare(args: &Args) {
    println!("═══ FASTER io_uring vs SyncFileDevice Comparison ═══");
    println!(
        "Config: threads={}, ops={}, value_size={}",
        args.threads, args.ops, args.value_size,
    );
    println!();

    // ── UringDevice ─────────────────────────────────────
    let uring_dir = args.dir.join("compare-uring");
    let _ = std::fs::remove_dir_all(&uring_dir);
    let uring_dev = Arc::new(
        make_uring_device(&uring_dir, args.queue_depth).expect("failed to create UringDevice"),
    );

    println!("▶ UringDevice");
    let uring_result = run_stress_on_device(uring_dev, args);
    uring_result.print();
    println!();

    // ── SyncFileDevice ──────────────────────────────────
    let sync_dir = args.dir.join("compare-sync");
    let _ = std::fs::remove_dir_all(&sync_dir);
    let sync_dev = Arc::new(make_sync_device(&sync_dir).expect("failed to create SyncFileDevice"));

    println!("▶ SyncFileDevice");
    let sync_result = run_stress_on_device(sync_dev, args);
    sync_result.print();
    println!();

    // ── Comparison table ────────────────────────────────
    println!("═══ Comparison Summary ═══");
    println!("{:<22} {:>12} {:>12}", "", "UringDevice", "SyncFileDev");
    println!("{:-<22} {:-<12} {:-<12}", "", "", "");

    let uring_wiops = uring_result.write_ops as f64 / uring_result.write_elapsed.as_secs_f64();
    let sync_wiops = sync_result.write_ops as f64 / sync_result.write_elapsed.as_secs_f64();
    println!(
        "{:<22} {:>12.0} {:>12.0}",
        "Write IOPS", uring_wiops, sync_wiops
    );

    let uring_wmbps = uring_result.total_bytes_written as f64
        / uring_result.write_elapsed.as_secs_f64()
        / 1_048_576.0;
    let sync_wmbps = sync_result.total_bytes_written as f64
        / sync_result.write_elapsed.as_secs_f64()
        / 1_048_576.0;
    println!(
        "{:<22} {:>11.1}M {:>11.1}M",
        "Write MB/s", uring_wmbps, sync_wmbps
    );

    if uring_result.read_ops > 0 && sync_result.read_ops > 0 {
        let uring_riops = uring_result.read_ops as f64 / uring_result.read_elapsed.as_secs_f64();
        let sync_riops = sync_result.read_ops as f64 / sync_result.read_elapsed.as_secs_f64();
        println!(
            "{:<22} {:>12.0} {:>12.0}",
            "Read IOPS", uring_riops, sync_riops
        );

        let uring_rmbps = uring_result.total_bytes_read as f64
            / uring_result.read_elapsed.as_secs_f64()
            / 1_048_576.0;
        let sync_rmbps = sync_result.total_bytes_read as f64
            / sync_result.read_elapsed.as_secs_f64()
            / 1_048_576.0;
        println!(
            "{:<22} {:>11.1}M {:>11.1}M",
            "Read MB/s", uring_rmbps, sync_rmbps
        );
    }

    let uring_errors = uring_result.errors;
    let sync_errors = sync_result.errors;
    println!("{:<22} {:>12} {:>12}", "Errors", uring_errors, sync_errors);

    // Speedup summary
    if sync_wiops > 0.0 {
        let speedup = uring_wiops / sync_wiops;
        println!();
        if speedup > 1.0 {
            println!("→ io_uring is {speedup:.2}x faster for writes");
        } else {
            println!(
                "→ SyncFileDevice is {:.2}x faster for writes",
                1.0 / speedup
            );
        }
    }
}

fn run_recovery(args: &Args) {
    println!("═══ FASTER io_uring Recovery Test ═══");

    let recovery_dir = args.dir.join("recovery");
    let _ = std::fs::remove_dir_all(&recovery_dir);

    let page_size = align_up(args.value_size).max(SECTOR as usize);
    let num_keys = args.ops.min(1000); // Cap for recovery test

    println!(
        "▶ Phase 1: Write {} keys (value_size={})",
        num_keys, page_size
    );

    // Write phase
    {
        let dev = make_uring_device(&recovery_dir, args.queue_depth)
            .expect("failed to create UringDevice");

        for i in 0..num_keys {
            let value = make_value(i, page_size);
            let offset = i as u64 * SEGMENT;
            dev.write_sync(offset, &value).expect("write_sync failed");
        }

        // Explicit close + drop
        dev.close();
    }

    println!("▶ Phase 2: Reopen and verify");

    // Reopen and verify
    {
        let dev = make_uring_device(&recovery_dir, args.queue_depth)
            .expect("failed to reopen UringDevice");

        let mut verified = 0u64;
        let mut failed = 0u64;

        for i in 0..num_keys {
            let offset = i as u64 * SEGMENT;
            let mut rbuf = vec![0u8; page_size];
            dev.read_sync(offset, &mut rbuf).expect("read_sync failed");

            if verify_value(i, &rbuf, page_size) {
                verified += 1;
            } else {
                failed += 1;
                if failed <= 5 {
                    eprintln!("  ✗ Verification failed for key {i}");
                }
            }
        }

        if failed == 0 {
            println!("  All {verified} keys verified after recovery ✅");
        } else {
            println!("  ⚠ {verified}/{} verified, {failed} failed", num_keys);
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────────

pub fn run() {
    let args = Args::parse();

    // Validate value_size alignment
    if !args.variable_sizes && args.value_size % SECTOR as usize != 0 {
        eprintln!(
            "Warning: --value-size {} is not sector-aligned ({}), rounding up to {}",
            args.value_size,
            SECTOR,
            align_up(args.value_size)
        );
    }

    if args.compare {
        run_compare(&args);
    } else if args.recovery {
        run_recovery(&args);
    } else {
        run_stress(&args);
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&args.dir);
    println!("\nDone. Test data cleaned up.");
}
