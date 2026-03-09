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
