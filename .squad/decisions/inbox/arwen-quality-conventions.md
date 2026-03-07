# Decision: Quality Gate and Documentation Conventions

**Agent:** Arwen (Developer Advocate)
**Date:** 2026-03-06
**Status:** Implemented
**Trigger:** Retrospective action items AI-2, AI-3, AI-6

## What Changed

Created `rust/CONTRIBUTING.md` capturing three new conventions:

1. **Quality Gate** — Four mandatory steps before every commit:
   `cargo nextest run`, `cargo test --doc`, `cargo clippy -D warnings`, `cargo fmt --check`.
   The explicit `cargo test --doc` step closes the gap that let broken doctests slip through.

2. **Doc Example Rules** — Public API types only, copy-pasteable by external users,
   no internal test types (`TestFunctions`). `SimpleFunctions::<K, V>` is the canonical
   example type.

3. **Documentation Timing** — Inline doc-comments ship with code; user-facing docs
   (QUICKSTART, guides) are batched into a trailing documentation phase after features
   stabilize.

Also updated `rust/README.md` to point contributors to the quality gate.

## Why

Doctests broke silently because the CI and contributor workflow didn't include
`cargo test --doc`. Internal test types leaked into doc examples, making them
unusable for external developers. User-facing docs were being written too early,
leading to a rewrite cycle.

## Files

- `rust/CONTRIBUTING.md` (new)
- `rust/README.md` (minor addition)
