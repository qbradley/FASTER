# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **RecordInfo bit layout:** `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`. Version max 4095 (12 bits).
- **Sealing protocol:** `seal()` = Release, `is_sealed()` = Acquire, `try_seal()` = AcqRel/Acquire CAS. Write paths (upsert, rmw) fall back to copy-to-tail on sealed records. Delete permitted on sealed.
- **Revivification:** `try_revivify(expected)` atomically clears sealed bit 60 via CAS (AcqRel/Acquire). Conditions: mutable region + sealed + CAS wins + value fits. Status variant: `OperationStatus::Revivified`.
- **EPVS (Epoch Protection Version Scheme):** `state` module — Phase, SystemState, AtomicSystemState. CheckpointStateMachine uses AtomicSystemState. Two-phase intermediate CAS prevents ABA. Version bumps only on Prepare→InProgress.
- **Key files:** `record/record_info.rs` (RecordInfo, sealing, revivification), `status.rs` (OperationStatus variants), `store/operations.rs` (write paths, integration tests), `state/` module (EPVS).
- **Hash table:** `find_entry()` skips tentative entries — must call `update_entry()` with `entry.without_tentative()`. `entry_count()` returns 0 (intentional). `count()` reflects bump allocations only.
- **Testing config pattern:** `FasterKvConfig { hash_index_size_log2: 1..10, buffer_size_pages: 4, mutable_fraction: 0.5, max_in_memory_pages: 3, grow_config.enabled: false }`.
- **Multi-agent safety:** Stage+commit immediately. Use `git worktree` for isolated directory. Sub-agents can destructively revert files — always verify.

## Learnings
<!-- Append new learnings -->
- Multiple concurrent agents sharing working tree creates constant conflicts — stage immediately.
- Sub-agent destructively reverted critical files — always verify sub-agent changes.
- Bash heredocs are reliable for recreating deleted files.
- Two-phase intermediate CAS protocol prevents ABA in EPVS state transitions.
- Keep version bumps on Prepare→InProgress only.
- Use `git worktree` for isolated directory — prevents file reverts from other agents' checkouts.
- `find_entry()` skips tentative entries — must call `update_entry()` with `entry.without_tentative()`.
- All 25 memory pressure tests run in <3s with small config.

## Session Log

### W2-02 — Sealed Bit in RecordInfo (P-03) (2026-03-07)
**Bit layout:** `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`

Version narrowed 13→12 bits (max 4095). `seal()` uses Release, `is_sealed()` uses Acquire, `try_seal()` uses AcqRel/Acquire CAS. Write paths audited; sealed records fall back to copy-to-tail. Delete permitted on sealed.

**Branch:** `sam/sealed-bit-p03`, **Commit:** `f2d516c1`

---

### EPVS Implementation (Iteration 6 A1)
Created `state` module (Phase, SystemState, AtomicSystemState) with 48 unit tests. Rewrote CheckpointStateMachine to use AtomicSystemState. Widened checkpoint metadata version u32→u64 with format_version field. Increased epoch PHASE_COUNT 8→16.

**Results:** 1215 lib tests passing (30 new). 42 integration tests passing.
**Branch:** `sam/epvs-implementation`, **Commit:** `88010752`

---

### A5 — Revivification (P-04) (2026-03-08)
CAS-based unsealing: `try_revivify(expected)` atomically clears sealed bit. AcqRel/Acquire ordering matches `try_seal` pattern. New `OperationStatus::Revivified` variant. `with_sealed_cleared()` clears bit 60.

**Conditions:** (1) mutable region, (2) sealed, (3) CAS wins, (4) value fits.
**Files:** `record/record_info.rs` (+8 unit tests +1 proptest), `status.rs`, `store/operations.rs` (+5 integration tests).
**Results:** 1228 tests passing. Clippy clean.
**Branch:** `sam/revivification`, **Commit:** `2bb358a5`

---

### Memory Pressure and OOM Testing (2026-03-08)
25 tests in `tests/memory_pressure_tests.rs` (1184 LOC): allocator stress (4), buffer pool/hybrid log (5), hybrid log checkpoint (2), hash table saturation (6), combined pressure (2), property-based (3), device variants (2).

**Key pattern:** Small `FasterKvConfig` with `hash_index_size_log2: 1..10`, `buffer_size_pages: 4`, `mutable_fraction: 0.5`, `max_in_memory_pages: 3`, `grow_config.enabled: false`.
**Branch:** `sam/memory-pressure-tests`, **Commit:** `737ab04c`
