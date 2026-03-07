# Architecture Review — Premortem Perspective

**Perspective**: premortem
**Reviewer**: Architecture Specialist
**Scope**: Variable-length record compaction — ~3,400 lines across 21 files
**Scenario**: 6 months post-deployment, the architecture has caused maintenance pain

---

### Finding: Unused `V: Value` type parameter on `AddressUpdater::swing` creates phantom dependency

**Severity**: should-fix
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/compaction/address_update.rs` (diff lines 1937-1943), the `swing` method signature was changed to:

```rust
pub fn swing<K: Key, V: Value>(
    &self,
    copy_result: &CopyResult,
    plan: &CompactionPlan,
) -> SwingStats {
```

The doc comment at diff line 1928 states: "`V` is needed so that the updater can construct per-record layouts for reading keys." However, examining the entire method body (diff lines 1944-1979), `V` is **never used**. The method uses `KEY_OFFSET` (a hardcoded constant), `K::deserialize`, and `mapping.record_size` — no reference to `V` anywhere in `swing`, `swing_one`, or `remove_tombstone`.

The orchestrator at `orchestrator.rs` (diff line 2713) calls `updater.swing::<K, V>(...)`, propagating the phantom constraint upward. The public API `compact<K: Key, V: Value>()` at `store/kv.rs` (diff line 4251) carries this requirement.

#### Warrant (Rule)

A type parameter that exists in a function signature but is never used in its body is a structural lie. It tells callers "this operation depends on V" when it doesn't. The codebase's existing pattern is precise type constraints — the original `swing<K: Key>` only required what it used. Adding `V: Value` violates this convention and creates three maintenance hazards:

1. **False coupling**: If someone changes `V` in `compact::<K, V>()`, they'll investigate whether the address updater handles the new type correctly — wasting time since it doesn't use `V` at all.
2. **Doc/code divergence**: The doc comment justifying `V` is already incorrect and will accumulate more incorrect justifications over time.
3. **Constraint propagation**: The phantom `V` must be threaded through every caller, increasing signature complexity for zero benefit.

#### Rebuttal Conditions

This is NOT a concern if: (1) there's a planned follow-up that will use `V` in the address updater (e.g., for value-aware deduplication during swing); or (2) the `V` parameter is retained for API symmetry with the scanner's `scan::<K, V>()`, and the team considers signature consistency more important than precision.

#### Suggested Verification

Remove `V: Value` from `swing` and verify the project compiles. If it does, the parameter is confirmed phantom. If callers need `V` for other reasons (e.g., turbofish syntax at the call site), document why.

---

### Finding: `LiveRecord` type used for tombstone records is a semantic mismatch that will mislead maintainers

**Severity**: should-fix
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/compaction/mod.rs` (diff lines 2631-2635), the `CompactionPlan` gains a new field:

```rust
pub tombstone_records: Vec<LiveRecord>,
```

`LiveRecord` is defined at `mod.rs:42-48` as:

```rust
pub struct LiveRecord {
    pub address: LogicalAddress,
    pub record_size: usize,
}
```

The scanner at `scanner.rs` (diff line 2916) pushes tombstoned records into this Vec:

```rust
plan.tombstone_records.push(LiveRecord {
    address: current,
    record_size,
});
```

The name `LiveRecord` specifically connotes "a record that is alive/current" — tombstones are the semantic opposite. The existing `live_records` field was named for this meaning (only records that pass `is_current_version` qualify).

#### Warrant (Rule)

The codebase's naming convention uses semantically precise names: `LiveRecord` for live records, `SwingStats` for swing statistics, `CompactionPlan` for plans. Reusing `LiveRecord` for tombstones creates a "type pun" — the type's name asserts something false about its contents. In 6 months, a developer reading `tombstone_records: Vec<LiveRecord>` will wonder: "Are these live? Were they live at some point? Is `LiveRecord` misnamed?" This increases cognitive load for every reader.

The existing codebase pattern is to create distinct types for distinct concepts (e.g., `CopyResult` vs `CompactionPlan` vs `SwingStats`), even when the struct fields overlap. The correct fix is a shared `ScannedRecord { address, record_size }` type that both `live_records` and `tombstone_records` use, or renaming `LiveRecord` to `ScannedRecord`.

#### Rebuttal Conditions

This is NOT a concern if: (1) the team has decided that `LiveRecord` is a generic "record found during scanning" type regardless of liveness, and this is documented; or (2) the tombstone_records field is temporary and will be replaced by a different mechanism (e.g., a bitmap) in a future refactor.

#### Suggested Verification

Rename `LiveRecord` to `ScannedRecord` and run `cargo build`. If all uses compile, the name was always semantically imprecise.

---

### Finding: `KEY_OFFSET = 8` hardcoded invariant duplicated across 3 modules without centralization

**Severity**: consider
**Confidence**: HIGH
**Category**: architecture

#### Grounds (Evidence)

The constant `KEY_OFFSET: usize = 8` (representing `pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)`) is defined independently in:

1. `compaction/scanner.rs` (diff line 2786): `const KEY_OFFSET: usize = 8;` — module-level
2. `compaction/address_update.rs` (diff line 1952): `const KEY_OFFSET: usize = 8;` — inside `swing()` body
3. `hybrid_log/record_ops.rs` (diff line 3370): `const KEY_OFFSET: usize = 8;` — inside `read_header_and_match_key_varlen()`

Each location includes a comment explaining the derivation from `pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT)`. The `address_update.rs` version also includes a `debug_assert_eq!(RECORD_HEADER_SIZE, 8, ...)` guard.

Meanwhile, `layout.rs` already defines both `RECORD_HEADER_SIZE` and `RECORD_ALIGNMENT` as public constants, and `pad_alignment` is a public `const fn`. The derived constant could trivially be:

```rust
pub const KEY_OFFSET: usize = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);
```

#### Warrant (Rule)

While each individual constant is small and documented, the duplication pattern violates the codebase's convention of centralizing layout constants in `layout.rs`. `RECORD_HEADER_SIZE`, `RECORD_ALIGNMENT`, and `pad_alignment` all live in `layout.rs`. A derived constant like `KEY_OFFSET` belongs there too. The `debug_assert` in `address_update.rs` implicitly acknowledges that this invariant could break — yet only one of three sites guards against it. If `RECORD_ALIGNMENT` changes to 16 (e.g., for SIMD alignment), two of three sites will silently hardcode the wrong offset.

#### Rebuttal Conditions

This is NOT a concern if: (1) the team prefers locality over centralization for small constants, accepting the tradeoff of potential divergence; or (2) there's a policy against adding more public constants to the `record` module's API surface.

#### Suggested Verification

Add `pub const KEY_OFFSET: usize = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);` to `layout.rs`, replace the three local definitions with imports, and verify compilation. Add a `const_assert!(KEY_OFFSET == 8)` if the crate supports static assertions.

---

### Finding: `CompactionPlan` evolving into a cross-phase communication channel increases coupling between scanner and address updater

**Severity**: consider
**Confidence**: MEDIUM
**Category**: architecture

#### Grounds (Evidence)

Previously, the address updater received explicit, narrow parameters:

```rust
// Old signature (diff line 1935)
pub fn swing<K: Key>(
    &self,
    copy_result: &CopyResult,
    tombstone_addresses: &[LogicalAddress],
    key_size: usize,
    value_size: usize,
) -> SwingStats {
```

Now it receives the entire `CompactionPlan`:

```rust
// New signature (diff line 1937)
pub fn swing<K: Key, V: Value>(
    &self,
    copy_result: &CopyResult,
    plan: &CompactionPlan,
) -> SwingStats {
```

The updater only uses `plan.tombstone_records` from the plan. But by accepting `&CompactionPlan`, it has visibility into `live_records`, `dead_count`, `tombstone_count`, `total_bytes_scanned`, and `live_bytes` — data it doesn't need.

The orchestrator at `orchestrator.rs` (diff line 2713) now passes the full plan to the updater, creating a data flow where the scanner's output struct is shared across the copier (which uses `plan.live_records`) and the updater (which uses `plan.tombstone_records`).

#### Warrant (Rule)

The existing codebase pattern is narrow interfaces: the copier receives `&[LiveRecord]` (not the full plan), and the old updater received `&[LogicalAddress]`. Passing the full `CompactionPlan` to the address updater widens the interface unnecessarily. In 6 months, someone adding a field to `CompactionPlan` for the scanner's benefit may inadvertently affect the updater's behavior or test setup. The address updater tests already had to create `empty_plan()` helpers (diff line 2069) that initialize fields the updater never reads.

The Interface Segregation Principle suggests the updater should receive only what it needs: `&[LiveRecord]` for tombstones and the `CopyResult` for live records.

#### Rebuttal Conditions

This is NOT a concern if: (1) there are planned future phases where the address updater needs more fields from the plan (e.g., `dead_count` for metrics); or (2) the team prefers passing the full plan for simplicity and testability over interface minimalism; or (3) the `CompactionPlan` is expected to remain small and stable.

#### Suggested Verification

Change `swing` to accept `tombstone_records: &[LiveRecord]` instead of `&CompactionPlan`. If the change is mechanical (only extracting `.tombstone_records`), the wider interface was unnecessary.

---

### Finding: Default implementations of `serialized_size_from_bytes` and `eq_from_bytes` create a silent performance cliff for custom Key/Value types

**Severity**: consider
**Confidence**: MEDIUM
**Category**: architecture

#### Grounds (Evidence)

In `rust/crates/faster-core/src/record/traits.rs` (diff lines 3789-3807), new default implementations are added:

```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}

fn eq_from_bytes(&self, buf: &[u8]) -> bool {
    *self == Self::deserialize(buf)
}
```

These defaults fully deserialize the key/value — allocating memory for types like `Vec<u8>` or `String` — and then compute/compare. The built-in types (`u64`, `Vec<u8>`, `String`) all override with zero-copy implementations. But a custom type implementing `Key` will inherit the default and silently perform full deserialization on every record during compaction scanning.

For a compaction of 10M records with 1KB keys, the default `eq_from_bytes` would allocate and free ~10M heap objects during the version chain walk. The custom zero-copy implementation allocates zero.

#### Warrant (Rule)

The existing trait pattern in this codebase is three required methods (`serialized_size`, `serialize`, `deserialize`) with no defaults — implementors must consciously handle each. Adding defaulted methods changes the contract: implementors can now skip these methods and get correct-but-slow behavior. Rust has no `#[must_override]` attribute, so the only defense is documentation.

The doc comments (diff lines 3776-3791) do mention "override for performance," but this is a compile-time-invisible cliff. The codebase's established pattern for performance-critical traits is `FixedSizeKey`/`FixedSizeValue` marker traits that require explicit opt-in. The new methods create a different pattern where performance is opt-in via override rather than trait bound.

#### Rebuttal Conditions

This is NOT a concern if: (1) the team expects very few custom Key/Value implementations; (2) clippy or CI tooling checks for missing overrides; or (3) the performance difference is negligible for expected workload sizes (small record counts, small keys).

#### Suggested Verification

Add a `tracing::warn_once!` or `#[cfg(debug_assertions)]` log in the default implementations to alert developers when the fallback path is used during compaction. This makes the performance cliff visible.
