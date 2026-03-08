# Decision: Miri Test Coverage Standard for Unsafe Code

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-08
**Status:** Recommendation
**Impact:** Testing discipline — all unsafe code paths

## Decision

Every module containing `unsafe` code blocks in `faster-core` must have
corresponding Miri tests that exercise the unsafe paths. The Miri test suite
(`miri_tests.rs`) is the safety net that catches undefined behavior the
compiler cannot.

## Coverage Achieved

| Module | Miri Status | Tests |
|--------|------------|-------|
| `allocator.rs` | ✅ Covered | 7 tests |
| `buffer_pool.rs` | ✅ Covered | 2 tests |
| `device.rs` | ✅ Covered (partial — callback paths only) | 2 tests |
| `hash/bucket.rs` | ✅ Covered | 11 tests |
| `hash/table.rs` | ✅ Covered | 4 tests |
| `hash/prefetch.rs` | ✅ Covered (no-op intrinsics) | 1 test |
| `hash/index.rs` | ✅ Covered (via compaction tests) | — |
| `hybrid_log/page.rs` | ✅ Covered | 1 test |
| `hybrid_log/log_allocator.rs` | ✅ Covered | 1 test |
| `hybrid_log/record_ops.rs` | ✅ Covered | 4+4 tests |
| `hybrid_log/scan.rs` | ✅ Covered | 4 tests |
| `epoch/drain.rs` | ✅ Covered (indirect via EpochTable) | 3 tests |
| `compaction/scanner.rs` | ✅ Covered | 3 tests |
| `compaction/copier.rs` | ✅ Covered | 3 tests |
| `compaction/address_update.rs` | ✅ Covered | 1 test |
| `store/operations.rs` | ✅ Covered (via FasterKv CRUD) | 6 tests |
| `store/functions.rs` | ✅ Covered (via FasterKv CRUD) | — |
| `record/record_info.rs` | ✅ Covered (atomics) | 4 tests |
| `state/mod.rs` | ✅ Covered (EPVS packing) | 4 tests |
| `recovery/index_recovery.rs` | ❌ Skipped — requires file I/O | — |
| `hybrid_log/flush.rs` | ❌ Skipped — async device I/O | — |
| `store/pending_io.rs` | ❌ Skipped — async device callbacks | — |
| `sync_file_device.rs` | ❌ Skipped — file system operations | — |

## Modules That Cannot Be Tested Under Miri

These require real file I/O or async device callbacks that Miri cannot execute:

1. **recovery/index_recovery.rs** — Reads bucket data from checkpoint files
2. **hybrid_log/flush.rs** — Async page flush with device callbacks
3. **store/pending_io.rs** — I/O completion callback reconstruction
4. **sync_file_device.rs** — Direct file system operations

**Alternative:** These should be covered by proptest fuzzing and integration
tests running under AddressSanitizer (`-Zsanitizer=address`).

## Rationale

Miri catches classes of bugs that no other tool reliably detects in safe
Rust tests: use-after-free, uninitialized reads, alignment violations,
invalid pointer arithmetic, and provenance violations. With 71 Miri tests
running in ~42s, this is a practical safety net for CI.
