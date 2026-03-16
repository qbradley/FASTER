# Skill: Loom Shim Integration Pattern

## When to Use

When adding or modifying production code that uses sync primitives (`Arc`, `Mutex`, `RwLock`, atomics, threads).

## Pattern

### Production Code: Use `crate::sync` Module

```rust
// ✅ Correct — uses shim
use crate::sync::{Arc, Mutex, RwLock, AtomicU64, Ordering};

pub fn some_function() {
    let counter = Arc::new(AtomicU64::new(0));
    counter.fetch_add(1, Ordering::Release);
}
```

```rust
// ❌ Wrong — direct std import in production code
use std::sync::{Arc, Mutex};  // Bypasses loom mocking
```

### Test Code: Direct `std` is Fine

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;  // ✅ Test code is exempt
    use std::thread;
    
    #[test]
    fn test_concurrent() {
        // Tests can use std directly
    }
}
```

### The Shim Module (`src/sync.rs`)

```rust
#[cfg(loom)]
pub use loom::sync::*;
pub use loom::thread;

#[cfg(not(loom))]
pub use std::sync::*;
#[cfg(not(loom))]
pub use std::thread;

// Export non-standard types explicitly
pub use std::sync::atomic::AtomicU8;
pub use std::sync::RwLockReadGuard;
```

### Import Style

```rust
// ✅ Correct — import Ordering from shim
use crate::sync::{AtomicU64, Ordering};

counter.load(Ordering::Acquire);  // Use imported Ordering
```

```rust
// ❌ Wrong — fully-qualified path bypasses shim
use crate::sync::AtomicU64;

counter.load(std::sync::atomic::Ordering::Acquire);  // ❌ Direct std access
```

## Migration Checklist

When wiring existing code to the shim:

1. [ ] Replace `use std::sync::*` with `use crate::sync::*` in production files
2. [ ] Replace `use std::thread` with `use crate::sync::thread`
3. [ ] Find fully-qualified paths like `std::sync::atomic::Ordering::Acquire`
4. [ ] Import `Ordering` and use `Ordering::Acquire` instead
5. [ ] Verify no direct `std::sync` imports remain in production code (grep check)
6. [ ] Confirm tests still pass: `cargo test`

## Why This Pattern?

- **Loom mocking:** Loom's mock sync primitives replace `std::sync` at compile time
- **Single point of control:** All production code goes through `crate::sync`
- **Test exemption:** Test code (inside `#[cfg(test)]`) can use `std` directly for simplicity
- **Build-time switching:** `#[cfg(loom)]` flag enables mock primitives when loom testing

## Key Files

- **Shim module:** `src/sync.rs`
- **Production files using shim:** All 9 core modules (allocator, hash, store, epoch, etc.)
- **Test files:** Exempt from shim requirement

## Confidence

High

## Learned From

- Loom Shim Integration Sprint (2026-03-11): Wired all 9 production files
- History.md: "All production code must use `crate::sync` module instead of direct `std::sync` or `std::thread` imports"
- Commit 229f0890: Wire loom shim to production code
