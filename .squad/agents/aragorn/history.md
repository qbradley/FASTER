# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Architecture

### Key Types & Modules

**Store:**
- `FasterKv<F: Functions>` — Core store, generic over Functions trait
- `FasterSession<F>` — Owns epoch protection per session
- `UnsafeContext<'a, F>` — Wraps `&'a mut FasterSession<F>` for batch ops with amortized epoch overhead
- **Key modules:** `store/kv.rs` (factory), `store/session.rs` (session + batch API), `store/operations.rs` (dispatch)

**Address Model:**
- `LogicalAddress` — 25-bit offset (LSB), 23-bit page, 16-bit reserved (MSB)
- `INVALID = 1` (NOT 0 — zero means empty bucket)
- `AtomicLogicalAddress` requires explicit `Ordering`
- Page size = 2^25 = 32 MiB (~1.4M records at 24 bytes/record)
- Regions: `Truncated < head < OnDisk < ReadOnly < Mutable < InMutable`

**Error Model:**
- `OperationStatus` — Control flow (Ok, Pending, NotFound, InPlaceUpdated, Copied)
- `FasterError` — True errors (IoError, ChecksumMismatch, CorruptedData)
- No `thiserror` — manual impls (~40 lines)

**Compaction:**
- `compaction/scanner.rs` — Stateless scan, conservative LIVE classification
- `compaction/mod.rs` — CompactionPlan, LiveRecord
- Module is `#[deny(unsafe_code)]`

**Other Modules:**
- `hash/` — Hash index
- `allocator/` — Page allocator
- `epoch/` — Epoch framework
- `device/` — Storage backends (InMemoryDevice, SyncFileDevice, UringDevice)
- `sync.rs` — Loom shim module (`crate::sync` → `std::sync` or `loom::sync`)

### Build & Test Commands

**Standard workflow:**
```bash
cargo fmt
cargo clippy
cargo nextest run              # Tier-1 tests (<1s each)
cargo nextest run --run-ignored  # Tier-2 tests (slow/exhaustive)
```

**Benchmark:**
```bash
cd rust && cargo bench --bench ycsb -p faster-core -- --nocapture
```

**Never use `--all-targets` with nextest** — Criterion binaries trigger full runs.

### Code Conventions

**Visibility:**
- `pub` — User-facing API only
- `pub(crate)` — Cross-module access (e.g., FasterKv fields accessed by session.rs)
- Private — Everything else

**Sync primitives:**
- Production code: `use crate::sync::{Arc, AtomicU64, Ordering}` (loom shim)
- Test code: Can use `std::sync` directly

**Builder pattern:**
- Turbofish required: `FasterKv::<SimpleFunctions<u64, u64>>::builder()` (`FasterKvBuilder` is non-generic)

**Unsafe discipline:**
- Every `unsafe` block needs `// SAFETY:` comment explaining invariants
- `#[deny(unsafe_code)]` on modules that shouldn't need unsafe (e.g., compaction)

**Test performance:**
- Tier-1: <1s in isolation, runs on every commit
- Tier-2: `#[ignore]`, slow tests (page-filling, I/O, stress tests)
- Proptest: `cases: 2` (tier-1), `cases: 64-256` (tier-2)

### Key Learnings

**Address Model:**
- `RecordInfo::is_null()` unreliable for detecting unwritten memory — version 0 is valid
- Truncated region handling: upsert/RMW must allocate fresh records, not return Aborted

**Performance:**
- Per-op epoch protect/unprotect costs >80% on hot paths → UnsafeContext amortizes
- `compute_safe_epoch()` scans 256 entries × Acquire = ~97-128ns

**Test Sizing:**
- Page fills cost ~1.4M records × ~1-2s minimum
- Proptest cases cost ~240ms each (epoch + allocator setup)
- Nextest parallel contention inflates times: 0.1s → 1.7s under load

**Multi-writer deadlock (2026-07-21 review):**
- Root cause: `maintenance()` submits async I/O (Sealed→Flushing), then runs eviction before callbacks land
- All pages stuck Flushing → head can't advance → buffer full → deadlock
- `SyncFileDevice` never enforces `max_outstanding: 1024` (unbounded mpsc)
- `allocate_at_tail` retries only once (not enough for async I/O)
- C++ `disk->TryComplete()` has no Rust equivalent — Device trait missing completion polling

**Shared workspace hazards:**
- Other agents (Legolas, Boromir) switch branches — always verify `git branch --show-current` before commit
- Git stash across branches dangerous — stash pop can fail on Cargo.lock conflicts

## Session Log (Compressed)

### AI-4 — Epoch-Amortized UnsafeContext Batch API (2026-07-17)
**Impact:** Upsert 2.83×, Read 9.76×, RMW 9.83× speedup via batch epoch amortization.  
**Files:** `store/session.rs`, `store/kv.rs`, `benches/ycsb.rs`  
**Architecture:** UnsafeContext holds `&'a mut FasterSession<F>`, batch methods take `&FasterKv<F>`. `Refresh()` updates epoch without leaving protection.

---

### Wave 1 K1 — Compaction Scanner (2026-03-06)
**Impact:** Stateless scanner with conservative LIVE classification, `#[deny(unsafe_code)]`.  
**Files:** `compaction/mod.rs`, `compaction/scanner.rs`, `store/kv.rs`  
**Bugs fixed:** Sentinel alignment (must start at `first_data_address()`), RecordInfo(0) is valid.

---

### Precheckin Fix — Formatting + Clippy Sweep (2026-03-09)
**Impact:** 33 files fmt drift, 6 unused imports, 27 undocumented unsafe blocks, nextest hang fix.  
**Fix:** Drop `--all-targets` from nextest.  
**Commit:** `74ce041f`

---

### Wave 4 — Mutation Testing on Critical Modules (2026-03-09)
**Results:** Allocator 79% kill rate, Hash 93% kill rate. 17 targeted tests added.  
**Findings:** Most survivors are equivalent mutations or Debug formatting, not test gaps.

---

### Wave 5 — Lossy LRU Cache Wrap-Around Mode (Session 6)
**Impact:** Wrap-around semantics for hybrid log — upsert/RMW on Truncated addresses allocate fresh records.  
**Tests:** 9 integration tests (eviction, re-upsert, RMW, concurrent).  
**Bugs fixed:** internal_upsert/internal_rmw returned Aborted on Truncated (infinite retry loops).  
**Results:** 1608 existing + 9 new tests pass.

---

### Code Quality Cleanup — Precheckin Gate (2026-03-10)
**Impact:** 17 tests → tier-2 (`#[ignore]`), 9 tests sped up via iteration reduction.  
**Target:** All tier-1 tests <1s in isolation.  
**Results:** 1628 tier-1 tests + 17 tier-2 tests (1645 total).

---

### Loom Shim Integration (2026-03-11)
**Impact:** Wired all 9 production files to `crate::sync` shim for loom testing.  
**Team:** Aragorn (shim), Sam (log_prefix param), Éowyn (DST tiers), Galadriel (miri expansion).  
**Commits:** 229f0890 (shim), fc0d3da9 (Sam), a4525cb5 (Éowyn), 9a7190d0 (Galadriel).  
**Results:** All 1719 tests pass.

---

### Multi-Writer Deadlock Review (2026-07-21)
**Verdict:** APPROVE WITH COMMENTS  
**Findings:** Fix A/B/C correct. Deadlock root cause: timing gap between I/O submission and callback landing. `SyncFileDevice` yields in `poll_completions` (good). `allocate_at_tail` retry loop bounded at 32 (good). `deadlock_tests.rs` strong regression coverage but `#[ignore]`-ed.  
**Action:** Validated fix by running ignored tests (all passed).

---

### Loom CI Compilation Fixes (2026-07-22)
**Impact:** Fixed all 7 loom compilation errors across 7 files.  
**Fixes:**
- `sync.rs`: Added `sleep` shim for loom's thread module (yields instead of sleeping — loom doesn't model time)
- `epoch/table.rs`: Split `register()` into cfg-gated method (non-loom) + associated fn (loom) with shared `register_impl`. Loom's Arc doesn't implement `Receiver`, so `self: &Arc<Self>` is unavailable.
- `allocator.rs`, `epoch/drain.rs`: Replaced `AtomicPtr::get_mut()` with `load(Relaxed)` — loom's AtomicPtr lacks `get_mut`; both sites have `&mut self` guaranteeing exclusive access.
- `device.rs`, `store/pending_io.rs`: Wrapped static atomics in `LazyLock` under `#[cfg(loom)]` — loom atomics aren't const-constructible.
- Updated 3 production call sites to UFCS `EpochTable::register(...)` which works under both cfgs.

**Learnings:**
- Loom's `Arc` doesn't implement `core::ops::Receiver` → `self: &Arc<Self>` breaks under loom. Use cfg-gated associated function pattern.
- Loom's `AtomicPtr` has no `get_mut()`. Use `load(Relaxed)` under exclusive access as the portable alternative.
- Loom atomics aren't const — any `static` initialization must use `LazyLock` or equivalent under loom.
- Loom's thread module has no `sleep` — shim it with `yield_now()` since loom doesn't model wall-clock time.

### Stress Test Binary — stress_5min.rs (2026-07-22)
**Impact:** Created sustained stress test example at `rust/crates/faster-core/examples/stress_5min.rs`.
**Features:** CLI-configurable threads/duration/lossy/key-range/report-interval, YCSB-like mixed workload (50% upsert, 35% read, 10% RMW, 5% delete), per-thread correctness oracle with sanity bounds, throughput monitoring with cliff detection, xorshift64 thread-local RNG (zero external deps).
**Verified:** 4-thread 10s smoke test: ~8.9M ops/s non-lossy, ~9.2M ops/s lossy, 0 violations both modes.
**API notes:** `SimpleFunctions<u64,u64>` Output type is `Option<u64>` (not raw `u64`). `FasterKv::new()` takes `FasterKvConfig` struct directly — builder pattern also available via `FasterKv::<SF>::builder()`.

## Learnings

- `SimpleFunctions<K, V>::Output` is `Option<V>`, not `V` — read/RMW output must be `&mut Option<u64>`.
- For examples, use `std::sync::Arc` (not `crate::sync`) since examples aren't under the loom shim.
- `complete_pending()` should be called periodically in long-running workloads to drain async I/O.
- `InMemoryDevice::new()` completes I/O synchronously — Pending status is rare but still possible during page transitions.
- Batching atomic counter updates (flush every 256 ops) eliminates contention on global counters at 4+ threads.
