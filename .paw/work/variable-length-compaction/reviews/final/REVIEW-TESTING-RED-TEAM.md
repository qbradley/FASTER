# Testing Specialist Review — Red-Team Perspective

**Perspective**: red-team
**Reviewer**: Testing Specialist
**Scope**: ~3,400 lines across 21 files — variable-length record compaction for FASTER (Rust)

---

## Adversarial Objective

An attacker (or a fuzzer simulating an attacker) wants to find inputs or scenarios that cause incorrect behavior — data loss, corruption, panics, or denial-of-service — that the current test suite would NOT detect. I'm looking for untested code paths that an adversary could exploit or that a fuzzer would find first.

---

### Finding 1: Crafted length prefix that causes `serialized_size_from_bytes` to return a size that *looks* valid but misaligns the next record

**Severity**: must-fix
**Confidence**: HIGH
**Category**: testing

#### Grounds (Evidence)

The scanner (`scanner.rs:180-200`) calls `record_size_from_bytes::<K, V>(bytes, 0, remaining)` and uses the returned `record_size` to advance to the next record. The validation in `record_size_from_bytes` (`layout.rs:330-350`) checks:
1. `value_offset > remaining` → `CorruptedRecord`
2. `total > remaining` → `CorruptedRecord`

But it does NOT validate that `key_size` or `value_size` are internally consistent with the actual data. Consider: a key with serialized data `[0x02, 0x00, 0x00, 0x00, 0xAA, 0xBB]` — the length prefix says 2 bytes, and the key data is `[0xAA, 0xBB]`. Now imagine a bitflip changes the prefix to `[0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB]` — the prefix says 1 byte. `serialized_size_from_bytes` returns `4 + 1 = 5` instead of `4 + 2 = 6`. The value offset shifts by 1 byte (after alignment), and the total record size is smaller than actual. The scanner advances by the wrong stride, landing in the middle of the next record.

This "off-by-small-amount" corruption is the hardest to detect because both `value_offset` and `total` remain within page bounds. No test creates this scenario. All corruption tests use extreme values (`u32::MAX`, `page_size + 1000`) that are caught by the bounds checks.

#### Warrant (Rule)

The corruption detection is designed for gross corruption (wild pointers, huge values) but not for subtle corruption (bitflips in length prefixes that produce valid-but-wrong sizes). In a real storage system, single-bit corruption in flash or disk is the most common failure mode. When the scanner uses the wrong stride, it reads the middle of the next record as a new record header, which could:
- Interpret value bytes as a `RecordInfo` with random `is_tombstone()`/`is_invalid()` flags → silent misclassification
- Interpret random bytes as a key → hash lookup finds no match → record classified as dead → data loss

No test exercises this "stride drift" scenario because all tests use either uncorrupted data or grossly corrupted data.

#### Rebuttal Conditions

Not a concern if: (1) there is a checksum on each record (RecordInfo has no checksum field — confirmed by the CodeResearch showing the 8-byte header layout with no checksum). (2) The application relies on external checksumming (e.g., filesystem or device-level). (3) FASTER's threat model explicitly excludes bitflip corruption and relies on ECC memory + checksummed storage.

#### Suggested Verification

Add a test: write 5 sequential variable-length records to a page. Corrupt the length prefix of record 2 by reducing it by 1. Scan the full page. Verify the scanner either detects the corruption (preferably) or at minimum does not report records 3-5 as valid (since their "starts" are misaligned). If the scanner reports them as valid with wrong data, this is a silent data loss bug that needs a mitigation (e.g., per-record checksum or at least a "next record starts at a known-good offset" breadcrumb).

---

### Finding 2: `read_header_and_match_key_varlen` uses `safe_record_size` with `RECORD_HEADER_SIZE` — an adversary with hash collisions can force chain walks into under-read records

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

In `record_ops.rs:548-560`, the new method:
```rust
pub fn read_header_and_match_key_varlen<K: Key>(
    &self,
    addr: LogicalAddress,
    key: &K,
) -> Option<(RecordInfo, bool)> {
    let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
    let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
    let ri = accessor.record_info();
    const KEY_OFFSET: usize = 8;
    let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
    Some((ri, key_matches))
}
```

`safe_record_size(addr, RECORD_HEADER_SIZE)` requests only enough bytes to read the header (8 bytes minimum + page-remaining). But `eq_from_bytes` reads `accessor.as_slice()[KEY_OFFSET..]` which needs the key data too. If `RecordAccessor::from_log` returns a view bounded by `record_size` (which is `RECORD_HEADER_SIZE` = 8 in the worst case), then `as_slice()[8..]` would be an empty slice.

Looking more carefully: `safe_record_size` likely returns `min(page_remaining, some_value)`. If it returns the page-remaining, the slice extends to the end of the page and `eq_from_bytes` has enough data. But if a record starts 9 bytes from the page end, `safe_record_size` returns ~9 bytes, `as_slice()` is 9 bytes, `as_slice()[8..]` is 1 byte — and `eq_from_bytes` for a `Vec<u8>` tries to read 4 bytes for the length prefix and panics.

No test exercises version chain walks where a chain-hop record is located near a page boundary with < 12 bytes of usable space.

#### Warrant (Rule)

An adversary who can control key values can engineer hash collisions, forcing long chain walks. If a chain hop lands on a record near a page boundary, `read_header_and_match_key_varlen` may panic or produce incorrect results. The scanner's main loop has the `MIN_READABLE` guard, but the version chain walk does not — it can follow `previous_address` pointers to any location in the log.

#### Rebuttal Conditions

Not a concern if: (1) `safe_record_size` always returns the full remaining page space (which would make the accessor slice large enough). Need to verify `safe_record_size` implementation. (2) Records are never placed within 12 bytes of a page boundary — the allocator ensures sufficient space. The scanner's old code had this property for fixed-size records; if the allocator pads to ensure `MIN_READABLE` free space at page ends, this is safe.

#### Suggested Verification

Write a unit test for `read_header_and_match_key_varlen` where a record's `previous_address` points to a location within 12 bytes of a page boundary. Verify it returns `None` (graceful failure) rather than panicking. Also verify that the version chain walk in `is_current_version` handles this `None` return correctly (the current code at `scanner.rs:290` matches `Some(...)` and falls through to `None` → returns `true` conservatively).

---

### Finding 3: `eq_from_bytes` for `Vec<u8>` returns `false` on short buffer instead of panicking — but the scanner trusts this as "key doesn't match" leading to silent misclassification

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

`Vec<u8>::eq_from_bytes` (`traits.rs:319-327`):
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

When `buf` is too short, it returns `false` — "this key doesn't match." In the scanner's `is_current_version` (`scanner.rs:252-310`), during the chain walk, if `eq_from_bytes` returns `false` for every hop, the loop reaches `MAX_CHAIN_DEPTH` or an invalid address and falls through to `true` (conservative default = "record is live").

But in `address_update.rs:swing_one` (lines ~160-180), the key is read from the **new** address to compute the hash for the CAS. If `eq_from_bytes` is used elsewhere to verify chain integrity during the swing, a false `false` could cause the swing to target the wrong hash entry. However, looking at the code, `swing_one` actually `deserialize`s the key (not `eq_from_bytes`), so this specific path is safe.

The more concerning scenario: during `is_current_version`, the scanner walks the chain looking for a match. If `eq_from_bytes` returns `false` due to a short buffer (not a real mismatch), the scanner continues walking. If it eventually finds no match, it returns `true` — "this record is live." So the failure mode is actually safe (record kept alive when it might be dead). But the opposite is also possible: if a chain walk reaches a valid match on a *different* key (hash collision), `eq_from_bytes` returning `true` correctly identifies it as not matching our key, and the walk continues. The short-buffer `false` is indistinguishable from "wrong key found" — both result in continuing the walk.

No test exercises `eq_from_bytes` returning `false` due to insufficient buffer during a chain walk, verifying the scanner's behavior.

#### Warrant (Rule)

When error conditions are silently mapped to "normal" return values (`false` for "doesn't match"), the caller cannot distinguish errors from legitimate mismatches. In a storage engine, this ambiguity can compound: the scanner's conservative default masks the problem, but a future code change that reverses the default (e.g., "if no match found, record is dead") would turn this silent error into data loss.

#### Rebuttal Conditions

Not a concern if the scanner's conservative default (`true` = keep alive) is permanent and the `eq_from_bytes` short-buffer case can only over-preserve records (never delete them). The current code confirms this, but it's a fragile invariant.

#### Suggested Verification

Add a test that validates the full chain of: short buffer → `eq_from_bytes` returns `false` → scanner conservatively keeps the record alive. Document this as an intentional behavioral contract with a comment in `is_current_version`.

---

### Finding 4: No test for compaction of an empty `Vec<u8>` key (`b""`) combined with a large value

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

The integration tests use `make_key(id)` which produces `format!("key-{id:06}").into_bytes()` — always 10+ bytes. The layout unit test `record_size_from_bytes_zero_length_value` tests empty values but with a non-empty key. No test uses an empty key (`Vec::<u8>::new()`).

For an empty key: `key_size = LENGTH_PREFIX_SIZE + 0 = 4`. `key_offset = 8`. `value_offset = pad_alignment(8 + 4, 8) = 16`. So far fine. But `eq_from_bytes` for an empty `Vec<u8>`: `len == self.len()` → `0 == 0` → `true`, then `buf[4..4] == self[..]` → `[] == []` → `true`. This should work.

The risk is subtler: an empty key hashes to a specific value. If multiple empty keys are inserted (which shouldn't happen in practice since keys should be unique), the scanner's chain walk would find all of them "matching." But the real risk is: an empty key followed by a very large value could cause alignment to place the value at offset 16, and if `record_size_from_bytes` doesn't handle the `key_size = 4` case correctly (e.g., off-by-one in alignment), the value offset could be wrong.

#### Warrant (Rule)

Zero-length inputs are classic boundary conditions. The serialization format uses a 4-byte length prefix, so zero-length data is `[0x00, 0x00, 0x00, 0x00]` — 4 bytes. The `pad_alignment(8 + 4, 8) = 16` calculation is correct, but was it tested? The `record_size_from_bytes_zero_length_value` test uses `key = vec![1, 2, 3]` (non-zero key) with empty value. The complementary case (empty key, non-empty value) is untested.

#### Rebuttal Conditions

Not a concern if: empty keys are prevented at the API level (e.g., a `debug_assert!(!key.is_empty())` somewhere). Checking the diff: no such assertion exists.

#### Suggested Verification

Add a unit test: `record_size_from_bytes_zero_length_key`. Write a record with `key = Vec::<u8>::new()`, `value = vec![0xAB; 1000]`. Verify `record_size_from_bytes` returns the correct size. Also add an integration test: insert with an empty key, compact, verify the record survives.

---

### Finding 5: The `u32` cast in `serialized_size_from_bytes` can silently truncate on 16-bit or exotic platforms, and more critically, a corrupt u32 prefix value close to `usize::MAX` on 32-bit platforms wraps to a valid size

**Severity**: consider
**Confidence**: LOW
**Category**: testing

#### Grounds (Evidence)

In `Vec<u8>::serialized_size_from_bytes` (`traits.rs:317`):
```rust
let len = u32::from_le_bytes(...) as usize;
LENGTH_PREFIX_SIZE + len
```

On a 32-bit platform, `len` as `usize` can be up to `u32::MAX` = 4,294,967,295. `LENGTH_PREFIX_SIZE + len` = `4 + 4_294_967_295` = `4_294_967_299` which wraps to `3` on 32-bit (usize overflow). This `3` would be returned as the key size, which is small enough to pass all corruption checks, causing the scanner to compute a tiny record size and drift into corruption.

On 64-bit platforms this is not an issue (`usize` is 64 bits). But the code doesn't assert `cfg(target_pointer_width = "64")`.

No test runs on a 32-bit target or tests with length prefix values close to `u32::MAX` in a context where overflow matters.

#### Warrant (Rule)

Rust's `as usize` cast is a silent truncation on narrow platforms. For a storage engine, this is a data-integrity issue on embedded 32-bit systems. While FASTER is primarily a server-side engine, the code compiles on 32-bit targets and the Cargo.toml doesn't restrict target architectures.

#### Rebuttal Conditions

Not a concern if: (1) FASTER officially only supports 64-bit platforms. (2) The `record_size_from_bytes` corruption checks catch the resulting too-small size. But as analyzed above, a wrapped size of 3 would actually pass the `total > remaining` check (3 < page_size), causing silent corruption.

#### Suggested Verification

Either add `#[cfg(not(target_pointer_width = "64"))] compile_error!("FASTER requires a 64-bit platform");` to the crate root, or add a checked arithmetic guard: `let len = u32::from_le_bytes(...) as usize; assert!(len <= page_size)` in `serialized_size_from_bytes`.

---

### Finding 6: No integration test exercises compaction with records that have been updated (RMW/upsert) multiple times creating long version chains with variable-length records

**Severity**: consider
**Confidence**: MEDIUM
**Category**: testing

#### Grounds (Evidence)

The integration tests insert records once (`compact_variable_length_crud_roundtrip`) or delete them (`compact_variable_length_tombstones`). None of the 5 new integration tests exercise the scenario where a variable-length key is updated multiple times (creating a version chain of 3+ records with potentially different value sizes) before compaction.

The `VarLenFunctions::rmw_in_place` returns `NeedsNewRecord`, forcing every RMW to create a new copy-on-write record. If a key is updated 5 times, there are 5 records in the log for that key, each with a potentially different value size. The scanner must correctly identify only the latest version as live and all predecessors as dead. The chain walk via `is_current_version` must correctly traverse records of different sizes.

The existing fixed-size scanner tests (`scanner.rs` test #4 "updated records") cover this for `u64`, but no variable-length equivalent exists.

#### Warrant (Rule)

Version chains with heterogeneous record sizes are the unique challenge of variable-length compaction. If the chain walk in `is_current_version` incorrectly reads a key from a record with a different value size (affecting record layout), it could misidentify the record. The scanner uses `KEY_OFFSET = 8` which is constant regardless of value size, so the key read should be correct. But the `previous_address` field in `RecordInfo` points to the old version, and the chain walk needs to read the key at that old address — if the old record has a different total size, the `read_header_and_match_key_varlen` must correctly handle it.

#### Rebuttal Conditions

Not a concern if: the `KEY_OFFSET = 8` constant ensures that key reading is independent of value size (which it is — the key always starts at offset 8 regardless of value size). The chain walk reads `RecordInfo` (8 bytes) and then the key starting at offset 8, never needing to know the value size.

#### Suggested Verification

Add an integration test: insert 20 variable-length records, update each 3-5 times with different value sizes (e.g., 10 bytes → 200 bytes → 50 bytes), compact, verify only the latest values survive.

---

## Summary

The test suite's primary blind spot is **subtle corruption** — length prefix bitflips that produce valid-but-wrong sizes (Finding 1). The corruption tests only cover gross corruption (`u32::MAX`), which the bounds checks catch easily. A single-bit flip in a length prefix could cause "stride drift" in the scanner, misaligning all subsequent record reads without triggering any error. Secondary gaps include the `serialized_size_from_bytes` panic window on edge-case page boundaries (Finding 5 from premortem, reinforced here) and the lack of version-chain-with-variable-sizes integration testing (Finding 6). The type-safety gap from the premortem review (unconstrained `compact::<K, V>()` types) is also exploitable by an adversary who can influence the call site.
