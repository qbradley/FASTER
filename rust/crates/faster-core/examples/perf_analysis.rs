//! Targeted performance profiling for AI-4: Epoch Amortization Analysis.
//!
//! This benchmark isolates individual cost components to quantify:
//! 1. Bare epoch protect/unprotect cost
//! 2. Per-op epoch upsert (current behavior)
//! 3. Amortized epoch upsert (UnsafeContext)
//! 4. Hash-only cost (no epoch)
//! 5. Multi-thread epoch contention scaling

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use faster_core::NullDevice;
use faster_core::epoch::EpochTable;
use faster_core::hash::Hashable;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

type Store = FasterKv<SimpleFunctions<u64, u64>>;

const OPS: u64 = 2_000_000;
const WARMUP_OPS: u64 = 100_000;
const ITERS: u32 = 5;
const REFRESH_INTERVAL: u64 = 128;

fn make_store(n: usize) -> Store {
    let log2 = ((n as f64 * 2.0).log2().ceil() as usize).max(10);
    let config = FasterKvConfig {
        hash_index_size_log2: log2,
        buffer_size_pages: 256,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: faster_core::grow::GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
}

fn populate(store: &Store, n: u64) {
    let mut session = store.new_session();
    for k in 0..n {
        let _ = store.upsert(&mut session, &k, &k, ());
    }
    store.dispose_session(session);
}

fn median(times: &mut [f64]) -> f64 {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times[times.len() / 2]
}

// ---------------------------------------------------------------------------
// 1. Bare Epoch protect/unprotect (0 contention)
// ---------------------------------------------------------------------------

fn bench_bare_epoch() {
    println!("\n=== 1. Bare Epoch Protect/Unprotect (0 contention) ===");
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    // Warmup
    for _ in 0..WARMUP_OPS {
        let guard = thread.protect();
        black_box(guard.epoch());
        drop(guard);
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let start = Instant::now();
        for _ in 0..OPS {
            let guard = thread.protect();
            black_box(guard.epoch());
            drop(guard);
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
    println!(
        "  All runs: {:?}",
        times
            .iter()
            .map(|t| format!("{:.1}", t))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 2. Epoch with try_drain active (epoch bumping in background)
// ---------------------------------------------------------------------------

fn bench_epoch_with_drain() {
    println!("\n=== 2. Epoch Protect/Unprotect (with drain list activity) ===");
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    // Push some drain callbacks to make try_drain do work
    for i in 0..100u64 {
        table.bump_current_epoch(move || {
            black_box(i);
        });
    }

    // Warmup
    for _ in 0..WARMUP_OPS {
        let guard = thread.protect();
        black_box(guard.epoch());
        drop(guard);
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        // Re-populate drain list
        for i in 0..10u64 {
            table.bump_current_epoch(move || {
                black_box(i);
            });
        }

        let start = Instant::now();
        for _ in 0..OPS {
            let guard = thread.protect();
            black_box(guard.epoch());
            drop(guard);
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
}

// ---------------------------------------------------------------------------
// 3. Hash computation cost only
// ---------------------------------------------------------------------------

fn bench_hash_only() {
    println!("\n=== 3. Hash Computation Only (u64 key) ===");

    // Warmup
    for k in 0..WARMUP_OPS {
        let kh = k.hash();
        black_box(kh.tag());
        black_box(kh.index(1 << 20));
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let start = Instant::now();
        for k in 0..OPS {
            let kh = k.hash();
            black_box(kh.tag());
            black_box(kh.index(1 << 20));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
}

// ---------------------------------------------------------------------------
// 4. Per-op epoch upsert (current behavior) — sequential inserts
// ---------------------------------------------------------------------------

fn bench_per_op_upsert_insert() {
    println!("\n=== 4a. Per-Op Epoch Upsert — Sequential INSERTS (current) ===");

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let store = make_store(OPS as usize);
        let mut session = store.new_session();

        // Warmup
        for k in 0..WARMUP_OPS {
            let _ = store.upsert(&mut session, &k, &k, ());
        }
        store.dispose_session(session);

        let mut session = store.new_session();
        let offset = WARMUP_OPS;
        let start = Instant::now();
        for k in offset..(offset + OPS) {
            let _ = black_box(store.upsert(&mut session, &k, &k, ()));
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
    println!(
        "  All runs: {:?}",
        times
            .iter()
            .map(|t| format!("{:.1}", t))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 4b. Per-op epoch upsert — in-place UPDATES (key exists)
// ---------------------------------------------------------------------------

fn bench_per_op_upsert_update() {
    println!("\n=== 4b. Per-Op Epoch Upsert — In-Place UPDATES (key exists) ===");

    let store = make_store(OPS as usize);
    populate(&store, OPS);

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let mut session = store.new_session();
        let start = Instant::now();
        for k in 0..OPS {
            let _ = black_box(store.upsert(&mut session, &k, &(k + 1), ()));
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
}

// ---------------------------------------------------------------------------
// 5a. Amortized epoch upsert — INSERTS via UnsafeContext
// ---------------------------------------------------------------------------

fn bench_amortized_upsert_insert() {
    println!("\n=== 5a. Amortized Epoch Upsert — Sequential INSERTS (UnsafeContext) ===");

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let store = make_store(OPS as usize);
        let mut session = store.new_session();

        // Warmup
        {
            let mut ctx = store.unsafe_context(&mut session);
            for k in 0..WARMUP_OPS {
                let _ = ctx.upsert(&store, &k, &k, ());
                if k % REFRESH_INTERVAL == 0 {
                    ctx.refresh();
                }
            }
        }
        store.dispose_session(session);

        let mut session = store.new_session();
        let offset = WARMUP_OPS;
        let start = Instant::now();
        {
            let mut ctx = store.unsafe_context(&mut session);
            for k in offset..(offset + OPS) {
                let _ = black_box(ctx.upsert(&store, &k, &k, ()));
                if k % REFRESH_INTERVAL == 0 {
                    ctx.refresh();
                }
            }
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
    println!(
        "  All runs: {:?}",
        times
            .iter()
            .map(|t| format!("{:.1}", t))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 5b. Amortized epoch upsert — in-place UPDATES via UnsafeContext
// ---------------------------------------------------------------------------

fn bench_amortized_upsert_update() {
    println!("\n=== 5b. Amortized Epoch Upsert — In-Place UPDATES (UnsafeContext) ===");

    let store = make_store(OPS as usize);
    populate(&store, OPS);

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let mut session = store.new_session();
        let start = Instant::now();
        {
            let mut ctx = store.unsafe_context(&mut session);
            for k in 0..OPS {
                let _ = black_box(ctx.upsert(&store, &k, &(k + 1), ()));
                if k % REFRESH_INTERVAL == 0 {
                    ctx.refresh();
                }
            }
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
}

// ---------------------------------------------------------------------------
// 6. Per-op vs Amortized READ (point read, key exists)
// ---------------------------------------------------------------------------

fn bench_reads() {
    println!("\n=== 6a. Per-Op Epoch Read — Point Read (key exists) ===");

    let store = make_store(OPS as usize);
    populate(&store, OPS);

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let mut session = store.new_session();
        let mut output: Option<u64> = None;
        let start = Instant::now();
        for k in 0..OPS {
            let _ = black_box(store.read(&mut session, &k, &0u64, &mut output, ()));
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);

    println!("\n=== 6b. Amortized Epoch Read — Point Read (UnsafeContext) ===");

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let mut session = store.new_session();
        let mut output: Option<u64> = None;
        let start = Instant::now();
        {
            let mut ctx = store.unsafe_context(&mut session);
            for k in 0..OPS {
                let _ = black_box(ctx.read(&store, &k, &0u64, &mut output, ()));
                if k % REFRESH_INTERVAL == 0 {
                    ctx.refresh();
                }
            }
        }
        let elapsed = start.elapsed();
        store.dispose_session(session);

        let ns_per_op = elapsed.as_nanos() as f64 / OPS as f64;
        times.push(ns_per_op);
    }

    let med = median(&mut times);
    println!("  Median: {:.1} ns/op ({:.1}M ops/sec)", med, 1000.0 / med);
}

// ---------------------------------------------------------------------------
// 7. Multi-thread scaling (per-op epoch)
// ---------------------------------------------------------------------------

fn bench_multithread_scaling() {
    println!("\n=== 7. Multi-Thread Upsert Scaling (Per-Op Epoch) ===");

    let available_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("  Available CPUs: {}", available_cpus);

    let thread_counts: Vec<usize> = [1, 2, 4]
        .iter()
        .copied()
        .filter(|&t| t <= available_cpus)
        .collect();

    let ops_per_thread = OPS;

    for &threads in &thread_counts {
        let total_ops = ops_per_thread * threads as u64;
        let mut times = Vec::new();

        for _ in 0..ITERS {
            let store = Arc::new(make_store((total_ops as usize) * 2));
            let start = Instant::now();

            let handles: Vec<_> = (0..threads)
                .map(|tid| {
                    let store = Arc::clone(&store);
                    let offset = tid as u64 * ops_per_thread;
                    std::thread::spawn(move || {
                        let mut session = store.new_session();
                        for i in 0..ops_per_thread {
                            let k = offset + i;
                            let _ = black_box(store.upsert(&mut session, &k, &k, ()));
                        }
                        store.dispose_session(session);
                    })
                })
                .collect();

            for h in handles {
                h.join().expect("worker panicked");
            }
            let elapsed = start.elapsed();
            let total_ops_sec = total_ops as f64 / elapsed.as_secs_f64();
            let per_thread = total_ops_sec / threads as f64;
            times.push((total_ops_sec, per_thread));
        }

        // Sort by total ops/sec
        times.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (total, per) = times[times.len() / 2];
        println!(
            "  {} thread(s): {:.1}M total ops/sec, {:.1}M/thread, {:.1} ns/op/thread",
            threads,
            total / 1e6,
            per / 1e6,
            1e9 / per
        );
    }
}

// ---------------------------------------------------------------------------
// 8. Multi-thread scaling (amortized epoch via UnsafeContext)
// ---------------------------------------------------------------------------

fn bench_multithread_amortized() {
    println!("\n=== 8. Multi-Thread Upsert Scaling (Amortized Epoch / UnsafeContext) ===");

    let available_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let thread_counts: Vec<usize> = [1, 2, 4]
        .iter()
        .copied()
        .filter(|&t| t <= available_cpus)
        .collect();

    let ops_per_thread = OPS;

    for &threads in &thread_counts {
        let total_ops = ops_per_thread * threads as u64;
        let mut times = Vec::new();

        for _ in 0..ITERS {
            let store = Arc::new(make_store((total_ops as usize) * 2));
            let start = Instant::now();

            let handles: Vec<_> = (0..threads)
                .map(|tid| {
                    let store = Arc::clone(&store);
                    let offset = tid as u64 * ops_per_thread;
                    std::thread::spawn(move || {
                        let mut session = store.new_session();
                        {
                            let mut ctx = store.unsafe_context(&mut session);
                            for i in 0..ops_per_thread {
                                let k = offset + i;
                                let _ = black_box(ctx.upsert(&store, &k, &k, ()));
                                if i % REFRESH_INTERVAL == 0 {
                                    ctx.refresh();
                                }
                            }
                        }
                        store.dispose_session(session);
                    })
                })
                .collect();

            for h in handles {
                h.join().expect("worker panicked");
            }
            let elapsed = start.elapsed();
            let total_ops_sec = total_ops as f64 / elapsed.as_secs_f64();
            let per_thread = total_ops_sec / threads as f64;
            times.push((total_ops_sec, per_thread));
        }

        times.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (total, per) = times[times.len() / 2];
        println!(
            "  {} thread(s): {:.1}M total ops/sec, {:.1}M/thread, {:.1} ns/op/thread",
            threads,
            total / 1e6,
            per / 1e6,
            1e9 / per
        );
    }
}

// ---------------------------------------------------------------------------
// 9. Refresh cost measurement
// ---------------------------------------------------------------------------

fn bench_refresh_cost() {
    println!("\n=== 9. UnsafeContext Refresh Cost (amortized per batch) ===");

    let store = make_store(OPS as usize);
    populate(&store, OPS);

    // Measure different refresh intervals
    for interval in [32u64, 64, 128, 256, 512, 1024] {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let mut session = store.new_session();
            let start = Instant::now();
            {
                let mut ctx = store.unsafe_context(&mut session);
                for k in 0..OPS {
                    let _ = black_box(ctx.upsert(&store, &k, &(k + 1), ()));
                    if k % interval == 0 {
                        ctx.refresh();
                    }
                }
            }
            let elapsed = start.elapsed();
            store.dispose_session(session);
            times.push(elapsed.as_nanos() as f64 / OPS as f64);
        }
        let med = median(&mut times);
        println!(
            "  Refresh every {} ops: {:.1} ns/op ({:.1}M ops/sec)",
            interval,
            med,
            1000.0 / med
        );
    }
}

// ---------------------------------------------------------------------------
// 10. Component cost breakdown (compute differences)
// ---------------------------------------------------------------------------

fn cost_breakdown() {
    println!("\n=== 10. Component Cost Breakdown (derived from measurements) ===");
    println!("  NOTE: These are computed from the measurements above.");
    println!("  Run all benchmarks above first, then compare the numbers.");
    println!();
    println!("  Formula:");
    println!("    epoch_cost = per_op_upsert - amortized_upsert");
    println!(
        "    hash_index_cost = amortized_upsert_insert - pipeline_insert (from existing bench)"
    );
    println!("    record_write_cost = pipeline_insert - epoch_cost - hash_cost");
    println!();
    println!("  To compute hash index traversal overhead:");
    println!("    hash_traversal = per_op_upsert_update - per_op_upsert_insert");
    println!("    (update must find existing entry, insert just finds empty slot)");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("╔══════════════════════════════════════════════════════╗");
    println!("║  FASTER Rust — Performance Analysis (AI-4)          ║");
    println!("║  Epoch Amortization & Bottleneck Breakdown          ║");
    println!("╚══════════════════════════════════════════════════════╝");
    println!();
    println!(
        "Configuration: {} ops per benchmark, {} iterations (median reported)",
        OPS, ITERS
    );

    bench_bare_epoch();
    bench_epoch_with_drain();
    bench_hash_only();
    bench_per_op_upsert_insert();
    bench_per_op_upsert_update();
    bench_amortized_upsert_insert();
    bench_amortized_upsert_update();
    bench_reads();
    bench_multithread_scaling();
    bench_multithread_amortized();
    bench_refresh_cost();
    cost_breakdown();

    println!("\n=== DONE ===");
}
