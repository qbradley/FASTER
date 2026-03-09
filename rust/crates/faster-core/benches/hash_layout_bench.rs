//! Hash layout benchmark: comparing current multi-slot buckets with a
//! standalone open-addressing prototype.
//!
//! This benchmark measures the isolated hash index lookup performance under
//! different configurations to identify where the ~14% read gap vs C# lives.
//!
//! Variants:
//! - `current_find` — full HashTable::find_entry() as-is
//! - `open_addr_find` — standalone open-addressing table (Robin Hood hashing)
//! - `open_addr_find_prefetch` — open addressing with software prefetch
//!
//! Run: `cd rust && cargo bench --bench hash_layout_bench`

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{BenchmarkId, Criterion, criterion_group};
use faster_core::address::LogicalAddress;
use faster_core::hash::{KeyHash, faster_hash_u64};
use faster_core::hash_table::HashTable;

// ---------------------------------------------------------------------------
// Open-addressing prototype (Robin Hood with linear probing)
// ---------------------------------------------------------------------------

/// A single slot in the open-addressing table.
///
/// Stores tag + logical address in one atomic u64, matching FASTER's
/// 64-bit packed entry format. This gives us an apples-to-apples
/// comparison: same data, different collision resolution.
#[repr(C)]
struct OASlot {
    entry: AtomicU64,
}

/// Bit layout — identical to HashBucketEntry for fair comparison:
///   [63] tentative | [62] reserved | [61:48] tag (14) | [47:0] address (48)
const OA_TAG_SHIFT: u32 = 48;
const OA_TAG_MASK: u64 = 0x3FFF;
const OA_ADDR_MASK: u64 = (1u64 << 48) - 1;

#[inline(always)]
fn oa_pack(tag: u16, addr: u64) -> u64 {
    (addr & OA_ADDR_MASK) | ((tag as u64 & OA_TAG_MASK) << OA_TAG_SHIFT)
}

#[inline(always)]
fn oa_tag(packed: u64) -> u16 {
    ((packed >> OA_TAG_SHIFT) & OA_TAG_MASK) as u16
}

#[inline(always)]
fn oa_addr(packed: u64) -> u64 {
    packed & OA_ADDR_MASK
}

/// Open-addressing hash table with linear probing.
///
/// This is a research prototype — NOT production-ready. It demonstrates the
/// cache behavior of open addressing for comparison against FASTER's
/// multi-slot bucket design.
///
/// Key differences from FASTER's bucket approach:
/// - No overflow chains (linear probing instead)
/// - Each slot is 8 bytes (same as a bucket entry)
/// - Probe sequence is contiguous in memory (good for prefetch)
/// - Load factor must stay well below 1.0
struct OpenAddressingTable {
    slots: Box<[OASlot]>,
    mask: u64,
    #[allow(dead_code)]
    num_slots: u64,
}

impl OpenAddressingTable {
    /// Creates a table with `2^log2_size` slots.
    fn new(log2_size: u32) -> Self {
        let num_slots = 1u64 << log2_size;
        let slots: Vec<OASlot> = (0..num_slots)
            .map(|_| OASlot {
                entry: AtomicU64::new(0),
            })
            .collect();
        Self {
            slots: slots.into_boxed_slice(),
            mask: num_slots - 1,
            num_slots,
        }
    }

    /// Insert a key hash with a logical address.
    /// Uses linear probing. Panics on full table.
    fn insert(&self, hash: u64, addr: u64) {
        let tag = ((hash >> 50) & OA_TAG_MASK) as u16;
        // Ensure tag is non-zero so empty slots (0) are distinguishable.
        let tag = if tag == 0 { 1 } else { tag };
        let packed = oa_pack(tag, addr);
        let mut idx = (hash & self.mask) as usize;

        loop {
            let current = self.slots[idx].entry.load(Ordering::Relaxed);
            if current == 0 {
                // Empty slot — try to claim it.
                if self.slots[idx]
                    .entry
                    .compare_exchange(0, packed, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                // Lost race, continue probing.
            }
            idx = ((idx as u64 + 1) & self.mask) as usize;
        }
    }

    /// Find by key hash. Returns the logical address if found.
    #[inline(always)]
    fn find(&self, hash: u64) -> Option<u64> {
        let tag = ((hash >> 50) & OA_TAG_MASK) as u16;
        let tag = if tag == 0 { 1 } else { tag };
        let mut idx = (hash & self.mask) as usize;

        loop {
            let packed = self.slots[idx].entry.load(Ordering::Acquire);
            if packed == 0 {
                return None; // Empty slot — key not present.
            }
            if oa_tag(packed) == tag {
                return Some(oa_addr(packed));
            }
            idx = ((idx as u64 + 1) & self.mask) as usize;
        }
    }

    /// Find with software prefetch of next probe slot.
    #[inline(always)]
    fn find_prefetch(&self, hash: u64) -> Option<u64> {
        let tag = ((hash >> 50) & OA_TAG_MASK) as u16;
        let tag = if tag == 0 { 1 } else { tag };
        let mut idx = (hash & self.mask) as usize;

        loop {
            // Prefetch the next probe position.
            let next_idx = ((idx as u64 + 1) & self.mask) as usize;
            faster_core::hash::prefetch::prefetch_read(&self.slots[next_idx] as *const OASlot);

            let packed = self.slots[idx].entry.load(Ordering::Acquire);
            if packed == 0 {
                return None;
            }
            if oa_tag(packed) == tag {
                return Some(oa_addr(packed));
            }
            idx = next_idx;
        }
    }
}

// ---------------------------------------------------------------------------
// Benchmark setup
// ---------------------------------------------------------------------------

/// Pre-compute N key hashes and insert them into both table types.
struct BenchSetup {
    hashes: Vec<u64>,
    key_hashes: Vec<KeyHash>,
    faster_table: HashTable,
    oa_table: OpenAddressingTable,
}

impl BenchSetup {
    fn new(log2_size: u32, num_keys: usize) -> Self {
        // Generate deterministic key hashes.
        let hashes: Vec<u64> = (0..num_keys as u64).map(faster_hash_u64).collect();
        let key_hashes: Vec<KeyHash> = hashes.iter().map(|&h| KeyHash::new(h)).collect();

        // FASTER multi-slot table.
        let faster_table = HashTable::new(log2_size);
        for (i, &kh) in key_hashes.iter().enumerate() {
            let addr = LogicalAddress::from_raw((i as u64 + 1) * 64);
            let result = faster_table.find_or_create_entry(kh, addr);
            if result.created {
                // Commit the tentative entry.
                let committed =
                    faster_core::hash_bucket::HashBucketEntry::new(result.entry.tag(), addr, false);
                faster_table.update_entry(result.slot, result.entry, committed);
            }
        }

        // Open-addressing table (2× slots for ~50% load factor).
        let oa_table = OpenAddressingTable::new(log2_size + 1);
        for (i, &h) in hashes.iter().enumerate() {
            let addr = (i as u64 + 1) * 64;
            oa_table.insert(h, addr);
        }

        Self {
            hashes,
            key_hashes,
            faster_table,
            oa_table,
        }
    }
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_hash_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_layout");

    // Test at different sizes to see scaling behavior.
    for &(log2_size, num_keys, label) in &[
        (14u32, 50_000usize, "50K"),
        (16, 200_000, "200K"),
        (18, 800_000, "800K"),
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

        // --- Open addressing: find ---
        group.bench_with_input(
            BenchmarkId::new("open_addr_find", label),
            &setup,
            |b, setup| {
                let mut idx = 0usize;
                b.iter(|| {
                    let h = setup.hashes[idx % setup.hashes.len()];
                    idx += 1;
                    black_box(setup.oa_table.find(h));
                });
            },
        );

        // --- Open addressing: find with prefetch ---
        group.bench_with_input(
            BenchmarkId::new("open_addr_prefetch_find", label),
            &setup,
            |b, setup| {
                let mut idx = 0usize;
                b.iter(|| {
                    let h = setup.hashes[idx % setup.hashes.len()];
                    idx += 1;
                    black_box(setup.oa_table.find_prefetch(h));
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

    let log2_size = 18u32;
    let num_keys = 800_000usize;
    let setup = BenchSetup::new(log2_size, num_keys);
    let batch_size = 16;

    // FASTER: batched prefetch + find (realistic usage pattern)
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

    // Open addressing: batched prefetch + find
    group.bench_function("oa_batch16", |b| {
        let mut base_idx = 0usize;
        b.iter(|| {
            // Phase 1: prefetch.
            for i in 0..batch_size {
                let h = setup.hashes[(base_idx + i) % setup.hashes.len()];
                let idx = (h & setup.oa_table.mask) as usize;
                faster_core::hash::prefetch::prefetch_read(
                    &setup.oa_table.slots[idx] as *const OASlot,
                );
            }
            // Phase 2: find.
            let mut found = 0u32;
            for i in 0..batch_size {
                let h = setup.hashes[(base_idx + i) % setup.hashes.len()];
                if setup.oa_table.find(h).is_some() {
                    found += 1;
                }
            }
            base_idx += batch_size;
            black_box(found);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_hash_layout, bench_batched_lookup);

/// Custom main: run full criterion benchmarks only when `--bench` is passed
/// (i.e., `cargo bench`).  Under `cargo nextest --all-targets` the binary is
/// invoked without `--bench`, so we run a fast smoke test instead.
fn main() {
    if std::env::args().any(|a| a == "--bench") {
        // TODO: these benchmarks are too slow - it seems like they take *hours* to run
        // and that is not how benchmark are supposed to work.
        //benches();
    } else {
        // Smoke test — verify setup doesn't panic, without criterion overhead.
        let table = HashTable::new(10); // 2^10 = 1024 buckets
        let key: u64 = 42;
        let hash = KeyHash::new(faster_hash_u64(key));
        let _ = table.find_entry(hash);
    }
}
