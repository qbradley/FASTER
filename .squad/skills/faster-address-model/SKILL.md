# Skill: FASTER Address Model Patterns

## When to Use

When working with addresses in the hybrid log (reads, writes, address arithmetic, hash index chains).

## Pattern

### Address Structure
```rust
// LogicalAddress: 48 bits total
// - 25 bits: offset within page (LSB)
// - 23 bits: page number
// - 16 bits: reserved (MSB)

pub struct LogicalAddress(u64);

impl LogicalAddress {
    pub const INVALID: Self = Self(1);  // NOT 0!
    
    pub fn new(page: u32, offset: u32) -> Self {
        // bit packing logic
    }
    
    pub fn page(&self) -> u32 { /* extract bits */ }
    pub fn offset(&self) -> u32 { /* extract bits */ }
}
```

### Key Invariants

1. **INVALID = 1, not 0**
   - `0` means empty hash bucket entry (no record ever inserted)
   - `1` means "address present but not yet valid" (tombstone, in-flight operation)
   
2. **Atomic operations require explicit Ordering**
   - `AtomicLogicalAddress` has no default — must specify `Acquire`, `Release`, `SeqCst`, etc.
   - More idiomatic than C++ implicit SeqCst default
   
3. **Page size is 2^25 = 32 MiB**
   - With 24-byte records (u64 key + u64 value + header), ~1.4M records per page
   - Tests that fill pages need millions of records

4. **Address classification regions**
   ```
   Truncated < head_address < OnDisk < ReadOnly < Mutable < InMutable
   ```
   See `AddressInfo::classify()` for region logic.

## Examples

### Correct INVALID check
```rust
if address == LogicalAddress::INVALID {
    return OperationStatus::NotFound;
}
```

### Atomic address with explicit ordering
```rust
let next = AtomicLogicalAddress::new(LogicalAddress::INVALID);
next.store(new_address, Ordering::Release);  // ✅ Explicit ordering
let addr = next.load(Ordering::Acquire);      // ✅ Explicit ordering
```

### Incorrect — using 0 as INVALID
```rust
if address.0 == 0 {  // ❌ 0 is valid (empty bucket), not INVALID
    return OperationStatus::NotFound;
}
```

## Anti-Patterns

- **Don't use `RecordInfo::is_null()` to detect unwritten memory** — version 0 is a valid record
- **Don't assume SeqCst** — always specify ordering for atomic operations
- **Don't forget truncated region handling** — upsert/RMW on truncated addresses must allocate fresh records at tail

## Confidence

High

## Learned From

- History.md line 48-50: Address model documentation from C++ analysis
- Wave 5 lossy cache (Session 6): Truncated region handling bug — upsert/RMW returned Aborted instead of creating new records
- Session log: "RecordInfo::is_null() unreliable for distinguishing unwritten memory from valid version-0 records"
