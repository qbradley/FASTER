use criterion::{black_box, criterion_group, criterion_main, Criterion};

// ---------------------------------------------------------------------------
// End-to-end pipeline benchmarks: hash → bucket → allocator → record
// These complement the micro-benchmarks in faster-core/benches/ by measuring
// the full pipeline cost that a real FASTER operation would incur.
// ---------------------------------------------------------------------------

/// 128-byte item for allocator benchmarks.
#[repr(C)]
struct RecordSlot {
    data: [u8; 128],
}

fn bench_pipeline_insert(c: &mut Criterion) {
    use std::sync::Arc;
    use faster_core::address::LogicalAddress;
    use faster_core::allocator::MallocFixedPageSize;
    use faster_core::epoch::EpochTable;
    use faster_core::hash::Hashable;
    use faster_core::record::{write_record, RecordInfo, RecordLayout};

    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");
    let alloc = MallocFixedPageSize::<RecordSlot>::new();

    c.bench_function("pipeline/insert (epoch+hash+alloc+write)", |b| {
        let mut key_counter = 0u64;
        b.iter(|| {
            let _guard = thread.protect();

            let key = key_counter;
            key_counter += 1;
            let value: u64 = key.wrapping_mul(31);

            let kh = key.hash();
            let _tag = kh.tag();
            let _bucket_idx = kh.index(1024);

            let addr = alloc.allocate();
            let layout = RecordLayout::for_kv(&key, &value);
            let slot = alloc.get(addr);
            let buf = unsafe {
                std::slice::from_raw_parts_mut(slot as *const _ as *mut u8, 128)
            };
            let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
            write_record(buf, &info, &key, &value, &layout);

            black_box(addr);
        });
    });
}

fn bench_pipeline_lookup(c: &mut Criterion) {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use faster_core::address::LogicalAddress;
    use faster_core::allocator::MallocFixedPageSize;
    use faster_core::epoch::EpochTable;
    use faster_core::hash::Hashable;
    use faster_core::hash_bucket::{HashBucket, HashBucketEntry};
    use faster_core::record::{write_record, read_key, read_value, RecordInfo, RecordLayout};

    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");
    let alloc = MallocFixedPageSize::<RecordSlot>::new();
    let bucket = HashBucket::new();

    // Pre-populate 7 entries.
    let keys: Vec<u64> = (100..107).collect();
    for &key in &keys {
        let kh = key.hash();
        let addr = alloc.allocate();
        let layout = RecordLayout::for_kv(&key, &key);
        let slot = alloc.get(addr);
        let buf = unsafe {
            std::slice::from_raw_parts_mut(slot as *const _ as *mut u8, 128)
        };
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        write_record(buf, &info, &key, &key, &layout);
        let entry = HashBucketEntry::new(kh.tag(), addr, false);
        if let Some(slot) = bucket.find_empty() {
            bucket
                .entry(slot)
                .store(entry, Ordering::Release);
        }
    }

    c.bench_function("pipeline/lookup (epoch+find+read)", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let _guard = thread.protect();

            let key = keys[idx % keys.len()];
            idx += 1;

            let kh = key.hash();
            let tag = kh.tag();

            if let Some((_, entry)) = bucket.find_entry(tag) {
                let slot = alloc.get(entry.address());
                let buf = unsafe {
                    std::slice::from_raw_parts(slot as *const _ as *const u8, 128)
                };
                let layout = RecordLayout::for_kv(&key, &key);
                let k: u64 = read_key(buf, &layout);
                let v: u64 = read_value(buf, &layout);
                black_box((k, v));
            }
        });
    });
}

fn bench_epoch_contended(c: &mut Criterion) {
    use std::sync::Arc;
    use std::thread;
    use faster_core::epoch::EpochTable;

    let mut group = c.benchmark_group("epoch_contended");

    // Measure protect/unprotect latency while background threads are also
    // doing epoch operations (simulating contention).
    for bg_threads in [0, 2, 4] {
        group.bench_with_input(
            criterion::BenchmarkId::new("protect_unprotect", bg_threads),
            &bg_threads,
            |b, &bg_threads| {
                let table = Arc::new(EpochTable::new());
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

                let bg_handles: Vec<_> = (0..bg_threads)
                    .map(|_| {
                        let table = Arc::clone(&table);
                        let stop = Arc::clone(&stop);
                        thread::spawn(move || {
                            let et = table.register().expect("register");
                            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                                let guard = et.protect();
                                guard.refresh();
                                drop(guard);
                                table.bump_current_epoch_no_callback();
                            }
                        })
                    })
                    .collect();

                let thread = table.register().expect("register");
                b.iter(|| {
                    let guard = thread.protect();
                    black_box(guard.epoch());
                    drop(guard);
                });

                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                for h in bg_handles {
                    h.join().ok();
                }
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_pipeline_insert,
    bench_pipeline_lookup,
    bench_epoch_contended,
);
criterion_main!(benches);

