# Mutation Testing Plan for FASTER Rust

**Author:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** Proposed
**Scope:** All 7 crates in the FASTER Rust workspace

---

## 1. Technology Recommendation

### Recommendation: `cargo-mutants` (v27.0.0)

#### Alternatives Evaluated

| Tool | Status | Verdict |
|------|--------|---------|
| **cargo-mutants** | Actively maintained, stable Rust, no source annotations | ✅ **Selected** |
| **mutagen** | Unmaintained (3+ years), requires nightly, requires `#[mutate]` annotations on every function | ❌ Rejected |
| **Custom approach** (proc-macro or source rewriting) | High engineering cost, no community support | ❌ Rejected |

#### Why `cargo-mutants`

1. **Zero source modification** — mutates temporary copies, never touches our code
2. **Works with stable Rust** — no nightly dependency, compatible with our edition 2024 / MSRV 1.85.0
3. **Workspace-aware** — supports `--package` filtering and `--file` globs for targeted runs
4. **Good filtering** — `--exclude`, `--exclude-re`, `--file`, `--re`, `--skip-calls` for precision
5. **CI-ready** — `--shard` support for parallelizing across CI workers
6. **Active community** — used by rust-bitcoin and other security-sensitive projects
7. **Configurable** via `.cargo/mutants.toml` for persistent project settings

#### Limitations (Important for FASTER)

| Limitation | Impact | Mitigation |
|-----------|--------|------------|
| **Cannot reason about unsafe semantics** | Mutating arithmetic/bitwise ops inside `unsafe` blocks can create UB (e.g., wrong pointer offset = segfault, not test failure) | These show up as **timeouts**, not missed mutants. We saw 8 timeouts in address.rs alone. Filter with `--timeout` and accept timeouts as "tested" |
| **Atomic ordering mutations** | cargo-mutants doesn't mutate `Ordering` enum values (Relaxed→SeqCst etc.) | Atomic correctness must be verified separately (Miri, loom, manual review) |
| **Equivalent mutants** | Some mutations don't change observable behavior (e.g., replacing `Display::fmt` with default) | Manual triage required; typically 10-15% of survivors |
| **No `#[test]` code mutation** | Only mutates production code (correct behavior) | N/A — this is what we want |
| **Build time dominance** | Each mutant requires an incremental rebuild (~8-50s observed) | Use `--jobs` parallelism and targeted `--file` runs |

---

## 2. Execution Plan

### 2.1 Codebase Profile (Measured)

| Metric | Value |
|--------|-------|
| Total Rust source files | 116 |
| Total lines of code | 68,539 |
| Total tests | ~1,456 (1,235 in faster-core alone) |
| Test suite runtime | ~50s (full workspace, single-threaded) |
| Unsafe usages (non-test) | ~610 occurrences |
| Total mutants (workspace) | **4,454** |
| Build time per mutant | 8–50s (incremental) |
| Test time per mutant | 50–265s (timeout ceiling) |

#### Mutants by Crate

| Crate | Mutants | % of Total |
|-------|---------|-----------|
| faster-core | 2,598 | 58.3% |
| samples (7 crates) | 1,147 | 25.8% |
| faster-uring | 305 | 6.8% |
| faster-dst | 171 | 3.8% |
| faster-tokio | 170 | 3.8% |
| faster-ffi | 63 | 1.4% |

#### Mutants by faster-core Module (Risk-Ordered)

| Module | Mutants | Risk Level | Priority |
|--------|---------|-----------|----------|
| store/ | 416 | 🔴 Critical — core KV operations | P0 |
| compaction/ | 200 | 🔴 Critical — data loss risk | P0 |
| hash/ | 250 | 🔴 Critical — correctness of index | P0 |
| hybrid_log/ | 369 | 🔴 Critical — memory/disk boundary | P1 |
| epoch/ | 72 | 🟠 High — concurrency correctness | P1 |
| record/ | 255 | 🟠 High — data layout correctness | P1 |
| checkpoint/ | 313 | 🟡 Medium — durability | P2 |
| recovery/ | 142 | 🟡 Medium — disaster recovery | P2 |
| grow/ | 138 | 🟡 Medium — online resize | P2 |
| state/ | 60 | 🟢 Low — state machine | P3 |
| address.rs | 53 | 🟢 Low — well-tested foundation | P3 |
| error.rs | 9 | 🟢 Low — error plumbing | P3 |

### 2.2 Phased Execution

#### Phase 1: Foundation & Calibration (Day 1–2)

**Goal:** Establish baseline, tune configuration, validate approach.

```bash
# Run on small, well-tested modules to calibrate
cargo mutants --package faster-core \
  --file "crates/faster-core/src/error.rs" \
  --file "crates/faster-core/src/status.rs" \
  --file "crates/faster-core/src/address.rs" \
  --file "crates/faster-core/src/state/*.rs" \
  --jobs 4
```

- **Expected mutants:** ~145
- **Expected runtime:** ~1.5 hours (based on 53 mutants / 20 min extrapolation)
- **Purpose:** Validate tooling, establish baseline kill rate, identify configuration needs

**Deliverable:** `.cargo/mutants.toml` configuration file with timeout/exclusion settings.

#### Phase 2: Critical Path — Core Operations (Day 3–5)

**Goal:** Test the riskiest code first.

```bash
# P0: store + compaction + hash (866 mutants)
cargo mutants --package faster-core \
  --file "crates/faster-core/src/store/*.rs" \
  --file "crates/faster-core/src/compaction/*.rs" \
  --file "crates/faster-core/src/hash/*.rs" \
  --jobs 4 --timeout-multiplier 3
```

- **Expected mutants:** 866
- **Expected runtime:** ~5–8 hours
- **After run:** Triage survivors → write targeted tests → re-run survivors only

#### Phase 3: Memory & Concurrency Layer (Day 6–8)

```bash
# P1: hybrid_log + epoch + record (696 mutants)
cargo mutants --package faster-core \
  --file "crates/faster-core/src/hybrid_log/*.rs" \
  --file "crates/faster-core/src/epoch/*.rs" \
  --file "crates/faster-core/src/record/*.rs" \
  --jobs 4 --timeout-multiplier 3
```

- **Expected mutants:** 696
- **Expected runtime:** ~4–6 hours

#### Phase 4: Durability & Resize (Day 9–10)

```bash
# P2: checkpoint + recovery + grow (593 mutants)
cargo mutants --package faster-core \
  --file "crates/faster-core/src/checkpoint/*.rs" \
  --file "crates/faster-core/src/recovery/*.rs" \
  --file "crates/faster-core/src/grow/*.rs" \
  --jobs 4
```

- **Expected mutants:** 593
- **Expected runtime:** ~3–5 hours

#### Phase 5: Peripheral Crates (Day 11–12)

```bash
# Non-core crates (709 mutants, excluding samples)
cargo mutants --package faster-uring --package faster-tokio \
  --package faster-dst --package faster-ffi \
  --jobs 4
```

- **Expected mutants:** 709
- **Expected runtime:** ~3–5 hours
- **Note:** Skip `samples/` — these are demo code, not production

#### Phase 6: Full Workspace Validation (Day 13–14)

```bash
# Final full run to confirm overall kill rate
cargo mutants --package faster-core --package faster-uring \
  --package faster-tokio --package faster-dst --package faster-ffi \
  --jobs 4 --exclude "samples/*"
```

### 2.3 Runtime Estimates

| Scenario | Mutants | Est. Time (--jobs 4) |
|----------|---------|---------------------|
| Single file (error.rs) | 9 | ~3 min |
| Single file (address.rs) | 53 | ~20 min |
| Phase 1 (calibration) | 145 | ~1.5 hours |
| Phase 2 (P0 critical) | 866 | ~5–8 hours |
| Full faster-core | 2,598 | ~15–25 hours |
| Full workspace (excl. samples) | 3,307 | ~20–30 hours |

**Key insight from real data:** With `--jobs 4` on a 20-core machine, each mutant takes ~20–25 seconds wall-clock average (build + test amortized). Higher parallelism (`--jobs 8`) would roughly halve these times but requires monitoring memory usage (~2GB per cargo invocation).

### 2.4 Parallelism Recommendations

| Machine | `--jobs` | Rationale |
|---------|----------|-----------|
| Dev machine (20 cores, 32GB) | 4–6 | Each parallel build uses 3-4 cores via cargo internals |
| CI (8 cores, 16GB) | 2–3 | Memory-constrained; use sharding instead |
| CI with sharding | 2 × 4 workers | `--shard 1/4`, `--shard 2/4`, etc. across workers |

### 2.5 Handling Unsafe Code Mutations

**Policy: Treat timeouts from unsafe mutations as "tested".**

Rationale: When cargo-mutants mutates arithmetic inside `unsafe` blocks (e.g., replacing `>>` with `<<` in pointer arithmetic), the result is typically undefined behavior that manifests as:
- Segfault → test timeout (counted as timeout, not missed)
- Infinite loop → test timeout
- Memory corruption → unpredictable failure

These are **not** equivalent mutants and **not** test gaps — they're mutations that produce UB, which Rust's test harness can't catch cleanly. Our observed data confirms this: address.rs had 8 timeouts, all from bitwise/arithmetic mutations in address encoding logic.

**Configuration to handle this:**

```toml
# .cargo/mutants.toml
timeout_multiplier = 3.0
build_timeout_multiplier = 2.0

# Skip sample crates
exclude_globs = ["crates/samples/*"]

# Skip functions that are pure logging/display
skip_calls = ["debug", "trace", "info", "warn", "error", "write!", "writeln!"]
```

---

## 3. Operational Playbook

### 3.1 Iteration Process

```
┌─────────────────────────────────────────┐
│  1. Run cargo-mutants on target module  │
│     (--file or --package scoped)        │
├─────────────────────────────────────────┤
│  2. Analyze results:                    │
│     - caught.txt → ✅ tests working     │
│     - missed.txt → ⚠️ test gaps        │
│     - unviable.txt → skip (won't build)│
│     - timeout.txt → review case-by-case│
├─────────────────────────────────────────┤
│  3. For each missed mutant:             │
│     a. Is it equivalent? (mark skip)    │
│     b. Is it a real gap? (write test)   │
│     c. Is it in unsafe? (review safety) │
├─────────────────────────────────────────┤
│  4. Re-run ONLY survivors:              │
│     cargo mutants --re "pattern"        │
├─────────────────────────────────────────┤
│  5. Track kill rate improvement          │
│     → Repeat until target met            │
└─────────────────────────────────────────┘
```

### 3.2 Stopping Criteria

| Metric | Target | Rationale |
|--------|--------|-----------|
| **Overall kill rate** | ≥ 80% | Industry standard for mature projects; 90%+ is diminishing returns |
| **Critical module kill rate** (store, compaction, hash) | ≥ 85% | Higher bar for data-integrity code |
| **Equivalent mutant ratio** | Document, don't chase | If 15% of survivors are equivalent, actual kill rate is higher |
| **Timeout ratio** | ≤ 10% of total | Timeouts above this suggest test suite is too slow for mutation testing |

**When to stop iterating on a module:**
1. Kill rate ≥ target AND
2. All "missed" mutants reviewed and either (a) test written or (b) classified as equivalent/acceptable
3. No missed mutants in safety-critical paths (unsafe blocks, atomic operations, error handling)

**Why not 90%+?** For a systems crate with significant unsafe code:
- Many unviable mutants inflate the denominator
- Equivalent mutants in display/formatting code are low-value
- Diminishing returns: going from 80% → 90% costs 3–5× the effort of 70% → 80%
- Our goal is pre-release confidence, not academic perfection

### 3.3 Classifying Surviving Mutants

| Category | Example | Action |
|----------|---------|--------|
| **Real test gap** | `replace store.read() -> Status with NotFound` survives | Write a test that asserts on `Status::Ok` after successful read |
| **Equivalent mutant** | `replace Display::fmt with Ok(())` — no test checks error messages | Mark as acceptable; optionally add a display test |
| **Unsafe mutation → UB** | `replace >> with << in pointer arithmetic` → timeout | Classify as "tested by timeout"; add `#[mutants::skip]` if noisy |
| **Dead code** | Mutation in unreachable branch survives | Either remove the dead code or add a test proving it's reachable |
| **Performance-only** | `replace capacity * 2 with capacity * 1` — works but slower | Low priority; note for optimization testing |

### 3.4 False Positive Management

**Common false positive patterns in FASTER:**

1. **Logging mutations** — replacing `log::debug!()` calls with nothing always survives (no test asserts on log output)
   - **Fix:** Add `skip_calls = ["debug", "trace", "info", "warn", "error"]` to config

2. **Drop impl mutations** — `replace Drop::drop with ()` often survives if cleanup is best-effort
   - **Fix:** Case-by-case; resource leak tests for critical Drop impls

3. **Capacity/hint mutations** — `replace with_capacity(n) with with_capacity(0)` — functionally equivalent
   - **Fix:** Already handled by `--skip-calls-defaults` (includes `with_capacity`)

4. **Display/Debug mutations** — formatting changes rarely caught
   - **Fix:** Accept these; low-risk

### 3.5 Tracking Progress

Track per-module results in a simple table:

```
| Module      | Run Date   | Total | Caught | Missed | Unviable | Timeout | Kill Rate |
|-------------|-----------|-------|--------|--------|----------|---------|-----------|
| error.rs    | 2026-03-06 | 9     | 4      | 0      | 5        | 0       | 100%*     |
| address.rs  | 2026-03-06 | 53    | 37     | 0      | 8        | 8       | 100%*     |
| store/      | TBD        | 416   | ?      | ?      | ?        | ?       | ?         |
```

\* Kill rate = caught / (caught + missed). Unviable and timeouts excluded from denominator.

---

## 4. CI Integration

### 4.1 Recommended Cadence

| When | What | Why |
|------|------|-----|
| **Pre-release** (now) | Full mutation testing campaign, all phases | Establish baseline, fix critical gaps |
| **Weekly (post-release)** | Incremental: `--in-diff` on changed files since last run | Catch regressions in test quality |
| **Nightly** | NOT recommended initially | Too expensive; revisit after baseline established |
| **Per-PR** | `--in-diff <pr.diff>` on changed files only | Lightweight; ~5-20 mutants per typical PR |

### 4.2 CI Configuration

```yaml
# .github/workflows/mutation-testing.yml (weekly)
name: Mutation Testing
on:
  schedule:
    - cron: '0 2 * * 0'  # Weekly, Sunday 2 AM
  workflow_dispatch: {}

jobs:
  mutants:
    runs-on: ubuntu-latest-16core  # Need beefy runner
    strategy:
      matrix:
        shard: ["1/4", "2/4", "3/4", "4/4"]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install cargo-mutants
      - run: |
          cargo mutants \
            --package faster-core \
            --package faster-uring \
            --package faster-tokio \
            --package faster-dst \
            --package faster-ffi \
            --shard ${{ matrix.shard }} \
            --jobs 2 \
            --timeout-multiplier 3
      - uses: actions/upload-artifact@v4
        with:
          name: mutants-${{ matrix.shard }}
          path: mutants.out/
```

### 4.3 Per-PR Integration (Lightweight)

```yaml
# Add to existing CI as optional job
mutation-check:
  runs-on: ubuntu-latest
  if: github.event_name == 'pull_request'
  steps:
    - uses: actions/checkout@v4
      with:
        fetch-depth: 0
    - run: |
        git diff origin/main...HEAD > pr.diff
        cargo mutants --in-diff pr.diff --jobs 2 --timeout-multiplier 3
```

### 4.4 Preventing Maintenance Burden

1. **Don't gate PRs on mutation testing** — use it as advisory, not blocking
2. **Review survivors weekly**, not per-commit
3. **Maintain `.cargo/mutants.toml`** with exclusions for known equivalent mutants
4. **Archive results** — keep mutants.out/ artifacts for trend analysis
5. **Set a time budget** — if full-workspace run exceeds 2 hours in CI, shard more aggressively or reduce scope to changed crates only

---

## 5. Estimated Timeline

### Pre-Release Mutation Testing Campaign

| Week | Activity | Effort |
|------|----------|--------|
| **Week 1** | Phase 1–2: Calibration + critical path (store, compaction, hash) | 2 days running + 2 days writing tests |
| **Week 2** | Phase 3–4: Memory/concurrency + durability (hybrid_log, epoch, record, checkpoint, recovery, grow) | 2 days running + 2 days writing tests |
| **Week 3** | Phase 5: Peripheral crates + Phase 6: full validation | 1 day running + 1 day writing tests |
| **Week 3–4** | Second pass: re-run all modules with new tests, verify kill rate targets met | 1–2 days |

**Total estimated effort:** 2–3 weeks of focused work (not full-time — mutation runs happen in background).

**Total compute time:** ~30–50 hours of CPU time across all runs (including re-runs after writing tests).

### Post-Campaign

- Weekly CI job: ~2–4 hours per run (sharded across 4 workers = ~30–60 min wall-clock)
- Per-PR checks: ~5–15 minutes per PR (only mutating changed files)

---

## Appendix A: Experimental Results

### Run 1: error.rs (9 mutants, --jobs 4)

```
Total: 9 | Caught: 4 | Missed: 0 | Unviable: 5 | Timeout: 0
Kill rate: 100% (4/4 viable mutants caught)
Wall-clock: 2 min 37 sec
```

**Insight:** 5 of 9 mutants were unviable (From impl replacements with Default::default() don't compile). This is expected for trait impls.

### Run 2: address.rs (53 mutants, --jobs 4)

```
Total: 53 | Caught: 37 | Missed: 0 | Unviable: 8 | Timeout: 8
Kill rate: 100% (37/37 viable, non-timeout mutants caught)
Wall-clock: 20 min 22 sec
```

**Insight:** All 8 timeouts were bitwise/arithmetic mutations in address encoding — these create invalid addresses that cause infinite loops or segfaults in downstream code. This is expected and acceptable behavior. Zero missed mutants — address.rs is exceptionally well-tested.

### Extrapolation

- ~23 seconds wall-clock per mutant (with --jobs 4 on 20-core machine)
- ~55% of mutants are "caught" (real kills)
- ~15% are "unviable" (won't compile)
- ~15% are "timeout" (mostly unsafe/bitwise mutations)
- ~15% expected "missed" in less-tested modules (our target to reduce)

---

## Appendix B: Recommended `.cargo/mutants.toml`

```toml
# FASTER Rust mutation testing configuration

# Timeout settings (based on 50s baseline test time)
timeout_multiplier = 3.0
build_timeout_multiplier = 2.0

# Exclude non-production code
exclude_globs = [
    "crates/samples/*",
    "crates/faster-bench/*",
]

# Skip mutations in logging calls (never tested, low risk)
skip_calls = [
    "debug",
    "trace",
    "info",
    "warn",
    "error",
]

# Use default skip list (includes with_capacity)
skip_calls_defaults = true

# Cap lints to prevent denied warnings from making mutants unviable
cap_lints = true
```
