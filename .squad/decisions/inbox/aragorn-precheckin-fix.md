# Decision: Precheckin Nextest Must Not Use --all-targets

**Author:** Aragorn  
**Date:** 2026-03-09  
**Status:** Implemented (commit 74ce041f)

## Context

The precheckin script (`rust/scripts/precheckin`) used `cargo nextest run --workspace --all-targets`, which includes Criterion bench binaries. Criterion's custom `main()` does not understand nextest's `--list --format terse` flag and enters an infinite benchmark loop, blocking all precheckin runs indefinitely.

## Decision

Remove `--all-targets` from the nextest invocation in precheckin. The clippy gate already runs with `--all-targets`, which verifies bench code compiles. Nextest only needs to run `--workspace` (lib + bin + test targets).

## Consequences

- Precheckin now completes in ~45 seconds instead of hanging forever.
- Bench code is still compile-checked by clippy.
- If a future Criterion version supports `--list`, we can re-add `--all-targets`.

## Related

- Root cause of formatting drift: nightly-only `imports_granularity`/`group_imports` in rustfmt.toml silently degrade on stable toolchain. All agents should use nightly rustfmt or the same toolchain version.
