# Skill: Unsafe Rust Discipline in FASTER

## When to Use

Every time you write an `unsafe` block in production code (not FFI tests).

## Pattern

```rust
// SAFETY: [Detailed explanation of why this unsafe operation is sound]
// Invariants:
// - [Invariant 1 that must hold for soundness]
// - [Invariant 2 that must hold for soundness]
// - [How these invariants are maintained]
unsafe {
    // unsafe operations
}
```

### Key Principles

1. **Unsafe is a contract, not a shortcut** — Every unsafe block must document why it's sound
2. **Document the invariants** — What conditions must be true for this code to be safe?
3. **Explain the reasoning** — Why do these invariants guarantee soundness?
4. **Prefer safe abstractions** — Minimize unsafe surface area; wrap in safe APIs

### Enforcement

- **Clippy warning level:** `clippy::undocumented_unsafe_blocks` must pass
- **Module-level opt-in:** Use `#[deny(unsafe_code)]` on modules that should never need unsafe (e.g., `compaction/`)
- **Code review bar:** Any unsafe without proper documentation is automatically rejected

## Examples

### Good — Documented Unsafe
```rust
// SAFETY: We hold epoch protection via UnsafeContext, which guarantees no page
// is evicted or truncated during batch operations. The address was validated
// as in-bounds during hash lookup. Record alignment is guaranteed by allocator.
unsafe {
    let record = &*(address.as_ptr());
    record.value()
}
```

### Bad — Undocumented Unsafe
```rust
unsafe {
    let record = &*(address.as_ptr());  // ❌ No explanation
    record.value()
}
```

## Confidence

High

## Learned From

- Wave 1 precheckin sweep (2026-03-09): Found 27 undocumented unsafe blocks in FFI tests
- Compaction scanner design (2026-03-06): Entire compaction module marked `#[deny(unsafe_code)]`
- Charter principle: "Thinks `unsafe` is a contract, not a shortcut"
