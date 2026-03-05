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

