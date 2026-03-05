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
    use faster_core::address::{LogicalAddress, Page, Offset};
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
    use faster_core::address::{LogicalAddress, Page, Offset};
    use faster_core::hash_bucket::{HashBucketEntry, AtomicHashBucketEntry};
    use core::sync::atomic::Ordering;

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
    use faster_core::address::{LogicalAddress, Page, Offset};
    use faster_core::hash_bucket::{HashBucketEntry, HashBucket, BUCKET_NUM_ENTRIES};
    use core::sync::atomic::Ordering;

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
    use faster_core::address::{LogicalAddress, Page, Offset};
    use faster_core::hash_bucket::{HashBucketEntry, HashBucket, BUCKET_NUM_ENTRIES};
    use core::sync::atomic::Ordering;

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
    use faster_core::address::{LogicalAddress, Page, Offset};
    use faster_core::hash_bucket::{HashBucketEntry, HashBucket, BUCKET_NUM_ENTRIES};
    use core::sync::atomic::Ordering;

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
    use faster_core::address::{LogicalAddress, Page, Offset};
    use faster_core::hash_bucket::{HashBucketEntry, HashBucket};
    use core::sync::atomic::Ordering;

    c.bench_function("HashBucket::try_insert+reset", |b| {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(42));
        b.iter(|| {
            // Insert into slot 0 then reset — measures CAS insert cost
            let _ = bucket.try_insert(black_box(0x1234), black_box(addr));
            bucket.entry(0).store(HashBucketEntry::EMPTY, Ordering::Relaxed);
        });
    });
}

criterion_group!(
    benches,
    bench_hash_u64,
    bench_hash_bytes_short,
    bench_hash_bytes_medium,
    bench_hashable_u64,
    bench_hashable_str,
    bench_tag_extraction,
    bench_entry_new,
    bench_entry_tag_extraction,
    bench_entry_address_extraction,
    bench_atomic_compare_exchange,
    bench_bucket_find_entry_miss,
    bench_bucket_find_entry_hit_first,
    bench_bucket_find_entry_hit_last,
    bench_bucket_try_insert,
    bench_alloc_bump,
    bench_alloc_free_reuse,
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
