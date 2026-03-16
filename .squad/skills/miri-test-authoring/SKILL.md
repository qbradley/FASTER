# Skill: Miri Test Authoring for Unsafe Code

## When to Use
When writing or reviewing ANY unsafe code that doesn't require file I/O or threading. Miri tests are **mandatory** for production unsafe code in FASTER.

## What is Miri?
Miri is Rust's interpreter that detects undefined behavior at runtime:
- Use-after-free
- Out-of-bounds access
- Unaligned pointer dereference
- Data races (when combined with isolation)
- Uninitialized memory reads
- Invalid enum discriminants
- Violation of type invariants

Think of it as a runtime memory safety validator — if Miri passes, the code is UB-free for that execution path.

## Pattern: One Test Per Unsafe Module

### Directory Structure
```
rust/faster-core/tests/
  miri_tests.rs               # Main test file
    mod miri_allocator;       # One module per source file with unsafe
    mod miri_hash_table;
    mod miri_epoch_table;
    ...
```

### Template
```rust
// tests/miri_tests.rs

#[cfg(miri)]
mod miri_allocator {
    use faster_core::allocator::*;

    #[test]
    fn test_descriptive_name() {
        // Setup: Create the unsafe structure
        let allocator = Allocator::new(/* params */);

        // Exercise: Call the unsafe operation
        let ptr = unsafe { allocator.allocate(size) };

        // Verify: Check the invariants
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % ALIGNMENT, 0);

        // Cleanup: Ensure no leaks
        unsafe { allocator.free(ptr) };
    }
}
```

### Key Patterns by Unsafe Type

#### Raw Pointer Dereference
```rust
#[test]
fn raw_pointer_access() {
    // Use aligned backing store (Vec<u64>, not Vec<u8>)
    let mut backing = vec![0u64; 10];
    let ptr = backing.as_mut_ptr() as *mut u8;

    // Miri will catch: out-of-bounds, misalignment, UAF
    unsafe {
        *ptr.add(3) = 42;
        assert_eq!(*ptr.add(3), 42);
    }
}
```

#### Atomic Operations
```rust
#[test]
fn atomic_cas_cycle() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let atomic = AtomicU64::new(100);
    
    // Miri validates ordering correctness
    let prev = atomic.compare_exchange(
        100, 200,
        Ordering::AcqRel,  // Success ordering
        Ordering::Acquire   // Failure ordering
    );
    
    assert_eq!(prev, Ok(100));
}
```

#### Slice from Raw Parts
```rust
#[test]
fn slice_construction() {
    let data = vec![1u32, 2, 3, 4];
    let ptr = data.as_ptr();
    
    // Miri checks: alignment, length bounds, lifetime
    let slice = unsafe { std::slice::from_raw_parts(ptr, 4) };
    assert_eq!(slice[2], 3);
}
```

#### FFI-Style Opaque Handles
```rust
#[test]
fn opaque_handle_lifecycle() {
    // Simulate C-style handle (Box → raw pointer → Box)
    let thing = Box::new(ComplexStruct::new());
    let handle = Box::into_raw(thing);

    // Miri ensures handle is valid
    let value = unsafe { (*handle).get_value() };
    assert_eq!(value, 42);

    // Cleanup: reconstruct Box to free
    unsafe { let _ = Box::from_raw(handle); }
}
```

## Common Pitfalls & Fixes

### Pitfall 1: Unaligned Pointer
```rust
// ❌ WRONG — Miri error: "pointer with alignment 1, but alignment 8 is required"
let mut backing = vec![0u8; 64];
let ptr = backing.as_mut_ptr() as *mut u64;  // u8 alignment, u64 required

// ✅ CORRECT
let mut backing = vec![0u64; 8];  // Naturally 8-byte aligned
let ptr = backing.as_mut_ptr();
```

### Pitfall 2: Hash Value of 0
```rust
// ❌ WRONG — KeyHash::new(0) produces tag=0 (empty bucket sentinel)
let hash = KeyHash::new(0);
let entry = HashBucket::new(hash, address);  // Treated as empty!

// ✅ CORRECT — Use Hashable trait
let hash = my_key.hash();  // Proper hash function with avalanche
```

### Pitfall 3: Using Private Types
```rust
// ❌ WRONG — DrainList is pub(crate), can't construct in tests
let drain = DrainList::new();

// ✅ CORRECT — Test through public API
let epoch_table = EpochTable::new();
epoch_table.defer(callback);  // Exercises DrainList internally
```

## Running Miri Tests
```bash
# Install nightly + Miri
rustup +nightly component add miri

# Run all miri tests
cargo +nightly miri test -p faster-core --test miri_tests

# Run one module
cargo +nightly miri test -p faster-core --test miri_tests miri_allocator

# With nextest (faster)
cargo +nightly miri nextest run -p faster-core --test miri_tests
```

## What Miri CAN'T Test
- File I/O (use integration tests instead)
- Real threading (use Loom for concurrency)
- Async runtime (no tokio under Miri)
- FFI calls to C libraries
- Inline assembly

For these, use:
- **File I/O:** Integration tests with tempfile
- **Concurrency:** Loom shuttle tests
- **Async:** Tokio test harness with `#[tokio::test]`

## Checklist for New Unsafe Code
- [ ] Unsafe module has corresponding `mod miri_<name>` in tests/miri_tests.rs
- [ ] Test covers all unsafe operations in the module
- [ ] Test uses aligned backing stores (Vec<u64>, not Vec<u8>)
- [ ] Test exercises roundtrip (construct → use → verify → drop)
- [ ] `cargo +nightly miri test` passes locally
- [ ] CI runs miri tests in pre-merge checks

## Coverage Goal
**100% of testable unsafe code MUST have miri tests.**

As of 2026-03-11:
- 78 miri tests covering all testable unsafe in faster-core
- Excluded modules: file I/O dependencies only
- Enforcement: Code review flags new unsafe without miri test

## Confidence: high

## Learned From
- 2026-03-08: Expanded from 26 → 71 tests, discovered alignment requirement for RecordAccessor
- 2026-03-11: Achieved 100% testable coverage (78 tests), documented all exclusions
- **Key lesson:** Miri catches subtle bugs that integration tests miss (alignment, uninitialized reads)
