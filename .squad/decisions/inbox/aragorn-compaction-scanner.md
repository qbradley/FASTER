# Decision: Compaction Scanner Module (Wave 1, K1)

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** IMPLEMENTED
**Impact:** New internal module — no public API changes

## Decision

Add `compaction::CompactionScanner` that walks a hybrid log region and classifies each record as live, dead, or tombstoned by consulting the hash index. Returns a `CompactionPlan` containing per-record liveness info plus aggregate counts.

## Rationale

Compaction needs to know which records are still the current version before it can copy live data and reclaim space. The scanner is the read-only analysis phase that feeds into later compaction stages (copy, address-update, truncation).

## Design Choices

### Stateless function over iterator
`scan()` is a standalone function, not a streaming iterator. Compaction is a batch operation — the full plan is needed before any records are moved. This avoids lifetime complexity and makes the API trivial to use.

### Conservative liveness classification
Key invariant: **never classify a live record as dead**. False positives (dead classified as live) waste copy bandwidth but are safe. False negatives (live classified as dead) cause data loss. When the hash index has no entry for a key or chain walk fails to resolve, the scanner assumes LIVE.

### Hash index chain walk
Mirrors `store::operations::find_record_for_key()` — walks the version chain from the hash entry, skipping invalid records, until it finds the first key-matching record. If that record's address matches the scanned record's address, it's LIVE; otherwise DEAD.

### No null-record skipping
`RecordInfo(0)` (version 0, no flags, previous_address = ZERO) is indistinguishable from zeroed memory. Rather than trying to detect unwritten pages, the scanner requires callers to provide a record-aligned begin address via `FasterKv::first_data_address()`, which skips the 8-byte sentinel.

### `#[deny(unsafe_code)]` on module
The compaction module is pure safe Rust. All unsafe operations (physical address translation, page mapping) are encapsulated in `RecordAccessor` and `HybridLogAllocator`.

## Key Types

- `CompactionPlan { live, dead, tombstoned, records: Vec<LiveRecord> }`
- `LiveRecord { address, record_size, status }` where status ∈ {Live, Dead, Tombstone}
- `CompactionScanner` — namespace struct with `scan<K: Key>()` method

## Test Coverage

- 10 unit tests: empty log, single record, multiple records, updates creating dead records, tombstones, mixed updates+deletes, subranges, plan computed fields
- 1 proptest: random upsert/delete sequences → live count matches hash index entry count
- Proptest regression seed committed for shrunk failure case

## Risks

- Scanner assumes epoch protection is held by caller (documented but not enforced at type level)
- Classification accuracy depends on hash index state at scan time — concurrent mutations during scan may cause stale classifications (acceptable for compaction which runs on frozen regions)
