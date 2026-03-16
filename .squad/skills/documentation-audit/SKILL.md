---
name: "documentation-audit"
description: "Systematic framework for evaluating developer documentation completeness"
domain: "documentation"
confidence: "high"
source: "2026-03-10 Wave 3 audit"
---

## When to Use

Before a release, after major feature additions, or when onboarding new users reports gaps. This is a structured way to find what's missing and prioritize fixes.

## Pattern

### 1. Inventory Phase
- List all crates and public APIs
- Identify existing doc locations (top-level markdown, crate READMEs, doc comments, examples/)
- Note which crates have //! module-level docs (should appear in rustdoc)

### 2. Coverage Assessment
- Count doc examples in lib.rs (cargo test --doc passes them)
- Check for public unsafe code documentation requirements
- Verify all public API types have doc comments
- Rate each crate: sparse (0–2 docs), minimal (3–5), moderate (6–15), comprehensive (15+)

### 3. Sample/Tutorial Review
- Which sample crates are shipped? All must have READMEs
- Do samples show real features in context (not toy examples)?
- Are CLI options documented with example runs?

### 4. Scoring Rubric
- **Core API coverage:** 15–20 points max (each public crate, doc examples present)
- **Sample documentation:** 10 points (all samples have READMEs)
- **README quality:** 10 points (top-level README guides new users)
- **Feature documentation:** 10 points (major features covered with usage examples)
- **Architecture/internals:** 5–10 points (decisions documented, trade-offs explained)
- **Total:** 60 points possible. Score as "X/10"

### 5. Gap Priority Table
For each gap, assign:
- **P0 (Release blocker):** Missing sample READMEs, empty lib.rs docs, broken examples
- **P1 (High priority):** Public API without examples, missing usage docs for major features
- **P2 (Nice to have):** Safety documentation, advanced patterns

### 6. Reporting Format
```markdown
## Documentation Audit Result

**Overall Score:** 7.5/10

### Coverage by Crate
- **faster-core:** 15 examples (comprehensive)
- **faster-ffi:** 8 examples (moderate)
- **faster-tokio:** 3 examples (minimal) — P1 gap

### Gap Summary
| Category | Severity | Effort | Notes |
|----------|----------|--------|-------|
| Sample READMEs | P0 | 2d | 5 crates missing |
| faster-device lib.rs | P0 | 3h | Add 3–4 core examples |

### Recommendation
Complete P0 before release.
```

## Confidence: high

Applied to FASTER Rust pre-0.1.0. Found ~697 doc examples, rated 7.5/10, identified P0 gaps (5 missing sample READMEs), and successfully remediated all gaps by 2026-03-14.

## Learned From

2026-03-10: Wave 3 documentation audit (created `rust/docs/documentation-audit.md`). Found core API well-covered but samples and crate-level docs sparse. Gaps remediated 2026-03-14 with 8 sample READMEs.
