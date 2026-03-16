# Skill: Atomic Ordering Review Checklist

## When to Use
When reviewing any code using `std::sync::atomic` types (AtomicU64, AtomicPtr, etc.) or when debugging concurrency bugs in lock-free data structures.

## The Ordering Hierarchy
From weakest to strongest synchronization:
1. **Relaxed** — no ordering, only atomicity
2. **Acquire** — synchronize-with Release on same variable (read side)
3. **Release** — synchronize-with Acquire on same variable (write side)
4. **AcqRel** — combined Acquire+Release (for RMW operations like CAS)
5. **SeqCst** — global total order (expensive, rarely needed)

## Pattern: The 4-Question Method

For each atomic operation, ask:

### 1. Is this a read, write, or read-modify-write?
- **Read:** load
- **Write:** store
- **RMW:** compare_exchange, fetch_add, swap

### 2. Does this operation need to synchronize with another thread?
- **No:** Use `Relaxed` (example: performance counter, statistics)
- **Yes:** Continue to question 3

### 3. What data does the synchronization protect?
Identify the non-atomic data that must be visible across threads:
- Pointer dereference after atomic load?
- Struct fields written before atomic flag set?
- Epoch data protected by atomic guard?

### 4. What is the synchronization direction?
- **Read side (consumer):** Use `Acquire` on load
- **Write side (producer):** Use `Release` on store
- **Both (RMW):** Use `AcqRel` on CAS/swap

## Common Patterns & Correct Orderings

### Pattern 1: Flag-Protected Data
```rust
// Writer thread
non_atomic_data.value = 42;
flag.store(true, Ordering::Release);  // ✅ Release: publishes data

// Reader thread
if flag.load(Ordering::Acquire) {     // ✅ Acquire: sees data
    let value = non_atomic_data.value;  // Safe: synchronized
}
```

### Pattern 2: Lazy Initialization (Once)
```rust
static INIT: AtomicBool = AtomicBool::new(false);
static mut DATA: Option<ExpensiveStruct> = None;

if !INIT.load(Ordering::Acquire) {    // ✅ Acquire before reading DATA
    unsafe { DATA = Some(initialize()); }
    INIT.store(true, Ordering::Release);  // ✅ Release after writing DATA
}
```

### Pattern 3: Lock-Free Queue (Pointer CAS)
```rust
let mut head = self.head.load(Ordering::Acquire);  // ✅ Acquire: load may alias
loop {
    let node = unsafe { &*head };
    let next = node.next.load(Ordering::Relaxed);  // ✅ Relaxed: already synchronized
    
    match self.head.compare_exchange_weak(
        head, next,
        Ordering::Release,   // ✅ Release: publish new head
        Ordering::Acquire    // ✅ Acquire: reload on failure
    ) {
        Ok(_) => break,
        Err(h) => head = h,
    }
}
```

### Pattern 4: Reference Counting
```rust
// Increment (clone)
old_count.fetch_add(1, Ordering::Relaxed);  // ✅ Relaxed: no data sync needed

// Decrement (drop)
if old_count.fetch_sub(1, Ordering::Release) == 1 {  // ✅ Release: sync before free
    atomic::fence(Ordering::Acquire);  // ✅ Acquire: see all writes
    drop(data);  // Safe: no other refs
}
```

### Pattern 5: Epoch Counter (Read-Side)
```rust
// Protect (reader enters epoch)
let epoch = global_epoch.load(Ordering::Acquire);  // ✅ Acquire: see all prior writes
register_reader(epoch);

// Unprotect (reader exits epoch)
unregister_reader();  // No ordering needed for unregister itself
```

## Red Flags: When Ordering is WRONG

### ❌ SeqCst Overuse
```rust
// ❌ WRONG — unnecessary global ordering
let value = counter.load(Ordering::SeqCst);

// ✅ CORRECT — Relaxed sufficient for independent counter
let value = counter.load(Ordering::Relaxed);
```

### ❌ Relaxed Underuse
```rust
// ❌ WRONG — Relaxed load after synchronized acquisition
let ptr = self.ptr.load(Ordering::Acquire);  // Acquires synchronization
let metadata = self.metadata.load(Ordering::Acquire);  // ❌ Already synchronized!

// ✅ CORRECT
let ptr = self.ptr.load(Ordering::Acquire);  // Acquire once
let metadata = self.metadata.load(Ordering::Relaxed);  // ✅ Piggyback on prior Acquire
```

### ❌ CAS Failure Ordering > Success Ordering
```rust
// ❌ WRONG — failure ordering can't be stronger than success
compare_exchange(old, new, Ordering::Acquire, Ordering::AcqRel)

// ✅ CORRECT
compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
```

### ❌ Missing Release Before Free
```rust
// ❌ WRONG — other threads may have pending reads
if refcount.fetch_sub(1, Ordering::Relaxed) == 1 {
    drop(data);  // ❌ UAF: no synchronization!
}

// ✅ CORRECT
if refcount.fetch_sub(1, Ordering::Release) == 1 {
    atomic::fence(Ordering::Acquire);
    drop(data);  // Safe: synchronized
}
```

## Verification Strategies

### Static Analysis
- Search for `Ordering::SeqCst` → justify or downgrade
- Search for `Ordering::Relaxed` on loads with pointer dereference → justify or upgrade
- Check all CAS calls: success ≥ failure ordering

### Loom Testing
```rust
#[cfg(loom)]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, Ordering};
    use loom::thread;

    #[test]
    fn ordering_verified_by_loom() {
        loom::model(|| {
            let flag = Arc::new(AtomicBool::new(false));
            let data = Arc::new(AtomicUsize::new(0));

            let t1 = {
                let flag = flag.clone();
                let data = data.clone();
                thread::spawn(move || {
                    data.store(42, Ordering::Relaxed);
                    flag.store(true, Ordering::Release);  // Must be Release
                })
            };

            let t2 = {
                let flag = flag.clone();
                let data = data.clone();
                thread::spawn(move || {
                    if flag.load(Ordering::Acquire) {  // Must be Acquire
                        assert_eq!(data.load(Ordering::Relaxed), 42);
                    }
                })
            };

            t1.join().unwrap();
            t2.join().unwrap();
        });
    }
}
```

### Miri (Sequential, but catches Relaxed data races)
```bash
cargo +nightly miri test
# Miri detects when Relaxed ordering permits reordering that breaks invariants
```

## Checklist for Atomic Code Review
- [ ] Every atomic operation has a comment explaining synchronization intent
- [ ] No `SeqCst` without justification (global ordering required?)
- [ ] All pointer loads use `Acquire` (or justified `Relaxed` after prior Acquire)
- [ ] All pointer stores use `Release` (publishes data for dereference)
- [ ] CAS operations use `AcqRel` for success (or justify weaker)
- [ ] Reference count decrements use `Release` + `Acquire` fence before drop
- [ ] Loom tests verify the ordering under thread interleaving

## Confidence: high

## Learned From
- 2026-03-06: Production audit — "All atomic orderings verified correct. No SeqCst overuse, no Relaxed underuse."
- **Key insight:** Most bugs are from Relaxed where Acquire/Release needed. SeqCst is almost never needed (only for global ordering like dekker's algorithm).
- **Verification workflow:** Loom for concurrency, Miri for sequential UB, manual review for ordering rationale

## References
- [Rust Atomics and Locks](https://marabos.nl/atomics/) by Mara Bos (definitive guide)
- `std::sync::atomic::Ordering` docs (compact summary)
