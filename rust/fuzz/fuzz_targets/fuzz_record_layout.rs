//! Fuzz target: record layout computation with adversarial sizes.
//!
//! Feeds arbitrary key_size and value_size pairs into `RecordLayout::compute`
//! and `pad_alignment` to look for integer overflow, underflow, or any
//! violation of the layout invariants (8-byte alignment, key_offset == 8,
//! value_offset >= key_offset + key_size, total_size >= value_offset + value_size).

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use faster_core::record::{pad_alignment, RecordLayout, RECORD_ALIGNMENT, RECORD_HEADER_SIZE};

#[derive(Arbitrary, Debug)]
struct LayoutInput {
    key_size: usize,
    value_size: usize,
}

fuzz_target!(|input: LayoutInput| {
    // Reject absurd sizes that would exhaust memory but are not
    // interesting from an arithmetic perspective.
    if input.key_size > 1 << 28 || input.value_size > 1 << 28 {
        return;
    }

    let layout = RecordLayout::compute(input.key_size, input.value_size);

    // ── Invariant checks ─────────────────────────────────────────────
    // key_offset must always be 8 (RecordInfo is 8 bytes, alignment is 8).
    assert_eq!(
        layout.key_offset(),
        RECORD_HEADER_SIZE,
        "key_offset must be RECORD_HEADER_SIZE (8)"
    );

    // value_offset must be >= key_offset + key_size, and 8-byte aligned.
    assert!(
        layout.value_offset() >= layout.key_offset() + input.key_size,
        "value_offset {} must be >= key_offset {} + key_size {}",
        layout.value_offset(),
        layout.key_offset(),
        input.key_size,
    );
    assert_eq!(
        layout.value_offset() % RECORD_ALIGNMENT,
        0,
        "value_offset must be 8-byte aligned"
    );

    // total_size must be >= value_offset + value_size, and 8-byte aligned.
    assert!(
        layout.total_size() >= layout.value_offset() + input.value_size,
        "total_size {} must be >= value_offset {} + value_size {}",
        layout.total_size(),
        layout.value_offset(),
        input.value_size,
    );
    assert_eq!(
        layout.total_size() % RECORD_ALIGNMENT,
        0,
        "total_size must be 8-byte aligned"
    );

    // ── pad_alignment correctness ────────────────────────────────────
    for &size in &[input.key_size, input.value_size, layout.total_size()] {
        let aligned = pad_alignment(size, RECORD_ALIGNMENT);
        assert!(aligned >= size, "pad_alignment must not shrink");
        assert_eq!(aligned % RECORD_ALIGNMENT, 0, "pad_alignment must align");
        // Must be the smallest aligned value >= size.
        if size % RECORD_ALIGNMENT == 0 {
            assert_eq!(aligned, size, "already-aligned size must not grow");
        } else {
            assert_eq!(
                aligned,
                size + (RECORD_ALIGNMENT - size % RECORD_ALIGNMENT),
                "pad_alignment must produce next alignment boundary"
            );
        }
    }
});
