# Feature Specification: Variable-Length Record Compaction

**Branch**: feature/variable-length-compaction  |  **Created**: 2025-07-15  |  **Status**: Draft
**Input Brief**: Extend the FASTER Rust compaction pipeline to support variable-length keys and values

## Overview

FASTER's hybrid log is an append-only structure where updates create new records at the tail rather than modifying existing ones in place. Over time, the log accumulates dead records — superseded versions, tombstones from deletions, and invalidated entries — that waste both disk space and I/O bandwidth. The compaction pipeline reclaims this space by identifying live records, copying them to the log tail, updating the hash index to point at the new locations, and truncating the old region from disk.

Today, compaction works exclusively with fixed-size keys and values. The scanner iterates records using a constant stride (computed once from `K::SIZE` and `V::SIZE`), the copier allocates uniform slots, and the public API (`FasterKv::compact()`) requires `FixedSizeKey + FixedSizeValue` trait bounds. This means any store using `Vec<u8>`, `String`, or other variable-length types cannot reclaim space — the log grows without bound.

Variable-length workloads are common in real-world deployments: document stores with JSON values, caches with heterogeneous payloads, and key-value stores with string keys of varying length. Without compaction support, these workloads face unbounded disk growth and degrading read performance as the log lengthens. Extending the compaction pipeline to handle variable-length records is essential for production readiness.

The core technical challenge is record size discovery. Unlike fixed-size records where the stride is a compile-time constant, variable-length records encode their sizes within the record itself using length prefixes. The scanner must read each record's key length prefix to compute the key size, derive the value offset, then read the value length prefix to compute the total record size before advancing to the next record. This per-record size computation changes every phase of the pipeline: scanning, copying, address updating, and orchestration.

## Objectives

- Enable space reclamation for stores using variable-length keys and/or values, preventing unbounded log growth
- Maintain the existing four-phase compaction architecture (scan → copy → swing → truncate) without structural changes
- Preserve the correctness guarantees of the fixed-size pipeline: no data loss, no dangling pointers, epoch-safe memory reclamation
- Support mixed workloads where key sizes vary dramatically (e.g., 4-byte integers to 64KB blobs) within the same compaction cycle
- Achieve compaction throughput within 2× of fixed-size compaction for equivalent data volumes (the per-record length-prefix reads are the expected overhead)
- Maintain full backward compatibility — fixed-size compaction continues to work identically

## User Scenarios & Testing

### User Story P1 – Compact a Variable-Length Store
Narrative: A developer creates a FASTER store with `Vec<u8>` keys and `Vec<u8>` values, performs millions of upserts creating dead records, then calls compact to reclaim space. After compaction, all live records are readable and disk usage has decreased.

Independent Test: Insert 10K records, overwrite 8K of them, compact, verify all 10K latest values are correct and begin_address has advanced.

Acceptance Scenarios:
1. Given a store with 10,000 `Vec<u8>` key-value pairs where 80% have been overwritten, When `compact()` is called, Then all 10,000 latest values are retrievable with correct content
2. Given a store after variable-length compaction, When `begin_address` is inspected, Then it has advanced past the compacted region
3. Given a store with variable-length records spanning multiple pages, When `compact()` is called, Then records that straddle page boundaries are correctly handled

### User Story P2 – Compact with Tombstones
Narrative: A developer deletes a significant fraction of variable-length records and compacts. Tombstoned entries are removed from the hash index, and their space is reclaimed.

Independent Test: Insert 5K records, delete 3K, compact, verify deleted keys return NotFound and remaining 2K are correct.

Acceptance Scenarios:
1. Given 3,000 tombstoned variable-length records, When compaction runs, Then those hash index entries are cleared and begin_address advances past them
2. Given a mix of tombstoned and live variable-length records on the same page, When compaction scans, Then live records are copied and tombstones are skipped

### User Story P3 – Compact with Mixed Record Sizes
Narrative: A store contains records ranging from 20 bytes to 100KB. Compaction correctly handles the full size spectrum, packing records of varying sizes at the log tail without corruption.

Independent Test: Insert records with value sizes [1B, 100B, 10KB, 100KB], overwrite half, compact, verify all live records have correct content and size.

Acceptance Scenarios:
1. Given records with values ranging from 1 byte to 100KB, When compaction copies them to the tail, Then each record's content is byte-for-byte identical to the original
2. Given a compacted region containing records of 10 different sizes, When the hash index is updated, Then every CAS points to the correct new address with the correct record size

### User Story P4 – Unified API for Fixed and Variable-Length
Narrative: The compaction API works uniformly regardless of whether keys/values are fixed or variable-length. A developer calls the same `compact()` method and the system chooses the correct strategy.

Independent Test: Call `compact()` on a store parameterized with `u64` keys (fixed) and on a store with `Vec<u8>` keys (variable) — both succeed.

Acceptance Scenarios:
1. Given the existing `compact::<u64, u64>()` call path, When invoked after this change, Then behavior is identical to before (backward compatible)
2. Given a new `compact::<Vec<u8>, Vec<u8>>()` call path, When invoked, Then variable-length compaction executes correctly

### Edge Cases
- A variable-length record whose serialized size exceeds the remaining space on a page — scanner must correctly skip to next page
- A record with zero-length value (4-byte length prefix with value 0) — must be handled without underflow
- A record exactly at a page boundary — offset arithmetic must not overflow
- All records in the compaction range are dead — no copies needed, begin_address still advances
- All records in the compaction range are live — all are copied, resulting in no space savings but no data loss
- Concurrent reads during compaction — readers using epoch protection must see consistent data throughout
- A record whose key matches a hash bucket entry that was concurrently modified — CAS failure during pointer swing is benign (new entry is newer)

## Requirements

### Functional Requirements
- FR-001: The compaction scanner shall determine each record's size by reading the key and value length prefixes from the log, rather than relying on compile-time constant sizes (Stories: P1, P3)
- FR-002: The compaction scanner shall iterate records using a variable stride computed per-record, correctly advancing across page boundaries (Stories: P1, P3)
- FR-003: The compaction copier shall allocate space at the log tail matching each source record's exact serialized size (Stories: P1, P3)
- FR-004: The compaction copier shall copy each record's full byte content (header + key + value with length prefixes) to the new location (Stories: P1, P3)
- FR-005: The address updater shall perform CAS-based hash index pointer swings for variable-length records, reading the key from the new address to compute the hash (Stories: P1, P2)
- FR-006: The address updater shall clear hash index entries for tombstoned variable-length records (Stories: P2)
- FR-007: The `FasterKv::compact()` API shall accept variable-length key/value types (removing the `FixedSizeKey + FixedSizeValue` bound) while maintaining backward compatibility for fixed-size types (Stories: P4)
- FR-008: The orchestrator shall coordinate variable-length compaction with the same epoch protection guarantees as fixed-size compaction (Stories: P1, P2, P3)
- FR-009: The `LiveRecord` struct shall store the actual per-record size (not a uniform size) so the copier allocates correctly (Stories: P1, P3)
- FR-010: The scanner shall handle records with zero-length keys or values without panicking or producing incorrect sizes (Stories: P3)

### Key Entities
- **RecordSizeResolver**: Mechanism to compute a record's total serialized size from its position in the log by reading length prefixes
- **LiveRecord**: Extended to carry per-record `record_size` that varies across records in the same compaction plan
- **CompactionPlan**: Unchanged in structure but populated with variable-sized LiveRecords

### Cross-Cutting / Non-Functional
- Compaction must never lose data: every record classified as "live" must be successfully copied and its hash index entry updated
- Compaction must be safe under concurrent reads: epoch protection ensures readers see consistent records throughout the process
- The existing fixed-size compaction path must not regress in performance (zero overhead when types are fixed-size)

## Success Criteria
- SC-001: A store with 10,000 variable-length records (80% overwritten) returns all correct values after compaction, and begin_address advances (FR-001, FR-002, FR-003, FR-004, FR-005)
- SC-002: A store with 5,000 variable-length records (60% deleted) returns NotFound for deleted keys and correct values for remaining keys after compaction (FR-005, FR-006)
- SC-003: Records with value sizes spanning 1B to 100KB are correctly compacted with byte-perfect content preservation (FR-003, FR-004, FR-009)
- SC-004: The existing `compact::<u64, u64>()` API continues to compile and produce identical results (FR-007)
- SC-005: Variable-length compaction throughput is within 2× of fixed-size compaction for equivalent total data volume (FR-001, FR-002)
- SC-006: Zero-length keys and values compact without panics or data corruption (FR-010)
- SC-007: Concurrent read operations during variable-length compaction return correct results (FR-008)
- SC-008: All existing compaction tests continue to pass without modification (FR-007)

## Assumptions
- Variable-length records use the existing length-prefix serialization format (4-byte LE length + data), as implemented in the `Key` and `Value` traits for `Vec<u8>` and `String` — no new serialization format is needed
- The `RecordInfo` header (8 bytes) does not store record size — size must be derived by reading key/value length prefixes from the record body — this is a fundamental constraint we work within rather than changing the header format
- Records do not span page boundaries — the existing allocator ensures this — so per-record scanning within a single page is always safe
- The compaction region (begin_address to safe_read_only_address) is always in-memory or on-disk with pages loadable — the scanner operates on in-memory pages

## Scope

In Scope:
- Variable-length record size discovery via length-prefix reading
- Variable-stride page scanning in the compaction scanner
- Per-record size tracking in LiveRecord and CompactionPlan
- Variable-size allocation in the copier
- Hash index pointer swings for variable-length records
- Tombstone clearing for variable-length records
- Unified `compact()` API that works for both fixed and variable-length types
- Unit tests, integration tests, and property tests for all new code paths
- Backward compatibility verification for existing fixed-size compaction

Out of Scope:
- Changing the RecordInfo header format to include a size field
- Online/incremental compaction (compaction runs as a batch operation)
- Compaction of on-disk records not currently in memory (requires page loading, separate feature)
- Compaction policy changes (when to trigger compaction — the policy module is unchanged)
- Read cache integration with compaction
- Parallel/multi-threaded compaction within a single cycle
- C FFI extensions for the compaction API

## Dependencies
- Existing compaction pipeline (scanner, copier, address_update, begin_address, orchestrator)
- RecordLayout::compute() for dynamic layout computation
- Key/Value trait serialized_size() methods for length-prefix reading
- HybridLog page table and frame access for in-memory record reading
- Epoch protection infrastructure for concurrent safety

## Risks & Mitigations
- **Per-record size computation overhead**: Reading two length prefixes per record adds I/O compared to fixed stride. Mitigation: length prefixes are in the same cache line as the record header; benchmark to verify <2× overhead
- **Page boundary edge cases**: Variable-sized records may leave gaps at page ends that the scanner must skip. Mitigation: The scanner already handles page-boundary gaps in fixed-size mode; extend with careful offset arithmetic and comprehensive edge-case tests
- **Incorrect size computation causing memory corruption**: If a length prefix is misread (e.g., from a partially-written record), the scanner could advance to an invalid offset. Mitigation: Validate that computed record size is positive, non-zero, and does not exceed page size; treat invalid records conservatively as dead
- **Backward compatibility regression**: Changing trait bounds on `compact()` could break existing callers. Mitigation: Ensure both `FixedSizeKey` and general `Key` types work; test both paths explicitly
- **Hash index CAS failures during variable-length swing**: More likely with varying record sizes since compaction takes longer per-record. Mitigation: CAS failures are already benign by design (concurrent update wins); no additional mitigation needed

## References
- Existing compaction: `rust/crates/faster-core/src/compaction/`
- Record layout: `rust/crates/faster-core/src/record/layout.rs`
- Variable-length traits: `rust/crates/faster-core/src/record/traits.rs`
- Scanner TODO: `compaction/scanner.rs:29-33` ("Variable-length record scanning is future work")
- FasterKv::compact(): `store/kv.rs:1407-1409`
