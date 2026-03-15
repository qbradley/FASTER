//! Fuzz target: PageTrailer CRC computation and verification.
//!
//! Feeds arbitrary bytes through `PageTrailer::from_bytes`, `from_slice`,
//! `write_size`, and `crc_range`. Verifies roundtrip consistency: a
//! trailer created from known CRC data must survive serialization and
//! deserialization without corruption. Targets the same parsing path
//! that `validate_page_checksums` uses during log recovery.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use faster_core::hybrid_log::page::PageTrailer;

#[derive(Arbitrary, Debug)]
struct TrailerInput {
    /// Raw 8-byte trailer (valid_bytes LE + crc32 LE).
    raw: [u8; 8],
    /// Page data for from_slice / CRC roundtrip testing.
    page_data: Vec<u8>,
    /// Fuzzer-controlled write_size for from_slice.
    write_size: u16,
    /// Fuzzer-controlled inputs for write_size / crc_range computation.
    valid_bytes: u32,
    sector_size: u32,
    page_size: u32,
}

fuzz_target!(|input: TrailerInput| {
    // ── from_bytes: any 8 bytes must parse without panic ─────────────
    let trailer = PageTrailer::from_bytes(input.raw);
    let _ = trailer.valid_bytes;
    let _ = trailer.crc32;

    // ── Roundtrip: from_bytes → to_bytes → from_bytes ────────────────
    let bytes = trailer.to_bytes();
    let roundtripped = PageTrailer::from_bytes(bytes);
    assert_eq!(trailer, roundtripped, "from_bytes → to_bytes roundtrip failed");

    // ── from_slice: extracts trailer from the last 8 bytes of a page ─
    // Only test when preconditions are met (from_slice asserts).
    let ws = input.write_size as usize;
    if ws >= PageTrailer::SIZE && ws <= input.page_data.len() {
        let t = PageTrailer::from_slice(&input.page_data, ws);
        let _ = t.valid_bytes;
        let _ = t.crc32;
    }

    // ── CRC roundtrip: compute CRC, embed in trailer, verify ─────────
    // This simulates what the flush path does: compute CRC over valid
    // bytes, write trailer at end of page, then recovery reads it back.
    if !input.page_data.is_empty() && input.page_data.len() >= PageTrailer::SIZE {
        let data_len = input.page_data.len() - PageTrailer::SIZE;
        if data_len > 0 {
            let crc = crc32fast::hash(&input.page_data[..data_len]);
            let trailer = PageTrailer::new(data_len as u32, crc);

            // Write trailer into a copy of the page.
            let mut page = input.page_data.clone();
            let trailer_bytes = trailer.to_bytes();
            page[data_len..data_len + PageTrailer::SIZE]
                .copy_from_slice(&trailer_bytes);

            // Read it back — must match.
            let recovered = PageTrailer::from_slice(&page, page.len());
            assert_eq!(recovered.valid_bytes, data_len as u32);
            assert_eq!(recovered.crc32, crc);

            // Verify CRC against the data.
            let actual_crc = crc32fast::hash(&page[..recovered.valid_bytes as usize]);
            assert_eq!(actual_crc, recovered.crc32, "CRC mismatch after roundtrip");
        }
    }

    // ── write_size: must not panic on any inputs ─────────────────────
    // Clamp to avoid degenerate values that would just overflow.
    let sector = input.sector_size.max(1).min(1 << 20);
    let page = input.page_size.max(1).min(1 << 26);
    let valid = input.valid_bytes.min(page);
    let _ = PageTrailer::write_size(valid, sector, page);

    // ── crc_range: must not panic on any inputs ──────────────────────
    let ws_computed = PageTrailer::write_size(valid, sector, page);
    if ws_computed > 0 {
        let _ = PageTrailer::crc_range(valid, ws_computed);
    }

    // ── new() + to_bytes() consistency ───────────────────────────────
    let t = PageTrailer::new(input.valid_bytes, trailer.crc32);
    let b = t.to_bytes();
    let t2 = PageTrailer::from_bytes(b);
    assert_eq!(t.valid_bytes, t2.valid_bytes);
    assert_eq!(t.crc32, t2.crc32);
});
