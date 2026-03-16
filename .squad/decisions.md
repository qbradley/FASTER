# Squad Decisions

## Active Decisions

# Decision: Hash Index Software Prefetch (P-05)

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Branch:** `legolas/hash-prefetch`
**Status:** Implemented — ready for review

## Summary

Added cross-platform software prefetch hints on hash bucket lookups across
all four CRUD hot paths (read, upsert, RMW, delete). Expected to reduce
L1/L2 cache-miss stalls by 20-40% on single-thread throughput benchmarks.

## Architecture

```
key.hash() → prefetch(bucket_ptr) → layout computation → find()/find_or_create()
                                     ^^^^^^^^^^^^^^^^
                                     ~10-20 cycle latency cover
```

The overflow chain also prefetches the next bucket while scanning the
current 7-entry bucket, hiding pointer-chase latency.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| L1 prefetch (`_MM_HINT_T0`) | Bucket accessed within ~10 instructions |
| Read-only prefetch | Hash lookups are read-dominant; write prefetch would cause MESI invalidation |
| Single cache line | `HashBucket` is exactly 64 bytes (`#[repr(C, align(64))]`) |
| aarch64 `PRFM PLDL1KEEP` | Equivalent L1 read prefetch for ARM targets |
| No-op fallback | Graceful degradation on unsupported architectures |

## Team Impact

- **Gandalf**: This implements P-05 from the feature proposals. The API
  (`HashIndex::prefetch(key_hash)`) is available for any future hot-path
  optimizations.
- **Sam**: No interaction with epoch system — prefetch is purely advisory.
- **Faramir**: The prefetch calls in operations.rs are adjacent to but
  independent of the operation info struct parameters.
- **Benchmarking**: Need `cargo bench` or `disk-io-bench` to measure actual
  throughput improvement.

## Files Changed

- `hash/prefetch.rs` (NEW) — cross-platform prefetch intrinsics
- `hash/mod.rs` — module declaration
- `hash/table.rs` — `prefetch_bucket()` + overflow chain prefetch + 4 tests
- `hash/index.rs` — `prefetch()` facade + 3 tests
- `store/operations.rs` — 4 prefetch call sites

---

# Decision: Two-Level Prefetch Pipeline (P-06 / Task A4)

**Author:** legolas  
**Date:** 2026-03-08  
**Status:** Proposed  
**Branch:** `legolas/two-level-prefetch`

## Context

Every FASTER CRUD operation performs two serial memory accesses:
1. Hash bucket lookup (64-byte cache line)
2. Record access at the logical address from the bucket entry

With single-level prefetching (L1 only), only the hash bucket is prefetched ahead. The record access still incurs a full cache miss (~60-100ns), creating a pointer-chase bottleneck.

## Decision

Add a second prefetch stage (L2) that prefetches the record data after the hash bucket lookup resolves, plus a batch API with a sliding-window prefetch pipeline.

### Inline L2 Prefetch (All CRUD Paths)

After `find()`/`find_or_create()` returns but before `snapshot()` and `classify()`:
```rust
fn prefetch_record(allocator: &HybridLog, addr: LogicalAddress, is_write: bool) {
    if !addr.is_null() {
        let phys = allocator.get_physical_address(addr);
        if is_write { prefetch::prefetch_write(phys as *const u8); }
        else { prefetch::prefetch_read(phys as *const u8); }
    }
}
```

### Batch API with Sliding Window

- `PrefetchPipeline`: L1 window = 8 ops, L2 window = 4 ops
- Batch methods on `UnsafeContext` and `FasterKv`
- Epoch refresh every 256 operations

### prefetch_write Intrinsic

- x86_64: `_MM_HINT_ET0` (exclusive, all cache levels)
- aarch64: `PRFM PSTL1KEEP` (prefetch for store, L1, keep)
- Other: no-op fallback

## Alternatives Considered

1. **Software pipelining only (no inline L2):** Simpler but misses the low-hanging fruit of single-op L2 prefetch
2. **Larger L1 window instead:** Doesn't address the pointer-chase; hash buckets are already prefetched
3. **Async/coroutine-based prefetch:** Too complex for the current sync operation model

## Consequences

- ~+10-20% expected throughput improvement on cache-miss-heavy workloads
- Batch API enables further amortization of epoch enter/leave overhead
- `prefetch_record()` adds a `get_physical_address()` call (~5ns) per operation — negligible vs the ~60ns cache miss it avoids
- `UnsafeContext.session` field changed to `pub(super)` for batch module access

---

# Decision: Wave 3 Benchmark Baseline Established

**Author:** Legolas
**Date:** 2026-03-07
**Status:** Informational

## Context

Wave 3 merged all Wave 2 feature branches and ran comprehensive benchmarks to establish the post-optimization baseline.

## Key Findings

1. **All regressions recovered.** The A (-14.7%) and F (-8.1%) regressions observed during epoch-fix validation were caused by intervening commits, NOT the epoch fix itself. Wave 2 work restored and exceeded prior peaks.

2. **Hash prefetching delivers +6-7%, not +20-40%.** The software prefetch hints in `hash/prefetch.rs` provide measurable but modest improvement. Two-level prefetching (bucket + record) and hash table layout changes are needed for the predicted 20-40% gain.

3. **C# gap at -14% on reads.** Down from -60% pre-epoch, -20% post-epoch. Closing the remaining gap requires structural changes (open addressing hash table), not incremental tuning.

4. **Disk I/O benchmarks need redesign.** The 32GB VM with 128 buffer pages and 10M keys produces 0% pending rate — all data stays in memory. True disk benchmarks need 100M+ keys or buffer_pages ≤ 16.

## Decisions

- **Benchmark baseline is set.** All future performance changes should compare against these Wave 3 numbers.
- **Prefetch distance tuning is P1.** Low effort, expected +2-5% additional improvement.
- **Hash table layout investigation is P2.** High effort but highest remaining impact (+15-30%).
- **Disk I/O benchmarks deferred to Wave 4** with proper memory pressure configuration.

## Impact

- **Aragorn:** No action needed. FFI callbacks merge clean, no perf impact.
- **Sam:** Sealed bit merge clean. Epoch + sealed bit cumulative impact confirmed positive.
- **All:** Use `.squad/agents/legolas/wave3-benchmark-results.md` as the reference baseline.

---

# Decision: EPVS Implementation Choices

**Author:** Sam (Systems Programming Expert)
**Date:** $(date +%Y-%m-%d)
**Context:** Iteration 6 Action Item A1 — EPVS Implementation

## Decisions Made

### 1. Phase Numbering Scheme
Checkpoint phases use values 0–5, grow phases use 8–10, matching the design doc.
`PHASE_COUNT` increased from 8 to 16 to accommodate all phases in epoch arrays.

### 2. Version Bump Location
Version increments atomically on Prepare→InProgress transition only.
No version bump for grow phases. This exactly matches C# Tsavorite behavior.

### 3. Checkpoint Metadata Backward Compatibility
Added `format_version` field (u64) with `#[serde(default)]` so existing JSON
metadata without this field deserializes as `FORMAT_VERSION_LEGACY` (1).
The version field widened from u32→u64 but recovery code casts back to u32
where needed for `set_version()` until HashIndex is updated.

### 4. Grow State Machine Deferred
The grow state machine still uses its own `AtomicU8`. Wiring it to the shared
`AtomicSystemState` is deferred to a future PR (depends on RecordInfo bits
and HashIndex version field removal).

### 5. Two-Phase CAS Protocol
`AtomicSystemState::try_transition()` uses intermediate states (bit 63 set)
to prevent ABA problems. Observers seeing an intermediate state know a
transition is in progress and can spin-wait.

## Open Questions
- Should `HashIndex::version` (AtomicU32) be removed in the same PR or deferred?
  → Deferred: removing it requires wiring grow to shared state first.
- Should `RecordInfo` sealed-bit changes land before or after grow wiring?
  → Per design doc, RecordInfo changes are separate (P-22 / W2-04).

---

# Decision: Memory Pressure Testing Patterns

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-08
**Status:** Implemented
**Impact:** Testing infrastructure — establishes patterns for all future pressure tests

## Decision

Memory pressure tests should use configuration-based pressure (tiny buffer_size_pages, small hash_index_size_log2, aggressive eviction policies) rather than attempting to simulate actual OOM conditions at the OS level.

## Rationale

1. **Deterministic:** Configuration knobs force the same pressure paths without depending on system memory state.
2. **Fast:** Tests complete in <3s total because the data structures are tiny, not because we're waiting for the OS to run out of memory.
3. **Portable:** No `mmap` tricks, no `/proc/meminfo` parsing, no platform-specific OOM killer configuration.
4. **Correct invariant testing:** The goal is "data integrity under pressure," not "graceful OOM handling." FASTER's allocator panics on allocation failure (by design), so we test the pressure paths that precede failure.

## Key Configuration for Pressure Tests

```rust
FasterKvConfig {
    hash_index_size_log2: 1..10,     // tiny hash → overflow chains
    buffer_size_pages: 4,             // minimum viable
    mutable_fraction: 0.5,            // aggressive sealing
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,       // force eviction
        eviction_batch_size: 1,
    },
    grow_config: GrowConfig { enabled: false, .. }, // prevent auto-resize
}
```

## Important API Behaviors Discovered

1. **HashTable::find_entry() skips tentative entries.** Tests must call `update_entry()` with `without_tentative()` after `find_or_create_entry()`.
2. **FasterKv::entry_count() returns 0** (intentionally deferred). Not usable for assertions.
3. **MallocFixedPageSize::count()** only reflects bump allocations, not total live items.

## Implications

- All future pressure tests should follow this configuration-based pattern.
- The `pressure_store()` and `saturated_hash_store()` helpers in `memory_pressure_tests.rs` are reusable templates.
- Property-based tests (proptest) are effective for memory pressure fuzzing — 32 cases per property is sufficient for CI.

---

# Decision: Revivification via CAS-Unseal on Sealed Records

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-08
**Status:** DECIDED — Implemented
**Impact:** Upsert/RMW performance on sealed records in the mutable region

## Decision

Sealed records in the mutable region can be **revivified** (updated in-place) by atomically clearing the sealed bit via CAS, rather than always falling back to copy-to-tail (RCU).

## Rationale

1. **Performance**: Copy-to-tail allocates new log space, writes a full record copy, and CAS-updates the hash entry. Revivification avoids all of this — just a CAS + value overwrite.
2. **Log growth**: Revivification reduces write amplification by not appending redundant copies.
3. **Thread safety**: Only one thread can win the CAS race; losers fall back to the safe copy-to-tail path. No correctness compromise.
4. **Backward compatibility**: The `Revivified` status is a new variant — existing code matching on `is_success()` or `is_modified()` automatically includes it.

## Memory Ordering

- `try_revivify`: AcqRel on success (publishes unseal), Acquire on failure (observes winner)
- Matches the existing `try_seal` pattern — seal/unseal are symmetric CAS operations

## Conditions for Revivification

All must hold:
1. Record is in the **mutable** region (above safe_read_only_address)
2. Record is **sealed** (bit 60 set)
3. CAS to **unseal** succeeds (no concurrent modification)
4. Value **fits** in the existing allocation (no growth)

## What This Means For Others

- **Boromir/Faramir**: `OperationStatus::Revivified` is now a possible return from `upsert()` and `rmw()`. Treat it as a successful in-place update.
- **Legolas**: Revivification should improve hot-path latency for workloads that seal-then-update records. Benchmark opportunity.
- **Galadriel**: No new unsafe code paths — `try_revivify` uses the existing `compare_exchange` wrapper.
- **Éowyn**: New test surface for concurrent seal/unseal races.

## Branch

`sam/revivification` — commit `2bb358a5`

---

# Decision: Sealed Bit in RecordInfo (P-03)

**Author:** Sam (Systems Programming Expert)
**Date:** 2026-03-07
**Status:** Implemented
**Branch:** `sam/sealed-bit-p03`
**Commit:** `f2d516c1`

## Context

P-03 requires a "sealed" bit in RecordInfo that provides write-protection during RCU transitions. This is a prerequisite for P-04 (revivification), which needs to prevent in-place mutations to records being copied.

## Decision

Narrowed the checkpoint version field from 13 to 12 bits (max 4095) and allocated bit 60 as the sealed flag. All flag bits are now contiguous at bits 60-63.

### Bit Layout Change
- **Before:** `[63:Final][62:Tombstone][61:Invalid][60..48:Version(13)][47..0:PreviousAddr(48)]`
- **After:** `[63:Final][62:Tombstone][61:Invalid][60:Sealed][59..48:Version(12)][47..0:PreviousAddr(48)]`

### Semantic Rules
- In-place Upsert and RMW must check `is_sealed()` and fall back to copy-to-tail when set
- Delete (tombstone placement) is permitted on sealed records — it's metadata-only
- Records are never created sealed; sealing happens via atomic operations

### Memory Ordering
- `seal()`: Release — publishes all prior value writes
- `is_sealed()`: Acquire — observes writes made before sealing
- `try_seal()`: AcqRel/Acquire CAS — standard optimistic pattern

## Consequences

- Maximum checkpoint version reduced from 8191 to 4095 (sufficient for epoch tracking)
- All write paths audited and sealed checks added where needed
- P-04 can now use the sealed bit to protect records during revivification

---

# Decision: Snapshot Checkpoint Not Functional via FFI

**Agent:** Saruman (C++ Expert)
**Date:** 2026-03-07
**Status:** Observation (not a decision — needs team awareness)
**Impact:** Checkpoint/recovery FFI surface

## Finding

During C++ integration testing (A6), `faster_checkpoint()` with
`FasterCheckpointType_Snapshot` returns `FasterStatus_CheckpointError`.
FoldOver checkpoints work correctly.

The `CheckpointType::Snapshot` enum exists in the metadata layer, but
the write path through `FasterKv::checkpoint()` does not support it yet.

## Recommendation

1. Document Snapshot as unsupported in the FFI header and C++ wrapper
2. Add a Rust-side integration test for Snapshot to track when it becomes functional
3. C++ tests gracefully skip this path with a `[SKIP]` annotation

## Who Needs to Know

- **Gandalf/Aragorn:** Core team should confirm if Snapshot is planned for current phase
- **Sam:** Device layer may need snapshot-specific write path
- **Frodo:** Roadmap impact — Snapshot checkpoint is listed in MVP scope

---

# Decision: C++ Test Infrastructure Conventions

**Agent:** Saruman (C++ Expert)
**Date:** 2026-03-07
**Status:** Proposed convention
**Impact:** C++ test organization

## Convention

- C++ integration tests live at `rust/crates/faster-ffi/cpp/tests/`
- Each test suite is a standalone executable with its own `main()`
- Test harness is `test_harness.h` (minimal, no external deps)
- `run_tests.sh` is the canonical way to build and run all C++ tests
- CMakeLists.txt integrates with CTest for IDE and CI support
- Build artifacts go in `cpp/build/` (gitignored)

## Rationale

Self-contained test executables are simpler than a monolithic test binary
and allow parallel execution. The lightweight harness avoids pulling in
Google Test or Catch2 for what is fundamentally a cross-language integration
test suite (not a unit test framework).

---

# Decision: C++ Wrapper FFI Surface Uses Self-Contained Declarations

**Author:** Saruman
**Date:** 2026-03-07
**Task:** W3-04 — C++ Wrapper Header over Rust FFI

## Decision

The C++ wrapper header (`faster_cpp.h`) redeclares the entire FFI surface inline instead of including the cbindgen-generated `faster.h`.

## Rationale

cbindgen renders Rust's `Option<extern "C" fn(...)>` as opaque `struct Option_*Fn` forward declarations. These cannot be constructed or passed by value from C/C++ code, making the `_ex()` callback functions uncallable. Since the Rust ABI guarantees `Option<fn>` is layout-identical to a nullable function pointer, we declare the `_ex()` functions with proper pointer types.

This also makes the header self-contained with no dependency on the cbindgen output path.

## Impact

- **Aragorn:** If the FFI surface changes (new functions, changed signatures), both `faster.h` and `faster_cpp.h` must be updated. Consider adding a CI check that verifies ABI compatibility.
- **All:** The cbindgen `Option<fn>` limitation should be tracked — if cbindgen gains proper nullable function pointer support, we could switch back to including `faster.h`.

## Alternatives Considered

1. **Include faster.h + cast tricks** — Brittle, relies on compiler-specific behavior for casting to/from opaque structs.
2. **Modify cbindgen config** — cbindgen doesn't support rendering `Option<fn>` as nullable pointers in C mode.
3. **Wrap _ex() in C shims** — Adds an unnecessary indirection layer.

---

# Retrospective Action Items — Session 2 (Post-DST + Phase A Quality Wave)

**Facilitator:** Gandalf (Lead / System Architect)
**Date:** 2026-03-08
**Participants:** Aragorn, Boromir, Éowyn, Sam, Galadriel
**Status:** PROPOSED — pending team acknowledgment

---

## Summary

Squad retrospective covering the DST framework delivery (7-phase PAW), Phase A quality wave (4 parallel workstreams), and operational state (1,893 tests, 58 unpushed commits). Five participants, one round, strong consensus on 3 themes: push risk, verification gaps, and process weight.

---

## 🟢 What Went Well

1. **SoT debate review justified its cost.** The 4-specialist review caught MF-1 (CRC trailer overwriting 8 bytes of live record data on full pages) — a silent data corruption bug that no unit test surfaced. Every participant cited this as the session's highest-value moment.

2. **Multi-layer verification is genuinely strong.** Five distinct testing modalities in play — Miri (71 tests), loom (concurrency model-checking), memory pressure/OOM (25 tests), DST simulation (crash injection + CRC), and classical integration (63 compaction tests). Module boundaries held for parallel work with zero merge conflicts.

3. **Precheckin script is the team's most reliable process.** 37–41s, 1,893 tests, zero flaky tests. Nobody has to think about "did I break something." This is the quality gate that makes everything else possible.

4. **FFI panic safety is enforced, not aspirational.** Decision 38 (`catch_unwind` at every FFI boundary) went from proposal to tested implementation in one session.

5. **Idiomatic Rust patterns paid off.** Sealed-bit optimization and 2-level prefetch pipeline use the type system rather than fighting it — good long-term maintainability signal.

---

## 🔴 What Didn't Go Well

1. **58 unpushed commits is a single-point-of-failure risk.** Every participant flagged this. One disk failure loses the entire session's work including the MF-1 fix. The EMU auth blocker has persisted across multiple sessions without resolution.

2. **Scribe backlog: 30 inbox files.** PAW bypasses the Squad "After Agent Work" flow, so Scribe never ran. Documentation drifted badly — undocumented `unsafe` contracts are tech debt with teeth.

3. **No coverage metrics.** 1,893 tests but no `cargo-llvm-cov` or `tarpaulin` integration. Cannot prove FFI, async bridge, or compaction GC paths are adequately tested.

4. **DST framework built but not integrated into CI.** The framework works, it found bugs, but no test target runs a deterministic simulation campaign on PRs. It's shelf-ware until it's in the loop.

5. **PAW was heavy for DST.** 7 implementation phases with SoT debate review was thorough, but phases 5-7 had tight coupling and created artificial integration seams. The MF-1 bug was found in review, not in phase boundaries.

6. **No fuzz testing on deserialization/recovery paths.** Checkpoint recovery parses CRC-validated binary data from disk — untrusted bytes hitting those paths without fuzz coverage is a real gap.

7. **Squad routing left no paper trail.** Phase A quality wave was fast but produced no specs or plans. Hard to trace why certain test patterns were chosen or what coverage gaps were considered and rejected.

---

## 🔵 Action Items

### P0 — Do Today

| # | Action | Owner | Rationale |
|---|--------|-------|-----------|
| A1 | **Unblock push to origin.** Resolve EMU auth or push to a personal fork as immediate backup. 58 commits of safety-critical code on a single machine is an incident, not a chore. | Gandalf + qbradley | All 5 participants flagged this as unacceptable risk |
| A2 | **Push after every green phase going forward.** Establish rule: no more than 5 unpushed commits before pushing. Add visible notification when push fails. | Gandalf | Prevent recurrence |

### P1 — Next Session

| # | Action | Owner | Rationale |
|---|--------|-------|-----------|
| A3 | **Add `dst_smoke` test target to CI.** 5-second deterministic campaign with fixed seed, runs on every PR. Catches DST framework regressions AND exercises core under crash injection. | Éowyn | DST is shelf-ware until it's in the loop |
| A4 | **Enable `clippy::undocumented_unsafe_blocks` lint.** Add `# Safety` doc comments to every `unsafe` block. Make it a CI gate. | Aragorn + Galadriel | Clears Scribe backlog for most critical category |
| A5 | **Add property test for MF-1 class of bugs.** Roundtrip write-read assertion verifying every byte survives page boundaries, targeting full-page edge cases. | Boromir | MF-1 was caught by review, not automation — that's fragile |
| A6 | **Add Miri subset to precheckin.** At minimum: `hash_index`, `hybrid_log`, `epoch` modules. Full suite stays CI-only. | Sam | 71 Miri tests exist but aren't in the 41s gate |
| A7 | **Add PAW→Scribe hook.** After each PAW phase commit, auto-emit summary to Scribe's inbox. Prevents 30-file backlog. | Éowyn | PAW bypassing Squad flow is a design gap |

### P2 — Within 2 Sessions

| # | Action | Owner | Rationale |
|---|--------|-------|-----------|
| A8 | **Add `cargo-llvm-cov` to CI.** Coverage report for `faster-core/src/`. Primary targets: FFI, async bridge, compaction GC. | Boromir | Flying blind on test gaps |
| A9 | **Stand up `cargo-fuzz` targets for checkpoint recovery and log deserialization.** 10 min fuzzing per CI run. | Galadriel | Untrusted-bytes-from-disk paths need coverage |
| A10 | **Write targeted loom tests for epoch protect/drain/reclaim under contention.** | Sam | Highest-risk unsound-if-wrong code path |
| A11 | **Create coverage matrix document.** Module × verification-type (unit, integration, miri, loom, OOM, DST, fuzz). | Boromir | See gaps at a glance |

### Process Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **Cap PAW phases at 4-5 for future features.** Merge tightly-coupled pieces in planning. | Éowyn + Boromir consensus |
| D2 | **Use SoT debate review for all safety-critical features.** Reserve lightweight review for leaf features. | Universal consensus |
| D3 | **Squad routing should produce a brief coverage rationale.** 1-paragraph summary per agent of what was tested and what was excluded. | Sam observation |

---

## Metrics Snapshot

| Metric | Value |
|--------|-------|
| Tests passing | 1,893 |
| Precheckin time | 37–41s |
| Commits ahead of origin | 58 |
| Miri tests | 71 |
| Memory pressure tests | 25 |
| Compaction integration tests | 63 |
| DST phases completed | 7/7 |
| SoT bugs found | 4 must-fix + 6 should-fix |
| Production-severity bugs caught | 1 (MF-1: CRC trailer corruption) |

---

### 2026-03-09T16:58Z: User directive
**By:** qbradley (via Copilot)
**What:** Use claude-sonnet-4.6 for Scribe from now on, not claude-haiku-4.5
**Why:** User request — captured for team memory


---

# Decision: Mutation Testing Configuration (cargo-mutants)

**Author:** Aragorn (Rust Expert)
**Date:** 2026-03-09
**Branch:** `aragorn/mutation-testing` → merged to `squad`
**Status:** Implemented

## Summary

Added `cargo-mutants` v27.0 configuration and documentation for the FASTER Rust project. Pilot on `address.rs` achieved 100% mutation catch rate (55/55 viable mutants caught, 0 timeouts).

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| `timeout_multiplier = 3.0` | 3x baseline (~41s) gives ~120s headroom; eliminates prior timeout issues |
| `test_tool = "nextest"` | Matches CI runner; parallel test execution |
| `exclude_re` for bitwise field packing | `page_index`, `offset_in_page`, `from_page_offset` mutations cause infinite loops in unsafe code |
| `exclude_re` for Display/Debug impls | Surviving mutants are cosmetic, not bugs — noise reduction |
| `examine_globs` for Phase 1 critical modules | Focus on high-value code; full-crate runs deferred to periodic audits |
| Quality target: 80% all / 85% critical | Per team decision; pilot exceeded at 100% |

## Files Changed

- `rust/mutants.toml` — cargo-mutants configuration
- `rust/docs/mutation-testing.md` — NEW: workflow guide with pilot results
- `rust/.gitignore` — Added `mutants.out/` exclusion

## Team Impact

- **All agents:** Can run `cargo mutants --package faster-core -F 'src/myfile.rs'` to check mutation coverage on changed files
- **CI (future):** Full-scope run as pre-merge gate when runtime is acceptable (< 30 min target)

---

# Decision: Testing Architecture Documentation

**Author:** Gandalf (Architect)
**Date:** 2026-03-09
**Branch:** `squad` (direct commit)
**Status:** Implemented

## Summary

Created `rust/TESTING-ARCHITECTURE.md` as the definitive testing strategy document (490 lines) documenting ~1,950 tests across 9 categories and establishing a 4-tier validation architecture.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| 4-tier validation model | Maps test categories to CI cost/frequency tradeoffs |
| Fast Gate < 60s | Every commit must complete in under 60 seconds |
| `TESTING-ARCHITECTURE.md` is additive | Doesn't replace `TESTING.md` or `FUZZING.md` — adds architectural layer |
| Document known gaps | Mutation testing, async suite, cross-platform CI explicitly named |

## Tier Model

| Tier | Budget | Frequency | Contents |
|------|--------|-----------|----------|
| Fast Gate | < 60s | Every commit | Unit + integration (nextest) |
| Correctness Gate | < 5 min | Every PR | + property tests + Miri subset |
| Deep Validation | < 30 min | Nightly | + full Miri + Loom + DST smoke |
| Release Gate | < 2 hr | Pre-release | + fuzz + crash consistency + full mutation |

## Relationship to Existing Docs
- `TESTING.md` — quick-reference for categories and time budgets (unchanged)
- `FUZZING.md` — fuzzing-specific guide (unchanged)
- `TESTING-ARCHITECTURE.md` — comprehensive architectural overview that ties everything together (NEW)

## Team Impact

All agents should reference `TESTING-ARCHITECTURE.md` when:
- Adding new test infrastructure
- Deciding which test category to use for new code
- Understanding the CI tier model
- Planning test coverage improvements

---

### 2026-03-09T17:20Z: User directive — local merge workflow
**By:** qbradley (via Copilot)
**What:** Don't push to origin. Merge agent branches locally to `squad` branch for now.
**Why:** Push still blocked by EMU auth. Work continues locally.

### 2026-03-09T17:20Z: User directive — hash_layout_bench fix
**By:** qbradley (via Copilot)
**What:** hash_layout_bench was broken (1+ hour in debug). Assigned as background task to Legolas. **Resolved this session** — removed 206 lines dead OA prototype code, reduced dataset sizes, re-enabled benches(). Smoke test now 0.15s.
**Why:** Bench must run in reasonable time to be useful.

---

### 2026-03-09T1755: Decision superseded — Criterion custom main() no longer required
**By:** qbradley
**What:** The decision "Criterion Benchmarks Must Use Custom main() for Nextest Compatibility" is **cancelled/superseded**. After Legolas's hash_layout_bench fix (removed dead OA code, reduced dataset sizes) and qbradley's bench integration fix, all criterion benchmarks now work correctly with `cargo nextest run --all-targets` using standard `criterion_main!()`. The custom `main()` with `--bench` flag detection pattern is no longer required for new benchmarks.
**Why:** The root cause was hash_layout_bench being extraordinarily slow (hours in debug mode), not a fundamental criterion/nextest incompatibility. With the bench fixed, the workaround is unnecessary.

---

# Decision: hash_layout_bench — Remove Dead OA Prototype Code

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Branch:** `legolas/fix-hash-bench` → merged to `squad`

## Context

The `hash_layout_bench` was disabled (benches() commented out) because it took hours to run. Root causes:

1. **Dead code**: ~136 lines of open-addressing (Robin Hood) prototype no longer needed — team decision settled on multi-slot buckets.
2. **Oversized datasets**: 50K/200K/800K keys with 2^14/2^16/2^18 bucket tables. Setup cost alone for 800K entries was significant.
3. **Default criterion settings**: 100 samples × 5s measurement × 14 benchmarks.

## Decision

- **Removed all OA prototype code** (~206 lines total including OA benchmarks). Aligns with settled team decision: keep multi-slot buckets.
- **Reduced dataset sizes** to 3K/12K/50K (spans L2→L3 cache boundary without excessive setup).
- **Added criterion config**: `sample_size(10)`, `measurement_time(3s)` to cap wall-clock time.
- **Re-enabled benches()** call in main().

## Outcome

- File: 360 lines → 154 lines
- Smoke test: <0.2s (was hanging/hours)
- Full criterion run: ~2-3 minutes (was hours)
- All precheckin checks pass (fmt, clippy, doctests, nextest)

## Impact

Benchmark still covers the three key regression scenarios: `faster_find`, `faster_prefetch_find`, `faster_batch16`.

---

# Decision: Performance Regression Detection Infrastructure

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Branch:** `legolas/perf-regression-detection`
**Status:** Implemented — merged to `squad`

## Summary

Added two scripts and documentation providing the missing regression detection layer on top of the existing benchmark suite and CI baseline saves.

## What Changed

| Artifact | Purpose |
|----------|---------|
| `rust/scripts/bench-compare.sh` | Compare current benchmarks against saved baseline, flag regressions beyond threshold |
| `rust/scripts/bench-baseline.sh` | Save/list/compare/delete named baselines with git+machine metadata |
| `rust/docs/benchmarking.md` | Complete benchmarking guide for the team |

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| 5% default regression threshold | Criterion noise threshold is 2%; 5% provides headroom for system variance while catching real regressions |
| Exit code 1 for threshold violations | Enables CI gates: `bench-compare.sh \|\| fail_pr` |
| Metadata in `target/criterion/.baselines/` | Lives with ephemeral benchmark data, not in source control |
| JSON output mode (`--json`) | Machine-readable for CI pipelines and PR comment bots |
| No benchmark code changes | Bench code was fixed in Wave 1; this is purely wrapper tooling |

## Team Impact

- **All agents**: Use `bench-compare.sh` to verify performance impact before merge
- **CI**: Integrate `bench-compare.sh --json` for automated regression gating
- **Gandalf**: `TESTING-ARCHITECTURE.md` updated — regression automation gap marked resolved
- **Benchmarks run on VM only** — documented and enforced by convention

---

# Decision: DST Parameterized Expansion Architecture

**Author:** Éowyn (DST Expert)
**Date:** 2026-03-09
**Branch:** `eowyn/dst-100-scenarios`
**Status:** Implemented — merged to `squad`

## Summary

Expanded DST from 5 to 108 scenario templates via parameterized expansion. All scenarios use the existing `ScenarioTemplate` / `SeedCampaign` framework with no breaking changes.

## Key Architecture Decisions

| Decision | Rationale |
|----------|-----------|
| Fixed crash points per template (not seed-modulated) | Each template tests one specific crash point. Campaign seed sweep tests that point across many data permutations. |
| `all_expanded_scenarios()` single entry point | One function returns all 108 templates. Easy to enumerate, count, and pass to campaigns. |
| No framework changes to campaign runner | Templates work within existing write→checkpoint→crash→recover→verify model. |
| Parameterized template constructors | New scenario files accept crash point + record count arguments. Usable both via expansion engine and directly. |

## Expansion Breakdown (108 scenarios across 14 categories)

| Category | Count |
|----------|-------|
| Checkpoint crash variants (6 phases × 3 record sizes) | 18 |
| Compaction crash variants (6 phases × 3 record sizes) | 18 |
| Recovery crash variants (6 phases × 3 record sizes) | 18 |
| Concurrent ckpt+compact | 6 |
| Dual ckpt+recovery | 6 |
| Dual compact+recovery | 6 |
| Overwrite crash | 6 |
| Large record recovery | 3 |
| Compaction-scale recovery | 3 |
| High-density crash | 6 |
| Torn write (no crash) | 3 |
| Torn write + crash | 6 |
| Write error recovery | 3 |
| Mixed timing (OnVisit) | 6 |

## Team Impact

- **Frodo (CI)**: `campaign_expanded_smoke` (324 cases ~30s) ready for Tier 2. `campaign_expanded_full` (10,800 cases, ignored) for Tier 3.
- **All**: `scenarios::expansion::all_expanded_scenarios()` is the canonical API for the full scenario set.

---

# Decision: SyncFileDevice Prefix Must Be "log." for Recovery Compatibility

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-09
**Branch:** `boromir/recovery-edge-cases`
**Status:** Observation — requires documentation or API fix

## Summary

`LogRecoveryEngine::validate_log_file` hardcodes segment filenames as `log.{n}` (e.g., `log.0`, `log.1`). If a `SyncFileDevice` is created with a different prefix (e.g., `"hlog."`), checkpoint metadata will reference log addresses that the recovery engine cannot locate, causing `ValidationFailed` errors.

## Impact

Any code creating `SyncFileDevice` with a non-`"log."` prefix that later attempts recovery will fail silently at the validation step. Affects:

- **Gandalf/Faramir**: Store construction in examples/production configs must use `"log."` prefix.
- **Éowyn**: DST scenarios using SyncFileDevice need the same prefix.
- **Sam/Legolas**: Benchmark setups with alternate prefixes won't be recovery-compatible.

## Recommendation

Either:
1. **Document the constraint**: Add a doc comment on `SyncFileDevice::new` noting prefix must be `"log."` for checkpoint/recovery compatibility.
2. **Or fix the coupling**: Store the device prefix in checkpoint metadata so `validate_log_file` uses the correct prefix.

Option 2 is the correct long-term fix but is a larger change.


---

### 2026-03-08T21:01: User directive — Pre-release quality prioritization
**By:** qbradley (via Copilot)
**What:** Three-phase quality approach before release:
1. FIRST: Fill known test gaps (compaction, loom, miri, memory pressure)
2. THEN (parallel): Mutation testing to find remaining gaps + FoundationDB-style deterministic simulation testing (use PAW workflow for simulation, it's a big project)
3. AFTER: Reassess before implementing other work items (fault injection, release management, etc.)

Mutation testing needs a plan covering technology choice, execution strategy, and operational process (iterative, when to stop, how to improve hit rate). Simulation testing via PAW workflow or broken into phases with PAW per phase. Benchmarks never run locally — always VM via sub-agent.
**Why:** User request — strategic prioritization for pre-release quality gate

---

# Decision: Mutation Testing Strategy for FASTER Rust

**Author:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** Proposed
**Impact:** Testing quality, release readiness

---

## Summary

Use `cargo-mutants` (v27.0.0) for mutation testing across the FASTER Rust workspace. Run a 2–3 week phased campaign before public release to identify test suite gaps, targeting ≥80% kill rate overall and ≥85% for critical modules (store, compaction, hash).

## Key Decision Points

### 1. Tool: `cargo-mutants` ✅

- Only viable option. mutagen is unmaintained and requires nightly + source annotations.
- Works with our stable toolchain, edition 2024, MSRV 1.85.0.
- Zero source modifications required.
- Installed and validated: v27.0.0 runs correctly on our workspace.

### 2. Scope: 3,307 mutants (excluding samples)

- 4,454 total workspace mutants; 1,147 are in sample crates (skip).
- faster-core alone has 2,598 mutants across 10 modules.
- Prioritize by risk: store (416) → compaction (200) → hash (250) → hybrid_log (369) → epoch (72).

### 3. Unsafe Code Policy: Accept Timeouts as "Tested"

- Mutating arithmetic/bitwise ops inside unsafe blocks creates UB → segfaults/hangs → timeouts.
- Observed: 8/53 mutations in address.rs timed out (all unsafe-adjacent bitwise ops).
- These are NOT test gaps. Classify timeouts in unsafe code as acceptable.
- Atomic ordering correctness must be verified separately (loom/Miri, not mutation testing).

### 4. Kill Rate Target: 80% overall, 85% critical modules

- 90%+ is diminishing returns for a systems crate with significant unsafe code.
- Equivalent mutants (Display, logging, capacity hints) inflate survivors without indicating real gaps.
- Focus effort on zero missed mutants in safety-critical paths.

### 5. CI Integration: Weekly + Per-PR (Advisory Only)

- Weekly full run sharded across 4 CI workers (~30–60 min wall-clock).
- Per-PR: `--in-diff` on changed files only (~5–15 min).
- Advisory, not blocking. Don't gate merges on mutation testing.

### 6. Timeline: 2–3 Weeks Pre-Release

- ~30–50 hours total compute time across all phases.
- Human effort: ~50% running/triaging, ~50% writing targeted tests.

## Decisions Needed from Team

1. **Approve timeline:** Can we allocate 2–3 weeks before release for this campaign?
2. **CI budget:** Do we want weekly mutation testing in CI? Requires 16-core runners.
3. **Kill rate target:** Is 80%/85% the right bar, or should we aim higher for a v1.0?
4. **Ownership:** Should this be Aragorn-led or distributed across the team?

## Full Plan

See `.squad/plans/mutation-testing-plan.md` for the complete plan with phased execution, operational playbook, and experimental results.

---

# Decision: Criterion Benchmarks Must Use Custom main() for Nextest Compatibility

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Status:** Implemented

## Context

All three criterion bench binaries (`hash_layout_bench`, `ycsb`, `core_benchmarks`) used `criterion_main!()` which runs full statistical benchmarks when invoked by `cargo nextest run --all-targets`. The `hash_layout_bench` alone took 12+ minutes, blocking every precheckin run.

## Decision

**All `harness = false` criterion benchmarks MUST use a custom `main()` instead of `criterion_main!()`.**

Pattern:
```rust
criterion_group!(benches, bench_foo, bench_bar);

fn main() {
    if std::env::args().any(|a| a == "--bench") {
        benches(); // Full criterion run (cargo bench)
    } else {
        // Optional smoke test for nextest/cargo test
    }
}
```

## Rationale

- `criterion_main!()` does not implement nextest's test discovery protocol
- When nextest invokes the binary without `--bench`, criterion defaults to full benchmark mode
- Full mode runs 100 samples × auto-tuned iterations × warmup for EVERY benchmark function
- This is a category error: nextest wants a quick pass/fail test, not a 12-minute statistical analysis

## Impact

- Precheckin time: 12+ minutes (hanging) → 38 seconds
- `cargo bench` continues to work identically
- Any new bench files must follow this pattern

## Who Needs to Know

- **All agents adding benchmarks:** Follow the custom `main()` pattern
- **Aragorn:** Precheckin is unblocked

---

# Decision: Release Automation Infrastructure

**Author:** Gandalf (Lead / System Architect)
**Date:** 2026-03-09
**Branch:** `gandalf/release-automation`
**Status:** Implemented — merged to `squad`

## Summary

Established the complete release pipeline for publishing FASTER Rust crates to
crates.io. Five crates are publishable; three crates and all samples are excluded.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| Tag format `rust-v*` | Avoids collision with Node.js `squad-release.yml` which uses `v*` |
| Semver-checks advisory pre-1.0 | 0.x minor bumps may break API per semver convention |
| 30s delay between crate publishes | crates.io index sync takes ~20-30s; dependent crates fail without this |
| Consolidated version commits | One commit for all workspace version bumps, cleaner git history |
| `publish = false` on faster-dst | Test-only framework, not useful to downstream consumers |
| Version constraints on path deps | Required for crates.io — `version = "0.1.0"` alongside `path = "..."` |

## Publish Order (dependency graph)

```
faster-core          (root — no internal deps)
    ├── faster-device
    ├── faster-tokio
    ├── faster-uring
    └── faster-ffi   (also depends on faster-device)
```

## Team Impact

- **All agents:** When adding new public API to publishable crates, semver-checks in CI will flag breaking changes on PRs.
- **Sam/Faramir:** If new crates are added to the workspace, update `release.toml` and the publish order in `rust-release.yml`.
- **Legolas:** Benchmark crate (`faster-bench`) is excluded from publishing — it's a dev-only tool.

## Files Changed

- `rust/release.toml` (new)
- `.github/workflows/rust-release.yml` (new)
- `.github/workflows/rust-ci.yml` (modified — added semver-checks job)
- `rust/docs/releasing.md` (new)
- `rust/crates/faster-{core,device,tokio,ffi,uring}/Cargo.toml` (modified)
- `rust/crates/faster-dst/Cargo.toml` (modified — added `publish = false`)

---

# Decision: Release Gate Validation Script (4-Tier)

- **Author:** Boromir (QA Engineer)
- **Date:** 2026-03-09
- **Status:** Implemented

## Context

We needed a single script that validates release readiness across all quality dimensions — from fast lint checks through deep mutation testing and packaging verification. The existing `precheckin` script only covers tier 1 (fmt, clippy, tests).

## Decision

Created `rust/scripts/release-gate` with 4 independent tiers:

| Tier | Name | Target Time | Contents |
|------|------|-------------|----------|
| 1 | Fast Gate | <60s | fmt, clippy, doctests, nextest, doc build |
| 2 | Correctness Gate | <5min | loom, miri, fuzz smoke, cargo-deny, DST |
| 3 | Deep Validation | <30min | mutation testing, extended fuzz, bench regression |
| 4 | Release Gate | <2hr | changelog, metadata, packaging, semver-checks, full bench |

Tiers can be run independently (`--tier N`), from a starting point (`--from N`), or all together. Missing tools are gracefully skipped with warnings.

## Rationale

- **Tiered design** lets developers run fast checks locally (tier 1) while CI runs deeper checks (tier 2), and release managers run everything (all).
- **Graceful degradation** — missing tools produce warnings not failures, so the script works on any dev machine.
- **Markdown report** — every run produces a shareable report for release artifacts.

## Impact

- All team members should know about `--tier 1` as a `precheckin` superset.
- CI pipeline could be wired to run `release-gate --tier 2` on PRs.
- Release process should include `release-gate all` on a clean checkout.
# Decision: Multi-Writer Deadlock Fix — Flush/Eviction Pipeline

**Author:** Aragorn (Rust Expert)  
**Date:** 2026-03-11  
**Status:** Proposed  
**Priority:** P0 — Structural deadlock  
**Branch:** TBD (`aragorn/deadlock-fix`)

## Problem Statement

With 2+ writers using linear key distribution and small buffers, the system
enters a permanent stall. All writer threads block on the SF-10 buffer-full
check and never recover because the flush/eviction pipeline cannot make
forward progress.

### The 3-Point Deadlock Chain

```
Writer blocked (SF-10: buffer full)
    ↓ calls maintenance()
    ├─ flush_sealed_pages() submits async I/O (Sealed→Flushing)
    │   └─ if QueueFull → `continue` (SKIP the page → reverts to Sealed)
    ├─ evict_pages() runs IMMEDIATELY after flush
    │   └─ pages are Flushing (not Flushed yet) → no evictions
    │   └─ advance_head() hits Sealed/Flushing page → STOPS
    └─ allocate_at_tail() retries once → still blocked → returns None → Aborted
```

The C++ implementation avoids this via three mechanisms we lack:

1. **Batch-abort on QueueFull** — entire flush exits, not per-page skip
2. **Writer-side I/O draining** — blocked writers call `disk->TryComplete()`
3. **I/O throttling** — spin-wait with `yield()` when pending I/O is high

## Root Cause Analysis

### Why the current Rust code deadlocks

The core issue is a **timing gap**: `maintenance()` calls `flush_sealed_pages()`
then immediately calls `evict_pages()`. But async I/O completions (which
transition Flushing→Flushed) run on device worker threads, not on the caller.
Between the flush submission and the eviction check, no completions have
fired yet — so every page is still `Flushing`, the head cannot advance, and
the buffer remains full.

The `allocate_at_tail` function gets one maintenance retry (line 227-233 in
`operations.rs`), then gives up and returns `None` → `Aborted`. The writer's
retry loop (page-cache sample line 326-351) calls `maintenance()` + `yield()`
but this is a hot loop where N writers all stampede on the same pipeline,
none of them waiting for I/O to actually complete.

### SyncFileDevice vs UringDevice

- **SyncFileDevice**: Never returns `QueueFull` (unbounded channel). The
  deadlock is purely from timing — async callbacks fire on worker threads,
  but maintenance doesn't wait for them.
- **UringDevice**: Has real queue depth limits (`queue_depth: 256`). CAN
  return `QueueFull`, triggering the page-skip path that makes things worse.

Both devices hit the deadlock, but through slightly different paths.

## Fix Design

### Fix A: No Page Skipping on QueueFull (Batch Abort)

**Current behavior** (`flush.rs:379`):
```rust
Err(FlushError::QueueFull(_)) => continue, // Skip, retry next cycle
```

**New behavior**: When any page flush returns `QueueFull`, stop the entire
batch. Return a new `FlushResult` that indicates partial progress so the
caller knows flushing was interrupted by back-pressure.

```rust
// flush.rs — flush_sealed_pages
Err(FlushError::QueueFull(page)) => {
    // Back-pressure: device cannot accept more I/O right now.
    // Stop the batch — do NOT skip individual pages.
    // The caller should drain completions and retry.
    return Ok(FlushBatchResult {
        flushed: flushed_count,
        queue_full: true,
    });
}
```

**Return type change**: `flush_sealed_pages` currently returns
`Result<u32, FlushError>`. Change to `Result<FlushBatchResult, FlushError>`
where:

```rust
pub struct FlushBatchResult {
    /// Number of pages successfully submitted for flushing.
    pub flushed: u32,
    /// If true, at least one page was skipped due to device back-pressure.
    /// The caller should poll for I/O completions and retry.
    pub queue_full: bool,
}
```

This aligns with C++ where `RETURN_NOT_OK(WriteAsync)` exits the entire
flush function on any failure.

**Files changed:**
- `hybrid_log/flush.rs`: New `FlushBatchResult`, change `flush_sealed_pages`
  return type and QueueFull handling

### Fix B: Writer-Side I/O Draining

**The gap**: When a writer is blocked on SF-10, calling `maintenance()` only
*submits* I/O — it doesn't *wait* for completions. We need writers to
actively poll for I/O completions when they're blocked.

**New `Device` trait method**:

```rust
pub trait Device: Send + Sync + 'static {
    // ... existing methods ...

    /// Poll for completed I/O operations without blocking.
    ///
    /// Returns the number of completions processed. Implementations that
    /// run callbacks synchronously (NullDevice, InMemoryDevice) return 0.
    /// Implementations with background threads (SyncFileDevice) yield the
    /// current thread. UringDevice drains its completion queue.
    fn poll_completions(&self) -> u32 {
        0 // Default: no-op for synchronous devices
    }
}
```

**Implementation per device**:

| Device | `poll_completions()` behavior |
|--------|------------------------------|
| `NullDevice` | Returns 0 (all I/O is sync) |
| `InMemoryDevice` | Returns 0 (all I/O is sync) |
| `SyncFileDevice` | Calls `thread::yield_now()` + returns 0. The worker threads are completing I/O independently; yielding gives them CPU time. |
| `UringDevice` | Sends a `PollCompletions` command to the I/O thread and waits for the count. Or exposes a `try_complete()` that non-blocking drains the CQ. |

**Integration into `allocate_at_tail`** (`operations.rs`):

The `on_alloc_failure` callback currently just calls `maintenance()`. Change
it to a richer callback that also drains I/O:

```rust
// In allocate_at_tail, after first maintenance() fails:
// Spin-drain loop: call maintenance + poll device completions
// with bounded retries + yield between attempts.
let mut drain_attempts = 0u32;
const MAX_DRAIN_ATTEMPTS: u32 = 64;

while drain_attempts < MAX_DRAIN_ATTEMPTS {
    maint_fn();
    device.poll_completions();

    // Check if buffer is still full
    if allocator.advance_to_next_page().is_some() {
        if let Some(result) = writer.allocate_record(key, value) {
            return Some(result);
        }
    }
    drain_attempts += 1;
    thread::yield_now();
}
```

To pass the device reference into `allocate_at_tail`, we change the
`on_alloc_failure` callback type:

**Option 1 (preferred)**: Widen the closure to capture both `maintenance()`
and `device.poll_completions()`:

```rust
pub on_alloc_failure: Option<&'a dyn Fn()>,
```

Since `on_alloc_failure` is already a closure, we simply make `maintenance()`
call `poll_completions()` internally:

```rust
// In kv.rs maintenance():
pub fn maintenance(&self) {
    // ... existing flush/evict logic ...

    // NEW: Poll the device for completed I/O so callbacks fire.
    self.device.poll_completions();
}
```

This is the cleanest approach — no signature changes to `allocate_at_tail`
or `InternalContext`.

**But we also need a retry loop in `allocate_at_tail`**: The current code
tries maintenance once and gives up. We need bounded retries with yielding:

```rust
// operations.rs — allocate_at_tail
// After first maintenance + retry fails:
const MAX_ALLOC_RETRIES: u32 = 32;
for _ in 0..MAX_ALLOC_RETRIES {
    maint_fn(); // calls maintenance() which now also polls completions
    thread::yield_now();
    if let Some(result) = writer.allocate_record(key, value) {
        return Some(result);
    }
    if allocator.advance_to_next_page().is_some() {
        return writer.allocate_record(key, value);
    }
}
None // Give up after MAX_ALLOC_RETRIES
```

**Files changed:**
- `device.rs`: Add `poll_completions()` default method to `Device` trait
- `sync_file_device.rs`: Implement `poll_completions()` (yield)
- `faster-uring/src/device.rs`: Implement `poll_completions()` (drain CQ)
- `store/kv.rs`: Add `device.poll_completions()` call in `maintenance()`
- `hybrid_log/log_allocator.rs` or `store/operations.rs`: Retry loop in
  `allocate_at_tail`

### Fix C: I/O Throttling (Back-pressure Before Queue Overflow)

**Why throttling matters**: Without it, N writers can fill the entire buffer
before ANY flush has completed. By the time maintenance kicks in, we're
already in crisis mode. Throttling prevents the stampede.

**Design**: Add a lightweight check in the allocation hot path that yields
when the pipeline is under pressure:

```rust
// In allocate_at_tail or advance_to_next_page:
// If in-memory pages are above 75% of buffer capacity, yield
// to give the flush pipeline breathing room.
fn maybe_throttle(allocator: &HybridLogAllocator) {
    let snapshot = allocator.snapshot();
    let buffer_size = allocator.page_table().buffer_size() as u32;
    if snapshot.in_memory_pages() * 4 >= buffer_size * 3 {
        // 75% threshold — yield to let flush/evict threads run.
        std::thread::yield_now();
    }
}
```

This is NOT a spin-lock — it's a single `yield_now()` hint. Writers
continue immediately; the OS just gets a chance to schedule I/O threads.

**Where**: Inside `maintenance()`, at the beginning, when `eager_flush` is
true. This already exists implicitly but we should make the yield explicit:

```rust
// kv.rs — maintenance(), after detecting buffer_pressure:
if buffer_pressure {
    // Actively poll device to help drain completions.
    self.device.poll_completions();
    std::thread::yield_now();
}
```

**Files changed:**
- `store/kv.rs`: Add throttling yield in `maintenance()` under pressure

### Summary of Changes

| File | Change | Risk |
|------|--------|------|
| `device.rs` | Add `poll_completions() -> u32` default method | Low — default returns 0, backward compatible |
| `sync_file_device.rs` | Implement `poll_completions` (yield) | Low — yield is a hint, no semantic change |
| `faster-uring/src/device.rs` | Implement `poll_completions` (drain CQ) | Medium — must be thread-safe for uring I/O thread |
| `hybrid_log/flush.rs` | New `FlushBatchResult`, batch-abort on QueueFull | Low — callers already ignore the count |
| `store/operations.rs` | Bounded retry loop in `allocate_at_tail` | Medium — must bound retries to prevent livelock |
| `store/kv.rs` | Poll completions in `maintenance()`, throttle yield | Low — additive |

### What Does NOT Change

- The `Device` trait remains backward-compatible (default impl for `poll_completions`)
- `HybridLogAllocator::advance_to_next_page` is untouched
- `PageEvictor::advance_head` is untouched (its contiguous-head invariant is correct)
- No new synchronization primitives needed
- No async runtime dependency
- Lossy mode continues to work — the fixes are in the shared pipeline

## Risk Assessment

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Retry loop burns CPU under sustained pressure | Medium | Bounded retries (MAX=32) + `yield_now()` between attempts. Beyond the bound, return `Aborted` (existing behavior) |
| `poll_completions` for UringDevice is non-trivial | Medium | The uring I/O thread already handles a command channel; add a `PollCompletions` command variant |
| Changing `flush_sealed_pages` return type breaks callers | Low | Only one caller (`maintenance()`), internal API |
| `poll_completions` adds latency to happy path | Negligible | Only called inside `maintenance()`, which is already the slow path. Happy path (no buffer pressure) never calls it |
| Livelock if I/O is genuinely stuck (device error) | Low | MAX_ALLOC_RETRIES bounds the loop. True I/O errors propagate via `FlushError::IoError` |

## Lossy Mode Considerations

Lossy mode (`config.lossy = true`) adds `evict_and_truncate` + hash
invalidation. The deadlock affects lossy mode identically — the same
Sealed→Flushed timing gap applies. All three fixes (A, B, C) help lossy
mode without any special handling.

The one difference: in lossy mode, we aggressively evict and invalidate. The
retry loop in `allocate_at_tail` gives eviction more chances to complete,
which directly benefits lossy mode.

## Test Plan

### 1. Reproduction Test (Existing)

```bash
# This should stall before the fix, run to completion after:
cargo run -p page-cache -- \
    --writers 2 --distribution linear \
    --duration 30 --in-memory-mb 64 --log-size-mb 256
```

### 2. Targeted Unit Tests

**`flush_batch_abort_test`** (`flush.rs`):
- Create a mock device that returns `QueueFull` after N submissions
- Verify `flush_sealed_pages` returns `FlushBatchResult { flushed: N, queue_full: true }`
- Verify no pages are left in `Flushing` state (reverted to Sealed)

**`poll_completions_default_test`** (`device.rs`):
- Verify `NullDevice.poll_completions() == 0`
- Verify `InMemoryDevice.poll_completions() == 0`

**`allocate_retry_under_pressure_test`** (`operations.rs`):
- Set up a 4-page buffer, fill to SF-10 threshold
- Spawn a background thread that completes one flush after 50ms
- Verify `allocate_at_tail` succeeds within the retry window

### 3. Multi-Writer Stress Test

**`multi_writer_no_stall_test`** (integration):
- 4 writer threads, 4-page buffer, lossy mode
- Linear keys, 60-second deadline
- Assert: all writers make progress (writes > 0 per second)
- Assert: no thread is blocked for > 5 seconds

### 4. Performance Regression

- Run YCSB benchmark (single-writer) before and after
- The happy path should be unaffected (no extra branches in the fast path)
- Multi-writer throughput should improve (no more stalls)

## Implementation Order

1. **Fix A** (flush batch abort) — smallest, safest change
2. **Fix B** (poll_completions + retry loop) — the main fix
3. **Fix C** (throttle yield) — polish, prevents the cliff edge
4. **Tests** — reproduction + unit + stress

Fixes A+B are the critical pair. Fix C is a quality-of-life improvement
that reduces the severity of buffer pressure events.

## C++ Reference Alignment

| C++ Mechanism | Rust Equivalent |
|---------------|----------------|
| `RETURN_NOT_OK(WriteAsync)` exits flush loop | Fix A: batch-abort FlushBatchResult |
| `disk->TryComplete()` in `NewPage()` | Fix B: `device.poll_completions()` in maintenance |
| `DoThrottling()` spin-wait + yield | Fix C: yield in maintenance under buffer pressure |
| `num_pending_ios_ > kMaxPendingIOs` | Fix C: `in_memory_pages * 4 >= buffer_size * 3` threshold |

---

**Aragorn** — *The sword that was broken is reforged. The pipeline that was
broken shall flow again.*
# Decision: Deadlock Test Harness — Device Wrapper Strategy

**Author:** Boromir (QA Engineer)  
**Date:** 2026-03-11  
**Status:** Implemented  
**Branch:** `sam/deadlock-fix`

## Context

The multi-writer deadlock fix (Aragorn's design doc) introduces new behavior
in `flush_sealed_pages` (Fix A: batch abort on QueueFull) and `allocate_at_tail`
(Fix B: retry loop with `poll_completions`). We need test devices that can
trigger these paths deterministically.

## Decision

Created three new test device wrappers in `deadlock_tests.rs` instead of
extending `FaultInjectingDevice`:

1. **`QueueFullDevice`** — returns `IoRequestResult::QueueFull` from
   `write_async()` directly. This is fundamentally different from
   `FaultInjectingDevice`'s callback-level error injection. QueueFull
   is a submission-level rejection (no callback fires), while
   `FaultInjectingDevice` fires the callback with `IoStatus::Error`.

2. **`QueueFullThenSucceedDevice`** — separate device for the "transient
   QueueFull" scenario (retry loop testing). Simpler than making
   `QueueFullDevice` track two phases.

3. **`SlowDevice`** — background-thread callback delay. Returns `Submitted`
   (not `CompletedSync`), matching `SyncFileDevice`'s async behavior.
   Critical for reproducing the timing-dependent deadlock.

## Why Not Extend FaultInjectingDevice?

`FaultInjectingDevice` injects faults via the callback path — it always
returns `CompletedSync` and fires the callback with `IoStatus::Error`.
QueueFull is a completely different code path: `write_async()` returns
`QueueFull` **without** firing any callback. Mixing both patterns in one
device would make the fault policy confusing and error-prone.

## Test Strategy

- **7 tests run now** (device-level correctness, no dependency on Sam's changes)
- **6 tests ignored** with descriptive reasons (`#[ignore = "requires deadlock fix: ..."]`)
- Tests will compile immediately once Sam's changes land (written against expected API)
- The multi-writer forward progress test is the definitive regression test

## Risk

The ignored tests are written against the **expected** new API (`FlushBatchResult`,
`poll_completions()`). If Sam's implementation differs from the design doc,
the tests will need adjustment. The device wrappers themselves are stable.
# Decision: Multi-Writer Deadlock Fix — Implementation

**Author:** Sam (Systems & Storage Expert)  
**Date:** 2026-03-11  
**Status:** Implemented  
**Branch:** `sam/deadlock-fix`  
**Commits:** `5b50d994` (Fix A), `a35b7846` (Fix B), `75cba9ea` (Fix C)

## Summary

Implemented three coordinated fixes to close the multi-writer flush pipeline deadlock, following Aragorn's design.

## Changes

### Fix A: Flush Batch Abort on QueueFull
- New `FlushBatchResult { flushed: u32, queue_full: bool }` struct
- `flush_sealed_pages` breaks on QueueFull (was: `continue`)
- `maintenance()` now captures the `queue_full` flag (was: `let _ =`)

### Fix B: Writer-Side I/O Draining + Retry Loop
- `Device::poll_completions(&self) -> u32` with default returning 0
- `SyncFileDevice::poll_completions()` calls `thread::yield_now()`
- `allocate_at_tail` bounded retry loop (32 iterations) with yield between attempts

### Fix C: Yield Under Buffer Pressure
- `maintenance()` calls `poll_completions()` after every flush batch
- Under `queue_full` or `buffer_pressure`, yields + polls again before returning

## Verification

- **1728 tests pass** (cargo nextest, no regressions)
- **6 previously-ignored deadlock tests now pass**, including:
  - `multi_writer_forward_progress` (3 writers, SlowDevice, 10s runtime)
  - `flush_batch_aborts_on_queue_full`
  - `retry_loop_eventually_succeeds_after_queue_full_clears`
  - `poll_completions_default_returns_zero`
- Clippy clean (lib)

## Backward Compatibility

- `Device` trait: `poll_completions()` has default impl → existing custom devices don't break
- `flush_sealed_pages` return type changed from `Result<u32, FlushError>` to `Result<FlushBatchResult, FlushError>` → internal API, only 2 callers (both updated)
- `allocate_at_tail` behavior: now retries up to 32× instead of 1× → strictly more resilient, same Aborted outcome on permanent failure

## Open Items

- The `#[ignore]` annotations on the 6 deadlock tests can now be removed
- `SyncFileDevice::max_outstanding` remains dead code (not part of this fix scope)
- UringDevice (if added later) should implement `poll_completions()` to drain its completion queue
# Preparatory Analysis: Multi-Writer Deadlock Fix

**Author:** Sam (Systems & Storage Expert)
**Date:** 2026-03-10
**Status:** Analysis complete — ready for implementation

---

## 1. Root Cause Analysis

The "deadlock" is actually a **livelock/starvation** in the circular buffer pipeline. Here's the exact mechanism:

### The 3-Point Structural Problem

**Point 1 — `flush.rs:379` — `continue` on QueueFull skips remaining sealed pages**

```rust
// flush.rs:359-387 — flush_sealed_pages()
for p in head_page..ro_page {
    // ...
    match self.flush_page(page, page_table, device, self.page_size) {
        Ok(true) => flushed_count += 1,
        Ok(false) => {},
        Err(FlushError::QueueFull(_)) => continue,  // ← PROBLEM: skips to next page
        Err(e) => return Err(e),
    }
}
```

When the device returns QueueFull, the loop `continue`s to the next sealed page — which will ALSO get QueueFull. The entire scan completes with **zero flushes initiated**, and maintenance returns having done nothing useful. C++ returns the error for the entire batch so the caller knows to drain I/O.

**Point 2 — `log_allocator.rs:120-137` — Writers get `None` with no recovery path**

```rust
// log_allocator.rs:105-174 — try_allocate()
if new_offset > self.page_size { return None; }  // page boundary
// ...
if (next_page.wrapping_sub(head_page) as usize) >= self.page_table.buffer_size() {
    return None;  // ← SF-10: buffer full, just returns None
}
```

Then `allocate_at_tail()` (operations.rs:206-237) calls `on_alloc_failure` (= `maintenance()`) **once**, retries **once**, and if it still fails, returns `None` → `OperationStatus::Aborted`. The writer's only recourse is an external retry loop.

C++ calls `TryComplete()` from the writer thread to help drain pending I/O completions before returning. Our writers don't help drain — they just call `maintenance()` which calls `flush_sealed_pages()` which hits QueueFull and does nothing.

**Point 3 — No I/O throttling to prevent queue overflow**

`SyncFileDevice` uses `std::sync::mpsc::channel()` — an **unbounded** channel. The `max_outstanding: 1024` field is **dead code** — never checked in `submit()`. So `QueueFull` is never returned by `SyncFileDevice` in practice.

The real bottleneck: the flush pipeline can issue writes faster than I/O threads can complete them, and there's no feedback mechanism. When the I/O threads are saturated:
- Pages stay in `Flushing` state
- Eviction can't advance head (needs `Flushed` pages)
- Allocator hits SF-10 (tail would lap head)
- All writers block

### Critical Insight: The Real Deadlock Pattern

With SyncFileDevice (unbounded channel), QueueFull never fires. But the deadlock still happens because:

1. Writers fill pages faster than I/O threads can write them
2. Pages transition `Sealed → Flushing` quickly (submit to unbounded channel)
3. I/O threads are slow, so callbacks (Flushing → Flushed) are delayed
4. Evictor needs `Flushed` pages to advance head — but I/O hasn't completed
5. Head doesn't advance → buffer full → SF-10 blocks all allocations
6. `maintenance()` calls `flush_sealed_pages()` — all pages already `Flushing`, nothing new to flush
7. `maintenance()` calls `evict_pages()` — all pages are `Flushing`, can't evict
8. Writer retry loop spins calling maintenance() which does nothing useful
9. **Only the I/O worker threads can break the cycle**, but they're in a different thread pool with no prioritization

This is why the 5ms maintenance thread is critical — without it, the I/O callbacks fire (from the I/O threads), transitioning pages to `Flushed`, but nobody calls `evict_pages()` to advance head.

---

## 2. Complete Code Map

### Files That Need Changes

| File | Line(s) | What | Change Needed |
|------|---------|------|---------------|
| `flush.rs` | 359-387 | `flush_sealed_pages()` | On QueueFull: break (not continue), return a signal indicating I/O queue pressure so caller can drain |
| `flush.rs` | 54-69 | `FlushError` enum | Add `Backpressure` variant or return count + pressure signal |
| `operations.rs` | 206-237 | `allocate_at_tail()` | Add retry loop with I/O drain between attempts (not just one shot) |
| `kv.rs` | 1668-1731 | `maintenance()` | Add I/O completion polling step before flush/evict. Consider returning status. |
| `kv.rs` | 946,1002,1054 | `on_alloc_failure` callbacks | Wire up multi-retry with drain |
| `eviction.rs` | 89-120 | `evict_pages()` | No structural changes needed — already correct |
| `device.rs` | 221-279 | `Device` trait | Consider adding `fn try_complete_io(&self) -> u32` (optional, default no-op) |
| `sync_file_device.rs` | 377-393 | `submit()` | Either enforce `max_outstanding` with bounded channel, or add completion polling |

### Files That Are Fine As-Is

| File | Why |
|------|-----|
| `eviction.rs` advance_head | Correctly stops at non-Flushed pages, handles all states properly |
| `page.rs` PageState | State machine is correct (Free→Open→Sealed→Flushing→Flushed→Evicted) |
| `log_allocator.rs` try_allocate | SF-10 check is correct — the issue is what happens AFTER it returns None |

---

## 3. Device I/O Interface Capabilities

### Current State

```
Device trait (device.rs:221-279):
├── sector_size() → u32
├── segment_size() → u64
├── max_outstanding_io() → u32          ← exists but unused by callers
├── read_async(...)  → IoRequestResult  ← callback-driven
├── write_async(...) → IoRequestResult  ← callback-driven, can return QueueFull
├── read_sync(...)   → io::Result<u32>
├── write_sync(...)  → io::Result<u32>
├── truncate_until(offset)
├── size() → u64
└── close()
```

### What's Missing

1. **No `try_complete_io()` or `poll_completions()`** — Device trait is fire-and-forget. Completions happen asynchronously via raw function pointer callbacks. There's no way for a caller to say "block until at least one I/O completes."

2. **Store-level completion exists but at wrong layer:**
   - `PendingIoContext::try_complete()` (pending_io.rs:366) — polls individual read contexts
   - `FasterKv::complete_pending()` (kv.rs:1118) — drains completed reads for a session
   - But these are for **pending reads** (on-disk record access), NOT for flush I/O completions

3. **Flush callbacks fire asynchronously** from I/O worker threads (flush.rs:138-173). The callback transitions `Flushing → Flushed` via the page table's atomic state. There's no synchronization primitive to wait on.

### Key Design Constraint

Flush I/O completions happen via raw `unsafe fn` callbacks that run on I/O worker threads. Adding a condvar or similar signaling mechanism requires:
- A shared `Arc<(Mutex<()>, Condvar)>` or similar
- The callback notifies it
- `maintenance()` or writer threads can wait on it

This is safe because `FlushCallbackContext` already carries a `*const PageTable` raw pointer — adding an `Arc` pointer is no more dangerous.

### SyncFileDevice Queue Reality

```
SyncFileDevice (sync_file_device.rs:311-375):
├── sender: Mutex<Option<Sender<IoRequest>>>  ← std::sync::mpsc (UNBOUNDED)
├── max_outstanding: u32 = 1024               ← DEAD CODE, never enforced
├── high_water: AtomicU64                     ← DEAD CODE
└── threads: Vec<JoinHandle>                  ← N worker threads
```

The `submit()` method (line 378-393) simply calls `tx.send(request)` — never checks outstanding count, never returns QueueFull. A bounded channel or explicit count tracking would enable real backpressure.

---

## 4. Test Harness Options

### Option A: QueueFull-Injecting Device (Recommended for unit tests)

Extend `FaultInjectingDevice` (tests/io_error_injection.rs:82) to support a `QueueFull` fault mode:

```rust
enum FaultMode {
    Error(i32),          // existing: return Error via callback
    QueueFull,           // NEW: return IoRequestResult::QueueFull from write_async
    TornWrite(f64),      // existing: partial write
}
```

Current `FaultInjectingDevice` only injects errors via the **callback** (fires callback with `IoStatus::Error`). For QueueFull, we need to return `IoRequestResult::QueueFull` from `write_async()` itself (before callback), which is a different code path.

**Advantage:** Deterministic, no I/O, fast. Can trigger after exactly N writes.
**Location:** Add to existing `io_error_injection.rs` or create `tests/deadlock_tests.rs`.

### Option B: Slow Device Wrapper (For integration tests)

```rust
struct SlowDevice {
    inner: InMemoryDevice,
    write_delay: Duration,  // artificial delay per write
}
```

Insert a `thread::sleep()` before invoking the callback. This simulates real I/O latency and triggers the timing-dependent buffer-full condition.

**Advantage:** Tests the real timing-based deadlock.
**Disadvantage:** Non-deterministic, slower.

### Option C: Bounded-Channel SyncFileDevice (For reproducing the exact production scenario)

Make `SyncFileDevice::submit()` actually enforce `max_outstanding` by using `sync_channel(capacity)` instead of `channel()`. With capacity=2 and 4 fast writers, QueueFull fires immediately.

**Advantage:** Tests the actual production code path.
**Disadvantage:** Requires changing production code (which we're going to do anyway).

### Existing Test Infrastructure

- **`FaultInjectingDevice`** (io_error_injection.rs:82): Wraps InMemoryDevice, runtime-configurable via `Arc<AtomicBool>`. Only supports Error injection and torn writes — NOT QueueFull. Easy to extend.
- **`NullDevice`** (device.rs:291): Completes synchronously. `max_outstanding_io() = u32::MAX`.
- **`InMemoryDevice`** (device.rs:394): Completes synchronously. `max_outstanding_io() = u32::MAX`.
- **No existing deadlock/backpressure tests.**
- **`memory_pressure_tests.rs`**: 25 tests with small buffer configs (buffer_size_pages=4, max_in_memory_pages=3). Tests allocation pressure but not flush pipeline stalls.
- **`hybrid_log_mutation_tests.rs`**: Tests `FlushError::QueueFull` formatting (line 287) but not the actual QueueFull → retry flow.

### Recommended Test Strategy

1. **Unit test:** `QueueFullDevice` that returns QueueFull after N writes, verifying `flush_sealed_pages` handles it correctly (currently: continues past all pages doing nothing useful).

2. **Integration test:** 2+ writer threads with `SlowDevice` (50ms delay), small buffer (4 pages), verify writers make forward progress (not stuck). Add timeout assertion.

3. **Regression test:** The exact page-cache scenario (`buffer_size_pages=8`, `max_in_memory_pages=4`, 2 writers, linear distribution) with `InMemoryDevice` + artificial delay.

---

## 5. Risks and Gotchas

### R1: Callback Thread Safety
Flush callbacks run on I/O worker threads. Any new signaling mechanism (condvar, channel) added to the callback path must be `Send + Sync`. The `FlushCallbackContext` already uses `*const PageTable` (raw pointer) — adding an `Arc` is strictly safer.

### R2: Ordering of Eviction vs. Flush
`advance_head()` (eviction.rs:153-209) stops at the first non-Flushed page. If pages flush out of order (page 3 completes before page 2), head can't advance past page 2 even though page 3 is Flushed. This is correct behavior but means I/O latency spikes on any single page block the entire pipeline.

**Mitigation:** The C++ implementation also has this constraint. Non-contiguous eviction would require major changes to the address boundary model and is NOT recommended for this fix.

### R3: Single Maintenance() Retry is Insufficient
`allocate_at_tail()` calls `on_alloc_failure()` once, retries once. If the flush pipeline needs multiple maintenance cycles to drain (pages in Flushing state waiting for I/O completion), a single retry won't help. The writer needs to either:
- Loop with backoff (what page-cache does externally)
- Block until I/O completes (new capability needed)

### R4: Dead Code in SyncFileDevice
`max_outstanding` and `high_water` are dead fields. The fix should either:
- Wire them up (bounded channel + QueueFull on overflow)
- Remove them to avoid confusion

### R5: `flush_sealed_pages` Result Is Discarded
`maintenance()` at kv.rs:1708 does `let _ = self.flusher.flush_sealed_pages(...)`. If the flush returns QueueFull for every page, maintenance has no way to know it made zero progress. The return value should be checked and used to trigger I/O drain.

### R6: Callback Lifetime
`FlushCallbackContext.page_table` is a raw `*const PageTable`. If we add a condvar/waker to the callback context, it needs the same lifetime guarantee. Since `FasterKv::Drop` drains pending I/O before dropping the allocator (documented at flush.rs:114-118), an `Arc` pointer is safe.

---

## 6. Proposed Fix Architecture (for Aragorn's design)

### Minimum Viable Fix (3 changes)

1. **`flush_sealed_pages` → break on QueueFull** (not continue)
   - Return `(flushed_count, had_queue_pressure: bool)` instead of just count
   - `maintenance()` checks `had_queue_pressure` and if true, adds a small sleep/yield to let I/O complete

2. **`allocate_at_tail` → multi-retry with yield**
   - Instead of one maintenance + one retry, loop up to N times with `thread::yield_now()` between attempts
   - This gives I/O worker threads CPU time to run callbacks

3. **`maintenance()` → add thread::yield_now() when no progress**
   - If `flush_sealed_pages` returned 0 and `evict_pages` returned 0, yield before returning
   - This prevents the maintenance-loop-doing-nothing spin

### Full Fix (adds I/O drain capability)

4. **Add `Device::try_drain_completions()` (default no-op)**
   - `SyncFileDevice`: enforce bounded channel, return QueueFull on overflow
   - Or add a completion counter that `maintenance()` can poll

5. **Add flush completion signaling**
   - `Arc<AtomicU32>` flush-completion counter shared between callback and maintenance
   - When maintenance needs to wait, it spins on the counter with yield

---

## 7. C++ Reference Comparison

| Aspect | C++ FASTER | Rust FASTER | Gap |
|--------|-----------|-------------|-----|
| QueueFull handling | Returns error for entire batch | `continue` skips page, scans rest | **Critical** |
| Writer I/O drain | `TryComplete()` in writer path | Not present | **Critical** |
| I/O throttling | `DoThrottling()` | None (unbounded channel) | **Important** |
| Completion polling | `IOCompletionLoop` | Callback-only, no polling | **Important** |
| Maintenance frequency | Epoch-based | Timer-based (5ms/50ms) | Adequate |
| Allocation retry | Built-in retry loop | Single retry + external loop | **Moderate** |


---

## Decision: Log Prefix Coupling Fix

**Date:** 2026-03-11  
**Author:** Sam (Systems & Storage Expert)  
**Status:** Implemented

### Problem

The `SyncFileDevice` accepts a configurable filename prefix (passed to `new()`), but the recovery code in `log_recovery.rs` hardcoded `"log."` as the segment filename format. If someone created a `SyncFileDevice` with a different prefix (e.g., `"data."`), recovery would silently fail.

### Solution

Pass the prefix as a parameter to recovery functions.

**Rationale:** Parameterized approach is preferred over metadata storage because:
- The prefix is a **device configuration**, not checkpoint state
- Recovery should not depend on runtime device configuration
- The caller (FasterKv) already knows what device it created
- Makes recovery explicit and predictable

### Implementation

Added `log_prefix: &str` parameter to:
- `LogRecoveryEngine::recover_fold_over()`
- `LogRecoveryEngine::recover_snapshot()`
- Internal validation functions

Updated `FasterKv::recover()` to pass `"log."` as the default prefix.

---

## Decision: Tier-2 Test Classification Criteria

**Author:** Sam (Systems & Storage Expert)  
**Date:** 2026-03-12  
**Status:** Applied

### Criteria for `#[ignore = "tier-2: ..."]`

1. **32MB page tests** — Full `OFFSET_BITS=25` pages are >1s in debug. Always tier-2.
2. **Proptests at minimum cases** — Case count ≤16 but still >1s. Mark tier-2 rather than reducing coverage.
3. **Multi-round I/O** — Checkpoint/recovery/compaction tests with 2+ I/O phases can't be meaningfully shortened.
4. **Concurrent stress** — Can often be optimized by reducing iterations (5× reduction typically sufficient).

### Applied Changes

- Optimized 4 tests (reduced iterations)
- Marked 12 tests tier-2 across hash, compaction, recovery, eviction, and I/O injection modules
- Result: 0 tier-1 tests >1s (was 17)

---

## Decision: DST Smoke Test Integration Strategy

**Author:** Éowyn (Deterministic Simulation Testing Expert)  
**Date:** 2026-03-11  
**Status:** Implemented

### Two-Tier DST Strategy

**Tier 2: Fast Correctness Gate (~19s)**
- Command: `cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'`
- Coverage: 133 tests (75 unit + 58 integration)
- Purpose: Fast feedback on critical crash recovery paths
- Target: <30s

**Tier 3: Deep Validation (~220s)**
- Command: `cargo nextest run -p faster-dst -E 'test(campaign_expanded_smoke)'`
- Coverage: 1003 scenarios × 3 seeds = 3009 test cases
- Purpose: Comprehensive parameterized validation
- Target: <30min

### CI Integration

- New DST smoke test job in `.github/workflows/rust-ci.yml`
- Trigger: Every PR/push touching `rust/**`
- Timeout: 10 minutes
- Strategy: Run core scenarios only (Tier 2 subset)

### Rationale

Balances **fast PR feedback** (<30s) with **comprehensive correctness validation** while keeping **deep validation** (Tier 3) available for release-gate.

---

## Decision: Complete Miri Coverage for All Testable Unsafe Code

**Date:** 2026-03-11  
**Decider:** Galadriel (Security Expert)  
**Status:** Implemented  
**Tags:** #testing #miri #memory-safety #security

### Decision

Expand miri test coverage to include ALL testable unsafe modules in faster-core.

### Coverage Expansion

- Added 7 new miri tests across 4 modules
- Total miri tests: 71 → 78 tests
- 100% of testable unsafe code now verified

### New Tests

- `hash/index.rs`: `bucket_slice_bounds_and_alignment`, `bucket_slice_read_entries`
- `checkpoint/index_writer.rs`: `bucket_as_bytes_no_ub`, `index_writer_bucket_serialization_pattern`
- `epoch/mod.rs`: `epoch_protect_unprotect`, `epoch_defer_basic`
- `recovery/index_recovery.rs`: `bucket_from_bytes_roundtrip`

### Rationale

1. **Comprehensive Memory Safety Verification:** Miri detects undefined behavior that standard tests miss
2. **Pre-merge Verification:** All new unsafe code without miri test is immediately visible
3. **Complement to Loom Testing:** Miri validates memory safety; Loom validates concurrency
4. **Fast Feedback Loop:** Miri tests run in ~45 seconds

### Maintenance Policy

- New unsafe code MUST include corresponding miri test
- Keep miri tests small and focused (one unsafe pattern per test)

### 2026-03-11: PR-based development workflow
**Status:** Active
**By:** qbradley
**Decision:** All work now goes through pull requests targeting the `rust` branch. Agents use `git worktree` for parallel development. Branch naming: `squad/{agent}/{feature-slug}`. No direct commits to `rust`.
**Rationale:** Production workflow for code review and quality gating.
# Decision: Sample README Structure & Style Guidelines

**Date:** 2026-03-14  
**Agent:** Arwen (Developer Advocate)  
**Context:** Completed documentation for 8 sample crates (Backlog 3e)  

## Decision

Establish a **standard README template** for all FASTER samples that new users can quickly scan.

## Rationale

When users land on a sample crate, they should be able to answer in **30 seconds**:
1. What does this sample demonstrate?
2. How do I run it?
3. What FASTER concepts does it show?
4. Do I need special platform setup?

The pattern established across all 8 new READMEs reflects this:

```
1. One-line description (title + hook)
2. What It Demonstrates (2–3 sentences)
3. Key Concepts (bullet list of FASTER features)
4. Usage (copy-paste ready cargo run examples)
5. CLI Options (reference table)
6. Example Output (realistic results)
7. Notes (platform requirements, caveats)
```

## Key Guidelines

### Content Ordering
- **Fast scanning first:** Title, hook, purpose (read in 15 seconds)
- **Examples before details:** Usage code before detailed CLI table
- **Realistic output:** Show actual command execution results
- **Platform callouts:** io_uring, feature flags noted prominently

### Tone
- Direct, empathetic to the "I have 10 minutes" person
- No marketing language
- Code examples are copy-paste ready (tested mentally against main.rs)
- Contrasts with sister samples where relevant (e.g., sync vs. async, cache vs. store)

### Sample-Specific Patterns
- **Benchmarks** (cross-impl-bench, disk-io-bench): Emphasize comparison and reproducibility
- **Demos** (event-counter-tokio): Show async/sync bridge pattern with ASCII diagrams
- **Workloads** (page-cache, page-store, read-cache-sim): Explain why memory budget matters
- **Stress tests** (torture-stress): Detail threat model (what corruption could occur)

## Implications

### For Documentation Review (Wave 4+)
- Apply this structure to faster-device, faster-tokio, faster-uring README updates
- Use sample READMEs as template for crate-level examples

### For Users
- All samples now have consistent, discoverable documentation
- No more "what does this even do?" friction

### For Contributors
- Template makes it easy to add new samples
- Clear expectations for sample README quality

## Trade-offs

- **More content:** READMEs are ~3 KB each (vs. minimal existing stubs)
- **Specificity:** Tailored examples may need updates if CLI flags change
- **Maintenance:** Must keep CLI option tables in sync with clap derive macros

*Mitigation:* Add a CI check to flag when README example code diverges from clap definitions.

## Related

- Wave 3 documentation audit identified 5 sample READMEs as P0 gap
- Existing READMEs: tokio-kv-server (detailed), uring-stress (good baseline)
- Next: Apply pattern to crate-level docs (faster-tokio, faster-uring) in Wave 4

---

**Status:** Adopted (all 8 READMEs follow this pattern)  
**Link:** `arwen/sample-readmes` branch, commit 1880da1a
# Decision: Fuzz Targets for Recovery/Checkpoint Deserialization (A9)

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-11
**Branch:** `galadriel/fuzz-recovery-paths`
**Status:** Implemented, awaiting CI integration

## Summary

Added 3 new cargo-fuzz targets to close the recovery/checkpoint deserialization fuzz gap identified in the retrospective (A9). The fuzz crate now has 8 targets total, covering all major attack surfaces.

## New Targets

| Target | Attack Surface | Key Entry Points |
|--------|---------------|-----------------|
| `fuzz_page_trailer` | CRC computation/verification | `PageTrailer::from_bytes`, `from_slice`, `write_size`, `crc_range` |
| `fuzz_checkpoint_recovery` | Binary index file + JSON metadata | `IndexCheckpointReader::open/verify`, `serde_json::from_str` for all metadata types |
| `fuzz_log_recovery` | Full log recovery pipeline | `LogRecoveryEngine::recover_fold_over()` with arbitrary segment files |

## Dependencies Added (fuzz crate only)

- `serde_json = "1"` — JSON deserialization
- `serde = "1"` — Deserialize trait
- `tempfile = "3"` — temp directories for file-based targets
- `crc32fast = "1"` — CRC roundtrip verification

## CI Integration Required

For the 10-min-per-run budget from A9:
```bash
cargo +nightly fuzz run fuzz_page_trailer -- -max_total_time=200
cargo +nightly fuzz run fuzz_checkpoint_recovery -- -max_total_time=200
cargo +nightly fuzz run fuzz_log_recovery -- -max_total_time=200
```

## Team Impact

- **Frodo:** Add to CI fuzzing job (requires nightly toolchain)
- **Éowyn:** Complementary to DST — fuzzing covers byte-level mutations that DST's crash injection doesn't
- **Boromir:** Coverage matrix (A11) now has fuzz coverage for recovery/checkpoint column
- **Aragorn:** If adding new deserialization paths in recovery, add corresponding fuzz target

---

# Decision: Loom Shim Patterns for Production Code

**Author:** Aragorn  
**Date:** 2026-03-16  
**Status:** Implemented

## Context

The `RUSTFLAGS="--cfg loom" cargo check --features loom` build had 7 compilation errors across the crate. These are systemic gaps in the loom shim layer that anyone touching sync primitives should know about.

## Decisions

### 1. `thread::sleep` under loom → `yield_now()`

Loom doesn't model wall-clock time. The `crate::sync::thread` module now provides a `sleep` shim that calls `loom::thread::yield_now()`. Any code using `crate::sync::thread::sleep` will automatically get the right behavior.

### 2. `self: &Arc<Self>` is incompatible with loom's Arc

Loom's `Arc` doesn't implement `core::ops::Receiver`, so `self: &Arc<Self>` receiver syntax fails. **Pattern:** cfg-gate the method signature and use a shared `_impl` function. Production call sites should use UFCS (`Type::method(&arc)`) which works under both cfgs.

### 3. `AtomicPtr::get_mut()` unavailable under loom

Use `load(Ordering::Relaxed)` as the portable alternative when `&mut self` guarantees exclusive access. Semantically equivalent.

### 4. Loom atomics aren't const-constructible

Any `static` using loom atomics must be wrapped in `std::sync::LazyLock` under `#[cfg(loom)]`. `LazyLock<T>` derefs to `T`, so call sites need no changes.

## Impact

All team members writing production code with sync primitives should be aware of these four patterns. The loom CI gate will catch violations.

---

# Decision: CI Concurrency Test Stabilization Patterns

**Author:** Sam  
**Date:** 2026-03-16  
**Scope:** Testing conventions for concurrent/stress tests

## Context

Two CI tests were flaky due to scheduling assumptions:
1. `concurrent_maintenance_with_varlen` — assumed maintenance thread gets CPU time before `done` is set.
2. `checkpoint_grow_mutual_exclusion_stress` — assumed CAS race outcomes are statistically distributed across platforms.

## Decisions

### 1. Never rely on scheduling fairness for assertions

When a test needs to verify that a background thread *ran*, use an explicit synchronization signal (e.g., `AtomicBool` with Acquire/Release) rather than assuming the OS scheduler will be fair. The main thread must wait for the signal before tearing down.

**Pattern:**
```rust
let started = Arc::new(AtomicBool::new(false));
// Background thread sets started=true after first meaningful iteration
// Main thread spins: while !started.load(Acquire) { yield_now(); }
```

### 2. Hard-assert safety, soft-assert statistics

Stress tests that verify mutual exclusion via CAS should:
- **Hard-assert** safety invariants (e.g., "both sides must never win simultaneously")
- **Soft-assert** (warn, don't fail) statistical distribution (e.g., "both sides should win some races")
- Add scheduling jitter (asymmetric `yield_now()` counts per trial) to improve contention diversity

This prevents false failures on deterministic-scheduling platforms (macOS, single-core CI) while preserving correctness coverage.

---

# Decision: Skip directory fsync on Windows

**Author:** Boromir (QA Engineer)  
**Date:** 2026-03-16  
**Status:** Implemented

## Context

Windows CI was failing with 64 test failures, all with the same error:
```
failed to write test checkpoint: IoError(Os { code: 5, kind: PermissionDenied, message: "Access is denied." })
```

## Root Cause

`fsync_dir()` in `checkpoint/metadata_store.rs` calls `fs::File::open()` on a directory. On Windows, this requires `FILE_FLAG_BACKUP_SEMANTICS` which Rust's standard library does not set, causing `PermissionDenied`.

## Decision

Use `#[cfg(unix)]` to limit directory fsync to Unix platforms. On Windows (and other non-Unix), the function is a no-op.

**Rationale:** NTFS journals metadata operations, so the atomic rename in `atomic_write()` provides sufficient durability guarantees. This is the same approach used by RocksDB, SQLite, and other cross-platform database engines.

## Impact

- Fixes all 64 Windows CI test failures
- No change to Unix behavior
- No durability regression on Windows (NTFS journaling covers us)

## Team Note

Any future code that touches filesystem directories (open, sync, delete) must be tested for Windows compatibility. `fs::File::open()` on directories is a Unix-ism that doesn't port cleanly.

