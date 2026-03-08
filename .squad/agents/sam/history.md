# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-05T18:33: Gandalf Rust FASTER Architecture Finalized

**What:** Gandalf completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Gandalf-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md`

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
- **Elrond:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Aragorn:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Sam:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Éowyn:** Simulation framework must model callback completion ordering for testing.
- **Galadriel:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Frodo:** Phase roadmap and implementation sequence encoded in Part 3.
- **Saruman:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Éowyn lead), unsafe audit planning (Galadriel lead), Phase 1 sprint (Frodo lead), async adapter spike (Elrond lead).

---

## 2026-03-06: Task 1d — Hash Utility Traits Completed (Sam)

**What:** Implemented `KeyHash`, `Hashable` trait, and FASTER hash functions in `rust/crates/faster-core/src/hash.rs`. Added hash benchmarks in `benches/core_benchmarks.rs`.

**Key design decisions:**
- Ported the exact C++ `FasterHash` algorithm (magic constant 40343, rotr64) for behavioral compatibility. Two variants: `faster_hash_u64` for integer keys (rotr 43) and `faster_hash_bytes` for byte slices (rotr 6).
- 14-bit tag extracted from bits 48..61 of the hash, matching architecture spec §3.1.5 and C++ `HotLogIndexBucketEntryDef::kTagBits = 14`.
- Bucket index via `hash & (table_size - 1)`, power-of-two table size enforced by debug_assert.
- No unsafe code. No external hash dependencies (ahash deferred behind feature flag per plan).
- Blanket `Hashable` impls for: `u32`, `u64`, `i32`, `i64`, `&[u8]`, `[u8; N]`, `&str`, `String`.

**Quality:** 28 hash-specific unit tests + 6 property tests (proptest) + chi-squared distribution test (1M keys). All 64 crate tests pass. Clippy clean.

**Artifacts:**
- `rust/crates/faster-core/src/hash.rs` — full implementation
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 6 criterion benchmarks (u64 hash, byte hash 12B/128B, Hashable u64/str, tag extraction)

---

## 2026-03-06: Task 2a — Hash Bucket Entry Types Completed (Sam)

**What:** Implemented `HashBucketEntry` and `AtomicHashBucketEntry` in `rust/crates/faster-core/src/hash_bucket.rs`. These are the 64-bit packed entries stored in hash bucket slots — the lowest level of the hash index.

**Key design decisions:**
- Bit layout matches C++ `HotLogIndexBucketEntryDef` exactly: `[63:Tentative][62:Reserved][61..48:Tag(14)][47..0:Address(48)]`. Note: the architecture doc §3.1.1 placed tentative at bit 61 and reserved at bits 62-63, but the C++ source code puts tentative at bit 63 and reserved at bit 62. I followed the C++ source as the authoritative reference.
- `#[repr(transparent)]` over `u64` for both `HashBucketEntry` (value type) and `AtomicHashBucketEntry` (over `AtomicU64`), ensuring exact 8-byte size with no padding.
- All bit manipulation is `#[inline(always)]` and branchless — shift+mask operations compile to single instructions on x86-64 and AArch64.
- Memory ordering rationale documented on every `AtomicHashBucketEntry` method: `Acquire` for lookups, `AcqRel`/`Acquire` for CAS operations.
- `EMPTY = Self(0)` matches C++ `kInvalidEntry = 0`.
- Provided both `compare_exchange` (strong) and `compare_exchange_weak` for CAS retry loops on ARM.
- No unsafe code required.

**Quality:** 42 hash-bucket-specific tests (31 unit + 7 property tests via proptest + 4 doc-tests). All 146 crate tests pass. Clippy clean (no warnings in new code). 4 new criterion benchmarks added (entry creation, tag extraction, address extraction, compare_exchange throughput).

**Artifacts:**
- `rust/crates/faster-core/src/hash_bucket.rs` — full implementation
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 4 new hash_bucket benchmarks added
- `rust/crates/faster-core/src/lib.rs` — `pub mod hash_bucket;` added

---

## 2026-03-05T19:20: Wave 1 Sprint Complete — 1d Done

**Context:** All 4 Wave 1 tasks executed in parallel by full team.

**Your Deliverable (1d):**
- Task 1d: Hash traits + FasterHash algorithm + quality validation — 64 tests passing

**Decision Merged:**
- Decision #12: Hash Function and Tag Layout — C++ FasterHash algorithm, 14-bit tags (bits 48..61), chi-squared validated uniform distribution

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-sam.md`

**What's Next:**
- Task 2a (Hash Bucket Entries) — ready to start, depends on 1c/1d ✅
- 2a will directly use `KeyHash`, `tag_from_hash`, `bucket_index` from your hash module
- Your second-pass focus: hash bucket entry layout, tag collision handling, probe sequence

**Team Status:**
- Aragorn (1b/1c): Address + status/error complete, 127 tests passing
- Boromir (1e): CI skeleton complete, 5-job GitHub Actions pipeline validated locally
- All 127 tests passing in workspace. Zero regressions.

---

## 2026-03-06: Task 2c — Hash Bucket Structure Completed (Sam)

**What:** Implemented `HashBucket` — the 64-byte cache-line-aligned container holding 7 `AtomicHashBucketEntry` slots plus one `AtomicLogicalAddress` overflow pointer. Added to existing `hash_bucket.rs`.

**Key design decisions:**
- `#[repr(C, align(64))]` guarantees 64-byte size and cache-line alignment. Two compile-time `const _: () = assert!(...)` verify both invariants.
- 7 entries stored as `[AtomicHashBucketEntry; 7]` (not `[AtomicU64; 8]` as sketched in the architecture doc) — strongly typed, no casting needed. The 8th slot is `AtomicLogicalAddress` for the overflow chain.
- `find_entry(tag)` scans 7 entries with `Acquire` loads, skipping empty and tentative entries. Returns first match `(index, entry)`.
- `find_empty()` returns first empty slot index. `try_insert(tag, addr)` does find-empty + CAS in one operation.
- `try_insert` uses `AcqRel`/`Acquire` CAS ordering and retries on next slot if CAS fails (another thread beat us).
- No unsafe code. All concurrency via atomic operations.

**Quality:** 30 new bucket-specific tests (unit + property), plus 4 new criterion benchmarks (find_entry miss/hit-first/hit-last, try_insert). All 293 crate tests pass (247 lib + 46 doc). Clippy clean.

**Also fixed:** Pre-existing compile error in `record/mod.rs` — `record_info` submodule visibility was `mod` (private), but `layout.rs` tests referenced `crate::record::record_info::MAX_VERSION`. Changed to `pub(crate) mod`.

**Artifacts:**
- `rust/crates/faster-core/src/hash_bucket.rs` — HashBucket struct + methods + tests
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 4 new bucket scan benchmarks

---

## 2026-03-07: Task 1h — Memory Allocator (MallocFixedPageSize) Completed (Sam)

**What:** Implemented `MallocFixedPageSize<T>` — the fixed-size page allocator for overflow hash buckets and internal metadata structures. Full replacement of the `allocator.rs` stub with a production-grade, lock-free allocator modeled after C++ `malloc_fixed_page_size.h`.

**Key design decisions:**
- Two-level allocation: growable page directory (level 1) + heap-allocated pages of 2^20 = 1,048,576 items each (level 2). Matches C++ `FixedPageAddress::kOffsetBits = 20`.
- Lock-free bump allocation via `AtomicU64::fetch_add` on a monotonic flat counter. Page/offset derived from counter using bit shifts (power-of-2 items per page).
- Lock-free free list via Treiber stack with 16-bit ABA tags in the upper bits of the head pointer. Freed items store next-pointer in their first 8 bytes. Requires `size_of::<T>() >= 8`.
- Pages heap-allocated via `alloc::alloc_zeroed` with cache-line alignment (64 bytes). Pages are never moved or freed until allocator drop — pointer stability guaranteed.
- Page directory growth uses Mutex (extremely rare: only when crossing a page boundary for the first time beyond current capacity). Old directories retired to a Vec and freed on Drop.
- Address encoding uses `LogicalAddress` directly: `page()` = directory index, `offset()` = item index within page. Counter starts at flat index 2 so addresses 0 (ZERO) and 1 (INVALID) are never returned.
- `get(&self) -> &T` is safe (interior mutability via atomics is the expected pattern for HashBucket). `get_mut(&self) -> &mut T` is `unsafe` — caller must prove exclusive access.
- Every `unsafe` block has a `// SAFETY:` comment per project `#![forbid(clippy::undocumented_unsafe_blocks)]` policy.

**Quality:** 23 new allocator-specific tests (15 unit + 4 property tests via proptest + 3 concurrent stress tests + 1 atomic-items concurrent test). 2 new criterion benchmarks (bump allocation throughput, free-list reuse throughput). All 310 lib tests + 56 doc tests pass. Clippy clean (zero new warnings).

**Artifacts:**
- `rust/crates/faster-core/src/allocator.rs` — full implementation (~700 lines)
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 2 new allocator benchmarks added

---

## 2026-03-07: Task 2b — Overflow Bucket Pool Completed (Sam)

**What:** Implemented `OverflowBucketPool` and all overflow chain operations: allocation, resolution, deallocation, chain traversal, chain search, and chain insert. This bridges the allocator (1h) to the hash bucket (2c) to form a complete overflow chain mechanism.

**Key design decisions:**
- `OverflowBucketPool` is a thin wrapper around `MallocFixedPageSize<HashBucket>`. The pool's `allocate()` explicitly re-zeroes buckets after allocation to handle free-list reuse (stale entries). Fresh bump-allocated pages are already zeroed, but the redundant zeroing cost is negligible vs. the allocation itself.
- `get_or_create_overflow()` is the critical concurrent operation: allocate → CAS → loser frees and uses winner's bucket. Memory ordering: `AcqRel`/`Acquire` on the overflow pointer CAS. `Release` semantics ensure the zeroed bucket is visible before the pointer is published; `Acquire` ensures readers see the fully-initialized bucket.
- Chain traversal via `for_each_bucket()` uses `ControlFlow<T>` for early termination — zero-cost when the closure returns `Continue`. Walk is iterative (not recursive) to avoid stack growth on deep chains.
- `insert_in_chain()` tries primary bucket first, then walks existing overflow chain, then allocates a new overflow via `get_or_create_overflow`. The rare recursive tail-call handles the case where concurrent insertions fill a freshly-allocated overflow before we can insert.
- `find_entry_in_chain()` returns `(bucket_addr, slot_index, entry)` — `LogicalAddress::ZERO` for the primary bucket, otherwise the overflow bucket's pool address. This gives callers the information needed for future CAS updates on the entry.
- Overflow chain methods live on `HashBucket` (not a separate helper) because they're integral to bucket semantics and need private access to `overflow_address`. The pool itself lives in a new `overflow.rs` module to keep hash_bucket.rs focused.

**Quality:** 22 new overflow tests in hash_bucket.rs (15 unit + 1 concurrent race + 1 deep chain + 1 proptest), 7 new tests in overflow.rs (6 unit + 1 concurrent stress), 1 new proptest for chain insert of N>7 entries. 6 new criterion benchmarks (chain traversal at depth 0/1/2/5, find_entry_in_chain, insert_in_chain). All 332 lib tests + 58 doc tests pass. Clippy clean.

**Artifacts:**
- `rust/crates/faster-core/src/overflow.rs` — OverflowBucketPool implementation + tests
- `rust/crates/faster-core/src/hash_bucket.rs` — overflow chain methods on HashBucket + tests + proptests
- `rust/crates/faster-core/src/lib.rs` — `pub mod overflow;` added
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 6 new overflow chain benchmarks

---

## 2026-07-24: Phase 1B — AtomicPtr Encapsulation + NonNull Safe Construction (Sam)

**What:** Reduced unsafe surface area in `allocator.rs` by encapsulating repeated `AtomicPtr` dereference patterns into a safe `current_dir()` helper and replacing `NonNull::new_unchecked` with `NonNull::new().expect()`.

**Changes made:**
1. **`current_dir()` helper** — `#[inline(always)]` private method on `MallocFixedPageSize<T>` that centralizes `unsafe { &*self.dir.load(Ordering::Acquire) }`. Replaced 3 call sites in `resolve()`, `bump_allocate()`, and `expand_directory()`, consolidating 3 identical unsafe blocks into 1 documented one.
2. **`NonNull::new().expect()` replacement** — Changed `NonNull::new_unchecked(old_dir)` in `expand_directory()` to `NonNull::new(old_dir).expect("retired directory pointer must be non-null")`. Zero-cost in release (optimized away), adds debug assertion.
3. **`epoch/drain.rs` audit** — Examined all 7 unsafe blocks. No encapsulatable `AtomicPtr` load patterns found (uses raw `*mut DrainNode` linked-list traversal, not directory pointer pattern). All blocks already have `// SAFETY:` comments.
4. **`Drop` impl audit** — All `Box::from_raw` calls in the Drop impl already have correct `// SAFETY:` comments.

**Unsafe block count (allocator.rs, non-test):** 21 before → 18 after (net −3: 3 dir loads consolidated into 1, plus 1 NonNull eliminated).

**drain.rs:** 7 unsafe blocks, all necessary, all with SAFETY comments. No changes.

**Quality:** All 1161 tests pass, 3 skipped. Clippy clean (zero warnings). Zero performance regression — `current_dir()` is `#[inline(always)]` producing identical codegen to the inlined unsafe pattern.

---

## 2026-07-25: Phase 2B — DrainList Encapsulation, Allocator Cleanup, PageTable Refactor (Sam)

**What:** Deeper unsafe isolation across three core files: extracted private helpers in DrainList, eliminated unnecessary unsafe in allocator new()/Drop, added frame_ref() helper in PageTable, and documented sector-alignment constraints preventing safe allocation patterns.

**drain.rs changes:**
1. **`link_and_cas_push()` private unsafe fn** — encapsulates the CAS-loop raw pointer linkage for the Treiber stack push. Used by both `push()` and `drain_up_to()` re-push path.
2. **`claim_chain()` private unsafe fn** — atomically walks a claimed chain, reconstitutes each node as `Box<DrainNode>`, returns in FIFO order. Replaces manual walk + separate `nodes`/`unready` Vecs.
3. **`push()`** — now delegates to `link_and_cas_push()` with a single `unsafe` call.
4. **`drain_up_to()`** — simplified: one `claim_chain()` call replaces 4 inline unsafe blocks. Unready nodes re-pushed via `link_and_cas_push()`.
5. **`Drop`** — uses `claim_chain()` for consistent ownership reconstitution.

**allocator.rs changes:**
1. **`new()`** — call `get_or_add_page(0)` on the `Box<PageDir>` before `Box::into_raw`, eliminating 1 unsafe block.
2. **`Drop`** — consolidated `unsafe { &*dir }` + `unsafe { Box::from_raw(dir) }` into single `Box::from_raw(dir_raw)`, saving 1 unsafe block.
3. **`dir` field documentation** — added explanation of why `AtomicPtr<PageDir<T>>` is required instead of `Box<PageDir<T>>` (atomic swap needed for `expand_directory()`).

**page.rs changes:**
1. **`frame_ref()` private unsafe fn** — centralizes `*mut PageFrame` → `&PageFrame` conversion with debug_assert and documented safety contract. Used by `get_frame()`, `get_or_allocate_frame()`, and `try_evict_frame()`.
2. **`get_or_allocate_frame()` CAS error path** — consolidated 2 unsafe blocks into 1.
3. **`PageTable::Drop`** — switched from `slot.load(Ordering::Acquire)` to `slot.get_mut()` (safe: `&mut self` guarantees exclusive access).
4. **`PageFrame::new()` doc** — added "Why `alloc_zeroed` instead of `Vec`/`Box<[u8]>`" section explaining sector-alignment (512B) prevents standard allocator patterns.

**Unsafe block counts (non-test `unsafe { }` blocks):**
| File | Before | After | Delta |
|------|--------|-------|-------|
| drain.rs | 6 | 7 | +1* |
| allocator.rs | 18 | 16 | −2 |
| page.rs | 14 | 14 | 0** |

\* drain.rs count increased by 1 because `claim_chain()` uses 2 explicit inner blocks (required by `unsafe_op_in_unsafe_fn`), but the structural win is that all raw pointer manipulation is now in 2 private helpers, and public methods have only safe delegation calls.
\*\* page.rs consolidated 2 → 1 in CAS error path but added `frame_ref()` inner block, net neutral in count but better documented.

**Overall project unsafe { } blocks:** ~104 (down from ~105 at Phase 1B baseline).

**Quality:** All 1161 tests pass, 3 skipped. Clippy clean. Zero performance regression — lock-free operations and atomic orderings unchanged.

---

## 2026-07-25: Wave 3, F1 — FFI Opaque Handle System (Sam)

**What:** Built the foundation for the C FFI layer: a type-safe, thread-safe opaque handle table (`HandleTable`) and C-compatible status codes (`FasterStatus`). The `faster-ffi` crate stub already existed; added `handle.rs`, `error.rs`, and updated `lib.rs`.

**Key design decisions:**
- **Monotonic handles via `AtomicU64::fetch_add(1, Relaxed)`**: Handles are never reused, preventing ABA-style bugs from C callers that cache stale handles. Handle 0 is reserved as invalid sentinel. Relaxed ordering is sufficient because the RwLock write-lock provides the happens-before for the HashMap insertion.
- **`RwLock<HashMap>` over lock-free**: FFI calls are not on the hot path (they're session-setup/teardown level). Simplicity wins over lock-free complexity. Concurrent reads allowed; writes serialize.
- **Callback-based access (`with`/`with_mut`)**: The RwLock read-guard must live while the reference is used. A closure-based API scopes the guard lifetime without leaking lock internals. This is the standard pattern for RwLock-guarded data.
- **Type-safe downcasting via `Any`**: Type mismatches on `remove` preserve the entry (don't destroy it). The type-check + remove is atomic under the write lock (no TOCTOU).
- **Poisoned-lock recovery (`into_inner`)**: At the FFI boundary, panicking is UB. We recover poisoned locks instead of unwrapping.
- **`#[repr(C)]` FasterStatus**: Success codes 0-99, error codes 100+. `is_success()`/`is_error()` helpers. Size is exactly 4 bytes (C int). Display impl for debugging.

**Quality:** 27 tests (handle lifecycle, invalid handles, type safety, 8-thread concurrency, double-remove, with_mut, heterogeneous types, len/is_empty) + 6 error tests. All 27 pass under Miri (zero UB). All 1167 faster-core tests still pass. Clippy clean, fmt clean.

**Also fixed:** Pre-existing compile error in `store/kv.rs` (`crate::record::MutableRecordAccessor` → `crate::hybrid_log::MutableRecordAccessor`) and pre-existing formatting issues.

**Artifacts:**
- `rust/crates/faster-ffi/src/handle.rs` — HandleTable implementation + 21 tests
- `rust/crates/faster-ffi/src/error.rs` — FasterStatus enum + 6 tests
- `rust/crates/faster-ffi/src/lib.rs` — Module structure


---

### Cross-Agent Update (2026-03-07T0245)

**From Quality Fanout Session:**

- **Legolas (Performance):** Benchmark baseline established — 59.77M ops/sec at 16T. Allocator performance is locked in as part of the overall throughput. Any allocator changes must maintain this baseline.

- **Gimli (Storage):** io_uring async read semantics documented. Allocator integration with UringDevice validated through 10 new battle tests. Async read behavior is well-understood and accounted for in recovery path.

- **Galadriel (Security):** Allocator FFI layer (if exposed) must follow panic-safety rule. All unsafe code in allocator has been audited; reference SECURITY-AUDIT.md.

---

## 2026-03-07: W2-02 — Sealed Bit in RecordInfo (P-03)

**What:** Implemented the "sealed" bit in RecordInfo — a write-protection flag for RCU transitions. This is the prerequisite for P-04 (revivification). Narrowed the checkpoint version field from 13 to 12 bits (max 4095 instead of 8191) and repurposed bit 60 as the sealed flag.

**New bit layout:**
```
  63    62    61    60     59..48           47..0
┌─────┬─────┬─────┬──────┬──────────────┬───────────────────────────────┐
│ Fin │ Tom │ Inv │ Seal │ Version (12) │ Previous Address (48)         │
└─────┴─────┴─────┴──────┴──────────────┴───────────────────────────────┘
```

**Key design decisions:**
- **Bit 60 for sealed** (not bit 57 as in Tsavorite): Our layout differs from Tsavorite's — stealing the top version bit keeps all flag bits contiguous at bits 60-63.
- **Version narrowed 13→12 bits**: Max 4095 checkpoint versions, sufficient for epoch tracking. No existing usage requires more.
- **`new()` never sealed**: Records always start unsealed; sealed only via `with_sealed()` builder or atomic `seal()` method.
- **Memory ordering**: `seal()` uses Release (publishes prior record writes), `is_sealed()` uses Acquire (observes writes before seal), `try_seal()` uses AcqRel/Acquire CAS.
- **Delete not checked**: Tombstone placement is metadata-only, not a value mutation — sealed records can still be deleted.

**Write-path auditing:**
- `internal_upsert`: Added sealed check before in-place update path. Sealed records fall back to copy-to-tail (RCU).
- `internal_rmw`: Added sealed check before in-place update path. Reads current value first, then falls back to copy-to-tail.
- `internal_delete`: Intentionally unchanged — tombstone placement is permitted on sealed records.

**Testing:** 14 new unit tests + 1 new property test (`seal_preserves_other_fields`). All 1168 lib tests + integration tests pass. Clippy clean.

**Learnings:**
- Multiple concurrent copilot agents sharing the same working tree creates constant file-reverting conflicts. Staging changes immediately after applying them is essential.
- The `edit` tool and Python/bash operate on potentially different filesystem views. Always verify changes via bash after applying.

**Branch:** `sam/sealed-bit-p03`
**Commit:** `f2d516c1` — `feat(record): add sealed bit to RecordInfo (P-03)`

---

### Session: EPVS Implementation (Iteration 6 A1)

**Date:** $(date +%Y-%m-%d)
**Task:** Implement EPVS — pack (phase, version) into AtomicU64 CAS word

**What was done:**
- Created new `state` module (Phase, SystemState, AtomicSystemState) with 48 unit tests
- Rewrote `CheckpointStateMachine` to use `AtomicSystemState` internally (EPVS)
- Widened checkpoint metadata version from u32→u64 with format_version field for backward compat
- Increased epoch PHASE_COUNT from 8→16 for unified phase numbering
- Fixed all downstream compilation in checkpoint writers, recovery, and integration tests
- Fixed pre-existing clippy `clone_on_copy` in store/functions.rs

**Results:**
- 1215 lib tests passing (1185 pre-existing + 30 new state/state_machine tests)
- 42 integration tests passing (checkpoint_recovery_tests)
- Clippy clean (`-D warnings`)

**Lessons learned:**
- Sub-agent (general-purpose task) destructively reverted critical files — had to recreate entire state module from memory. Always verify sub-agent changes didn't revert unrelated files.
- Bash heredocs are reliable for recreating deleted files; the edit tool can fail silently.
- Two-phase intermediate CAS protocol prevents ABA — the intermediate bit blocks concurrent transitions.
- Keep version bumps strictly on Prepare→InProgress to match C# Tsavorite exactly.

**Branch:** `sam/epvs-implementation`
**Commit:** `88010752` — `feat(faster-core): implement EPVS (Epoch-Protected Version Scheme)`

---

## 2026-03-08: A5 — Revivification Implementation (P-04)

**What:** Implemented revivification for sealed records in the Rust FASTER hybrid log. When a sealed record in the mutable region is targeted by an Upsert or RMW, the system now attempts to atomically clear the sealed bit (CAS) and perform an in-place update, rather than unconditionally falling back to copy-to-tail.

**Key design decisions:**
- **CAS-based unsealing**: `AtomicRecordInfo::try_revivify(expected)` atomically clears the sealed bit if the header matches `expected`. Only one thread wins the race; losers fall back to copy-to-tail.
- **AcqRel/Acquire memory ordering**: Matches the `try_seal` pattern — AcqRel on success publishes the unseal to other threads, Acquire on failure observes the winner's writes.
- **New `Revivified` status**: Added `OperationStatus::Revivified` to distinguish revivification from normal in-place updates. This lets callers track revivification frequency for diagnostics.
- **`with_sealed_cleared()`**: Symmetric to `with_sealed()` — clears bit 60 while preserving all other fields (version, flags, previous address).
- **Conditions for revivification**: (1) Record is in the mutable region, (2) record is sealed, (3) CAS to unseal succeeds, (4) value fits in existing allocation. Version check deferred until EPVS is integrated into the operation paths.

**Files modified:**
- `rust/crates/faster-core/src/record/record_info.rs` — `with_sealed_cleared()` + `try_revivify()` + 8 unit tests + 1 property test
- `rust/crates/faster-core/src/status.rs` — `Revivified` variant + updated `is_success`/`is_modified`/`Display`
- `rust/crates/faster-core/src/store/operations.rs` — Revivification in Upsert + RMW sealed paths + 5 integration tests
- `rust/crates/faster-core/src/store/kv.rs` — Doc comments updated

**Testing:** 1228 tests passing (1215 baseline + 13 new). Clippy clean.

**Learnings:**
- Multiple concurrent agents sharing the same working tree create constant branch-switching conflicts. Using `git worktree` to create an isolated working directory was the solution — no more file reverts from other agents' checkouts.
- The `try_revivify` CAS pattern is symmetric to `try_seal` — seal/unseal form a natural pair of atomic operations on the same bit, with matching memory ordering.

**Branch:** `sam/revivification`
**Commit:** `2bb358a5`
