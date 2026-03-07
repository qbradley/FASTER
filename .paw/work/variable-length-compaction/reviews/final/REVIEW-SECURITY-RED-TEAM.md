# REVIEW-SECURITY-RED-TEAM.md

**Specialist**: Security
**Perspective**: red-team
**Diff**: `git diff squad..feature/variable-length-compaction`

---

## Attacker Model

The attacker has the ability to write crafted key-value pairs to the FASTER store (e.g., as a tenant in a multi-tenant system, or through an application that stores user-supplied data). The attacker's goal is to cause denial-of-service, data corruption, or information leakage during the compaction phase. The attacker cannot directly modify page bytes but can influence what gets written to the log through the store's normal API.

---

### Finding: Attacker-controlled key lengths can force compaction into quadratic memory consumption via unbounded `tombstone_records` and `live_records` vectors

**Severity**: should-fix
**Confidence**: HIGH
**Category**: security
**Perspective**: red-team

#### Grounds (Evidence)

The `CompactionPlan` struct (compaction/mod.rs) contains:

```rust
pub struct CompactionPlan {
    pub live_records: Vec<LiveRecord>,
    pub tombstone_records: Vec<LiveRecord>,
    // ...
}
```

Each `LiveRecord` is ~16 bytes (address + record_size). The scanner pushes to both vectors without any capacity limit. An attacker who writes millions of small records (e.g., 1-byte keys, 0-byte values, total record size ≈ 24 bytes with padding) and then deletes all of them creates a compaction region where:

- Every record is a tombstone → pushed to `tombstone_records`
- With a 1 GiB compaction region and 24-byte records: ~44.7 million tombstone entries
- Memory: 44.7M × 16 bytes = ~715 MB just for the tombstone vector

The planning review (REVIEW-SYNTHESIS.md, C-3) noted this risk but it was classified as "consider" with no mitigation implemented. From a red-team perspective, this is a memory exhaustion DoS attack that an application user can trigger through normal API usage.

Additionally, in the scanner (scanner.rs), when records are classified as live, the key is deserialized (`K::deserialize(&bytes[KEY_OFFSET..])`) into a full `K` value for hash lookup. For `Vec<u8>` keys, each deserialization allocates on the heap. An attacker writing keys of size, say, 10KB each forces 10KB × N_records heap allocations during the scan phase. With 1M records: 10 GB of heap pressure just for key deserialization during scanning.

The `eq_from_bytes` optimization only helps during version chain walks, not during the initial deserialization of the scanned key for hash computation.

#### Warrant (Rule)

An unbounded memory allocation driven by user-controlled data volume is a classic resource exhaustion attack. The compaction pipeline holds an epoch guard during scanning, which prevents page eviction. Combining epoch hold + unbounded memory growth creates a compound resource exhaustion: memory pressure from vectors + inability to free pages held by the epoch.

#### Rebuttal Conditions

This is NOT a concern if: (1) the compaction region size is bounded (it is — from begin_address to safe_read_only_address), but this region can still be very large in a write-heavy workload; or (2) the application layer limits the number of records that can be written before compaction, imposing backpressure. This is a library crate with no built-in backpressure mechanism.

#### Suggested Verification

Profile memory usage during compaction with 10M small tombstone records. Consider adding a configurable cap on `tombstone_records.len()` (with fallback to re-reading from old addresses) and pre-allocating vectors with estimated capacity.

---

### Finding: Attacker-crafted key that produces `serialized_size_from_bytes` returning a value that exactly matches `remaining` can force `value_buf` to be a zero-length slice, panicking in `V::serialized_size_from_bytes`

**Severity**: must-fix
**Confidence**: HIGH
**Category**: security
**Perspective**: red-team

#### Grounds (Evidence)

In `record_size_from_bytes` (layout.rs:~465-483):

```rust
let key_buf = &page_bytes[offset + key_offset..];
let key_size = K::serialized_size_from_bytes(key_buf);
let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);

if value_offset > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}

let value_buf = &page_bytes[offset + value_offset..];
let value_size = V::serialized_size_from_bytes(value_buf);
```

The check is `value_offset > remaining` (strictly greater). When `value_offset == remaining`, the check passes, and `value_buf = &page_bytes[offset + value_offset..]` has length `page_size - (offset + value_offset) = page_size - offset - remaining = 0` (since `remaining = page_size - offset`).

Wait — `value_buf = &page_bytes[offset + value_offset..]`. The slice length is `page_bytes.len() - (offset + value_offset)`. If `page_bytes.len() == page_size` and `value_offset == remaining == page_size - offset`, then `offset + value_offset = offset + page_size - offset = page_size`. So `value_buf` is a zero-length slice.

`V::serialized_size_from_bytes` for `Vec<u8>` does `buf[..4].try_into().expect(...)` — this panics on a zero-length buffer.

**Attack construction**: The attacker writes a `Vec<u8>` key whose serialized size, after padding, causes `value_offset` to exactly equal the page remainder. This requires:
1. The record is placed near the end of a page by the allocator
2. The key size satisfies: `pad_alignment(8 + (4 + key_data_len), 8) == page_remaining`

For example, with page_remaining = 40 bytes and RECORD_ALIGNMENT = 8:
- `key_offset = 8`
- Need `pad_alignment(8 + key_serialized_size, 8) = 40`
- `key_serialized_size = 32` → `pad(8 + 32, 8) = 40` ✓
- `key_data_len = 28` (4-byte prefix + 28 bytes data = 32)

The attacker writes a 28-byte key. If the allocator places this record at an offset where page_remaining = 40, the scanner panics.

The attacker doesn't control record placement directly, but with enough writes, the allocator will eventually place records at various offsets. The attacker can also exploit this by writing records of sizes calibrated to land near page boundaries.

#### Warrant (Rule)

This is a bounds-checking gap at a trust boundary. The `record_size_from_bytes` function checks `value_offset > remaining` but should check `value_offset >= remaining` (or equivalently, `value_offset + MIN_VALUE_READABLE > remaining`). The off-by-one allows a zero-length buffer to reach `serialized_size_from_bytes`, which panics. An attacker who can write key-value pairs to the store can craft keys that trigger this panic during the next compaction cycle, causing a denial-of-service.

#### Rebuttal Conditions

This is NOT a concern if: (1) the allocator never places a record where `value_offset == page_remaining` — but the allocator's page-fitting logic may allow it if the record's total size fits on the page but the key alone fills it to the boundary; or (2) `serialized_size_from_bytes` is changed to return a Result instead of panicking (which would be the complete fix). This finding is about the **combination** of the off-by-one in `record_size_from_bytes` AND the panic in `serialized_size_from_bytes`.

#### Suggested Verification

Write a unit test with a carefully sized key that causes `value_offset == remaining` and verify the function returns `Err` instead of panicking. Fix: change the check to `value_offset + LENGTH_PREFIX_SIZE > remaining` (need at least 4 bytes to read the value prefix).

---

### Finding: Version chain walk via `read_header_and_match_key_varlen` follows attacker-controllable `previous_address` pointers with only a depth limit, no address range validation

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: security
**Perspective**: red-team

#### Grounds (Evidence)

In `scanner.rs:285-310`, the `is_current_version` function walks the version chain:

```rust
while current.is_valid() && depth < MAX_CHAIN_DEPTH {
    depth += 1;
    match reader.read_header_and_match_key_varlen::<K>(current, key) {
        Some((ri, key_matches)) => {
            // ...
            current = ri.previous_address();
        }
        None => return true, // conservative
    }
}
```

The `previous_address()` is read from the `RecordInfo` header of each record in the chain. If an attacker writes records that create long hash chains (via hash collisions in the 1024-bucket index), the scanner walks up to `MAX_CHAIN_DEPTH = 4096` records per scanned record.

Attack: With N scanned records and deliberate hash collisions, the scanner performs up to N × 4096 chain hops. Each hop calls `read_header_and_match_key_varlen`, which calls `RecordAccessor::from_log` (allocator memory access) and `key.eq_from_bytes` (key comparison). For a compaction region with 1M records all in the same hash bucket, the scanner performs up to 4 billion chain hops — a computational DoS.

Additionally, `read_header_and_match_key_varlen` calls `safe_record_size(addr, RECORD_HEADER_SIZE)` which uses the address's offset to compute `remaining`. A corrupted `previous_address` pointing **outside the compaction region** (but within the valid log range) would cause the scanner to read records from arbitrary log locations. This doesn't cause a memory safety violation (the allocator validates the address), but it does cause the scanner to make liveness decisions based on records outside the compaction region.

#### Warrant (Rule)

Attacker-controlled data flow (hash collisions) drives the computational cost of the scanner. While `MAX_CHAIN_DEPTH = 4096` bounds the per-record cost, it doesn't bound the total cost across all records. A patient attacker who understands the hash function can construct key sets that maximize chain walk time. The 4096-depth limit is a safety valve for corrupted chains but is insufficient against deliberate collision attacks that create long but valid chains within the depth limit.

#### Rebuttal Conditions

This is NOT a concern if: (1) the hash function is cryptographically strong against collision attacks — but FASTER typically uses fast non-cryptographic hashes for performance; or (2) the application layer prevents an attacker from controlling key values (which is application-dependent); or (3) the 4096 depth limit combined with the bounded compaction region makes the total cost acceptable for the deployment scenario.

#### Suggested Verification

Benchmark the scanner with a worst-case hash collision workload: 10K keys, all hashing to the same bucket. Measure wall-clock time and compare to the expected compaction SLA. If the ratio exceeds 10×, consider adding a per-bucket chain length metric that surfaces during compaction.

---

### Finding: `key.hash()` is computed from deserialized key data in the scanner hot path — a hash collision attack against the hash function becomes a compaction amplification attack

**Severity**: consider
**Confidence**: MEDIUM
**Category**: security
**Perspective**: red-team

#### Grounds (Evidence)

In `scanner.rs` (diff line ~2924-2925), for each non-tombstone, non-invalid record:

```rust
let key: K = K::deserialize(&bytes[KEY_OFFSET..]);
if self.is_current_version(&key, current) {
```

`is_current_version` calls `key.hash()` to find the hash bucket, then walks the chain. The key is fully deserialized (heap allocation for `Vec<u8>`) and hashed. For `Vec<u8>`, the hash computation is O(key_length).

An attacker who writes keys of maximum size (limited only by page size) forces:
1. Large heap allocations per scanned record (key deserialization)
2. Expensive hash computations (linear in key size)
3. Large comparison buffers during chain walks

A 10KB key on a 32KB page means each record's scan requires 10KB allocation + 10KB hash + potentially 4096 × 10KB comparisons in the chain walk. The `eq_from_bytes` optimization avoids allocation during chain walks, but the initial deserialization and hash computation remain.

#### Warrant (Rule)

The combination of key deserialization + hashing + chain walking creates an amplification factor that an attacker can exploit. The per-record cost is: O(key_size) for deserialization + O(key_size) for hashing + O(chain_depth × key_size) for chain comparisons. For maximum-sized keys in collision chains, this is O(page_size × MAX_CHAIN_DEPTH) per scanned record. The total cost is O(N × page_size × MAX_CHAIN_DEPTH), which can be orders of magnitude more expensive than the expected O(N × key_size) for well-distributed keys.

#### Rebuttal Conditions

This is NOT a concern if: (1) the application layer bounds key sizes to reasonable values; or (2) the compaction region size is bounded and compaction runs are time-limited; or (3) the hash function distributes keys well enough that collision chains are short in practice (which is true for random keys but not for attacker-chosen keys with non-cryptographic hashes).

#### Suggested Verification

Add a metric that tracks the maximum chain depth encountered during compaction scanning. If it exceeds a threshold (e.g., 100), log a warning indicating potential hash collision attack or poor hash distribution.

---

### Finding: The `record_size_from_bytes` function does not validate that `page_bytes.len() >= page_size`, trusting the caller to provide a correctly-sized buffer

**Severity**: consider
**Confidence**: LOW
**Category**: security
**Perspective**: red-team

#### Grounds (Evidence)

In `layout.rs:~450-484`:

```rust
pub fn record_size_from_bytes<K: Key, V: Value>(
    page_bytes: &[u8],
    offset: usize,
    page_size: usize,
) -> Result<usize, RecordSizeError> {
    let remaining = page_size.saturating_sub(offset);
    // ...
    let key_buf = &page_bytes[offset + key_offset..];
```

The function takes `page_size` as a parameter but uses `page_bytes` as the actual data source. If `page_bytes.len() < page_size` (a caller bug), the slice `&page_bytes[offset + key_offset..]` could be shorter than expected, causing the validation checks (which use `remaining` derived from `page_size`) to pass while the actual buffer is too short.

However, in the scanner's usage, `page_bytes` comes from `RecordAccessor::as_slice()` which is constructed with `remaining` as the record size (scanner passes `remaining` to `RecordAccessor::from_log`), so `page_bytes.len() == remaining`. The `record_size_from_bytes` call then uses `offset=0` and `page_size=remaining`. So `page_bytes.len() == page_size` holds.

#### Warrant (Rule)

Functions that operate on raw byte buffers with a separate size parameter should validate that the buffer is at least as large as the declared size. This is a defense-in-depth measure against future refactoring that might change the caller's buffer construction.

#### Rebuttal Conditions

This is NOT a concern if: (1) all callers always provide `page_bytes.len() >= offset + page_size` — which is true for the current scanner usage; or (2) a `debug_assert!(page_bytes.len() >= page_size)` is added at the function entry.

#### Suggested Verification

Add `debug_assert!(page_bytes.len() >= page_size, "page_bytes buffer shorter than declared page_size")` at the start of `record_size_from_bytes`.

---

## Attack Chain Summary

The highest-impact attack chain combines findings 1 and 2:

1. **Setup**: Attacker writes variable-length key-value pairs through the normal store API, choosing key sizes that maximize the probability of a record landing at a page offset where `value_offset == page_remaining`.

2. **Trigger**: Application triggers compaction (either manually or via auto-compact policy).

3. **Exploit**: Scanner calls `record_size_from_bytes` on the attacker's record. The key length prefix produces `value_offset == remaining`, passing the `>` check. `V::serialized_size_from_bytes` receives a zero-length buffer and panics via `expect()`.

4. **Impact**: Process crash. If compaction is retried automatically, the same record triggers the same panic — **persistent denial-of-service** until the corrupted/adversarial data is manually removed or the compaction region is skipped.

5. **Amplification**: Because compaction aborts without advancing `begin_address`, the log grows without bound, eventually exhausting disk space — converting a crash-loop DoS into a storage exhaustion DoS.

This chain is realistic for any multi-tenant application that stores user-supplied data in FASTER and runs periodic compaction.
