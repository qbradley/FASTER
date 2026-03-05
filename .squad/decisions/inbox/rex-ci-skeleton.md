# Decision: Rust CI Architecture

**Agent:** Rex (QA Engineer)
**Date:** 2026-03-05T19:25:00Z
**Status:** Implemented
**Impact:** All Rust development — CI pipeline for workspace

## Decision

Use GitHub Actions (not Azure Pipelines) for Rust CI, with 5 separate jobs: fmt, ci (3-OS matrix), miri, bench, deny.

## Rationale

1. **GitHub Actions over Azure Pipelines for Rust:** The existing Azure Pipelines are configured for C#/C++ with .NET SDK, MSBuild, CMake, and Azurite dependencies. Rust CI has zero overlap with those toolchains. A separate GitHub Actions workflow keeps concerns isolated and avoids polluting the existing pipeline.

2. **Nightly for formatting only:** `rustfmt.toml` uses `imports_granularity = "Module"` and `group_imports = "StdExternalCrate"`, which are nightly-only. Rather than requiring nightly for all jobs, the `fmt` job uses nightly rustfmt while everything else runs on stable. This balances formatting strictness against toolchain stability.

3. **Formatting as gate:** `fmt` runs first and fast-fails. The `ci` matrix job depends on it (`needs: fmt`). No point compiling on 3 OSes if the code isn't formatted.

4. **Miri as informational:** `continue-on-error: true` on the Miri job. We expect unsafe code to appear as the implementation grows (hash index atomics, record pointer arithmetic). Miri should inform, not block, until the unsafe surface stabilizes.

5. **Path filtering:** All triggers filtered to `rust/**` to avoid firing on C#/C++ changes. The monorepo has three language ecosystems; each should have independent CI.

6. **`Swatinem/rust-cache@v2`:** Standard Rust caching action with per-OS cache keys. Keeps CI fast by caching `~/.cargo` and `rust/target`.

## Implications

- All Rust PRs will be gated on fmt + clippy + test (debug & release) across 3 OSes.
- Miri runs weekly (Sunday 06:00 UTC) and on manual trigger. Team can watch for UB reports.
- Benchmarks run on main-branch pushes only — no PR noise, but provides regression baseline.
- cargo-deny runs on every push/PR for license and advisory audits.
- Future: When benchmark groups are added to `faster-bench`, consider adding `criterion-compare-action` for PR regression comments.
