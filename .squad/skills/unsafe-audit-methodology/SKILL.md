# Skill: Unsafe Code Audit Methodology

## When to Use
When reviewing any Rust code containing `unsafe` blocks, functions, or trait implementations for production readiness.

## Pattern: The 5-Layer Security Audit

### Layer 1: Inventory & Coverage
```bash
# Count all unsafe sites
rg "unsafe\s+(fn|impl|trait|{)" --count-matches
# Verify SAFETY comments exist (should be enforced at compile time)
grep -r "#!\[forbid(clippy::undocumented_unsafe_blocks)\]" src/lib.rs
```

**What to catalog:**
- Total count by type (blocks, fns, impls)
- Location distribution (which modules have most unsafe)
- Percentage that could be eliminated or abstracted

### Layer 2: SAFETY Comment Quality Review
For each unsafe site, the SAFETY comment must answer:
1. **What invariants** does this code rely on?
2. **Who enforces** those invariants? (caller? type system? prior check?)
3. **What happens** if the invariant is violated?

**Red flags:**
- "This is safe because..." with no invariant statement
- "Guaranteed by..." with no reference to the guarantee mechanism
- "Should be safe" or "probably safe" language
- SAFETY comment just restates what the code does

### Layer 3: Known Hazard Patterns
Check for these specific vulnerabilities:

**Memory Safety:**
- [ ] Raw pointer dereference without bounds/alignment checks
- [ ] `slice::from_raw_parts()` with unvalidated length
- [ ] Mutable aliasing (two `&mut` to same data)
- [ ] Use-after-free via dangling pointers
- [ ] Data race on non-atomic shared mutable state

**FFI-Specific:**
- [ ] Missing `catch_unwind` on `extern "C"` functions (panic = instant UB)
- [ ] Callback pointers without lifetime validation
- [ ] Null pointer handling (always check, never assume)
- [ ] Integer truncation (u64 → u32 casts in FFI boundaries)
- [ ] Unvalidated enum discriminants from C

**Concurrency:**
- [ ] ABA problem in lock-free structures (check tag bit width)
- [ ] Ordering too weak (Relaxed where Acquire/Release needed)
- [ ] Ordering too strong (SeqCst where Acquire/Release sufficient)
- [ ] Missing synchronization for cross-thread pointer sharing

### Layer 4: Testability Assessment
For each unsafe module, verify:
- [ ] Miri test exists and passes (for non-I/O code)
- [ ] Loom test exists for concurrency primitives
- [ ] Fuzz target exists for deserialization/parsing
- [ ] Integration test with realistic usage pattern

**Coverage goal:** 100% of testable unsafe code

### Layer 5: Documentation & Enforcement
- [ ] All findings tracked with severity (Critical/High/Medium/Low)
- [ ] Each finding has reproduction case or proof
- [ ] `#![deny(unsafe_op_in_unsafe_fn)]` in lib.rs (explicit unsafe blocks)
- [ ] `#![forbid(clippy::undocumented_unsafe_blocks)]` in lib.rs (compiler-enforced SAFETY comments)
- [ ] Security audit artifact (SECURITY-AUDIT.md) in repo

## Confidence: high

## Learned From
- 2026-03-05: Initial 90-site unsafe audit of faster-core
- 2026-03-06: Full 302-site production audit across 5 crates
- 2026-03-11: Complete miri coverage expansion (78 tests)

## Key Insight
**"All SAFETY comments present" means nothing if the comments are wrong.** The methodology must verify the *reasoning*, not just the presence of text. Use Miri/Loom/fuzz to validate the invariants, not just document them.
