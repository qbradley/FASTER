# Release Manager Review — Red-Team Perspective

**Perspective**: red-team
**Reviewer**: Release Manager Specialist
**Diff scope**: ~3,400 lines across 21 files — variable-length record compaction for FASTER Rust engine

---

## Scenario

A consumer of the `faster-core` crate wants to exploit or stress the new API surface to find backward-incompatible or surprising behaviors. What attack vectors exist?

---

### Finding 1: `compact::<K, V>()` type parameters are NOT constrained to match the store's `Functions::Key`/`Functions::Value` types — silent data corruption possible

**Severity**: must-fix
**Confidence**: HIGH
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/store/kv.rs` (diff line 4251):

```rust
pub fn compact<K: Key, V: Value>(&self) -> Result<CompactionResult, CompactionError> {
```

The store is `FasterKv<F: Functions>` where `F::Key` and `F::Value` are the actual types stored. But `compact()` accepts *any* `K: Key, V: Value` — there is no `where K: F::Key` or similar constraint. A consumer can legally call:

```rust
let store: FasterKv<SimpleFunctions<u64, u64>> = ...;
store.compact::<Vec<u8>, Vec<u8>>(); // compiles, interprets u64 records as Vec<u8>
```

This causes `record_size_from_bytes::<Vec<u8>, Vec<u8>>()` to read the first 4 bytes of a `u64` key as a length prefix. For a key like `42u64` (LE bytes: `[42, 0, 0, 0, 0, 0, 0, 0]`), the length prefix reads as `42`, causing the scanner to compute `key_size = 4 + 42 = 46` bytes and read 46 bytes past the key offset — reading into adjacent records or page padding as if it were key data. This bypasses the corruption check if `46 + padding` happens to fit within the page.

The same issue exists for `maybe_compact::<K, V>()` (diff line 4260).

#### Warrant (Rule)

An API that allows a type mismatch between the data format on disk and the interpretation at read time is a **data-safety hole**. During compaction, this mismatch could:
- Classify live records as dead (data loss)
- Copy wrong byte ranges (data corruption in the compacted region)
- Skip records that span the miscomputed size (silent truncation)

This is not a hypothetical — the pre-existing API had `K: FixedSizeKey, V: FixedSizeValue` which had the same hole, but the blast radius was smaller because fixed-size types all have compile-time-constant sizes. Variable-length types amplify the risk because the size is read from data bytes.

#### Rebuttal Conditions

This is NOT a concern if: (1) there is a runtime assertion in `record_size_from_bytes` that always catches mismatched types — but the corruption check only validates that the total fits within the page, not that the data is semantically correct; (2) the mismatch always triggers `RecordSizeError::CorruptedRecord` — it will NOT for small numeric keys because `u64` byte patterns often produce small u32 length values that fit within the page.

#### Suggested Verification

Add a compile-time constraint: `pub fn compact<K, V>(&self) -> ... where K: Key, V: Value, F: Functions<Key = K, Value = V>` to enforce that the generic params match the store's actual types. Alternatively, remove the generic params entirely and use `F::Key`/`F::Value` directly. Write a test that calls `compact::<Vec<u8>, Vec<u8>>()` on a `u64`-typed store and assert it either fails to compile or returns an error.

---

### Finding 2: `CompactionError` enum lacks `#[non_exhaustive]` — downstream `match` breaks on upgrade

**Severity**: should-fix
**Confidence**: HIGH
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/compaction/orchestrator.rs` (diff lines 2655–2661):

```rust
pub enum CompactionError {
    EmptyRegion { begin: LogicalAddress, until: LogicalAddress },
    CopyFailed(CopyError),
    EpochDrainTimeout,
+   ScanCorruption(RecordSizeError),  // NEW
}
```

No `#[non_exhaustive]` annotation. A consumer with:

```rust
match err {
    CompactionError::EmptyRegion { .. } => { ... }
    CompactionError::CopyFailed(_) => { ... }
    CompactionError::EpochDrainTimeout => { ... }
}
```

will get a "non-exhaustive patterns" error after upgrading. The error message won't point to a migration guide.

#### Warrant (Rule)

This is the "weekly batch job" pattern from the release-manager backstory: a downstream consumer that runs infrequently discovers the break after the change has been deployed. For a storage engine error type, exhaustive matching is common practice because consumers want to handle each failure mode differently (e.g., retry on `EpochDrainTimeout`, alert on `ScanCorruption`). The new variant is the correct addition — the missing safeguard is `#[non_exhaustive]`.

#### Rebuttal Conditions

Only safe if the team has a monorepo-wide policy requiring `_ =>` catch-all arms on all external enum matches, or if CI builds all consumers on every commit.

#### Suggested Verification

Add `#[non_exhaustive]` to `CompactionError`. This is a one-line change that prevents all future variant additions from being breaking changes.

---

### Finding 3: Default `eq_from_bytes` and `serialized_size_from_bytes` impls panic on truncated buffers for custom Key types

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/record/traits.rs` (diff lines 3789–3807):

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}

fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    *self == Self::deserialize(buf)
}
```

The default implementations delegate to `deserialize()`, which for all built-in types panics on short buffers (e.g., `expect("buffer too short...")`). While the built-in types (`Vec<u8>`, `String`, numerics) provide optimized overrides with proper bounds checking, a **custom Key type** that only implements the three required trait methods (`serialized_size`, `serialize`, `deserialize`) will inherit these defaults. If the compaction scanner ever passes a truncated buffer to the default impl (e.g., at a page boundary race), the process panics instead of returning a recoverable error.

The doc comment (diff line 3788) says "the default implementation deserializes fully — override for performance" but does not warn about the panic risk on truncated input.

#### Warrant (Rule)

A storage engine API where "forgetting to override an optional method" leads to a panic during compaction (a background maintenance operation) is a deployment hazard. The consumer's process terminates during a routine compaction cycle, and the only fix is to implement the override methods they didn't know were necessary for correctness (not just performance). The doc comment frames the override as a performance optimization, masking the safety requirement.

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` always validates that sufficient bytes remain before calling `serialized_size_from_bytes` — it does check `MIN_READABLE` (12 bytes) but this may not be sufficient for all custom key types; (2) `deserialize()` is required by the trait contract to handle truncated input gracefully — the current doc comments do not specify this.

#### Suggested Verification

Update the `serialized_size_from_bytes` and `eq_from_bytes` doc comments to explicitly state that `buf` may be truncated and that the implementation must not panic on short input. Alternatively, add `buf.len()` guards in the default impls that return a sentinel or early-return false.

---

### Finding 4: `LENGTH_PREFIX_SIZE` export creates an implicit ABI contract with downstream custom Key/Value implementors

**Severity**: consider
**Confidence**: MEDIUM
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/record/traits.rs` (diff line 3867):

```rust
-const LENGTH_PREFIX_SIZE: usize = 4;
+pub const LENGTH_PREFIX_SIZE: usize = 4;
```

Re-exported via `record/mod.rs` (diff line 3698):

```rust
+pub use traits::{FixedSizeKey, FixedSizeValue, Key, LENGTH_PREFIX_SIZE, Value};
```

And referenced in the doc example (diff line 3741):

```rust
//! # use faster_core::record::{Key, Value, LENGTH_PREFIX_SIZE};
//! fn serialized_size_from_bytes(buf: &[u8]) -> usize {
//!     let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
//!     LENGTH_PREFIX_SIZE + len
//! }
```

This teaches downstream implementors to use `LENGTH_PREFIX_SIZE` as part of their `serialized_size_from_bytes` implementation. If this constant ever changes (e.g., to support >4GB records with a u64 prefix), all downstream implementations silently break — they'll read the wrong number of prefix bytes.

#### Warrant (Rule)

Exporting an internal constant and embedding it in API documentation creates an implicit contract that the value won't change. This is the "configuration file" pattern: a value that worked at documentation time becomes a coupling point. The risk is low (4-byte prefix is deeply baked in), but the coupling is now explicit and documented, making it harder to change later.

#### Rebuttal Conditions

This is NOT a concern if: (1) the team considers `LENGTH_PREFIX_SIZE = 4` a permanent invariant of the serialization format; (2) any format change would require a new trait or major version anyway. Both are likely true for a storage engine.

#### Suggested Verification

Add a doc comment to `LENGTH_PREFIX_SIZE` stating this is a stable format constant: `/// This is a stable serialization format constant. Changing it is a breaking change.`

---

### Finding 5: `read_header_and_match_key_varlen` uses `safe_record_size` which may return a smaller-than-actual access window

**Severity**: consider
**Confidence**: LOW
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/hybrid_log/record_ops.rs` (diff lines 3364–3371):

```rust
pub fn read_header_and_match_key_varlen<K: Key>(
    &self,
    addr: LogicalAddress,
    key: &K,
) -> Option<(RecordInfo, bool)> {
    let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
    let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
    ...
    let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
```

`safe_record_size` computes the remaining page bytes from the record's offset. The `accessor.as_slice()` then provides a view of `[KEY_OFFSET..]` which is the remaining page bytes minus the header. For `eq_from_bytes`, this slice may contain data from subsequent records, which is fine for comparison (the key implementation knows its own length). But if a custom `eq_from_bytes` implementation reads *past* its key data (e.g., using a buggy length calculation), it would silently compare against garbage from the next record.

This method is now part of the public API (visible in compaction scanner's version chain walk), and its safety depends on the correctness of `Key::eq_from_bytes` — a method that downstream consumers are encouraged to override.

#### Warrant (Rule)

Public methods that delegate correctness to user-implemented trait methods without defensive checks create an implicit trust boundary. The scanner will not detect if `eq_from_bytes` reads beyond the key's serialized size — it will simply get wrong comparison results, potentially classifying live records as dead (data loss during compaction).

#### Rebuttal Conditions

This is NOT a concern if: (1) `eq_from_bytes` is documented to only read up to `serialized_size_from_bytes(buf)` bytes; (2) all built-in implementations respect this contract (they do). The risk is limited to custom types.

#### Suggested Verification

Add a doc comment to `eq_from_bytes` specifying: "Implementations must not read beyond `Self::serialized_size_from_bytes(buf)` bytes." Add a debug assertion in test builds that validates the contract.

---

## Deployment-Path Traces

| Path | Status | Notes |
|------|--------|-------|
| Type-safety of compact() API | ❌ Finding 1 | K/V generics not constrained to store's actual types |
| Enum exhaustiveness | ⚠️ Finding 2 | `CompactionError` missing `#[non_exhaustive]` |
| Default trait impl safety | ⚠️ Finding 3 | Panic on truncated buffer in default `deserialize` path |
| Exported constant stability | ℹ️ Finding 4 | `LENGTH_PREFIX_SIZE` now public API without stability annotation |
| Trait implementation trust boundary | ℹ️ Finding 5 | `eq_from_bytes` correctness is trusted without runtime validation |
| On-disk format compatibility | ✅ OK | No changes to record layout, serialization format, or log structure |
| Rolling deploy coexistence | ✅ OK | Old and new binaries read the same log format; no behavioral divergence during transition |
| Rollback path | ✅ OK | No destructive data changes; reverting to previous binary reads the same log |
