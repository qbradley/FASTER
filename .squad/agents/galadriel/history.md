# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-08: Miri Test Expansion — Full Unsafe Coverage

**What:** Expanded Miri test suite from 757 LOC (26 tests) to 1935 LOC (71 tests), covering all testable unsafe modules in faster-core.

**New Test Modules Added (14):**
- `miri_record_info` — RecordInfo bit-packing roundtrips, AtomicRecordInfo CAS
- `miri_system_state` — EPVS Phase+Version packing, AtomicSystemState CAS
- `miri_hash_table` — find_or_create_entry (get_unchecked paths), overflow chains, bucket_by_index
- `miri_record_accessor` — Raw pointer construction with aligned buffers, write/read/zero/value_mut_ptr
- `miri_log_record_ops` — LogRecordWriter/Reader full write-read cycle, raw allocation
- `miri_log_scan` — LogScanIterator with tombstone filtering, address ordering
- `miri_epoch_drain` — Deferred callbacks via EpochTable::defer/drain (indirect DrainList coverage)
- `miri_compaction_scanner` — Live/dead/tombstone record classification
- `miri_compaction_copier` — Record copy with old→new address mapping
- `miri_compaction_address_update` — CAS-based pointer swing in hash index
- `miri_store_operations` — Full CRUD via FasterKv+NullDevice (upsert/read/delete/overwrite)
- `miri_prefetch` — Prefetch safe wrapper (no-op under Miri)

**Key Technical Learnings:**
1. `MutableRecordAccessor::new()` requires 8-byte aligned pointers — use `Vec<u64>` not `Vec<u8>` as backing store
2. `KeyHash::new(0)` produces tag=0 which is treated as empty bucket entry — always use `Hashable::hash()` trait for proper hashes
3. `DrainList` is `pub(crate)` — tested indirectly through `EpochTable::defer()` + epoch advancement
4. Miri catches integer overflow in debug mode — use `wrapping_mul()` for hash mixing constants
5. `HashTable::update_entry()` returns `bool`, not `Result`
6. Compaction scanner classifies records based on hash index presence — unindexed records = dead

**Modules NOT Testable Under Miri (with reasons):**
- `recovery/index_recovery.rs` — Requires file I/O for checkpoint reading
- `hybrid_log/flush.rs` — Device async I/O callbacks, threading
- `store/pending_io.rs` — Async device callbacks
- `sync_file_device.rs` — Real file system operations
- `device.rs` (full path) — Already partially covered; full coverage needs file I/O

**What This Means:**
- **Aragorn:** All new unsafe code should have a corresponding Miri test before merge
- **Éowyn:** Miri + Loom together cover both memory safety and concurrency correctness
- **Frodo:** Pre-merge CI should run `cargo +nightly miri test -p faster-core --test miri_tests` (takes ~42s)

---

## 2026-03-05T19:15: Comprehensive Unsafe Audit Completed

**What:** Systematic security audit of all 90 unsafe sites in `rust/crates/faster-core/src/`. Categorized each into Eliminable (3%), Abstractable (24%), or Necessary (72%). Identified 44 missing SAFETY comments (49% gap).

**Artifact Location:** `.squad/decisions/inbox/galadriel-unsafe-audit.md` (49KB detailed report)

**Key Findings:**
- **Total surface:** 71 unsafe blocks, 10 unsafe fns, 9 unsafe impls
- **Highest-risk modules:** `allocator.rs` (28 sites, lock-free Treiber stack), `page.rs` (21 sites, raw frame manipulation), `record_ops.rs` (16 sites, pointer-based accessors)
- **Critical gaps:** 49% of unsafe code lacks SAFETY justifications (violation of SF-7 best practices)
- **ABA risk:** Allocator's 16-bit tag in Treiber stack could overflow under extreme churn; mitigated by epoch deferral but not runtime-enforced
- **FFI callbacks:** Device completion callbacks assume context pointer validity with no runtime validation

**Category Breakdown:**
1. **Category A (Eliminable, 3 sites):** Checkpoint writer casts `HashBucket` to byte array — replace with `bytemuck::bytes_of`; allocator init dereferences raw pointer — refactor to safe accessor
2. **Category B (Abstractable, 22 sites):** `PageFrame::as_mut_slice`/`::zero` could take `&mut self`; `RecordAccessor::new` could have safe factory; manual Send/Sync impls could be eliminated via Arc wrapper
3. **Category C (Necessary, 65 sites):** FFI boundaries, custom allocators, lock-free CAS loops, pointer arithmetic for records — intrinsically require unsafe

**Critical Recommendations (Phase 0 Blockers):**
1. Add SAFETY comments to all 44 missing sites (Aragorn responsibility)
2. Run Miri on allocator + epoch drain tests (Éowyn responsibility)
3. Fix 3 Category A eliminable sites (30 min effort)

**Non-Blocking Improvements (Phase 1+):**
- Refactor 22 Category B sites to reduce unsafe surface
- Add debug assertions to FFI callbacks for null context checks
- Document ABA tag overflow risk in allocator header
- Consider phantom lifetime for `RecordAccessor` to tie to epoch guard

**What This Means For You:**
- **Aragorn:** No new unsafe code without SAFETY comments. Fix 3 Category A sites and add 44 missing comments before Phase 1 feature work.
- **Éowyn:** Run Miri stress tests on allocator before Aragorn depends on it. Focus on concurrent alloc/free churn (Treiber stack ABA scenarios).
- **Frodo:** Phase 0 must include unsafe documentation sprint (1-2 days). Block Phase 1 until Miri passes and comments are complete.
- **Sam:** Add debug assertions to device callbacks (null context checks). Review FFI boundary safety contracts.
- **Elrond:** Consider lifetime-parameterized record accessors for Phase 2 API stabilization (optional for Phase 1).
- **Gandalf:** Audit validates your architecture's unsafe usage is appropriate but under-documented. 72% necessary aligns with FASTER's performance goals.

**Risk Profile:** Overall verdict: unsafe usage is **appropriate but under-documented**. No memory safety bugs detected. Lock-free algorithms follow standard patterns. Main risk is ABA tag overflow if epoch integration breaks (currently mitigated but not compile-time enforced).

**Next Steps:** Phase 0 cleanup sprint (SAFETY comments + Category A fixes + Miri validation) before any Phase 1 implementation work begins.

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

## 2026-03-05T20:XX: Phase 4 — SAFETY Documentation Sweep (No-Op)

**What:** Tasked with adding SAFETY comments to all unsafe blocks missing them (~44 sites from audit). Upon systematic review, found that **all SAFETY comments are already in place** across every file in scope. Additionally, `lib.rs` already enforces three critical lints:

- `#![deny(unsafe_op_in_unsafe_fn)]` — forces explicit `unsafe {}` blocks inside unsafe fn bodies
- `#![forbid(clippy::undocumented_unsafe_blocks)]` — compilation fails if any unsafe block lacks a `// SAFETY:` comment
- `#![warn(missing_docs)]` — warns on undocumented public items

**Files Audited (no changes needed):**
- `allocator.rs` — 18 unsafe blocks, 3 unsafe fns, 3 unsafe impls — all documented
- `buffer_pool.rs` — 4 unsafe blocks, 2 unsafe impls — all documented
- `checkpoint/index_writer.rs` — 2 unsafe blocks — all documented
- `epoch/drain.rs` — 8 unsafe blocks, 2 unsafe fns, 2 unsafe impls — all documented
- `hash/index.rs` — 1 unsafe block — documented
- `hash/table.rs` — 2 unsafe blocks — all documented
- `hybrid_log/log_allocator.rs` — 3 unsafe blocks — all documented
- `hybrid_log/page.rs` — 15 unsafe blocks, 2 unsafe fns, 4 unsafe impls — all documented
- `hybrid_log/record_ops.rs` — 10 unsafe blocks, 2 unsafe fns — all documented
- `hybrid_log/scan.rs` — 1 unsafe block — documented
- `recovery/index_recovery.rs` — 1 unsafe block — documented
- `store/functions.rs` — 6 unsafe blocks, 4 unsafe fns — all documented
- `store/operations.rs` — 6 unsafe blocks — all documented
- `store/kv.rs` — 2 unsafe impls — all documented

**Skipped (Aragorn's territory):** `device.rs`, `sync_file_device.rs`, `flush.rs`, `pending_io.rs`

**SAFETY comments added/fixed: 0** — prior work (likely Phase 2/3) already completed this.
**Tests:** 1161 passed, 3 skipped. **Clippy:** clean (0 warnings).

**Key Insight:** The `#![forbid(clippy::undocumented_unsafe_blocks)]` lint makes this task self-enforcing — the codebase cannot compile with undocumented unsafe blocks. This is the strongest possible guarantee. The 44-site gap identified in the original audit has been fully closed.

---

## 2026-03-06: Full Production Security Audit — All 5 Crates

**What:** Comprehensive security audit of the entire Rust FASTER codebase (302 unsafe sites across 55,500 LOC in 5 crates) for production readiness.

**Artifact:** `rust/SECURITY-AUDIT.md` (full report with findings table, threat model, fuzzing targets)

**Verdict: CONDITIONAL PASS**

**Key Findings:**
- **Critical (1, FIXED):** FFI panic safety — all 12 `extern "C"` functions lacked `catch_unwind`. A Rust panic would unwind across FFI boundary = instant UB. Fixed by wrapping all functions in `catch_unwind` with `AssertUnwindSafe`.
- **High (4, OPEN):** (1) Device callback context pointer lifetime unenforceable at compile time. (2) `PendingIoContext` holds raw pointers to session state — dangling if session disposed during pending I/O. (3) FFI `SessionCell` Send+Sync relies on C caller honoring thread-affinity contract. (4) Tokio device casts raw pointers to usize for spawn_blocking — temporal window for UAF.
- **Medium (6):** Allocator 16-bit ABA tag sufficient with epoch but fragile without. Buffer size u32 truncation in FFI read. io_uring buffer lifetime documentation-only. Flush callback page_table lifetime.
- **Low/Info (8):** Handle counter debug_assert. Test-only pending_count visibility. O_DIRECT alignment validation.

**Unsafe Inventory (302 total):**
- faster-core: 142 (101 blocks, 29 fns, 12 impls)
- faster-ffi: 80 (78 blocks, 0 fns, 2 impls)
- faster-uring: 62 (56 blocks, 3 fns, 3 impls)
- faster-tokio: 10 (7 blocks, 3 fns, 0 impls)
- faster-dst: 8 (6 blocks, 2 fns, 0 impls)
- **100% SAFETY comment coverage** — compiler-enforced via `forbid(clippy::undocumented_unsafe_blocks)`

**Atomic Orderings:** All verified correct. No SeqCst overuse, no Relaxed underuse.

**What This Means:**
- **Aragorn:** Ensure `dispose_session` drains pending I/O (CORE-04). Document ABA tag limitation in `free_immediate`.
- **Elrond:** Add optional thread-ID validation for FFI sessions (FFI-02). Fix u32 truncation in read path (FFI-03).
- **Sam:** Add debug-mode validation for device callback context pointers (CORE-03).
- **Éowyn:** Implement P0 fuzzing targets (FFI boundary, checkpoint recovery). Run Miri on allocator.
- **Frodo:** Track High findings for remediation before GA.

**Fix Applied:** `catch_unwind` added to all FFI `extern "C"` functions. 70 FFI tests pass. Committed via `security(audit): add catch_unwind panic protection to all FFI boundary functions`.
