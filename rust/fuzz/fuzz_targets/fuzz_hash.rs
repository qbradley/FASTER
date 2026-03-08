//! Fuzz target: hash function robustness.
//!
//! Feeds arbitrary data into all `Hashable` implementations and verifies
//! the hash functions never panic. Also checks `KeyHash` accessor
//! invariants (tag range, index range) for every produced hash.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use faster_core::hash::{Hashable, KeyHash};

#[derive(Arbitrary, Debug)]
struct HashInput {
    bytes: Vec<u8>,
    u64_val: u64,
    u32_val: u32,
    i64_val: i64,
    i32_val: i32,
    table_size_log2: u8,
}

fuzz_target!(|input: HashInput| {
    // ── Hash u64 ─────────────────────────────────────────────────────
    let h = input.u64_val.hash();
    verify_key_hash(h);

    // ── Hash u32 ─────────────────────────────────────────────────────
    let h = input.u32_val.hash();
    verify_key_hash(h);

    // ── Hash i64 ─────────────────────────────────────────────────────
    let h = input.i64_val.hash();
    verify_key_hash(h);

    // ── Hash i32 ─────────────────────────────────────────────────────
    let h = input.i32_val.hash();
    verify_key_hash(h);

    // ── Hash [u8] slice ──────────────────────────────────────────────
    let h = input.bytes.as_slice().hash();
    verify_key_hash(h);

    // ── Hash Vec<u8> ─────────────────────────────────────────────────
    let h = input.bytes.hash();
    verify_key_hash(h);

    // ── Hash String (if valid UTF-8) ─────────────────────────────────
    if let Ok(s) = std::str::from_utf8(&input.bytes) {
        let h = s.hash();
        verify_key_hash(h);
        // String and &str must produce the same hash.
        let owned = s.to_string();
        let h2 = owned.hash();
        assert_eq!(h.value(), h2.value(), "String and &str hashes must match");
    }

    // ── Hash fixed-size byte arrays ──────────────────────────────────
    if input.bytes.len() >= 16 {
        let arr: [u8; 16] = input.bytes[..16].try_into().unwrap();
        let h = arr.hash();
        verify_key_hash(h);
    }

    // ── Determinism: same input → same hash ──────────────────────────
    let h1 = input.u64_val.hash();
    let h2 = input.u64_val.hash();
    assert_eq!(h1.value(), h2.value(), "hash must be deterministic");

    let h1 = input.bytes.as_slice().hash();
    let h2 = input.bytes.as_slice().hash();
    assert_eq!(h1.value(), h2.value(), "hash must be deterministic");

    // ── KeyHash::index with various table sizes ──────────────────────
    // table_size must be a power of two; clamp log2 to [1, 48].
    let log2 = (input.table_size_log2 as u32 % 48).max(1);
    let table_size = 1u64 << log2;
    let h = input.u64_val.hash();
    let idx = h.index(table_size);
    assert!(
        idx < table_size,
        "index {} must be < table_size {}",
        idx,
        table_size
    );
});

/// Verifies invariants on a `KeyHash` that must hold for any input.
fn verify_key_hash(h: KeyHash) {
    // Tag is 14 bits (0..16383).
    let tag = h.tag();
    assert!(tag < (1 << 14), "tag {} must fit in 14 bits", tag);

    // index(1) must always be 0 (hash & 0 == 0).
    assert_eq!(h.index(1), 0, "index(1) must be 0");

    // index(power_of_two) must be < that power.
    for log2 in [1, 4, 10, 20, 30, 48] {
        let size = 1u64 << log2;
        let idx = h.index(size);
        assert!(idx < size, "index({}) = {} must be < {}", size, idx, size);
    }

    // value() must be consistent.
    let _ = h.value();
}
