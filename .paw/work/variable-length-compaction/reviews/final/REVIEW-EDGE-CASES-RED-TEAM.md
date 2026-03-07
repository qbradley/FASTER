# Edge Cases Review — Red-Team Perspective

**Perspective**: red-team
**Reviewer**: Edge Cases Specialist
**Scope**: Variable-length record compaction for FASTER (~3,400 lines across 21 files)
**Date**: 2025-07-15

---

## Adversarial Goal

Craft inputs that trigger panics, silent data corruption, or denial-of-service in the compaction pipeline. Focus on malformed records, extreme sizes, and degenerate hash distributions that exploit assumptions in the variable-length scanning and pointer-swing logic.

---

## Attack Surface Enumeration

| Entry Point | Untrusted Input | Trust Boundary |
|---|---|---|
| `record_size_from_bytes` (layout.rs:312) | Raw page bytes (key/value length prefixes) | Disk → memory |
| `serialized_size_from_bytes` (traits.rs:320) | Raw bytes from page buffer | Disk → memory |
| `eq_from_bytes` (traits.rs:246, 330) | Raw bytes from page buffer | Disk → memory |
| `K::deserialize` (scanner.rs:220) | Raw bytes from page buffer | Disk → memory |
| `read_header_and_match_key_varlen` (record_ops.rs:544) | Page bytes via version chain traversal | Disk → memory |
| `previous_address` chain pointers | RecordInfo.previous_address from disk | Disk → memory |

---

## Findings

### Finding: Crafted key length prefix triggers panic in `V::serialized_size_from_bytes` via empty value buffer

**Severity**: must-fix
**Confidence**: HIGH
**Category**: edge-cases

#### Grounds (Evidence)

**Attack**: Write a page (or corrupt on-disk data) so that the key length prefix at byte offset 8 contains the value `remaining - 12` (where `remaining` is 8-byte aligned). This makes `value_offset == remaining`, producing an empty slice for the value buffer.

**Concrete exploit** against `record_size_from_bytes::<Vec<u8>, Vec<u8>>`:

```
Page: 64 bytes, record at offset 40, remaining = 24 (aligned to 8)
Byte 48 (offset 40 + key_offset 8): key length prefix = 12 (u32 LE: 0x0C000000)
→ key_size = 4 + 12 = 16
→ value_offset = pad_alignment(8 + 16, 8) = 24 = remaining
→ value_buf = &page_bytes[64..] = empty slice
→ Vec<u8>::serialized_size_from_bytes(&[]) panics at expect()
```

**Code path** (layout.rs:332-338):
```rust
if value_offset > remaining {     // 24 > 24 is FALSE — guard bypassed
    return Err(RecordSizeError::CorruptedRecord { offset });
}
let value_buf = &page_bytes[offset + value_offset..];  // empty slice!
let value_size = V::serialized_size_from_bytes(value_buf);  // PANIC
```

The scanner (scanner.rs:188) passes `bytes` of exactly `remaining` length with `offset = 0` and `page_size = remaining`, confirming the empty-slice path is reachable from production code.

**Impact**: Compaction thread panics while holding `compaction_lock` (kv.rs). The lock is a `Mutex`, so it becomes poisoned. All subsequent `compact()` calls fail with `PoisonError`. Recovery requires process restart.

#### Warrant (Rule)

The corruption detection is off-by-one: `value_offset > remaining` permits the boundary case where exactly zero bytes remain for value size inspection. Variable-length types require at least `LENGTH_PREFIX_SIZE` (4) bytes to read their length prefix. The guard must account for this minimum.

#### Rebuttal Conditions

NOT a concern if corrupted on-disk data is impossible (e.g., checksummed pages with verified reads). The FASTER architecture does not appear to have page-level checksums in this codebase.

#### Suggested Verification

Replace the guard at layout.rs:333:

```rust
// Before (off-by-one):
if value_offset > remaining {

// After (ensures value prefix readable):
if value_offset + LENGTH_PREFIX_SIZE > remaining {
```

Exploit test:
```rust
#[test]
fn exploit_value_offset_equals_remaining() {
    let remaining = 24usize;
    let mut page = vec![0u8; remaining];
    page[8..12].copy_from_slice(&12u32.to_le_bytes()); // key_len = 12
    let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, remaining);
    assert_eq!(result, Err(RecordSizeError::CorruptedRecord { offset: 0 }));
}
```

---

### Finding: Numeric `eq_from_bytes` can be crashed via crafted `previous_address` pointing to page-end padding

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: edge-cases

#### Grounds (Evidence)

**Attack**: Corrupt a record's `previous_address` field (stored in the 48-bit field of `RecordInfo`) to point to an offset with fewer than `KEY_OFFSET + key_size` bytes remaining on the target page. During version chain walking in `is_current_version`, the scanner calls `read_header_and_match_key_varlen` which uses `safe_record_size(addr, 8)` — returning `max(remaining, 8)`. If `remaining = 8`, the accessor has 8 bytes, and `eq_from_bytes` receives `&accessor.as_slice()[8..]` = empty slice.

For numeric types (traits.rs:246-248):
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()  // PANICS on empty buf
}
```

No length guard. Contrast with `Vec<u8>::eq_from_bytes` (traits.rs:330-331) which checks `buf.len() < LENGTH_PREFIX_SIZE`.

**Exploit scenario**: A bitflip in the `previous_address` field of any record in a hash chain causes the chain walk to visit an invalid location near a page boundary. The compaction scanner panics during `is_current_version`.

#### Warrant (Rule)

All data-dependent comparisons on untrusted bytes must validate buffer length before indexing. The variable-length types (`Vec<u8>`, `String`) already follow this pattern defensively. The numeric types break the pattern, creating an asymmetry where fixed-size key compaction is *less* robust than variable-length key compaction — the opposite of what one would expect.

#### Rebuttal Conditions

NOT a concern if `previous_address` pointers are always valid (no disk corruption, no bitflips). In practice, storage engines must handle these cases — FASTER's existing `MAX_CHAIN_DEPTH` limit (scanner.rs:72) already demonstrates awareness that chains can be corrupted. The depth limit prevents infinite loops but not panics from short buffers.

#### Suggested Verification

Add length guard to the numeric macro (traits.rs:246-248):
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

---

### Finding: Degenerate hash distribution causes O(n × MAX_CHAIN_DEPTH) compaction scan time — no progress feedback or timeout

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: edge-cases

#### Grounds (Evidence)

**Attack**: Insert keys that all hash to the same bucket (feasible with a 1024-bucket hash table — `hash_index_size_log2: 10` as used in tests). During compaction scanning, every non-tombstone, non-invalid record triggers `is_current_version` (scanner.rs:221) which walks the version chain up to `MAX_CHAIN_DEPTH = 4096` hops (scanner.rs:72).

With `N` records in the compaction region, worst-case chain walking is `O(N × 4096)`. For N = 100,000 records: ~410 million hash-chain hops, each involving `RecordAccessor::from_log` and `eq_from_bytes`. This is a quadratic blowup that could stall compaction for minutes or hours.

The scanner (scanner.rs:160-239) has **no progress callback, cancellation token, or timeout**. The orchestrator (orchestrator.rs:172) blocks on the scan with no way to abort.

#### Warrant (Rule)

Batch operations processing untrusted data volumes must have progress monitoring or bounded runtime. The MAX_CHAIN_DEPTH limit prevents infinite loops per record but doesn't bound total scan time. In a multi-tenant environment, one tenant with a degenerate key pattern can monopolize the compaction thread indefinitely, blocking compaction for all tenants.

#### Rebuttal Conditions

NOT a concern if: (1) the hash table is always sized proportionally to the data (preventing high collision rates); or (2) compaction is always run with appropriately-sized hash indices. The test configurations use `hash_index_size_log2: 10` (1024 buckets) which is realistic for small datasets but not for production workloads with variable-length keys that could have crafted hash distributions.

#### Suggested Verification

Add a total-hops counter to the scan loop and log a warning or return an error if total chain traversal exceeds a configurable threshold (e.g., `total_hops > N × 16` where N is the number of records scanned). Consider adding a `CancellationToken` parameter to `scan()`.

---

### Finding: Custom `Key` implementations with malicious `serialized_size_from_bytes` can cause arbitrary memory reads

**Severity**: consider
**Confidence**: LOW
**Category**: edge-cases

#### Grounds (Evidence)

**Attack**: Implement a custom `Key` type whose `serialized_size_from_bytes` returns a very large value (e.g., `usize::MAX / 2`). When `record_size_from_bytes` calls `K::serialized_size_from_bytes(key_buf)` (layout.rs:329), the returned `key_size` is used to compute `value_offset = pad_alignment(8 + usize::MAX/2, 8)` which overflows, producing a small value. The subsequent `value_buf = &page_bytes[offset + value_offset..]` could then read from an unexpected location.

On 64-bit: `pad_alignment(8 + usize::MAX/2, 8)` = `pad_alignment(9223372036854775811, 8)` = `9223372036854775816` which is > remaining → caught by corruption check. So this is safe on 64-bit.

On 32-bit: `pad_alignment(8 + 2147483647, 8)` wraps around, producing a small value that could bypass the corruption check.

Additionally, the default `serialized_size_from_bytes` implementation calls `Self::deserialize(buf).serialized_size()` (traits.rs:130). A custom `deserialize` could perform arbitrary computation, panic, or return inconsistent sizes.

The docstring (traits.rs:123-129) documents the contract but there's no runtime enforcement.

#### Warrant (Rule)

Trait methods called on untrusted data have an implicit trust boundary — the implementation must be correct for all possible byte inputs. While this is a general concern with any trait-based extensibility, the compaction pipeline is particularly sensitive because it processes raw disk data in a batch operation. A single buggy `Key` implementation can crash the entire compaction cycle.

#### Rebuttal Conditions

NOT a concern if: (1) only the built-in types (`u32`, `u64`, `i32`, `i64`, `Vec<u8>`, `String`) are used in practice; or (2) custom types are tested against the contract before deployment. The documentation in the diff (traits.rs:32-66) provides an explicit override example with correct patterns, which mitigates the risk.

#### Suggested Verification

Add a property test that verifies the `serialized_size_from_bytes` contract for any `Key` type:
```rust
// For all keys k: serialized_size_from_bytes(serialize(k)) == k.serialized_size()
```
The diff already includes this test for `Vec<u8>` (traits.rs:694-701) — extend it to other types and document it as a required test for custom implementations.

---

### Finding: `read_header_and_match_key_varlen` requests only `RECORD_HEADER_SIZE` minimum from `safe_record_size`, potentially truncating key data in the accessor

**Severity**: consider
**Confidence**: MEDIUM
**Category**: edge-cases

#### Grounds (Evidence)

In `rust/crates/faster-core/src/hybrid_log/record_ops.rs:550`:

```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
```

`safe_record_size` returns `max(remaining, 8)`. For valid records, `remaining >= actual_record_size` (since records never span pages), so this returns `remaining` — providing the full page remainder to the accessor. The accessor's `as_slice()` then has `remaining` bytes, and `eq_from_bytes` at `KEY_OFFSET` gets `remaining - 8` bytes.

However, this works only because `safe_record_size` coincidentally returns `remaining` (which is ≥ record_size) rather than `RECORD_HEADER_SIZE`. The function's semantics are "give me at least `min_size` bytes" — but the caller actually needs "give me the full record." If `safe_record_size` were ever changed to return exactly `min_size` when `remaining >= min_size`, `eq_from_bytes` would receive only header bytes with no key data.

**The current code works by accident**, not by intent. The `min_size` parameter should be the full page remainder or the expected record size, not just the header size.

#### Warrant (Rule)

Code that depends on a side effect of a helper function (returning more bytes than the minimum requested) is fragile. The function name `safe_record_size` and parameter name `min_size` suggest it returns *at least* `min_size` bytes — not necessarily the full page remainder. A future refactor of `safe_record_size` to clamp at `min_size` (e.g., for memory efficiency) would silently break `read_header_and_match_key_varlen`.

#### Rebuttal Conditions

NOT a concern if: (1) `safe_record_size` is stable and its behavior of returning `max(remaining, min_size)` is considered part of its contract; or (2) the function is refactored to make the intent explicit (e.g., renamed to `page_remaining_or_min`). The current implementation is correct; the concern is maintainability, not immediate correctness.

#### Suggested Verification

Either: (a) rename the call to use `remaining` directly:
```rust
let page_size = 1u32 << OFFSET_BITS;
let remaining = page_size.saturating_sub(addr.offset().0);
let accessor = RecordAccessor::from_log(self.allocator, addr, remaining)?;
```
Or (b) add a comment explaining the reliance on `safe_record_size` returning the page remainder.
