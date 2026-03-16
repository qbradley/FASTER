# Skill: Cross-Language Consistency Validation

## When to Use

When implementing or reviewing core FASTER primitives (hash table, epoch, checkpoint, recovery). Ensures behavioral equivalence with reference implementations.

## Pattern

### C++ Reference Implementation
- **Location:** `cc/src/` in the FASTER repo
- **Key files:**
  - `cc/src/core/hash_table.h` — hash bucket layout, tag bits, overflow chain
  - `cc/src/core/epoch.h` — epoch-based memory reclamation
  - `cc/src/core/checkpoint.h` — checkpoint state machine
  - `cc/src/device/` — I/O device abstraction

### C# Reference Implementation
- **Location:** `cs/src/` in the FASTER repo
- **Key files:**
  - `cs/src/core/Index/FASTER/FASTERBase.cs` — main hash index
  - `cs/src/core/Index/FASTER/FASTERThread.cs` — session API
  - `cs/src/core/Allocator/` — log-structured allocator

### Validation Checklist

**1. Hash Table Layout**
- [ ] Bucket size (64 bytes = 1 cache line)
- [ ] Entries per bucket (7 + overflow pointer)
- [ ] Tag bit width (8 bits from hash MSBs)
- [ ] Linear probe within bucket before overflow
- [ ] Prefetch-friendly access pattern

**2. Epoch Protection**
- [ ] Version scheme matches reference (Prepare → InProgress → Complete)
- [ ] Two-phase CAS for ABA prevention
- [ ] Version only bumps on Prepare→InProgress
- [ ] Reclamation deferred until all threads exit epoch

**3. Session Model**
- [ ] Single-writer session (no interior mutability on hot path)
- [ ] Thread-local state isolation
- [ ] Checkpoint coordination across sessions

**4. Semantic Divergences**
If Rust implementation intentionally diverges:
- Document in architecture notes
- Include rationale (e.g., "Rust ownership prevents X pattern")
- Verify equivalent guarantees

## Confidence: medium

## Learned From

- Hash table layout investigation (A8) — verified 64-byte bucket optimality against C++/C# implementations
- EPVS coordination — two-phase intermediate CAS pattern matches C# reference
- "Cross-reference C++ and C# implementations to ensure behavioral equivalence" (charter)
