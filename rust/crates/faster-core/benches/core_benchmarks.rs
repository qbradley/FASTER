use criterion::{Criterion, black_box, criterion_group, criterion_main};

// ---------------------------------------------------------------------------
// Hash benchmarks
// ---------------------------------------------------------------------------

fn bench_hash_u64(c: &mut Criterion) {
    c.bench_function("faster_hash_u64", |b| {
        let mut key = 0u64;
        b.iter(|| {
            key = key.wrapping_add(1);
            black_box(faster_core::hash::faster_hash_u64(black_box(key)));
        });
    });
}

fn bench_hash_bytes_short(c: &mut Criterion) {
    let data = b"hello world!";
    c.bench_function("faster_hash_bytes/12B", |b| {
        b.iter(|| {
            black_box(faster_core::hash::faster_hash_bytes(black_box(data)));
        });
    });
}

fn bench_hash_bytes_medium(c: &mut Criterion) {
    let data = vec![0xABu8; 128];
    c.bench_function("faster_hash_bytes/128B", |b| {
        b.iter(|| {
            black_box(faster_core::hash::faster_hash_bytes(black_box(&data)));
        });
    });
}

fn bench_hashable_u64(c: &mut Criterion) {
    use faster_core::hash::Hashable;
    c.bench_function("Hashable::hash/u64", |b| {
        let mut key = 0u64;
        b.iter(|| {
            key = key.wrapping_add(1);
            black_box(Hashable::hash(black_box(&key)));
        });
    });
}

fn bench_hashable_str(c: &mut Criterion) {
    use faster_core::hash::Hashable;
    let keys: Vec<String> = (0..1000).map(|i| format!("key-{i:08}")).collect();
    c.bench_function("Hashable::hash/str", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let key = &keys[idx % keys.len()];
            idx += 1;
            black_box(Hashable::hash(key.as_str()));
        });
    });
}

fn bench_tag_extraction(c: &mut Criterion) {
    c.bench_function("tag_from_hash", |b| {
        let mut h = 0u64;
        b.iter(|| {
            h = h.wrapping_add(0x0001_0001_0001_0001);
            black_box(faster_core::hash::tag_from_hash(black_box(h)));
        });
    });
}

// ---------------------------------------------------------------------------
// Hash bucket entry benchmarks
// ---------------------------------------------------------------------------

fn bench_entry_new(c: &mut Criterion) {
    use faster_core::address::{LogicalAddress, Offset, Page};
    c.bench_function("HashBucketEntry::new", |b| {
        let mut tag = 0u16;
        let addr = LogicalAddress::new(Page(42), Offset(1024));
        b.iter(|| {
            tag = tag.wrapping_add(1) & 0x3FFF;
            black_box(faster_core::hash_bucket::HashBucketEntry::new(
                black_box(tag),
                black_box(addr),
                false,
            ));
        });
    });
}

fn bench_entry_tag_extraction(c: &mut Criterion) {
    use faster_core::hash_bucket::HashBucketEntry;
    c.bench_function("HashBucketEntry::tag", |b| {
        let mut raw = 0u64;
        b.iter(|| {
            raw = raw.wrapping_add(0x0001_0001_0001_0001);
            let entry = HashBucketEntry::from_raw(raw);
            black_box(entry.tag());
        });
    });
}

fn bench_entry_address_extraction(c: &mut Criterion) {
    use faster_core::hash_bucket::HashBucketEntry;
    c.bench_function("HashBucketEntry::address", |b| {
        let mut raw = 0u64;
        b.iter(|| {
            raw = raw.wrapping_add(0x0001_0001_0001_0001);
            let entry = HashBucketEntry::from_raw(raw);
            black_box(entry.address());
        });
    });
}

fn bench_atomic_compare_exchange(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{AtomicHashBucketEntry, HashBucketEntry};

    c.bench_function("AtomicHashBucketEntry::compare_exchange", |b| {
        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let entry_a = HashBucketEntry::new(0x1234, addr, false);
        let entry_b = HashBucketEntry::EMPTY;
        let mut toggle = false;

        b.iter(|| {
            let (expected, desired) = if toggle {
                (entry_a, entry_b)
            } else {
                (entry_b, entry_a)
            };
            let _ = black_box(atomic.compare_exchange(
                expected,
                desired,
                Ordering::AcqRel,
                Ordering::Acquire,
            ));
            toggle = !toggle;
        });
    });
}

// ---------------------------------------------------------------------------
// Hash bucket benchmarks
// ---------------------------------------------------------------------------

fn bench_bucket_find_entry_miss(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};

    let bucket = HashBucket::new();
    let addr = LogicalAddress::new(Page(1), Offset(42));
    // Fill all 7 slots with distinct tags
    for i in 0..BUCKET_NUM_ENTRIES {
        let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
        bucket.entry(i).store(entry, Ordering::Relaxed);
    }
    c.bench_function("HashBucket::find_entry/miss", |b| {
        b.iter(|| {
            black_box(bucket.find_entry(black_box(0x3FFF)));
        });
    });
}

fn bench_bucket_find_entry_hit_first(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};

    let bucket = HashBucket::new();
    let addr = LogicalAddress::new(Page(1), Offset(42));
    for i in 0..BUCKET_NUM_ENTRIES {
        let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
        bucket.entry(i).store(entry, Ordering::Relaxed);
    }
    c.bench_function("HashBucket::find_entry/hit_slot0", |b| {
        b.iter(|| {
            black_box(bucket.find_entry(black_box(1)));
        });
    });
}

fn bench_bucket_find_entry_hit_last(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};

    let bucket = HashBucket::new();
    let addr = LogicalAddress::new(Page(1), Offset(42));
    for i in 0..BUCKET_NUM_ENTRIES {
        let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
        bucket.entry(i).store(entry, Ordering::Relaxed);
    }
    c.bench_function("HashBucket::find_entry/hit_slot6", |b| {
        b.iter(|| {
            black_box(bucket.find_entry(black_box(7)));
        });
    });
}

fn bench_bucket_try_insert(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{HashBucket, HashBucketEntry};

    c.bench_function("HashBucket::try_insert+reset", |b| {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(42));
        b.iter(|| {
            // Insert into slot 0 then reset — measures CAS insert cost
            let _ = bucket.try_insert(black_box(0x1234), black_box(addr));
            bucket
                .entry(0)
                .store(HashBucketEntry::EMPTY, Ordering::Relaxed);
        });
    });
}

// ---------------------------------------------------------------------------
// Epoch benchmarks
// ---------------------------------------------------------------------------

fn bench_epoch_protect_unprotect(c: &mut Criterion) {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;

    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    c.bench_function("epoch/protect+unprotect (single-threaded)", |b| {
        b.iter(|| {
            let guard = thread.protect();
            black_box(guard.epoch());
            drop(guard);
        });
    });
}

fn bench_epoch_protect_refresh_unprotect(c: &mut Criterion) {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;

    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");

    c.bench_function("epoch/protect+refresh+unprotect", |b| {
        b.iter(|| {
            let guard = thread.protect();
            guard.refresh();
            drop(guard);
        });
    });
}

fn bench_epoch_bump(c: &mut Criterion) {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;

    let table = Arc::new(EpochTable::new());

    c.bench_function("epoch/bump_current_epoch (no callback)", |b| {
        b.iter(|| {
            table.bump_current_epoch_no_callback();
        });
    });
}

fn bench_epoch_bump_with_drain(c: &mut Criterion) {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;

    let table = Arc::new(EpochTable::new());

    c.bench_function("epoch/bump+drain callback", |b| {
        b.iter(|| {
            table.bump_current_epoch(|| {
                black_box(());
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Record serialization benchmarks
// ---------------------------------------------------------------------------

fn bench_record_write_u64(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, write_record};

    let key: u64 = 0xDEAD_BEEF;
    let value: u64 = 0xCAFE_BABE;
    let layout = RecordLayout::for_kv(&key, &value);
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let mut buf = vec![0u8; layout.total_size()];

    c.bench_function("record/write u64+u64", |b| {
        b.iter(|| {
            write_record(
                black_box(&mut buf),
                black_box(&info),
                black_box(&key),
                black_box(&value),
                &layout,
            );
        });
    });
}

fn bench_record_read_u64(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, read_key, read_value, write_record};

    let key: u64 = 0xDEAD_BEEF;
    let value: u64 = 0xCAFE_BABE;
    let layout = RecordLayout::for_kv(&key, &value);
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let mut buf = vec![0u8; layout.total_size()];
    write_record(&mut buf, &info, &key, &value, &layout);

    c.bench_function("record/read u64+u64", |b| {
        b.iter(|| {
            let k: u64 = read_key(black_box(&buf), &layout);
            let v: u64 = read_value(black_box(&buf), &layout);
            black_box((k, v));
        });
    });
}

fn bench_record_write_variable(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, write_record};

    let key: Vec<u8> = vec![0xAB; 32];
    let value: Vec<u8> = vec![0xCD; 128];
    let layout = RecordLayout::for_kv(&key, &value);
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let mut buf = vec![0u8; layout.total_size()];

    c.bench_function("record/write Vec<u8> 32B+128B", |b| {
        b.iter(|| {
            write_record(
                black_box(&mut buf),
                black_box(&info),
                black_box(&key),
                black_box(&value),
                &layout,
            );
        });
    });
}

fn bench_record_read_variable(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, read_key, read_value, write_record};

    let key: Vec<u8> = vec![0xAB; 32];
    let value: Vec<u8> = vec![0xCD; 128];
    let layout = RecordLayout::for_kv(&key, &value);
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let mut buf = vec![0u8; layout.total_size()];
    write_record(&mut buf, &info, &key, &value, &layout);

    c.bench_function("record/read Vec<u8> 32B+128B", |b| {
        b.iter(|| {
            let k: Vec<u8> = read_key(black_box(&buf), &layout);
            let v: Vec<u8> = read_value(black_box(&buf), &layout);
            black_box((k, v));
        });
    });
}

// ---------------------------------------------------------------------------
// Hash throughput for various key sizes
// ---------------------------------------------------------------------------

fn bench_hash_bytes_long(c: &mut Criterion) {
    let data = vec![0xABu8; 1024];
    c.bench_function("faster_hash_bytes/1KB", |b| {
        b.iter(|| {
            black_box(faster_core::hash::faster_hash_bytes(black_box(&data)));
        });
    });
}

fn bench_hash_bytes_4k(c: &mut Criterion) {
    let data = vec![0xABu8; 4096];
    c.bench_function("faster_hash_bytes/4KB", |b| {
        b.iter(|| {
            black_box(faster_core::hash::faster_hash_bytes(black_box(&data)));
        });
    });
}

// ---------------------------------------------------------------------------
// Allocator throughput benchmark
// ---------------------------------------------------------------------------

fn bench_alloc_throughput(c: &mut Criterion) {
    use faster_core::allocator::MallocFixedPageSize;

    #[repr(C)]
    struct Item64([u64; 8]);

    let mut group = c.benchmark_group("allocator_throughput");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("bump_only", |b| {
        let alloc = MallocFixedPageSize::<Item64>::new();
        b.iter(|| {
            black_box(alloc.allocate());
        });
    });

    group.bench_function("alloc+free_cycle", |b| {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let addr = alloc.allocate();
        alloc.free(addr);
        b.iter(|| {
            let a = alloc.allocate();
            alloc.free(a);
            black_box(a);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bucket scan with various fill levels
// ---------------------------------------------------------------------------

fn bench_bucket_find_various_fill(c: &mut Criterion) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{HashBucket, HashBucketEntry};

    let mut group = c.benchmark_group("bucket_find_fill_levels");
    let addr = LogicalAddress::new(Page(1), Offset(42));

    for fill in [1, 3, 5, 7] {
        group.bench_with_input(
            criterion::BenchmarkId::new("miss", fill),
            &fill,
            |b, &fill| {
                let bucket = HashBucket::new();
                for i in 0..fill {
                    let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
                    bucket.entry(i).store(entry, Ordering::Relaxed);
                }
                b.iter(|| {
                    black_box(bucket.find_entry(black_box(0x3FFF)));
                });
            },
        );

        group.bench_with_input(
            criterion::BenchmarkId::new("hit_last", fill),
            &fill,
            |b, &fill| {
                let bucket = HashBucket::new();
                for i in 0..fill {
                    let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
                    bucket.entry(i).store(entry, Ordering::Relaxed);
                }
                let target_tag = fill as u16;
                b.iter(|| {
                    black_box(bucket.find_entry(black_box(target_tag)));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    // Hash
    bench_hash_u64,
    bench_hash_bytes_short,
    bench_hash_bytes_medium,
    bench_hash_bytes_long,
    bench_hash_bytes_4k,
    bench_hashable_u64,
    bench_hashable_str,
    bench_tag_extraction,
    // Hash bucket entry
    bench_entry_new,
    bench_entry_tag_extraction,
    bench_entry_address_extraction,
    bench_atomic_compare_exchange,
    // Hash bucket
    bench_bucket_find_entry_miss,
    bench_bucket_find_entry_hit_first,
    bench_bucket_find_entry_hit_last,
    bench_bucket_try_insert,
    bench_bucket_find_various_fill,
    // Allocator
    bench_alloc_bump,
    bench_alloc_free_reuse,
    bench_alloc_throughput,
    // Overflow chain
    bench_chain_traversal_depth0,
    bench_chain_traversal_depth1,
    bench_chain_traversal_depth2,
    bench_chain_traversal_depth5,
    bench_find_entry_in_chain,
    bench_insert_in_chain,
    // Epoch
    bench_epoch_protect_unprotect,
    bench_epoch_protect_refresh_unprotect,
    bench_epoch_bump,
    bench_epoch_bump_with_drain,
    // Record serialization
    bench_record_write_u64,
    bench_record_read_u64,
    bench_record_write_variable,
    bench_record_read_variable,
    // Hash index
    bench_hash_index_insert_sequential,
    bench_hash_index_find_hit,
    bench_hash_index_find_miss,
    bench_hash_index_insert_concurrent,
    bench_hash_index_find_concurrent,
    bench_hash_index_mixed_rw_concurrent_8,
    // Compaction — size discovery and key comparison
    bench_compaction_size_from_bytes_u64,
    bench_compaction_size_from_bytes_variable,
    bench_compaction_eq_from_bytes_u64,
    bench_compaction_eq_from_bytes_variable,
    bench_compaction_record_size_u64,
    bench_compaction_record_size_variable,
);
criterion_main!(benches);

// ---------------------------------------------------------------------------
// Allocator benchmarks
// ---------------------------------------------------------------------------

fn bench_alloc_bump(c: &mut Criterion) {
    use faster_core::allocator::MallocFixedPageSize;

    #[repr(C)]
    struct Item64([u64; 8]);

    c.bench_function("MallocFixedPageSize::allocate (bump)", |b| {
        let alloc = MallocFixedPageSize::<Item64>::new();
        b.iter(|| {
            black_box(alloc.allocate());
        });
    });
}

fn bench_alloc_free_reuse(c: &mut Criterion) {
    use faster_core::allocator::MallocFixedPageSize;

    #[repr(C)]
    struct Item64([u64; 8]);

    c.bench_function("MallocFixedPageSize::allocate+free (reuse)", |b| {
        let alloc = MallocFixedPageSize::<Item64>::new();
        // Pre-allocate one item and free it so the free list is populated.
        let addr = alloc.allocate();
        alloc.free(addr);
        b.iter(|| {
            let a = alloc.allocate();
            alloc.free(a);
            black_box(a);
        });
    });
}

// ---------------------------------------------------------------------------
// Overflow chain benchmarks
// ---------------------------------------------------------------------------

/// Helper: build a bucket chain of the given depth (0 = primary only).
fn build_chain(
    depth: usize,
) -> (
    faster_core::hash_bucket::HashBucket,
    faster_core::overflow::OverflowBucketPool,
) {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};

    let bucket = HashBucket::new();
    let pool = faster_core::overflow::OverflowBucketPool::new();

    // Fill primary bucket.
    let addr = LogicalAddress::new(Page(1), Offset(42));
    for i in 0..BUCKET_NUM_ENTRIES {
        let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
        bucket.entry(i).store(entry, Ordering::Relaxed);
    }

    // Fill overflow buckets.
    let mut current = &bucket;
    for d in 0..depth {
        let overflow = current.get_or_create_overflow(&pool);
        for i in 0..BUCKET_NUM_ENTRIES {
            let tag = ((d + 1) * BUCKET_NUM_ENTRIES + i) as u16 + 1;
            let entry = HashBucketEntry::new(tag, addr, false);
            overflow.entry(i).store(entry, Ordering::Relaxed);
        }
        current = overflow;
    }

    (bucket, pool)
}

fn bench_chain_traversal_depth0(c: &mut Criterion) {
    let (bucket, pool) = build_chain(0);
    c.bench_function("chain_traversal/depth=0", |b| {
        b.iter(|| {
            let mut count = 0u32;
            bucket.for_each_bucket(&pool, |_| {
                count += 1;
                core::ops::ControlFlow::<()>::Continue(())
            });
            black_box(count);
        });
    });
}

fn bench_chain_traversal_depth1(c: &mut Criterion) {
    let (bucket, pool) = build_chain(1);
    c.bench_function("chain_traversal/depth=1", |b| {
        b.iter(|| {
            let mut count = 0u32;
            bucket.for_each_bucket(&pool, |_| {
                count += 1;
                core::ops::ControlFlow::<()>::Continue(())
            });
            black_box(count);
        });
    });
}

fn bench_chain_traversal_depth2(c: &mut Criterion) {
    let (bucket, pool) = build_chain(2);
    c.bench_function("chain_traversal/depth=2", |b| {
        b.iter(|| {
            let mut count = 0u32;
            bucket.for_each_bucket(&pool, |_| {
                count += 1;
                core::ops::ControlFlow::<()>::Continue(())
            });
            black_box(count);
        });
    });
}

fn bench_chain_traversal_depth5(c: &mut Criterion) {
    let (bucket, pool) = build_chain(5);
    c.bench_function("chain_traversal/depth=5", |b| {
        b.iter(|| {
            let mut count = 0u32;
            bucket.for_each_bucket(&pool, |_| {
                count += 1;
                core::ops::ControlFlow::<()>::Continue(())
            });
            black_box(count);
        });
    });
}

fn bench_find_entry_in_chain(c: &mut Criterion) {
    use faster_core::hash_bucket::BUCKET_NUM_ENTRIES;

    let (bucket, pool) = build_chain(2);
    // Search for a tag in the last overflow bucket (worst-case traversal).
    let target_tag = (2 * BUCKET_NUM_ENTRIES + BUCKET_NUM_ENTRIES) as u16;
    c.bench_function("find_entry_in_chain/depth=2 (last)", |b| {
        b.iter(|| {
            black_box(bucket.find_entry_in_chain(black_box(target_tag), &pool));
        });
    });
}

fn bench_insert_in_chain(c: &mut Criterion) {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket};

    c.bench_function("insert_in_chain (overflow trigger)", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for _ in 0..iters {
                let bucket = HashBucket::new();
                let pool = faster_core::overflow::OverflowBucketPool::new();
                let addr = LogicalAddress::new(Page(1), Offset(42));
                // Fill primary + trigger one overflow.
                for i in 0..=BUCKET_NUM_ENTRIES {
                    let _ = bucket.insert_in_chain((i as u16) + 1, addr, &pool);
                }
                black_box(&bucket);
            }
            start.elapsed()
        });
    });
}

// ---------------------------------------------------------------------------
// Hash index benchmarks
// ---------------------------------------------------------------------------

/// Helper: create a KeyHash from a seed with good distribution.
fn make_bench_hash(seed: u64) -> faster_core::hash::KeyHash {
    faster_core::hash::KeyHash::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Helper: pre-populate a hash index with N committed entries. Returns the index.
fn prepopulate_index(log2_size: u32, n: u64) -> std::sync::Arc<faster_core::hash_index::HashIndex> {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::HashBucketEntry;

    let index = faster_core::hash_index::HashIndex::new(log2_size);
    let et = index.register_thread().unwrap();
    let _guard = et.protect();

    for i in 0..n {
        let hash = make_bench_hash(i);
        let addr = LogicalAddress::new(Page((i / 1000) as u32), Offset((i % 1000 + 2) as u32));
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        if r.created {
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            index.update(r.slot, r.entry, committed);
        }
    }

    std::sync::Arc::new(index)
}

fn bench_hash_index_insert_sequential(c: &mut Criterion) {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::HashBucketEntry;
    use faster_core::hash_index::HashIndex;

    c.bench_function("hash_index/insert_sequential", |b| {
        b.iter_custom(|iters| {
            let index = HashIndex::new(20); // 1M buckets
            let et = index.register_thread().unwrap();
            let _guard = et.protect();

            let start = std::time::Instant::now();
            for i in 0..iters {
                let hash = make_bench_hash(i);
                let addr = LogicalAddress::new(Page(0), Offset((i as u32 & 0x01FF_FFFF).max(2)));
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                if r.created {
                    let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                    index.update(r.slot, r.entry, committed);
                }
                black_box(&r.entry);
            }
            start.elapsed()
        });
    });
}

fn bench_hash_index_find_hit(c: &mut Criterion) {
    let n = 100_000u64;
    let index = prepopulate_index(20, n);
    let et = index.register_thread().unwrap();

    c.bench_function("hash_index/find_hit", |b| {
        let _guard = et.protect();
        let mut key = 0u64;
        b.iter(|| {
            key = (key + 1) % n;
            let hash = make_bench_hash(key);
            black_box(index.find(hash));
        });
    });
}

fn bench_hash_index_find_miss(c: &mut Criterion) {
    let n = 100_000u64;
    let index = prepopulate_index(20, n);
    let et = index.register_thread().unwrap();

    c.bench_function("hash_index/find_miss", |b| {
        let _guard = et.protect();
        let mut key = n + 1_000_000; // guaranteed misses
        b.iter(|| {
            key = key.wrapping_add(1);
            let hash = make_bench_hash(key);
            black_box(index.find(hash));
        });
    });
}

fn bench_hash_index_insert_concurrent(c: &mut Criterion) {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::HashBucketEntry;
    use faster_core::hash_index::HashIndex;

    let mut group = c.benchmark_group("hash_index/insert_concurrent");
    group.throughput(criterion::Throughput::Elements(1));

    for num_threads in [2, 4, 8] {
        group.bench_with_input(
            criterion::BenchmarkId::new("threads", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_custom(|iters| {
                    let index = std::sync::Arc::new(HashIndex::new(20));
                    let barrier = std::sync::Arc::new(std::sync::Barrier::new(num_threads));
                    let per_thread = (iters / num_threads as u64).max(1);

                    let start = std::time::Instant::now();
                    let handles: Vec<_> = (0..num_threads)
                        .map(|tid| {
                            let index = std::sync::Arc::clone(&index);
                            let barrier = std::sync::Arc::clone(&barrier);
                            std::thread::spawn(move || {
                                let et = index.register_thread().unwrap();
                                let _guard = et.protect();
                                barrier.wait();

                                let base = tid as u64 * per_thread + 1_000_000;
                                for i in 0..per_thread {
                                    let seed = base + i;
                                    let hash = make_bench_hash(seed);
                                    let addr = LogicalAddress::new(
                                        Page(tid as u32),
                                        Offset((i as u32 & 0x01FF_FFFF).max(2)),
                                    );
                                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                                    if r.created {
                                        let committed =
                                            HashBucketEntry::new(r.entry.tag(), addr, false);
                                        index.update(r.slot, r.entry, committed);
                                    }
                                }
                            })
                        })
                        .collect();

                    for h in handles {
                        h.join().unwrap();
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

fn bench_hash_index_find_concurrent(c: &mut Criterion) {
    let n = 200_000u64;
    let index = prepopulate_index(20, n);

    let mut group = c.benchmark_group("hash_index/find_concurrent");
    group.throughput(criterion::Throughput::Elements(1));

    for num_threads in [2, 4, 8] {
        group.bench_with_input(
            criterion::BenchmarkId::new("threads", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_custom(|iters| {
                    let barrier = std::sync::Arc::new(std::sync::Barrier::new(num_threads));
                    let per_thread = (iters / num_threads as u64).max(1);

                    let start = std::time::Instant::now();
                    let handles: Vec<_> = (0..num_threads)
                        .map(|tid| {
                            let index = std::sync::Arc::clone(&index);
                            let barrier = std::sync::Arc::clone(&barrier);
                            std::thread::spawn(move || {
                                let et = index.register_thread().unwrap();
                                let _guard = et.protect();
                                barrier.wait();

                                let base = tid as u64 * per_thread % n;
                                for i in 0..per_thread {
                                    let seed = (base + i) % n;
                                    let hash = make_bench_hash(seed);
                                    black_box(index.find(hash));
                                }
                            })
                        })
                        .collect();

                    for h in handles {
                        h.join().unwrap();
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

fn bench_hash_index_mixed_rw_concurrent_8(c: &mut Criterion) {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::HashBucketEntry;

    let n = 200_000u64;
    let index = prepopulate_index(20, n);
    let num_threads = 8usize;

    c.bench_function("hash_index/mixed_rw_concurrent_8t", |b| {
        b.iter_custom(|iters| {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(num_threads));
            let per_thread = (iters / num_threads as u64).max(1);

            let start = std::time::Instant::now();
            let handles: Vec<_> = (0..num_threads)
                .map(|tid| {
                    let index = std::sync::Arc::clone(&index);
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let et = index.register_thread().unwrap();
                        let _guard = et.protect();
                        barrier.wait();

                        let base = tid as u64 * per_thread;
                        for i in 0..per_thread {
                            let seed = base + i;
                            let hash = make_bench_hash(seed);

                            if i % 5 == 0 {
                                // 20% writes
                                let addr = LogicalAddress::new(
                                    Page(tid as u32 + 100),
                                    Offset((i as u32 & 0x01FF_FFFF).max(2)),
                                );
                                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                                if r.created {
                                    let committed =
                                        HashBucketEntry::new(r.entry.tag(), addr, false);
                                    index.update(r.slot, r.entry, committed);
                                }
                            } else {
                                // 80% reads
                                black_box(index.find(hash));
                            }
                        }
                    })
                })
                .collect();

            for h in handles {
                h.join().unwrap();
            }
            start.elapsed()
        });
    });
}

// ---------------------------------------------------------------------------
// Compaction benchmarks — SC-009 (fixed-size regression) and SC-005
// (variable-length overhead curve)
// ---------------------------------------------------------------------------

/// Benchmark `serialized_size_from_bytes` for fixed-size u64 keys.
///
/// SC-009 gate: verifies LLVM const-folds this to a no-op for fixed types.
fn bench_compaction_size_from_bytes_u64(c: &mut Criterion) {
    use faster_core::record::Key;

    let buf = 42u64.to_le_bytes();
    c.bench_function("compaction/serialized_size_from_bytes/u64", |b| {
        b.iter(|| {
            black_box(<u64 as Key>::serialized_size_from_bytes(black_box(&buf)));
        });
    });
}

/// Benchmark `serialized_size_from_bytes` for variable-length Vec<u8> keys
/// at several record sizes to establish the overhead curve (SC-005).
fn bench_compaction_size_from_bytes_variable(c: &mut Criterion) {
    use faster_core::record::Key;

    let mut group = c.benchmark_group("compaction/serialized_size_from_bytes/Vec<u8>");

    for payload_size in [16, 64, 256, 1024, 10240] {
        let key: Vec<u8> = vec![0xAB; payload_size];
        let mut buf = vec![0u8; key.serialized_size()];
        key.serialize(&mut buf);

        group.bench_with_input(
            criterion::BenchmarkId::new("payload", payload_size),
            &payload_size,
            |b, _| {
                b.iter(|| {
                    black_box(<Vec<u8> as Key>::serialized_size_from_bytes(black_box(
                        &buf,
                    )));
                });
            },
        );
    }
    group.finish();
}

/// Benchmark `eq_from_bytes` for fixed-size u64 keys (should be ~memcmp).
fn bench_compaction_eq_from_bytes_u64(c: &mut Criterion) {
    use faster_core::record::Key;

    let key: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let buf = key.to_le_bytes();
    c.bench_function("compaction/eq_from_bytes/u64", |b| {
        b.iter(|| {
            black_box(key.eq_from_bytes(black_box(&buf)));
        });
    });
}

/// Benchmark `eq_from_bytes` for variable-length Vec<u8> at various sizes.
fn bench_compaction_eq_from_bytes_variable(c: &mut Criterion) {
    use faster_core::record::Key;

    let mut group = c.benchmark_group("compaction/eq_from_bytes/Vec<u8>");

    for payload_size in [16, 64, 256, 1024, 10240] {
        let key: Vec<u8> = vec![0xAB; payload_size];
        let mut buf = vec![0u8; key.serialized_size()];
        key.serialize(&mut buf);

        group.bench_with_input(
            criterion::BenchmarkId::new("payload", payload_size),
            &payload_size,
            |b, _| {
                b.iter(|| {
                    black_box(key.eq_from_bytes(black_box(&buf)));
                });
            },
        );
    }
    group.finish();
}

/// Benchmark `record_size_from_bytes` for fixed-size u64/u64 records.
///
/// SC-009 gate: fixed-size compaction size discovery should have near-zero cost.
fn bench_compaction_record_size_u64(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, record_size_from_bytes, write_record};

    let key: u64 = 0xDEAD_BEEF;
    let value: u64 = 0xCAFE_BABE;
    let layout = RecordLayout::for_kv(&key, &value);
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let mut page = vec![0u8; 4096];
    write_record(&mut page, &info, &key, &value, &layout);

    c.bench_function("compaction/record_size_from_bytes/u64+u64", |b| {
        b.iter(|| {
            let _ = black_box(record_size_from_bytes::<u64, u64>(
                black_box(&page),
                0,
                4096,
            ));
        });
    });
}

/// Benchmark `record_size_from_bytes` for variable-length records at
/// several sizes to document the overhead curve (SC-005).
fn bench_compaction_record_size_variable(c: &mut Criterion) {
    use faster_core::address::LogicalAddress;
    use faster_core::record::{RecordInfo, RecordLayout, record_size_from_bytes, write_record};

    let mut group = c.benchmark_group("compaction/record_size_from_bytes/Vec<u8>");

    for payload_size in [16, 64, 256, 1024, 10240] {
        let key: Vec<u8> = vec![0xAB; payload_size];
        let value: Vec<u8> = vec![0xCD; payload_size];
        let layout = RecordLayout::for_kv(&key, &value);
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let total = layout.total_size();
        let mut page = vec![0u8; total.max(4096)];
        write_record(&mut page, &info, &key, &value, &layout);

        group.bench_with_input(
            criterion::BenchmarkId::new("payload", payload_size),
            &payload_size,
            |b, _| {
                b.iter(|| {
                    let _ = black_box(record_size_from_bytes::<Vec<u8>, Vec<u8>>(
                        black_box(&page),
                        0,
                        page.len(),
                    ));
                });
            },
        );
    }
    group.finish();
}
