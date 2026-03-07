# Correctness Review — Red-Team Perspective

**Perspective**: red-team
**Specialist**: correctness
**Scope**: `git diff squad..feature/variable-length-compaction` (~4,702 lines, 21 files)

---

## Attack Surface Assessment

The variable-length compaction change introduces a new code path that reads **untrusted data** (length prefixes from raw log pages) and uses those values to control memory access patterns, loop advancement, and hash index mutations. The trust boundary is the on-disk/in-memory log content — if an adversary can corrupt log pages (via storage-layer attacks, bit flips, or exploiting write-path bugs), they can influence compaction behavior.

Key attack surfaces:
1. **Length prefix values** — control record size computation, slice offsets, and loop stride
2. **Record header `previous_address` fields** — control version chain traversal destinations
3. **Tombstone volume** — control memory allocation during scanning
4. **Key content** — control hash computation and hash index CAS operations

---

### Finding: Crafted key length prefix creates permanent compaction denial-of-service via deterministic panic

**Severity**: must-fix
**Confidence**: HIGH
**Category**: correctness

#### Grounds (Evidence)

An attacker who can write a single corrupted byte sequence at a specific log offset can make compaction permanently unrecoverable for that page region.

**Attack vector**: Write a record whose key length prefix is crafted so that `value_offset == remaining` (i.e., the key "fills" the page exactly). The specific value depends on page layout:

1. Record is placed at page offset such that `remaining = R` (known from page size)
2. Set key length prefix to `L` where `pad_alignment(8 + (4 + L), 8) == R`
3. For R=200: `pad_alignment(12 + L, 8) = 200` → `12 + L = 200` → `L = 188`
4. Write 188 bytes as key data. This is a "valid" record as far as the allocator is concerned if it happened before corruption.

Now corrupt just the key length prefix from `L=3` (original) to `L=188`. In `record_size_from_bytes` (layout.rs:327-343):

```rust
let key_size = K::serialized_size_from_bytes(key_buf);  // returns 4 + 188 = 192
let value_offset = pad_alignment(8 + 192, 8);           // = 200
if value_offset > remaining { ... }                      // 200 > 200 = false, passes!
let value_buf = &page_bytes[offset + 200..];             // empty slice: 0 bytes
let value_size = V::serialized_size_from_bytes(value_buf); // PANICS: .expect() on empty slice
```

The panic occurs inside `Vec<u8>::serialized_size_from_bytes` (traits.rs:320-327) which calls `.expect("buffer too short for Vec<u8> length prefix")` on a 0-byte slice.

**Persistence**: Every subsequent compaction attempt reads the same page, hits the same record, and panics identically. The corruption is in a single 4-byte field. The result is permanent compaction denial until manual intervention.

**Blast radius**: The compaction mutex is held at the time of panic. If the panic is caught at a higher level (e.g., `catch_unwind` in the application), the mutex is poisoned, preventing all future compaction attempts even on other regions.

#### Warrant (Rule)

The spec (MF-3, T-2) explicitly chose "abort compaction" as the corruption response — meaning return `Err`, not panic. The implementation correctly returns `Err` for most corruption cases but has a gap at the exact boundary where `value_offset == remaining`. An adversary who understands the alignment arithmetic can craft a prefix that targets this exact gap. The attack requires flipping a single 4-byte field and produces a permanent, self-reinforcing failure.

#### Rebuttal Conditions

This is NOT exploitable if:
1. The alignment arithmetic guarantees `remaining - value_offset >= 8` whenever `value_offset <= remaining` — partially true due to 8-byte alignment (see premortem Finding 1 rebuttal). If both `value_offset` and `remaining` are multiples of 8, the gap is either 0 or ≥ 8. The gap of exactly 0 is the attack target.
2. The corrupted prefix is detected before `record_size_from_bytes` — no such check exists.
3. `std::panic::catch_unwind` wraps the compaction call and the mutex is not poisoned — this is application-dependent and the FASTER library provides no such wrapper.

**Even with the alignment rebuttal**, the 0-byte gap case is a real trigger because alignment makes `value_offset == remaining` more likely, not less — both values snap to multiples of 8.

#### Suggested Verification

Change the bounds check in `record_size_from_bytes` from `value_offset > remaining` to `value_offset >= remaining` (or `value_offset + LENGTH_PREFIX_SIZE > remaining` for variable-length types). This eliminates the 0-byte gap.

Regression test: inject a key prefix that makes `value_offset == remaining`, assert `Err(CorruptedRecord)`.

---

### Finding: Corrupted `previous_address` in record header enables panic via unchecked `eq_from_bytes` on numeric types during chain walk

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: correctness

#### Grounds (Evidence)

**Attack vector**: An attacker corrupts the `previous_address` field (bits 0-47 of the RecordInfo header) of a live record to point to an address near a page boundary.

In `record_ops.rs:544-556`, `read_header_and_match_key_varlen`:
```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
const KEY_OFFSET: usize = 8;
let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
```

`safe_record_size(addr, 8)` returns `max(remaining, 8)`. If the corrupted `previous_address` points to page offset 65528 (on a 65536-byte page), `remaining = 8`, `record_size = 8`, `accessor.as_slice()` has 8 bytes, `accessor.as_slice()[8..]` is an **empty slice**.

For `u64::eq_from_bytes` (traits.rs, macro expansion):
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..8] == self.to_le_bytes()  // panics: buf is empty
}
```

**Chain of exploitation**:
1. Attacker corrupts one record's `previous_address` (6 bytes within the 8-byte header)
2. Scanner scans the page, reaches the record, calls `is_current_version`
3. `is_current_version` follows the hash chain, reaching the corrupted record
4. Chain walk calls `read_header_and_match_key_varlen` with the corrupted address
5. `eq_from_bytes` on the near-page-boundary accessor panics

This is a **one-hop** attack: corrupt one field, get a panic in the compaction scanner. The `MAX_CHAIN_DEPTH` limit doesn't help because the panic occurs on the first corrupted hop.

#### Warrant (Rule)

The `previous_address` field is stored in the first 48 bits of the RecordInfo header and is read without validation beyond "is_valid()". A corrupted address that passes `is_valid()` but points near a page boundary creates a different attack surface than the key-prefix corruption above. The Vec<u8> `eq_from_bytes` is resilient (checks `buf.len() < 4`), but numeric types are not. This asymmetry means stores using `u64` keys are more vulnerable than stores using `Vec<u8>` keys — the opposite of what might be expected.

#### Rebuttal Conditions

This is NOT exploitable if:
1. The allocator never places records at offsets where `remaining < 16` — likely true for normal operation (minimum record size for u64/u64 is 24). But a corrupted `previous_address` can point *anywhere* on the page, including offsets that were never used for record placement.
2. `from_log` returns `None` for addresses not backed by valid pages — it only checks if the page frame is allocated, not if the specific offset is valid.

#### Suggested Verification

Add bounds checks to all numeric `eq_from_bytes` implementations:
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

Test: create a mock accessor with 10 bytes, call `read_header_and_match_key_varlen` for u64, verify no panic.

---

### Finding: Unbounded tombstone vector enables memory exhaustion attack

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: correctness

#### Grounds (Evidence)

In `scanner.rs:182-189`, every tombstone is unconditionally pushed to `plan.tombstone_records`:

```rust
if info.is_tombstone() {
    plan.tombstone_count += 1;
    plan.tombstone_records.push(LiveRecord {
        address: current,
        record_size,
    });
}
```

`LiveRecord` is approximately 24 bytes (8 bytes `LogicalAddress` + 8 bytes `record_size` + padding). An adversary with write access can flood the store with deletes, creating tombstone-heavy pages. When compaction scans these pages:

- 10 million tombstones → 240 MB of `Vec<LiveRecord>` allocation
- 100 million tombstones → 2.4 GB
- The allocator will realloc and copy the Vec multiple times as it grows

The planning review (C-3) identified this and recommended "log warning if > 100MB." The implementation has **no such warning or cap**. The `tombstone_records` vector grows without bound during a single `scan()` call, which holds the epoch guard the entire time.

**Chained attack**: Memory pressure from tombstone allocation can cause the system allocator to fail, panicking in `.push()`. Combined with the epoch guard being held, this causes an epoch leak, preventing page eviction, compounding the memory pressure.

#### Warrant (Rule)

The tombstone vector is allocated proportionally to attacker-controlled input (number of deletes). The planning review identified this risk but the mitigation (logging + cap) was not implemented. A patient attacker who can issue deletes over time can accumulate an arbitrary number of tombstones, then trigger compaction to exhaust memory.

#### Rebuttal Conditions

Not a concern if:
1. The compaction region is bounded by `safe_read_only_address - begin_address`, which limits how many records are scanned. For a 64KB page with 24-byte records, that's ~2,730 records per page. With 1,000 pages in the compaction region, ~2.7M records → 64 MB. Reasonable.
2. The application limits the compaction region size before calling `compact()` — the current API compacts the full region without size limits.

The rebuttal depends on the compaction region size. For stores with large read-only regions (many pages), tombstone memory is unbounded.

#### Suggested Verification

Add a configurable tombstone cap (e.g., 1M records) with an error return when exceeded. Or implement the Option B strategy from the planning review (re-read tombstones from old addresses during Phase 3 instead of collecting them).

At minimum, add the planned logging:
```rust
if plan.tombstone_records.len() * std::mem::size_of::<LiveRecord>() > 100_000_000 {
    log::warn!("Tombstone collection exceeds 100MB during compaction scan");
}
```

---

### Finding: `serialized_size_from_bytes` return value is trusted without overflow protection on 32-bit targets

**Severity**: consider
**Confidence**: LOW
**Category**: correctness

#### Grounds (Evidence)

In `traits.rs:320-327`, `Vec<u8>::serialized_size_from_bytes`:
```rust
let len = u32::from_le_bytes(...) as usize;
LENGTH_PREFIX_SIZE + len
```

On 32-bit platforms, `usize` is 32 bits. If `len = u32::MAX` (0xFFFFFFFF = 4,294,967,295):
- `LENGTH_PREFIX_SIZE + len` = `4 + 4_294_967_295` = overflow
- In debug mode: panic
- In release mode: wraps to `3`

Then in `record_size_from_bytes`:
```rust
let key_size = 3;  // wrapped
let value_offset = pad_alignment(8 + 3, 8) = 16;
// value_offset (16) <= remaining (say, 200) → passes check
let value_buf = &page_bytes[offset + 16..]; // reads from wrong position
```

The corrupted prefix successfully bypasses all validation and causes the scanner to read a "value" from an arbitrary position on the page. This could lead to:
- An artificially small `record_size` → scanner reads overlapping records
- Misclassification of subsequent records → data loss

**Platform note**: FASTER is primarily deployed on 64-bit systems where `usize` is 64 bits and the overflow doesn't occur. This finding is relevant only if the crate is compiled for 32-bit targets.

#### Warrant (Rule)

Overflow-based bypass of validation logic is a classic exploitation technique. The attacker controls the 4-byte length prefix, the overflow happens in safe Rust (no UB, just wrong arithmetic), and the result is a validation bypass that leads to incorrect record scanning.

#### Rebuttal Conditions

Not a concern if:
1. The crate is never compiled for 32-bit targets. This should be enforced with a compile-time assertion:
   ```rust
   const _: () = assert!(core::mem::size_of::<usize>() >= 8, "FASTER requires 64-bit");
   ```
2. The `len as usize` cast is checked: `usize::try_from(len).ok()?`

#### Suggested Verification

Add a `#[cfg(not(target_pointer_width = "64"))]` compile error or add overflow-checked arithmetic in `serialized_size_from_bytes`.

---

### Finding: Key deserialization in `swing_one` allocates on the heap for every pointer swing — potential timing side-channel for key content

**Severity**: consider
**Confidence**: LOW
**Category**: correctness

#### Grounds (Evidence)

In `address_update.rs:157-158` (new code):
```rust
let key: K = K::deserialize(&accessor.as_slice()[key_offset..]);
let hash = key.hash();
```

For `Vec<u8>` keys, `deserialize` allocates a new `Vec` on the heap. The allocation size depends on the key's content (the length prefix value). An adversary observing compaction timing can infer key sizes by measuring the time taken for different pointer swings.

This is a **weak** side-channel: it reveals key *sizes* (not content) and requires precise timing measurements during compaction. In most threat models this is not actionable, but in environments where key sizes are sensitive metadata (e.g., cryptographic key stores), it could be relevant.

The scanner's `is_current_version` path uses `eq_from_bytes` (zero-copy, no allocation), which is resistant to this side-channel. The asymmetry between scanner (zero-copy) and updater (full deserialization) is by design — the updater needs the key to compute the hash. An alternative would be to add `Key::hash_from_bytes(buf) -> KeyHash` to avoid deserialization entirely.

#### Warrant (Rule)

This is a minor trust boundary issue: the address updater processes key data from copied records (which the compaction pipeline controls), but the timing observable is proportional to key size. In a red-team analysis, side-channels are worth flagging even when exploitation requires impractical conditions.

#### Rebuttal Conditions

Not a concern in any practical deployment. Key sizes are rarely sensitive metadata. The timing difference between allocating 4 bytes vs 1KB is measurable only with nanosecond-precision instrumentation.

#### Suggested Verification

No immediate action needed. If key-size confidentiality becomes a requirement, add `Key::hash_from_bytes(buf: &[u8]) -> KeyHash` as a trait method to avoid heap allocation in the address updater.

---

## Summary

The primary attack vector is **crafted length prefix corruption** that exploits a bounds-check gap in `record_size_from_bytes` where `value_offset == remaining` passes the `>` check but leaves 0 bytes for the value prefix read, causing a deterministic panic. This is a one-shot DoS that permanently blocks compaction. The secondary vector is **corrupted `previous_address`** exploitation of unchecked numeric `eq_from_bytes` during chain walks. Both attacks require write access to the storage layer — either physical disk corruption, a write-path bug, or compromised storage infrastructure. The fixes are minimal: tighten one comparison operator (`>` to `>=`) and add bounds checks to numeric `eq_from_bytes`.
