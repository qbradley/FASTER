# Release Gate Validation

The `release-gate` script provides a comprehensive, tiered validation pipeline for FASTER Rust releases. It runs everything from fast lint checks through deep mutation testing and release-readiness verification.

## Quick Start

```bash
# Run all 4 tiers (full release validation)
rust/scripts/release-gate

# Run just the fast gate (great for pre-push)
rust/scripts/release-gate --tier 1

# Run tiers 1 and 2 (correctness checks without deep fuzzing)
rust/scripts/release-gate 1 2

# See what would run without executing
rust/scripts/release-gate --dry-run

# Don't stop on first tier failure
rust/scripts/release-gate --continue
```

## The 4 Tiers

### Tier 1: Fast Gate (target: under 60s)

The same checks as `precheckin`, plus documentation build verification. This is the absolute minimum before any commit.

| Check | What it catches |
|-------|----------------|
| `cargo fmt --check` | Formatting violations |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint warnings, common mistakes |
| `cargo test --doc --workspace` | Broken doc examples |
| `cargo nextest run --workspace --all-targets` | Unit + integration test failures (~2,067 tests) |
| `cargo doc --workspace --no-deps` | Broken rustdoc links, doc build errors |

**When to run:** Before every commit. Equivalent to the existing `precheckin` script plus doc build.

### Tier 2: Correctness Gate (target: under 5 minutes)

Specialized correctness tools that go beyond standard testing — concurrency model checking, undefined behavior detection, and fuzz testing.

| Check | What it catches |
|-------|----------------|
| Loom concurrency tests | Data races, deadlocks, ordering bugs (deterministic exploration) |
| Miri UB detection | Undefined behavior, aliasing violations, use-after-free |
| Fuzz smoke (10s/target × 5) | Crash-inducing inputs in parsing, layout, hashing, store ops |
| `cargo deny check` | License violations, known security advisories |
| DST scenarios (faster-dst) | 108 deterministic simulation scenarios across 14 categories |

**When to run:** Before merge to main. Matches what CI runs minus the multi-platform matrix.

**Requirements:** Nightly toolchain (`rustup toolchain install nightly`), Miri component (`rustup +nightly component add miri`), cargo-fuzz (`cargo install cargo-fuzz`), cargo-deny (`cargo install cargo-deny`).

### Tier 3: Deep Validation (target: under 30 minutes)

Extended testing for release candidates — longer fuzz runs, mutation testing, and benchmark regression detection.

| Check | What it catches |
|-------|----------------|
| Mutation testing (cargo-mutants) | Missing test coverage — mutants that survive = untested logic |
| Extended fuzz (5 min/target × 5) | Deeper input space exploration than smoke tests |
| Benchmark comparison | Performance regressions above threshold (requires saved baseline) |

**When to run:** For release candidates. Typically run in CI or on a dedicated VM.

**Requirements:** cargo-mutants (`cargo install cargo-mutants`), saved benchmark baseline (`rust/scripts/bench-baseline.sh save main`).

### Tier 4: Release Gate (target: under 2 hours)

Final release-readiness checks — metadata, packaging, and semver compliance.

| Check | What it catches |
|-------|----------------|
| CHANGELOG.md verification | Missing changelog entry for the current version |
| Cargo.toml metadata audit | Missing description, license, or repository fields |
| `cargo package --list` | Files that won't be included in published crate |
| `cargo semver-checks` | Accidental breaking API changes (informational) |
| Full benchmark suite | Comprehensive performance regression check |

**When to run:** Before cutting a release tag. This is the final go/no-go gate.

**Requirements:** cargo-semver-checks (`cargo install cargo-semver-checks`), saved benchmark baseline.

## Usage Reference

```
Usage: release-gate [OPTIONS] [TIER...]

  TIER: 1, 2, 3, 4, or "all" (default: all)

  Options:
    --tier N        Run only tier N (1-4)
    --from N        Run tiers N through 4
    --dry-run       Show what would run without executing
    --report FILE   Write results to FILE (default: release-gate-report.md)
    --continue      Don't stop on first tier failure
    --verbose       Show full test output
    --help          Show this help
```

### Examples

```bash
# Run tiers 2 through 4 (skip fast gate if you just ran precheckin)
rust/scripts/release-gate --from 2

# Write report to a specific file
rust/scripts/release-gate --report build/validation-report.md

# Run tier 3 with full output for debugging
rust/scripts/release-gate --tier 3 --verbose

# Run all tiers, don't stop on failure, see everything
rust/scripts/release-gate --continue --verbose
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All checks passed |
| 1 | One or more checks failed |
| 2 | Warnings only (no hard failures) |

## Report Output

The script generates a markdown report file (`release-gate-report.md` by default) with:

- Timestamp, commit, and branch metadata
- Overall pass/fail/warn/skip counts
- Per-tier summary with timing
- Detailed table of every check with result and timing

This report can be attached to release PRs or archived with release artifacts.

## How This Fits Into the Release Process

```
Developer workflow:
  precheckin (tier 1) → commit → push

PR merge workflow:
  CI runs tiers 1-2 automatically
  
Release candidate:
  release-gate --from 3  (tier 3 + 4 on dedicated VM)

Release tag:
  release-gate all       (full validation on clean checkout)
  → attach report to release
```

## Interpreting Results

### ✅ Pass
The check completed successfully with no issues.

### ❌ Fail
The check found a real problem that must be fixed before release. The script stops at the first failed tier (unless `--continue` is used).

### ⚠️ Warning
An advisory issue that doesn't block the release but should be reviewed. Examples: missing CHANGELOG entry, cargo semver-checks finding a potential breaking change, packaging warnings.

### ⏭️ Skip
The check was skipped because a required tool is not installed or a prerequisite is missing (e.g., no benchmark baseline). Install the missing tool and re-run.

## Troubleshooting

### "nightly toolchain not installed"
```bash
rustup toolchain install nightly
rustup +nightly component add miri
```

### "cargo-fuzz not found"
```bash
cargo install cargo-fuzz
```

### "cargo-deny not installed"
```bash
cargo install cargo-deny
```

### "cargo-mutants not installed"
```bash
cargo install cargo-mutants
```

### "no baseline in target/criterion/"
```bash
# Save a benchmark baseline first
rust/scripts/bench-baseline.sh save main
```

### Tier 1 fails on formatting
```bash
# Auto-fix formatting, then re-run
cargo fmt --all
rust/scripts/release-gate --tier 1
```

### Loom tests timeout
Loom tests explore all interleavings, which can be slow for complex concurrent code. The CI workflow sets a 10-minute timeout. If tests are timing out locally, consider:
- Running with `LOOM_MAX_PREEMPTIONS=2` to reduce exploration depth
- Isolating the slow test with `-E 'test(specific_test_name)'`

### Miri reports false positives
Miri can flag code patterns that are technically valid. Check the [Miri documentation](https://github.com/rust-lang/miri) and consider adding `#[cfg_attr(miri, ignore)]` for known false positives, with a comment explaining why.

### Fuzz target crashes
```bash
# Reproduce a specific crash
cd rust/fuzz
cargo fuzz run <target> artifacts/<target>/<crash-file>

# Minimize the crash input
rust/scripts/fuzz-minimize <target> <crash-file>
```

## Relationship to Other Scripts

| Script | Purpose | Relationship |
|--------|---------|-------------|
| `precheckin` | Fast lint + test gate | Tier 1 is a superset of precheckin |
| `checkin` | precheckin + git commit | Use checkin for commits, release-gate for validation |
| `fuzz` / `fuzz-ci` | Standalone fuzz runner | Tiers 2 and 3 invoke fuzzing directly |
| `bench-baseline.sh` | Save benchmark baseline | Prerequisite for tier 3/4 benchmark checks |
| `bench-compare.sh` | Regression detection | Invoked by tiers 3 and 4 |
