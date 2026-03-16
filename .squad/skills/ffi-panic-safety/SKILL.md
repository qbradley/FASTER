# Skill: FFI Panic Safety Pattern

## When to Use
Any time you write an `extern "C"` function that Rust code will call, or that will be called from C/C++ with function pointers stored in foreign code.

## The Problem
**Rust panics unwinding across FFI boundaries = instant undefined behavior.**

Even if your Rust code is "safe", a panic in:
- Allocation failure
- Integer overflow (in debug mode)
- Index out of bounds
- Assertion failure
- Explicit `panic!()` in dependencies

...will corrupt the C stack, violate exception handling ABIs, and likely segfault.

## The Pattern: catch_unwind Wrapper

### Basic Template
```rust
use std::panic::{catch_unwind, AssertUnwindSafe};

#[no_mangle]
pub extern "C" fn your_ffi_function(arg: *const SomeType) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // Your actual implementation here
        // Return success/error code
        0
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            // Panic occurred — log if possible, return error code
            eprintln!("PANIC in your_ffi_function");
            -1  // Or your FFI error convention
        }
    }
}
```

### With Return Values
```rust
#[no_mangle]
pub extern "C" fn create_something(out: *mut *mut Thing) -> Status {
    if out.is_null() {
        return Status::InvalidArgument;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let thing = Box::new(Thing::new());
        unsafe { *out = Box::into_raw(thing) };
        Status::Ok
    }));

    match result {
        Ok(status) => status,
        Err(_) => {
            eprintln!("PANIC in create_something");
            Status::InternalError
        }
    }
}
```

### With Cleanup on Panic
```rust
#[no_mangle]
pub extern "C" fn process_with_lock(handle: *mut Handle) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let h = unsafe { &mut *handle };
        h.lock.acquire();
        // ... work ...
        h.lock.release();
        0
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            // Attempt cleanup even on panic
            if !handle.is_null() {
                let h = unsafe { &mut *handle };
                h.lock.release();  // May double-release; design accordingly
            }
            -1
        }
    }
}
```

## Why AssertUnwindSafe?
`catch_unwind` requires the closure to be `UnwindSafe`, which most types are NOT by default. `AssertUnwindSafe` is your assertion that:
- The panic won't leave shared mutable state in inconsistent state, OR
- You're okay with the inconsistency because this is a boundary and C code won't see the internals

For FFI boundaries, this is usually correct — you're returning control to C, which can't observe Rust's internal invariants.

## Checklist
For every `extern "C"` function:
- [ ] Wrapped in `catch_unwind(AssertUnwindSafe(|| { ... }))`
- [ ] Panic case returns valid error code per FFI contract
- [ ] Null pointer checks BEFORE `catch_unwind` (so null returns error, not panic)
- [ ] Test: verify panic in Rust doesn't crash C caller

## Testing
```rust
#[test]
fn ffi_panic_safety() {
    // Call FFI function that triggers panic (e.g., via mock)
    let result = your_ffi_function(trigger_panic_arg());
    assert_eq!(result, ERROR_CODE);  // Should return error, not abort
}
```

## Confidence: high

## Learned From
**2026-03-06: Production Security Audit — Critical Finding**

Discovered that ALL 12 `extern "C"` functions in faster-ffi lacked `catch_unwind`. This was the only **Critical** severity finding in the 302-unsafe-site audit. One panic during a C API call would have been instant UB.

**Fix:** Added `catch_unwind` wrapper to all FFI boundary functions. Verified with 70 FFI tests.

**Commit:** `security(audit): add catch_unwind panic protection to all FFI boundary functions`

## Related Hazards
- **Callbacks INTO Rust from C:** If C calls a Rust callback, that callback is also an FFI boundary and needs catch_unwind
- **Thread panic with FFI state:** If a thread panics while holding a lock exposed via FFI, C code may deadlock
- **Panic in Drop:** Even with catch_unwind, if a Drop impl panics, it can still abort the process
