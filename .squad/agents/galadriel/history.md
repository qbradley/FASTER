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

## Core Context

- **Unsafe audit (2026-03-05):** 90 unsafe sites in faster-core. 3% eliminable, 24% abstractable, 72% necessary. 44 missing SAFETY comments (now closed — compiler-enforced via `#![forbid(clippy::undocumented_unsafe_blocks)]`). ABA risk in allocator's 16-bit Treiber tag (mitigated by epoch, not compile-time enforced). Critical: FFI callbacks assume context pointer validity.
- **Architecture (Gandalf 2026-03-05):** 12 binding decisions — no async in core, custom epoch, inline VL records, lock-free hash, 32MB pages, completion-based Device, opaque FFI handles, own checkpoint format, Result<Status,Error>, thread-affine sessions (!Send), Key/Value traits, epoch-grow protocol.
- **All SAFETY comments are present** — Phase 4 sweep (2026-03-05) confirmed 0 gaps remaining. `#![forbid(clippy::undocumented_unsafe_blocks)]` enforces this at compile time permanently.

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
