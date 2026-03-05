# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

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

## 2026-03-06: Task 1d — Hash Utility Traits Completed (Chirrut)

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

## 2026-03-06: Task 2a — Hash Bucket Entry Types Completed (Chirrut)

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

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-chirrut.md`

**What's Next:**
- Task 2a (Hash Bucket Entries) — ready to start, depends on 1c/1d ✅
- 2a will directly use `KeyHash`, `tag_from_hash`, `bucket_index` from your hash module
- Your second-pass focus: hash bucket entry layout, tag collision handling, probe sequence

**Team Status:**
- Mando (1b/1c): Address + status/error complete, 127 tests passing
- Rex (1e): CI skeleton complete, 5-job GitHub Actions pipeline validated locally
- All 127 tests passing in workspace. Zero regressions.

---

## 2026-03-06: Task 2c — Hash Bucket Structure Completed (Chirrut)

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

## 2026-03-07: Task 1h — Memory Allocator (MallocFixedPageSize) Completed (Chirrut)

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

