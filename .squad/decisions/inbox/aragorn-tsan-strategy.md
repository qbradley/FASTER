# Decision: ThreadSanitizer Testing Strategy for Compaction Scanner Race

**Author:** Aragorn (Rust Expert)  
**Date:** 2026-07-24  
**Status:** Implemented  
**Commit:** `4675d33a`

## Summary

Added ThreadSanitizer (TSan) tests that exercise the specific scanner-vs-evictor use-after-free data race causing SIGSEGV under 16-thread concurrent compaction. TSan instruments all memory accesses and can detect the race that MIRI (single-threaded) and Loom (raw pointer bypass) cannot.

## Design Choices

| Decision | Rationale |
|----------|-----------|
| `InMemoryDevice` (not custom async device) | Simplicity. Synchronous I/O means tight race windows, but TSan's 5-15× instrumentation overhead widens them. Async devices add complexity without TSan benefit. |
| Separate maintenance thread from compactor | The race is between scanner (inside `compact()`) and evictor (inside `maintenance()`). Dedicating separate threads maximizes overlap. |
| 10-second test duration | 32 MiB pages require ~1.4M records each to fill. At debug speed (~600K ops/s), 10 seconds fills ~4 pages — enough for page transitions. |
| `#[ignore]` tier-2 | TSan requires nightly, slows execution 5-15×, and isn't available on all platforms. Tests must not break `cargo nextest run`. |
| Suppression file for epoch/RecordInfo | Epoch counters use `Relaxed` ordering intentionally (benign TOCTOU). RecordInfo flag reads are conservative-safe on stale data. Suppressing avoids false positive noise. |
| `Barrier` synchronization | All threads (writers + maintainer + compactor) start simultaneously for maximum interleaving. |
| No seed phase | Writers generate data continuously from test start. Avoids the InMemoryDevice pitfall where seeding + draining leaves head ≈ safe_read_only (empty scan range). |

## Race Model

The SIGSEGV occurs when:
1. `compact()` captures `head=H, safe_read_only=S` where `H < S`
2. Scanner reads pages in `[H, S)` via raw pointers from `get_physical_address()`
3. Concurrently, `maintenance()` runs `evict_pages()` → `advance_head()` which frees the page frames the scanner is reading
4. Scanner dereferences freed memory → SIGSEGV (or TSan data race report)

With InMemoryDevice (synchronous I/O), step 3 happens within a single `maintenance()` call. The race window is the time between `shift_read_only_to_tail()` (which advances `safe_read_only`) and `evict_pages()` (which advances `head`) within the same function. Under TSan instrumentation, memory access overhead widens this window from nanoseconds to microseconds.

## What TSan Reports Look Like

When the race exists, TSan output will include:
```
WARNING: ThreadSanitizer: data race (pid=...)
  Write of size 8 at 0x... by thread T2 (mutexes: ...):
    #0 ... try_evict_frame ...
    #1 ... advance_head ...
  Previous read of size 8 at 0x... by thread T3:
    #0 ... RecordAccessor::from_log ...
    #1 ... CompactionScanner::scan ...
```

## Files Changed

- `rust/crates/faster-core/tests/tsan_compaction.rs` — 3 test scenarios
- `rust/crates/faster-core/tests/tsan_suppressions.txt` — epoch + RecordInfo suppressions
- `rust/crates/faster-core/tests/README.md` — Test suite documentation (new)
- `rust/scripts/run-tsan.sh` — One-command TSan runner

## Team Impact

- **Boromir (Testing):** TSan tests are tier-2 (`#[ignore]`). They don't affect `cargo nextest run`.
- **All:** The `scripts/run-tsan.sh` script is the recommended way to run TSan. It sets RUSTFLAGS, TSAN_OPTIONS, and RUST_TEST_THREADS correctly.
- **CI:** Can add `run-tsan.sh` to a nightly CI job. Not required for every PR.
