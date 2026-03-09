# Decision: Release Gate Failure Diagnostics & Advisory Checks

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-09
**Commit:** 5c21cf94
**Branch:** squad

## Summary

Two changes to `rust/scripts/release-gate`:

1. **Always show failure diagnostics.** When any check fails (or warns), the script now immediately prints the exact command, last ~15 lines of output, and a "To reproduce:" line. Previously this was gated behind `--verbose`, which meant users saw `❌ Loom concurrency tests (14s)` with zero guidance on what went wrong. The markdown report detail column also includes the repro command.

2. **Loom and Miri checks are now advisory (warnings, not hard failures).** Loom support is aspirational — there are no `cfg(loom)` type aliases and zero loom-specific tests, so compiling with `--cfg loom` produces 56 type errors. Miri similarly has no `test(miri)` annotated tests. Both use `run_check_warn` with comments explaining when to promote back to `run_check`.

## Team Impact

- **Aragorn:** When you implement a loom compatibility layer (cfg(loom) type aliases swapping std→loom types), tell Boromir — the loom check should be promoted back to `run_check` at that point.
- **All agents:** If your tier 2+ check fails, you'll now see exactly what command to run and the actual error output. No more guessing.
- **Gandalf/CI:** The gate exit code changes: loom/miri warnings produce exit 2 (warnings) instead of exit 1 (failure). CI pipelines gating on exit code should treat exit 2 as acceptable for now.
