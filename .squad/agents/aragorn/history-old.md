# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Architecture:** `FasterKv<F>` is the core store generic over `Functions` trait. Sessions (`FasterSession<F>`) own epoch protection. `UnsafeContext` wraps `&'a mut FasterSession<F>` for batch ops without per-op epoch overhead.
- **Key modules:** `store/kv.rs` (FasterKv, factory), `store/session.rs` (FasterSession, UnsafeContext, batch CRUD), `store/operations.rs` (operation dispatch), `compaction/` (scanner, plans), `benches/ycsb.rs` (YCSB benchmarks).
- **Error model:** `OperationStatus` for control flow (Ok, Pending, NotFound, etc.), `FasterError` for true errors (I/O, corruption). No `thiserror` — manual impls.
- **Address model:** `LogicalAddress` — 25-bit offset, 23-bit page, 16-bit reserved. `INVALID = 1` (0 = empty bucket). `AtomicLogicalAddress` requires explicit `Ordering`.
- **Build/test:** `cargo fmt`, `cargo clippy`, `cargo nextest run` (no `--all-targets` — Criterion benches hang nextest). Bench files use custom `main()` to detect `--bench` flag.
- **Visibility:** `pub(crate)` on FasterKv fields for cross-module access (e.g., session.rs batch methods).
- **Builder pattern:** `FasterKv::<SimpleFunctions<K,V>>::builder()` — turbofish required because `FasterKvBuilder` is non-generic.

### Review: Multi-Writer Deadlock Fix (2026-07-21)
- **Verdict:** APPROVE WITH COMMENTS
- **Findings:**
  - Fix A/B/C implemented correctly.
  - `deadlock_tests.rs` adds strong regression coverage but tests are `#[ignore]`-ed.
  - `SyncFileDevice` yields in `poll_completions` (good).
  - `allocate_at_tail` retry loop is bounded (32 retries).
- **Action:** Validated fix by running ignored tests (all passed). Recommended enabling tests before merge.

## Learnings
<!-- Append new learnings -->
- **Multi-writer deadlock root cause:** The 3-point deadlock is a timing gap, not a logic error. `maintenance()` submits async I/O (Sealed→Flushing) then immediately runs eviction — but callbacks (Flushing→Flushed) fire on worker threads and haven't landed yet. All pages are still Flushing, head can't advance, buffer stays full. The `continue` on QueueFull in flush.rs:379 makes it worse by leaving individual pages Sealed (blocking head advancement), but even without QueueFull the timing gap alone causes stalls.
- **SyncFileDevice never returns QueueFull:** It uses an unbounded mpsc channel. `max_outstanding: 1024` is declared but never enforced. The deadlock on SyncFileDevice is purely from the timing gap. UringDevice with its real queue depth DOES trigger QueueFull, compounding the issue.
- **Device trait has no completion polling:** No `try_complete()` or `poll_completions()` exists. Writers cannot help drain I/O. The C++ `disk->TryComplete()` in `NewPage()` has no Rust equivalent. This is the key missing piece.
- **`allocate_at_tail` retries only once:** After `maintenance()` fails to free buffer space, it tries exactly one more `advance_to_next_page` and gives up. With async I/O, one retry is never enough — completions need time to land. A bounded retry loop with yields is needed.
- **Shared workspace branch switching:** Other agents (Legolas, Boromir) switch branches in the shared working tree. Always verify `git branch --show-current` immediately before `git commit`. Cherry-picks across diverged branches cause conflicts — prefer re-applying changes directly.
- **Property test case cost:** HashIndex proptest cases cost ~240ms each in isolation (epoch framework + allocator setup). Even 8 cases = ~2s. For <1s gate, use 2 cases in tier-1, full counts in tier-2.
- **Page-fill tests are fundamentally slow:** With 32MB pages and 24-byte records, filling 1 page = 1.4M records ≈ 1-2s minimum. Tests requiring eviction (6+ pages) need 40s+. Only #[ignore] works.
- **Nextest parallel contention inflates times:** Tests taking 0.1-0.8s in isolation show 1-1.7s under full parallel load on 4-core VMs. Individual times (in isolation) are the true metric.
- **Write-pending tests need precise record counts:** With 4KB BigVal records, 8192 records/page. Config `buffer_size_pages=4, max_in_memory_pages=2` needs ~25K records for eviction (3 pages). Too few = no eviction = test fails.
- **Doctest type inference pitfall:** `FasterKv::builder()` returns `FasterKvBuilder` (non-generic). Doctests must use turbofish: `FasterKv::<SimpleFunctions<u64, u64>>::builder()`.
- **Mutation testing config:** `rust/mutants.toml` configures cargo-mutants v27.0. Key files: `rust/docs/mutation-testing.md` (workflow guide), `rust/mutants.out/` (gitignored output).
- **cargo-mutants `-F` flag is substring match:** `-F 'address.rs'` also matches `begin_address.rs`. Use `-F 'src/address.rs'` for precision.
- **Bitwise const ops produce unviable mutants:** `<<`→`>>` and `-`→`+` in const bit-mask definitions fail to compile (type system catches them). This is good — no test gap.
- **address.rs pilot: 100% catch rate.** 55 caught, 9 unviable, 0 missed, 0 timeouts in ~6min. Prior pilot had 8 timeouts — resolved by `exclude_re` for field-packing functions.
- **`mutants.out/` must be gitignored** — cargo-mutants writes outcomes.json, caught.txt, etc. on every run.
- **Status vs Error separation:** Operational outcomes (Ok, Pending, NotFound) are control-flow signals, not errors. True errors (I/O failure, corruption) go in `FasterError`.
- **No thiserror needed:** Manual Display/Error/From impls are ~40 lines for small enums.
- **`OperationStatus` variant set:** Merges C++ Status + C# Status + architecture OkKind::Deleted into one flat enum.
- C++ `Address`: 25 bits offset (LSB), 23 bits page, 16 bits reserved (MSB). `kInvalidAddress = 1` (not 0). Rust `LogicalAddress` matches exactly.
- `Address::INVALID = 1` because all-zeros = empty hash bucket entry, 1 = "address present but not yet valid."
- `AtomicLogicalAddress` requires explicit `Ordering` (more idiomatic than C++ seq_cst default).
- Per-op epoch protect/unprotect is >80% of cost for read/RMW hot paths. UnsafeContext with batch refresh amortizes this.
- `RecordInfo::is_null()` unreliable for distinguishing unwritten memory from valid version-0 records.
- Criterion bench binaries trigger full benchmark run under `cargo nextest run --all-targets`. Drop `--all-targets` from nextest.
- `imports_granularity` silently degrades on stable rustfmt toolchain.

## Session Log

### AI-4 — Epoch-Amortized UnsafeContext Batch API (2026-07-17)
**Files:** `store/session.rs` (UnsafeContext struct, batch CRUD), `store/kv.rs` (factory), `benches/ycsb.rs` (4 new bench groups)

**Architecture:** UnsafeContext holds `&'a mut FasterSession<F>`, batch methods take `&FasterKv<F>` as first param. `Refresh()` updates local epoch without leaving protection. Drop calls `session.end_unsafe()`.

**Benchmark results:** Upsert 3.56→10.07 Mops/s (2.83×), Read 4.85→47.33 Mops/s (9.76×), Mixed 3.37→13.55 Mops/s (4.03×), RMW 4.83→47.45 Mops/s (9.83×).

---

### Wave 1 K1 — Compaction Scanner (2026-03-06)
**Files:** `compaction/mod.rs` (CompactionPlan, LiveRecord), `compaction/scanner.rs` (scan(), page-boundary advance), `store/kv.rs` (first_data_address())

**Architecture:** Stateless scan function. Classification: tombstone → invalid → hash-index chain walk → conservative LIVE. Never classifies live as dead. `#[deny(unsafe_code)]` on compaction module.

**Bugs fixed:** (1) Sentinel alignment — must start at `first_data_address()` (offset+8). (2) `RecordInfo(0)` is valid first-version record, not null memory.

---

### Precheckin Fix — Formatting + Clippy Sweep (2026-03-09)
33 files had fmt drift (nightly rustfmt silently skips `imports_granularity` on stable). 6 unused `DeleteInfo` imports, 27 undocumented unsafe blocks in FFI tests. Nextest hung due to Criterion bench binaries with `--all-targets`.

**Fix:** Drop `--all-targets` from nextest. Clippy handles bench code compilation.
**Commit:** `74ce041f` — 35 files, ~1150 insertions / ~470 deletions.

---

### Wave 4 — Mutation Testing on Critical Modules (2026-03-09)
**Files:** `tests/mutation_tests.rs` (16 tests), `compaction/address_update.rs` (+1 test)

**Modules tested:** allocator.rs (82 mutants), hash/ (251 mutants), compaction/ (partial)

**Results:**
- **Allocator:** 52 caught / 14 missed / 5 timeout / 11 unviable → 79% kill rate
- **Hash module:** 201 caught / 16 missed / 11 timeout / 23 unviable → 93% kill rate
- **New tests killed:** 3 hash shift-direction mutations, overflow pool free→noop

**Key finding:** Most surviving mutants are equivalent mutations (|→^ when bits don't overlap, >0 vs >=0 on unsigned), performance-only (prefetch noop, eager allocation), or Debug formatting. True test gaps were addressed.

**Tests added:** 17 targeted tests in `mutation_tests.rs` and `address_update.rs`.

### Wave 5 — Lossy LRU Cache Wrap-Around Mode (Session 6)

**Task:** Implement lossy cache semantics so the hybrid log wraps gracefully.

**Investigation findings:**
- No circular address wrap-around — FASTER uses monotonic 48-bit addresses (23-bit page + 25-bit offset). The "circular" aspect is only the in-memory page frame buffer.
- `AddressInfo::classify()` maps addresses to regions: Truncated < head < OnDisk < ReadOnly < Mutable < InMutable.
- Read/delete paths already handle Truncated correctly (return NotFound).
- **Bug:** internal_upsert and internal_rmw returned `Aborted` for Truncated addresses, causing infinite retry loops.
- `evict_and_truncate()` and `invalidate_entries_in_range()` existed but were never called from `maintenance()`.

**Implementation:**
- Added `lossy: bool` to FasterKvConfig (default false).
- Fixed internal_upsert: Truncated → `upsert_copy_to_tail()` (fresh record, old_value=None).
- Fixed internal_rmw: Truncated → `rmw_create_at_tail()` (calls rmw_initial).
- Lossy maintenance: `evict_and_truncate()` + `invalidate_entries_in_range()`.
- Updated page-cache sample to `lossy: true`.
- 9 integration tests (eviction, re-upsert, RMW, delete, concurrent, begin_address advancement).

**Key learnings:**
- Page size = 2^25 = 32 MiB. Each u64/u64 record = 24 bytes → ~1.4M records/page. Need 6M+ records to force eviction in a 4-page buffer.
- InMemoryDevice `truncate_until` is a no-op; hash invalidation does the actual cleanup.
- Git stash across branches is dangerous in shared workspaces — stash pop can fail on Cargo.lock conflicts, losing all uncommitted changes.
- Test parallelism (>2 threads) with lossy tests (~300MB each) causes resource contention failures in unrelated tests.

**Results:** 1608 existing tests pass, 9 new lossy tests pass, page-cache sample runs 60s stable with readers+writers.

---

### Code Quality Cleanup — Precheckin Gate (2026-03-10)
**Files:** 10+ test files, 2 source files (kv.rs, sync_file_device.rs, scan.rs, scanner.rs)

**Task:** Make precheckin clean: `cargo fmt --check`, `cargo clippy`, all tests passing with none >1s individually.

**Fixes applied:**
1. **cargo fmt** — 13 files had whitespace/line-wrap drift (stable rustfmt skips nightly-only features).
2. **clippy** — 2 undocumented unsafe blocks in mutation_tests.rs and hybrid_log_mutation_tests.rs.
3. **Slow tests** — 17 tests moved to #[ignore] (tier-2), 9 tests sped up via iteration reduction.

**Test speedups (made <1s in isolation):**
- property_tests: proptest cases 64/32 → 2
- memory_pressure_tests: proptest cases 32 → 2
- write_pending_completion: RECORD_COUNT 42K→25K, smaller buffer config (4 pages, 2 max in-memory)
- quickstart concurrent: ops_per_thread 1M→10K
- concurrent_readers_writers_lossy: deadline 5s→100ms
- allocator_page_advancement: records 1M→50K
- scanner proptest: cases 256→8
- discover_many_checkpoints: 20→5 checkpoints

**Tests moved to tier-2 (#[ignore]):**
- 8 lossy_cache_tests (fill 6-12 pages, 40-71s)
- 4 hybrid_log_mutation_tests (500K-2M records, 19-22s)
- 3 epvs_integration stress tests (multi-thread + checkpoint, ~30s)
- 1 scan_across_page_boundary (fills 32MB page, ~2.5s)
- 1 quickstart_example_2_persistent_storage (SyncFileDevice disk I/O, ~3.6s)

**Results:** 1628 tests pass (<1s each), 22 ignored (pass with --run-ignored). Total count unchanged (1645→1628+17).
- **Loom shim integration:** All production code must use `crate::sync` module instead of direct `std::sync` or `std::thread` imports. Test code (inside `#[cfg(test)]` blocks) is exempt and uses std directly. The sync.rs module exports AtomicU8, RwLockReadGuard, and all standard types under `#[cfg(loom)]`, `#[cfg(simulation)]`, and standard builds. Fully-qualified paths like `std::sync::atomic::Ordering::Acquire` must be replaced with imported `Ordering::Acquire`.

## 2026-03-11: Backlog Sprint — Loom Shim Integration

**Timestamp:** 2026-03-11T19:33:26Z  
**Collaboration:** Quadrant sprint (Aragorn, Sam, Éowyn, Galadriel)

### What Happened

Wired all 9 production files to use `crate::sync` shim for loom testing integration. This allows loom's mock sync primitives to patch the entire codebase at once.

### Key Changes

- Replaced direct `std::sync` imports with shim module indirection
- Exported `AtomicU8` and `RwLockReadGuard` from sync module
- No API changes; full backward compatibility

### Team Coordination

- **Sam:** Log prefix coupling fix (recovery API parameterization) — SUCCESS
- **Éowyn:** DST smoke test integration (Tier 2/3 split) — SUCCESS
- **Galadriel:** Miri coverage expansion (71→78 tests) — SUCCESS

**Commits:**
- 229f0890: Wire loom shim to production code
- fc0d3da9 (Sam): Add log_prefix parameter
- 33fb44cf (Sam): Update test callsites
- 4fca1523 (Coordinator): Fix missed callsites
- a4525cb5 (Éowyn): Split DST tiers
- 62acd905 (Éowyn): Add CI DST job
- 9a7190d0 (Galadriel): Expand miri coverage

### Verification

All 1719 existing tests pass. Loom framework ready for coordinated sync primitive mocking.
