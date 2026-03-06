//! YCSB-style performance benchmarks for the FASTER KV store.
//!
//! These benchmarks establish a performance baseline for the Rust FASTER
//! implementation using `SimpleFunctions<u64, u64>` with in-memory storage.

use std::sync::Arc;

use criterion::{
    BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
    measurement::WallTime,
};
use faster_core::NullDevice;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type Store = FasterKv<SimpleFunctions<u64, u64>>;

/// Build a store sized for `n` expected keys.
fn make_store(n: usize) -> Store {
    // Choose hash index size so load factor stays ≤ 50%.
    let log2 = ((n as f64 * 2.0).log2().ceil() as usize).max(10);
    let config = FasterKvConfig {
        hash_index_size_log2: log2,
        buffer_size_pages: 256, // large buffer to keep everything in memory
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
    };
    FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
}

/// Pre-populate `store` with keys `0..n`, each with value = key.
fn populate(store: &Store, n: u64) {
    let mut session = store.new_session();
    for k in 0..n {
        let _ = store.upsert(&mut session, &k, &k, ());
    }
    store.dispose_session(session);
}

/// Simple deterministic PRNG (xorshift64) for reproducible key access patterns.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    #[inline]
    fn next_in_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SMALL_N: u64 = 100_000; // operations per iteration for quick benchmarks
const LARGE_N: u64 = 250_000; // used for mixed-workload benchmarks

// ---------------------------------------------------------------------------
// 1. Sequential upsert throughput
// ---------------------------------------------------------------------------

fn bench_sequential_upsert(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/sequential_upsert");
    group.throughput(Throughput::Elements(SMALL_N));
    group.sample_size(10);

    group.bench_function("100K_ops", |b| {
        b.iter_with_setup(
            || make_store(SMALL_N as usize),
            |store| {
                let mut session = store.new_session();
                for k in 0..SMALL_N {
                    let _ = black_box(store.upsert(&mut session, &k, &k, ()));
                }
                store.dispose_session(session);
            },
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 2. Sequential read throughput
// ---------------------------------------------------------------------------

fn bench_sequential_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/sequential_read");
    group.throughput(Throughput::Elements(SMALL_N));
    group.sample_size(10);

    let store = make_store(SMALL_N as usize);
    populate(&store, SMALL_N);

    group.bench_function("100K_ops", |b| {
        b.iter(|| {
            let mut session = store.new_session();
            let mut output: Option<u64> = None;
            for k in 0..SMALL_N {
                let status = store.read(&mut session, &k, &0u64, &mut output, ());
                debug_assert_eq!(status, OperationStatus::Ok);
                black_box(&output);
            }
            store.dispose_session(session);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 3. Mixed read/upsert (50/50) — YCSB Workload A
// ---------------------------------------------------------------------------

fn bench_mixed_50_50(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/mixed_50_50");
    group.throughput(Throughput::Elements(LARGE_N));
    group.sample_size(10);

    let store = make_store(LARGE_N as usize);
    populate(&store, LARGE_N);

    group.bench_function("workload_a", |b| {
        b.iter_with_setup(
            || Rng::new(0xDEAD_BEEF),
            |mut rng| {
                let mut session = store.new_session();
                let mut output: Option<u64> = None;
                for _ in 0..LARGE_N {
                    let k = rng.next_in_range(LARGE_N);
                    if rng.next_u64() & 1 == 0 {
                        let _ = black_box(store.read(&mut session, &k, &0u64, &mut output, ()));
                    } else {
                        let _ = black_box(store.upsert(&mut session, &k, &k, ()));
                    }
                }
                store.dispose_session(session);
            },
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 4. Read-heavy (95/5) — YCSB Workload B
// ---------------------------------------------------------------------------

fn bench_read_heavy_95_5(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/read_heavy_95_5");
    group.throughput(Throughput::Elements(LARGE_N));
    group.sample_size(10);

    let store = make_store(LARGE_N as usize);
    populate(&store, LARGE_N);

    group.bench_function("workload_b", |b| {
        b.iter_with_setup(
            || Rng::new(0xCAFE_BABE),
            |mut rng| {
                let mut session = store.new_session();
                let mut output: Option<u64> = None;
                for _ in 0..LARGE_N {
                    let k = rng.next_in_range(LARGE_N);
                    if rng.next_u64() % 100 < 95 {
                        let _ = black_box(store.read(&mut session, &k, &0u64, &mut output, ()));
                    } else {
                        let _ = black_box(store.upsert(&mut session, &k, &k, ()));
                    }
                }
                store.dispose_session(session);
            },
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 5. Read-Modify-Write throughput
// ---------------------------------------------------------------------------

fn bench_rmw(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/rmw");
    group.throughput(Throughput::Elements(SMALL_N));
    group.sample_size(10);

    let store = make_store(SMALL_N as usize);
    populate(&store, SMALL_N);

    group.bench_function("100K_ops", |b| {
        b.iter(|| {
            let mut session = store.new_session();
            let mut output: Option<u64> = None;
            for k in 0..SMALL_N {
                let _ = black_box(store.rmw(&mut session, &k, &1u64, &mut output, ()));
            }
            store.dispose_session(session);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 6. Multi-threaded scaling — upsert throughput at 1, 2, 4, 8 threads
// ---------------------------------------------------------------------------

fn bench_multithread_upsert(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/multithread_upsert");
    group.sample_size(10);

    let available_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let thread_counts: Vec<usize> = [1, 2, 4, 8]
        .iter()
        .copied()
        .filter(|&t| t <= available_cpus)
        .collect();

    let ops_per_thread = SMALL_N;

    for &threads in &thread_counts {
        let total_ops = ops_per_thread * threads as u64;
        group.throughput(Throughput::Elements(total_ops));

        group.bench_with_input(
            BenchmarkId::new("threads", threads),
            &threads,
            |b, &threads| {
                b.iter_with_setup(
                    || Arc::new(make_store((total_ops as usize) * 2)),
                    |store| {
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
                            h.join().expect("worker thread panicked");
                        }
                    },
                );
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// 7. Point read latency distribution (p50, p99, p99.9)
// ---------------------------------------------------------------------------

fn bench_read_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb/read_latency");
    // Measure single operations to capture latency distribution.
    group.throughput(Throughput::Elements(1));

    let n: u64 = 100_000;
    let store = make_store(n as usize);
    populate(&store, n);
    let mut session = store.new_session();
    let mut rng = Rng::new(0x1234_5678);

    group.bench_function("point_read", |b| {
        b.iter(|| {
            let k = rng.next_in_range(n);
            let mut output: Option<u64> = None;
            let _ = black_box(store.read(&mut session, &k, &0u64, &mut output, ()));
            black_box(&output);
        });
    });

    store.dispose_session(session);
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

fn ycsb_benchmarks(c: &mut Criterion) {
    bench_sequential_upsert(c);
    bench_sequential_read(c);
    bench_mixed_50_50(c);
    bench_read_heavy_95_5(c);
    bench_rmw(c);
    bench_multithread_upsert(c);
    bench_read_latency(c);
}

fn custom_config() -> Criterion<WallTime> {
    Criterion::default().warm_up_time(std::time::Duration::from_secs(1))
}

criterion_group! {
    name = benches;
    config = custom_config();
    targets = ycsb_benchmarks
}
criterion_main!(benches);
