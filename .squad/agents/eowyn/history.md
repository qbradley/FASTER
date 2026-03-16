# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

### DST Framework Architecture

**Crates:**
- `faster-dst` — Deterministic simulation test framework (scenarios, runner, campaign engine)
- Locations: `rust/crates/faster-dst/src/{scenarios/, runner.rs, workload.rs, crash_point.rs, fault_config.rs}`

**Scenario Library:** 1003 parameterized templates across 5 families (checkpoint, compaction, recovery, cross-subsystem, fault injection). See `.squad/skills/dst-campaign-design/` for expansion pattern.

**18 Critical Crash Points:**
- Checkpoint: BeforeFlush, DuringFlush, AfterFlush, BeforeMetadata, DuringMetadata, AfterMetadata (6)
- Compaction: BeforeCompact, DuringCompact, AfterCompact, BeforeMerge, DuringMerge, AfterMerge (6)  
- Recovery: BeforeValidate, DuringValidate, AfterValidate, BeforeLoad, DuringLoad, AfterLoad (6)

**Fault Injection:**
- Write errors (limits: 100, 200, 500)
- Torn writes (rates: 5%, 10%, 25%)
- Graduated faults (write error + torn write)

**Test Tiers:**
- **Tier 2 (CI):** 133 core tests, ~19s, runs on every PR
- **Tier 3 (Deep):** 3009 test cases (1003 scenarios × 3 seeds), ~220s, pre-release

**Device Constraint:** `SyncFileDevice` prefix must be `"log."` for recovery compatibility (`LogRecoveryEngine::validate_log_file` hardcodes `log.{n}`).

### Loom Concurrency Testing

**Location:** `rust/crates/faster-core/tests/loom_tests.rs` (21 tests, 1800 LOC)

**Subsystems Covered:**
- Epoch framework (protect, drain)
- Hash buckets (two-phase insert, tentative entry visibility)
- RecordInfo (seal, revivify, concurrent seal+tombstone)
- EPVS state machine (transition exclusive winner, wait_non_intermediate)
- Log allocator (no overlap, page boundary, sequential integrity)

**Approach:** Re-implement algorithms using loom primitives, matching exact bit layouts and atomic orderings. See `.squad/skills/loom-concurrency-testing/` for pattern.

**Execution:** `RUSTFLAGS="--cfg loom" cargo test --features loom -p faster-core --test loom_tests` (~64s)

### Architecture Decisions

- **12 Binding Decisions (Gandalf 2026-03-05):** No async in core, custom epoch, inline VL records, lock-free hash, 32MB pages, completion-based Device, opaque FFI handles, own checkpoint format, Result<Status,Error>, thread-affine sessions (!Send), Key/Value traits, epoch-grow protocol. Full spec: `.squad/agents/gandalf/rust-faster-architecture.md`
- **Éowyn's async usage:** Only for DST/simulation orchestration, never in core FASTER code
- **Quality baselines:** Benchmark 59.77M ops/s (Rust), 302 unsafe sites (1 critical fixed, see `SECURITY-AUDIT.md`), io_uring stable (50 tests)
- **Fuzz infrastructure:** 5 targets in `rust/fuzz/`, `fuzz-ci --smoke` for PR (~60s), nightly `FUZZ_DURATION=300`

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

### 2026-03-09: DST Expanded to 108 Scenario Templates

**What:** Expanded DST scenario library from 5 to 108 parameterized templates covering all 18 crash points.

**Files:** 8 new modules in `rust/crates/faster-dst/src/scenarios/`, expansion engine, campaign tests.

**Key Design:** Fixed crash points (not seed-dependent) for systematic per-point coverage. All 137 DST tests pass. Campaign smoke: 324 test cases in ~30s.

**Skill:** See `.squad/skills/dst-campaign-design/`

---

### 2026-03-10: Loom Concurrency Test Expansion

**What:** Expanded `loom_tests.rs` from 7 to 21 tests (664 → 1800 LOC), covering all 5 critical FASTER concurrency subsystems.

**Approach:** Re-implement algorithms using loom primitives, matching exact bit layouts and atomic orderings (production uses `std::sync::atomic` directly).

**Verification:** All 21 tests pass. No bugs found (confirms production correctness). Orderings: SeqCst for epoch bumps, AcqRel/Acquire for CAS, Release for stores.

**Skill:** See `.squad/skills/loom-concurrency-testing/`

---

### 2026-03-10: Extended DST Campaign to 1003 Scenarios

**What:** Expanded DST from 108 to 1003 scenario templates (9.3× increase), organized into 44 categories across 5 families.

**Files:** 7 new scenario modules (sparse key, overwrite compaction, graduated fault, boundary record, triple crash, write error crash, overwrite torn write).

**Bug Found:** `overwrite_recovery::template()` missing `count` in name → name collisions. Fixed.

**Verification:** 3009 test cases (1003 × 3 seeds) in ~5min, all pass. Zero production bugs found (confirms robustness).

**Skill:** See `.squad/skills/dst-fault-injection/`, `.squad/skills/dst-recovery-verification/`, `.squad/skills/dst-crash-point-selection/`

---

### 2026-03-11: DST Smoke Tests Integrated into Release-Gate and CI

**What:** Integrated DST into release-gate and CI with two-tier strategy for fast feedback.

**Strategy:**
- **Tier 2 (Fast Correctness):** 133 core tests in ~19s, runs on every PR
- **Tier 3 (Deep Validation):** 3009 test cases in ~220s, pre-release only

**Files:** `rust/scripts/release-gate`, `.github/workflows/rust-ci.yml`

**Impact:** PR validation +19s, comprehensive coverage preserved in Tier 3. Zero coverage loss.

**Skill:** See `.squad/skills/dst-two-tier-testing/`

---
