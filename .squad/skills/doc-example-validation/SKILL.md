---
name: "doc-example-validation"
description: "Ensuring doc comment examples are compiled, tested, and maintainable"
domain: "documentation"
confidence: "high"
source: "2026-03-06 quality gate (CONTRIBUTING.md)"
---

## When to Use

When defining or auditing documentation practices. This ensures that examples in doc comments stay correct as code evolves, and that new contributors understand the standard.

## Pattern

### Rule: Public API = Doc Comment with Example

Every public type, function, trait, and method must have a `///` or `//!` doc comment. For public API, the comment should include a working example.

### Example Requirement

An example means:
- Compilable Rust code in a `` ``` `` block (language tagged as ```rust)
- Code that runs without panicking (for real usage examples)
- Demonstrates the common case or the primary feature of that API

Example (good):

```rust
/// Stores a key-value pair in the KV map.
///
/// # Example
///
/// ```
/// let mut kv = FasterKv::new()?;
/// kv.insert("key", "value")?;
/// assert_eq!(kv.get("key")?, Some("value".into()));
/// ```
pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<Status> { ... }
```

### Testing Doc Examples

**Critical rule:** `cargo test --doc` must pass.

`cargo nextest` does NOT run doctests — you must explicitly run:

```bash
cargo test --doc
```

This compiles and runs every example in every doc comment. If an example is broken, this command will catch it before release.

### Example Authoring Guidelines

1. **Keep examples small:** 3–10 lines max. Long examples go in the crate README or examples/ directory.

2. **Use the happy path:** Show the normal, expected usage. Edge cases and error paths can be documented separately.

3. **Don't use internal types:** Examples use public APIs only. If you need to show an internal pattern, document it in implementation code, not doc comments.

4. **Include setup if necessary:** If the example needs initialization (e.g., creating a KV instance), show that inline.

5. **Test assertions:** If the example shows output, use `assert_eq!()` or `assert!()` so the test verifies correctness.

### Documentation Timing Convention

- **Inline doc-comments:** Written at the same time as the code during development. Don't defer.
- **User-facing docs:** Written after the API has stabilized (e.g., after 2–3 iterations, when the shape is final).

Rationale: Inline docs document the API contract; user guides document adoption strategy. Don't write tutorials for an unstable API.

### Anti-Patterns

| Pattern | Problem | Fix |
|---------|---------|-----|
| `/// Does something` | No example | Add `` ``` ``rust example showing `something()` in context |
| Example with unwrap() | May panic | Use `?` operator and return `Result`, or show `let _ = ...;` pattern |
| Example with internal types | Won't compile for users | Use public types only |
| Doc comment with no ///? | Not visible in rustdoc | Use `///` for public items, `//!` for modules |

### Integration with CI

Pre-commit check should include:

```bash
cargo test --doc
```

Release gate should verify all doc tests pass. Broken docs = blocker for release.

## Confidence: high

Documented in 2026-03-06 `rust/CONTRIBUTING.md`. Quality gate includes `cargo test --doc` as explicit step. Applied across project; ~697 doc examples across 6 crates validated.

## Learned From

2026-03-06: Discovered that `cargo nextest` does NOT run doctests. Created CONTRIBUTING.md with explicit `cargo test --doc` step in quality gate. Documented convention: inline docs during development, user guides after stabilization.
