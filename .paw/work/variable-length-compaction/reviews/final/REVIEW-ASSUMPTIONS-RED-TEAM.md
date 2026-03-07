# REVIEW-ASSUMPTIONS-RED-TEAM.md

**Perspective**: red-team
**Specialist**: assumptions
**Scenario**: An adversary wants to violate the system's assumptions to cause data corruption, crash (panic/abort), or denial-of-service during compaction.

---

## Threat Model

The adversary has the ability to:
1. **Corrupt stored data** — modify bytes on the storage device (bit flips, partial writes, malicious storage layer).
2. **Influence API parameters** — call `compact::<K, V>()` with incorrect type parameters (e.g., through a misconfigured application, a plugin system, or dynamic dispatch).
3. **Craft workload patterns** — insert records designed to trigger edge cases (zero-length keys, keys that hash-collide, maximum-length values).

The goal is to find assumptions that an adversary can externally influence to cause corruption or crash.

---

### Finding: Type confusion attack — `compact::<K, V>()` accepts arbitrary type parameters, allowing an attacker who controls the call to silently corrupt the hash index

**Severity**: must-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

In `rust/crates/faster-core/src/store/kv.rs:1441`:
```rust
pub fn compact<K: Key, V: Value>(&self) -> Result<CompactionResult, CompactionError>
```

The store is `FasterKv<F: Functions>` where `F::Key` and `F::Value` define the actual data types. But `compact()` accepts **any** `K: Key, V: Value` with no connection to `F::Key`/`F::Value`.

**Attack vector**: If an application exposes compaction through a configuration layer, plugin system, or CLI that dynamically selects types, an adversary could cause `compact::<Vec<u8>, Vec<u8>>()` to run on a store containing `u64` keys. The scanner at scanner.rs:~2889 calls `record_size_from_bytes::<Vec<u8>, Vec<u8>>()`, which reads a 4-byte length prefix at offset 8. For a `u64` key with value `0x0000_0000_DEAD_BEEF`, the first 4 bytes at offset 8 are `0xEF 0xBE 0xAD 0xDE` (little-endian) = `0xDEADBEEF = 3,735,928,559` — interpreted as a key length of 3.7 GB.

The `value_offset > remaining` check in `record_size_from_bytes` (layout.rs:~3471) catches this and returns `CorruptedRecord`, aborting compaction. So **the worst case with the current guards is denial-of-service (compaction always fails)**. But this is still a violation — legitimate compaction is permanently blocked.

**Worse scenario**: If `K=u64` (correct key) but `V=Vec<u8>` (wrong value) on a store with `V=u64`, the key size computation is correct (8 bytes), but the value is interpreted with a 4-byte length prefix. The first 4 bytes of the u64 value become the "length" — for small u64 values (e.g., `42 = 0x2A000000` in LE = `0x0000002A`), this gives `len=42`, `value_size=46`. If the page has enough space, `record_size_from_bytes` returns a plausible-but-wrong total size. The scanner advances past the "record" and lands in the middle of the next actual record. Depending on what bytes are there, this could cascade — corrupting the entire CompactionPlan with garbage addresses and sizes. The copier would then copy garbage to the log tail, and the address updater would swing hash index pointers to corrupted data.

#### Warrant (Rule)

The assumption is that callers always provide matching types. This assumption is externally influenceable — the type parameters are provided by the caller, not derived from the store's own type system. A defense-in-depth approach would use the store's own `F::Key`/`F::Value` types, making the correct types the only possible types.

#### Rebuttal Conditions

This is NOT a concern if: (1) `compact()` is only called from internal code that always uses `F::Key`/`F::Value`; or (2) the method is changed to `compact(&self)` (no type params) and uses `F::Key`/`F::Value` directly; or (3) a `where K: Into<F::Key>` or `TypeId` assertion prevents misuse.

#### Suggested Verification

Add `pub fn compact(&self) -> Result<...>` that uses `F::Key` and `F::Value` directly. Deprecate the generic `compact::<K, V>()` or add `#[doc(hidden)]` + runtime TypeId check. Test: call `compact::<String, String>()` on a `u64` store and verify it returns an error rather than corrupting data.

---

### Finding: Crafted length prefix that fits within page bounds but misaligns all subsequent records — a single corrupted byte poisons the entire compaction plan

**Severity**: must-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

`record_size_from_bytes` in layout.rs:~3450-3484 validates that `total <= remaining`, but it does NOT validate that the computed key/value sizes match the actual data. An adversary who can flip a single bit in a length prefix can produce a size that is plausible (fits on the page) but wrong.

**Attack**: On a page with 10 records of 24 bytes each (u64+u64, total 240 bytes on a 4096-byte page), corrupt the first record's key length prefix from `0x08 0x00 0x00 0x00` (8 bytes, correct for u64) to `0x10 0x00 0x00 0x00` (16 bytes). For `Vec<u8>` scanning:
- `key_size = 4 + 16 = 20`
- `value_offset = pad(8 + 20, 8) = 32`
- Reads value prefix at offset 32 (which is actually the middle of record 2)
- Gets some plausible value length → returns a total that advances past multiple real records

The scanner then advances to an arbitrary mid-record position. Every subsequent "record" is read from misaligned offsets. Some may appear valid (the `RecordInfo` header at a wrong offset might have valid-looking bits). These phantom records get added to `live_records`, the copier copies garbage bytes, and the address updater swings hash pointers to corrupted data.

The abort-on-corruption design (T-2) only triggers for clearly invalid sizes. A single-bit flip that produces a plausible-but-wrong size bypasses all validation and cascades silently.

#### Warrant (Rule)

Fixed-stride scanning is self-healing — if one record is corrupted, the next record is still at `offset + fixed_stride` because the stride is a compile-time constant. Variable-stride scanning loses this property because the stride depends on data that can be corrupted. This is a fundamental trade-off acknowledged in the planning review (MF-3: "the self-healing property of fixed-stride scanning is lost"), but the implementation has no mitigation for plausible-but-wrong sizes.

#### Rebuttal Conditions

This is NOT a concern if: (1) checksums are added per-record or per-page to detect silent corruption; or (2) the scanner cross-validates record sizes against hash index entries (records in the index should have consistent sizes); or (3) storage-layer checksums (e.g., ZFS, dm-integrity) guarantee bit-level integrity before FASTER sees the data.

#### Suggested Verification

Implement a record checksum or use existing RecordInfo bits to store a size hint. Alternatively, add a post-scan validation step that verifies the total bytes of live_records + dead_records + tombstones equals `total_bytes_scanned` and that `total_bytes_scanned` is close to `(until - begin)`. A significant mismatch indicates corruption.

---

### Finding: `read_header_and_match_key_varlen` trusts hash chain addresses and can read past page boundaries via `safe_record_size` returning `max(remaining, min_size)`

**Severity**: should-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

In `record_ops.rs:38-43`:
```rust
fn safe_record_size(addr: LogicalAddress, min_size: u32) -> u32 {
    let page_size = 1u32 << OFFSET_BITS;
    let offset = addr.offset().0;
    let remaining = page_size.saturating_sub(offset);
    remaining.max(min_size)
}
```

Called from `read_header_and_match_key_varlen` (line 551):
```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32); // min_size=8
```

**Attack**: An adversary corrupts a record's `previous_address` field (stored in the lower 48 bits of `RecordInfo`). They set it to an address near the end of a page — say, offset `page_size - 2`. When the scanner's `is_current_version` walks the chain and reaches this address:
- `safe_record_size` returns `max(2, 8) = 8`
- `RecordAccessor::from_log(allocator, addr, 8)` creates an accessor for 8 bytes starting at an address with only 2 bytes on-page
- `get_physical_address(addr)` returns a pointer to the page frame at that offset
- The accessor reads 8 bytes, but only 2 are on the current page frame; the remaining 6 come from whatever follows in physical memory (possibly the next page frame, possibly unmapped memory)
- `record_info()` reads a `u64` from these 8 bytes — potentially garbage
- `eq_from_bytes` reads key data starting at offset 8, which is entirely in the garbage region

Best case: the chain walk continues with a garbage `previous_address`, eventually hitting an invalid address and returning `false` (conservative behavior). Worst case on some allocator layouts: a segfault from reading unmapped memory, or a false key match causing a live record to be classified as dead (data loss).

#### Warrant (Rule)

The assumption is that all addresses in hash chain entries point to valid record positions within page boundaries. This is enforced by the allocator during writes, but a single corrupted `previous_address` field violates it. The `safe_record_size` function's use of `max` instead of `min` means it requests MORE bytes than the page has, which is the opposite of "safe."

#### Rebuttal Conditions

This is NOT a concern if: (1) `get_physical_address` bounds-checks the access to the page frame and returns `None` for out-of-bounds offsets; or (2) page frames are always followed by valid memory (e.g., mmap'd contiguously); or (3) the `is_current_version` chain walk has independent validation that prevents acting on garbage data.

#### Suggested Verification

Change `safe_record_size` to return `remaining.min(page_size)` (never exceed remaining). Add a check: `if remaining < RECORD_HEADER_SIZE as u32 { return None; }` in `read_header_and_match_key_varlen` before calling `from_log`. Test with a corrupted `previous_address` pointing to `page_size - 1`.

---

### Finding: `serialized_size_from_bytes` for variable-length types panics on buffers shorter than 4 bytes — an adversary who can cause a short buffer triggers a process abort

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: assumptions

#### Grounds (Evidence)

In `traits.rs:320-327` (Vec<u8> Key):
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

The `.expect()` panics if `buf.len() < 4`. The `record_size_from_bytes` wrapper provides `MIN_READABLE=12` guard for the key buffer, but for the **value** buffer the guard is `value_offset <= remaining`, meaning `value_buf = &page_bytes[offset + value_offset..]` has `remaining - value_offset` bytes. If `remaining - value_offset >= 0` but `< 4`, the value's `serialized_size_from_bytes` panics.

**Attack path**: Consider `remaining = 20` (valid record region), key_size correctly computed as `4 + 4 = 8` (Vec<u8> with 4-byte payload). `value_offset = pad(8 + 8, 8) = 16`. `value_buf` has `20 - 16 = 4` bytes — just barely enough. But if key_size were `4 + 5 = 9`, then `value_offset = pad(8 + 9, 8) = 24 > 20` → caught by the `value_offset > remaining` check. So the guard works for this path.

However, for `value_offset = remaining` exactly, the value buffer has 0 bytes. The check is `value_offset > remaining` (strictly greater), so `value_offset == remaining` passes! Then `value_buf = &page_bytes[offset + remaining..]` which is a zero-length slice. Calling `serialized_size_from_bytes` on a zero-length buffer panics.

This requires: `pad_alignment(key_offset + key_size, RECORD_ALIGNMENT) == remaining`, which is achievable with crafted key sizes.

#### Warrant (Rule)

The boundary condition `value_offset > remaining` should be `value_offset >= remaining` (or `value_offset + LENGTH_PREFIX_SIZE > remaining`) to prevent a zero-length value buffer from reaching the panic. A single `>` vs `>=` error in a bounds check is a classic off-by-one that an adversary can exploit if they control the stored data.

#### Rebuttal Conditions

This is NOT a concern if: (1) `pad_alignment(key_offset + key_size, 8) == remaining` is impossible given valid records (it would mean zero bytes for the value, which for fixed-size types like u64 would make the total record larger than remaining — but for variable-length types, a zero-byte value prefix alone takes 4 bytes, so value_offset + 4 > remaining... wait, if value_offset == remaining, then no 4 bytes exist). Actually, this IS exploitable for variable-length: the check should be `value_offset + LENGTH_PREFIX_SIZE > remaining` for variable-length types. For fixed-size types, `serialized_size_from_bytes` ignores the buffer, so the panic doesn't happen.

#### Suggested Verification

Change the guard in `record_size_from_bytes` from `if value_offset > remaining` to `if value_offset + LENGTH_PREFIX_SIZE > remaining` (or simply `if value_offset >= remaining`). Add a test: create a page where `pad_alignment(key_offset + key_size, 8)` equals exactly the remaining space, and verify `record_size_from_bytes` returns `CorruptedRecord` instead of panicking.

---

### Finding: An adversary can cause unbounded memory allocation by inserting records with crafted tombstone patterns that fill `tombstone_records` during compaction

**Severity**: consider
**Confidence**: MEDIUM
**Category**: assumptions

#### Grounds (Evidence)

The scanner at scanner.rs:~2912-2918 collects tombstone records into `plan.tombstone_records: Vec<LiveRecord>`:
```rust
if info.is_tombstone() {
    plan.tombstone_count += 1;
    plan.tombstone_records.push(LiveRecord {
        address: current,
        record_size,
    });
}
```

`LiveRecord` is 16 bytes (address + usize). A compaction region with millions of small records that are all tombstoned would allocate `n * 16` bytes. For a 1GB compaction region with 24-byte records: ~44 million tombstones × 16 bytes = ~700 MB of vector allocation.

The planning review noted this as C-3 (Consider: "tombstone vector memory scaling") with a suggestion to "log warning if > 100MB." But the implementation has no such warning or cap.

**Attack**: An adversary with write access to the store inserts millions of small records, then deletes all of them, creating a tombstone-heavy region. When compaction runs, the `tombstone_records` vector grows unbounded, potentially causing OOM.

#### Warrant (Rule)

The assumption is that tombstone counts are manageable. For most workloads this is true — tombstones represent recent deletes, and compaction runs frequently enough to keep the count reasonable. But for a delete-heavy workload (e.g., a cache with TTL-based eviction), the compaction region could be dominated by tombstones.

#### Rebuttal Conditions

This is NOT a concern if: (1) the compaction region size is bounded by policy (it is — safe_read_only_address limits it); or (2) the application monitors memory usage and avoids compacting during high-delete periods; or (3) a tombstone count cap with fallback to re-reading is implemented.

#### Suggested Verification

Add a `warn!()` log if `tombstone_records.len() > 1_000_000`. Consider pre-allocating with a capacity hint based on estimated region size. Document the memory overhead in the `CompactionPlan` struct.
