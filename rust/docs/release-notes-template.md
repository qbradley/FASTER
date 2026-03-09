# Release Notes Template

Use this template when writing release notes for a new version.
Copy it, fill in each section, and publish alongside the CHANGELOG entry.

---

## FASTER Rust v{VERSION} Release Notes

**Release date:** YYYY-MM-DD
**MSRV:** {minimum supported Rust version}

### Highlights

> 2–3 sentence summary for someone who has 30 seconds. What's the headline?
> Focus on the single most important improvement and why it matters.

- **Headline feature:** {one sentence}
- **Key numbers:** {performance gains, test counts, API changes}

### Breaking Changes

> List every change that requires user action to upgrade. If there are none,
> write "None" — don't remove the section.

| Change | Migration |
|--------|-----------|
| `OldApi::method()` removed | Replace with `NewApi::method()` |
| `Config` field renamed | `old_name` → `new_name` |

### New Features

> Group by crate. Use the same crate ordering as CHANGELOG.md.

#### `faster-core`

- Feature description with link to docs or API item.

#### `faster-device`

- ...

#### `faster-tokio`

- ...

#### `faster-ffi`

- ...

#### `faster-uring`

- ...

#### `faster-dst`

- ...

#### `faster-bench`

- ...

### Bug Fixes

> Reference issue numbers where possible. Briefly describe impact.

- **#123** — Fixed panic when {condition}. Affected {which users}.
- **#456** — Corrected {behavior}. Previously {old behavior}, now {new behavior}.

### Performance

> Include before/after numbers with methodology. Link to benchmarks.

| Benchmark | Before | After | Change |
|-----------|--------|-------|--------|
| YCSB-A (8 threads) | X Mops | Y Mops | +Z% |

- **How to reproduce:** `cargo bench --bench main` (see `docs/benchmarking.md`)

### Migration Guide

> Step-by-step upgrade instructions. Only needed for releases with breaking
> changes. If no breaking changes, write "No migration required."

1. Update `Cargo.toml`: `faster-core = "{VERSION}"`
2. {Rename any changed types or methods}
3. {Update configuration if schema changed}
4. Run `cargo check` to surface remaining issues.

### Known Issues

> Honest list of limitations or regressions discovered during this cycle.
> If none, write "None known."

- {Issue description} — tracked in #{issue_number}
- {Limitation} — workaround: {description}

### Contributors

> Thank people. Use GitHub handles.

- @handle — {contribution summary}

---

## Instructions for Filling This Out

1. **Start with CHANGELOG.md.** The CHANGELOG is the source of truth for what
   changed. Release notes are the human-friendly version.

2. **Write Highlights first.** Force yourself to pick the one thing that
   matters most. Everything else is supporting detail.

3. **Breaking Changes must include migration steps.** "We renamed X" is not
   enough — tell people exactly what to change in their code.

4. **Performance claims need numbers.** Don't say "faster" — say "23% higher
   throughput on YCSB-A with 8 threads" and link to the benchmark config.

5. **Be honest in Known Issues.** Users trust projects that acknowledge
   limitations. Hiding known problems erodes trust.

6. **Review the diff.** Run `git log --oneline v{PREV}..v{VERSION}` and check
   every commit. The most important fix is often the one nobody remembered to
   write down.

7. **CHANGELOG format reference:**
   [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Categories:
   Added, Changed, Deprecated, Removed, Fixed, Security.
