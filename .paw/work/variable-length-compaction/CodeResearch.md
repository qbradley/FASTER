# Code Research: Variable-Length Record Compaction

**Branch**: feature/variable-length-compaction | **Created**: 2025-07-15

## Existing Compaction Pipeline

The compaction pipeline is fully implemented for fixed-size records in `rust/crates/faster-core/src/compaction/`:

| File | Lines | Purpose |
|------|-------|---------|
| `mod.rs` | 95 | Core types: `LiveRecord`, `CompactionPlan` |
| `scanner.rs` | 702 | K1 — Scan records, classify live/dead/tombstone |
| `copier.rs` | 563 | K2 — Copy live records to log tail |
| `address_update.rs` | 603 | K3 — CAS pointer swing in hash index |
| `begin_address.rs` | 359 | K4 — Advance begin_address, truncate device |
| `orchestrator.rs` | 490 | Wire K1-K4 into single cycle with epoch coordination |
| `policy.rs` | 558 | Compaction trigger policy |

## Key Constraint: FixedSizeKey/FixedSizeValue Bounds

### Orchestrator (orchestrator.rs:147-157)
```rust
pub(crate) fn run<K: FixedSizeKey, V: FixedSizeValue>(
    ...
) -> Result<CompactionResult, CompactionError> {
    let key_size = K::SIZE;       // compile-time constant
    let value_size = V::SIZE;     // compile-time constant
```

### Public API (store/kv.rs:1407-1409)
```rust
pub fn compact<K: FixedSizeKey, V: FixedSizeValue>(
    &self,
) -> Result<CompactionResult, CompactionError>
```

### Scanner (scanner.rs:93-98)
```rust
pub fn scan<K: Key>(
    &self,
    begin_address: LogicalAddress,
    until_address: LogicalAddress,
    key_size: usize,
    value_size: usize,
) -> CompactionPlan
```

## Scanner Fixed-Stride Iteration (scanner.rs:105-163)

The scanner computes layout once and uses a **fixed stride**:
```rust
let layout = RecordLayout::compute(key_size, value_size);
let record_size = layout.total_size();  // single stride for all records
// ...
while current < until_address {
    // page boundary skip
    if offset > 0 && (page_size - offset) < record_size as u32 { ... }
    // read record, classify, advance by FIXED stride
    advance(&mut current, record_size as u32, page_size);
}
```

The `advance()` helper (scanner.rs:236-245) moves by a fixed step size.

### is_current_version (scanner.rs:180-233)
- Hashes key, looks up hash index entry
- Fast path: if record IS the chain head → live
- Slow path: walks version chain with depth limit
- **Conservative**: returns `true` (live) in ambiguous cases

## Record Size Discovery

### RecordInfo Header — NO Size Field (record/record_info.rs:1-80)
```
  63    62    61    60..48           47..0
┌─────┬─────┬─────┬──────────────┬───────────────────────────────┐
│ Fin │ Tom │ Inv │ Version (13) │ Previous Address (48)         │
└─────┴─────┴─────┴──────────────┴───────────────────────────────┘
```
**No size field** — size must be derived from key/value length prefixes.

### Variable-Length Serialization (record/traits.rs:171-217)
```rust
const LENGTH_PREFIX_SIZE: usize = 4;  // NOT exported (private const)

// Vec<u8> as Key
fn serialized_size(&self) -> usize { LENGTH_PREFIX_SIZE + self.len() }
fn serialize(&self, buf: &mut [u8]) -> usize {
    buf[..4].copy_from_slice(&(self.len() as u32).to_le_bytes());
    buf[4..4+self.len()].copy_from_slice(self);
    4 + self.len()
}
fn deserialize(buf: &[u8]) -> Self {
    let len = u32::from_le_bytes(buf[..4].try_into()...) as usize;
    buf[4..4+len].to_vec()
}
```

### RecordLayout Computation (record/layout.rs:119-137)
```rust
pub const fn compute(key_size: usize, value_size: usize) -> Self {
    let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);  // = 8
    let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);
    let total_size = pad_alignment(value_offset + value_size, RECORD_ALIGNMENT);
    Self { key_offset, value_offset, total_size }
}
```

### Reading Size from Raw Bytes (NO EXISTING HELPER)
To discover a variable-length record's size from raw page bytes:
1. Read key length prefix at offset 8 (key_offset = RECORD_HEADER_SIZE rounded to RECORD_ALIGNMENT = 8)
2. key_serialized_size = 4 + u32::from_le_bytes(bytes[8..12])
3. value_offset = pad_alignment(8 + key_serialized_size, 8)
4. Read value length prefix at value_offset
5. value_serialized_size = 4 + u32::from_le_bytes(bytes[value_offset..value_offset+4])
6. total_size = pad_alignment(value_offset + value_serialized_size, 8)

**LENGTH_PREFIX_SIZE (4) is private** in traits.rs:171 — needs to be exported or a helper function provided.

## Component Readiness Assessment

### Copier (copier.rs:150-198) — ✅ ALREADY GENERIC
```rust
let size = record.record_size as u32;  // from LiveRecord, not fixed const
// ...
dst.as_mut_slice().copy_from_slice(src.as_slice());  // raw byte copy
```
The copier works at the byte level with sizes from `LiveRecord`. **No changes needed.**

### Address Updater (address_update.rs:108-134) — ⚠️ NEEDS CHANGES
```rust
pub fn swing<K: Key>(
    &self, copy_result: &CopyResult, tombstone_addresses: &[LogicalAddress],
    key_size: usize, value_size: usize,
) -> SwingStats
```
- Line 145: Reads key at new address using fixed `RecordLayout`
- Line 190: Uses `layout.total_size()` with fixed key_size/value_size
- **Problem**: For variable-length, must compute layout per-record from the actual record bytes
- **Solution**: Need per-mapping record size info, or re-read key size from new address

### Orchestrator (orchestrator.rs:147-227) — ❌ NEEDS MAJOR CHANGES
- Lines 156-157: `let key_size = K::SIZE; let value_size = V::SIZE;` — fixed-size only
- Line 172: Passes fixed sizes to scanner
- Line 189: Passes fixed sizes to address updater
- **Solution**: Remove `FixedSizeKey`/`FixedSizeValue` bounds; use a size resolution strategy

### Scanner (scanner.rs:93-167) — ❌ NEEDS MAJOR CHANGES
- Line 105-106: Computes single layout from fixed key_size/value_size
- Line 163: Advances by fixed stride
- **Solution**: Read length prefixes per-record; compute per-record layout and stride

## FasterKv Store Structure (store/kv.rs:179)
```rust
pub struct FasterKv<F: Functions> {
    pub(crate) hash_index: HashIndex,
    pub(crate) allocator: HybridLogAllocator,
    pub(crate) functions: F,
    device: Box<dyn Device>,
    pub(crate) epoch_table: Arc<EpochTable>,
    // ... other fields
    compaction_lock: Mutex<()>,
    compaction_policy: Option<Box<dyn CompactionPolicy>>,
}
```

### Functions Trait (store/functions.rs:54)
```rust
pub trait Functions: Send + Sync + 'static {
    type Key: Key;
    type Value: Value;
    type Input: Send + Sync + Clone;
    type Output: Send + Sync + Default;
    type Context: Send + Sync;
}
```

## Key/Value Trait Hierarchy (record/traits.rs)

```
Key (line 40): serialized_size(), serialize(), deserialize()
  ├── FixedSizeKey (line 87): const SIZE: usize
  │     └── impls: u32, u64, i32, i64 (lines 107-166)
  └── Variable-length impls: Vec<u8> (line 173), String (line 221)

Value (line 73): serialized_size(), serialize(), deserialize()
  ├── FixedSizeValue (line 96): const SIZE: usize
  │     └── impls: u32, u64, i32, i64 (lines 107-166)
  └── Variable-length impls: Vec<u8> (line 196), String (line 245)
```

## Existing Tests (tests/compaction_integration.rs)

6 integration tests, all using fixed-size types (`u64`):
1. `compact_write_delete_verify` — 100K write, 50K delete, compact, verify
2. `compact_public_api_basic` — Basic API test
3. `compact_during_concurrent_writes` — Concurrent writes during compaction
4. `multiple_compactions_serialized` — Serialization of concurrent attempts
5. `maybe_compact_with_policy` — Policy-based triggering
6. `maybe_compact_disabled_by_default` — Auto-compact disabled

Variable-length test file exists: `tests/variable_length.rs` — uses `auto_compact: false`.

## Documentation Infrastructure

- Scanner has TODO comment (scanner.rs:29-33): `"Variable-length record scanning is future work."`
- Record module doc (record/mod.rs:1): `"inline variable-length key-value storage"`
- Copier test `copy_variable_length_records()` exists but only tests fixed-size records

## Design Implications

1. **New helper needed**: A function to read record size from raw page bytes by reading length prefixes. Should live in `record/layout.rs` or a new utility.

2. **Scanner redesign**: Replace fixed-stride loop with per-record size discovery loop. The `scan()` method needs to either:
   - Accept a closure/trait for size resolution, or
   - Accept a flag indicating fixed vs variable-length mode, or
   - Always use per-record size discovery (with optimization for fixed-size via const generics)

3. **Address updater**: Needs per-record layout information. Options:
   - Carry record sizes in `AddressMapping` (extend the struct)
   - Re-read sizes from the new addresses (records are at tail, always in memory)
   - Accept a size resolver

4. **Orchestrator**: Must work with `Key + Value` bounds instead of `FixedSizeKey + FixedSizeValue`. The `compact()` API can use `F::Key` and `F::Value` associated types.

5. **Backward compatibility**: Fixed-size types implement `Key`/`Value` (supertraits), so relaxing bounds is backward-compatible at the type level. Performance must not regress for fixed-size types.
