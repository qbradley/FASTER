# Decision: Sample README Structure & Style Guidelines

**Date:** 2026-03-14  
**Agent:** Arwen (Developer Advocate)  
**Context:** Completed documentation for 8 sample crates (Backlog 3e)  

## Decision

Establish a **standard README template** for all FASTER samples that new users can quickly scan.

## Rationale

When users land on a sample crate, they should be able to answer in **30 seconds**:
1. What does this sample demonstrate?
2. How do I run it?
3. What FASTER concepts does it show?
4. Do I need special platform setup?

The pattern established across all 8 new READMEs reflects this:

```
1. One-line description (title + hook)
2. What It Demonstrates (2–3 sentences)
3. Key Concepts (bullet list of FASTER features)
4. Usage (copy-paste ready cargo run examples)
5. CLI Options (reference table)
6. Example Output (realistic results)
7. Notes (platform requirements, caveats)
```

## Key Guidelines

### Content Ordering
- **Fast scanning first:** Title, hook, purpose (read in 15 seconds)
- **Examples before details:** Usage code before detailed CLI table
- **Realistic output:** Show actual command execution results
- **Platform callouts:** io_uring, feature flags noted prominently

### Tone
- Direct, empathetic to the "I have 10 minutes" person
- No marketing language
- Code examples are copy-paste ready (tested mentally against main.rs)
- Contrasts with sister samples where relevant (e.g., sync vs. async, cache vs. store)

### Sample-Specific Patterns
- **Benchmarks** (cross-impl-bench, disk-io-bench): Emphasize comparison and reproducibility
- **Demos** (event-counter-tokio): Show async/sync bridge pattern with ASCII diagrams
- **Workloads** (page-cache, page-store, read-cache-sim): Explain why memory budget matters
- **Stress tests** (torture-stress): Detail threat model (what corruption could occur)

## Implications

### For Documentation Review (Wave 4+)
- Apply this structure to faster-device, faster-tokio, faster-uring README updates
- Use sample READMEs as template for crate-level examples

### For Users
- All samples now have consistent, discoverable documentation
- No more "what does this even do?" friction

### For Contributors
- Template makes it easy to add new samples
- Clear expectations for sample README quality

## Trade-offs

- **More content:** READMEs are ~3 KB each (vs. minimal existing stubs)
- **Specificity:** Tailored examples may need updates if CLI flags change
- **Maintenance:** Must keep CLI option tables in sync with clap derive macros

*Mitigation:* Add a CI check to flag when README example code diverges from clap definitions.

## Related

- Wave 3 documentation audit identified 5 sample READMEs as P0 gap
- Existing READMEs: tokio-kv-server (detailed), uring-stress (good baseline)
- Next: Apply pattern to crate-level docs (faster-tokio, faster-uring) in Wave 4

---

**Status:** Adopted (all 8 READMEs follow this pattern)  
**Link:** `arwen/sample-readmes` branch, commit 1880da1a
