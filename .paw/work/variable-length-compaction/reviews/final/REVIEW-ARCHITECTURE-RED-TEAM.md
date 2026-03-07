# Architecture Review — Red-Team Perspective

**Perspective**: red-team
**Reviewer**: Architecture Specialist
**Scope**: Variable-length record compaction — ~3,400 lines across 21 files
**Scenario**: An adversary wants to exploit architectural weaknesses — abstraction leaks, trust boundary gaps, composability flaws

---

### Finding: `record_size_from_bytes` trusts `page_size` parameter without validating it against `page_bytes.len()`, enabling out-of-bounds panics

**Severity**: must-fix
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/layout.rs` (diff lines 3450-3484), the public function:

```rust
pub fn record_size_from_bytes<K: Key, V: Value>(
    page_bytes: &[u8],
    offset: usize,
    page_size: usize,
) -> Result<usize, RecordSizeError> {
    let remaining = page_size.saturating_sub(offset);
    // ...
    let key_buf = &page_bytes[offset + key_offset..];        // line ~3466
    let key_size = K::serialized_size_from_bytes(key_buf);
    // ...
    let value_buf = &page_bytes[offset + value_offset..];     // line ~3475
```

The `remaining` calculation uses `page_size`, not `page_bytes.len()`. If a caller passes `page_size = 4096` but `page_bytes` with length 100, the MIN_READABLE check passes (4096 - 0 >= 12), and then `&page_bytes[8..]` yields 92 bytes. If `K::serialized_size_from_bytes` reads a corrupted/large key length prefix from those bytes (say, 2000 bytes), `value_offset` becomes `pad_alignment(8 + 2004, 8) = 2016`. The validation `if value_offset > remaining` checks `2016 > 4096` → false (passes!). Then `&page_bytes[0 + 2016..]` panics with index out of bounds.

This is a public API (`pub fn record_size_from_bytes` re-exported via `record/mod.rs` at diff line 3694). Any caller that mismatches `page_bytes.len()` and `page_size` can trigger a panic in safe Rust code that was designed to return `Result`.

The scanner avoids this in practice because `RecordAccessor::from_log` returns page-remainder bytes where `page_bytes.len() == remaining`. But the function's contract doesn't enforce this.

#### Warrant (Rule)

A function that returns `Result<T, E>` establishes a trust boundary: "I handle errors via the type system, not panics." This function violates that contract when `page_bytes.len() < page_size`. The codebase's error handling conventions (documented in `error.rs:33-77`) reserve panics for programmer-error invariants (lock poisoning, memory layout), not for parameter validation on public APIs. Either the function should `assert!(page_bytes.len() >= page_size)` early (fail-fast on invariant violation), or replace `page_size` with `page_bytes.len()` as the source of truth.

An adversary doesn't need to call this function directly — they need only trigger a code path where the log page buffer is shorter than expected (e.g., partial I/O read during recovery, truncated device page).

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` is only ever called with `page_bytes` whose length equals `page_size` (verified by audit of all call sites); (2) the function is made `pub(crate)` to restrict its exposure; or (3) the allocator guarantees that page buffers are always exactly `page_size` bytes.

#### Suggested Verification

Add `debug_assert!(page_bytes.len() >= offset + remaining)` after computing `remaining`. Better: add a bounds check `if page_bytes.len() < page_size { return Err(RecordSizeError::InsufficientPageSpace); }`.

---

### Finding: `eq_from_bytes` for numeric types panics on short buffers instead of returning `false`

**Severity**: should-fix
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs`, the numeric `eq_from_bytes` implementation (diff lines 3844-3846):

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```

This slices `buf[..8]` for u64 (or `buf[..4]` for u32). If `buf.len() < size_of::<T>()`, this panics with index out of bounds.

Compare with the `Vec<u8>` implementation (diff lines 3887-3895):

```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    if buf.len() < LENGTH_PREFIX_SIZE {
        return false;
    }
    // ...
}
```

And the `String` implementation (diff lines 3931-3939) which also has a length guard. The variable-length types defensively return `false` on short buffers; the numeric types panic.

This inconsistency is exploitable: if a corrupted previous-address pointer in a record's header leads the version chain walker to an address near a page boundary with fewer than 8 bytes remaining, `eq_from_bytes::<u64>` will panic rather than returning `false`.

The chain walk at `scanner.rs` calls `read_header_and_match_key_varlen` → `key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..])`. If `accessor.as_slice()` has fewer than `KEY_OFFSET + 8` bytes (e.g., record at page offset 4088 on a 4096-byte page → 8 bytes total, KEY_OFFSET=8 → 0 bytes for key), the slice `[KEY_OFFSET..]` is empty and `eq_from_bytes` panics.

#### Warrant (Rule)

The `eq_from_bytes` trait contract (diff lines 3798-3807) states: "Must return `true` iff `*self == Self::deserialize(buf)`." Panicking on short buffers violates this contract — the function should return `false` since no valid key can be deserialized from insufficient bytes. The existing `Vec<u8>` and `String` implementations establish the convention of defensive length checks. The numeric implementations break this convention.

In a high-performance concurrent system, a panic in the compaction scanner brings down the entire process. A corrupted chain pointer (one-bit flip in a previous-address field) escalates from "compaction finds a stale chain entry" to "process crash."

#### Rebuttal Conditions

This is NOT a concern if: (1) the accessor always guarantees at least `KEY_OFFSET + size_of::<K>()` bytes (but `safe_record_size` in `record_ops.rs` only guarantees `RECORD_HEADER_SIZE` minimum); or (2) corrupted chain pointers are impossible due to hardware ECC + checksumming at the storage layer.

#### Suggested Verification

Add a buffer length check to the numeric `eq_from_bytes`:
```rust
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    buf.len() >= core::mem::size_of::<$t>()
        && buf[..core::mem::size_of::<$t>()] == self.to_le_bytes()
}
```
Add a test with `eq_from_bytes` called on a 2-byte buffer for u64 to verify it returns `false` instead of panicking.

---

### Finding: `read_header_and_match_key_varlen` uses `safe_record_size(addr, RECORD_HEADER_SIZE)` which may overread past the actual record

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/hybrid_log/record_ops.rs` (diff lines 3360-3373):

```rust
pub fn read_header_and_match_key_varlen<K: Key>(
    &self,
    addr: LogicalAddress,
    key: &K,
) -> Option<(RecordInfo, bool)> {
    let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
    let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
```

`safe_record_size` (line 38-43) computes `max(page_remaining, min_size)`. This returns the remaining bytes on the page, which could be hundreds or thousands of bytes beyond the actual record size.

The accessor's `as_slice()` then returns a slice covering all remaining page bytes. The call `key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..])` passes a potentially enormous slice to the key comparison function. For `Vec<u8>`, `eq_from_bytes` reads a 4-byte length prefix from this slice. If the record is followed by garbage data (freed record slots, padding bytes), the length prefix could be arbitrary — returning a false match or reading deep into unrelated data.

Contrast with the existing `read_header_and_match_key` method (same file, line 520-532) which uses `safe_record_size(addr, layout.value_offset() as u32)` — providing a tighter bound based on the known layout.

#### Warrant (Rule)

The existing method provides a bounded access window proportional to the record's known size. The new method provides a page-remainder-sized window because it doesn't know the record size. This is architecturally correct (records don't span pages), but it changes the trust model: `eq_from_bytes` must now be robust against seeing bytes beyond the current record's boundary.

The `eq_from_bytes` implementations for variable-length types (`Vec<u8>`, `String`) read a length prefix and then compare exactly that many bytes — they don't validate that the length is reasonable relative to the record's actual size. If a record's key ends at offset 20 but the accessor provides 4000 bytes, and the key data is followed by another record whose header bytes happen to encode a valid-looking length prefix at offset 20, `eq_from_bytes` could read into the adjacent record's data.

In practice, `eq_from_bytes` for the *scanner's own key* should compare correctly because the serialized key data at `KEY_OFFSET` is always the record's own key. But this relies on the key's serialized format being self-delimiting (length-prefixed types are, fixed-size types are) — an implicit contract not enforced by the type system.

#### Rebuttal Conditions

This is NOT a concern if: (1) `eq_from_bytes` is documented as only reading `serialized_size_from_bytes(buf)` bytes from `buf` (i.e., self-delimiting); (2) all Key implementations in the codebase and expected custom implementations follow this pattern; or (3) the page-remainder approach is the only feasible option since the record size isn't known before reading.

#### Suggested Verification

Add a property test: for any Key type, `eq_from_bytes` reading from a buffer with trailing garbage bytes must produce the same result as reading from a tight buffer. Test with `buf.extend(&[0xFF; 1000])` appended to a correctly serialized key.

---

### Finding: Scanner requests full `remaining` page bytes from allocator for every record, creating unnecessary memory pressure for small records

**Severity**: consider
**Confidence**: MEDIUM
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/compaction/scanner.rs` (diff lines 2874-2882):

```rust
let accessor = match RecordAccessor::from_log(self.allocator, current, remaining) {
    Some(a) => a,
    None => {
        current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
        continue;
    }
};
```

The scanner requests `remaining` bytes (up to a full page = `1 << OFFSET_BITS` bytes) even for a record that might be 24 bytes. This contrasts with the old scanner which requested exactly `record_size` bytes:

```rust
// Old code (before diff)
let accessor = match RecordAccessor::from_log(self.allocator, current, record_size as u32) {
```

The scanner needs the full remainder for `record_size_from_bytes` to inspect key/value length prefixes. But after discovering `record_size`, the accessor's slice is still page-sized. All subsequent reads (`accessor.record_info()`, `K::deserialize(&bytes[KEY_OFFSET..])`) operate on this oversized slice.

#### Warrant (Rule)

The existing copier pattern (at `copier.rs:160-196`) uses `record.record_size as u32` for precise accessor sizing. The new scanner uses `remaining` for a different reason (needs to read length prefixes before knowing size), but the architectural inconsistency means the scanner holds wider memory views than other compaction phases.

For dense pages with many small variable-length records (e.g., 100 records of 40 bytes each on a 4096-byte page), each `from_log` call creates an accessor with up to 4096 bytes of view when only 40 are needed. If `RecordAccessor` pins or copies the view (rather than borrowing), this amplifies memory usage by ~100x per page.

#### Rebuttal Conditions

This is NOT a concern if: (1) `RecordAccessor::from_log` borrows rather than copies (zero-copy memory-mapped access); (2) the page is already pinned in memory by the epoch protection, so the accessor size doesn't affect memory pressure; or (3) the design is intentional to avoid a two-phase read (first read to discover size, second read to create tight accessor).

#### Suggested Verification

Check `RecordAccessor::from_log` implementation — if it borrows a `&[u8]` slice from a pinned page, the `remaining` size has no memory cost beyond the slice header. If it copies, consider a two-step approach: `from_log(allocator, current, MIN_READABLE)` to discover size, then `from_log(allocator, current, record_size)` for the full record.

---

### Finding: `serialized_size_from_bytes` for variable-length types panics on malformed input instead of returning an error

**Severity**: should-fix
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs`, the `Vec<u8>` implementation (diff lines 3877-3882):

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

This method panics via `.expect()` if `buf.len() < 4`, and returns an attacker-controlled value (`LENGTH_PREFIX_SIZE + len` where `len` can be up to `u32::MAX = 4,294,967,295`) without any bounds check.

The caller `record_size_from_bytes` (layout.rs, diff lines 3466-3481) does validate the result against `remaining`, catching the overflow case. But `serialized_size_from_bytes` is a `pub` trait method — any code calling it directly (outside of `record_size_from_bytes`) gets no protection.

The `String` version (diff lines 3920-3926) has the identical pattern.

The `record_size_from_bytes` function ensures `remaining >= MIN_READABLE (12)` before calling, which guarantees `key_buf` has at least 4 bytes (since `key_offset = 8` and `remaining >= 12` means `page_bytes[offset+8..]` has at least 4 bytes). So the panic path is unreachable *from that caller*. But the trait method is public.

#### Warrant (Rule)

The codebase's error handling convention (per `error.rs`) reserves `expect()` panics for invariant violations where recovery is impossible (lock poisoning, memory layout errors). A short buffer in `serialized_size_from_bytes` is a data corruption issue, not a programmer invariant violation — it should return an error or a sentinel value.

The existing `deserialize` methods for numeric types also use `expect()` (established before this diff), but those are only called on validated buffers from the allocator. `serialized_size_from_bytes` is new and explicitly designed for raw page scanning where corruption is expected (the entire `RecordSizeError::CorruptedRecord` error type exists for this purpose).

An adversary who can write a malformed record (or trigger a partial page write during a crash) can create a page where offset 8 has fewer than 4 valid bytes remaining. If any code path calls `serialized_size_from_bytes` without the `record_size_from_bytes` wrapper's validation, the process panics.

#### Rebuttal Conditions

This is NOT a concern if: (1) `serialized_size_from_bytes` is only ever called via `record_size_from_bytes` which pre-validates remaining space; (2) the trait method is changed to `pub(crate)` or documented as requiring pre-validated buffers; or (3) the function signature is changed to return `Result<usize, _>` (breaking change to the trait).

#### Suggested Verification

Audit all call sites of `serialized_size_from_bytes` outside of `record_size_from_bytes`. If none exist, add `// SAFETY: caller must ensure buf.len() >= LENGTH_PREFIX_SIZE` to the doc comment. Consider returning a `Result` or adding a `buf.len()` check with a saturating fallback.

---

### Finding: `read_header_and_match_key` (fixed-size) and `read_header_and_match_key_varlen` (variable) coexist as parallel methods, creating a consistency maintenance burden

**Severity**: consider
**Confidence**: MEDIUM
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/hybrid_log/record_ops.rs`, two methods now exist:

1. `read_header_and_match_key` (existing, line 520-532) — takes `RecordLayout`, uses `accessor.key(layout)`, typed comparison
2. `read_header_and_match_key_varlen` (new, diff lines 3360-3373) — no layout, uses `eq_from_bytes`, zero-copy comparison

The scanner switched from method 1 to method 2 (diff line 2973: `reader.read_header_and_match_key_varlen::<K>(current, key)`). But method 1 is still used elsewhere in the codebase (e.g., the existing non-compaction read paths).

Both methods read a record header and compare a key — they differ in how they determine the access window and compare keys. This is a "two ways to do the same thing" divergence.

#### Warrant (Rule)

The codebase's existing pattern for key operations is a single method per concern. Adding `_varlen` as a suffix creates a naming convention problem: will future methods need both `_fixed` and `_varlen` variants? The `eq_from_bytes` approach subsumes the fixed-size case (fixed-size types implement `eq_from_bytes` as a simple byte comparison, which is equivalent to deserialize-then-compare). Method 2 could replace method 1 entirely.

The risk is divergence: if a bug fix is applied to `read_header_and_match_key` but not `read_header_and_match_key_varlen` (or vice versa), the two code paths silently differ. This is the same "two database access patterns" anti-pattern from the monolith anecdote.

#### Rebuttal Conditions

This is NOT a concern if: (1) `read_header_and_match_key` has performance advantages for fixed-size types that justify keeping both (e.g., the layout-based version avoids the `eq_from_bytes` virtual dispatch); (2) there's a planned migration to unify them; or (3) they serve genuinely different use cases (compaction vs. normal reads) that merit distinct implementations.

#### Suggested Verification

Benchmark `read_header_and_match_key_varlen::<u64>` against `read_header_and_match_key` with u64 keys. If performance is equivalent (likely, since the compiler inlines `eq_from_bytes` for numeric types), deprecate the old method and migrate callers.
