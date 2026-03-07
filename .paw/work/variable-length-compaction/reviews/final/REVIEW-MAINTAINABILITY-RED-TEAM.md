# REVIEW-MAINTAINABILITY-RED-TEAM.md

**Perspective**: red-team
**Specialist**: maintainability
**Scenario**: A new developer wants to introduce a subtle bug through a "reasonable-looking" change. What code patterns make bugs easy to introduce?

---

## Cognitive Strategy Applied

Red-team walkthrough: I read the code asking "where could a seemingly innocent change silently break things?" I focused on implicit contracts (not enforced by types), panic vs error boundaries, numeric type transitions (usize ↔ u32), and places where the code relies on caller discipline rather than structural guarantees.

---

### Finding: `serialized_size_from_bytes` can panic inside `record_size_from_bytes`'s error-returning context — a corrupted value prefix bypasses the corruption-abort design

**Severity**: must-fix
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

In `record/layout.rs:327-338`, `record_size_from_bytes` does:
```rust
let key_buf = &page_bytes[offset + key_offset..];
let key_size = K::serialized_size_from_bytes(key_buf);       // can panic
let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);

if value_offset > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}

let value_buf = &page_bytes[offset + value_offset..];
let value_size = V::serialized_size_from_bytes(value_buf);   // can panic
```

The `Vec<u8>` implementation of `serialized_size_from_bytes` (traits.rs ~317):
```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(
        buf[..LENGTH_PREFIX_SIZE].try_into()
            .expect("buffer too short for Vec<u8> length prefix"),
    ) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

**Attack vector**: A "reasonable" change such as reducing `MIN_READABLE` from 12 to 8 (with the justification "fixed-size types don't need 12 bytes") would cause `serialized_size_from_bytes` to be called with only 0 bytes after the key offset, triggering a panic in production. Even without changing `MIN_READABLE`, the check `value_offset > remaining` does not guarantee `remaining - value_offset >= LENGTH_PREFIX_SIZE`. A corrupted key prefix that yields `key_size = remaining - key_offset - 2` would produce `value_offset = remaining - 2`, pass the check, and then `V::serialized_size_from_bytes` panics when trying to read 4 bytes from a 2-byte buffer.

#### Warrant (Rule)

The compaction abort design (T-2 decision) explicitly chose "abort on corruption, don't panic." But the implementation has a gap: `record_size_from_bytes` returns `Result`, but `serialized_size_from_bytes` panics. A malicious or careless developer could exploit this by constructing page data where the key prefix is valid but leaves insufficient bytes for the value prefix — the system panics instead of cleanly aborting. This violates the stated design intent and creates a denial-of-service vector in a storage engine.

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` is augmented with `if remaining - value_offset < LENGTH_PREFIX_SIZE { return Err(CorruptedRecord) }` before calling `V::serialized_size_from_bytes`, making the panic path unreachable; or (2) `serialized_size_from_bytes` is changed to return `Result` and handle short buffers gracefully.

#### Suggested Verification

Construct a test page where: key length prefix = `remaining - key_offset - LENGTH_PREFIX_SIZE - 2` (leaves exactly 2 bytes for value). Call `record_size_from_bytes::<Vec<u8>, Vec<u8>>` and verify it returns `Err`, not a panic. Currently, this test would demonstrate a panic.

---

### Finding: `read_header_and_match_key_varlen` requests only `RECORD_HEADER_SIZE` bytes minimum, leaving `eq_from_bytes` to silently handle zero-length key slices during chain walks

**Severity**: should-fix
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

In `hybrid_log/record_ops.rs:548-555`:
```rust
pub fn read_header_and_match_key_varlen<K: Key>(
    &self, addr: LogicalAddress, key: &K,
) -> Option<(RecordInfo, bool)> {
    let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
    let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
    let ri = accessor.record_info();
    const KEY_OFFSET: usize = 8;
    let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
    Some((ri, key_matches))
}
```

`safe_record_size(addr, RECORD_HEADER_SIZE as u32)` (line 38-42):
```rust
fn safe_record_size(addr: LogicalAddress, min_size: u32) -> u32 {
    let remaining = page_size.saturating_sub(offset);
    remaining.max(min_size)
}
```

This returns `max(remaining, 8)`. In normal operation, `remaining` covers the full record. But the function name suggests "safe" minimum, while the `min_size` parameter is actually a *floor* — the function returns the larger of remaining and min_size. If a record address somehow pointed near a page boundary with 6 bytes remaining, this returns 8 (more than available), which could cause `RecordAccessor::from_log` to access memory beyond the page.

**Attack vector**: A developer adding a new record type with a larger header (say 16 bytes) might reasonably change the `RECORD_HEADER_SIZE` constant. The `read_header_and_match_key_varlen` call with `RECORD_HEADER_SIZE as u32` now requests 16 bytes minimum, but `safe_record_size` still returns `max(remaining, 16)`. If remaining < 16, the function returns 16 and the accessor reads 16 bytes from 6 bytes of page data.

#### Warrant (Rule)

The function `safe_record_size` has a misleading name and API: it sounds like it returns a "safe" size but actually returns the *maximum* of remaining and min_size, which can exceed available page space. A developer changing the chain walk logic who sees `safe_record_size` might trust it to return a bounded value. The reliance on the allocator guarantee ("records never span pages") means the min_size path is never taken in practice — but if it ever were, it would cause undefined behavior.

#### Rebuttal Conditions

This is NOT a concern if: (1) `safe_record_size` is guaranteed to never be called when `remaining < min_size` because the allocator invariant ensures all valid record addresses have sufficient remaining bytes; and (2) the function is only called for addresses extracted from valid chain pointers (which always point to valid records).

#### Suggested Verification

Add a `debug_assert!(remaining >= min_size)` inside `safe_record_size` to catch violations of the allocator invariant. Alternatively, rename to `record_size_with_floor` to clarify semantics.

---

### Finding: `record_size as u32` casts in the scanner and address updater silently truncate on 64-bit platforms for records > 4 GiB

**Severity**: consider
**Confidence**: MEDIUM
**Category**: maintainability

#### Grounds (Evidence)

Multiple `record_size as u32` casts appear in the diff:
- `scanner.rs:229` (diff line 2938): `advance(&mut current, record_size as u32, page_size);`
- `address_update.rs:136` (diff line 2001): `mapping.record_size as u32`
- `address_update.rs:219` (diff line 2034): `record_size as u32`

The `advance` function (scanner.rs:315) has a debug_assert: `debug_assert!(step <= page_size)`. But if `record_size` exceeds `u32::MAX`, the cast truncates silently and the assert checks the truncated value.

**Attack vector**: A developer adding support for very large values (e.g., 5 GiB blobs) might increase page size to accommodate them. The `record_size` field is `usize` (correct), but every place it's used for page navigation casts to `u32`. The page_size is `u32` (from `1u32 << OFFSET_BITS`), so records can't exceed u32::MAX in the current architecture — but the type system doesn't enforce this, and a "reasonable" change to increase OFFSET_BITS to 33 would overflow silently.

#### Warrant (Rule)

When a narrowing cast (`usize` → `u32`) appears repeatedly without a validation function, each cast site is a potential truncation bug. The debug_assert catches overflow in development but not in release. A pattern of `x as u32` where `x: usize` is exactly the kind of code where a reasonable change (larger pages) creates a silent truncation that causes an infinite loop in `advance()` (the step wraps to a small number and the scanner never makes progress).

#### Rebuttal Conditions

This is NOT a concern if: (1) `OFFSET_BITS` is architecturally limited to ≤ 32 and this is enforced by a `const_assert!`; (2) page sizes are guaranteed to be ≤ 2^31 by the allocator design, making records always fit in u32.

#### Suggested Verification

Add `debug_assert!(record_size <= u32::MAX as usize)` before every `record_size as u32` cast, or introduce a helper: `fn record_size_u32(size: usize) -> u32 { u32::try_from(size).expect("record_size exceeds u32") }`.

---

### Finding: `eq_from_bytes` for `Vec<u8>` compares raw bytes without validating the length prefix against the available buffer, enabling a false-positive match on truncated data

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: maintainability

#### Grounds (Evidence)

`Vec<u8>::eq_from_bytes` in traits.rs (diff line ~3888-3895):
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    if buf.len() < LENGTH_PREFIX_SIZE {
        return false;
    }
    let len = u32::from_le_bytes(buf[..LENGTH_PREFIX_SIZE].try_into().unwrap()) as usize;
    len == self.len()
        && buf.len() >= LENGTH_PREFIX_SIZE + len
        && buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len] == self[..]
}
```

The `buf.len() >= LENGTH_PREFIX_SIZE + len` check is correct and prevents out-of-bounds access. However, the function trusts that `buf` contains a properly serialized key. If `buf` is a slice extending to the end of a page and the bytes after the actual key happen to match (e.g., because a previous record's data is still in the page), the comparison succeeds — but the "key" being compared is actually garbage data from a different record.

**Attack vector**: A developer could introduce a "performance optimization" that reduces the accessor size in chain walking (e.g., requesting only enough bytes for the expected key size). If the accessor returns a buffer that overlaps with adjacent record data, `eq_from_bytes` could match on data that spans two records. This is unlikely with the current `safe_record_size` returning full remaining bytes, but reducing that size is a tempting "optimization."

#### Warrant (Rule)

`eq_from_bytes` operates on raw byte slices with no structural validation — it trusts the caller to provide a correctly-bounded buffer. In a chain walk, the buffer comes from `accessor.as_slice()[KEY_OFFSET..]`, which extends to the end of the accessor's window. For same-page records, this window includes data beyond the current key. The comparison is still correct because the length prefix bounds the comparison, but a maintainer who changes the accessor size or the slice bounds could accidentally widen or narrow the comparison window.

#### Rebuttal Conditions

This is NOT a concern if: (1) `eq_from_bytes` is always called with a buffer that starts exactly at the key's serialized bytes (which it does currently — `KEY_OFFSET` is correct), and (2) the length prefix in the buffer is always written by a trusted serializer (never corrupted user data).

#### Suggested Verification

Add a test where `eq_from_bytes` is called with extra trailing bytes beyond the key to verify it doesn't match a different key that happens to share a prefix. (The current implementation handles this correctly via the length prefix check, but the test documents the contract.)

---

### Finding: The scanner's `scan()` now returns `Result<CompactionPlan, RecordSizeError>` but the `RecordSizeError` type lacks the page address context needed for debugging

**Severity**: consider
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

`RecordSizeError` in `layout.rs:261-270`:
```rust
pub enum RecordSizeError {
    InsufficientPageSpace,
    CorruptedRecord { offset: usize },
}
```

The `offset` is the byte offset *within the page*, but doesn't include:
- The page number (which page was corrupted?)
- The `LogicalAddress` of the record
- The key/value type names involved

The `CompactionError::ScanCorruption` wrapper in `orchestrator.rs:80` adds no additional context:
```rust
ScanCorruption(RecordSizeError),
```

The Display impl formats as: `"scan detected corrupted record: corrupted record at page offset 1234"`.

#### Warrant (Rule)

At 2 AM, you get: `"scan detected corrupted record: corrupted record at page offset 1234"`. Which page? Page 0? Page 57? Out of how many pages? The scanner knows the `LogicalAddress` (`current`) when the error occurs, which contains both the page number and offset. But this information is discarded when `record_size_from_bytes` returns the error — the scanner propagates it as-is. A future maintainer investigating corruption would need to add temporary logging or a debugger to find which page is corrupted.

#### Rebuttal Conditions

This is NOT a concern if: (1) the caller adds logging before propagating the error (e.g., `tracing::error!("corruption at {current}")`); or (2) corruption is expected to be extremely rare and the page offset alone is sufficient to investigate with a hex dump of the store.

#### Suggested Verification

Consider enriching `RecordSizeError::CorruptedRecord` to include the `LogicalAddress`, or have the scanner wrap the error with additional context before returning. For example:
```rust
Err(e @ RecordSizeError::CorruptedRecord { .. }) => {
    return Err(RecordSizeError::CorruptedRecord {
        offset: current.raw() as usize, // full logical address
    });
}
```
