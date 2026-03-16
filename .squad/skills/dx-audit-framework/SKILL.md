---
name: "dx-audit-framework"
description: "Framework for evaluating and improving the developer experience of an API"
domain: "documentation"
confidence: "medium"
source: "2026-03-06 charter (Arwen role)"
---

## When to Use

When planning an API review, evaluating a new feature for user impact, or investigating why adoption is slow. DX audit goes beyond code review—it evaluates discoverability, error messages, documentation, and first-time user experience.

## Pattern

### The First 5 Minutes Test

Try the API as a new user would:

1. Clone or check out the project
2. Read the top-level README
3. Follow the quickstart without reading source code
4. Run a basic example
5. Read error messages when something goes wrong

At each step, ask:
- **Clarity:** Do I know what to do next?
- **Confidence:** Am I on the right track, or am I guessing?
- **Friction:** How many times did I search for answers?

If you had to read source code to understand how to call an API, that's a DX failure.

### API Discoverability Checklist

- [ ] Public API appears in `cargo doc --open` (no `#[doc(hidden)]` on core APIs)
- [ ] Function/type names are self-documenting (avoid single-letter generic names in public signatures)
- [ ] Method names follow conventions (getters don't prefix with `get_`, mutators use `set_`, predicates use `is_`/`has_`)
- [ ] Related functions are grouped (e.g., all iterator methods near each other)
- [ ] Traits are composable (users can implement their own without reading source)

### Error Message Quality Audit

Walk through error paths:

- [ ] Error messages are human-readable (not debug dumps)
- [ ] Error includes the problem and (ideally) the fix
- [ ] Related errors are grouped (don't spam with 10 identical errors)
- [ ] Panics have clear messages (or don't panic—return Result instead)

Example (bad):
```
thread 'main' panicked at 'index out of bounds: x >= len()'
```

Example (good):
```
Error: Failed to recover checkpoint: corrupted metadata at page 1023
Suggestion: Verify storage device is not full. Check logs for I/O errors.
```

### Documentation Structure Audit

- [ ] README answers: "What is this? Why would I use it? Can I run a working example in 2 minutes?"
- [ ] QUICKSTART or tutorial: "How do I build my first app?"
- [ ] API docs: "How do I use each public type?"
- [ ] Samples/examples: "How do I integrate this into a real system?"
- [ ] Architecture guide: "How does this work internally?" (for power users)

### Sample/Workload Representation

- [ ] Samples cover common use cases (not just "hello world")
- [ ] Samples are real, tested code (not pseudocode)
- [ ] Are there both sync and async examples (if applicable)?
- [ ] Do samples show error handling or just the happy path?
- [ ] Can a new user copy-paste a sample and extend it?

### Adoption Friction Checklist

- [ ] Zero dependencies on major cloud SDKs (users choose their own integrations)
- [ ] Clear guidance on "should I use the sync or async API?"
- [ ] Cargo features are documented (what happens if I enable `feature-x`?)
- [ ] Migration path is clear (old API → new API, with examples)
- [ ] Troubleshooting section explains common "it's not working" scenarios

## Examples of DX Friction Found

| Issue | Impact | Fix |
|-------|--------|-----|
| Sparse `lib.rs` docs | Users don't know what a crate does | Add 3–4 examples to the module-level `//!` doc |
| Error: `PoisonedLock` with no context | Users panic; unclear recovery path | Change error message: "Lock was poisoned (recovery mechanism active). Retry operation." |
| Sample missing CLI flag docs | Users can't explore the behavior | Add table of flags with examples and default values |
| No async/sync bridge example | Users confused about which API to use | Create sample showing both APIs, when to choose each |

## Writing This Audit

Structure the audit report as:

```markdown
# Developer Experience Audit — [Date]

## Rating: X/10

### What Works Well
- [List strengths]

### Critical Gaps (Block Adoption)
- [List P0 issues with fix suggestions]

### High Priority (Improve Experience)
- [List P1 issues]

### Nice to Have
- [List P2 issues]

## Recommendation
Priority: Fix P0 before next release. Users will struggle without these fixes.
```

## Confidence: medium

DX audit is subjective—depends on the auditor's fresh perspective. However, the checklist above is based on Arwen's role (API ergonomics feedback, first-time user lens) and has been applied to the FASTER project successfully.

## Learned From

2026-03-06: Arwen's charter establishes "Developer experience is a feature. Confusing APIs, bad error messages, and missing docs are bugs." Applied this mindset in 2026-03-10 documentation audit and 2026-03-14 sample README writing. Pattern: user-first lens → gaps found → documented fixes.
