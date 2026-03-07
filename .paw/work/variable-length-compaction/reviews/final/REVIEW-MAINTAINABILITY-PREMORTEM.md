# REVIEW-MAINTAINABILITY-PREMORTEM.md

**Perspective**: premortem
**Specialist**: maintainability
**Scenario**: 6 months later, the code has become a maintenance burden. What makes future changes difficult?

---

## Cognitive Strategy Applied

Narrative code walkthrough: I read the diff as a new maintainer encountering these files for the first time during a 2 AM incident. I traced the critical `KEY_OFFSET = 8` invariant across files, followed the `serialized_size_from_bytes` → `record_size_from_bytes` → scanner call chain, read the test names to understand intent, and simulated an error investigation through the corruption abort path.

---

### Finding: The critical `KEY_OFFSET = 8` invariant is repeated in 3 files without compile-time enforcement, creating a fragile coupling

**Severity**: should-fix
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

The constant `KEY_OFFSET: usize = 8` is independently defined in:
- `compaction/scanner.rs:87` — `const KEY_OFFSET: usize = 8;` (module-level)
- `compaction/address_update.rs:101` — `const KEY_OFFSET: usize = 8;` (inside `swing()`)
- `hybrid_log/record_ops.rs:554` — `const KEY_OFFSET: usize = 8;` (inside `read_header_and_match_key_varlen`)

Each location includes a doc comment explaining the invariant (`pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT) == 8`), but the value is hardcoded rather than computed. Only `address_update.rs:99-101` has a `debug_assert_eq!(RECORD_HEADER_SIZE, 8, ...)` to partially verify the assumption at runtime.

The `record_size_from_bytes` function in `layout.rs:327` computes `key_offset` dynamically: `let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)` — so it does *not* hardcode 8. This means the layout module will correctly handle a change to RECORD_HEADER_SIZE, but the scanner and address updater will not.

#### Warrant (Rule)

Imagine being paged 6 months from now because someone added a 4-byte field to `RecordInfo` (now 12 bytes), changing the alignment calculation. `record_size_from_bytes` in `layout.rs` computes the new offset correctly (16), but the scanner, address updater, and chain walker all silently use `KEY_OFFSET = 8`, reading garbage bytes as key data. The `debug_assert` in `address_update.rs` catches this in tests but not in release builds. The scanner and `record_ops.rs` have no assert at all.

A single-point invariant maintained by copy-paste across 3 modules is a maintenance trap. It should be a single `pub const` in `record/layout.rs` or computed dynamically.

#### Rebuttal Conditions

This is NOT a concern if: (1) `RecordInfo` is guaranteed to remain 8 bytes by design and will never change (add a `const_assert!` to prove it), or (2) the `KEY_OFFSET` is replaced with a `pub const` computed from `pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)` in `layout.rs` and all consumers import it.

#### Suggested Verification

Add to `record/layout.rs`:
```rust
pub const KEY_OFFSET: usize = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);
```
Replace all 3 local definitions with `use crate::record::KEY_OFFSET`. Add a `const_assert_eq!(KEY_OFFSET, 8)` if the value is structurally guaranteed.

---

### Finding: `MIN_READABLE` is defined independently in both `scanner.rs` and `layout.rs` with different types, risking silent divergence

**Severity**: consider
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

- `compaction/scanner.rs:81` — `const MIN_READABLE: u32 = (RECORD_HEADER_SIZE + crate::record::LENGTH_PREFIX_SIZE) as u32;`
- `record/layout.rs:322` — `const MIN_READABLE: usize = RECORD_HEADER_SIZE + LENGTH_PREFIX_SIZE; // 12`

Both compute the same value (12) from the same components, but scanner.rs casts to `u32` and layout.rs uses `usize`. They also have different visibility: both are module-private. If either RECORD_HEADER_SIZE or LENGTH_PREFIX_SIZE changes, both must be updated independently — and they'd have to be found by grep, not compiler errors.

#### Warrant (Rule)

When the same derived constant appears in two modules with different types and no shared definition, future changes to the base constants produce a maintenance hazard. A future developer updating `LENGTH_PREFIX_SIZE` to support 8-byte prefixes would need to find both definitions. The scanner constant is `u32` because page offsets are `u32`, adding a type conversion that could mask overflow on very large page sizes.

#### Rebuttal Conditions

This is NOT a concern if: (1) `RECORD_HEADER_SIZE` and `LENGTH_PREFIX_SIZE` are structurally frozen constants that will never change, or (2) the scanner is the only caller of `record_size_from_bytes` so the pre-check is redundant (the function does its own check).

#### Suggested Verification

Consider promoting `MIN_READABLE` to a `pub const` in `record/layout.rs` so the scanner imports it rather than re-deriving it.

---

### Finding: `serialized_size_from_bytes` for `Vec<u8>` and `String` panics on short buffers, while `eq_from_bytes` gracefully returns false — inconsistent defensive coding

**Severity**: should-fix
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

`Vec<u8>` in `traits.rs`:
- `serialized_size_from_bytes` (line ~317): `buf[..LENGTH_PREFIX_SIZE].try_into().expect("buffer too short for Vec<u8> length prefix")` — **panics**.
- `eq_from_bytes` (line ~327): `if buf.len() < LENGTH_PREFIX_SIZE { return false; }` — **gracefully handles short buffer**.

Same pattern in `String` at lines ~395 and ~405.

The `record_size_from_bytes` function in `layout.rs` does check `remaining < MIN_READABLE` (12 bytes) before calling `K::serialized_size_from_bytes`. However, after reading the key size, the validation is `if value_offset > remaining { return Err(...) }`, which only confirms the value *starts* within the page — not that there are enough bytes to read a length prefix at `value_offset`. If `remaining - value_offset` is 1, 2, or 3 bytes, `V::serialized_size_from_bytes(value_buf)` will panic with `.expect()`.

#### Warrant (Rule)

A future developer will reasonably assume that `record_size_from_bytes` returns `Err` on any corrupted data because the function's doc says *"Returns CorruptedRecord if the computed field offsets or total size exceed the remaining page space."* But a corrupted key prefix that happens to produce a `value_offset` that leaves 1-3 bytes for the value will panic inside `serialized_size_from_bytes` rather than returning an error. At 2 AM, the panic backtrace points to `traits.rs` deep inside a trait method — not to the compaction scanner. The inconsistency between the two methods (panic vs graceful) means a maintainer can't form a mental model of "how do these methods handle bad input?"

#### Rebuttal Conditions

This is NOT a concern if: (1) `record_size_from_bytes` adds an explicit check `if remaining - value_offset < LENGTH_PREFIX_SIZE { return Err(CorruptedRecord) }` before calling `V::serialized_size_from_bytes`, which would make the trait method's panic unreachable; or (2) the allocator guarantees that any page byte sequence after a valid key will always have at least 4 bytes for a value prefix.

#### Suggested Verification

Add after the `value_offset > remaining` check in `record_size_from_bytes`:
```rust
if remaining - value_offset < LENGTH_PREFIX_SIZE {
    return Err(RecordSizeError::CorruptedRecord { offset });
}
```
Or make `serialized_size_from_bytes` return `Result<usize, ...>` for consistency with the error-returning pattern.

---

### Finding: Variable-length address updater unit tests use only `u64` types despite test names claiming variable-length coverage

**Severity**: should-fix
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

In `compaction/address_update.rs` (test section starting at diff line 2309):

- Test `swing_variable_length_different_key_sizes` (line 2310): Uses `u64` keys and `u64` values. The test name says "variable length different key sizes" but the comment at line 2321 admits: *"The real test is that swing_one reads each record using its own record_size from AddressMapping rather than a shared layout."* All records have identical fixed sizes.

- Test `tombstone_removal_variable_length` (line 2369): Also uses `u64` keys. Name says "variable-length."

- Test `swing_mixed_records_same_cycle` (line 2440): Uses `u64` keys with different value magnitudes (which doesn't change serialized size for `u64`).

None of these tests exercise actual `Vec<u8>` or `String` keys through the address updater. The integration tests in `compaction_integration.rs` do test with `Vec<u8>`, but the unit-level tests for the address updater don't.

#### Warrant (Rule)

Test names are documentation. A future maintainer who reads `swing_variable_length_different_key_sizes` and sees it passing will believe the variable-length path is unit-tested at the address updater level. When a bug is reported in the `swing_one` path for `Vec<u8>` keys, the maintainer will look at this test, see it passes, and conclude the issue must be elsewhere — wasting debugging time. The test verifies record_size propagation (useful) but not the actual key deserialization path for variable-length types.

#### Rebuttal Conditions

This is NOT a concern if: (1) the integration tests provide sufficient coverage for the variable-length address updater path and unit tests are intentionally limited to fixed-size; (2) the test names are updated to reflect what they actually test (e.g., `swing_propagates_per_record_size`).

#### Suggested Verification

Either: (a) rename the tests to reflect what they actually verify, or (b) add a test that creates a store with `VarLenFunctions` (like the integration tests) and exercises the address updater directly with `Vec<u8>` keys of different sizes.

---

### Finding: Tombstone removal test silently discards read results for deleted keys instead of asserting expected behavior

**Severity**: consider
**Confidence**: HIGH
**Category**: maintainability

#### Grounds (Evidence)

In `address_update.rs`, test `tombstone_removal_variable_length` (diff line 2422-2425):
```rust
for i in 0u64..10 {
    let _result = store.read_simple(&mut session, &i);
    // After tombstone removal, reads may return None or default.
    // The key point: we shouldn't crash, and surviving keys work.
}
```

The result is bound to `_result` (explicitly ignored) with a comment explaining the ambiguity. No assertion is made about what the read returns — it could be `Some(value)`, `None`, or anything else.

#### Warrant (Rule)

Tests should document intent. This test says "tombstone removal works" but doesn't verify that deleted keys are actually gone. The comment reveals uncertainty about expected behavior: "reads may return None or default." A future developer modifying tombstone removal who causes deleted keys to start returning their old values would not be caught by this test. The test's value is limited to "doesn't crash."

#### Rebuttal Conditions

This is NOT a concern if: (1) the exact post-tombstone-removal behavior depends on hash index state that's non-deterministic at the unit test level, making assertions unreliable; (2) the integration tests in `compaction_integration.rs` properly assert `NotFound` for deleted keys.

#### Suggested Verification

Inspect the integration test `compact_variable_length_tombstones` to verify it asserts deleted keys return appropriate status. If so, consider adding a comment in the unit test referencing the integration test. If not, add assertions.

---

### Finding: No test exercises the default trait implementations of `serialized_size_from_bytes` and `eq_from_bytes`

**Severity**: consider
**Confidence**: MEDIUM
**Category**: maintainability

#### Grounds (Evidence)

The `Key` trait provides default implementations (traits.rs:127-128, 143-144):
```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}
fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    *self == Self::deserialize(buf)
}
```

Every built-in type (`u32`, `u64`, `i32`, `i64`, `Vec<u8>`, `String`) provides optimized overrides. The test suite only tests these overrides. No test creates a type that relies on the default implementations to verify they work correctly and match the optimized overrides' semantics.

#### Warrant (Rule)

The defaults exist for backward compatibility (MF-4 from the planning review). If a downstream crate implements `Key` without overriding these methods, it silently uses the defaults. If a future change breaks the defaults (e.g., a refactor to `serialized_size` that changes semantics), no test catches it. The planning review specifically called out backward compatibility as a must-fix — the default implementations are the mechanism, but they're untested.

#### Rebuttal Conditions

This is NOT a concern if: (1) the default implementations are trivially correct (they delegate to `deserialize` + `serialized_size`, which are already tested), and the risk of them breaking independently is negligible.

#### Suggested Verification

Add a test with a custom struct implementing `Key` that does NOT override `serialized_size_from_bytes` or `eq_from_bytes`, then verify the defaults produce correct results.
