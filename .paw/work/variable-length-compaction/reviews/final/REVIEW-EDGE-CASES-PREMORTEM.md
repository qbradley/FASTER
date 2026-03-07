# Edge Cases Review — Premortem Perspective

**Perspective**: premortem
**Reviewer**: Edge Cases Specialist
**Scope**: Variable-length record compaction for FASTER (~3,400 lines across 21 files)
**Date**: 2025-07-15

---

## Scenario

Six months after deploying variable-length compaction, a production database instance running with `Vec<u8>` keys and values experiences a compaction crash. The process panics during the scan phase, leaving the compaction lock held until the process restarts. Post-mortem analysis reveals the failure was triggered by a specific combination of record sizes and page alignment.

---

## Boundary Categories Checked

For each critical function in the diff, I systematically checked:

| Category | `record_size_from_bytes` | `serialized_size_from_bytes` | `eq_from_bytes` | Scanner loop | `read_header_and_match_key_varlen` |
|---|---|---|---|---|---|
| Null/empty input | ✅ MIN_READABLE guard | ⚠️ panics on short buf | ✅ Vec/String guard, ❌ numeric | ✅ skips empty pages | ✅ returns None |
| Zero-length key/value | ✅ tested | ✅ returns PREFIX_SIZE | ✅ tested | ✅ handled | ✅ handled |
| Maximum values | ✅ CorruptedRecord | ⚠️ 32-bit overflow | N/A | ✅ propagates error | N/A |
| Boundary exact-fit | ❌ **value_offset == remaining** | N/A | N/A | ❌ inherits bug | N/A |
| Concurrent access | N/A (pure function) | N/A | N/A | ✅ epoch-protected | ✅ epoch-protected |
| Interrupted operation | N/A | N/A | N/A | ✅ abort-safe | N/A |

---

## Findings

### Finding: `record_size_from_bytes` panics when corrupted key length makes `value_offset` exactly equal `remaining`

**Severity**: must-fix
**Confidence**: HIGH
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/layout.rs:332-338`:

```rust
// Validate that the key didn't push us past the page boundary.
if value_offset > remaining {                          // line 333
    return Err(RecordSizeError::CorruptedRecord { offset });
}

let value_buf = &page_bytes[offset + value_offset..];  // line 337
let value_size = V::serialized_size_from_bytes(value_buf); // line 338
```

The guard uses strict `>`, allowing `value_offset == remaining` to pass. When this happens, `value_buf` is an **empty slice**. For variable-length types (`Vec<u8>`, `String`), `serialized_size_from_bytes` (traits.rs:320-324) calls `buf[..LENGTH_PREFIX_SIZE].try_into().expect(...)` on the empty slice, which **panics**.

**Triggering condition**: A corrupted key length prefix of exactly `remaining - 12` bytes (when `remaining` is 8-aligned) produces `key_size = remaining - 8`, then `value_offset = pad_alignment(8 + remaining - 8, 8) = remaining`. Concrete example:

- Page has `remaining = 24` bytes at current record offset
- Corrupted key length prefix = `12` (stored as u32 LE at byte offset 8)
- `key_size = 4 + 12 = 16`, `value_offset = pad_alignment(8 + 16, 8) = 24 = remaining`
- `value_buf = &page_bytes[24..]` → empty slice → **panic in `expect()`**

The scanner (scanner.rs:188) calls `record_size_from_bytes::<K, V>(bytes, 0, remaining as usize)` where `bytes` has exactly `remaining` bytes, confirming the empty-slice path is reachable.

#### Warrant (Rule)

Every input boundary must be explicitly handled. The function already validates `remaining < MIN_READABLE` (line 323) and `value_offset > remaining` (line 333), but the equality case `value_offset == remaining` leaves zero bytes for value size discovery. Since variable-length types require at least `LENGTH_PREFIX_SIZE` (4) bytes to determine their size, the guard should ensure those bytes are available. A panic in the compaction scanner crashes the compaction thread while potentially holding `compaction_lock` (kv.rs), requiring process restart.

#### Rebuttal Conditions

This is NOT a concern if: (1) all records in the log are guaranteed to be well-formed (no corruption possible), making `value_offset == remaining` unreachable; or (2) the corrupted key length can never be exactly `remaining - 12`. Neither holds — the entire purpose of the corruption checks in this function is to handle corrupted data gracefully, and the triggering value is a single specific integer among many possible corrupted values.

#### Suggested Verification

Change line 333 from `value_offset > remaining` to `value_offset + LENGTH_PREFIX_SIZE > remaining`:

```rust
if value_offset + LENGTH_PREFIX_SIZE > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}
```

This is conservative for fixed-size types (they ignore the buffer) but maintains the type-agnostic philosophy established by `MIN_READABLE`. Add a test:

```rust
#[test]
fn record_size_from_bytes_value_offset_equals_remaining() {
    let remaining = 24usize; // 8-aligned
    let mut page = vec![0u8; remaining];
    // Set key length prefix to exactly remaining - 12 = 12
    let key_len: u32 = (remaining - 12) as u32;
    page[8..12].copy_from_slice(&key_len.to_le_bytes());
    let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, remaining);
    assert!(result.is_err(), "should detect corruption, not panic");
}
```

---

### Finding: Numeric `eq_from_bytes` has no buffer length check — panics on short buffers

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs:246-248` (via `impl_key_value_for_numeric!` macro):

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

This performs **no length check** on `buf`. For `u64`, it accesses `buf[..8]`. If `buf` has fewer than 8 bytes, this panics with an index-out-of-bounds error.

Contrast with `Vec<u8>::eq_from_bytes` (traits.rs:330-331) which defensively checks `buf.len() < LENGTH_PREFIX_SIZE` before accessing the buffer.

The call path: `read_header_and_match_key_varlen` (record_ops.rs:550-555) calls `safe_record_size(addr, RECORD_HEADER_SIZE as u32)` which returns `max(remaining, 8)`. When a version chain's `previous_address` points to a location with `remaining = 8`, the accessor has exactly 8 bytes. After slicing at `KEY_OFFSET = 8`, the buffer passed to `eq_from_bytes` has **0 bytes** → panic for any numeric key type.

#### Warrant (Rule)

The `eq_from_bytes` contract (traits.rs:140-143) states `buf` contains at least `serialized_size_from_bytes(buf)` bytes, but the contract is only a documentation promise — it's not enforced by the type system. During version chain walks, the chain may traverse to records near page boundaries. If a `previous_address` pointer is corrupt or points to page-end padding, `eq_from_bytes` for numeric types panics instead of returning `false`.

The Vec<u8> and String implementations already demonstrate the correct defensive pattern (`if buf.len() < LENGTH_PREFIX_SIZE { return false; }`). The numeric implementations should follow the same pattern.

#### Rebuttal Conditions

This is NOT a concern if: (1) `safe_record_size` always returns a value ≥ `KEY_OFFSET + key_size` for valid records, AND (2) `previous_address` pointers in the version chain can never point to invalid locations. Condition (1) holds for valid records but not for corrupt data. Condition (2) depends on data integrity which is exactly what compaction should be robust against.

#### Suggested Verification

Add a length guard to the numeric `eq_from_bytes` macro implementation:

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

Add a test for short-buffer behavior:

```rust
#[test]
fn eq_from_bytes_u64_short_buf() {
    let key: u64 = 42;
    let buf = vec![0u8; 3]; // shorter than 8
    assert!(!key.eq_from_bytes(&buf));
}
```

---

### Finding: `serialized_size_from_bytes` for `Vec<u8>` panics instead of returning an error on short buffers

**Severity**: should-fix
**Confidence**: HIGH
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs:320-327`:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(
        buf[..LENGTH_PREFIX_SIZE]
            .try_into()
            .expect("buffer too short for Vec<u8> length prefix"),  // PANIC
    ) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

The identical pattern appears for `String` (traits.rs:397-403) and `Value for Vec<u8>` (traits.rs:363-368) and `Value for String` (traits.rs:443-448). All six variable-length `serialized_size_from_bytes` implementations use `.expect()` which panics on short buffers.

While `record_size_from_bytes` guards the key buffer with `MIN_READABLE >= 12` (ensuring at least 4 bytes for the key prefix), the value buffer guard at line 333 allows an empty buffer to reach `V::serialized_size_from_bytes` (as described in Finding 1). These panics are the mechanism of Finding 1.

#### Warrant (Rule)

Functions called on untrusted data (raw page bytes, potentially corrupted) should return errors rather than panic. The `serialized_size_from_bytes` trait method's default implementation already has this issue (it calls `deserialize` which may panic), but the overridden implementations for built-in types should be defensive since they're explicitly designed for compaction-time use on raw bytes.

The `eq_from_bytes` implementations for `Vec<u8>` and `String` already demonstrate the correct pattern: check `buf.len() < LENGTH_PREFIX_SIZE` before accessing. The `serialized_size_from_bytes` implementations should follow suit.

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` is the **only** caller and Finding 1 is fixed (the value buffer would always have ≥ 4 bytes). In that case, the panics are unreachable from the compaction path. However, `serialized_size_from_bytes` is a `pub` trait method — external callers could invoke it with arbitrary buffers.

#### Suggested Verification

Consider returning the size via `Result` or at minimum adding a bounds check. Since changing the trait signature would be a larger refactor, the pragmatic fix is to guard within the implementation:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    assert!(buf.len() >= LENGTH_PREFIX_SIZE, "buffer too short for length prefix");
    let len = u32::from_le_bytes(buf[..LENGTH_PREFIX_SIZE].try_into().unwrap()) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

Or better yet, fix Finding 1 so the caller never passes a short buffer, and document the precondition.

---

### Finding: `usize` overflow in `serialized_size_from_bytes` on 32-bit platforms silently produces a small size

**Severity**: consider
**Confidence**: LOW
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs:320-327`:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(buf[..LENGTH_PREFIX_SIZE].try_into()...) as usize;
    LENGTH_PREFIX_SIZE + len  // ← wrapping addition on 32-bit if len is large
}
```

On a 32-bit platform (`usize` = 32 bits), a corrupted length prefix of `u32::MAX` (0xFFFFFFFF) produces `len = 4294967295`. Then `LENGTH_PREFIX_SIZE + len = 4 + 4294967295 = 4294967299` which wraps to `3` on 32-bit. This causes `key_size = 3`, `value_offset = pad_alignment(8 + 3, 8) = 16`, and the corruption check `value_offset > remaining` may pass — interpreting garbage bytes as a valid record.

On 64-bit platforms (the likely deployment target), `LENGTH_PREFIX_SIZE + len = 4294967299` which is huge and caught by the subsequent `> remaining` check.

#### Warrant (Rule)

Arithmetic on values derived from untrusted input should use checked or saturating arithmetic to prevent silent wraparound. While FASTER likely targets 64-bit servers, the code compiles for 32-bit targets and contains no `#[cfg]` guards or compile-time assertions to prevent 32-bit use.

#### Rebuttal Conditions

This is NOT a concern if: (1) FASTER explicitly documents or enforces 64-bit-only support; or (2) `Cargo.toml` or CI configuration restricts the target to 64-bit architectures. A `static_assert!(std::mem::size_of::<usize>() >= 8)` at crate level would be sufficient.

#### Suggested Verification

Add a compile-time assertion in `lib.rs` or `record/mod.rs`:

```rust
const _: () = assert!(
    core::mem::size_of::<usize>() >= 8,
    "FASTER requires a 64-bit platform"
);
```

Or use saturating arithmetic: `LENGTH_PREFIX_SIZE.saturating_add(len)`.

---

### Finding: Scanner deserializes full key for non-tombstone/non-invalid records even when the record might be dead

**Severity**: consider
**Confidence**: MEDIUM
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/compaction/scanner.rs:220`:

```rust
let key: K = K::deserialize(&bytes[KEY_OFFSET..]);
if self.is_current_version(&key, current) {
```

For every record that is neither a tombstone nor invalid, the scanner fully deserializes the key (heap allocation for `Vec<u8>` and `String`) before checking whether it's the current version. For workloads with high dead-record ratios (e.g., update-heavy with many superseded versions), most deserialized keys are immediately discarded.

The `is_current_version` method internally calls `read_header_and_match_key_varlen` which uses `eq_from_bytes` for zero-copy comparison. But the initial key deserialization at the scan site is a full allocation.

#### Warrant (Rule)

In a compaction cycle scanning millions of records where 80%+ are dead (common in update-heavy workloads), this means millions of unnecessary heap allocations. The key is needed for hashing (`key.hash()`) and for `eq_from_bytes` comparison, but both could potentially be computed from raw bytes for types that provide efficient implementations.

#### Rebuttal Conditions

This is NOT a concern if: (1) the `K::deserialize` cost is negligible compared to the hash index lookup and page access costs; or (2) a profiling benchmark shows deserialization is not a bottleneck. For fixed-size types (u64), deserialization is a stack copy with no allocation — this only matters for variable-length types.

#### Suggested Verification

Profile compaction with `Vec<u8>` keys under a high dead-record ratio. If allocation overhead is significant, consider adding a `hash_from_bytes` trait method or computing the hash from the key buffer directly.
