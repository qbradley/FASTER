# Testing Specialist Review — Premortem Perspective

**Perspective**: premortem
**Reviewer**: Testing Specialist
**Scope**: ~3,400 lines across 21 files — variable-length record compaction for FASTER (Rust)

---

## Cognitive Strategy Applied

Starting from the implementation, the highest-risk scenarios for this change are:

1. **Variable-stride scanning produces an incorrect record size** → silent data loss (live records classified as dead, or the scanner overshoots into the next record)
2. **`eq_from_bytes` mismatches in version chain walks** → wrong record promoted as "current version," causing stale reads or dropped updates
3. **Records that span or nearly span page boundaries** → scanning gap logic either skips valid records or reads corrupt data from the padding zone
4. **Tombstone collection + removal interaction** → tombstones not removed (index bloat) or non-tombstones removed (data loss)
5. **Type-mismatch between `compact::<K, V>()` and actual stored types** → the type parameters are now the only way sizes are resolved; wrong types silently produce wrong sizes

---

### Finding 1: No test verifies that `compact::<WrongK, WrongV>()` is caught or produces correct behavior

**Severity**: must-fix
**Confidence**: HIGH
**Category**: testing

#### Grounds (Evidence)

The public API `FasterKv::compact::<K, V>()` (`store/kv.rs:1387`) accepts arbitrary `Key`/`Value` type parameters. Previously, only `FixedSizeKey`/`FixedSizeValue` were accepted, which provided compile-time size guarantees. Now, any `Key + Value` pair is accepted. If a caller invokes `compact::<u64, u64>()` on a store that was populated with `Vec<u8>` keys and values, `record_size_from_bytes::<u64, u64>()` will interpret the variable-length length prefix as the fixed 8-byte key, compute a wrong layout, and either:
- Return `CorruptedRecord` (safe abort) if the resulting size exceeds the page, or
- Return a plausible but wrong size (silent data corruption) if the interpreted bytes happen to be small.

No integration test or unit test exercises this mismatch scenario. All 5 new integration tests correctly match `compact::<K, V>()` types to the stored types.

#### Warrant (Rule)

When a function changes from a restrictive type bound (`FixedSizeKey`) to a permissive one (`Key`), the risk of callers providing incorrect types increases. The behavioral contract — "K and V must match the types used when records were written" — is a documentation-only constraint with no runtime enforcement. The most likely production incident 6 months out is someone calling `compact::<u64, u64>()` on a heterogeneous or variable-length store because that's what the old code required, and they didn't update the call site.

#### Rebuttal Conditions

This is NOT a concern if: (1) the Rust type system at the `FasterKv` level enforces that `K` and `V` always match `F::Key` and `F::Value` from the `Functions` trait, making a mismatch a compile error. Checking `kv.rs:1387`, the signature is `pub fn compact<K: Key, V: Value>(&self)` — it does NOT constrain `K == F::Key`. So the mismatch is possible at the API level. (2) The `record_size_from_bytes` corruption checks always catch the mismatch — but this is not guaranteed for all type combinations (e.g., `compact::<u32, u32>()` on `u64` data could produce a "valid" but wrong 8-byte record size vs the correct 24-byte one).

#### Suggested Verification

Add a test: `compact_type_mismatch_returns_error_or_safe_abort`. Populate a store with `Vec<u8>` keys, then call `store.compact::<u64, u64>()`. Verify it either returns an error or produces no data loss. Additionally, consider a compile-time fix: change `compact()` to `compact(&self) -> ...` with `K = F::Key, V = F::Value` inferred from the `Functions` trait, eliminating the mismatch vector entirely.

---

### Finding 2: No test for records whose serialized size exactly fills a page (page-spanning boundary)

**Severity**: should-fix
**Confidence**: HIGH
**Category**: testing

#### Grounds (Evidence)

The scanner (`scanner.rs:160-195`) uses `MIN_READABLE` (12 bytes) as the page-end gap threshold. When a variable-length record's size is close to the page size (e.g., a record with a 4000-byte value on a 4096-byte page), the remaining bytes after that record might be between 0 and 11 — triggering the `MIN_READABLE` skip to the next page.

The integration tests (`compaction_integration.rs`) use `make_value(i, 10 + (i % 50))` producing values from 10 to 59 bytes — all tiny relative to any reasonable page size. The unit test `record_size_from_bytes_page_boundary_tight_fit` (layout.rs:~line 770) tests a record that exactly fills a page-sized buffer, but this is a `record_size_from_bytes` unit test, not a scanner test that exercises the full scanning loop with page-boundary gap handling.

No test writes a record large enough to leave a gap < `MIN_READABLE` at the end of a physical page, then verifies the scanner correctly advances to the next page and resumes scanning.

#### Warrant (Rule)

Page boundary handling is the #1 source of off-by-one errors in log-structured storage engines. The scanner's gap-skip logic (`scanner.rs:163-167`) is new code that replaces the old fixed-stride boundary check. If `MIN_READABLE` is wrong (e.g., should be 16 instead of 12 for some alignment scenario), or if the gap calculation has an off-by-one, records near page boundaries could be silently skipped. This is exactly the class of bug (boundary condition unexercised by tests) from my CI pipeline incident.

#### Rebuttal Conditions

Not a concern if: the page size used in tests is so large relative to record sizes that no record ever approaches a page boundary. But with `InMemoryDevice` and small `buffer_size_pages`, there's a risk that a future page size reduction (or a user with a custom device) exposes this gap.

#### Suggested Verification

Add a scanner unit test that: (1) writes variable-length records to a page until < 12 bytes remain, (2) writes more records on the next page, (3) scans the full range and asserts all records are found. Also test the exact boundary: a record whose total size equals `page_size - 12`, leaving exactly `MIN_READABLE` bytes, and another whose total size equals `page_size - 11`, leaving 11 bytes (should skip).

---

### Finding 3: `eq_from_bytes` for `Vec<u8>` and `String` is not tested with trailing garbage bytes in the buffer

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

In `traits.rs`, `eq_from_bytes` for `Vec<u8>` (lines ~315-327) reads the length prefix, checks `len == self.len()`, then compares `buf[4..4+len] == self[..]`. The `buf` parameter in production is a slice from a page buffer — it extends from the key offset to the end of the page (or the end of the accessor's slice). This means `buf` contains the serialized key followed by the serialized value and potentially other records' data.

The unit tests (`eq_from_bytes_vec_u8_match`, etc.) all create a `buf` that's exactly the serialized size of the key — no trailing bytes. If `eq_from_bytes` had a bug that compared more bytes than the key (e.g., `buf[4..] == self[..]` instead of `buf[4..4+len] == self[..]`), these tests would not catch it because the buffer ends at exactly the key boundary.

#### Warrant (Rule)

In production, `eq_from_bytes` is called from `read_header_and_match_key_varlen` (`record_ops.rs:555`) with `accessor.as_slice()[KEY_OFFSET..]` — a slice that contains the key bytes followed by value bytes and possibly padding. If the comparison inadvertently includes trailing bytes, it would produce false negatives (key doesn't match when it should), causing the scanner to misclassify records as dead. Tests that use exact-fit buffers cannot detect this class of bug.

#### Rebuttal Conditions

Not a concern if: the `eq_from_bytes` implementation explicitly bounds the slice to `buf[4..4+len]` (which it does in the current code). However, this is precisely the invariant that should be regression-protected — if someone changes the comparison bounds in a future refactor, the current tests won't catch it.

#### Suggested Verification

Add tests with trailing garbage: create a buffer with the serialized key followed by 100 bytes of `0xFF`. Verify `eq_from_bytes` still returns `true`. Also verify that two keys with the same prefix but different lengths correctly return `false` when the buffer has trailing data that would match the longer key.

---

### Finding 4: Concurrent compaction integration test (`compact_variable_length_concurrent_reads`) does not assert read *completeness*

**Severity**: should-fix
**Confidence**: HIGH
**Category**: testing

#### Grounds (Evidence)

In `compaction_integration.rs:622-645`, the reader thread's inner loop:
```rust
let _ = store_r.read(&mut session, &i, &vec![], &mut out, ());
// We don't assert Ok here because compaction may move records,
// but the data should never be corrupted if returned.
if let Some(ref data) = out {
    // ... assert data correctness
}
```

The test explicitly ignores read failures during concurrent compaction. After compaction, it verifies all reads succeed (lines 650-667). However, the *during-compaction* verification only checks "if data comes back, it's correct" — it does not track how many reads returned `None` vs `Some`. If compaction has a bug that causes all reads to return `None` during the compaction window (e.g., a brief period where hash index entries point to neither old nor new addresses), this test would pass because `if let Some(ref data) = out` is never entered.

#### Warrant (Rule)

A test that conditionally skips its key assertion provides weaker protection than one that quantifies the expected behavior. The behavioral contract during concurrent compaction is: "reads may temporarily miss records during pointer swing, but the miss window should be brief." The test doesn't verify that any reads succeeded during compaction — it could be that 100% of reads returned `None`, which would suggest a much longer disruption window than intended.

#### Rebuttal Conditions

Not a concern if: (1) the post-compaction verification is considered sufficient (all reads succeed after compaction), and the during-compaction leniency is intentional to avoid flakiness. (2) The FASTER epoch protocol guarantees that reads always see *some* valid version, making `None` impossible for non-deleted keys.

#### Suggested Verification

Add a counter for successful reads during compaction. After the reader thread joins, assert that at least some fraction (e.g., >50%) of reads returned `Some`. This verifies that the concurrent read path is functional during compaction, not just after.

---

### Finding 5: `serialized_size_from_bytes` panic paths are untested with realistic corrupt data patterns

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

The `serialized_size_from_bytes` implementations for `Vec<u8>` and `String` (`traits.rs:315-322`, `395-402`) contain:
```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(
        buf[..LENGTH_PREFIX_SIZE].try_into()
            .expect("buffer too short for Vec<u8> length prefix"),
    ) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

This panics if `buf.len() < 4`. In `record_size_from_bytes` (`layout.rs:~line 330`), the caller slices `page_bytes[offset + key_offset..]` — but the `MIN_READABLE` check only guarantees 12 bytes remain from `offset`, and `key_offset` is 8, leaving only 4 bytes guaranteed for the key buffer. If the key's `serialized_size_from_bytes` reads a length prefix indicating a 100-byte key, then the value buffer slice `page_bytes[offset + value_offset..]` could be empty, causing the value's `serialized_size_from_bytes` to panic.

The `CorruptedRecord` check at layout.rs:~line 343 validates `value_offset > remaining` but this check happens *after* `K::serialized_size_from_bytes` returns. If `serialized_size_from_bytes` returns a huge `key_size` (e.g., from a corrupt length prefix where the u32 value is 0x0000_00FF = 255, which is valid but wrong), then `value_offset` could be valid but the `value_buf` slice passed to `V::serialized_size_from_bytes` could be too short, causing a panic instead of a clean error return.

The corruption tests in `layout.rs` only test `u32::MAX` as the corrupted value — which triggers the `value_offset > remaining` check before the value's `serialized_size_from_bytes` is called. A moderate corruption value (e.g., key_size = 200 on a 512-byte page) that passes the `value_offset` check but leaves insufficient bytes for the value prefix is not tested.

#### Warrant (Rule)

A panic in a storage engine during compaction is a production incident — it crashes the process instead of returning a clean error. The `record_size_from_bytes` function's documentation promises to return `RecordSizeError`, but the underlying `serialized_size_from_bytes` can panic. This is a gap between the documented contract and the actual behavior.

#### Rebuttal Conditions

Not a concern if: the `value_offset > remaining` check (layout.rs:~line 343) always catches corrupt key sizes before `V::serialized_size_from_bytes` is called with an insufficient buffer. Let me trace: `key_size` could be, say, 200 (from corrupt prefix). `value_offset = pad_alignment(8 + 200, 8) = 208`. `remaining = 512 - 0 = 512`. `208 > 512` is false, so we proceed. `value_buf = &page_bytes[0 + 208..]` which is 304 bytes — enough for the 4-byte prefix. So for a 512-byte page, key_size=200 wouldn't trigger the panic. The panic requires `value_buf.len() < 4`, which means `remaining - value_offset < 4`. The check `value_offset > remaining` catches `value_offset > remaining` but not `remaining - value_offset < 4`. So a key_size of 496 on a 512-byte page: `value_offset = pad_alignment(8 + 496, 8) = 504`. `504 > 512` is false. `value_buf = &page_bytes[504..]` = 8 bytes. That's fine. key_size = 500: `value_offset = 508 > 512`? `pad_alignment(508, 8) = 512`. `512 > 512` is false. `value_buf = &page_bytes[512..]` = 0 bytes. Panic!

This is a real gap: key_size = 500 on a 512-byte page produces `value_offset = 512`, `value_buf` is empty, `V::serialized_size_from_bytes` panics.

#### Suggested Verification

Add a unit test: 512-byte page, write a corrupt key length prefix of 500 at key_offset (8). Call `record_size_from_bytes::<Vec<u8>, Vec<u8>>`. It should return `Err(CorruptedRecord)`, not panic. If it panics, the fix is to add a `remaining - value_offset < LENGTH_PREFIX_SIZE` check before calling `V::serialized_size_from_bytes`.

---

### Finding 6: No property test asserting the `serialized_size_from_bytes` ↔ `serialized_size` roundtrip invariant across random inputs

**Severity**: consider
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

The trait documentation (`traits.rs:120-124`) states: "The result **must** equal `Self::deserialize(buf).serialized_size()`." There is one explicit roundtrip test (`serialized_size_from_bytes_matches_serialized_size` in traits.rs tests) but it only tests `Vec<u8>` with a single 100-byte payload. The existing proptest module (`traits.rs:631+`) is not shown to include roundtrip property tests for this invariant.

For a custom `Key` implementation (the trait is public and intended for extension), violating this invariant would cause the compaction scanner to compute wrong strides. A property test that generates random `Vec<u8>`, `String`, and numeric keys, serializes them, and asserts `serialized_size_from_bytes(buf) == key.serialized_size()` would catch any drift between the two codepaths.

#### Warrant (Rule)

When a contract is documented but only spot-checked, it's one refactor away from silent violation. Property-based testing is the standard technique for verifying serialization roundtrip invariants across a wide input space.

#### Rebuttal Conditions

Not a concern if existing proptests in the `proptests` module already cover this invariant (I cannot see the full proptest module in the diff).

#### Suggested Verification

Add proptests:
```rust
proptest! {
    #[test]
    fn serialized_size_from_bytes_roundtrip_vec(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let mut buf = vec![0u8; Key::serialized_size(&data)];
        Key::serialize(&data, &mut buf);
        prop_assert_eq!(<Vec<u8> as Key>::serialized_size_from_bytes(&buf), Key::serialized_size(&data));
    }
}
```

---

## Summary

The test suite is strong for the happy-path scenarios it covers — 5 well-structured integration tests, thorough unit tests for `record_size_from_bytes` and `eq_from_bytes`, and benchmarks for performance regression. The most critical gap is the **type-mismatch vector** (Finding 1), where `compact::<K, V>()` accepts types that may not match the stored data with no runtime guard. The second most impactful gap is the **potential panic in `serialized_size_from_bytes`** on edge-case corrupt data (Finding 5), where the corruption detection path has a window that produces a panic instead of a clean error. Both are reachable in production under plausible conditions.
