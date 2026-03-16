# Skill: Design Decision Documentation

## When to Use

After making any architectural decision that:
- Affects multiple subsystems
- Changes the API surface
- Involves a trade-off between alternatives
- Other agents need to know about
- Future contributors might question

## Pattern

### Decision Document Structure

File: `.squad/decisions/inbox/gandalf-{brief-slug}.md`

```markdown
# Decision: {Title}

**Author:** {Your Name/Role}
**Date:** {YYYY-MM-DD}
**Branch:** {branch-name or "direct commit"}
**Status:** {Proposed | Implemented | Archived}

## Summary

2-3 sentence summary of what was decided.

## Architecture

{Diagram, code snippet, or structural description}

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| {What}   | {Why}     |

## Alternatives Considered

- **Option A:** {description} — Rejected because {reason}
- **Option B:** {description} — Rejected because {reason}

## Team Impact

- **{Agent/Role}:** {How this affects their work}
- **{Agent/Role}:** {How this affects their work}

## Files Changed

{List of key files or modules}
```

### Examples from History

**Good decision capture:**
- Hash Index Software Prefetch (P-05) — includes rationale, team impact, alternatives
- EPVS coordination — includes two-phase CAS design, cross-module dependencies
- Release automation — includes dependency ordering rationale, semver strategy

**Patterns to capture:**
- "We chose X over Y because..." → Document alternatives
- "This affects {subsystem}" → Document team impact
- "This assumes {constraint}" → Document assumptions

### When to Skip

Don't document:
- Local implementation details (variable naming, loop structure)
- Decisions already covered by existing patterns (e.g., "used cargo nextest")
- Trivial choices with no reasonable alternative

## Confidence: high

## Learned From

- "Design decisions are documented with rationale, not just outcomes" (charter)
- decisions.md structure (existing team decisions follow this pattern)
- Retrospective: "12 binding decisions on Day 1 were never revisited" — early documentation prevents rework
