# MIRI Audit: Compaction Scanner Unsafe Code

**Author:** Aragorn (Rust Expert)  
**Date:** 2025-07-24  
**Triggered by:** SIGSEGV crash under 16-thread compaction load  
**Branch:** `rust`

## Summary

**MIRI finds zero undefined behavior in the compaction path.** The compaction
code itself contains no `unsafe` — it's built entirely on safe abstractions.
The unsafe code lives in the supporting infrastructure (record_ops, scan,
page, log_allocator) and is well-covered. Three new MIRI tests were added to
close coverage gaps in version chain walks, invalidated-record scanning, and
page-boundary behavior.

**The 16-thread SIGSEGV is almost certainly a data race, not single-threaded
UB.** MIRI cannot catch concurrent access issues. Recommend Loom model
checking or ThreadSanitizer for the concurrent scenario.

## Unsafe Blocks in the Compaction Path

### Compaction modules (ZERO unsafe)

| File | Unsafe blocks | Notes |
|------|:---:|-------|
| `compaction/scanner.rs` | 0 | `#[deny(unsafe_code)]` on module |
| `compaction/copier.rs` | 0 | Uses safe `LogRecordWriter`/`LogRecordReader` |
| `compaction/address_update.rs` | 0 | Uses safe `HashIndex::update` |
| `compaction/begin_address.rs` | 0 | Pure address arithmetic |
| `compaction/orchestrator.rs` | 0 | Orchestration logic only |
| `compaction/policy.rs` | 0 | Policy decisions only |
| `compaction/mod.rs` | 0 | Type definitions only |

### Infrastructure exercised by compaction (unsafe lives here)

| File:Line | Unsafe Operation | Description |
|-----------|------------------|-------------|
| `hybrid_log/record_ops.rs:76` | `RecordAccessor::new(ptr, size)` | Raw pointer → accessor |
| `hybrid_log/record_ops.rs:105` | `&*(ptr as *const AtomicRecordInfo)` | Ptr cast to atomic ref |
| `hybrid_log/record_ops.rs:143` | `from_raw_parts(ptr, size)` | Byte slice from raw pointer |
| `hybrid_log/record_ops.rs:172` | `Self::new(ptr, size)` via `from_log()` | Safe wrapper calls unsafe ctor |
| `hybrid_log/record_ops.rs:207` | `MutableRecordAccessor::new(ptr, size)` | Mutable raw pointer → accessor |
| `hybrid_log/record_ops.rs:238` | `&*(ptr as *const AtomicRecordInfo)` | Mutable accessor atomic ref |
| `hybrid_log/record_ops.rs:276` | `from_raw_parts(ptr, size)` | Mutable accessor slice |
| `hybrid_log/record_ops.rs:339` | `from_raw_parts_mut(ptr, size)` | Mutable byte slice |
| `hybrid_log/record_ops.rs:398` | `MutableRecordAccessor::new(ptr, size)` | `allocate_record` path |
| `hybrid_log/record_ops.rs:417` | `MutableRecordAccessor::new(ptr, size)` | `try_allocate_record` path |
| `hybrid_log/scan.rs:243` | `from_raw_parts(ptr, size)` | Log scan iterator record read |
| `hybrid_log/log_allocator.rs:187` | `ptr.add(offset)` | Physical address computation |
| `hybrid_log/page.rs:202` | `alloc_zeroed(layout)` | Page frame allocation |
| `hybrid_log/page.rs:243,254` | `from_raw_parts` / `from_raw_parts_mut` | Page frame slice access |
| `hybrid_log/page.rs:264` | `write_bytes(0, size)` | Page zeroing |
| `hybrid_log/page.rs:396-399` | `&*ptr` / `&mut *ptr` | Page table frame ref |

All blocks have `// SAFETY:` comments documenting invariants.

## MIRI Coverage Assessment

### Previously covered (7 tests)

| Test | Covers |
|------|--------|
| `scan_empty_range` | Scanner construction, empty scan |
| `scan_with_live_and_dead_records` | RecordAccessor::from_log, record_info read, hash lookup |
| `scan_with_tombstone_records` | Tombstone flag read path |
| `copy_single_record` | LogRecordWriter + LogRecordReader unsafe paths |
| `copy_multiple_records` | Multi-record copy with address mapping |
| `copy_empty_list` | Edge case: empty copy |
| `swing_updates_hash_entries` | Full scan→copy→swing pipeline |

### Gaps found and filled (+3 new tests)

| New Test | Gap Addressed |
|----------|--------------|
| `scan_version_chain_classifies_superseded_as_dead` | **Version chain walk** — exercises `is_current_version` → `read_header_and_match_key_varlen` → `RecordAccessor::from_log` on chain-linked records. Previous tests used `INVALID` as previous_address (chain length 1). |
| `scan_invalid_record_classified_as_dead` | **Invalidated record read** — exercises `RecordInfo::is_invalid()` through the unsafe `RecordAccessor` → `record_info()` path on a pre-invalidated record. |
| `scan_near_page_boundary_advances_safely` | **Page boundary pointer arithmetic** — fills pages to trigger `MIN_READABLE` guard and `advance()` page wrap. Validates no out-of-bounds access at page edges. |

### Test results

```
cargo +nightly miri test -p faster-core --test miri_tests
→ 81 passed, 0 failed (was 78 before, +3 new)

cargo +nightly miri test -p faster-core --test miri_tests -- compaction
→ 10 passed, 0 failed (was 7 before, +3 new)
```

**MIRI found zero undefined behavior.**

## What MIRI Cannot Catch

The SIGSEGV under 16 threads is **not reachable by MIRI** because:

1. **MIRI is single-threaded** — it can't model the timing window where a
   concurrent page eviction races with the scanner reading from that page.
2. **The likely root cause is a page eviction race**: the scanner reads a
   physical pointer from `get_physical_address()`, then the page is evicted
   by another thread before the scanner dereferences the pointer → SIGSEGV.
3. The scanner's epoch protection (`is_current_version` → chain walk →
   `RecordAccessor::from_log`) is correct in the single-threaded model, but
   may have insufficient epoch refresh frequency under 16-thread contention.

### Recommended next steps for the crash

| Tool | What it can catch |
|------|-------------------|
| **ThreadSanitizer** (`RUSTFLAGS="-Z sanitizer=thread"`) | Data races on page pointers |
| **Loom** (via `crate::sync` shim) | Model-checked concurrent epoch protocol |
| **Stress test** (`stress_5min.rs` with 16 threads + compaction enabled) | Reproduce the crash |
| **Epoch audit** | Verify scanner holds epoch protection across the entire chain walk |

## Files Modified

- `rust/crates/faster-core/tests/miri_tests.rs` — 3 new tests in `miri_compaction_scanner`
