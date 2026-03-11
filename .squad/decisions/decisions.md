# Decision Log

## [2026-03-11] Team Retrospective — Rust FASTER Milestone

**Author:** Gandalf (Lead / System Architect)  
**Date:** 2026-03-11  
**Status:** Informational — team retrospective  
**Participants:** Gandalf, Aragorn, Sam, Legolas, Boromir, Éowyn, Galadriel, Elrond, Arwen

---

## Context

The Rust FASTER implementation has reached a significant quality milestone. This retrospective covers the full project journey from inception (2026-03-05) through the current state: a production-grade concurrent durable hash map with 2,102 tests, 107K lines of Rust, 1,003 DST scenarios, 21 loom tests, 71 Miri tests, and a 60-minute torture stress test with zero oracle violations.

---

## Metrics Snapshot

| Metric | Value |
|--------|-------|
| Total Rust LOC (all crates) | 107,454 |
| faster-core source LOC | 51,658 |
| faster-core test LOC | 23,488 |
| Tests registered (nextest) | 2,102 |
| DST scenarios | 1,003 (44 categories) |
| Loom concurrency tests | 21 |
| Miri memory safety tests | 71 |
| Fuzz targets | 5 |
| Mutation testing (hybrid_log) | 399+ mutants, 100% kill rate |
| Torture stress test | 203M ops / 60 min / 0 violations |
| Crates in workspace | 8 (+ samples) |
| Unsafe sites audited | 302 |
| Benchmark peak throughput | 49.17M ops/s (16T, Workload C) |
| Precheckin time | ~38s |
| Active decisions | 30+ |
| Team size | 9 active agents + 4 alumni |

---

## 1. What Went Well 🟢

### Architecture-first approach paid off
Gandalf's 6,101-line architecture spec (12 binding decisions) written on Day 1 prevented rework. Key decisions — no async in core, callback-based Device trait, thread-affine sessions, custom epoch — were never revisited. Every agent worked from the same structural blueprint. The architecture survived contact with reality.

### Multi-layer verification is world-class
Five distinct verification modalities working in concert:
- **Unit + Integration** (2,102 tests, 38s) — Aragorn and Sam built the foundation
- **Miri** (71 tests) — Galadriel verified every unsafe module
- **Loom** (21 tests) — Éowyn covered all 5 critical concurrency subsystems
- **DST** (1,003 scenarios) — Éowyn built a FoundationDB-style crash simulation framework from scratch
- **Mutation testing** (399+ hybrid_log mutants) — Aragorn and Boromir achieved 100% kill rate

This layered approach caught bugs no single modality would find. The MF-1 CRC trailer corruption bug was caught by SoT review. The checkpoint stall was found by torture stress. The reader panic was found by lossy eviction testing.

### Aragorn: Rust craftsmanship
Aragorn delivered consistently clean, idiomatic code: the OperationOutcome API (context recovery on aborted operations), UnsafeContext with epoch-amortized batching (9.8x read throughput improvement), compaction scanner, and mutation testing infrastructure. The `let _ =` audit converting 24 silently-discarded Results to proper assertions was the kind of unglamorous work that prevents production incidents.

### Sam: Systems depth
Sam owned the hardest correctness work — sealing protocol, EPVS implementation, revivification, lossy cache mode, and the reader panic fix. Sam's learnings about epoch protection not preventing eviction, and the 4x buffer headroom requirement for async flush, are the kind of operational insights that come only from deep systems work.

### Éowyn: DST framework is a force multiplier
Growing from 5 → 108 → 1,003 scenario templates with parameterized expansion was exceptional. The campaign runner with seed-controlled fault injection, crash points across 18 subsystem phases, and graduated fault injection gives us FoundationDB-tier testing confidence. The loom expansion (7 → 21 tests, 1800 LOC) was equally thorough.

### Legolas: Performance became measurable
The benchmarking infrastructure — YCSB suite, two-level prefetch pipeline, baseline management, regression detection with 5% threshold gating — transformed performance from anecdotal to scientific. Identifying the throughput cliff root cause (3-point deadlock in flush/evict pipeline) was a critical contribution.

### Boromir: Mutation testing rigor
Boromir's systematic approach to mutation-killing tests — 17 gap survivors with documented patterns (boundary mutations, counter mutations, arithmetic mutations) — established the methodology. The release-gate script unifying all 4 validation tiers was the infrastructure the team needed.

### Galadriel: Security audit was definitive
The 302-site unsafe audit with conditional pass, catching the critical FFI panic safety gap (all 12 extern "C" functions lacking catch_unwind), and the compiler-enforced `#![forbid(clippy::undocumented_unsafe_blocks)]` lint gave the project a verified security posture. Zero SAFETY comments missing.

### Elrond: Async bridge done right
The callback→Future bridge is runtime-agnostic (zero Tokio types), cancel-safe, and single-allocation. The 29 integration tests covering lifecycle, concurrent sessions, checkpoint recovery, and shutdown-under-load proved the pattern works. The `spawn_blocking` approach for !Send sessions is the correct pattern.

### Arwen: Documentation infrastructure
Documentation audit (7.5/10 rating), CHANGELOG, release notes template, and blog posts created organizational memory. The P0 gap identification (missing sample READMEs) gave the team a clear docs roadmap.

### Precheckin script: the unsung hero
38 seconds, 2,102 tests, zero flaky tests. Every agent cited this as the foundation that made parallel work possible. No one had to wonder "did I break something."

---

## 2. What Could Be Improved 🔴

### Push discipline was dangerously lax
At peak, we had 58 unpushed commits — all safety-critical code on a single VM. The EMU auth blocker persisted across multiple sessions. This was the team's #1 concern in the prior retro and improved only partially.

### Shared working directory caused preventable conflicts
Multiple agents switching branches in the same working tree caused: stash pop failures losing uncommitted changes, `git add -A` staging unintended files, concurrent `/tmp/commit-msg.txt` collisions, and agents destructively reverting each other's files. The `git worktree` recommendation was made early but not universally adopted.

### DST framework not in CI
1,003 scenarios exist but no CI target runs them on PRs. The framework has proven its value (bugs found!) but remains "shelf-ware" until integrated into the automated pipeline. Same for loom and extended Miri suites.

### Multi-writer stall is an unsolved core bug
Legolas identified a 3-point deadlock: SF-10 buffer overflow check blocks writers → head only advances contiguously past Flushed pages → device back-pressure reverts pages to Sealed. This makes >1 concurrent writer with linear (all-new-key) distributions unusable. This is a **core library issue**, not a sample bug.

### No coverage metrics
2,102 tests with no `cargo-llvm-cov` or `tarpaulin` integration. We cannot objectively prove which code paths are exercised. Mutation testing partially fills this gap but isn't a substitute.

### Loom tests re-implement algorithms instead of testing production code
The `crate::sync` loom shim exists but isn't wired into production code. All 21 loom tests faithfully re-implement FASTER algorithms using loom primitives. This means loom validates the algorithms are correct, not that the production code implements them correctly. A divergence between loom test and production code would go undetected.

### PAW process was heavy for some tasks
7 implementation phases with SoT debate review for DST was thorough but created artificial integration seams. The team consensus was to cap PAW phases at 4-5 for future features and reserve heavyweight review for safety-critical work only.

---

## 3. What We Learned 🔵

### Technical Insights

- **Epoch protection doesn't prevent eviction.** Sam discovered that epoch guards are advisory, not preventive, when a separate maintenance thread drives lossy eviction concurrently. This led to fixing three operation paths (read, RMW, delete) that assumed epoch safety.

- **Buffer needs 4x headroom for async flush.** FASTER's Sealed→Flushing→Flushed pipeline is asynchronous. The tail can lap the head while waiting for I/O callbacks. 4x target_pages prevents this.

- **Maintenance thread interval is a Goldilocks problem.** 1ms interval *hurts* throughput by 38% vs 5ms due to CPU contention with the I/O thread pool. Counter-intuitive but reproducible.

- **Throughput cliff has a structural root cause.** Not a tuning problem — it's a 3-point deadlock in the flush/evict pipeline. The eager flush watermark (75% buffer capacity) and increased eviction batch size (4→16) were mitigations, not fixes.

- **Mutation testing: bitwise const ops produce unviable mutants.** `<<`→`>>` and `-`→`+` in const bit-mask definitions fail to compile. This is the type system catching "mutations" that aren't real test gaps.

- **RecordInfo(0) is a valid first-version record, not null memory.** This subtlety caused the compaction scanner's sentinel alignment bug.

- **Single-allocation async bridge works.** One `Arc<Mutex<SharedState>>` per pending operation, runtime-agnostic, cancel-safe. No need for channels or complex state machines.

### Process Insights

- **Multi-agent coordination needs explicit protocol.** Shared files (Cargo.toml, mod.rs) are collision magnets. Stage + commit immediately. Use `git worktree` for isolation.

- **SoT debate review justifies its cost for safety-critical code.** The MF-1 CRC corruption bug was found by review, not by any automated test. But the cost is too high for leaf features.

- **Smaller PRs merge cleaner.** Average merge time dropped significantly when scope was constrained. Feature branches per agent with consolidation merges to integration branch was the correct strategy.

- **Property tests need careful tuning.** 64 proptest cases × 240ms each = 15s per property. For <1s tier-1, use 2 cases. Full counts belong in tier-2.

---

## 4. Surprises 😮

### Good surprises
- **203M ops / 60 minutes / 0 oracle violations.** The torture stress test passed on the first serious run. This was not expected given the checkpoint race condition and reader panic bugs that were still being fixed in the same period.
- **49.17M ops/s peak throughput.** Within 14% of C# Tsavorite. For a ground-up Rust implementation that prioritized correctness over performance, this was ahead of projections.
- **100% mutation kill rate on hybrid_log.** 399+ mutants with zero survivors after targeted tests. The test suite is genuinely strong, not just large.
- **UnsafeContext batch API: 9.8x read throughput.** Epoch amortization had a dramatically larger impact than predicted (expected 2-3x, got nearly 10x).

### Bad surprises
- **Multi-writer permanent stall.** A fundamental deadlock in the core flush/evict pipeline, not a configuration issue. Affects all write-heavy multi-writer workloads with linear key distributions.
- **io_uring degrades under sustained load.** Peaks 24% above SyncFileDevice but ends 23% slower over 5 minutes. The completion processing contends with the eviction pipeline.
- **`cargo-mutants -F` is substring match.** `-F 'address.rs'` also matches `begin_address.rs`. Caused false positives in mutation testing. Use `-F 'src/address.rs'` for precision.
- **Checkpoint race condition between compaction and checkpoint.** Caused a 30-second stall. The interaction between two independently-correct subsystems created an emergent bug.

---

## 5. Risks & Technical Debt ⚠️

### P0 — Must fix before production

| Risk | Description | Owner |
|------|-------------|-------|
| Multi-writer deadlock | 3-point deadlock in flush/evict pipeline. >1 writer with linear keys permanently stalls. | Aragorn + Sam |
| 4 High security findings open | Device callback lifetime, PendingIoContext dangling pointer, FFI SessionCell Send+Sync, Tokio raw pointer cast. | Galadriel tracks |
| Snapshot checkpoint not functional | `CheckpointType::Snapshot` returns error. FoldOver works. | Sam |
| Loom shim not wired to production | Loom tests validate algorithms, not production code. Any drift is undetected. | Éowyn + Aragorn |

### P1 — Should fix before GA

| Risk | Description | Owner |
|------|-------------|-------|
| No coverage metrics | Cannot prove code path coverage objectively. | Boromir |
| DST not in CI | 1,003 scenarios exist but run only manually. | Éowyn |
| SyncFileDevice "log." prefix coupling | Recovery hardcodes `log.{n}` filenames. Non-"log." prefix silently fails. | Sam |
| Missing sample READMEs | 5 sample crates have no README. P0 per docs audit. | Arwen |
| io_uring sustained load degradation | Needs investigation — completion processing contention with eviction pipeline. | Legolas |

### P2 — Track for future

| Risk | Description |
|------|-------------|
| Variable-length records untested in DST | Only u64→u64 pairs tested |
| No cross-platform CI | Tests only run on Linux x86_64 |
| ABA tag in allocator is 16-bit | Sufficient with epoch but fragile without |
| Multi-checkpoint recovery untested | DST framework supports single checkpoint per scenario |

---

## 6. Action Items 📋

### Immediate (next session)

| # | Action | Owner | Rationale |
|---|--------|-------|-----------|
| A1 | Investigate multi-writer deadlock root cause and fix | Aragorn + Sam | Only P0 correctness bug remaining |
| A2 | Wire `crate::sync` loom shim into production code | Aragorn + Éowyn | Loom tests must test real code |
| A3 | Add `dst_smoke` target to CI (Tier 2) | Éowyn | 1,003 scenarios are shelf-ware without CI |
| A4 | Add `cargo-llvm-cov` to CI | Boromir | Flying blind on coverage |

### Near-term (within 3 sessions)

| # | Action | Owner | Rationale |
|---|--------|-------|-----------|
| A5 | Fix SyncFileDevice "log." prefix coupling | Sam | Store prefix in checkpoint metadata |
| A6 | Address 4 High security findings | Galadriel + relevant owners | Pre-GA requirement |
| A7 | Write missing sample READMEs (5 crates) | Arwen | P0 per docs audit |
| A8 | Investigate io_uring sustained load degradation | Legolas | Understand before recommending io_uring |
| A9 | Implement Snapshot checkpoint | Sam | Listed in MVP scope |

### Process improvements

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Cap PAW phases at 4-5 | Reduce artificial integration seams |
| D2 | Push after every green precheckin | Never exceed 5 unpushed commits |
| D3 | Use `git worktree` universally | Prevent cross-agent file destruction |
| D4 | SoT debate review for safety-critical only | Reserve heavyweight review for where it matters |
| D5 | Quick check script as team standard | 9s fmt+tests+clippy for inner-loop iteration |

---

## Team Appreciation 🏆

This team built a production-grade concurrent durable hash map in Rust — from architecture spec to 2,102 tests, 107K LOC, 1,003 crash simulation scenarios, formal concurrency verification, and a 60-minute torture stress test — in under two weeks of wall-clock time. The quality bar is comparable to production systems at companies that take months to reach this point.

Special recognition:
- **Sam** for the deepest systems debugging work (epoch/eviction interaction, checkpoint race, reader panic)
- **Éowyn** for building a testing framework that would make FoundationDB engineers nod approvingly
- **Aragorn** for consistently clean code and the OperationOutcome API that makes the library a pleasure to use
- **Legolas** for making performance a science instead of folklore
- **Galadriel** for the security audit that found the critical FFI panic safety gap before it found us
- **Boromir** for the mutation testing discipline and release-gate infrastructure
- **Elrond** for an async bridge that proves zero-overhead abstraction isn't just a slogan
- **Arwen** for organizational memory — changelogs, docs audits, blog posts that make this work legible

The road to 1.0 is visible from here. The multi-writer deadlock is the one true dragon left to slay.

— Gandalf
