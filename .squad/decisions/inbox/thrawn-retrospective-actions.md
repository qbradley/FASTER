# Retrospective Action Items — Iteration 3 Session

**Facilitator:** Thrawn (Lead / System Architect)  
**Date:** 2026-03-05  
**Scope:** All work on `squad` branch this session (Iterations 1–3 + Iter 4 planning)

---

## ⚡ Action Items

### AI-1: Fix 4 Failing Doctests in `builder.rs` and `kv.rs`
**Priority:** P0 — broken CI  
**Owner:** Next available agent  
**Description:** Four doctests in `crates/faster-core/src/store/builder.rs` and `kv.rs` reference `TestFunctions` which is not in scope for doc examples. These should use `SimpleFunctions::<u64, u64>` instead. This was missed because `cargo test --lib` and integration tests pass — doctests aren't in the default test flow.  
**Acceptance:** `cargo test --doc -p faster-core` passes with 0 failures.

### AI-2: Add `cargo test --doc` to Pre-Commit Quality Gate
**Priority:** P1  
**Owner:** Thrawn / qbradley  
**Description:** The 4 broken doctests went undetected because our testing flow runs unit + integration tests but not doc tests. Either add `cargo test --doc` to the standard test command or switch to `cargo test` (which includes doctests) as the gate.  
**Acceptance:** CI/quality gate command explicitly includes doc tests.

### AI-3: Enforce "Doc Examples Must Compile" Convention
**Priority:** P1  
**Owner:** All agents  
**Description:** Doc examples that reference internal test types (`TestFunctions`) are not real examples — they mislead users. Convention: every `/// # Example` block must use only public API types and be copy-pasteable by an external user. Add `#![deny(rustdoc::broken_intra_doc_links)]` to `lib.rs` if not already present.  
**Acceptance:** Convention documented; rustdoc lint enabled.

### AI-4: Investigate 10M ops/sec Target Gap
**Priority:** P2  
**Owner:** Performance-focused agent  
**Description:** Iteration 3 peaked at 8.11M single-threaded upserts/sec on shared infrastructure. The 10M target was set in the iteration plan. Either (a) validate on dedicated hardware, (b) profile the remaining ~20% gap (likely epoch overhead, hash computation, or memory allocation), or (c) revise the target with justification.  
**Acceptance:** Root-cause analysis with profiling data, or validated benchmark on dedicated hardware.

### AI-5: Audit `unsafe` Footprint Before Iteration 4
**Priority:** P2  
**Owner:** Maul (Safety Auditor) or equivalent  
**Description:** 23 files contain `unsafe` code (~191 occurrences). Before adding io_uring and C FFI (Iteration 4), audit current unsafe sites for soundness documentation. Each `unsafe` block should have a `// SAFETY:` comment explaining the invariant. Several were added during fast iteration without full documentation.  
**Acceptance:** Every `unsafe` block in `faster-core` has a `// SAFETY:` comment.

### AI-6: Write-Path Pending I/O Completion — Close the Loop
**Priority:** P1  
**Owner:** Implementation agent  
**Description:** Write-path pending I/O (`bfeea1a3`) was delivered but QUICKSTART.md was updated (`6b94ce3c`) to be "honest about pending I/O semantics" — meaning the mental model had to be corrected after the docs were written. The lesson: write the doc *after* the feature stabilizes, not during. For Iteration 4, batch docs into a single phase after all features land.  
**Acceptance:** Iteration 4 plan structures docs as a trailing phase.

### AI-7: Reduce Test Execution Time Sensitivity
**Priority:** P3  
**Owner:** Any  
**Description:** The `write_pending_completion` tests originally took 18.5s and were sped up 20× to 0.9s (`6a2d4ebd`). This suggests timeouts and sleep-based synchronization were used initially. Convention for Iteration 4: no `thread::sleep` in tests unless explicitly justified. Use condition variables, barriers, or event-driven synchronization.  
**Acceptance:** Convention documented; new tests reviewed for sleep-based waits.

---

## Decision Proposals (from retrospective discussion)

### DP-1: Iteration 4 Should Gate on `cargo test` (not `cargo test --lib`)
All test types (unit, integration, doc, example) must pass before any commit is considered green.

### DP-2: Documentation Phase Should Trail Feature Phases
Quickstart and public docs written once per iteration, after all features stabilize. Internal doc-comments written inline with code.

### DP-3: Benchmark Validation on Dedicated Hardware
Performance targets should be validated on dedicated (non-shared) infrastructure before claiming or missing targets.
