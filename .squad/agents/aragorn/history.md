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

## Learnings
<!-- Append new learnings -->
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
