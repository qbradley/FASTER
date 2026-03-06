# Contributing to FASTER Rust

Thank you for contributing to the Rust implementation of FASTER.
This document covers the quality standards and conventions that every change must meet.

For detailed test categories, tooling, and time budgets see [TESTING.md](TESTING.md).

---

## Quality Gate (must pass before commit)

Run these four checks before pushing any change.
CI enforces all of them — save yourself a round-trip.

```bash
# 1. Unit + integration tests (nextest for speed & output)
cargo nextest run -p faster-core

# 2. Doc examples — must compile AND pass
cargo test --doc -p faster-core

# 3. Clippy — zero warnings policy
cargo clippy -p faster-core --all-targets -- -D warnings

# 4. Formatting — must be clean
cargo fmt -p faster-core -- --check
```

All four steps must succeed. A failure in any step blocks the commit.

> **Why `cargo test --doc` separately?**
> `cargo nextest` does not run doctests. Without this explicit step, broken
> doc examples go unnoticed until a user tries to copy-paste them.

---

## Doc Example Rules

Doc examples are user-facing code. Treat them as part of the public API contract.

- Every `/// # Example` block must use only **public API types**.
- Doc examples must be copy-pasteable by an external user —
  if it requires internal types or hidden setup, it's not a good example.
- Never reference internal test types (e.g., `TestFunctions`) in doc examples.
- Use `SimpleFunctions::<K, V>` for examples that need a `Functions` implementation.
- The `cargo test --doc` gate catches violations automatically.

### Good

```rust
/// # Example
///
/// ```
/// use faster_core::SimpleFunctions;
///
/// let functions = SimpleFunctions::<u64, String>::default();
/// ```
```

### Bad

```rust
/// # Example
///
/// ```
/// use faster_core::test_utils::TestFunctions; // ← internal type
/// ```
```

---

## Documentation Timing

Not all documentation belongs in the same phase of work.

- **Code comments and doc-comments:** Write inline with the code, during
  implementation. Every public item gets a doc-comment before the PR merges.
- **User-facing docs (QUICKSTART, guides, examples):** Write **after** features
  stabilize, not during active development.
- **Per-iteration:** Batch user-facing docs into a trailing documentation phase
  after all features in the iteration have landed.

This prevents the "write docs → feature changes → rewrite docs" cycle.
Inline doc-comments still ship with the code — they are cheap to update
when an API changes.

---

## Code Style

- Follow `rustfmt` defaults (see [`rustfmt.toml`](rustfmt.toml)).
- Run `cargo clippy` with `-D warnings` — no suppression without a comment explaining why.
- Every `unsafe` block must document its soundness invariants in a
  `// SAFETY:` comment immediately above.
- See [TESTING.md](TESTING.md) for mandatory test coverage rules
  (miri for `unsafe`, loom for atomics).

---

## Pull Request Checklist

Before requesting review, confirm:

- [ ] Quality gate passes (all four steps above)
- [ ] New public items have doc-comments
- [ ] New `unsafe` blocks have `// SAFETY:` comments and miri tests
- [ ] New atomic/CAS patterns have loom tests
- [ ] Tests are within [time budgets](TESTING.md#test-time-budgets)
