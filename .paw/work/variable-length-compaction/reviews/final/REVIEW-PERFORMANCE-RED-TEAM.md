# Performance Review — Red-Team Perspective

**Perspective**: red-team
**Reviewer**: Performance Specialist
**Date**: 2025-07-15
**Diff scope**: Variable-length record compaction (~3,400 lines, 21 files)

---

## Scenario

An adversary has write access to the FASTER database (can insert arbitrary keys/values) and wants to cause performance degradation — DoS through algorithmic complexity, cache thrashing, or resource exhaustion. What attack vectors does the variable-length compaction implementation expose?

---

### Finding: Adversary-crafted large key length prefixes trigger O(N) allocation during scan via `K::deserialize`, enabling heap exhaustion DoS

**Severity**: must-fix
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff line 2924):

```rust
let key: K = K::deserialize(&bytes[KEY_OFFSET..]);
```

For `Vec<u8>` keys, `K::deserialize` reads a 4-byte length prefix and allocates `len` bytes:

```rust
// traits.rs:310-317
fn deserialize(buf: &[u8]) -> Self {
    let len = u32::from_le_bytes(buf[..4].try_into()...) as usize;
    buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec()
}
```

**Attack vector**: An adversary inserts keys with sizes near the page-remaining limit. The `record_size_from_bytes` corruption check (layout.rs, diff line 3479) validates that `total_size <= remaining`, so a 32-byte key length prefix that claims `len = 33,554,400` (just under 32 MB page size minus overhead) would pass the corruption check if the record genuinely occupies that space. The scanner then heap-allocates a ~32 MB `Vec<u8>` for deserialization, which is immediately discarded after hashing.

Even without approaching the page limit, an adversary inserting 100K records with 10 KB keys would cause the scanner to allocate ~1 GB of transient heap memory during a single compaction cycle (100K × 10 KB). This amplifies the adversary's write bandwidth into an order-of-magnitude larger memory pressure during compaction.

**Amplification factor**: The adversary writes N bytes of key data. The scanner deserializes each key in the scan region, allocating N bytes, then the address updater deserializes each live/tombstone key again for another N bytes. Total amplification: ~2× the adversary's stored data volume in transient allocations.

#### Warrant (Rule)

Compaction is typically a background operation that shares memory with the foreground workload. If an adversary can trigger compaction (directly via `compact()` or indirectly by filling the log to trigger auto-compaction), they can force the system to allocate memory proportional to the stored key data during the scan phase. With no per-key allocation cap in the scanner, the transient memory spike could exceed available RAM and cause OOM.

The corruption detection only checks that the record fits within the page — it does not limit individual key or value sizes. A "valid" 10 MB key inside a 32 MB page passes all checks.

#### Rebuttal Conditions

This is NOT a concern if:
1. The FASTER store enforces maximum key/value sizes at write time (upstream of compaction). Check the upsert path for size limits.
2. Compaction runs in a memory-limited sandbox (e.g., cgroup with memory limit).
3. Only fixed-size types are used (no heap allocation during deserialization).

#### Suggested Verification

1. Insert 1000 records with 1 MB `Vec<u8>` keys, trigger compaction, and measure peak RSS. If RSS spikes by >2 GB (1000 × 1 MB × 2 passes), this confirms the amplification.
2. Add a `MAX_KEY_SIZE` / `MAX_VALUE_SIZE` validation in `record_size_from_bytes` that treats oversized fields as corruption.

---

### Finding: Hash collision flooding with variable-length keys degrades scanner's `is_current_version` to O(chain_depth × key_size) per record

**Severity**: should-fix
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff line 2973), the chain walk uses `read_header_and_match_key_varlen`:

```rust
match reader.read_header_and_match_key_varlen::<K>(current, key) {
```

Which calls `key.eq_from_bytes()` per hop (record_ops.rs:555):

```rust
let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
```

For `Vec<u8>`, `eq_from_bytes` (traits.rs, diff line 3887-3895):

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    len == self.len()
        && buf.len() >= LENGTH_PREFIX_SIZE + len
        && buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len] == self[..]
}
```

The byte comparison `buf[4..4+len] == self[..]` is O(key_size) for each non-matching hop.

**Attack vector**: An adversary crafts keys that all hash to the same bucket (hash collision flooding). With a hash table of `1 << 10 = 1024` buckets, inserting 100K records where all keys hash to the same value creates chains of depth 100K. During compaction, for each record in that bucket, `is_current_version` walks the chain, performing `eq_from_bytes` at each hop.

**Complexity**: For N records in a single chain with key size S:
- Per-record classify cost: O(chain_depth × S) for key comparison
- Total scan cost: O(N² × S) for all records in the chain
- With N=100K and S=1KB: 100K × 100K × 1KB = 10 TB of memory comparisons

Even with `MAX_CHAIN_DEPTH = 4096`, the adversary can still cause: 100K records × 4096 hops × 1KB key = 400 GB of comparison work per scan.

#### Warrant (Rule)

Hash collision attacks are well-known (HashDoS). The `MAX_CHAIN_DEPTH = 4096` limit (scanner.rs line 2773) bounds the per-record cost, but the total cost is still N × 4096 × S for all N records that hash to the same bucket. With variable-length keys, the comparison cost S is adversary-controlled, unlike fixed-size types where S is constant (8 bytes for u64).

The combination of hash collision flooding + large variable-length keys creates a multiplicative amplification that was impossible with fixed-size-only compaction.

#### Rebuttal Conditions

This is NOT a concern if:
1. The hash function is cryptographic or has collision resistance guarantees (check if `Hashable` uses a salted hash).
2. The hash table uses a resize/rehash strategy that limits chain depth (check if `HashIndex` has a load factor trigger).
3. Real-world key distributions are unlikely to produce collisions (true for uniform distributions, not for adversarial inputs).

#### Suggested Verification

1. Examine the `Hashable` implementation for `Vec<u8>` — if it uses a non-randomized hash (e.g., FNV, xxHash without per-instance salt), collision flooding is practical.
2. Insert 10K keys that collide into 1 bucket, trigger compaction, and measure wall-clock time vs. 10K uniform keys. If the collision case is >100× slower, this confirms the attack.
3. Consider adding per-chain-walk timeout or abort logic to the scanner.

---

### Finding: Corrupted record injection at strategic page offsets forces repeated full-compaction abort, creating a liveness DoS

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff lines 2896-2899):

```rust
Err(e @ RecordSizeError::CorruptedRecord { .. }) => {
    // MF-3: Abort compaction on corruption (T-2 decision).
    return Err(e);
}
```

The scanner aborts the **entire** compaction cycle on any corrupted record. The orchestrator surfaces this as `CompactionError::ScanCorruption` (orchestrator.rs, diff line 2704).

**Attack vector**: If an adversary can corrupt a single record's length prefix near the beginning of the compaction region (e.g., via a race condition, direct memory manipulation, or a bug in the write path), every subsequent compaction attempt on that region will fail. The original log region is left untouched (as documented), which means:

1. Dead records in the corrupted region are never reclaimed.
2. The begin-address is never advanced past the corruption.
3. The log grows without bound as new data accumulates at the tail.
4. Eventually, disk space (or memory for in-memory stores) is exhausted.

**Amplification**: A single corrupted byte (one bit-flip in a 4-byte length prefix) permanently blocks compaction for the entire region. The system has no mechanism to skip the corrupted record and continue compacting the remaining healthy records.

#### Warrant (Rule)

The abort-on-corruption design is a deliberate safety decision (documented in the scanner module docs, diff lines 2751-2760). It's the correct conservative choice to avoid silent data loss. However, from a liveness perspective, it creates a permanent DoS if corruption occurs.

The recovery strategy described in the docs — "skip the corrupted segment, run a consistency check, or restore from a checkpoint" — requires manual operator intervention. In an automated system, this means compaction is permanently blocked until a human notices and acts.

#### Rebuttal Conditions

This is NOT a concern if:
1. The application-level code handles `ScanCorruption` errors by advancing the begin-address past the corrupted page (skip strategy).
2. The system has checksums on records that detect corruption before it reaches the scanner.
3. Corruption is detected and repaired by a separate background process (e.g., scrubber).

The diff itself acknowledges this is a design trade-off (safety over liveness). The concern is that no automated recovery path is provided.

#### Suggested Verification

1. Corrupt a single record's length prefix in a test, trigger compaction repeatedly, and verify that the error is surfaced clearly enough for automated handling.
2. Consider adding a `CompactionOptions::skip_corrupted_records` flag that allows the scanner to skip corrupted records (marking them as dead) with a log warning, as an opt-in for liveness-critical deployments.

---

### Finding: Variable-stride scanning prevents CPU hardware prefetcher effectiveness, enabling cache-thrashing attacks with adversary-chosen record sizes

**Severity**: consider
**Confidence**: MEDIUM
**Category**: performance

#### Grounds (Evidence)

The scanner iterates with a data-dependent stride (diff lines 2887-2900):

```rust
let record_size = match record_size_from_bytes::<K, V>(bytes, 0, remaining as usize) {
    Ok(size) => size,
    // ...
};
// ... advance by record_size
advance(&mut current, record_size as u32, page_size);
```

For fixed-size records, the hardware prefetcher detects the constant stride pattern and prefetches ahead. For variable-length records, each stride is different.

**Attack vector**: An adversary alternates between very small records (e.g., 24 bytes = minimum) and very large records (e.g., 16 KB). This creates a stride pattern like: 24, 16384, 24, 16384, ... The hardware prefetcher cannot predict this bi-modal pattern, causing nearly every record header access to miss L1/L2 cache.

**Impact estimate**: With ~50% L1d miss rate (vs. ~10% for constant stride), the per-record overhead increases from ~4ns to ~10ns. Over 130K records, that's ~0.8ms additional latency per scan. This is modest for a single compaction but could matter in tight SLA environments.

#### Warrant (Rule)

Hardware prefetchers on Intel CPUs (like the i9-10900K in the test environment) use a stride detection algorithm that tracks the last 2-4 strides per cache line stream. Adversary-chosen alternating strides defeat this pattern detection. The diff acknowledges this by commenting about software prefetch (scanner.rs, diff lines 2932-2937).

The impact is modest because the scanner is memory-bound on the page data anyway — the prefetch miss adds ~6ns per record, which is small compared to the ~30ns for key deserialization and ~20ns for hash comparison.

#### Rebuttal Conditions

This is NOT a concern if:
1. The scan is not latency-critical (it's a background operation).
2. The additional ~0.8ms per scan is within the compaction SLA budget.
3. Software prefetch is added in a future release (as documented in the comment).

**Verdict: This is a minor concern.** The ~0.8ms overhead is real but small relative to total compaction time.

#### Suggested Verification

Benchmark the scanner with adversary-chosen bi-modal record sizes (alternating 24B and 16KB) vs. uniform 256B records. If the performance difference is <2×, this is not worth mitigating.

---

### Finding: `eq_from_bytes` for Vec<u8> does not short-circuit on first byte mismatch, comparing full key length for near-miss keys

**Severity**: consider
**Confidence**: LOW
**Category**: performance

#### Grounds (Evidence)

In `traits.rs` (diff lines 3887-3895):

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

The slice comparison `buf[4..4+len] == self[..]` compiles to `memcmp`, which does short-circuit on the first differing byte in practice (libc implementations compare word-at-a-time). So this concern is likely a non-issue.

However, if an adversary crafts keys that share a long common prefix (e.g., `aaaa...aab` vs `aaaa...aac`, differing only in the last byte), `memcmp` must compare all `len-1` bytes before finding the mismatch.

**Attack vector**: Insert keys with long shared prefixes (e.g., 10KB keys that differ only in the last byte). During chain walks with hash collisions, each `eq_from_bytes` call must compare ~10KB before determining non-match.

**Impact**: Combined with hash collision flooding (Finding 2): N records × D chain depth × S key size comparisons where S is the adversary-chosen common prefix length. For N=10K, D=4096 (capped), S=10KB: 10K × 4096 × 10KB = 400 GB of byte comparisons — identical to Finding 2's calculation but emphasizing the key-content dependency.

#### Warrant (Rule)

This is a refinement of Finding 2 rather than a separate attack. The adversary controls both the hash collision rate (via hash value) and the comparison cost (via key content). The combination creates a multiplicative attack surface.

In practice, Rust's `memcmp` (`<[T]>::eq`) uses SIMD-accelerated comparison on x86_64, processing 16-32 bytes per cycle. At 10KB per comparison: ~300-600ns. At 10K × 4096 × 600ns = 24.6 seconds per compaction scan. This is already captured by Finding 2's analysis.

#### Rebuttal Conditions

This is NOT a concern beyond Finding 2 because:
1. The length check (`len == self.len()`) short-circuits on different-length keys — only same-length keys proceed to byte comparison.
2. `memcmp` is highly optimized on modern CPUs with SIMD.
3. This finding adds minimal incremental risk beyond hash collision flooding.

**Verdict: Subsumed by Finding 2.** The mitigation for hash collision flooding (randomized hash, chain depth limits) also mitigates this attack.

#### Suggested Verification

No additional verification beyond Finding 2's recommendations.

---

### Finding: `record_size_from_bytes` trusts length prefixes without upper-bound validation, allowing adversary to control memory access patterns

**Severity**: should-fix
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `layout.rs` (diff lines 3466-3477):

```rust
let key_buf = &page_bytes[offset + key_offset..];
let key_size = K::serialized_size_from_bytes(key_buf);
let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);

if value_offset > remaining {
    return Err(RecordSizeError::CorruptedRecord { offset });
}

let value_buf = &page_bytes[offset + value_offset..];
let value_size = V::serialized_size_from_bytes(value_buf);
let total = pad_alignment(value_offset + value_size, RECORD_ALIGNMENT);
```

For `Vec<u8>`, `serialized_size_from_bytes` reads:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    let len = u32::from_le_bytes(buf[..4].try_into()...) as usize;
    LENGTH_PREFIX_SIZE + len
}
```

The `len` value is read from untrusted data (the page bytes). While the corruption check at `if value_offset > remaining` catches the case where the computed offset exceeds page bounds, the intermediate value `key_size` can be up to `4 + u32::MAX` = ~4 GB. The `pad_alignment` computation on an inflated `key_size` would overflow on 32-bit targets (though FASTER targets 64-bit).

On 64-bit, `value_offset = pad_alignment(8 + 4_294_967_299, 8)` = 4,294,967,304. The check `4,294,967,304 > remaining` correctly triggers `CorruptedRecord`. So the corruption is caught.

However, the slice creation `&page_bytes[offset + key_offset..]` creates a sub-slice of the entire page from the key offset onward. For `serialized_size_from_bytes`, only 4 bytes are read, but the slice itself extends to the end of `page_bytes`. If `page_bytes` is not properly bounded (e.g., the scanner passes a slice of the entire page), this is fine — but if the page_bytes slice is shorter than expected, the index could panic.

#### Warrant (Rule)

The corruption detection is correctly structured: compute offsets → validate against remaining → proceed or error. The validation catches the adversary's inflated length prefixes before any out-of-bounds access occurs. This is a **well-designed defense**.

The remaining concern is that `serialized_size_from_bytes` for `Vec<u8>` calls `.expect("buffer too short...")` on `try_into()`, which panics if `buf` is shorter than 4 bytes. The scanner's `MIN_READABLE` check (12 bytes) ensures at least 4 bytes are available for the key prefix, but the value prefix read at `value_buf = &page_bytes[offset + value_offset..]` relies on `value_offset <= remaining` being checked first. Since the check is indeed performed before the value_buf access, this is safe.

**Verdict: The corruption detection is correct.** The adversary cannot cause out-of-bounds access or panics through crafted length prefixes. However, a `u32::MAX` length prefix will cause a large but caught overflow, wasting a few cycles on the corruption path.

#### Rebuttal Conditions

This is NOT a concern because:
1. The corruption check catches all oversized prefixes before memory access.
2. On 64-bit targets, the arithmetic doesn't overflow.
3. The expected panic path is never reached due to the MIN_READABLE guard.

**Verdict: The defense is sound.** No action needed.

#### Suggested Verification

The existing `record_size_from_bytes_corrupted_key_prefix` and `record_size_from_bytes_corrupted_value_prefix` tests in `layout.rs` (diff lines 3622-3649) verify this. Add a test with `len = u32::MAX - 4` to confirm no arithmetic overflow on 64-bit.

---

## Summary

| # | Finding | Severity | Confidence | Attack Complexity |
|---|---------|----------|------------|-------------------|
| 1 | Heap exhaustion via large key deserialization | must-fix | HIGH | Low — just insert large keys |
| 2 | Hash collision flooding × key_size amplification | should-fix | HIGH | Medium — requires hash collision knowledge |
| 3 | Corruption-based compaction liveness DoS | should-fix | MEDIUM | High — requires data corruption capability |
| 4 | Cache-thrashing via bi-modal record sizes | consider | MEDIUM | Low — just insert alternating sizes |
| 5 | Long-prefix key comparison amplification | consider | LOW | Subsumed by Finding 2 |
| 6 | Length prefix validation correctness | — | HIGH | Defense is sound; no vulnerability found |

**Top priority**: Finding 1 is the most practical attack — an adversary with write access can insert large keys that cause O(GB) transient memory allocation during compaction. Mitigation: add a `Key::hash_from_bytes` trait method to eliminate deserialization in the scan path, and/or add `MAX_KEY_SIZE` / `MAX_VALUE_SIZE` constants that `record_size_from_bytes` validates against.

**Secondary priority**: Finding 2 (hash collision flooding) is a classic DoS vector amplified by variable-length key comparison cost. Mitigation depends on the hash function's collision resistance — if using a non-randomized hash, this is a real risk. The `MAX_CHAIN_DEPTH = 4096` limit provides a per-record bound but not a per-scan bound.
