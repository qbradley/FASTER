# REVIEW-ASSUMPTIONS-PREMORTEM.md

**Perspective**: premortem
**Specialist**: assumptions
**Scenario**: 6 months after merge, a production incident revealed an assumption that was load-bearing but undocumented or weakly enforced.

---

## Examination Strategy

Applied Socratic first-principles questioning to every design decision in the diff, asking "what must be true for this to be correct?" and "what would change to make this false?" Focused on assumptions that are invisible to future maintainers — the kind that are obvious now but will be forgotten in 6 months.

---

### Finding: `compact::<K, V>()` type parameters are disconnected from the store's actual `F::Key`/`F::Value` types — nothing prevents type confusion

**Severity**: must-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

In `rust/crates/faster-core/src/store/kv.rs:1441` the signature is:
```rust
pub fn compact<K: Key, V: Value>(&self) -> Result<CompactionResult, CompactionError>
```
The store is `FasterKv<F: Functions>` where `F::Key` and `F::Value` define the actual stored types. But `compact()` accepts **any** `K: Key` and `V: Value`. There is no `where K: Into<F::Key>` bound, no `assert_eq!(TypeId::of::<K>(), TypeId::of::<F::Key>())`, no connection whatsoever.

A caller can write `store.compact::<String, String>()` on a store that holds `Vec<u8>` records. The scanner would read String length prefixes from Vec<u8>-encoded data — they share the same `LENGTH_PREFIX_SIZE=4` wire format, so this might even silently "work" in some cases. But if the store held `u64` keys and the caller passed `Vec<u8>`, the scanner would read 4 bytes at offset 8 as a length prefix from what is actually a u64 key value, computing a wildly wrong record size.

The orchestrator at `orchestrator.rs:172-174` passes `K` and `V` through to `scanner.scan::<K, V>()` and `updater.swing::<K, V>()` without validation.

#### Warrant (Rule)

This code assumes that the caller always passes the correct type parameters to `compact()`. This is an **undocumented runtime contract** — the compiler cannot enforce it because the type parameters are free. The previous signature `compact<K: FixedSizeKey, V: FixedSizeValue>` was equally unchecked, but the risk was lower because all fixed-size types have the same serialization format (raw bytes). Variable-length types have format-dependent length prefixes, making type confusion much more dangerous.

What would change to make this false? A new developer calls `compact::<F::Key, F::Value>()` intending to be correct, but `F` has been refactored and the call site wasn't updated. Or a generic wrapper uses the wrong associated types.

#### Rebuttal Conditions

This is NOT a concern if: (1) The call sites are always `compact::<F::Key, F::Value>()` using the store's own associated types, making misuse unlikely; or (2) A future change adds a `where K = F::Key, V = F::Value` equality constraint. The current integration tests all use matching types, so this has never been tested with mismatched types.

#### Suggested Verification

Add a method `compact_auto(&self)` that uses `F::Key` and `F::Value` directly, making misuse impossible. Alternatively, add a runtime `TypeId` check in debug builds: `debug_assert_eq!(TypeId::of::<K>(), TypeId::of::<F::Key>())`. At minimum, add a `# Safety` or `# Contract` doc section warning that K and V must match F::Key/F::Value.

---

### Finding: `serialized_size_from_bytes` for `Vec<u8>` and `String` does not validate the length prefix against the buffer, panicking on corrupted input when called outside `record_size_from_bytes`

**Severity**: should-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs:320-327` (Vec<u8> Key impl):
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

This has two issues:
1. The `.expect()` **panics** if `buf.len() < 4`. When called from `record_size_from_bytes` (layout.rs:~3467), the `MIN_READABLE=12` guard ensures at least 4 bytes for the key. But the value buffer (`page_bytes[offset + value_offset..]`) may have fewer than 4 bytes if a corrupted key prefix caused `value_offset` to be close to `remaining`. Actually, the check `if value_offset > remaining { return Err(CorruptedRecord) }` at layout.rs:~3471 catches this. So the guard works for the `record_size_from_bytes` caller.
2. However, the returned size (`4 + len`) is **not validated** against `buf.len()`. A corrupted prefix of `u32::MAX` returns `4 + 4,294,967,295 = 4,294,967,299`. The caller `record_size_from_bytes` checks `total > remaining`, which catches this. But if ANY other caller uses `serialized_size_from_bytes` directly (e.g., a future compaction helper), the unchecked size is returned.

The same pattern exists for String (traits.rs:~3922).

Additionally, the **default trait implementation** at traits.rs:~3789 calls `Self::deserialize(buf).serialized_size()`. For any custom `Key` implementation that doesn't override this, `deserialize` on corrupted data may panic (as the built-in `deserialize` methods do with `expect()`). This means the "safe fallback" is not actually safe for corruption.

#### Warrant (Rule)

The assumption is that `serialized_size_from_bytes` is only ever called from `record_size_from_bytes` with properly guarded buffers. This is true today, but the method is `pub` on a public trait — any downstream crate can call it with arbitrary buffers. The contract ("buf contains at least the bytes needed to determine the size") is documented but not enforced. In 6 months, when someone writes a new compaction utility or debugging tool that calls `serialized_size_from_bytes` directly, the panic will be a surprise.

#### Rebuttal Conditions

This is NOT a concern if: (1) `serialized_size_from_bytes` is only ever called from `record_size_from_bytes`, which has proper guards; or (2) the method is made `pub(crate)` to prevent external misuse; or (3) the documentation is considered sufficient warning.

#### Suggested Verification

Add a `checked_serialized_size_from_bytes(buf: &[u8]) -> Option<usize>` variant that returns `None` on short buffers or impossibly large prefixes. Or add validation to the existing method: `if buf.len() < LENGTH_PREFIX_SIZE { panic or return a sentinel }` and `if len > buf.len() - LENGTH_PREFIX_SIZE { return some safe value or panic with a clear message }`.

---

### Finding: KEY_OFFSET=8 is hardcoded in 4 locations with only a debug_assert linking it to the actual record layout — a future RecordInfo change silently breaks compaction in release builds

**Severity**: should-fix
**Confidence**: HIGH
**Category**: assumptions

#### Grounds (Evidence)

The assumption that key data starts at byte offset 8 within every record is load-bearing across the entire compaction pipeline:

1. `scanner.rs:84`: `const KEY_OFFSET: usize = 8;`
2. `address_update.rs:121-124`: `const KEY_OFFSET: usize = 8;` with `debug_assert_eq!(RECORD_HEADER_SIZE, 8, ...)`
3. `record_ops.rs:555`: `const KEY_OFFSET: usize = 8;` (in `read_header_and_match_key_varlen`)
4. `layout.rs:~3465`: `let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT); // = 8`

The invariant derives from: `pad_alignment(size_of::<RecordInfo>(), align_of::<u64>()) = pad_alignment(8, 8) = 8`. `RecordInfo` is `#[repr(transparent)] struct RecordInfo(u64)` so `size_of` is 8. But `#[repr(transparent)]` only guarantees the same layout as the inner type — if someone adds a second field or changes the repr, the size changes silently.

The only validation is a `debug_assert_eq!(RECORD_HEADER_SIZE, 8)` in `address_update.rs:121`, which is **stripped in release builds**. The scanner and `read_header_and_match_key_varlen` have no runtime check at all.

#### Warrant (Rule)

This is a classic "invisible invariant" — four files independently assume the same constant without a shared compile-time assertion. If `RecordInfo` ever gains a size field (the Spec explicitly considers this out-of-scope but acknowledges it as a possibility), all four sites break silently. The `RECORD_HEADER_SIZE` constant would update, but the hardcoded `8` values would not.

The comment `// = 8` at each site is the only documentation, and it's not machine-verifiable.

#### Rebuttal Conditions

This is NOT a concern if: (1) `RecordInfo` is `#[repr(transparent)]` over `u64` and this is considered a permanent design decision; or (2) a `const_assert!(RECORD_HEADER_SIZE == 8)` is added to a shared location that all four files depend on; or (3) the constant `8` is replaced with `RECORD_HEADER_SIZE` everywhere (since `pad_alignment(8, 8) == 8`).

#### Suggested Verification

Add `const_assert_eq!(pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT), 8)` in `record/layout.rs` (using the `static_assertions` crate or a manual const-eval check). Replace the hardcoded `8` in all four files with a shared `const KEY_OFFSET: usize = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)` defined once in `record/layout.rs` and imported everywhere.

---

### Finding: `safe_record_size` uses `max(remaining, min_size)` which requests MORE bytes than the page has remaining when the record is near a page boundary

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: assumptions

#### Grounds (Evidence)

In `rust/crates/faster-core/src/hybrid_log/record_ops.rs:38-43`:
```rust
fn safe_record_size(addr: LogicalAddress, min_size: u32) -> u32 {
    let page_size = 1u32 << OFFSET_BITS;
    let offset = addr.offset().0;
    let remaining = page_size.saturating_sub(offset);
    remaining.max(min_size)
}
```

When called from `read_header_and_match_key_varlen` (line 551): `let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);` with `min_size=8`.

If a hash chain's `previous_address` points to an offset with only 2 bytes remaining on the page (e.g., offset = `page_size - 2`), then `remaining = 2` and the function returns `max(2, 8) = 8`. The `RecordAccessor::from_log` then creates an accessor for 8 bytes starting at an address with only 2 bytes left on the page. Since `from_log` trusts the caller's size (record_ops.rs:171: "The caller supplies the correct record_size"), the accessor would read 6 bytes past the page boundary into whatever physical memory follows the page frame.

#### Warrant (Rule)

The assumption is that hash index entries and version chain addresses always point to valid record start positions with enough page space for the full record. This is true when the allocator is working correctly (records never span pages), so the only way this triggers is: (1) a corrupted `previous_address` field in a record header, or (2) a corrupted hash index entry.

The pre-existing `read_header_and_match_key` (which uses a fixed RecordLayout) has the same issue, so this isn't new to this diff. But the diff's `read_header_and_match_key_varlen` makes it more relevant because variable-length compaction increases the frequency of chain walks.

#### Rebuttal Conditions

This is NOT a concern if: (1) `RecordAccessor::from_log` / `get_physical_address` internally bounds the access to the page frame regardless of the requested size; or (2) the allocator guarantees that all addresses in hash entries and record headers point to valid record positions; or (3) `safe_record_size` was intentionally designed to allow cross-page reads for some reason (the function name suggests otherwise).

#### Suggested Verification

Change `remaining.max(min_size)` to `remaining.min(page_size)` (or just use `remaining`) to ensure the accessor never exceeds the page boundary. If `remaining < RECORD_HEADER_SIZE`, the chain walk should return `None` since no valid record can exist there.

---

### Finding: The default `Key::serialized_size_from_bytes` fallback to full deserialization means custom Key implementations will panic on corrupted data during compaction

**Severity**: consider
**Confidence**: MEDIUM
**Category**: assumptions

#### Grounds (Evidence)

In `traits.rs:~3789`:
```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}
```

This is the default implementation that any custom `Key` type gets if they don't override it. The `deserialize` method for built-in types uses `expect()` / `unwrap()` on buffer lengths — e.g., `Vec<u8>::deserialize` at traits.rs:188 does `buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec()` which panics if `len` exceeds `buf.len()`.

If a downstream crate implements `Key` for their custom type without overriding `serialized_size_from_bytes`, and a page contains corrupted data, the compaction scanner will call the default impl, which calls `deserialize`, which panics. The "abort compaction on corruption" design decision (T-2 in the planning review) intended corruption to return `Err`, not panic.

#### Warrant (Rule)

The assumption is that the default implementation is "correct but slow" (as documented). But "correct" here means "produces the right answer for valid input." For invalid input — which is exactly the corruption scenario the compaction pipeline is designed to handle — the default implementation panics. The documented contract says "buf contains at least the bytes needed to determine the size," but during compaction the whole point of `record_size_from_bytes` is that we're reading bytes that MIGHT be corrupted.

The `record_size_from_bytes` wrapper validates `value_offset > remaining` and `total > remaining`, which catches most corruption before calling the trait method. But it cannot catch corruption that produces a plausible-but-wrong size (e.g., a prefix of 10 when the real value is 1000 — the computed total might still fit on the page).

#### Rebuttal Conditions

This is NOT a concern if: (1) all Key/Value types used in practice override `serialized_size_from_bytes` with corruption-safe implementations; or (2) the `record_size_from_bytes` guards reliably prevent corrupted data from reaching the trait method; or (3) the default is only a convenience for development and production types are expected to override it.

#### Suggested Verification

Add documentation to `serialized_size_from_bytes` stating: "WARNING: The default implementation may panic on corrupted input. Types used in stores with compaction enabled MUST override this with a corruption-safe implementation." Consider making the default return `Result` or adding a separate `try_serialized_size_from_bytes` method.

---

### Finding: `CompactionPlan` is consumed across phases with no validation that `tombstone_records` addresses are still valid by the time `swing()` reads them

**Severity**: consider
**Confidence**: LOW
**Category**: assumptions

#### Grounds (Evidence)

The scanner (Phase 1/K1) collects tombstone addresses in `plan.tombstone_records` (scanner.rs:~2913). The address updater (Phase 3/K3) reads keys from these addresses in `remove_tombstone` (address_update.rs:~2032). The orchestrator runs K1→K2→K3 under a single epoch guard (orchestrator.rs:166-193).

The assumption is that old addresses remain readable under epoch protection during K1-K3. This is explicitly stated in the Spec (line 746: "Records do not span page boundaries — the existing allocator ensures this — so per-record scanning within a single page is always safe") and the address_update.rs doc comment (line ~2020).

But what if a future change introduces batched epoch release (mentioned in C-2 as a possibility for large stores)? If the epoch guard is released between K1 and K3, pages in the compaction region could be evicted, making the tombstone addresses in the plan invalid.

#### Warrant (Rule)

This is a temporal coupling assumption — the plan is created at time T1 and consumed at time T3, with the validity depending on an external constraint (epoch guard held continuously). The assumption is correct today but undocumented in the `CompactionPlan` struct itself. The planning review mentioned epoch hold duration as C-2 (consider), and the Spec mentions it in the Risks section, but neither the `CompactionPlan` struct nor its doc comments state "must be consumed under the same epoch guard that produced it."

#### Rebuttal Conditions

This is NOT a concern if: (1) the epoch guard is architecturally guaranteed to span K1-K3 and this is never changed; or (2) the `CompactionPlan` struct carries a marker or assertion that validates it was created in the current epoch; or (3) the concern about epoch batching is resolved by documenting the single-guard requirement.

#### Suggested Verification

Add a doc comment to `CompactionPlan`: "// Lifetime: must be consumed under the same epoch guard that produced it. Tombstone addresses reference pages in the compaction region that may be evicted if the epoch is released."
