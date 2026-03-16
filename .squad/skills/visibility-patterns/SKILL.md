# Skill: Visibility Patterns in FASTER

## When to Use

When deciding visibility modifiers for structs, fields, and methods in the FASTER crate.

## Pattern

### `pub(crate)` for Cross-Module Access

FASTER uses `pub(crate)` extensively to allow cross-module access within the crate without exposing internals to users.

```rust
// In store/kv.rs
pub struct FasterKv<F: Functions> {
    pub(crate) hash_index: HashIndex,      // ✅ Accessed by session.rs
    pub(crate) allocator: RecordAllocator,  // ✅ Accessed by session.rs
    config: FasterKvConfig,                 // Private — internal only
}
```

**Why:** `FasterSession` and `UnsafeContext` (in `session.rs`) need direct access to hash_index and allocator for batch operations, but users should never touch these internals.

### Common `pub(crate)` Use Cases

1. **Store internals accessed by sessions:**
   - `FasterKv` fields → accessed by `FasterSession`, `UnsafeContext`
   - `HashIndex` methods → called by compaction, maintenance

2. **Allocator internals accessed by store:**
   - `RecordAllocator` fields → accessed by maintenance, eviction
   - Page state → checked by maintenance thread

3. **Epoch internals accessed by sessions:**
   - `EpochTable` methods → called by `FasterSession`
   - Thread registration → accessed by session init

### Public API Surface

```rust
// Public user-facing API
pub struct FasterKv<F> { /* ... */ }

impl<F: Functions> FasterKv<F> {
    pub fn builder() -> FasterKvBuilder { /* ... */ }
    pub fn session(&self) -> FasterSession<F> { /* ... */ }
}

// Everything else is pub(crate) or private
```

## Decision Tree

```
Is this item part of the public API users call directly?
├─ YES → `pub`
│  Examples:
│  - FasterKv::builder()
│  - FasterSession::read()
│  - FasterKvBuilder::with_capacity()
│
├─ NO → Does another module in the crate need it?
│  ├─ YES → `pub(crate)`
│  │  Examples:
│  │  - FasterKv.hash_index (session needs it)
│  │  - RecordAllocator.advance_tail() (kv needs it)
│  │  - EpochTable.protect() (session needs it)
│  │
│  └─ NO → Private (default)
│     Examples:
│     - FasterKv.config (only used in kv.rs)
│     - Helper functions used in one module
```

## Examples

### Correct Usage
```rust
// store/kv.rs
pub struct FasterKv<F> {
    pub(crate) hash_index: HashIndex,  // ✅ session.rs batch ops need this
    config: FasterKvConfig,             // ✅ Private — only used here
}

// store/session.rs
impl<F> UnsafeContext<'_, F> {
    pub fn batch_read(&mut self, kv: &FasterKv<F>, key: &K) -> Status {
        kv.hash_index.lookup(key)  // ✅ Can access pub(crate) field
    }
}
```

### Incorrect Usage
```rust
// ❌ Don't expose internals as pub
pub struct FasterKv<F> {
    pub hash_index: HashIndex,  // ❌ Users can now mess with internals
}

// ❌ Don't make everything pub(crate)
pub struct FasterKv<F> {
    pub(crate) config: FasterKvConfig,  // ❌ Only used in kv.rs, should be private
}
```

## Anti-Patterns

- **Over-publicizing:** Making fields `pub` when only internal code needs them
- **Under-publicizing:** Making fields private when sibling modules need them (forces getters)
- **Getter proliferation:** Adding getters for `pub(crate)` fields accessed frequently

## Confidence

High

## Learned From

- History.md: "Visibility: `pub(crate)` on FasterKv fields for cross-module access (e.g., session.rs batch methods)"
- Code review: Session batch methods need direct access to hash_index, allocator
- Architecture decision: Keep public API minimal, expose internals only within crate
