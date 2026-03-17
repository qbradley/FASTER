# Beyond Testing: Strategic Reliability Methodology for FASTER

**Author:** Gandalf (Lead Architect)  
**Requested by:** qbradley  
**Date:** 2025-07-23  
**Status:** PROPOSAL  

---

## Executive Summary

We have 2,100+ tests across 9 modalities, 28.77 billion ops in stress, and a deterministic simulation framework. That's world-class *testing*. But testing can only show the *presence* of bugs, never their *absence*. To reach zero unsoundness and zero crashes, we need to shift from "find bugs by running code" to "prove bugs cannot exist by construction."

This document analyzes eight approaches beyond testing, maps each to our codebase, and proposes a prioritized roadmap. The three highest-leverage moves are:

1. **Type-state programming** — make invalid states unrepresentable at compile time (high impact, medium effort)
2. **Kani bounded model checking** — prove absence of panics/UB in unsafe-heavy modules (critical impact, medium effort)
3. **Continuous fuzzing in CI** — we have 8 targets sitting idle; wire them up (high impact, small effort)

---

## What We Have Today (Baseline)

| Category | Count | Coverage |
|----------|-------|----------|
| Unit tests | ~1,950 `#[test]` functions | Functional correctness |
| Loom tests | 21 tests | Atomic ordering (Treiber stack, epoch, bucket CAS) |
| MIRI tests | 71 tests | Single-threaded UB in unsafe blocks |
| TSan tests | 3 tests | Data races (compaction only) |
| DST | 133 smoke + 3,009 expanded | Crash recovery, deterministic scheduling |
| Stress tests | 28.77B ops clean | Scale-dependent liveness |
| Fuzz targets | 8 targets | Record parsing, hash, store ops, recovery |
| Property tests | ~42 proptest cases | Round-trip invariants (address, record, hash) |
| Mutation testing | cargo-mutants configured | Test quality verification |
| Static analysis | cargo-deny, cargo-semver-checks, clippy | Deps, API compat, lint |

**Blind spots in the current approach:**
- Fuzzing exists but doesn't run in CI — dead investment
- No formal proof of absence for any invariant
- State machines are runtime-checked enums, not compile-time enforced
- Only 3 TSan tests (compaction path only)
- Mutation testing configured but not in CI
- No benchmark regression detection in CI (benchmarks run but don't fail on regression)

---

## Analysis of Each Approach

### 1. Formal Verification with Kani

**What it is:** AWS's bounded model checker for Rust. Translates Rust to CBMC, exhaustively explores all paths within configurable bounds. Proves *absence* of panics, arithmetic overflows, out-of-bounds access, and UB — not just for tested inputs, but for *all* inputs within bounds.

**Specific applicability to faster-core:**

| Target Module | Unsafe Count | What Kani Proves | Priority |
|---------------|-------------|-------------------|----------|
| `address.rs` (LogicalAddress) | 0 unsafe, but bit-packing | No overflow in page/offset encoding for ALL valid inputs | HIGH — foundational type |
| `record/record_info.rs` (RecordInfo) | 0 unsafe, bit-packing | Field encode/decode roundtrip holds for ALL 2^64 values | HIGH — every record depends on this |
| `hash/bucket.rs` (HashBucket) | 2 unsafe | Entry pack/unpack, tag extraction correct for all inputs | HIGH — hash index correctness |
| `allocator.rs` | 9 unsafe | Page arithmetic never produces invalid addresses | CRITICAL — memory safety |
| `hybrid_log/page.rs` | 7 unsafe | Pin count never wraps, state transitions are monotonic | CRITICAL — eviction safety |
| `epoch/drain.rs` | 3 unsafe | CAS-based linked list never loses nodes | HIGH — memory reclamation |

**What Kani cannot do:**
- Multi-threaded verification (use Loom for that)
- Unbounded loops (must set loop unwind bounds)
- I/O interactions (use DST for that)

**Example harness (address.rs):**
```rust
#[cfg(kani)]
#[kani::proof]
fn verify_logical_address_roundtrip() {
    let page: u64 = kani::any();
    let offset: u32 = kani::any();
    kani::assume(page <= MAX_PAGE);
    kani::assume(offset <= MAX_OFFSET);
    let addr = LogicalAddress::new(page, offset);
    assert_eq!(addr.page(), page);
    assert_eq!(addr.offset(), offset);
}
```

**Effort:** M (Medium) — 2-3 weeks for initial 10 harnesses on high-value targets  
**Impact:** CRITICAL — transforms "we tested it" into "we proved it"  
**ROI:** Extremely high for bit-packing modules. Kani finds the class of bugs where one wrong shift or mask silently corrupts data for rare input combinations. These bugs are invisible to testing because they only trigger on specific bit patterns.

**What the best do:** TigerBeetle uses VOPR (their deterministic simulator) + formal TLA+ specs. FoundationDB used TLA+ to verify their transaction protocol. CockroachDB uses Jepsen. *None* of these Rust-ecosystem tools existed when those projects started — Kani gives us something they didn't have: machine-checked proofs integrated directly into Rust source.

---

### 2. Type-State Programming

**What it is:** Encode state machine transitions into Rust's type system so that invalid transitions are *compile errors*, not runtime panics.

**Current state:** We have beginnings — PinnedPage is a lifetime-scoped RAII guard, RecordAccessor uses PhantomData, Session is `!Send`. But our four major state machines (PageState, CheckpointPhase, Phase, GrowPhase) are all runtime enums with `try_transition()` methods that return `Result` or `bool`.

**Where to apply it:**

#### a) Flush Pipeline (Highest Value)

Current: `PageState` enum with 6 variants, atomic CAS transitions.

Problem: The deadlock post-mortem showed that bugs in state transitions escaped 2,100 tests. Runtime checks caught the bug *eventually* — but a compile-time encoding would prevent the entire class.

Proposed typestate encoding:
```rust
// Each state is a distinct type
struct PageOpen<'log> { frame: &'log PageFrame }
struct PageSealed<'log> { frame: &'log PageFrame }
struct PageFlushing<'log> { frame: &'log PageFrame, io_handle: IoHandle }
struct PageFlushed<'log> { frame: &'log PageFrame }

// Transitions are methods that consume self and return next state
impl<'log> PageOpen<'log> {
    fn seal(self) -> PageSealed<'log> { /* atomic CAS */ }
}
impl<'log> PageSealed<'log> {
    fn begin_flush(self, device: &Device) -> PageFlushing<'log> { ... }
}
impl<'log> PageFlushing<'log> {
    fn complete(self) -> PageFlushed<'log> { ... }
}
```

**Caveat:** Our page states are stored in atomics (`AtomicPageState`) for concurrent access. Pure typestate doesn't work for shared-state concurrency. The practical pattern is a *typestate wrapper at the API boundary* — the internal representation stays atomic, but the public API only exposes transitions through typed handles. The `PinnedPage` pattern already does this for the pin/unpin lifecycle.

**Effort:** L (Large) — 4-6 weeks to redesign flush pipeline API  
**Impact:** HIGH — eliminates an entire class of state-transition bugs at compile time

#### b) Session Lifecycle

```rust
struct SessionBuilder { config: SessionConfig }
struct ActiveSession<'kv> { inner: RawSession<'kv> }
struct CompletedSession { stats: SessionStats }

impl SessionBuilder {
    fn open(self, kv: &FasterKv) -> ActiveSession<'_> { ... }
}
impl<'kv> ActiveSession<'kv> {
    fn upsert(&mut self, ...) -> Status { ... }
    fn complete(self) -> CompletedSession { ... }
}
// Cannot call upsert on CompletedSession — compile error
```

**Effort:** M (Medium) — 2-3 weeks  
**Impact:** MEDIUM — prevents API misuse, improves developer experience

#### c) Checkpoint Protocol

The checkpoint phase machine (Rest → Prepare → InProgress → WaitFlush → WaitCompletion → Completed) could use typestate to ensure sessions can only acknowledge checkpoints they've been notified about.

**Effort:** L (Large)  
**Impact:** MEDIUM — checkpoint bugs are already rare due to DST coverage

---

### 3. Property-Based Testing (Expand proptest)

**What it is:** Generate random operation sequences and verify invariants hold. We have 42 proptest cases but they only cover encoding round-trips. The big gap: no *model-based* testing.

**What model-based testing looks like:**

```rust
// Oracle: a simple HashMap that's trivially correct
// System under test: FasterKv
proptest! {
    #[test]
    fn model_check_crud(ops in vec(crud_operation(), 1..1000)) {
        let mut oracle = HashMap::new();
        let kv = FasterKv::new(test_config());
        let session = kv.start_session();
        
        for op in ops {
            match op {
                Op::Upsert(k, v) => {
                    oracle.insert(k, v);
                    session.upsert(k, v);
                }
                Op::Read(k) => {
                    let expected = oracle.get(&k);
                    let actual = session.read(k);
                    assert_eq!(expected, actual.as_ref());
                }
                Op::Delete(k) => {
                    oracle.remove(&k);
                    session.delete(k);
                }
            }
        }
        // Final consistency check
        for (k, v) in &oracle {
            assert_eq!(session.read(*k), Some(*v));
        }
    }
}
```

**Where to apply:**

| Property | Oracle | Validates |
|----------|--------|-----------|
| CRUD linearizability | HashMap | No data loss through insert/read/delete cycles |
| Checkpoint correctness | Snapshot of HashMap at checkpoint time | Recovery restores exact state |
| Compaction preservation | HashMap minus deleted keys | Compaction never loses live data |
| Grow correctness | HashMap (lookups work through grow) | Hash table resize preserves all entries |

**Effort:** M (Medium) — 2-3 weeks for first 4 properties  
**Impact:** HIGH — model-based testing is the single most effective technique for finding logic bugs in storage engines. FoundationDB credits their simulator's oracle-based checking for finding most of their bugs.

---

### 4. Continuous Fuzzing in CI

**Current state:** 8 fuzz targets exist, scripts exist (`fuzz-ci`, `fuzz-minimize`), but fuzzing does NOT run in CI. This is dead investment.

**What to do:**

1. **PR gate (smoke):** Add `fuzz-ci --smoke` job to `rust-ci.yml` — 10 seconds per target, ~80 seconds total. Catches obvious crashes introduced by the PR.

2. **Nightly (deep):** Add scheduled job running `fuzz-ci` for 60 seconds per target (~8 minutes). Builds corpus over time.

3. **Weekly (extended):** 5 minutes per target (~40 minutes). Finds deeper bugs.

4. **Corpus management:** Commit minimized corpus to `rust/fuzz/corpus/` so coverage accumulates across runs.

5. **OSS-Fuzz integration (stretch):** Google's OSS-Fuzz provides continuous fuzzing for open-source projects with ClusterFuzz infrastructure.

**Missing fuzz targets to add:**

| Target | Attack Surface | Priority |
|--------|---------------|----------|
| `fuzz_concurrent_ops` | Multi-session interleaving | HIGH |
| `fuzz_checkpoint_protocol` | Checkpoint + crash + recover | HIGH |
| `fuzz_grow_operations` | Hash table resize under load | MEDIUM |
| `fuzz_epoch_drain` | Deferred cleanup ordering | MEDIUM |

**Effort:** S (Small) — 1-2 days to wire existing targets into CI  
**Impact:** HIGH — coverage-guided fuzzing finds edge cases that proptest misses because it explores execution paths, not just input space

---

### 5. Benchmarking as Regression Detection

**Current state:** Criterion benchmarks exist (3 suites, 1,747 LOC), baseline scripts exist, benchmarks run in CI on main push. But: **no regression detection gate.** Benchmarks run, save a baseline, and nobody looks unless they manually compare.

**What to do:**

1. **PR benchmark job with threshold:** Run benchmarks on PR, compare against main baseline, fail if any benchmark regresses >5%.

```yaml
bench-regression:
  name: Benchmark Regression Check
  runs-on: ubuntu-latest
  if: github.event_name == 'pull_request'
  steps:
    - name: Run benchmarks
      run: cargo bench --workspace -- --save-baseline pr
    - name: Compare against main
      run: |
        cargo bench --workspace -- --baseline main --load-baseline pr
        # critcmp or custom threshold check
```

2. **Track throughput over time:** Use `github-action-benchmark` to post results to GitHub Pages — visible trend lines.

3. **Nightly extended benchmarks:** YCSB workload with larger datasets (the current benchmarks are micro-benchmarks).

**Effort:** S (Small) — 2-3 days  
**Impact:** MEDIUM — prevents silent performance regressions. Essential for a performance-critical system but doesn't affect correctness.

---

### 6. Mutation Testing in CI

**Current state:** `mutants.toml` is configured, targets 13 high-value source files, excludes FFI and benchmarks. But not in CI.

**What mutation testing tells us:** If `cargo mutants` inserts `return Default::default()` into a function and all tests still pass, we have a testing gap — that function's behavior isn't verified by any test.

**What to do:**

1. **Nightly mutation run:** Too slow for PR gate (~30-60 min), but perfect for nightly.
2. **Track survival rate:** Surviving mutants → filed as testing gaps.
3. **Incremental mutation:** Only test mutants in files changed by PR (requires `cargo-mutants --in-diff`).

**Effort:** S (Small) — 1-2 days to add nightly job  
**Impact:** MEDIUM — meta-testing; verifies test *quality* rather than code *correctness*

---

### 7. Architecture-Level Safety Patterns

Beyond typestate (covered in §2), there are structural patterns that prevent bugs by design:

#### a) Builder Pattern for Configuration

**Problem:** `FasterKvBuilder::new()` with many optional fields — easy to create a misconfigured store.

**Pattern:** Required fields as constructor params, optional fields as builder methods, `build()` validates and returns `Result<FasterKv>`.

**Status:** Already partially implemented. Gap: some configurations that are invalid together (e.g., lossy mode + certain device types) are only caught at runtime.

#### b) Newtype Pattern for Address Types

**Problem:** `LogicalAddress`, `PhysicalAddress`, page indices, and byte offsets are all `u64`. Easy to pass the wrong one.

**Status:** `LogicalAddress` is already a newtype. Verify that all address arithmetic uses typed operations, not raw `u64` math.

#### c) "No O(n) Under Lock" Architectural Rule

From the deadlock post-mortem: Bug 2 was caused by `fill(0)` taking O(total_bytes_flushed) while holding a lock.

**Pattern:** Enforce at code review: any code holding a lock must be O(1). Add `#[must_not_block]` annotation (custom attribute + clippy lint or code review checklist).

#### d) Sealed Trait for Extension Points

The `Functions` trait and `Device` trait are extension points. Seal them to prevent external implementations from violating internal invariants.

**Effort:** S-M (varies by pattern)  
**Impact:** MEDIUM — defense in depth

---

### 8. Static Analysis Beyond Clippy

**Current state:**
- ✅ cargo-deny (licenses + advisories) — in CI
- ✅ cargo-semver-checks — in CI (informational)
- ✅ clippy with -D warnings — in CI
- ❌ cargo-audit — not needed (cargo-deny covers advisories)
- ❌ cargo-geiger (unsafe code counter) — not integrated
- ❌ MIRAI (abstract interpretation) — not explored

**What to add:**

| Tool | What It Does | Effort | Impact |
|------|-------------|--------|--------|
| `cargo-geiger` | Counts unsafe usage, tracks over time | S | LOW — awareness, not prevention |
| MIRAI | Abstract interpretation, finds broader UB classes than Miri | L | HIGH — but experimental and slow |
| `cargo-vet` | Supply chain audit (Mozilla's tool) | S | LOW — cargo-deny covers most of this |

**Recommendation:** Our static analysis stack is already good. The marginal return of adding more tools is low compared to Kani and typestate.

---

## What Best-in-Class Systems Databases Do

| Practice | TigerBeetle | FoundationDB | CockroachDB | FASTER (us) |
|----------|-------------|--------------|-------------|-------------|
| **Deterministic simulation** | VOPR (Zig) | Sim (C++) — credited with finding most bugs | — | ✅ faster-dst (7 phases) |
| **Formal specification** | TLA+ for protocol | TLA+ for transactions | — | ❌ Gap |
| **Model checking** | — | — | — | ❌ Gap (Kani would fill) |
| **Property-based/Oracle testing** | Fuzzy testing with oracle | Sim includes oracle | Metamorphic testing | ⚠️ Partial (42 proptest, no oracle) |
| **Continuous fuzzing** | ✅ In CI | ✅ | ✅ OSS-Fuzz | ❌ 8 targets but not in CI |
| **Typestate/compile-time safety** | Zig comptime constraints | N/A (C++) | N/A (Go) | ⚠️ Partial (PinnedPage only) |
| **Benchmark regression gates** | ✅ Automated | — | ✅ roachtest perf | ❌ Benchmarks run but don't gate |
| **Mutation testing** | — | — | — | ⚠️ Configured, not in CI |
| **Supply chain audit** | Minimal deps (Zig) | C++ (manual) | Go (govulncheck) | ✅ cargo-deny |
| **Memory safety by construction** | Zig (no hidden allocations) | C++ (careful manual management) | Go (GC) | ✅ Rust ownership + 315 audited unsafe |

**Key insight:** TigerBeetle and FoundationDB's competitive advantage is deterministic simulation + formal specification. We already have simulation. The gap is formal methods (Kani/TLA+) and oracle-based testing.

---

## Prioritized Roadmap

### Phase 1: Quick Wins (1-2 weeks)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 1 | Wire fuzz targets into CI (`rust-ci.yml` smoke + nightly) | S | HIGH | Sam |
| 2 | Add mutation testing nightly job | S | MEDIUM | Sam |
| 3 | Add benchmark regression gate to PR workflow | S | MEDIUM | Legolas |

**Why first:** These are existing investments sitting idle. Zero new code — just CI configuration. Immediate ROI.

### Phase 2: Formal Foundations (3-6 weeks)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 4 | Kani harnesses for address.rs, record_info.rs, bucket.rs | M | CRITICAL | Gandalf + Eowyn |
| 5 | Model-based proptest (HashMap oracle for CRUD) | M | HIGH | Aragorn |
| 6 | Kani harnesses for allocator.rs, page.rs (unsafe-heavy) | M | CRITICAL | Gandalf + Eowyn |

**Why second:** Kani transforms our confidence from "we tested many cases" to "we proved all cases within bounds." Model-based proptest catches logic bugs that unit tests structurally miss.

### Phase 3: Compile-Time Safety (4-8 weeks)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 7 | Typestate for session lifecycle (SessionBuilder → Active → Completed) | M | MEDIUM | Boromir |
| 8 | Typestate wrapper for flush pipeline (PageOpen → Sealed → Flushing → Flushed) | L | HIGH | Gandalf |
| 9 | Builder pattern hardening for FasterKvBuilder | S | MEDIUM | Sam |

**Why third:** These are refactoring efforts that improve long-term maintainability and prevent API misuse. They don't find existing bugs — they prevent future ones.

### Phase 4: World-Class (ongoing)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 10 | TLA+ specification of checkpoint/grow protocol | XL | HIGH | Gandalf |
| 11 | OSS-Fuzz integration | M | MEDIUM | Sam |
| 12 | Expand Kani to cover all 315 unsafe blocks | XL | CRITICAL | Team |
| 13 | MIRAI abstract interpretation exploration | L | UNKNOWN | Research |

---

## Effort/Impact Matrix

```
                    LOW EFFORT          MEDIUM EFFORT         HIGH EFFORT
                ┌─────────────────┬─────────────────────┬──────────────────┐
   CRITICAL     │                 │ [4] Kani (core)     │ [12] Kani (all   │
   IMPACT       │                 │ [6] Kani (unsafe)   │      unsafe)     │
                │                 │                     │                  │
   ─────────────┼─────────────────┼─────────────────────┼──────────────────┤
   HIGH         │ [1] Fuzz in CI  │ [5] Model proptest  │ [8] Flush        │
   IMPACT       │                 │                     │     typestate    │
                │                 │                     │ [10] TLA+        │
   ─────────────┼─────────────────┼─────────────────────┼──────────────────┤
   MEDIUM       │ [2] Mutations   │ [7] Session         │                  │
   IMPACT       │    in CI        │    typestate        │                  │
                │ [3] Bench gate  │ [9] Builder harden  │                  │
                │                 │ [11] OSS-Fuzz       │                  │
   ─────────────┼─────────────────┼─────────────────────┼──────────────────┤
   LOW          │ cargo-geiger    │                     │ [13] MIRAI       │
   IMPACT       │                 │                     │                  │
                └─────────────────┴─────────────────────┴──────────────────┘
```

---

## The Philosophy

Testing asks: *"Does this work for the inputs I tried?"*  
Formal verification asks: *"Can this possibly fail for any input?"*  
Type-state programming asks: *"Can the compiler even express the wrong state?"*  

The path to zero unsoundness runs through all three. Testing finds bugs. Verification proves their absence. Type systems make them inexpressible. Our roadmap layers these approaches so each compensates for the others' blind spots:

- **Kani** covers the 315 unsafe blocks where Rust's type system can't help
- **Typestate** covers the state machines where runtime checks are currently our only defense  
- **Model-based proptest** covers the semantic invariants ("never lose data") that neither Kani nor types can express
- **Continuous fuzzing** covers the edge cases that structured approaches miss through raw exploration

Together, they form a verification stack that no single technique — including more testing — can match.

---

## Decision Required

**Proposal:** Adopt the 4-phase roadmap above, starting with Phase 1 (CI wiring) immediately.

**Alternatives considered:**
- A: Stay testing-only — rejected; diminishing returns, can't prove absence
- B: Go all-in on formal verification — rejected; too expensive without quick wins first  
- C: Proposed roadmap — quick wins → formal foundations → compile-time → world-class

**Vote:** @qbradley @team
