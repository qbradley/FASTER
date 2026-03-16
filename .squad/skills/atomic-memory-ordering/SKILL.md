# Skill: Atomic Memory Ordering for RecordInfo Protocol

## When to Use

When implementing lock-free protocols using bit-packed atomic state (e.g., RecordInfo, sealing, revivification) where different threads coordinate through atomic read-modify-write operations on shared state.

## Pattern

### Sealing Protocol Ordering Rules

1. **Write (seal)**: Use `Release` ordering
   - `fetch_or(SEALED_BIT_MASK, Ordering::Release)`
   - Ensures all prior writes to the record are visible before seal flag becomes visible

2. **Read (is_sealed)**: Use `Acquire` ordering
   - `load(Ordering::Acquire)`
   - Ensures if we see the seal bit, we also see all prior writes to the record

3. **Read-Modify-Write (try_seal, try_revivify)**: Use `AcqRel`/`Acquire` CAS
   - Success: `AcqRel` (acts as both Release and Acquire)
   - Failure: `Acquire` (synchronizes with other thread's success)
   - `compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)`

### Justification Template

For every atomic operation, document:
```rust
// Memory ordering: [Release|Acquire|AcqRel|Relaxed]
// Rationale: [What synchronizes with what]
// Synchronized with: [file:line of corresponding operation]
```

### Common Patterns

| Operation | Ordering | Rationale |
|-----------|----------|-----------|
| Set flag visible to readers | Release | Prior writes must be visible before flag |
| Read flag, act on it | Acquire | Must see writes that happened-before flag |
| CAS state transition | AcqRel/Acquire | Full synchronization on success |
| Increment counter | Relaxed | If no other state depends on visibility |

## Confidence: High

## Learned From

- **W2-02 (Sealed Bit)**: Initial sealing protocol design with Release/Acquire
- **A5 (Revivification)**: CAS-based unsealing following same ordering pattern
- **Charter principle**: "Every atomic operation gets explicit memory ordering justification"

## Key Files

- `rust/crates/faster-core/src/record/record_info.rs` (seal/revivify implementations)
- FASTER C++ reference: `cc/src/core/record_info.h` (original ordering choices)

## Anti-Patterns

❌ **Don't use Relaxed for coordination flags** — it breaks happens-before relationships
❌ **Don't use SeqCst by default** — it's overkill for most protocols and harms performance
❌ **Don't leave ordering choices undocumented** — future maintainers need the rationale
