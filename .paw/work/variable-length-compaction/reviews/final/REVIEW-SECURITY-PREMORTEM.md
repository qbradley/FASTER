# REVIEW-SECURITY-PREMORTEM.md

**Specialist**: Security
**Perspective**: premortem
**Diff**: `git diff squad..feature/variable-length-compaction`

---

## Threat Context

This change extends a storage engine's compaction pipeline to process variable-length records. The scanner reads raw page bytes, interprets length prefixes to discover record boundaries, deserializes keys, and walks hash chains. The trust boundary is between the on-disk/in-memory page data and the compaction logic's assumptions about that data's integrity. Corruption in page bytes — whether from hardware failure, torn writes, or a bug in the allocator — becomes adversarial input to the scanner.

---

### Finding: `serialized_size_from_bytes` for `Vec<u8>` panics on corrupted length prefix instead of returning an error, creating an uncontrolled crash vector through the `record_size_from_bytes` call chain

**Severity**: must-fix
**Confidence**: HIGH
**Category**: security
**Perspective**: premortem

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs:320-327`, the `Vec<u8>::serialized_size_from_bytes` implementation reads a u32 length prefix and returns `LENGTH_PREFIX_SIZE + len` without validating that `len` fits within the buffer:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(
        buf[..LENGTH_PREFIX_SIZE]
            .try_into()
            .expect("buffer too short for Vec<u8> length prefix"),
    ) as usize;
    LENGTH_PREFIX_SIZE + len  // no bounds check on `len`
}
```

The caller `record_size_from_bytes` (layout.rs:~line 466-467) passes `&page_bytes[offset + key_offset..]` as `key_buf`, then calls `K::serialized_size_from_bytes(key_buf)`. The returned `key_size` is used to compute `value_offset`, which is then validated against `remaining`. However, the **value of `key_size` itself is never validated against the buffer length** before it's returned from `serialized_size_from_bytes`.

Consider: a corrupted key length prefix of `0x7FFFFFFF` (2 GiB). `serialized_size_from_bytes` returns `4 + 2,147,483,647 = 2,147,483,651`. Then `pad_alignment(8 + 2147483651, 8)` overflows on 32-bit usize or produces an astronomically large value on 64-bit. The `value_offset > remaining` check in `record_size_from_bytes` catches this, but **only after the arithmetic**. On a 32-bit target (or any platform where the addition wraps), `value_offset` could wrap to a small value, passing the validation check and causing `&page_bytes[offset + value_offset..]` to produce a valid but semantically wrong slice — potentially reading value length data from the wrong location.

The same pattern exists for `String::serialized_size_from_bytes` (traits.rs:~395-401) and `Value for Vec<u8>::serialized_size_from_bytes` (traits.rs:~359-366).

Furthermore, the `expect()` on the `try_into()` in `serialized_size_from_bytes` will panic if `buf.len() < 4`. While `record_size_from_bytes` checks `remaining >= 12` before calling, the value buffer check (`value_buf` at `&page_bytes[offset + value_offset..]`) could produce a zero-length slice if `value_offset == remaining` exactly, and then `V::serialized_size_from_bytes` would panic on `buf[..4].try_into().expect(...)`.

The `String` implementation has identical issues.

#### Warrant (Rule)

In a storage engine, page data is a trust boundary. Data read from pages must be treated as potentially adversarial — hardware corruption, torn writes, bugs in concurrent writers, or filesystem errors can produce arbitrary byte patterns. A function that interprets user/page data and can cause an unrecoverable panic (via `expect()`) violates defense-in-depth: the compaction pipeline's stated design is "abort compaction on corruption, preserve data," but a panic in `serialized_size_from_bytes` crashes the entire process rather than returning a recoverable error. The blast radius changes from "compaction fails gracefully" to "process crashes, potentially during production traffic."

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` always provides a buffer slice guaranteed to be >= 4 bytes to both key and value `serialized_size_from_bytes` calls (but the value call is NOT guaranteed — see `value_offset == remaining` edge case); or (2) all platforms are 64-bit and the arithmetic overflow path is unreachable (but the contract should enforce this, not assume it); or (3) page data corruption is truly impossible (it is not — this is a storage engine).

#### Suggested Verification

Write a test that constructs a page with `value_offset == remaining` (i.e., key length prefix that causes `pad_alignment(key_offset + key_size, 8)` to exactly equal `remaining`). Call `record_size_from_bytes::<Vec<u8>, Vec<u8>>` and verify it returns `Err`, not a panic. Also test with a corrupted key length prefix on a 64-byte page where the returned `key_size` > `remaining`.

---

### Finding: `read_header_and_match_key_varlen` uses `safe_record_size` which returns `max(remaining, RECORD_HEADER_SIZE)` — providing potentially unbounded access past page boundaries on later `eq_from_bytes` call

**Severity**: should-fix
**Confidence**: HIGH
**Category**: security
**Perspective**: premortem

#### Grounds (Evidence)

In `record_ops.rs:548-556`, the new `read_header_and_match_key_varlen` function:

```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
let ri = accessor.record_info();
const KEY_OFFSET: usize = 8;
let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
```

`safe_record_size` (line 38-43) computes `remaining.max(min_size)`. When `remaining` (page_size - offset) is large (e.g., record is near the start of a page), `record_size` = `remaining` — which could be the entire page (e.g., 32KB). The `RecordAccessor` then provides a slice of `remaining` bytes. `key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..])` passes this large slice to the key comparison.

For `Vec<u8>::eq_from_bytes` (traits.rs:330-338), this is safe because it reads the 4-byte prefix and uses `len` for bounds. But for a **custom Key implementor** who overrides `eq_from_bytes` and reads beyond the actual record boundary (into the next record's data), this could:
1. Compare against wrong bytes (correctness issue)
2. In future, if pages are memory-mapped from disk and the mapping ends at page boundary, read past the mapped region

The `as_slice()` call itself constructs a `from_raw_parts` slice of `record_size` bytes (line 146), so if the page frame is actually `page_size` bytes, this is technically safe. But the slice semantically contains multiple records, and the key comparison function has no way to know where the current record ends.

#### Warrant (Rule)

The code gives `eq_from_bytes` a buffer that extends well beyond the actual record. While built-in implementations handle this correctly (they read only the prefix + declared length), custom implementations may not. This violates the principle of minimal data exposure — the function should only provide the key region, not the entire page remainder. Any future `Key` implementation that iterates or reads to the end of the buffer will silently read into adjacent records' data.

#### Rebuttal Conditions

This is NOT a concern if: (1) all `eq_from_bytes` implementations are contractually required to only read `serialized_size_from_bytes(buf)` bytes — but this contract is not documented in the trait definition (the doc says "buf contains at least `Self::serialized_size_from_bytes(buf)` bytes", implying there may be more, not that the implementation must ignore extra bytes); or (2) page frames always contain valid, initialized memory for the full page size, and reading adjacent records' bytes has no side effects beyond incorrect comparisons (which `eq_from_bytes` handles by design). Given these are in-memory pages, this is likely the case.

#### Suggested Verification

Add a `debug_assert!` or document in the `eq_from_bytes` contract that implementations MUST NOT read beyond `serialized_size_from_bytes(buf)` bytes. Alternatively, in `read_header_and_match_key_varlen`, call `K::serialized_size_from_bytes` first to determine the actual key size and truncate the slice.

---

### Finding: `record_size as u32` cast in scanner's `advance()` call can silently truncate record sizes > 4 GiB, causing infinite scanning loops

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: security
**Perspective**: premortem

#### Grounds (Evidence)

In `scanner.rs` (approximately diff line 2937), the scanner calls:

```rust
advance(&mut current, record_size as u32, page_size);
```

`record_size` is `usize` (returned by `record_size_from_bytes`). On a 64-bit system, `usize` is 64 bits. `record_size as u32` silently truncates. If a corrupted length prefix produces a `record_size` of e.g., `0x1_0000_0010` (4 GiB + 16), the cast produces `0x10` (16 bytes). The `advance` function moves forward 16 bytes instead of the enormous value. This could cause the scanner to re-read the same corrupted region repeatedly, though the `while current < until_address` guard would eventually terminate.

However, the `record_size_from_bytes` function validates `total <= remaining` where `remaining <= page_size` (which fits in `u32` since page sizes are <= 2^32). So in practice, `record_size` should always fit in `u32`.

The `address_update.rs` code has the same pattern in lines 2001 and 2035:
```rust
mapping.record_size as u32
```
and
```rust
record_size as u32
```

#### Warrant (Rule)

Silent integer truncation is a category of vulnerability that storage engines must guard against. Even if the current validation makes overflow unreachable, a future refactor that changes the validation order or relaxes the page-size constraint could expose this truncation. Defensive coding requires either compile-time assertions (`debug_assert!(record_size <= u32::MAX as usize)`) or `try_from` conversions at every cast site.

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` guarantees `total <= remaining <= page_size <= u32::MAX` — which it currently does, since `page_size` is `usize` derived from `1u32 << OFFSET_BITS` (always fits u32). The finding is defensive, not exploitable today.

#### Suggested Verification

Add `debug_assert!(record_size <= u32::MAX as usize)` before every `record_size as u32` cast. Or better: use `u32::try_from(record_size).expect("record_size exceeds u32")` to catch this at runtime in debug and release builds.

---

### Finding: No rate-limiting or logging of corruption detection events — a sustained corruption attack or hardware degradation is invisible to operators

**Severity**: consider
**Confidence**: MEDIUM
**Category**: security
**Perspective**: premortem

#### Grounds (Evidence)

When `record_size_from_bytes` returns `Err(RecordSizeError::CorruptedRecord { offset })` (layout.rs:~472), the scanner (scanner.rs) propagates this as an `Err` to the orchestrator (orchestrator.rs:~174), which wraps it in `CompactionError::ScanCorruption`. The error is returned to the caller.

There is **no logging, metrics emission, or alerting** at any level. In a production deployment, repeated compaction failures due to silent data corruption would manifest only as "compaction not making progress" — operators would see growing log sizes without understanding why.

#### Warrant (Rule)

Corruption events in a storage engine are security-relevant signals — they may indicate hardware failure, but they may also indicate an attacker tampering with the storage layer. A storage engine that silently swallows corruption signals (even by returning errors) without providing observability makes it harder for operators to detect and respond to integrity violations. The blast radius of undetected ongoing corruption is unbounded data loss.

#### Rebuttal Conditions

This is NOT a concern if: (1) the application layer is expected to log `CompactionError::ScanCorruption` and alert on it — but this is a library crate, not an application, so the library should at least provide structured diagnostic information; or (2) a separate integrity-checking mechanism exists that would detect corruption independently of compaction.

#### Suggested Verification

Add `tracing::error!` or `log::error!` at the corruption detection site with the offset and page address. Consider adding a corruption counter metric if the crate supports metrics.

---

### Finding: `eq_from_bytes` for numeric types performs unchecked slice indexing that panics if buffer is shorter than type size

**Severity**: consider
**Confidence**: MEDIUM
**Category**: security
**Perspective**: premortem

#### Grounds (Evidence)

In the `impl_key_value_for_numeric!` macro (traits.rs:~244-246):

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

If `buf.len() < size_of::<T>()`, this panics. Contrast with `Vec<u8>::eq_from_bytes` which checks `if buf.len() < LENGTH_PREFIX_SIZE { return false; }`.

In the version chain walk (via `read_header_and_match_key_varlen`), the buffer passed to `eq_from_bytes` is `accessor.as_slice()[KEY_OFFSET..]` where the accessor has `safe_record_size(addr, 8)` bytes. If the record is at offset `page_size - 8` (i.e., only 8 bytes remaining), then `as_slice()` returns 8 bytes, and `as_slice()[8..]` is an empty slice. Calling `eq_from_bytes` on a `u64` with an empty buffer panics.

This scenario requires a record starting within 8 bytes of the page end — which shouldn't happen with aligned records, but a corrupted `previous_address` in a version chain could point to such an address.

#### Warrant (Rule)

A corrupted `previous_address` field in a version chain is a plausible corruption scenario. Following such an address into the last 8 bytes of a page would produce a valid `RecordInfo` read (exactly 8 bytes) but a zero-length buffer for the key comparison, causing a panic in the numeric `eq_from_bytes`. This is similar to the length-prefix corruption vector but affects fixed-size types.

#### Rebuttal Conditions

This is NOT a concern if: (1) `safe_record_size` always returns at least `RECORD_HEADER_SIZE + key_size` bytes — but it only guarantees `max(remaining, RECORD_HEADER_SIZE)`, and `remaining` at offset `page_size - 8` is exactly 8, which equals `RECORD_HEADER_SIZE`, leaving zero bytes for the key; or (2) the allocator guarantees that no valid or corrupted `previous_address` can point to within 8 bytes of a page end. The MAX_CHAIN_DEPTH limit prevents infinite loops but not the panic.

#### Suggested Verification

Add a bounds check in the numeric `eq_from_bytes`: `if buf.len() < core::mem::size_of::<$t>() { return false; }`. Or add a minimum-size check in `read_header_and_match_key_varlen` before calling `eq_from_bytes`.

---

## Summary

The primary premortem failure mode is **panics from corrupted data reaching `serialized_size_from_bytes` and `eq_from_bytes` without sufficient bounds validation**. The stated corruption-handling strategy (abort compaction, preserve data) is correct in intent but incompletely implemented: the `record_size_from_bytes` function validates at the caller level, but the callees (`serialized_size_from_bytes` for variable-length types) can panic before the validation executes. The `read_header_and_match_key_varlen` path provides oversized buffers to `eq_from_bytes`, and numeric types lack the defensive bounds checks that variable-length types have. Six months after deployment, the most likely incident is a process crash during compaction triggered by a single corrupted byte on disk, because the panic in `serialized_size_from_bytes` bypasses the `Result`-based error handling designed to prevent exactly this.
