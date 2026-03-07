# Correctness Review — Premortem Perspective

**Perspective**: premortem
**Specialist**: correctness
**Scope**: `git diff squad..feature/variable-length-compaction` (~4,702 lines, 21 files)

---

## Premortem Scenario

It is 6 months after deployment. A production FASTER store using `Vec<u8>` keys and values experiences a **compaction panic** during routine maintenance. The process aborts with `"buffer too short for Vec<u8> length prefix"` in `serialized_size_from_bytes`. Investigation reveals a single partially-written record on a page boundary — the key prefix was valid but inflated, pushing the value offset to within 3 bytes of the page end. The spec said "corruption returns Err, not panic" — but a gap in the bounds validation between `record_size_from_bytes` and `Vec<u8>::serialized_size_from_bytes` meant the panic was possible. Every compaction retry hits the same page, the same corrupted record, the same panic. The log grows without bound for 10 days before an engineer reads the stack trace carefully enough to understand the gap.

---

### Finding: Corrupted key prefix can cause panic in value `serialized_size_from_bytes` via insufficient remaining bytes

**Severity**: must-fix
**Confidence**: HIGH
**Category**: correctness

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/layout.rs:327-338`, `record_size_from_bytes` validates that `value_offset <= remaining` before reading the value buffer:

```rust
let key_size = K::serialized_size_from_bytes(key_buf);
let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);

if value_offset > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}

let value_buf = &page_bytes[offset + value_offset..];
let value_size = V::serialized_size_from_bytes(value_buf);
```

The check ensures `value_offset <= remaining`, meaning `value_buf.len() = remaining - value_offset` could be as small as **0 bytes**. For `Vec<u8>::serialized_size_from_bytes` (traits.rs:320-327):

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(
        buf[..LENGTH_PREFIX_SIZE]
            .try_into()
            .expect("buffer too short for Vec<u8> length prefix"),
    ) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

This `.expect()` panics if `buf.len() < 4`. Concrete trigger: a corrupted key prefix that yields `key_size = remaining - 8 - 2` (e.g., page has 200 bytes remaining, corrupted key prefix claims length 186, so `key_size = 4 + 186 = 190`, `value_offset = pad_alignment(8 + 190, 8) = 200`, check passes since `200 <= 200`, then `value_buf.len() = 200 - 200 = 0`). The `.expect()` fires.

More precisely, any `value_offset` in the range `(remaining - 3)..=remaining` passes the `<=` check but leaves fewer than 4 bytes for the value length prefix.

#### Warrant (Rule)

The specification (MF-3 from planning review, Spec.md line ~781) requires: "corrupted prefix returns Err, not panic." The `record_size_from_bytes` function was designed as the corruption boundary — all validation happens here. But the check `value_offset > remaining` is necessary but **not sufficient** — it should also verify that at least `LENGTH_PREFIX_SIZE` bytes remain after `value_offset` for variable-length value types. The panic in `serialized_size_from_bytes` violates the spec's corruption safety contract.

This is a specification-implementation mismatch in the validation boundary: the spec says Err, the implementation panics for a specific class of corrupted prefixes.

#### Rebuttal Conditions

This is NOT a concern if:
1. `value_offset` is always a multiple of `RECORD_ALIGNMENT` (8), meaning the smallest possible gap between `value_offset` and `remaining` is 8 bytes (≥ 4 needed). Let me check: `pad_alignment(key_offset + key_size, 8)` rounds up to the next multiple of 8. So `value_offset` is always a multiple of 8. And `remaining = page_size - offset`. If page_size is a power of 2 (typically 65536) and offset is always a multiple of 8, then `remaining` is a multiple of 8. So `remaining - value_offset` is always a multiple of 8, meaning the minimum gap is 0 or 8.

**Partial rebuttal**: If remaining is always a multiple of 8 AND value_offset is always a multiple of 8, then `remaining - value_offset` is either 0 or ≥ 8. When it's 0, `value_buf.len() = 0` → panic. When it's 8, `value_buf.len() = 8` → the 4-byte read succeeds. So the panic is only triggered when `value_offset == remaining` exactly. The `value_offset > remaining` check misses this case because `==` passes.

**The rebuttal narrows but does not eliminate the bug**: `value_offset == remaining` (0 bytes for value) is the exact trigger. The check should be `value_offset >= remaining` (strict) or `value_offset + LENGTH_PREFIX_SIZE > remaining`.

#### Suggested Verification

Change the check in `record_size_from_bytes` to:
```rust
if value_offset + LENGTH_PREFIX_SIZE > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}
```
For fixed-size types where `serialized_size_from_bytes` ignores the buffer, this is slightly more conservative than needed, but correctness > micro-optimization.

Test: inject a key prefix that makes `value_offset == remaining`, verify Err (not panic).

---

### Finding: Numeric `eq_from_bytes` implementations perform unchecked slice indexing, risking panic on corrupted chain pointers

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: correctness

#### Grounds (Evidence)

In `traits.rs` (macro `impl_key_value_for_numeric`), the `eq_from_bytes` implementation for u64, u32, i64, i32:

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

For u64, this indexes `buf[..8]`. There is no bounds check — if `buf.len() < 8`, this panics.

In `record_ops.rs:544-556`, `read_header_and_match_key_varlen`:

```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
// ...
const KEY_OFFSET: usize = 8;
let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
```

`safe_record_size` (line 38-42) returns `remaining.max(min_size)` where `min_size = 8`. If `remaining >= 8`, the accessor has `remaining` bytes. `accessor.as_slice()[8..]` has `remaining - 8` bytes. For u64, `eq_from_bytes` needs 8 bytes, requiring `remaining >= 16`.

For valid records this invariant holds (allocator guarantees records fit on pages, minimum record = 24 bytes for u64/u64). But during version chain walks, a **corrupted `previous_address`** in a record header could point to a location near a page end where `remaining` is between 8 and 15. In that case, the u64 `eq_from_bytes` panics.

Contrast with `Vec<u8>::eq_from_bytes` which checks `buf.len() < LENGTH_PREFIX_SIZE` and returns `false` gracefully.

#### Warrant (Rule)

The version chain walk is reachable with arbitrary addresses (stored in record headers). A corrupted `previous_address` is indistinguishable from a valid one without reading and validating the target. The existing `MAX_CHAIN_DEPTH` limit prevents infinite loops but doesn't prevent following a corrupted pointer to a near-page-boundary location. Every other corruption path in this diff returns Err or aborts; this one panics.

#### Rebuttal Conditions

This is NOT a concern if:
1. `safe_record_size` always returns values ≥ 16 for valid page frames — but it returns `remaining.max(8)`, which could be 8-15 for addresses near page ends.
2. The allocator never places records where `remaining < MIN_RECORD_SIZE` — true for records placed by the allocator, but not guaranteed for corrupted pointers.
3. Chain walks only follow addresses that point to valid, allocator-placed records — true in a non-corrupted system.

The MEDIUM confidence reflects that triggering this requires data corruption, which is unlikely in normal operation but is exactly what the premortem scenario investigates.

#### Suggested Verification

Add bounds checks to numeric `eq_from_bytes`:
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

Test: construct a `RecordAccessor` with only 10 bytes, call `read_header_and_match_key_varlen` with a u64 key, verify it returns `None` or `Some((_, false))` without panic.

---

### Finding: Default `serialized_size_from_bytes` and `eq_from_bytes` trait implementations will panic on corrupted buffers for any custom Key/Value type

**Severity**: should-fix
**Confidence**: HIGH
**Category**: correctness

#### Grounds (Evidence)

The default implementations in `traits.rs:131-133` and `traits.rs:147-149`:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}

fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    *self == Self::deserialize(buf)
}
```

Both delegate to `Self::deserialize(buf)`. For every built-in type, `deserialize` panics on insufficient or corrupted input (e.g., Vec<u8>::deserialize indexes `buf[4..4+len]` without bounds checking the `len` value — a corrupted prefix with `len > buf.len() - 4` causes an out-of-bounds panic).

Any custom type implementing `Key` that does NOT override `serialized_size_from_bytes` will use this default, which calls `deserialize`. If `deserialize` panics on malformed data (which is the common pattern — see all built-in impls), then `serialized_size_from_bytes` panics. The corruption safety contract requires returning `Err` from `record_size_from_bytes`, but the panic bypasses the error path entirely.

The doc comment on `serialized_size_from_bytes` says "The default implementation deserializes fully — override for performance" but does not warn that the default will panic on corrupted data. This is a documentation gap that will lead to surprise panics from user-defined types.

#### Warrant (Rule)

The default path is the most dangerous path — it gets the least attention (correctness specialist lesson 3). Developers who implement `Key` for their custom types will not override `serialized_size_from_bytes` unless they know about the corruption risk. The doc comment frames the override as a performance optimization, not a safety requirement. Six months out, when someone adds a new key type and corruption hits, the system panics.

#### Rebuttal Conditions

This is NOT a concern if:
1. All production Key/Value types override `serialized_size_from_bytes` — but this cannot be enforced at compile time.
2. The `record_size_from_bytes` function wraps the call in `std::panic::catch_unwind` — but it doesn't.
3. Corruption is impossible in practice — but the spec explicitly plans for it.

#### Suggested Verification

Document in the `serialized_size_from_bytes` and `eq_from_bytes` doc comments that the default implementations may panic on corrupted buffers, and that types used in compaction-eligible stores SHOULD override them with corruption-safe implementations. Consider adding a `serialized_size_from_bytes_checked(buf) -> Option<usize>` variant in a future release.

---

### Finding: `record_size_from_bytes` trusts `page_bytes.len() >= page_size` without assertion — caller mismatch could cause slice indexing panic

**Severity**: consider
**Confidence**: MEDIUM
**Category**: correctness

#### Grounds (Evidence)

In `layout.rs:312-316`:
```rust
pub fn record_size_from_bytes<K: Key, V: Value>(
    page_bytes: &[u8],
    offset: usize,
    page_size: usize,
) -> Result<usize, RecordSizeError> {
```

The function uses `page_size` for bounds validation (`remaining = page_size.saturating_sub(offset)`) but indexes into `page_bytes` directly: `&page_bytes[offset + key_offset..]`. If `page_bytes.len() < page_size`, the validation passes but the slice index panics.

In the scanner (scanner.rs), `page_bytes = accessor.as_slice()` which has `remaining` bytes, and `page_size = remaining as usize`. These are consistent. But the function's public API does not enforce `page_bytes.len() >= page_size`, and the function is `pub` — any caller could pass mismatched values.

#### Warrant (Rule)

The mismatch between the validation domain (`page_size`) and the access domain (`page_bytes.len()`) creates a correctness gap. A `debug_assert!(page_bytes.len() >= offset + remaining)` at the function entry would catch this at development time.

#### Rebuttal Conditions

Not a concern if all callers are internal and correct (currently true). But as a `pub` function, external callers could misuse it.

#### Suggested Verification

Add `debug_assert!(page_bytes.len() >= page_size, "page_bytes shorter than page_size")` at the start of `record_size_from_bytes`.

---

### Finding: All tombstone records collected regardless of liveness — no filtering of superseded tombstones

**Severity**: consider
**Confidence**: MEDIUM
**Category**: correctness

#### Grounds (Evidence)

In `scanner.rs:182-189` (new code):
```rust
if info.is_tombstone() {
    plan.tombstone_count += 1;
    plan.tombstone_records.push(LiveRecord {
        address: current,
        record_size,
    });
}
```

Every tombstone is collected, whether it's the current version (head of hash chain) or an old version superseded by a newer operation. In `address_update.rs:169-176`, the address updater attempts to CAS-remove each tombstone:

```rust
for tombstone in &plan.tombstone_records {
    self.remove_tombstone::<K>(
        tombstone.address, tombstone.record_size, KEY_OFFSET, &mut stats,
    );
}
```

For superseded tombstones, the CAS will fail (the hash entry doesn't point to the old tombstone address). This is **functionally correct** — CAS failure is benign. But it means the address updater does wasted work: reading the key, hashing it, finding the hash entry, and attempting a CAS for every superseded tombstone. For workloads with high delete churn (multiple deletes of the same key), this wasted work scales linearly.

#### Warrant (Rule)

This is not a correctness bug — the system produces the right result. But it's an accidental correctness: the code works because CAS failure is a no-op, not because it intentionally filters tombstones. In a premortem scenario, a workload with 10M tombstones (90% superseded) would spend significant time on wasted CAS attempts, making compaction appear slow or hung.

#### Rebuttal Conditions

Not a concern if: tombstone churn is low (each key deleted at most once) or compaction is infrequent enough that the wasted work is negligible.

#### Suggested Verification

Add an `is_current_version` check for tombstones during scanning, or add a stat counter (`stats.tombstone_cas_failed`) to make the wasted work visible in compaction results.

---

## Summary

The most critical finding is the **panic gap** in `record_size_from_bytes` where a corrupted key prefix can leave fewer than `LENGTH_PREFIX_SIZE` bytes for the value prefix read, causing a panic instead of the specified `Err`. This directly contradicts the MF-3 design decision ("corruption → Err, not panic") and creates a permanent compaction failure mode for the affected page. The fix is a one-line change to the bounds check. The numeric `eq_from_bytes` unchecked indexing is a secondary concern that only manifests with corrupted chain pointers but violates the same robustness principle.
