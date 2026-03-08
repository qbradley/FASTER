//! Fuzz target: compaction scanner record size discovery.
//!
//! Feeds a synthetic page filled with random bytes into
//! `record_size_from_bytes` for multiple type combinations.
//! The function must never panic or corrupt memory — it should
//! gracefully return `RecordSizeError` on malformed input.
//! (Relates to SF-8 corruption cascade fix.)

#![no_main]

use libfuzzer_sys::fuzz_target;

use faster_core::record::{
    record_size_from_bytes, RecordSizeError, RECORD_ALIGNMENT, RECORD_HEADER_SIZE,
};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let page_size = data.len();

    // ── Test with u64 key / u64 value (fixed-size) ───────────────────
    // Fixed-size types always return compile-time constants for
    // serialized_size_from_bytes, so corruption is caught by the
    // page-boundary check.
    match record_size_from_bytes::<u64, u64>(data, 0, page_size) {
        Ok(size) => {
            assert!(size > 0, "record size must be positive");
            assert!(
                size <= page_size,
                "record size must fit within the page"
            );
            assert_eq!(
                size % RECORD_ALIGNMENT,
                0,
                "record size must be 8-byte aligned"
            );
            assert!(
                size >= RECORD_HEADER_SIZE + 8 + 8,
                "u64/u64 record must be at least header + 8 + 8"
            );
        }
        Err(RecordSizeError::InsufficientPageSpace) => {
            // Expected for pages smaller than MIN_READABLE (12 bytes).
        }
        Err(RecordSizeError::CorruptedRecord { offset }) => {
            assert_eq!(offset, 0, "we scanned from offset 0");
        }
    }

    // ── Test with Vec<u8> key / Vec<u8> value (variable-length) ──────
    // Variable-length types read a 4-byte length prefix from the buffer.
    // A malicious prefix can claim gigabytes of data. The function must
    // detect this via page-boundary validation and return CorruptedRecord.
    match record_size_from_bytes::<Vec<u8>, Vec<u8>>(data, 0, page_size) {
        Ok(size) => {
            assert!(size > 0, "record size must be positive");
            assert!(
                size <= page_size,
                "record size must fit within the page"
            );
            assert_eq!(
                size % RECORD_ALIGNMENT,
                0,
                "record size must be 8-byte aligned"
            );
        }
        Err(RecordSizeError::InsufficientPageSpace) => {}
        Err(RecordSizeError::CorruptedRecord { .. }) => {}
    }

    // ── Test with u32 key / Vec<u8> value (mixed) ────────────────────
    match record_size_from_bytes::<u32, Vec<u8>>(data, 0, page_size) {
        Ok(size) => {
            assert!(size > 0);
            assert!(size <= page_size);
            assert_eq!(size % RECORD_ALIGNMENT, 0);
        }
        Err(_) => {}
    }

    // ── Scan at multiple offsets within the page ─────────────────────
    // Simulates the compaction scanner walking through a page with
    // records at various positions.
    let mut offset = 0;
    let mut records_found = 0;
    while offset < page_size {
        match record_size_from_bytes::<u64, u64>(data, offset, page_size) {
            Ok(size) => {
                assert!(size > 0);
                assert!(offset + size <= page_size);
                assert_eq!(size % RECORD_ALIGNMENT, 0);
                offset += size;
                records_found += 1;
                if records_found > 1000 {
                    break; // avoid excessive iteration on degenerate input
                }
            }
            Err(_) => break,
        }
    }
});
