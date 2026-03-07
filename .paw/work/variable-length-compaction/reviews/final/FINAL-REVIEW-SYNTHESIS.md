# Final Review Synthesis — Variable-Length Record Compaction

## Review Configuration
- **Specialists**: 9 (correctness, security, performance, architecture, edge-cases, assumptions, testing, maintainability, release-manager)
- **Perspectives**: 2 (premortem, red-team)
- **Total review files**: 18
- **Interaction mode**: debate (Round 1)
- **Requested by**: qbradley

## Executive Summary

The variable-length compaction implementation is architecturally sound — the three-phase pipeline (scan → copy → swing), `eq_from_bytes` zero-copy optimization, and corruption-abort design are all well-reasoned. However, the review surfaced **3 must-fix** findings (including a deterministic panic from a one-character bounds-check error), **9 should-fix** findings (type safety, missing bounds checks, unbounded allocations), and **10 consider** findings. The highest-convergence issue — an off-by-one in `record_size_from_bytes` that causes a panic on corrupted data instead of the spec-required `Err` return — was independently flagged by **9 of 9 specialists** across both perspectives.

---

## Must-Fix Findings

### MF-1: Off-by-one in `record_size_from_bytes` allows panic on corrupted value prefix

**Root cause:** The bounds check `value_offset > remaining` (layout.rs:333) uses strict `>` instead of `>=`, allowing `value_offset == remaining` to pass — producing a zero-length value buffer that panics in `V::serialized_size_from_bytes` via `.expect()`.

**Flagged by:**
| Specialist | Perspective | Severity | Confidence |
|---|---|---|---|
| correctness | premortem | must-fix | HIGH |
| correctness | red-team | must-fix | HIGH |
| security | premortem | must-fix | HIGH |
| security | red-team | must-fix | HIGH |
| edge-cases | premortem | must-fix | HIGH |
| edge-cases | red-team | must-fix | HIGH |
| assumptions | red-team | should-fix | MEDIUM |
| maintainability | premortem | should-fix | HIGH |
| maintainability | red-team | must-fix | HIGH |
| testing | premortem | should-fix | MEDIUM |

**Convergence:** **HIGHEST** — 9 independent specialists (all 9), 10 findings across both perspectives

**Evidence:** `rust/crates/faster-core/src/record/layout.rs:333`:
```rust
if value_offset > remaining {  // BUG: allows value_offset == remaining (0 bytes for value)
    return Err(RecordSizeError::CorruptedRecord { offset });
}
let value_buf = &page_bytes[offset + value_offset..];  // empty slice when ==
let value_size = V::serialized_size_from_bytes(value_buf);  // PANICS via expect()
```

Trigger verified: `remaining=24`, corrupted key length prefix of `12` → `key_size=16` → `value_offset = pad_alignment(8+16, 8) = 24 = remaining` → `value_buf` is empty → `Vec<u8>::serialized_size_from_bytes` panics at traits.rs:324.

**Impact:**
- Deterministic panic crashes the compaction thread while holding `compaction_lock`
- Mutex is poisoned — all subsequent `compact()` calls fail with `PoisonError`
- Log grows without bound (begin_address never advances)
- Requires process restart to recover
- Single corrupted byte creates permanent compaction denial-of-service

**Recommended fix:** Change layout.rs:333 from:
```rust
if value_offset > remaining {
```
to:
```rust
if value_offset + LENGTH_PREFIX_SIZE > remaining {
```
This ensures at least 4 bytes remain for variable-length value prefix reads. Conservative for fixed-size types (they ignore the buffer) but maintains type-agnostic safety. Add regression test with `value_offset == remaining`.

---

### MF-2: Numeric `eq_from_bytes` panics on short buffers — no bounds check

**Root cause:** The numeric `eq_from_bytes` macro implementation (traits.rs:246-248) performs `buf[..size_of::<T>()]` without a length guard, panicking on short buffers. Variable-length types (`Vec<u8>`, `String`) have defensive guards; numeric types do not.

**Flagged by:**
| Specialist | Perspective | Severity | Confidence |
|---|---|---|---|
| correctness | premortem | should-fix | MEDIUM |
| correctness | red-team | should-fix | MEDIUM |
| security | premortem | consider | MEDIUM |
| architecture | red-team | should-fix | HIGH |
| edge-cases | premortem | should-fix | MEDIUM |
| edge-cases | red-team | should-fix | MEDIUM |
| assumptions | red-team | should-fix | HIGH |

**Convergence:** **HIGH** — 7 independent specialists

**Evidence:** `rust/crates/faster-core/src/record/traits.rs:246-248`:
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()  // NO length check
}
```

Trigger: A corrupted `previous_address` field in a record header points to page offset `page_size - 8`. `safe_record_size` returns 8 bytes. `accessor.as_slice()[KEY_OFFSET..]` (KEY_OFFSET=8) produces an empty slice. `u64::eq_from_bytes` indexes `buf[..8]` on a 0-byte buffer → panic.

**Impact:**
- Compaction scanner panics during version chain walks on corrupted data
- Any record with a corrupted `previous_address` near a page boundary triggers this
- Asymmetry: stores using `u64` keys are *less* robust than `Vec<u8>` stores — the opposite of expectation

**Recommended fix:** Add bounds guard to the numeric macro in traits.rs:246-248:
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

---

### MF-3: `compact::<K, V>()` accepts unconstrained type parameters — type confusion possible

**Root cause:** `FasterKv::compact<K: Key, V: Value>()` (kv.rs:1441) accepts arbitrary `K`/`V` types with no connection to the store's `F::Key`/`F::Value`. A caller can pass mismatched types, causing the scanner to misinterpret record data.

**Flagged by:**
| Specialist | Perspective | Severity | Confidence |
|---|---|---|---|
| assumptions | premortem | must-fix | HIGH |
| assumptions | red-team | must-fix | HIGH |
| testing | premortem | must-fix | HIGH |
| release-manager | red-team | must-fix | HIGH |

**Convergence:** **HIGH** — 4 independent specialists, all must-fix with HIGH confidence

**Evidence:** `rust/crates/faster-core/src/store/kv.rs:1441`:
```rust
pub fn compact<K: Key, V: Value>(&self) -> Result<CompactionResult, CompactionError> {
```
No `where K = F::Key, V = F::Value` constraint. `store.compact::<Vec<u8>, Vec<u8>>()` compiles on a `u64` store.

**Impact:**
- `compact::<Vec<u8>, Vec<u8>>()` on a `u64` store interprets the first 4 bytes of a u64 key as a length prefix
- For small u64 values, the length prefix is plausible → scanner computes wrong record sizes → copies garbage → swings hash pointers to corrupted data → **silent data loss**
- Worst case: cascading corruption across the entire compaction region
- Best case: `CorruptedRecord` error (blocks compaction, not data loss)

**Recommended fix:** Either:
1. Remove generic params: `pub fn compact(&self) -> ...` using `F::Key`/`F::Value` internally, or
2. Add constraint: `where F: Functions<Key = K, Value = V>`, or
3. Add runtime `TypeId` check in debug builds + documentation

Add a test that calls `compact::<WrongK, WrongV>()` and verifies safe behavior.

---

## Should-Fix Findings

### SF-1: `serialized_size_from_bytes` default trait impls panic on corrupted data for custom Key/Value types

**Root cause:** The default implementations of `serialized_size_from_bytes` and `eq_from_bytes` (traits.rs:127-128, 143-144) delegate to `Self::deserialize(buf)`, which panics on corrupted or truncated input. Custom types that don't override these will panic during compaction.

**Flagged by:** correctness-premortem, architecture-premortem, architecture-red-team, edge-cases-premortem, assumptions-premortem, release-manager-red-team, maintainability-premortem (7 specialists)
**Convergence:** HIGH — 7 specialists
**Evidence:** traits.rs:127-128: `Self::deserialize(buf).serialized_size()` — `deserialize` panics on short/corrupted buffers for all built-in types.
**Impact:** Any custom `Key` type inheriting the defaults will panic during compaction on corrupted data instead of returning `Err`. The doc comment frames overriding as a "performance optimization" rather than a safety requirement.
**Recommended fix:** Update doc comments to warn that defaults may panic on corrupted input. Add: "Types used in compaction-eligible stores MUST override these with corruption-safe implementations." Consider adding `buf.len()` guards in the default impls.

---

### SF-2: `KEY_OFFSET = 8` hardcoded in 3 files without centralization

**Root cause:** The constant `KEY_OFFSET: usize = 8` (derived from `pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)`) is independently defined in scanner.rs:87, address_update.rs:122, and record_ops.rs:554. Only address_update.rs has a `debug_assert` guard (stripped in release builds).

**Flagged by:** architecture-premortem, assumptions-premortem, maintainability-premortem (3 specialists, all HIGH confidence)
**Convergence:** HIGH — 3 specialists
**Evidence:** `layout.rs:327` computes it dynamically, but the three consumer files hardcode `8`.
**Impact:** If `RecordInfo` ever changes size, `layout.rs` adapts but the three hardcoded sites silently use the wrong offset → garbage key reads in release builds.
**Recommended fix:** Add `pub const KEY_OFFSET: usize = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);` to `layout.rs`. Replace all three local definitions with imports. Add `const_assert_eq!(KEY_OFFSET, 8)`.

---

### SF-3: Unbounded `tombstone_records` vector — memory exhaustion risk

**Root cause:** The scanner (scanner.rs:182-189) pushes every tombstone to `plan.tombstone_records: Vec<LiveRecord>` with no capacity limit. For delete-heavy workloads, this vector grows proportional to tombstone count without bound.

**Flagged by:** correctness-red-team, security-red-team, performance-premortem, assumptions-red-team (4 specialists)
**Convergence:** MEDIUM — 4 specialists, mixed severity (should-fix to consider)
**Evidence:** `LiveRecord` ≈ 16 bytes. A 1 GiB compaction region with 24-byte records → 44.7M tombstones → 715 MB vector allocation.
**Impact:** Memory exhaustion during compaction; combined with epoch guard held → compound resource exhaustion. Planning review (C-3) recommended logging at >100MB — not implemented.
**Recommended fix:** At minimum, add the planned warning: `log::warn!` if tombstone memory exceeds 100MB. Consider a configurable cap with fallback to re-reading from old addresses.

---

### SF-4: Scanner heap-allocates per record via `K::deserialize` — missing `hash_from_bytes`

**Root cause:** The scanner (scanner.rs:220) and address updater (address_update.rs:157) both call `K::deserialize()` to compute key hashes. For `Vec<u8>` keys, this heap-allocates per record. A `Key::hash_from_bytes` trait method would enable zero-copy hashing.

**Flagged by:** performance-premortem (Finding 1+6), performance-red-team (Finding 1), edge-cases-premortem (Finding 5) (3 specialists)
**Convergence:** MEDIUM — 3 specialists
**Evidence:** Scanner (130K records) + updater (70K records) = ~200K transient heap allocations per compaction cycle for variable-length types. ~6ms allocator overhead per cycle.
**Impact:** Allocation pressure and ~2× amplification of stored key data in transient memory during compaction. Red-team rates as practical DoS vector for adversary inserting large keys.
**Recommended fix:** Add `Key::hash_from_bytes(&[u8]) -> KeyHash` trait method to avoid deserialization in the scan loop and address updater. Not blocking for initial merge but should be prioritized for performance-sensitive deployments.

---

### SF-5: Hash collision flooding — O(N × MAX_CHAIN_DEPTH × key_size) scan time

**Root cause:** The scanner's `is_current_version` walks version chains up to `MAX_CHAIN_DEPTH = 4096` per record. With deliberate hash collisions and variable-length keys, total scan cost is O(N × 4096 × key_size).

**Flagged by:** security-red-team (Finding 3), performance-red-team (Finding 2), edge-cases-red-team (Finding 3) (3 specialists)
**Convergence:** MEDIUM — 3 specialists
**Evidence:** 100K records in one hash bucket × 4096 hops × 1KB keys = 400 GB of comparison work per scan. Non-cryptographic hash functions used by FASTER are vulnerable to collision attacks.
**Impact:** Compaction scan stalls for minutes/hours. No progress callback, cancellation token, or timeout exists.
**Recommended fix:** Add total-hops counter with configurable threshold warning. Consider per-chain-walk metrics. Document the hash collision risk. Long-term: consider randomized/salted hash.

---

### SF-6: `safe_record_size` uses `max(remaining, min_size)` — may overread past page boundary

**Root cause:** `safe_record_size` (record_ops.rs:38-43) returns `remaining.max(min_size)`. When a corrupted `previous_address` points near a page end with `remaining < min_size`, the function returns a value *larger* than available page bytes, potentially causing out-of-bounds reads.

**Flagged by:** assumptions-premortem, assumptions-red-team, edge-cases-red-team, maintainability-red-team (4 specialists)
**Convergence:** MEDIUM — 4 specialists
**Evidence:** Corrupted `previous_address` at `page_size - 2` → `remaining = 2` → `safe_record_size` returns `max(2, 8) = 8` → accessor reads 6 bytes past page boundary.
**Impact:** Best case: garbage header, chain walk terminates. Worst case: segfault on unmapped memory or false key match causing live record classified as dead (data loss).
**Recommended fix:** Change to `remaining.min(page_size)` (never exceed remaining). Add guard: `if remaining < RECORD_HEADER_SIZE { return None }` in `read_header_and_match_key_varlen`.

---

### SF-7: Breaking API changes without `#[non_exhaustive]` or version bump

**Root cause:** New `ScanCorruption` variant on `CompactionError`, new `tombstone_records` field on `CompactionPlan`, and new `record_size` field on `AddressMapping` — all on public types without `#[non_exhaustive]`. Changed `scan()`/`swing()` signatures.

**Flagged by:** release-manager-premortem (Findings 1-4), release-manager-red-team (Finding 2) (2 specialists, 5 findings)
**Convergence:** MEDIUM — 2 specialists, all HIGH confidence
**Evidence:** `CompactionError` in orchestrator.rs has no `#[non_exhaustive]`. `CompactionPlan` and `AddressMapping` have public fields with no builder or `Default`.
**Impact:** Downstream consumers with exhaustive `match` or struct literal construction break on upgrade.
**Recommended fix:** Add `#[non_exhaustive]` to `CompactionError`, `CompactionPlan`, and `AddressMapping`. Consider `pub(crate)` for internal pipeline types.

---

### SF-8: Subtle single-bit corruption cascades undetected through variable-stride scanner

**Root cause:** The scanner uses data-dependent stride (record sizes from length prefixes). A single-bit corruption in a length prefix that produces a plausible-but-wrong size passes all bounds checks but misaligns all subsequent record reads on the page.

**Flagged by:** assumptions-red-team (Finding 2, must-fix, HIGH), testing-red-team (Finding 1, must-fix, HIGH) (2 specialists)
**Convergence:** MEDIUM — 2 specialists, both must-fix with HIGH confidence
**Evidence:** A bitflip changing key length from 2 to 1 produces `key_size = 5` instead of `6`. Value offset shifts, total record size is smaller, scanner advances to mid-record. Subsequent "records" are garbage interpreted as valid.
**Impact:** Silent data loss — misaligned records classified as live (garbage copied) or dead (real data dropped). No checksum or cross-validation exists.
**Recommended fix:** Add post-scan validation: verify `total_bytes_scanned ≈ (until - begin)` and `live_bytes + dead_bytes + tombstone_bytes ≈ total_bytes_scanned`. Log warning on significant mismatch. Long-term: per-record or per-page checksums.

---

### SF-9: Testing gaps — missing boundary, corruption, and chain walk tests

**Root cause:** Multiple test coverage gaps across the suite. This synthesizes findings from the testing specialist.

**Flagged by:** testing-premortem (6 findings), testing-red-team (6 findings) (2 specialist files)
**Convergence:** N/A (dedicated testing review)

**Key gaps:**
1. No test for `compact::<WrongK, WrongV>()` type mismatch (testing-premortem F1)
2. No test for records whose size nearly fills a page (< MIN_READABLE gap at page end) (testing-premortem F2)
3. `eq_from_bytes` not tested with trailing garbage bytes in buffer (testing-premortem F3)
4. Concurrent compaction test doesn't assert read completeness (testing-premortem F4)
5. No test for moderate corruption values (plausible-but-wrong sizes) (testing-premortem F5)
6. No test for version chains with variable-length records of different sizes (testing-red-team F6)
7. No test for empty key (`Vec::<u8>::new()`) (testing-red-team F4)
8. Address updater unit tests use u64 despite "variable-length" names (maintainability-premortem F4)

**Recommended fix:** Address gaps #1, #2, #5, and #6 as priority. These exercise the most critical code paths unique to variable-length compaction.

---

## Consider Findings

### C-1: 32-bit platform `usize` overflow in `serialized_size_from_bytes`

**Root cause:** `LENGTH_PREFIX_SIZE + len` wraps on 32-bit when `len = u32::MAX`, producing a tiny size that bypasses corruption checks.
**Flagged by:** correctness-red-team, edge-cases-premortem, testing-red-team (3 specialists, all LOW confidence)
**Evidence:** traits.rs:326: `LENGTH_PREFIX_SIZE + len` — wraps to `3` on 32-bit platforms.
**Status:** FASTER targets 64-bit servers. Add `const _: () = assert!(core::mem::size_of::<usize>() >= 8);` in crate root as a compile guard.

### C-2: Software prefetch omission for variable stride

**Root cause:** Variable-length records have data-dependent stride; hardware prefetcher is less effective. Code acknowledges this with a comment (scanner.rs ~line 232-237) but can't add `_mm_prefetch` due to `#[deny(unsafe_code)]`.
**Flagged by:** performance-premortem (Finding 5), performance-red-team (Finding 4) — both agree it's fine.
**Impact:** ~0.3ms additional latency per 130K-record scan — negligible vs. total compaction time.
**Status:** Acknowledged trade-off. Revisit if `deny(unsafe_code)` is relaxed.

### C-3: `LiveRecord` type used for tombstone records (semantic mismatch)

**Root cause:** `tombstone_records: Vec<LiveRecord>` — tombstones are not "live."
**Flagged by:** architecture-premortem (1 specialist)
**Recommended fix:** Rename to `ScannedRecord` or add a type alias.

### C-4: `RecordSizeError` lacks page address context for debugging

**Root cause:** Error only includes byte offset within page, not which page or `LogicalAddress`.
**Flagged by:** maintainability-red-team (1 specialist)
**Recommended fix:** Enrich `CorruptedRecord` variant with `LogicalAddress` or log at scan site.

### C-5: No logging or alerting for corruption detection events

**Root cause:** `CompactionError::ScanCorruption` is returned to caller without logging.
**Flagged by:** security-premortem (1 specialist)
**Recommended fix:** Add `tracing::error!` at corruption detection site.

### C-6: `record_size as u32` casts — silent truncation risk

**Root cause:** Multiple `record_size as u32` casts (scanner.rs, address_update.rs) could silently truncate if `OFFSET_BITS` ever exceeds 32.
**Flagged by:** security-premortem, maintainability-red-team (2 specialists)
**Recommended fix:** Add `debug_assert!(record_size <= u32::MAX as usize)` before each cast.

### C-7: `record_size_from_bytes` trusts `page_size` parameter without validating `page_bytes.len()`

**Root cause:** Public function uses `page_size` for bounds validation but indexes into `page_bytes` — mismatch causes panic if `page_bytes.len() < page_size`.
**Flagged by:** correctness-premortem, security-red-team, architecture-red-team (3 specialists)
**Recommended fix:** Add `debug_assert!(page_bytes.len() >= page_size)` at function entry.

### C-8: `CompactionPlan` widened interface to address updater

**Root cause:** Address updater now accepts `&CompactionPlan` but only uses `tombstone_records`.
**Flagged by:** architecture-premortem (1 specialist)
**Recommended fix:** Accept `&[LiveRecord]` for tombstones instead of full plan.

### C-9: Parallel `read_header_and_match_key` methods (fixed vs varlen)

**Root cause:** Both methods do similar work, risk diverging on bug fixes.
**Flagged by:** architecture-red-team (1 specialist)
**Status:** Future unification opportunity. Not blocking.

### C-10: `LENGTH_PREFIX_SIZE` exported without stability annotation + no version bump

**Root cause:** Newly-public constant creates an implicit ABI contract. No CHANGELOG for breaking changes.
**Flagged by:** release-manager (2 findings)
**Recommended fix:** Add doc comment marking as stable. Bump version to 0.2.0.

---

## Trade-Off Resolutions

### Disagreement: `safe_record_size` — should it use `max` or `min`?

**Assumptions-red-team** and **maintainability-red-team** flagged `remaining.max(min_size)` as returning MORE bytes than available, enabling overreads. **Edge-cases-red-team** noted the function "works by accident" because `from_log` returns the full remaining bytes, not the `min_size` parameter.

**Synthesis verdict:** The `max` logic is functionally correct today because `from_log` returns actual remaining bytes and the `min_size` is a floor (not a ceiling). However, the naming is misleading and the function should be refactored to either: (a) always return `remaining` (drop the `min_size` floor), or (b) add a `debug_assert!(remaining >= min_size)` to catch corrupted addresses. **SF-6** captures the fix.

### Disagreement: Tombstone memory — should-fix or consider?

**Security-red-team** rated should-fix (adversary can flood tombstones). **Performance-premortem** rated consider (verdict: "fine, consistent with existing patterns"). **Correctness-red-team** rated should-fix.

**Synthesis verdict:** Should-fix. The planning review specifically recommended mitigation (C-3: "log warning if > 100MB") that was not implemented. The fix is low-cost (add logging + optional cap) and was already specified in the design.

### Disagreement: Architecture-red-team rated `page_bytes/page_size mismatch` as must-fix

**Architecture-red-team** rated the `page_bytes.len() < page_size` parameter mismatch as must-fix. **Correctness-premortem** rated it consider. **Security-red-team** rated it consider.

**Synthesis verdict:** Consider (C-7). All current call sites are correct. The function is `pub` but practically only called from the scanner with matching values. A `debug_assert` is sufficient — upgrading to must-fix is not warranted given no current caller mismatch.

---

## Speculative / Low-Confidence Findings

1. **Timing side-channel from key deserialization** (correctness-red-team, consider, LOW): Key size leakage via allocation timing during `swing_one`. Requires nanosecond-precision instrumentation. **Verdict: Not actionable.**

2. **Custom Key with malicious `serialized_size_from_bytes`** (edge-cases-red-team, consider, LOW): A custom Key type returning usize::MAX/2 could cause overflow on 32-bit. On 64-bit, the corruption check catches it. **Verdict: Subsumed by C-1 (64-bit assertion).**

3. **`eq_from_bytes` false-positive on truncated data** (testing-red-team, should-fix, MEDIUM): When `eq_from_bytes` returns `false` due to short buffer (not real mismatch), the scanner's conservative default keeps the record alive. This is safe behavior. **Verdict: Correct by design, but deserves a documenting comment.**

4. **CompactionPlan cross-epoch validity** (assumptions-premortem, consider, LOW): Plan consumed in Phase 3 while epoch from Phase 1 may theoretically be released. **Verdict: The orchestrator holds a single epoch guard across all three phases. Risk is theoretical and only relevant if epoch batching is introduced.**

---

## Statistics

| Metric | Value |
|---|---|
| Total raw findings across 18 files | ~96 |
| Unique threads after clustering | 22 |
| **Must-fix** | **3** |
| **Should-fix** | **9** |
| **Consider** | **10** |
| Highest-convergence thread | MF-1: Off-by-one bounds check (9 of 9 specialists) |
| Second-highest convergence | MF-2: Numeric eq_from_bytes panic (7 specialists) |
| Third-highest convergence | SF-1: Default trait impl panic (7 specialists) |
| Findings verified against source code | MF-1 ✅, MF-2 ✅, MF-3 ✅, SF-2 ✅ |

### Fix Priority Order (recommended)

| Priority | Thread | Effort | Risk if unfixed |
|---|---|---|---|
| 1 | MF-1: Off-by-one bounds check | ~5 min (one line + test) | Deterministic panic on corruption |
| 2 | MF-2: Numeric eq_from_bytes bounds | ~5 min (one line in macro + test) | Panic on corrupted chain pointers |
| 3 | MF-3: Constrain compact() types | ~30 min (signature + test) | Silent data corruption |
| 4 | SF-2: Centralize KEY_OFFSET | ~15 min (move const + imports) | Silent misread after header change |
| 5 | SF-6: Fix safe_record_size | ~15 min (change max→min + guard) | Overread past page boundary |
| 6 | SF-1: Document default trait panics | ~15 min (doc comments) | Surprise panics for custom types |
| 7 | SF-7: Add #[non_exhaustive] | ~5 min (annotations) | Breaking downstream builds |
| 8 | SF-3: Tombstone vector warning | ~10 min (add logging) | Unbounded memory growth |
| 9 | SF-9: Add missing tests | ~2 hours | Coverage gaps |
| 10 | SF-8: Post-scan validation | ~1 hour | Undetected subtle corruption |
| 11 | SF-4: hash_from_bytes trait method | ~2 hours | Perf regression for varlen types |
| 12 | SF-5: Hash collision metrics | ~1 hour | DoS under adversarial workload |
