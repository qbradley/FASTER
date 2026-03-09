# FASTER Rust — Pre-Release Quality & Release Management Plan

## Context

Iteration 6 complete. 1,456+ tests across 6 crates. Rust FASTER leads C# on write-heavy workloads (+15.5%), read gap closed to -14%. Before public release, two major initiatives:

1. **Quality Validation Upleveling** — Ensure automated validation is comprehensive enough that passing = customer-ready
2. **Release Management Process** — Reliable, predictable system for determining release readiness

## Test Economics Model (User Directive)

| Activity | Cost | Implication |
|----------|------|-------------|
| Writing/reading/maintaining test code | **~Free** (AI) | Can be as sophisticated as needed |
| Running tests | **Real money** | Must run fast |
| Investigating failures | **Real money** | Zero false positives |
| Calendar time | **Real money** | Parallelize, automate |

**Anti-pattern**: Flaky tests that cost to run + investigate but don't find real bugs.
**Ideal**: Very advanced test suite, lots of code, runs quickly, zero false positive bug reports.

---

## Part 1: Quality Validation Architecture

### Current State Assessment

**Strengths:**
- 1,456+ tests, 82 source files with unit tests (82% file coverage)
- 11,095 LOC of integration tests across 15 files
- Property-based tests (proptest, 19+ blocks)
- Loom deterministic concurrency tests (664 LOC)
- Miri UB detection tests (757 LOC)
- 5 fuzz targets with corpus (92 seeds)
- CI: 7 GitHub Actions jobs (fmt, ci, loom, miri, coverage, bench, deny)
- Precheckin gate: fmt, clippy, doctests, nextest
- Zero flaky tests, no sleep-based synchronization

**Critical Gaps:**
1. **Memory pressure / OOM** — ZERO tests (CRITICAL)
2. **Compaction** — 1 small synthetic test, no real-scale (CRITICAL)
3. **Hybrid log management** — no dedicated eviction/flush/regions tests (HIGH)
4. **Async/Tokio integration** — 37 scattered inline tests, no dedicated suite (HIGH)
5. **Device I/O errors** — all tests use InMemoryDevice/NullDevice (HIGH)
6. **Recovery edge cases** — happy path only, no corruption/partial checkpoint (MEDIUM-HIGH)
7. **Unsafe code coverage** — 47 files with unsafe, only ~10 have explicit miri tests (MEDIUM)
8. **Concurrent maintenance ops** — no explicit checkpoint+CRUD or compaction+CRUD tests (MEDIUM)

**Missing test categories entirely:**
- Mutation testing (test quality measurement)
- Deterministic simulation testing (FoundationDB-style)
- Crash consistency testing (write then simulate crash then verify recovery)
- Fault injection testing (I/O failures, partial writes, corruption)
- Performance regression detection (automated, not manual benchmarks)
- cargo-semver-checks (API compatibility)

### Tiered Validation Architecture

#### Tier 1: Fast Gate (under 60s, every commit via precheckin)
- cargo fmt --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --doc --workspace
- cargo nextest run --workspace --all-targets
- **ADD**: cargo semver-checks (catch accidental API breaks)
- **ADD**: Compile-time assertions for key invariants

#### Tier 2: Correctness Gate (under 5 min, every PR via CI)
- Loom tests (deterministic concurrency) — exists, expand
- Miri tests (UB detection) — exists, expand to all unsafe modules
- Fuzz smoke test (10s/target) — exists
- **ADD**: Fault injection tests (I/O errors, partial writes)
- **ADD**: Crash consistency tests (simulated crash then recovery verification)
- **ADD**: Property-based state machine tests (any op sequence then invariants hold)

#### Tier 3: Deep Validation (under 30 min, nightly/pre-merge)
- Extended fuzz (5 min/target)
- Stress tests (16-thread, high contention scenarios)
- **ADD**: Mutation testing (cargo-mutants on critical modules)
- **ADD**: Deterministic simulation testing (FoundationDB-style)
- **ADD**: Performance regression detection (VM-based, automated comparison)

#### Tier 4: Release Gate (under 2 hours, pre-release only)
- Full fuzz corpus (30+ min)
- Full benchmark suite on dedicated VM
- Cross-platform CI (Linux + macOS + Windows)
- cargo-semver-checks against last published version
- CHANGELOG validation
- Documentation build + link checker
- Mutation testing full workspace
- Extended simulation testing (1000+ scenarios)

### New Testing Capabilities to Build

#### A. Deterministic Simulation Testing Framework (HIGH VALUE)
**Inspired by**: FoundationDB, Antithesis, MadSim, S2.dev
**Core idea**: Run actual FASTER code under controlled scheduler that abstracts all nondeterminism (thread scheduling, I/O completion timing), injects faults systematically, and makes every failure perfectly reproducible from a seed.
**Why ideal for our economics**: Lots of code (cheap), runs single-threaded (fast), finds real bugs deterministically (zero false positives), seed-based reproduction eliminates investigation cost.
**Focus areas**: Checkpoint state machine, compaction, hybrid log flush/eviction, recovery.

#### B. Fault Injection Device + Crash Consistency Tests (HIGH VALUE)
**Fault injection**: Pluggable Device impl that returns errors at configured points, corrupts data at offsets, simulates partial writes and disk-full.
**Crash consistency**: Record all I/O during workload, for each I/O boundary truncate/corrupt, run recovery, verify committed data present and no phantom data.
**Pattern**: FaultInjectionDevice::new(inner).fail_write_at(page: 5).corrupt_read_at(offset: 1024)

#### C. Property-Based State Machine Testing (HIGH VALUE)
Generate random op sequences, verify invariants hold after any interleaving:
- After any sequence of Read/Upsert/RMW/Delete, hash index is consistent
- After checkpoint + recovery, all committed ops are durable
- Compaction preserves all live records exactly

#### D. Mutation Testing Integration (MEDIUM VALUE — measures test quality)
Tool: cargo-mutants — injects small code changes, verifies tests catch them.
Usage: Periodic (nightly), focused on critical modules. Tells us WHERE tests are weak.

#### E. Performance Regression Detection (MEDIUM VALUE)
Benchmarks on dedicated VM (never local), results compared to stored baselines, more than 5% regression = failure. Automated in CI for main branch pushes.

---

## Part 2: Release Management Process

### Versioning Strategy
- SemVer 2.0: MAJOR.MINOR.PATCH
- Initial release: **0.1.0** (pre-1.0, API may change)
- Pre-release: 0.1.0-alpha.1 then 0.1.0-beta.1 then 0.1.0-rc.1 then 0.1.0

### Release Quality Gates (All Must Pass)

| Gate | Tool | Tier | Required |
|------|------|------|----------|
| Format check | cargo fmt --check | 1 | Yes |
| Lint clean | cargo clippy -- -D warnings | 1 | Yes |
| All tests pass | cargo nextest run | 1 | Yes |
| Doc tests pass | cargo test --doc | 1 | Yes |
| API compat | cargo-semver-checks | 1 | Yes |
| Loom clean | Loom tests | 2 | Yes |
| Miri clean | Miri tests | 2 | Yes |
| Fuzz clean | 60s/target all targets | 2 | Yes |
| No advisories | cargo deny check | 2 | Yes |
| Coverage | cargo llvm-cov over 80% | 2 | Yes |
| Crash consistency | Crash tests | 2 | Yes |
| Mutation score | cargo-mutants critical over 70% killed | 3 | Yes |
| Benchmark stable | YCSB on VM under 5% regression | 3 | Yes |
| Simulation clean | DST framework | 3 | Yes |
| Cross-platform | Linux + macOS + Windows | 4 | Yes |
| CHANGELOG | git-cliff + review | 4 | Yes |
| Docs build | cargo doc --no-deps | 4 | Yes |

### Release Process
1. Release candidate branch: release/v0.1.0 from squad
2. Run all 4 tiers of validation
3. Fix issues, re-run affected tiers
4. Tag: v0.1.0-rc.1
5. Soak: Extended fuzz + simulation
6. Final tag: v0.1.0
7. Publish: cargo publish (dependency order)
8. GitHub Release: changelog, migration guide, known issues

### Automation
- cargo-release — version bumping, tagging, publishing
- git-cliff — CHANGELOG from conventional commits
- cargo-semver-checks — API compatibility
- GitHub Actions release workflow — tag-triggered

---

## Part 3: Implementation Waves

### Wave 1: Foundation (6 agents, parallel)

- [ ] **Gandalf** — Test architecture doc + tiered validation spec (TESTING-ARCHITECTURE.md)
- [ ] **Eowyn** — Deterministic simulation testing framework (faster-sim-test crate)
- [ ] **Galadriel** — Fault injection device + crash consistency tests
- [ ] **Sam** — Memory pressure tests + expand miri to all 47 unsafe files
- [ ] **Boromir** — Fill gaps: compaction + hybrid log integration tests
- [ ] **Aragorn** — Mutation testing integration + property state machine tests

### Wave 2: Expansion (5 agents, parallel)

- [ ] **Elrond** — Dedicated faster-tokio test suite
- [ ] **Saruman** — Device I/O error testing + cross-platform CI
- [ ] **Legolas** — Performance regression detection system (VM benchmark runner)
- [ ] **Eowyn** — DST integration with checkpoint/compaction/recovery (100+ scenarios)
- [ ] **Boromir** — Recovery edge cases + concurrent maintenance tests

### Wave 3: Release Infrastructure (4 agents, parallel)

- [ ] **Arwen** — CHANGELOG, release notes template, documentation audit
- [ ] **Gandalf** — Release automation (cargo-release + semver-checks + CI workflow)
- [ ] **Legolas** — Full benchmark baseline on VM
- [ ] **Boromir** — Release gate validation script (scripts/release-gate)

### Wave 4: Validation and Dry Run (serial)

- [ ] **Aragorn** — Run mutation testing, fix gaps found
- [ ] **Eowyn** — Extended simulation campaign (1000+ scenarios)
- [ ] **All** — Release candidate dry run (full process end-to-end)

---

## Standing Directives

1. Always use claude-opus-4.6 for all sub-agent spawns
2. All agents must use rust/scripts/checkin instead of git commit
3. Benchmarks NEVER run locally — always on VM via sub-agent
4. User name: qbradley
5. Names are LOTR-themed
6. Test economics: Code is cheap, runtime + investigation is expensive. Fast execution + zero false positives.
