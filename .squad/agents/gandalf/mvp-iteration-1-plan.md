# MVP Iteration 1: Foundation + Hash Index
## Detailed Bottom-Up Implementation Plan

**Author:** Gandalf (Lead / System Architect)  
**Date:** 2026-03-05  
**Requested by:** qbradley  
**Status:** APPROVED FOR EXECUTION  
**Architecture Reference:** `.squad/agents/gandalf/rust-faster-architecture.md` (Sections 2–4, 6.1, 6.9, 12)

---

## Governing Principles

These come directly from qbradley's directives:

1. **Iterative MVP:** Build the smallest useful foundation first. Each iteration must be performant, secure, correct, maintainable, and useful within its scope.
2. **Bottom-up execution:** Given our top-down architecture, implement bottom-up — leaf types first, composites second, integration last.
3. **Quality at every step:** Even a single newtype wrapper must have tests, docs, clippy-clean code, and correct `Debug`/`Display`. No "we'll fix it later."
4. **PAW consideration:** Complex multi-file work items should use PAW (phased-agent-workflow). Simple type definitions and thin modules can be direct implementation.

---

## Team Roster (Active for Iteration 1)

| Agent | Role | Primary Assignments |
|-------|------|-------------------|
| **Aragorn** | Rust Expert | Core implementation lead — epoch, allocator, hash index |
| **Sam** | Systems Programming | Cache-line alignment, atomics, memory layout |
| **Boromir** | QA Engineer | CI pipeline, test infrastructure, property tests |
| **Frodo** | Reverse Engineer | Behavioral spec extraction from C++/C# for validation |
| **Saruman** | C++ Expert | Reference consultation — epoch, allocator, hash index |
| **Galadriel** | Security Expert | Unsafe audit — atomics, pointer arithmetic |
| **Legolas** | Performance Guru | Early micro-benchmarks for epoch + hash index |
| **Gandalf** | Lead Architect | Review authority on all PRs |

---

## Phase 1: Foundation

**Architecture Reference:** Section 12, Phase 1 (lines 5084–5126)  
**Duration:** 3–4 weeks  
**Goal:** Establish the Rust workspace, core types, epoch system, memory allocator, and record format. Everything Phase 2+ depends on lives here.

### Dependency Graph (Phase 1 Internal)

```
                    ┌──────────────┐
                    │  1a: Workspace │ ← Must be first (everything depends on cargo build)
                    │     Setup      │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┬─────────────────┐
              ▼            ▼            ▼                 ▼
     ┌────────────┐ ┌───────────┐ ┌──────────┐   ┌────────────┐
     │ 1b: Core   │ │ 1c: Status│ │ 1d: Hash │   │ 1e: CI     │
     │  Address & │ │  & Error  │ │  Utility │   │  Skeleton  │
     │  Newtypes  │ │  Types    │ │  Traits  │   │            │
     └─────┬──────┘ └─────┬─────┘ └────┬─────┘   └────────────┘
           │              │             │
           ▼              │             │
     ┌──────────────┐     │             │
     │ 1f: Record   │◄────┘             │
     │  Format      │                   │
     └─────┬────────┘                   │
           │                            │
     ┌─────┴─────────────┐              │
     ▼                   ▼              ▼
┌──────────┐     ┌────────────────┐ ┌────────────────┐
│ 1g: Epoch│     │ 1h: Memory     │ │ 1i: Foundation │
│  System  │     │  Allocator     │ │  Tests &       │
│          │     │(MallocFixed)   │ │  Benchmarks    │
└──────────┘     └────────────────┘ └────────────────┘
```

**Parallelism:** After 1a completes, items 1b/1c/1d/1e can run in parallel. Items 1f depends on 1b+1c. Items 1g and 1h depend on 1b+1f. Item 1i depends on all prior items.

---

### 1a: Workspace Setup

**What:** Create the Rust workspace with crate scaffolding, Cargo.toml configuration, rustfmt/clippy settings, and .gitignore. This is the skeleton everything builds on.

**Who:**
- **Primary:** Boromir (QA — CI and project structure)
- **Support:** Aragorn (Cargo.toml dependencies, edition settings)
- **Review:** Gandalf

**Files to Create:**
```
faster-rs/                          # New directory at repo root
├── Cargo.toml                      # Workspace root: members = ["crates/*"]
├── rustfmt.toml                    # Team formatting rules
├── clippy.toml                     # Clippy lint configuration
├── .gitignore                      # /target, *.swp, etc.
├── deny.toml                       # cargo-deny config (license + advisory)
├── crates/
│   ├── faster-core/
│   │   ├── Cargo.toml              # deps: crossbeam-utils, cfg-if ONLY
│   │   └── src/
│   │       ├── lib.rs              # pub mod declarations, #![deny(unsafe_op_in_unsafe_fn)]
│   │       ├── epoch/mod.rs        # placeholder
│   │       ├── index/mod.rs        # placeholder
│   │       ├── log/mod.rs          # placeholder
│   │       ├── alloc/mod.rs        # placeholder
│   │       ├── ops/mod.rs          # placeholder
│   │       ├── session/mod.rs      # placeholder
│   │       ├── checkpoint/mod.rs   # placeholder
│   │       └── util/mod.rs         # placeholder
│   ├── faster-device/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs              # Device trait placeholder
│   └── faster-bench/
│       ├── Cargo.toml
│       └── src/main.rs             # placeholder
└── tests/                          # Integration test directory
    └── smoke.rs                    # Minimal smoke test (imports compile)
```

**Key Configuration Decisions:**
- `edition = "2024"` (latest stable Rust edition)
- `rust-version = "1.85.0"` (MSRV — first edition 2024 release)
- `#![deny(unsafe_op_in_unsafe_fn)]` in faster-core lib.rs
- `#![warn(missing_docs)]` in faster-core lib.rs
- `lto = true` and `codegen-units = 1` in release profile for benchmarks
- `crossbeam-utils = "0.8"` and `cfg-if = "1"` as only faster-core dependencies
- `proptest` and `criterion` as dev-dependencies

**Dependencies:** None — this is the root.

**Success Criteria:**
- [ ] `cargo build --workspace` succeeds
- [ ] `cargo test --workspace` succeeds (smoke test)
- [ ] `cargo clippy --workspace -- -D warnings` zero warnings
- [ ] `cargo fmt --check` clean
- [ ] Module structure matches architecture Section 2.1

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — straightforward scaffolding, direct implementation.

---

### 1b: Core Address and Newtype System

**What:** Implement the foundational type system: `LogicalAddress`, `AtomicLogicalAddress`, `PageOffset`, `Epoch`, `HashTag`, `BucketIndex`. These are the vocabulary types used everywhere. Every type needs `Debug`, `Display`, `Clone`, `Copy`, `PartialEq`, `Eq`, and where applicable `PartialOrd`/`Ord`/`Hash`. All use `#[repr(transparent)]` for zero-cost.

**Who:**
- **Primary:** Aragorn (Rust Expert — trait impls, const generics)
- **Support:** Sam (alignment validation, repr correctness)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/util/
├── mod.rs                          # pub mod address, epoch_id, hash_tag, ...
├── address.rs                      # LogicalAddress, AtomicLogicalAddress
├── constants.rs                    # PAGE_SIZE_BITS, DEFAULT_PAGE_SIZE, etc.
└── newtypes.rs                     # PageOffset, Epoch(u64), HashTag(u16), BucketIndex(u64)
```

**Detailed Specification (from Architecture §3.4):**

`LogicalAddress`:
- `#[repr(transparent)]` wrapper over `u64`
- `INVALID = Self(0)`, `MIN_VALID = Self(1)`
- Methods: `new(page: u32, offset: u32)`, `page() -> u32`, `offset() -> u32`, `is_valid() -> bool`
- Page/offset split determined by `PAGE_SIZE_BITS` (compile-time configurable, default 25 for 32MB)
- Implements: `Clone`, `Copy`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, `Debug`, `Display`

`AtomicLogicalAddress`:
- `#[repr(transparent)]` wrapper over `AtomicU64`
- Methods: `load(Ordering)`, `store(LogicalAddress, Ordering)`, `compare_exchange(...)`
- No `Clone`/`Copy` (atomics are not copyable)

`Epoch`:
- Newtype over `u64`. Monotonically increasing.
- `Debug`, `Display`, `Clone`, `Copy`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`

`HashTag`:
- Newtype over `u16`. Only lower 14 bits valid.
- Constructor validates: `debug_assert!(val < (1 << 14))`
- `from_hash(hash: u64) -> HashTag` — extracts bits 48..61

`BucketIndex`:
- Newtype over `u64`.
- `from_hash(hash: u64, size_mask: u64) -> BucketIndex`

**Dependencies:** 1a (workspace must build)

**Success Criteria:**
- [ ] All newtypes compile to zero-cost wrappers (verified via `size_of` assertions)
- [ ] `LogicalAddress::new(page, offset).page() == page` for all valid inputs (property test)
- [ ] `LogicalAddress::new(page, offset).offset() == offset` for all valid inputs (property test)
- [ ] `HashTag::from_hash(h)` extracts correct bits (exhaustive test for boundary values)
- [ ] `AtomicLogicalAddress` CAS semantics match `AtomicU64` behavior
- [ ] `#[doc]` on every public item

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — pure type definitions with simple tests. Direct implementation.

---

### 1c: Status and Error Types

**What:** Implement the operation result model (`Status`, `OkKind`, `FasterError`, `OperationResult`) and the pending result type. These are used by every operation in the system.

**Who:**
- **Primary:** Aragorn (Rust Expert — thiserror, enum design)
- **Review:** Gandalf, Arwen (future API ergonomics)

**Files to Create/Modify:**
```
crates/faster-core/src/util/
├── status.rs                       # Status, OkKind enums
└── error.rs                        # FasterError, OperationResult type alias
```

**Detailed Specification (from Architecture §6.1, §6.9):**

```rust
// status.rs
pub enum Status { Ok(OkKind), Pending, NotFound, RetryLater }
pub enum OkKind { ReadSuccess, InPlaceUpdated, CreatedRecord, Deleted }

// error.rs
pub type OperationResult = Result<Status, FasterError>;

#[derive(Debug, thiserror::Error)]
pub enum FasterError {
    #[error("I/O error on device: {0}")]
    Io(#[from] std::io::Error),
    #[error("Record corruption at address {address:#x}")]
    Corruption { address: u64 },
    #[error("Session is not active")]
    SessionInactive,
    #[error("Epoch protection violated")]
    EpochViolation,
    #[error("Checkpoint error: {0}")]
    Checkpoint(String),
    #[error("Allocator exhausted")]
    AllocatorExhausted,
}
```

Note: `thiserror` is the one additional dependency for `faster-core` beyond `crossbeam-utils` and `cfg-if`. This is acceptable — it's a proc-macro with zero runtime cost, generates `Display` + `Error` impls. The alternative (manual impls) adds boilerplate without benefit.

**Dependencies:** 1a (workspace must build)

**Success Criteria:**
- [ ] `Status` and `FasterError` are `Send + Sync`
- [ ] `OperationResult` works with `?` operator
- [ ] `FasterError::from(io::Error)` conversion works
- [ ] All variants have `#[doc]` explaining when they occur
- [ ] `Display` output is human-readable for every variant

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — simple enum definitions. Direct implementation.

---

### 1d: Hash Utility Traits

**What:** Define the pluggable hash function trait and default implementation. The hash function feeds both the hash index (bucket selection + tag extraction) and is critical for distribution quality.

**Who:**
- **Primary:** Aragorn
- **Support:** Legolas (hash quality benchmarking)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/util/
└── hash.rs                         # Hasher trait, default impl, tag extraction
```

**Specification:**
- Trait `FasterHasher`: `fn hash_key<K: FasterKey>(key: &K) -> u64`
- Default implementation using `std::hash::Hasher` pathway
- Standalone functions: `bucket_index(hash: u64, size_mask: u64) -> BucketIndex` and `tag_from_hash(hash: u64) -> HashTag`
- Consider `ahash` as an optional dependency behind a feature flag for production quality hashing (v1 can use the std DefaultHasher)

**Dependencies:** 1a (workspace), 1b (HashTag, BucketIndex newtypes)

**Success Criteria:**
- [ ] Hash distribution test: 1M random u64 keys → bucket distribution within 5% of uniform (chi-squared)
- [ ] Tag extraction matches architecture spec: bits 48..61
- [ ] No collisions in tag space for known edge cases (0, u64::MAX, sequential)

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — single file, simple logic.

---

### 1e: CI Skeleton

**What:** GitHub Actions CI pipeline with the commit-level checks from Architecture §11.7: `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check`. Multi-platform: Linux x86_64 + macOS aarch64.

**Who:**
- **Primary:** Boromir (QA — CI/CD)
- **Support:** Aragorn (toolchain config)
- **Review:** Gandalf

**Files to Create:**
```
.github/
└── workflows/
    └── ci.yml                      # On push + PR: build, test, clippy, fmt
```

**Pipeline Stages (Commit Level < 5 min):**
1. `cargo fmt --check` (fail fast on formatting)
2. `cargo clippy --workspace -- -D warnings` (lint)
3. `cargo build --workspace` (compile check)
4. `cargo test --workspace` (unit + integration tests)
5. Matrix: `ubuntu-latest` (x86_64) + `macos-latest` (aarch64)
6. Rust toolchain: stable + nightly (nightly for Miri in future)

**Dependencies:** 1a (workspace must exist)

**Success Criteria:**
- [ ] CI runs on every push and PR to `main`
- [ ] All 4 stages pass (fmt, clippy, build, test)
- [ ] Both Linux and macOS matrix entries pass
- [ ] Pipeline completes in < 5 minutes
- [ ] Badge in workspace README shows CI status

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — YAML configuration, direct implementation.

---

### 1f: Record Format and Key/Value Traits

**What:** Implement `RecordInfo` (8-byte packed header), the record memory layout, and the `FasterKey`/`FasterValue` traits with blanket implementations for `Copy` types. This defines how data is stored in the hybrid log.

**Who:**
- **Primary:** Aragorn (Rust Expert — trait design, unsafe ser/deser)
- **Support:** Sam (memory layout, alignment validation)
- **Consulting:** Saruman (C++ record format reference)
- **Review:** Gandalf, Galadriel (unsafe audit)

**Files to Create/Modify:**
```
crates/faster-core/src/log/
├── record_info.rs                  # RecordInfo (8-byte header)
├── record_layout.rs                # Record layout calculation, alignment
└── traits.rs                       # FasterKey, FasterValue traits + blanket impls
```

**Detailed Specification (from Architecture §3.2):**

`RecordInfo`:
- `#[repr(transparent)]` over `u64`
- Bit layout: `[63:Final][62:Tombstone][61:Invalid][60..48:Version(13)][47..0:PreviousAddress(48)]`
- Methods: `previous_address()`, `version()`, `is_invalid()`, `is_tombstone()`, `is_final()`
- Mutation: `set_invalid()`, `set_tombstone()` — these use `AtomicU64::fetch_or` when in mutable region
- Atomic variant: `AtomicRecordInfo` wrapping `AtomicU64`

`FasterKey` trait:
- `hash() -> u64`, `serialized_size() -> usize`, `unsafe serialize_to(*mut u8)`, `unsafe deserialize_from(*const u8) -> Self`
- Blanket impl for `T: Copy + Eq + Hash + Sized + 'static`

`FasterValue` trait:
- Same pattern as FasterKey minus `hash()` and `Eq`
- Blanket impl for `T: Copy + Clone + Sized + 'static`

Record layout:
- `fn record_size<K: FasterKey, V: FasterValue>(key: &K, value: &V) -> usize`
- `unsafe fn write_record<K, V>(dst: *mut u8, info: RecordInfo, key: &K, value: &V)`
- `unsafe fn read_record_info(src: *const u8) -> RecordInfo`
- `unsafe fn read_key<K: FasterKey>(src: *const u8) -> K`
- `unsafe fn read_value<V: FasterValue>(src: *const u8, key_size: usize) -> V`

**Dependencies:** 1a (workspace), 1b (LogicalAddress for PreviousAddress), 1c (for error integration)

**Success Criteria:**
- [ ] `RecordInfo` is exactly 8 bytes (`size_of::<RecordInfo>() == 8`)
- [ ] Bit extraction round-trips: set all fields → extract → values match (property test, 100K iterations)
- [ ] Record write → read round-trip correct for: `u64` key + `u64` value, `u32` key + `[u8; 128]` value
- [ ] Blanket impls work for primitive types: `u64`, `i32`, `u128`, `[u8; 32]`
- [ ] `unsafe` blocks documented with safety invariants
- [ ] Miri clean: `cargo +nightly miri test` on record serialization tests

**Estimated Complexity:** M (Medium)  
**PAW Candidate:** Yes — multiple files, unsafe code, needs careful design + safety review. Good PAW candidate for structured implementation + review cycle.

---

### 1g: Epoch-Based Reclamation System

**What:** Full `LightEpoch` implementation: per-thread epoch tracking table, drain list, safe-to-reclaim computation, RAII `EpochGuard`, thread registration/deregistration. This is the coordination backbone of the entire system.

**Who:**
- **Primary:** Aragorn (Rust Expert — atomics, lock-free programming)
- **Support:** Sam (cache-line alignment, memory ordering validation)
- **Consulting:** Saruman (C++ epoch system reference), Frodo (behavioral spec)
- **Review:** Gandalf, Galadriel (unsafe + memory ordering audit)

**Files to Create/Modify:**
```
crates/faster-core/src/epoch/
├── mod.rs                          # pub use, module docs
├── light_epoch.rs                  # LightEpoch struct + core operations
├── epoch_entry.rs                  # EpochEntry (per-thread slot)
├── drain_list.rs                   # DrainList (deferred action queue)
└── guard.rs                        # EpochGuard (RAII protection)
```

**Detailed Specification (from Architecture §4.1):**

`LightEpoch`:
- `current_epoch: CachePadded<AtomicU64>` — global monotonic counter
- `safe_to_reclaim_epoch: CachePadded<AtomicU64>` — no thread references this or earlier
- `table: Box<[CachePadded<EpochEntry>; MAX_THREADS]>` — per-thread entries
- `drain_list: DrainList` — epoch-keyed deferred actions

`EpochEntry`:
- `local_current_epoch: AtomicU64` — 0 = inactive
- `reentrant: AtomicU32` — nested protect/unprotect count
- `phase_finished: [AtomicBool; Phase::COUNT]` — checkpoint phase markers (stub for now)
- `thread_id: AtomicU64` — occupant (0 = free)

Core operations:
- `protect(thread_index)` — enter epoch-protected region. First entry: store current epoch. Reentrant: increment count.
- `unprotect(thread_index)` — exit region. Last exit: clear slot + try_drain.
- `bump_current_epoch(callback)` — advance global epoch, queue callback for when safe.
- `try_drain()` — compute safe epoch, execute ready callbacks.
- `compute_safe_epoch()` — scan all slots, find minimum active epoch.
- `register_thread() -> usize` — claim a slot in the table.
- `deregister_thread(index)` — release slot.

`DrainList`:
- Lock-free queue of `(epoch: u64, action: Box<dyn FnOnce() + Send>)` entries
- `push(epoch, action)` — enqueue
- `drain_up_to(epoch)` — execute all actions with epoch ≤ target

`EpochGuard`:
- RAII guard: `protect` on creation, `unprotect` on drop
- Lifetime-bound to `LightEpoch`
- `!Send` (thread-affine, matching session model)

**Memory Ordering (from Architecture §4.1.3):**
- `protect` first entry: `local_current_epoch.store(epoch, Release)`
- `unprotect` last exit: `local_current_epoch.store(0, Release)`
- `bump_current_epoch`: `current_epoch.fetch_add(1, SeqCst)`
- `compute_safe_epoch`: global epoch `load(SeqCst)`, per-thread `load(Acquire)`

**Dependencies:** 1a (workspace), 1b (Epoch newtype, constants)

**Success Criteria:**
- [ ] Single-thread: protect/unprotect/bump_epoch 1M cycles, no leak
- [ ] Concurrent: 16 threads × protect/unprotect, safe_to_reclaim advances monotonically
- [ ] Drain callback: `bump_current_epoch(cb)` → callback fires after all threads pass that epoch
- [ ] Reentrance: nested protect/unprotect works correctly (2, 3 levels deep)
- [ ] EpochGuard: drop fires unprotect (verified by safe_epoch advancement)
- [ ] Thread registration: 256 threads register/deregister correctly
- [ ] Slot exhaustion: register 257th thread returns error (not panic)
- [ ] `cargo +nightly miri test` clean (checks atomics ordering under TSO relaxation)
- [ ] Property test: random interleaving of protect/unprotect/bump across N threads, invariants hold (10K iterations)

**Estimated Complexity:** L (Large)  
**PAW Candidate:** **YES — strongly recommended.** This is the most critical concurrency primitive. It needs:
- Careful spec (extract from architecture §4.1)
- Code research (C++ `LightEpoch.h` reference)
- Phased implementation (entry → drain list → light epoch → guard → tests)
- Formal review (Galadriel unsafe audit + Sam ordering review)

---

### 1h: Memory Allocator (MallocFixedPageSize)

**What:** Fixed-size block allocator for overflow hash buckets and internal structures. Page-based growth with free list. This is NOT the hybrid log allocator — it's the simpler arena-style allocator for fixed-size blocks.

**Who:**
- **Primary:** Sam (Systems Programming — allocator design)
- **Support:** Aragorn (Rust API design)
- **Consulting:** Saruman (C++ `MallocFixedPageSize` reference)
- **Review:** Gandalf, Galadriel (unsafe audit)

**Files to Create/Modify:**
```
crates/faster-core/src/alloc/
├── mod.rs                          # pub use
├── fixed_page_alloc.rs             # MallocFixedPageSize<T>
└── aligned.rs                      # AlignedBuffer, alignment utilities
```

**Detailed Specification (from Architecture §3.1.3, §12 Phase 1):**

`MallocFixedPageSize<T>`:
- Allocates blocks of `size_of::<T>()` from pre-allocated pages
- Page-based growth: allocates chunks of N blocks (e.g., 4096 per chunk)
- Lock-free free list (Treiber stack) for deallocation/reuse
- Bump allocator within each chunk for fresh allocations
- Alignment: blocks are aligned to `align_of::<T>()` (minimum 8 bytes, max 64 for cache-line types)

API:
```rust
impl<T> MallocFixedPageSize<T> {
    pub fn new() -> Self;
    pub fn allocate(&self) -> *mut T;           // Never returns null (panics on OOM)
    pub fn free(&self, ptr: *mut T);            // Returns to free list
    pub fn allocated_count(&self) -> usize;     // For leak checking
}
```

Implementation details:
- Free list: `AtomicPtr<FreeNode>` Treiber stack with `AcqRel` CAS
- Chunk list: `Vec<Box<[MaybeUninit<T>]>>` protected by `Mutex` (allocation of new chunks is rare)
- Bump pointer: `AtomicUsize` within current chunk

`AlignedBuffer`:
- Heap-allocated buffer with guaranteed alignment (512 bytes for O_DIRECT, 64 bytes for cache lines)
- `fn new(size: usize, alignment: usize) -> Self`
- Wraps `std::alloc::alloc` / `std::alloc::dealloc` with layout

**Dependencies:** 1a (workspace), 1b (for potential address integration)

**Success Criteria:**
- [ ] Allocate and free 1M blocks without leak (drop count matches alloc count)
- [ ] Concurrent: 8 threads × 100K alloc/free interleaved, no corruption
- [ ] Free list reuse: free 1000 blocks → allocate 1000 blocks → no new pages allocated
- [ ] Alignment: every returned pointer aligned to `align_of::<T>()`
- [ ] AlignedBuffer: 512-byte alignment verified for O_DIRECT use case
- [ ] `cargo +nightly miri test` clean
- [ ] No memory leaked on `Drop` of `MallocFixedPageSize`

**Estimated Complexity:** M (Medium)  
**PAW Candidate:** Yes — contains unsafe code (raw pointers, custom allocation), benefits from structured implementation + Galadriel safety review.

---

### 1i: Foundation Integration Tests and Benchmarks

**What:** Comprehensive test suite validating Phase 1 components work together. Property-based tests (proptest) for all types. Micro-benchmarks (criterion) for epoch acquire/release and allocator throughput. This is the quality gate before Phase 2.

**Who:**
- **Primary:** Boromir (QA — test design and infrastructure)
- **Support:** Legolas (benchmark design), Aragorn (test impl)
- **Consulting:** Frodo (behavioral reference from C++/C# tests)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/lib.rs       # Update: internal integration assertions
tests/
├── foundation_smoke.rs             # Workspace-level smoke tests
└── foundation_props.rs             # Property-based tests for all Phase 1 types

crates/faster-bench/
├── Cargo.toml                      # criterion dependency
└── benches/
    ├── epoch_bench.rs              # Epoch protect/unprotect throughput
    └── alloc_bench.rs              # MallocFixedPageSize throughput
```

**Test Categories:**

1. **Type round-trip property tests (proptest):**
   - LogicalAddress: `∀ page, offset: new(page, offset).page() == page ∧ .offset() == offset`
   - RecordInfo: `∀ fields: set_all(fields).extract_all() == fields`
   - HashTag: `∀ hash: from_hash(hash) < (1 << 14)`
   - Key/Value serialization: `∀ T: Copy: serialize(x) → deserialize → x`

2. **Epoch concurrent stress tests:**
   - 16 threads × 100K protect/unprotect cycles → safe_epoch monotonically increases
   - Drain callback ordering: callbacks fire in epoch order
   - Thread registration storm: 256 threads register/deregister rapidly

3. **Allocator stress tests:**
   - 8 threads × alternating alloc/free → no double-free, no use-after-free
   - OOM handling: allocate until chunk limit → graceful error

4. **Micro-benchmarks (criterion):**
   - Epoch protect+unprotect latency (target: < 5ns single-threaded)
   - Epoch bump + drain latency
   - MallocFixedPageSize allocate throughput (target: > 50M allocs/sec single-threaded)
   - MallocFixedPageSize allocate+free throughput

**Dependencies:** All Phase 1 items (1a–1h)

**Success Criteria:**
- [ ] All property tests pass 100K iterations with shrinking
- [ ] All concurrent stress tests pass under Miri (where feasible) or ThreadSanitizer
- [ ] Benchmark baselines established and documented
- [ ] Zero clippy warnings, zero test failures
- [ ] CI pipeline runs all tests in < 5 minutes

**Estimated Complexity:** M (Medium)  
**PAW Candidate:** No — test code is straightforward to write once components are stable. Direct implementation by Boromir with Legolas support.

---

### Phase 1 Execution Schedule

```
Week 1:
  [Boromir]     1a: Workspace Setup ──────────────────── (Day 1-2)
  [Boromir]     1e: CI Skeleton ──────────────────────── (Day 2-3, after 1a)
  [Aragorn]   1b: Core Address & Newtypes ──────────── (Day 2-5, after 1a)
  [Aragorn]   1c: Status & Error Types ─────────────── (Day 2-3, parallel with 1b)

Week 2:
  [Aragorn]   1d: Hash Utility Traits ──────────────── (Day 1-2, after 1b)
  [Aragorn]   1f: Record Format & Key/Value Traits ─── (Day 2-5, after 1b+1c) [PAW]
  [Sam] 1h: Memory Allocator ─────────────────── (Day 1-5, after 1a) [PAW]

Week 3:
  [Aragorn]   1g: Epoch System ─────────────────────── (Full week, after 1b) [PAW]
  [Sam] 1h: Memory Allocator (cont.) ─────────── (Wrap up + review)

Week 4:
  [Aragorn]   1g: Epoch System (cont.) ─────────────── (Testing + hardening)
  [Boromir]     1i: Foundation Tests & Benchmarks ─────── (Full week, after all)
  [Galadriel]    Unsafe audit of 1f, 1g, 1h ───────────── (Parallel with 1i)
  [Gandalf]  Architecture review of all components ─── (Continuous)
```

---

## Phase 2: Hash Index

**Architecture Reference:** Section 12, Phase 2 (lines 5128–5153), Section 3.1 (lines 261–440)  
**Duration:** 3–4 weeks  
**Dependencies:** Phase 1 items: 1b (address types), 1c (status/error), 1d (hash traits), 1g (epoch), 1h (allocator)  
**Goal:** A fully functional, latch-free, concurrent hash index that maps key hashes to logical addresses.

### Dependency Graph (Phase 2 Internal)

```
        Phase 1 Complete
               │
       ┌───────┴───────┐
       ▼               ▼
┌────────────┐  ┌────────────────┐
│ 2a: Bucket │  │ 2b: Overflow   │
│   Entry    │  │   Bucket Pool  │
│   Types    │  │   (from 1h)    │
└─────┬──────┘  └──────┬─────────┘
      │                │
      ▼                │
┌────────────┐         │
│ 2c: Bucket │◄────────┘
│  Structure │
│  (64-byte) │
└─────┬──────┘
      │
      ▼
┌────────────────────┐
│ 2d: Hash Table     │
│  (bucket array +   │
│   find/insert ops) │
└─────┬──────────────┘
      │
      ▼
┌────────────────────────┐
│ 2e: Concurrent Ops     │
│ (CAS insert, tentative │
│  bit protocol, delete) │
└─────┬──────────────────┘
      │
      ▼
┌────────────────────────────┐
│ 2f: Hash Index Tests       │
│ (concurrent correctness,   │
│  property-based, benches)  │
└────────────────────────────┘
```

**Parallelism:** 2a and 2b can run in parallel. 2c depends on both. 2d–2f are sequential.

---

### 2a: Hash Bucket Entry Types

**What:** Implement `HashBucketEntry` — the 8-byte packed entry storing a 48-bit address + 14-bit tag + tentative bit + reserved bits. This is the atomic unit of the hash index.

**Who:**
- **Primary:** Aragorn
- **Support:** Sam (bit manipulation validation)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/index/
├── mod.rs                          # pub mod
└── entry.rs                        # HashBucketEntry
```

**Detailed Specification (from Architecture §3.1.1):**
- `#[repr(transparent)]` over `u64`
- Bit layout: `[63:Reserved][62:Reserved][61:Tentative][60..48:Tag(14)][47..0:Address(48)]`
- Constants: `ADDRESS_MASK`, `TAG_MASK`, `TENTATIVE_BIT`, `INVALID = Self(0)`
- Methods: `new(address, tag)`, `address()`, `tag()`, `is_tentative()`, `with_tentative()`, `without_tentative()`, `is_empty()`
- Debug: display as `Entry(addr=0x{:x}, tag=0x{:x}, tent={})` format

**Dependencies:** 1b (LogicalAddress, HashTag)

**Success Criteria:**
- [ ] `size_of::<HashBucketEntry>() == 8`
- [ ] Bit field round-trip: all fields survive pack/unpack (property test)
- [ ] Tentative bit is independent of address and tag
- [ ] `INVALID.is_empty() == true`
- [ ] `new(LogicalAddress(1), HashTag(1)).is_empty() == false`

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — single file, pure bit manipulation.

---

### 2b: Overflow Bucket Pool

**What:** Specialize `MallocFixedPageSize` for 64-byte-aligned hash bucket overflow allocation. This bridges the Phase 1 allocator to the Phase 2 hash index.

**Who:**
- **Primary:** Sam
- **Support:** Aragorn
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/index/
└── overflow.rs                     # OverflowBucketAllocator (wraps MallocFixedPageSize)
```

**Specification:**
- Type alias or thin wrapper: `type OverflowBucketAllocator = MallocFixedPageSize<HashBucket>`
- Ensures 64-byte cache-line alignment for allocated buckets
- Provides `allocate_bucket() -> *mut HashBucket` and `free_bucket(*mut HashBucket)`
- Initializes buckets to zero (all entries invalid) on allocation

**Dependencies:** 1h (MallocFixedPageSize), 2a (HashBucketEntry, used in bucket type)

**Success Criteria:**
- [ ] Allocated buckets are 64-byte aligned
- [ ] Allocated buckets are zero-initialized
- [ ] Free + reallocate cycle: bucket is clean (zero-initialized again)

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — thin wrapper, straightforward.

---

### 2c: Hash Bucket Structure

**What:** Implement the 64-byte cache-line-aligned hash bucket with 7 data entries + 1 overflow pointer. This is the core building block of the hash table.

**Who:**
- **Primary:** Aragorn
- **Support:** Sam (alignment verification, atomic ordering)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/index/
└── bucket.rs                       # HashBucket, bucket operations
```

**Detailed Specification (from Architecture §3.1.2):**
```rust
const BUCKET_ENTRIES: usize = 7;
const OVERFLOW_ENTRY: usize = 7;

#[repr(C, align(64))]
pub(crate) struct HashBucket {
    entries: [AtomicU64; 8],  // 7 data + 1 overflow pointer
}
```

Methods:
- `find_tag(&self, tag: HashTag) -> Option<HashBucketEntry>` — scan 7 entries, return first match (non-tentative, valid)
- `find_empty_slot(&self) -> Option<usize>` — find first empty slot (for insertion)
- `get_entry(&self, index: usize) -> HashBucketEntry` — load entry at index
- `cas_entry(&self, index: usize, expected: HashBucketEntry, new: HashBucketEntry) -> Result<_, HashBucketEntry>` — CAS at index
- `overflow_ptr(&self) -> *const HashBucket` — load overflow pointer
- `set_overflow(&self, ptr: *const HashBucket)` — store overflow pointer
- `cas_overflow(&self, expected: *const HashBucket, new: *const HashBucket) -> Result<_, _>` — CAS overflow

**Dependencies:** 2a (HashBucketEntry), 1b (LogicalAddress)

**Success Criteria:**
- [ ] `size_of::<HashBucket>() == 64` (exactly one cache line)
- [ ] `align_of::<HashBucket>() == 64`
- [ ] `find_tag` correctly scans all 7 entries
- [ ] `find_tag` skips tentative entries
- [ ] Overflow pointer is independent of data entries
- [ ] CAS operations use correct memory ordering (AcqRel)

**Estimated Complexity:** S (Small)  
**PAW Candidate:** No — single file, well-specified.

---

### 2d: Hash Table Core

**What:** The hash table proper: a power-of-2 array of `HashBucket` plus the primary lookup and insertion algorithms. This includes overflow chain walking and the two-phase CAS insertion protocol with tentative bit.

**Who:**
- **Primary:** Aragorn (Rust Expert — lock-free algorithms)
- **Support:** Sam (memory ordering, cache behavior)
- **Consulting:** Saruman (C++ `InternalHashTable` reference), Frodo (behavioral spec)
- **Review:** Gandalf, Galadriel (unsafe + ordering audit)

**Files to Create/Modify:**
```
crates/faster-core/src/index/
├── hash_table.rs                   # HashTable struct, FindEntry, FindOrCreateEntry
└── mod.rs                          # Updated pub exports
```

**Detailed Specification (from Architecture §3.1.2, §3.1.4, §3.1.5):**

```rust
pub struct HashTable {
    buckets: Box<[HashBucket]>,      // Power-of-2 count
    size_mask: u64,                  // buckets.len() - 1
    overflow_allocator: OverflowBucketAllocator,
}
```

Core operations:

`find_entry(hash: u64) -> Option<(HashBucketEntry, &AtomicU64)>`:
1. Compute bucket index: `hash & size_mask`
2. Compute tag: bits 48..61
3. Scan bucket's 7 entries for matching tag (non-tentative, valid address)
4. If not found, follow overflow chain and repeat
5. Return entry + reference to the atomic slot (for updates)

`find_or_create_entry(hash: u64, address: LogicalAddress) -> &AtomicU64`:
1. Try `find_entry` first
2. If not found, find empty slot in bucket (or overflow chain)
3. If no empty slot, allocate overflow bucket
4. CAS new entry with tentative bit set
5. Return reference to the slot for caller to finalize

`delete_entry(hash: u64, tag: HashTag, address: LogicalAddress) -> bool`:
1. Find entry matching tag + address
2. CAS to INVALID (0)
3. Return whether entry was found

**Dependencies:** 2a (entry), 2b (overflow pool), 2c (bucket), 1d (hash functions), 1b (addresses)

**Success Criteria:**
- [ ] Single-threaded: insert 1M entries → find all 1M entries → 100% found
- [ ] Delete: insert → delete → find → not found
- [ ] Overflow: insert 8+ entries hashing to same bucket → all found via overflow chain
- [ ] Table sizing: power-of-2 enforced at construction (panic on non-power-of-2)
- [ ] Memory: no leaked overflow buckets after drop

**Estimated Complexity:** L (Large)  
**PAW Candidate:** **YES — strongly recommended.** This is the most performance-critical data structure. Needs:
- Behavioral spec from C++ reference (Frodo)
- Careful implementation of CAS protocol
- Memory ordering review (Galadriel)
- Multi-phase: types → single-threaded ops → concurrent ops → tests

---

### 2e: Concurrent Hash Index Operations

**What:** Add full concurrent correctness to the hash table: the tentative-bit two-phase insert protocol, concurrent find during insert, epoch-integrated GC hooks for invalidating entries when log regions are reclaimed.

**Who:**
- **Primary:** Aragorn
- **Support:** Sam (ordering), Galadriel (safety review)
- **Consulting:** Saruman (C++ concurrency protocol), Frodo (behavioral contracts)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/index/
├── hash_table.rs                   # Extended with concurrent protocols
└── gc.rs                           # GC integration: invalidate entries by address range
```

**Detailed Specification:**

Tentative-bit protocol (from Architecture §3.1.1):
1. Find empty slot via CAS with tentative bit SET
2. Caller writes record to log at the address
3. CAS entry WITHOUT tentative bit (commit)
4. If step 1 succeeds but step 3 fails (another thread stole the slot), retry

GC integration:
- `invalidate_entries_in_range(begin: LogicalAddress, end: LogicalAddress)` — scan all buckets, zero any entry whose address falls in [begin, end)
- Called by epoch drain callback when pages are evicted
- Must be safe to call concurrently with find/insert operations

Concurrent insert conflict resolution:
- Two threads inserting the same tag to the same bucket: both CAS tentative entries; first to commit wins; second detects via `find_entry` returning existing entry, abandons its tentative entry
- Tentative cleanup: periodic scan or on next access removes stale tentative entries

**Dependencies:** 2d (hash table core), 1g (epoch system for GC coordination)

**Success Criteria:**
- [ ] 8 threads × 10M inserts of unique keys → zero lost entries (all findable)
- [ ] 8 threads × mixed insert/find → no panics, no infinite loops
- [ ] Concurrent insert of duplicate hash → exactly one entry survives (last-writer-wins on address)
- [ ] GC invalidation: invalidate range → entries in range return NotFound, others unaffected
- [ ] No data races under ThreadSanitizer (`RUSTFLAGS="-Z sanitizer=thread"`)
- [ ] Liveness: no thread starves (bounded retry count)

**Estimated Complexity:** L (Large)  
**PAW Candidate:** **YES** — concurrent correctness is subtle, benefits enormously from structured implementation → review → iterate cycle.

---

### 2f: Hash Index Tests and Benchmarks

**What:** Comprehensive test suite for the hash index: single-threaded correctness, concurrent stress tests, property-based tests, and performance benchmarks establishing baseline throughput.

**Who:**
- **Primary:** Boromir (QA — test design)
- **Support:** Legolas (benchmark design), Éowyn (concurrent test patterns)
- **Consulting:** Frodo (behavioral reference: what does C++ test?)
- **Review:** Gandalf

**Files to Create/Modify:**
```
crates/faster-core/src/index/
└── tests.rs                        # Unit tests (in-module)

tests/
├── hash_index_correctness.rs       # Integration: single-threaded CRUD
├── hash_index_concurrent.rs        # Integration: multi-threaded stress
└── hash_index_props.rs             # Property-based tests

crates/faster-bench/benches/
└── hash_index_bench.rs             # criterion benchmarks
```

**Test Categories:**

1. **Single-threaded correctness:**
   - Insert N keys → find all N → all found
   - Insert → delete → find → not found
   - Overflow chains: force 20 entries into one bucket → all found
   - Empty table: find → None
   - Full table: high load factor → all entries still findable

2. **Property-based (proptest):**
   - `∀ keys: insert(k) → find(k) == Some(addr)` (10K iterations)
   - `∀ keys: insert(k) → delete(k) → find(k) == None` (10K iterations)
   - Model-based: maintain a HashMap<u64, LogicalAddress> shadow, verify hash table matches after random operations (1K iterations × 1K ops each)

3. **Concurrent stress:**
   - 8 threads × 10M inserts (unique key ranges) → verify all found
   - 8 threads × mixed insert/find/delete → no panics, no lost entries that shouldn't be deleted
   - Contention: all threads insert to deliberately colliding hashes → no corruption

4. **Benchmarks (criterion):**
   - Single-threaded insert throughput (target: > 50M ops/sec)
   - Single-threaded lookup throughput (target: > 100M ops/sec)
   - Multi-threaded insert scaling (1, 2, 4, 8, 16 threads)
   - Multi-threaded lookup scaling (1, 2, 4, 8, 16 threads)

**Dependencies:** All Phase 2 items (2a–2e)

**Success Criteria:**
- [ ] All correctness tests pass
- [ ] Property tests pass 10K+ iterations with shrinking
- [ ] Concurrent tests pass under ThreadSanitizer
- [ ] Single-threaded: 100M insert + lookup < 10 seconds (per architecture §12)
- [ ] Concurrent: 8 threads × 10M inserts, zero lost entries
- [ ] Benchmark baselines documented in repo

**Estimated Complexity:** M (Medium)  
**PAW Candidate:** No — test code, direct implementation by Boromir with support.

---

### Phase 2 Execution Schedule

```
Week 5 (overlaps Phase 1 Week 4):
  [Aragorn]   2a: Hash Bucket Entry Types ──────────── (Day 1-2, after 1b)
  [Sam] 2b: Overflow Bucket Pool ─────────────── (Day 1-2, after 1h)
  [Aragorn]   2c: Hash Bucket Structure ────────────── (Day 3-5, after 2a)

Week 6:
  [Aragorn]   2d: Hash Table Core ──────────────────── (Full week) [PAW]

Week 7:
  [Aragorn]   2e: Concurrent Operations ────────────── (Full week) [PAW]
  [Galadriel]    Unsafe audit of 2c, 2d ────────────────── (Parallel)

Week 8:
  [Boromir]     2f: Tests & Benchmarks ────────────────── (Full week)
  [Galadriel]    Unsafe audit of 2e ────────────────────── (Parallel)
  [Legolas]  Performance analysis + optimization ───── (After benchmarks)
  [Gandalf]  Architecture conformance review ────────── (End of week)
```

---

## PAW Workflow Candidates Summary

Items recommended for PAW (phased-agent-workflow) execution:

| Item | Reason | PAW Benefit |
|------|--------|------------|
| **1f: Record Format** | Unsafe code, multi-file, safety-critical | Structured review catches ptr arithmetic bugs |
| **1g: Epoch System** | Most critical concurrency primitive, complex ordering | Phased impl + formal ordering review |
| **1h: Memory Allocator** | Unsafe (raw allocation), lock-free free list | Safety review + concurrent correctness verification |
| **2d: Hash Table Core** | Performance-critical, CAS protocol | Behavioral spec → impl → review cycle |
| **2e: Concurrent Ops** | Subtle concurrency, tentative-bit protocol | Multi-model review catches race conditions |

Items for direct implementation (no PAW overhead needed):

| Item | Reason |
|------|--------|
| 1a: Workspace Setup | Configuration, no logic |
| 1b: Newtypes | Pure types, trivial tests |
| 1c: Status/Error | Simple enums |
| 1d: Hash Traits | Single file, simple logic |
| 1e: CI Skeleton | YAML config |
| 2a: Entry Types | Single file, bit manipulation |
| 2b: Overflow Pool | Thin wrapper |
| 2c: Bucket Structure | Well-specified, small |
| 1i: Foundation Tests | Test code |
| 2f: Hash Index Tests | Test code |

---

## Success Criteria for Iteration 1 Completion

The MVP Iteration 1 is **DONE** when:

1. ✅ `cargo build --workspace` succeeds with zero warnings
2. ✅ `cargo test --workspace` passes all unit, integration, and property tests
3. ✅ `cargo clippy --workspace -- -D warnings` clean
4. ✅ CI pipeline green on Linux + macOS
5. ✅ Epoch system passes 16-thread concurrent stress test
6. ✅ Hash index passes 8-thread × 10M insert correctness test
7. ✅ Single-threaded hash index: 100M insert+lookup < 10 seconds
8. ✅ All `unsafe` code has Galadriel-approved safety documentation
9. ✅ `cargo +nightly miri test` clean on non-concurrent tests
10. ✅ Every public item has `#[doc]` documentation

**What This Iteration Enables:** Phase 3 (Hybrid Log) can immediately begin building on the hash index and epoch system. The hash index can store entries, and the epoch system can coordinate concurrent access. The foundation types (addresses, records, errors) are the vocabulary for every subsequent phase.

---

## Risk Mitigation for Iteration 1

| Risk | Mitigation | Owner |
|------|-----------|-------|
| Epoch ordering bug | Miri testing + Galadriel formal review + ThreadSanitizer | Galadriel, Aragorn |
| Hash index CAS protocol subtle race | Model-based property test (shadow HashMap comparison) | Boromir, Aragorn |
| Allocator leak or corruption | Drop-counting test + Miri + leak sanitizer | Sam, Galadriel |
| Cache-line false sharing | Verify `CachePadded` with benchmark (compare padded vs unpadded) | Legolas |
| C++ behavioral divergence | Frodo extracts expected behavior from C++ tests for comparison | Frodo |

---

*This plan reflects the architecture in `.squad/agents/gandalf/rust-faster-architecture.md` and the directives in `.squad/decisions/inbox/`. All team members should read this document before beginning their assigned work items.*
