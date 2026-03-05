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

criterion_group!(
    benches,
    bench_hash_u64,
    bench_hash_bytes_short,
    bench_hash_bytes_medium,
    bench_hashable_u64,
    bench_hashable_str,
    bench_tag_extraction,
);
criterion_main!(benches);
