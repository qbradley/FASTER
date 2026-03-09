//! Hash layout benchmark for FASTER multi-slot bucket hash index.
//!
//! Measures isolated hash index lookup performance to detect regressions.
//!
//! Variants:
//! - `faster_find` — full HashTable::find_entry() as-is
//! - `faster_prefetch_find` — prefetch + find_entry
//! - `faster_batch16` — batched prefetch + find (realistic usage pattern)
//!
//! Run: `cd rust && cargo bench --bench hash_layout_bench`

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group};
use faster_core::address::LogicalAddress;
use faster_core::hash::{KeyHash, faster_hash_u64};
use faster_core::hash_table::HashTable;

// ---------------------------------------------------------------------------
// Benchmark setup
// ---------------------------------------------------------------------------

/// Pre-compute N key hashes and insert them into the FASTER hash table.
struct BenchSetup {
    key_hashes: Vec<KeyHash>,
    faster_table: HashTable,
}

impl BenchSetup {
    fn new(log2_size: u32, num_keys: usize) -> Self {
        let hashes: Vec<u64> = (0..num_keys as u64).map(faster_hash_u64).collect();
        let key_hashes: Vec<KeyHash> = hashes.iter().map(|&h| KeyHash::new(h)).collect();

        let faster_table = HashTable::new(log2_size);
        for (i, &kh) in key_hashes.iter().enumerate() {
            let addr = LogicalAddress::from_raw((i as u64 + 1) * 64);
            let result = faster_table.find_or_create_entry(kh, addr);
            if result.created {
                let committed =
                    faster_core::hash_bucket::HashBucketEntry::new(result.entry.tag(), addr, false);
                faster_table.update_entry(result.slot, result.entry, committed);
            }
        }

        Self {
            key_hashes,
            faster_table,
        }
    }
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_hash_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_layout");

    // Sizes chosen to span L2-cache to L3-cache boundary while keeping setup fast.
    for &(log2_size, num_keys, label) in &[
        (10u32, 3_000usize, "3K"),
        (12, 12_000, "12K"),
        (14, 50_000, "50K"),
    ] {
        let setup = BenchSetup::new(log2_size, num_keys);

        // --- FASTER multi-slot bucket: find_entry ---
        group.bench_with_input(
            BenchmarkId::new("faster_find", label),
            &setup,
            |b, setup| {
                let mut idx = 0usize;
                b.iter(|| {
                    let kh = setup.key_hashes[idx % setup.key_hashes.len()];
                    idx += 1;
                    black_box(setup.faster_table.find_entry(kh));
                });
            },
        );

        // --- FASTER multi-slot bucket: prefetch + find_entry ---
        group.bench_with_input(
            BenchmarkId::new("faster_prefetch_find", label),
            &setup,
            |b, setup| {
                let mut idx = 0usize;
                b.iter(|| {
                    let kh = setup.key_hashes[idx % setup.key_hashes.len()];
                    idx += 1;
                    setup.faster_table.prefetch_bucket(kh);
                    black_box(setup.faster_table.find_entry(kh));
                });
            },
        );
    }

    group.finish();
}

/// Benchmark that simulates a batch of lookups with interleaved prefetch,
/// which is how FASTER actually operates in practice.
fn bench_batched_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_layout_batched");

    let log2_size = 14u32;
    let num_keys = 50_000usize;
    let setup = BenchSetup::new(log2_size, num_keys);
    let batch_size = 16;

    group.bench_function("faster_batch16", |b| {
        let mut base_idx = 0usize;
        b.iter(|| {
            // Phase 1: issue prefetch for entire batch.
            for i in 0..batch_size {
                let kh = setup.key_hashes[(base_idx + i) % setup.key_hashes.len()];
                setup.faster_table.prefetch_bucket(kh);
            }
            // Phase 2: find all entries (bucket data should be in cache now).
            let mut found = 0u32;
            for i in 0..batch_size {
                let kh = setup.key_hashes[(base_idx + i) % setup.key_hashes.len()];
                if setup.faster_table.find_entry(kh).is_some() {
                    found += 1;
                }
            }
            base_idx += batch_size;
            black_box(found);
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).measurement_time(Duration::from_secs(3));
    targets = bench_hash_layout, bench_batched_lookup
}

/// Custom main: run full criterion benchmarks only when `--bench` is passed
/// (i.e., `cargo bench`).  Under `cargo nextest --all-targets` the binary is
/// invoked without `--bench`, so we run a fast smoke test instead.
fn main() {
    if std::env::args().any(|a| a == "--bench") {
        benches();
    } else {
        // Smoke test: verify setup doesn't panic, without criterion overhead.
        let table = HashTable::new(10); // 2^10 = 1024 buckets
        let key: u64 = 42;
        let hash = KeyHash::new(faster_hash_u64(key));
        let _ = table.find_entry(hash);
    }
}
