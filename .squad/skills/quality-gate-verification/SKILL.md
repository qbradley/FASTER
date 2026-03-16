# Skill: Quality Gate Verification

## When to Use

- Before every merge to integration branch
- Before every release tag
- After resolving merge conflicts
- When validating a PR ready for review

## Pattern

### 4-Tier Validation Model

Defined in `rust/TESTING-ARCHITECTURE.md`. Use appropriate tier for context:

**Tier 1: Fast Gate (<60s)**
```bash
cd rust
cargo fmt --check
cargo clippy -- -D warnings
cargo nextest run
```
**When:** Every commit before push, every merge conflict resolution.

**Tier 2: Correctness Gate (<5 min)**
```bash
cd rust
# Tier 1 +
cargo nextest run --all-features
cargo miri test --package faster-core --lib  # Subset, not full
```
**When:** Every PR, before requesting review.

**Tier 3: Deep Validation (<30 min)**
```bash
cd rust
./scripts/release-gate deep
# Includes: full Miri, Loom subset, DST smoke, benchmark baseline check
```
**When:** Nightly CI, before major integration merges.

**Tier 4: Release Gate (<2 hr)**
```bash
cd rust
./scripts/release-gate all --report
# Includes: fuzz (short), crash consistency, full mutation testing
```
**When:** Pre-release only, creates release artifact.

### Common Pitfalls

- ❌ **Don't:** `cargo test --all-targets` (includes benches, slow)
- ✅ **Do:** `cargo nextest run` (unit + integration only)

- ❌ **Don't:** Run full Miri on every PR (20+ min)
- ✅ **Do:** Run Miri subset on PR, full Miri in nightly

- ❌ **Don't:** Assume `cargo build` passing means tests pass
- ✅ **Do:** Always verify `cargo nextest run` after merge

### Failure Triage

1. **fmt failure:** `cargo fmt` then re-run
2. **clippy failure:** Fix warning, never suppress without architectural review
3. **test failure:** Isolate with `cargo nextest run --package <pkg> --test <test>`
4. **Miri failure:** Indicates UB, always investigate before proceeding

## Confidence: high

## Learned From

- TESTING-ARCHITECTURE.md (Wave 3, authoritative testing strategy)
- "Always verify cargo nextest run passes after merge, not just cargo build"
- Post-merge validation pattern (Iteration 6)
- Quality gates from charter: "cargo fmt --check, cargo clippy -- -D warnings, cargo nextest run"
