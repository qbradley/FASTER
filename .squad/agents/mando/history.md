# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

- **Status vs Error separation:** FASTER's operational outcomes (Ok, Pending, NotFound) are *not* errors — they're control-flow signals. True errors (I/O failure, corruption) go in `FasterError`. This matches C++ FASTER's split between external `Status` and internal error handling, and aligns with Rust `Result` idioms.
- **No thiserror needed for small enums:** Manual `Display`, `Error`, `From` impls are ~40 lines for a 6-variant enum. Avoids a build-time proc-macro dependency. Can always add thiserror later if the enum grows significantly.
- **`OperationStatus` variant set:** The final set merges C++ `Status` (Ok, NotFound, Pending, Aborted), C# `Status` (InPlaceUpdated, Created, CopyUpdated), and architecture `OkKind::Deleted` into one flat enum. This is more ergonomic than nested `Ok(OkKind)` — callers match directly on the variant they care about.

- C++ `Address` uses a union bitfield: 25 bits offset (LSB), 23 bits page, 16 bits reserved (MSB). `kInvalidAddress = 1` (not 0), `kMaxAddress = (1 << 48) - 1`. The Rust `LogicalAddress` matches this exactly with shift/mask ops instead of bitfields.
- `Address::INVALID = 1` (not 0) because all-zeros represents an empty hash bucket entry, while 1 means "address present but not yet valid." This distinction matters for hash bucket initialization.
- The C++ `AtomicAddress` doesn't expose ordering parameters (defaults to `memory_order_seq_cst`). Our Rust `AtomicLogicalAddress` requires explicit `Ordering` — more idiomatic and lets callers choose weaker orderings for performance.

---

## 2026-03-05: Task 1a — Workspace Setup Complete (Mando)

**What:** Created the Rust workspace skeleton under `rust/` at repo root. Five crates: `faster-core`, `faster-device`, `faster-ffi`, `faster-tokio`, `faster-bench`. All config files in place (rustfmt, clippy, deny, gitignore).

**Key details:**
- Rust 2024 edition, MSRV 1.85.0, resolver v3
- `faster-core` deps: crossbeam-utils + cfg-if only (zero async). Dev-deps: proptest, criterion.
- Workspace-level dependency management — all versions pinned once in root Cargo.toml
- Release profile: LTO=thin, codegen-units=1, opt-level=3
- `faster-core/lib.rs` modules: epoch, allocator, hash, record, address, status, error
- All lib.rs files carry `#![deny(unsafe_op_in_unsafe_fn)]`, `#![warn(missing_docs)]`, `#![forbid(clippy::undocumented_unsafe_blocks)]`
- `rustfmt.toml` includes `imports_granularity` and `group_imports` (nightly-only — warns on stable but degrades gracefully)

**Verification:**
- `cargo check --workspace` ✅
- `cargo clippy --workspace -- -D warnings` ✅ zero warnings
- `cargo fmt --check` ✅ clean (nightly fmt options warn but don't fail)

**Learnings:**
- `imports_granularity` and `group_imports` in rustfmt.toml require nightly rustfmt to enforce. On stable they produce warnings but `--check` still passes. CI should use nightly rustfmt or accept the warnings.
- `resolver = "3"` is the correct resolver for edition 2024 workspaces.
- Criterion 0.5.1 is the latest compatible with MSRV 1.85.0 (0.7.0 requires newer).

---

## 2026-03-05T18:33: Thrawn Rust FASTER Architecture Finalized

**What:** Thrawn completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Thrawn-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/thrawn/rust-faster-architecture.md`

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Kenobi:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Mando:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Chirrut:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Jyn:** Simulation framework must model callback completion ordering for testing.
- **Maul:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Cassian:** Phase roadmap and implementation sequence encoded in Part 3.
- **Grievous:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Jyn lead), unsafe audit planning (Maul lead), Phase 1 sprint (Cassian lead), async adapter spike (Kenobi lead).

---

## 2026-03-05: Task 1b — Core Address and Newtype System Complete (Mando)

**What:** Implemented `LogicalAddress`, `AtomicLogicalAddress`, `Page`, and `Offset` types in `rust/crates/faster-core/src/address.rs`. This is the foundation type system that every other module depends on.

**Key details:**
- `LogicalAddress`: `#[repr(transparent)]` newtype over `u64`, 48-bit encoding (25 offset + 23 page), matches C++ `address.h` bit layout exactly
- Constants: `INVALID = 1` (not 0, matching C++), `ZERO = 0`, `MIN_VALID = 2`, `MAX = (1<<48)-1`
- Methods: `new(Page, Offset)`, `page()`, `offset()`, `is_valid()`, `is_null()`, `in_read_cache()`, `from_raw()`, `raw()`
- `AtomicLogicalAddress`: `#[repr(transparent)]` over `AtomicU64`, explicit `Ordering` parameters on all methods
- `Page(u32)` and `Offset(u32)`: typed newtypes with full standard trait implementations
- All public items have doc comments with runnable examples
- Zero `unsafe` code

**Verification:**
- `cargo test -p faster-core` ✅ — 35 unit tests + 11 doc-tests, all passing
- `cargo clippy -p faster-core -- -D warnings` ✅ — zero warnings
- Property-based tests (proptest): round-trip page/offset, raw round-trip, ordering consistency, validity, nullness

**Design decisions:**
- Used typed `Page`/`Offset` newtypes as method parameters instead of raw `u32` — the compiler catches page/offset mix-ups at call sites
- `from_raw()` masks upper 16 bits for safety — prevents hash-table control bits from leaking into address comparisons
- `AtomicLogicalAddress` requires explicit `Ordering` (unlike C++ which defaults to SeqCst) for better performance when callers know weaker ordering suffices

---

## 2025-07-18: Task 1c — Status and Error Types Complete (Mando)

**What:** Implemented `OperationStatus`, `FasterError`, `Result<T>`, and `OperationResult<T>` in `rust/crates/faster-core/src/status.rs` and `rust/crates/faster-core/src/error.rs`.

**Key details:**
- `OperationStatus`: 8-variant enum (Ok, Pending, NotFound, Created, InPlaceUpdated, CopyUpdated, Deleted, Aborted) — merges C++ Status, C# Status, and architecture spec into one flat enum
- Helper predicates: `is_success()`, `is_pending()`, `is_not_found()`, `is_aborted()`, `is_modified()`
- `FasterError`: 6-variant enum (Io, CheckpointError, RecoveryError, InvalidOperation, InternalError, SessionError)
- Manual `Display`, `std::error::Error`, `From<std::io::Error>` impls (no thiserror dependency)
- `Result<T>` alias for `std::result::Result<T, FasterError>`
- `OperationResult<T>` struct with status + optional output, plus `new()`, `into_output()`, `map()` helpers
- All public items have doc comments with runnable examples
- Zero `unsafe` code, zero new dependencies

**Verification:**
- `cargo test -p faster-core` ✅ — 96 unit tests + 31 doc-tests, all passing
- `cargo clippy -p faster-core -- -D warnings` ✅ — zero warnings

**Design decisions (see `.squad/decisions.md` Decision #11):**
- Flat enum instead of nested `Status { Ok(OkKind) }` — more ergonomic pattern matching
- Manual error impls instead of thiserror — keeps dependency count at 2
- `RetryLater` excluded from public API — sessions handle retries internally

---

## 2025-07-18: Task 1f — Record Format and Key/Value Traits Complete (Mando)

**What:** Implemented `RecordInfo` (8-byte packed header), `AtomicRecordInfo`, `Key`/`Value` serialization traits with `FixedSizeKey`/`FixedSizeValue` marker traits, and `RecordLayout` for record offset computation. Created as `record/` directory module with three submodules.

**Key details:**
- `RecordInfo`: `#[repr(transparent)]` over `u64`, bit layout matches C++ `record.h` exactly: `[63:Final][62:Tombstone][61:Invalid][60..48:Version(13)][47..0:PreviousAddress(48)]`
- `AtomicRecordInfo`: wraps `AtomicU64`, provides `set_invalid`/`set_tombstone`/`set_final` via `fetch_or`, plus CAS operations with explicit `Ordering`
- `Key` trait: requires `Hashable + Eq + Clone + Send + Sync + 'static` with safe `serialize(&self, buf: &mut [u8])` / `deserialize(buf: &[u8]) -> Self` interface
- `Value` trait: requires `Clone + Send + Sync + 'static` with same serialization interface
- `FixedSizeKey`/`FixedSizeValue`: marker traits with `const SIZE: usize` for compile-time record sizing
- Implementations for: `u32`, `u64`, `i32`, `i64` (via macro, little-endian), `Vec<u8>` and `String` (length-prefixed)
- `RecordLayout`: computes key/value offsets and total record size with 8-byte alignment
- `pad_alignment()`: port of C++ `pad_alignment` from `auto_ptr.h`
- `write_record`/`read_record_info`/`read_key`/`read_value`: safe record I/O helpers
- Added `Hashable for Vec<u8>` to `hash.rs` (delegates to `[u8]` slice impl)
- Zero `unsafe` code — all serialization via safe byte slices

**Files created/modified:**
- `record/mod.rs` — module docs, re-exports
- `record/record_info.rs` — RecordInfo, AtomicRecordInfo (27 KB)
- `record/traits.rs` — Key, Value, FixedSizeKey, FixedSizeValue + impls (16 KB)
- `record/layout.rs` — RecordLayout, pad_alignment, record R/W helpers (20 KB)
- `hash.rs` — added Hashable for Vec<u8>

**Verification:**
- `cargo test -p faster-core -- record` ✅ — 74 unit tests + 12 doc-tests = 86 new tests, all passing
- All existing tests unaffected (67 address + 28 status/error tests still pass)
- `cargo clippy -p faster-core` ✅ — zero new warnings
- Property tests (proptest): RecordInfo round-trip, Key/Value serialization round-trip, full record write/read round-trip for both fixed-size and variable-length types

**Design decisions (see `.squad/decisions/inbox/mando-record-format.md`):**
- Safe slice-based serialization API (Section 8.2 style) rather than unsafe pointer API (Section 3.2.3 style) — can add unsafe low-level accessors when allocator module needs direct memory access
- 8-byte alignment cap for v1 (all padding to 8 bytes) — simple, matches architecture v1 recommendation, < 8 bytes of waste per field for small types
- `#[repr(transparent)]` for RecordInfo (not `#[repr(C)]`) — correct for single-field newtype over u64
- RecordInfo stores as native u64, serializes as little-endian for on-disk format — matches x86 behavior, correct on big-endian too

## Learnings

- When types implement both Key and Value (e.g., u32), method calls are ambiguous. Tests must use fully-qualified syntax: `Key::serialize(&key, &mut buf)` rather than `key.serialize(&mut buf)`.
- C++ RecordInfo bitfield layout (LSB to MSB): previous_address(48), checkpoint_version(13), invalid(1), tombstone(1), final(1). The C++ naming of "invalid" at bit 61 is slightly confusing since "invalid" usually means "not valid" but in FASTER it means "superseded."
- `pad_alignment(size, alignment)` uses `(size + (alignment-1)) & !(alignment-1)` — this is a classic bit trick that works for any power-of-two alignment and correctly handles `size = 0` (returns 0).
- **Epoch initial value must be 1, not 0**: 0 is reserved as the "inactive" sentinel in per-thread `local_current_epoch`. Starting at 0 would make a freshly-protected thread look inactive.
- **`core::array::from_fn` avoids clippy `declare_interior_mutable_const`**: Initializing arrays of atomics via `const INIT: AtomicBool = ...` triggers clippy's interior mutability lint. `core::array::from_fn(|_| AtomicBool::new(false))` is the idiomatic workaround.
- **Drain callbacks must execute outside the drain list lock**: If a callback calls `bump_current_epoch`, it will try to acquire the drain list lock again. Collecting actions under the lock and executing after release prevents deadlocks.
- **`compute_safe_epoch` race with `protect` is acceptable**: A thread in the middle of `protect()` (local=0 but about to store epoch) can be missed by `compute_safe_epoch`, making safe_epoch temporarily higher. This is safe because the thread hasn't started reading data yet, and drain callbacks free superseded data, not data at the current epoch.
- **Hash table tentative-bit duplicate detection is tag-level, not key-level**: The `find_or_create_entry` re-scan after tentative CAS only catches *committed* duplicates. Two threads can both create tentative entries with the same tag in different slots. The log-level key comparison (in the caller) resolves true duplicates. This matches C++ FASTER behavior.
- **`get_unchecked` for bucket lookup is justified**: The bucket index is always `hash & (num_buckets - 1)` which is strictly `< num_buckets == buckets.len()`. The bounds check elimination saves a branch on every hash probe.


## 2025-07-18: Task 2d — Hash Table Core Complete (Mando)

**What:** Implemented the latch-free concurrent hash table — FASTER's primary data structure. Created `hash_table.rs` with `HashTable`, `FindOrCreateResult`, and the three core operations: `find_entry`, `find_or_create_entry`, and `update_entry`.

**Key details:**
- `HashTable`: owns `Box<[HashBucket]>` (power-of-2 sized) + `OverflowBucketPool`
- `new(log2_size)`: validates range 1..=30, allocates zeroed buckets
- `bucket(hash)`: branchless lookup via `get_unchecked` with documented safety invariant
- `find_entry(hash)`: scans bucket chain for committed (non-tentative) entry matching tag
- `find_or_create_entry(hash, addr)`: two-phase CAS protocol — reserves tentative slot, re-scans for duplicates, returns `FindOrCreateResult` with entry + slot reference + created flag
- `update_entry(slot, old, new)`: CAS-based entry update (commit, address update, or abort)
- `entry_count()`: O(N) diagnostic scan
- `overflow_count()`: delegates to pool's `allocated_count()`
- Added `allocated_count()` helper to `OverflowBucketPool`
- Every atomic ordering choice documented inline
- Single `unsafe` block (`get_unchecked` in bucket lookup) with full safety proof

**Files created/modified:**
- `hash_table.rs` — HashTable struct + all operations + comprehensive tests (new)
- `overflow.rs` — added `allocated_count()` method
- `lib.rs` — added `pub mod hash_table`

**Verification:**
- `cargo test -p faster-core` ✅ — 362 lib tests + 64 doc-tests = 426 total, all passing
- `cargo clippy -p faster-core -- -D warnings` ✅ — zero warnings
- 30 new hash_table tests: construction (4), bucket lookup (2), find_entry (4), find_or_create (4), update_entry (2), abort (1), overflow (2), statistics (1), property tests (3), concurrent tests (4) — including 8-thread same-key, 8-thread overlapping keys, 16-thread mixed read/write, 8-thread high-contention small table
- All 332 pre-existing tests still pass — zero regressions


## 2026-03-05T19:20: Wave 1 Sprint Complete — 1b + 1c + 1d + 1e Done

**Context:** All 4 Wave 1 tasks executed in parallel by full team.

**Your Deliverables (1b + 1c):**
- Task 1b: LogicalAddress with Page/Offset newtypes — 46 tests passing
- Task 1c: OperationStatus + FasterError + Result/OperationResult types — cumulative 127 tests passing

**Decisions Merged:**
- Decision #10: Typed Page/Offset Newtypes — prevents argument transposition via Rust type system
- Decision #11: Status and Error Type Design — flat enum, manual impls, OperationResult struct
- Decision #13: `rust/` workspace directory naming (matches cc/, cs/ convention)

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-mando.md`

**What's Next:**
- Task 1f (Record Format) — ready to start, depends on 1b/1c/1d ✅
- 1f will use `LogicalAddress`, `Page`, `Offset`, `OperationStatus`, `FasterError` directly
- Your second-pass focus: record serialization and checkpoint format integration

**Team Status:**
- Chirrut (1d): Hash traits complete, 64 tests passing
- Rex (1e): CI skeleton complete, 5-job GitHub Actions pipeline validated locally
- All 127 tests passing in workspace. Zero regressions.

---

## 2025-07-18: Task 1g — Epoch-Based Reclamation System Complete (Mando)

**What:** Implemented the full epoch-based reclamation system: `EpochTable` (central coordination), `EpochEntry` (per-thread slots), `DrainList` (deferred callbacks), `EpochGuard` (RAII protection), and `EpochThread` (per-thread handle). This is the coordination backbone for all concurrent operations.

**Key details:**
- `EpochTable`: global `current_epoch` (AtomicU64, SeqCst bump), `safe_to_reclaim_epoch`, 256-slot cache-line-padded thread table, drain list, mutex-protected free list for registration
- `EpochEntry`: `local_current_epoch` (Release/Acquire), `reentrant` counter (Relaxed, single-writer), `thread_id`, `phase_finished[8]` (checkpoint stub), all `#[repr(C)]`
- `DrainList`: `Mutex<Vec<DrainAction>>` — callbacks collected under lock, executed after release to prevent deadlocks (callbacks can call `bump_current_epoch`)
- `EpochGuard`: RAII protect/unprotect, `!Send` via `PhantomData<*const ()>` (thread-affine), reentrant (nested guards increment counter), `refresh()` for long operations
- `EpochThread`: holds `Arc<EpochTable>` + entry index, auto-deregisters on Drop, explicit `unregister()` available
- `LightEpoch` type alias for C++ naming compatibility
- Every atomic operation has documented memory ordering rationale in comments
- Zero `unsafe` code

**Files created:**
- `epoch/mod.rs` — module docs, re-exports, constants
- `epoch/entry.rs` — EpochEntry (per-thread slot)
- `epoch/drain.rs` — DrainList + drain action queue
- `epoch/table.rs` — EpochTable (core coordination)
- `epoch/guard.rs` — EpochGuard (RAII) + EpochThread (handle)
- `epoch/tests.rs` — comprehensive test suite

**Verification:**
- `cargo test -p faster-core` ✅ — 286 unit tests + 54 doc-tests = 340 total, all passing
- `cargo clippy -p faster-core -- -D warnings` ✅ — zero warnings
- 39 epoch-specific tests: basic ops, reentrance (2-3 deep), drain callbacks (immediate, delayed, ordered, nested), guard RAII (including panic unwind), registration limits (256 + overflow), concurrent stress (16-thread protect/unprotect, 8-thread bump+protect, monotonicity checks), property tests (proptest: random ops, symmetry, invariants)
- All existing tests unaffected (247 pre-existing tests still pass)

**Design decisions (see `.squad/decisions/inbox/mando-epoch-system.md`):**
- Custom epoch over crossbeam-epoch: FASTER needs drain callbacks and phase coordination, not pointer-level GC
- Mutex-based DrainList: acceptable because push/drain are rare (not on hot path); prevents deadlocks via lock-then-release-then-execute pattern
- `Relaxed` load of `current_epoch` in `protect()`: conservative (stale epoch → lower safe epoch → delayed drains → safe)
- `SeqCst` for `bump_current_epoch`: total ordering required to prevent stale-epoch race with local stores


---

### Task: cache_store.rs example (Rust equivalent of C# CacheStore sample)
**Date:** $(date +%Y-%m-%d)
**Requested by:** qbradley

**Summary:**
Created `rust/crates/faster-core/examples/cache_store.rs` — a Rust port of the C# `cs/samples/CacheStore` sample demonstrating FASTER as a disk-backed cache/KV store.

**What it demonstrates:**
- SyncFileDevice-backed store creation with 1M hash buckets
- Bulk population of 1M key-value pairs with throughput reporting
- Random-read workload with correct `OperationStatus::Pending` handling (drain every 100 pending, sync drain at end)
- Interactive read mode with per-key latency reporting
- `--evict` flag to flush/evict data to disk before reads
- Proper session lifecycle (`complete_pending` before `dispose_session`)

**Key decisions:**
- Used inline xorshift64 PRNG instead of adding `rand` dependency (not in dev-deps)
- Command-line args (`--evict`, `--interactive`) instead of stdin prompts for mode selection
- Matched C# sample's numbers: 1M keys, progress every 2^19 records, drain every 100 pending

**Verification:**
- `cargo build -p faster-core --example cache_store` ✅ — zero warnings
- `cargo run -p faster-core --example cache_store` ✅ — full run: 419K inserts/sec, 440K reads/sec
- `cargo run -p faster-core --example cache_store -- --evict` ✅ — evict mode runs cleanly

## Learnings

### 2025-01-27: Unsafe Code Audit & Safe Abstraction Design

**Context:** Comprehensive analysis of ~41K LOC FASTER Rust implementation to minimize unsafe code without performance regression.

**Key Findings:**
1. **Unsafe inventory:** 157 total sites (115 blocks, 27 functions, 15 Send/Sync impls)
2. **60% reducible:** 94 sites can be eliminated through safe abstractions with zero performance cost
3. **40% necessary:** 63 sites are genuinely required (allocator internals, I/O callbacks, hot-path optimizations)

**Patterns identified:**
- **Slice construction** (18 sites): `from_raw_parts` → safe `as_slice()` methods on owning types
- **AtomicPtr loads** (15 sites): Raw pointer dereference → encapsulated accessor methods with documented invariants
- **Record access** (22 sites): Raw pointer manipulation → lifetime-bound `RecordView<'page>` safe abstraction
- **Page allocation** (12 sites): Manual `alloc::alloc` → refactor to use existing `AlignedBuffer` type
- **Device I/O** (25 sites): Raw callbacks → type-safe `TypedIoContext<T>` wrapper for common cases
- **Hot-path bounds checks** (3 sites): Keep `get_unchecked` for hash table lookups (provably safe, 1-cycle overhead if removed)

**Safe abstraction designs:**
1. **`RecordView<'page>` / `RecordViewMut<'page>`**: Lifetime-bound zero-copy views into page memory. Eliminates 22 unsafe sites in `hybrid_log/record_ops.rs`.
2. **`PageFrame` using `AlignedBuffer`**: Replace manual sector-aligned allocation with safe buffer type. Eliminates 12 sites.
3. **`PageDirectory<T>` safe refactor**: Encapsulate growable pointer array in `Box<[AtomicPtr]>`. Eliminates 13 sites.
4. **`TypedIoContext<T>`**: Safe callback wrapper for device I/O. Makes 13 call sites safe while keeping low-level API raw.

**Performance guarantees:**
- All abstractions inline to identical assembly (verified strategy with `cargo-asm`)
- Hot-path `get_unchecked` remains unsafe by design (provably redundant bounds check)
- Zero-cost slice references compile to same code as raw pointers
- Benchmark gate: <1% variance on `faster-bench` suite required for PR approval

**Phased rollout plan:**
- **Phase 1 (1 week):** Quick wins — safe slice constructors, AtomicPtr encapsulation (35 sites)
- **Phase 2 (2-3 weeks):** Abstraction types — RecordView, PageFrame refactor, PageDirectory (47 sites)
- **Phase 3 (1 week):** Device I/O safe wrappers (13 sites)
- **Phase 4 (3 days):** Documentation sweep — SAFETY comments for all remaining 63 sites
- **Phase 5 (stretch):** Module-level `#[forbid(unsafe_code)]` boundaries (4+ modules)

**Dependencies evaluated:**
- `zerocopy` / `bytemuck`: Not needed (0 transmute sites currently)
- `crossbeam-epoch`: Our custom impl is simpler and faster (no change)

**Risk mitigations:**
- Benchmark before/after on every PR (perf gate: <1% regression)
- Start with low-risk refactors to build confidence
- Keep unsafe in hot paths where provably necessary (hash table lookups)
- Gradual migration — don't force lifetime complexity where unsafe is clearer

**Impact:** Post-cleanup, FASTER Rust will have 60% less unsafe code, all remaining unsafe well-documented and isolated, with zero performance regression. This positions the codebase for easier auditing, safer evolution, and potential formal verification of safe abstractions.

**Next action:** Review with team, then begin Phase 1 PRs (safe slice constructors).

### Phase 1A — Safe Slice Constructors (Completed)

**Date:** 2026-03-06
**Files changed:** `record_ops.rs`, `page.rs`, `flush.rs`

**Changes made:**
1. `MutableRecordAccessor::key_ref()` / `value_ref()` — replaced inline `from_raw_parts` with safe `self.as_slice()` delegation (2 blocks removed)
2. `MutableRecordAccessor::value_mut_ptr()` — replaced `unsafe { ptr.add() }` with `self.as_mut_slice()[offset..].as_mut_ptr()` (1 block removed)
3. `LogRecordReader::read_record_info()` — replaced inline `from_raw_parts` with `RecordAccessor::new()` + `.record_info()` (better encapsulation, same count)
4. `LogRecordReader::read_header_and_match_key()` — collapsed 2 unsafe blocks into 1 by using a single `RecordAccessor` for both header and key reads (1 block removed)
5. `PageFrame::as_mut_slice()` — promoted from `unsafe fn` to safe fn (takes `&mut self`, exclusive access guaranteed by borrow checker). Removed 2 `unsafe {}` blocks at test call sites.
6. `flush.rs::flush_page_sync()` — replaced `unsafe { from_raw_parts(frame.as_ptr(), ...) }` with safe `frame.as_slice()[..n]` (1 block removed)

**Result:** 7 unsafe blocks eliminated (38 → 31 across the 3 files). All 1161 tests pass, clippy clean, zero API changes.

**What must remain unsafe:**
- `RecordAccessor::new()` / `MutableRecordAccessor::new()` — raw pointer construction with caller-guaranteed validity
- `RecordAccessor::atomic_record_info()` / `MutableRecordAccessor::atomic_record_info()` — raw pointer cast to `&AtomicRecordInfo`
- `RecordAccessor::as_slice()` / `MutableRecordAccessor::as_slice()` / `as_mut_slice()` — foundational `from_raw_parts` on owned raw pointers (these ARE the safe wrappers)
- `PageFrame::zero()` — takes `&self` (not `&mut self`) because called through shared refs in PageTable; CAS guards exclusivity
- `PageFrame::as_slice()` — internal `from_raw_parts` over owned `NonNull`


---

## Phase 2A — RecordView Safe Abstraction + MutableRecordAccessor Cleanup

**Date:** 2026-03-06
**Files changed:** `record_ops.rs`, `operations.rs`, `kv.rs`

**Changes made:**
1. **RecordAccessor::from_log() factory** — new safe `pub(crate)` method that encapsulates the allocator→ptr→RecordAccessor unsafe construction. Consolidated 3 separate unsafe blocks in `LogRecordReader` (get_record, read_record_info, read_header_and_match_key) into 1 centralized unsafe in the factory. Net: -2 unsafe blocks.
2. **Fixed SF-2 Stacked Borrows violation** — changed `MutableRecordAccessor` write methods (`write_record_info`, `write_key`, `write_value`, `write_full_record`, `value_mut_ptr`, `as_mut_slice`) from `&self` to `&mut self`. This fixes the soundness issue where `as_mut_slice(&self)` created `&mut [u8]` from a shared reference, violating Stacked Borrows. Removed `#[allow(clippy::mut_from_ref)]` and the SF-2 TODO comment.
3. **Added safe `MutableRecordAccessor::zero(&mut self)`** — `self.as_mut_slice().fill(0)`, completely safe thanks to the `&mut self` receiver. No unsafe needed.
4. **Derived `atomic_record_info()` from `as_slice()` on both types** — pointer cast now derives from the validated slice rather than from the raw `self.ptr` field directly. Reduces the number of methods that touch raw pointers.
5. **Updated all callers** in `operations.rs` (9 sites) and `kv.rs` (4 sites) to use `mut accessor` bindings.

**unsafe in record_ops.rs:** 12 → 10 (2 blocks eliminated via from_log() consolidation)
**Tests:** 1161 passed, 3 skipped. Clippy clean.

**Key insight:** The biggest safety win was fixing SF-2. The `&self` → `&mut self` change on write methods doesn't reduce the unsafe _count_, but it fixes a real soundness issue (Stacked Borrows violation) and removes the need for `#[allow(clippy::mut_from_ref)]`.

**What remains unsafe in record_ops.rs:**
- `RecordAccessor::new()` / `MutableRecordAccessor::new()` — raw pointer construction (must remain unsafe fn)
- `RecordAccessor::from_log()` — 1 centralized unsafe block calling `new()`
- `atomic_record_info()` on both types — pointer cast to `&AtomicRecordInfo`
- `as_slice()` on both types — foundational `from_raw_parts`
- `as_mut_slice()` on MutableRecordAccessor — `from_raw_parts_mut`
- `LogRecordWriter::allocate_record()` — `MutableRecordAccessor::new()` call (freshly allocated region)
- 1 test unsafe (direct `MutableRecordAccessor::new()` for in-place update test)

---

## Phase 3: Device I/O Safe Callback Wrappers

**What:** Introduced `TypedIoContext<T>` in `device.rs` — a zero-cost wrapper that encapsulates the `Box → raw pointer → typed reconstruct` lifecycle used by all device I/O completion callbacks. Refactored flush.rs and pending_io.rs to use it.

**Changes:**
- **device.rs**: Added `TypedIoContext<T>` with `new()`, `as_raw()`, safe `reclaim()`, and `unsafe from_raw()`. Exported via `lib.rs`.
- **flush.rs**: `flush_page()` error paths (QueueFull, Error) now use `io_ctx.reclaim()` instead of `unsafe { Box::from_raw(...) }`. Callback uses `TypedIoContext::from_raw()`.
- **pending_io.rs**: `issue_read()` error paths now use `io_ctx.reclaim()` instead of `unsafe { Box::from_raw(...) }`. Callback uses `TypedIoContext::from_raw()`.

**Unsafe block count (across 4 target files):**
| File | Before | After | Delta |
|------|--------|-------|-------|
| device.rs | 11 | 13 | +2 (encapsulated in TypedIoContext) |
| sync_file_device.rs | 6 | 6 | 0 |
| flush.rs | 6 | 4 | −2 |
| pending_io.rs | 5 | 3 | −2 |
| **Total** | **28** | **26** | **−2 net** |

**Overall crate unsafe blocks:** 102 → 100

**What stayed unsafe and why:**
- `Device::read_async`/`write_async` — FFI boundary, raw pointer contracts
- `flush_completion_callback`/`read_completion_callback` — receive `*mut u8` from device, must remain `unsafe fn`
- `TypedIoContext::from_raw` — callback boundary, must be called exactly once
- `unsafe impl Send for IoRequest` / `FlushCallbackContext` — justified by lifetime contracts
- `execute_request` raw pointer slicing — inherent to the buffer contract

**Tests:** 1161 passed, 3 skipped (unchanged). Clippy clean.

**Learnings:**
- The callback pattern (Box::into_raw → pass to device → Box::from_raw in callback) appears in every async I/O call site. TypedIoContext centralizes the invariants.
- Error-path reclaim is the biggest win: callers no longer need to remember the cast type in `Box::from_raw(ptr as *mut T)` — `reclaim()` is type-safe and requires no unsafe.
- TypedIoContext intentionally has no Drop impl: on the success path, ownership transfers to the I/O subsystem silently.
