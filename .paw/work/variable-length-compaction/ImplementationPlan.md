# Implementation Plan: Variable-Length Record Compaction

**Branch**: feature/variable-length-compaction | **Created**: 2025-07-15 | **Status**: Revised (post-SoT review)
**Spec**: Spec.md | **Research**: CodeResearch.md | **Review**: reviews/planning/REVIEW-SYNTHESIS.md

## Strategic Overview

The existing compaction pipeline (scan → copy → swing → truncate) is well-architected and the copier already works at the byte level with per-record sizes. The key changes are:

1. **Record size resolver** — A new utility to discover record sizes from raw page bytes by reading length prefixes, with bounds validation and corruption detection
2. **Scanner upgrade** — Replace fixed-stride iteration with per-record variable-stride scanning, with per-hop layout in version chain walking, tombstone collection, and page-end safety guards
3. **Address updater upgrade** — Carry per-record sizes through the pipeline; key reading uses fixed key_offset=8 (invariant for all record types)
4. **Orchestrator generalization** — Relax `FixedSizeKey + FixedSizeValue` bounds to `Key + Value`
5. **Public API update** — Keep generic params with relaxed bounds for backward compatibility

The design uses a unified scanner path. For fixed-size types, `serialized_size_from_bytes` is `#[inline] fn(_) -> usize { Self::SIZE }` — LLVM const-folds this through monomorphization. A benchmark gate (SC-009) verifies no regression.

### Key Design Decisions from SoT Review

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Separate trait vs default impl for size discovery | **Default impl on Key/Value** | Backward compat > API purity; default falls back to deserialize round-trip |
| Abort vs skip-to-next-page on corruption | **Abort compaction** | Correctness > availability; original data preserved, retry later |
| Dual-path vs unified scanner | **Unified + benchmark gate** | Maintainability; dual-path doubles maintenance for likely-zero benefit |
| Tombstone storage: Option A (Vec) vs Option B (re-read) | **Option A with caveat** | Simpler data flow; add memory warning for large tombstone counts |

## Phase 1: Record Size Resolution Infrastructure

**Goal**: Create a reusable mechanism to compute record sizes from raw page bytes, with backward-compatible trait extensions and corruption-safe validation.

### 1.1 Export LENGTH_PREFIX_SIZE (record/traits.rs)
- Make `LENGTH_PREFIX_SIZE` pub: `pub const LENGTH_PREFIX_SIZE: usize = 4;`
- Re-export from `record/mod.rs`
- **File**: `record/traits.rs:171`

### 1.2 Add `Key::serialized_size_from_bytes()` with default impl (record/traits.rs)
- Add to `Key` trait with **default implementation** (backward-compatible — [MF-4]):
  ```rust
  /// Returns the serialized size of a key from raw bytes.
  /// Default falls back to full deserialization; override for performance.
  /// INVARIANT: result must equal Self::deserialize(buf).serialized_size()
  fn serialized_size_from_bytes(buf: &[u8]) -> usize {
      Self::deserialize(buf).serialized_size()
  }
  ```
- **Validation** ([MF-3]): implementations MUST check `len ≤ buf.len() - LENGTH_PREFIX_SIZE`
- Optimized overrides for built-in types:
  - Fixed-size types: `#[inline] fn serialized_size_from_bytes(_: &[u8]) -> usize { Self::SIZE }`
  - `Vec<u8>`: read 4-byte LE prefix, validate `len ≤ buf.len() - 4`, return `4 + len`
  - `String`: same as `Vec<u8>`
- Same pattern for `Value` trait

### 1.3 Add `Key::eq_from_bytes()` zero-copy comparison ([SF-2])
- Add to `Key` trait with default implementation:
  ```rust
  /// Compare self against serialized bytes without deserialization.
  /// Default falls back to deserialize+compare; override for zero-copy.
  fn eq_from_bytes(&self, buf: &[u8]) -> bool {
      *self == Self::deserialize(buf)
  }
  ```
- Optimized overrides:
  - `Vec<u8>`: compare length prefix + raw bytes directly (zero heap allocation)
  - `String`: same
  - Fixed-size types: direct byte comparison

### 1.4 Add `record_size_from_bytes()` utility (record/layout.rs)
- New function with full bounds validation ([MF-2], [MF-3]):
  ```rust
  /// Compute record size from raw page bytes. Returns Err on corruption.
  pub fn record_size_from_bytes<K: Key, V: Value>(
      page_bytes: &[u8], offset: usize, page_size: usize
  ) -> Result<usize, CompactionError> {
      let remaining = page_size - offset;
      const MIN_READABLE: usize = RECORD_HEADER_SIZE + LENGTH_PREFIX_SIZE; // 12
      if remaining < MIN_READABLE {
          return Err(CompactionError::InsufficientPageSpace);
      }
      
      let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT); // = 8
      let key_buf = &page_bytes[offset + key_offset..];
      let key_size = K::serialized_size_from_bytes(key_buf);
      let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);
      
      // Validate key_size doesn't exceed remaining page space
      if value_offset > remaining {
          return Err(CompactionError::CorruptedRecord { offset });
      }
      
      let value_buf = &page_bytes[offset + value_offset..];
      let value_size = V::serialized_size_from_bytes(value_buf);
      let total = pad_alignment(value_offset + value_size, RECORD_ALIGNMENT);
      
      // Validate total record fits on page
      if total > remaining {
          return Err(CompactionError::CorruptedRecord { offset });
      }
      
      Ok(total)
  }
  ```
- Add `debug_assert!(offset + record_size <= page_size)` after every call ([SF-5])
- Validate `record_size` fits in `u32` (audit all casts)

### Success Criteria
- `record_size_from_bytes::<Vec<u8>, Vec<u8>>()` correctly computes sizes for records with varying key/value lengths
- `record_size_from_bytes::<u64, u64>()` returns the same value as `RecordLayout::compute(8, 8).total_size()`
- Edge cases: zero-length values, maximum-length values, page boundary proximity
- **Corrupted prefix returns Err**, not panic (inject `u32::MAX` prefix → Err)
- **Default trait impls compile** without changes to downstream `Key`/`Value` implementations
- **`eq_from_bytes` for Vec<u8>** produces zero heap allocations (verify via test)

## Phase 2: Scanner Variable-Stride Support

**Goal**: Extend the scanner to iterate variable-length records with per-record size discovery, per-hop version chain layout, tombstone collection, and corruption safety.

### 2.1 Update scanner.scan() signature (compaction/scanner.rs)
- Change from `scan<K: Key>(..., key_size: usize, value_size: usize)` to `scan<K: Key, V: Value>(...)`
- Remove `key_size` and `value_size` parameters — sizes are now discovered per-record

### 2.2 Implement variable-stride scanning loop with safety guards
- Replace fixed-stride loop with ([MF-2], [MF-3]):
  ```rust
  while current < until_address {
      // Access page bytes via existing RecordAccessor pattern ([C-7])
      let page_remaining = page_size as usize - offset;
      
      // MIN_READABLE guard — chicken-and-egg resolution ([MF-2])
      const MIN_READABLE: usize = RECORD_HEADER_SIZE + LENGTH_PREFIX_SIZE; // 12
      if page_remaining < MIN_READABLE {
          current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
          continue;
      }
      
      // Per-record size discovery with corruption detection ([MF-3])
      let record_size = match record_size_from_bytes::<K, V>(page_bytes, offset, page_size) {
          Ok(size) => size,
          Err(CompactionError::CorruptedRecord { .. }) => {
              // ABORT compaction — corruption detected ([T-2])
              return Err(CompactionError::CorruptedRecord { offset });
          }
          Err(CompactionError::InsufficientPageSpace) => {
              current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
              continue;
          }
          Err(e) => return Err(e),
      };
      
      // Universal page boundary check — no offset > 0 guard ([MF-2])
      if record_size > page_remaining {
          current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
          continue;
      }
      
      debug_assert!(offset + record_size <= page_size as usize);  // [SF-5]
      
      // classify and collect (same logic as before, but per-record)
      // ...
      
      // Collect tombstones during scan ([MF-5])
      if info.is_tombstone() {
          plan.tombstone_count += 1;
          plan.tombstone_records.push(LiveRecord {
              address: current,
              record_size,
          });
      }
      
      // Collect live records
      plan.live_records.push(LiveRecord {
          address: current,
          record_size,  // now varies per record
      });
      
      advance_by(&mut current, record_size as u32, page_size);
  }
  ```
- Log `tombstone_records` memory usage; warn if > 100MB ([C-3])

### 2.3 Per-hop layout in is_current_version ([MF-1])
- **Critical redesign**: Version chain walking must compute layout at each hop address
- `key_offset` is always 8 (RECORD_HEADER_SIZE padded to 8) — this is chain-invariant
- For each chain hop:
  1. Read `RecordInfo` header (8 bytes) at hop address — get `previous_address`
  2. Use `RecordAccessor::from_log(allocator, addr, page_remaining)` to get enough bytes
  3. Use `Key::eq_from_bytes(&key, &accessor_bytes[key_offset..])` for zero-copy comparison ([SF-2])
  4. No need for full `RecordLayout` — only key comparison is performed during chain walk

- New `read_header_and_match_key_varlen` function:
  ```rust
  fn read_header_and_match_key_varlen<K: Key>(
      &self, addr: LogicalAddress, key: &K
  ) -> Option<(RecordInfo, bool)> {
      // Read with page-remaining size to get full key region
      let accessor = RecordAccessor::from_log(self.allocator, addr, page_remaining)?;
      let header = accessor.record_info();
      const KEY_OFFSET: usize = 8;  // invariant: pad_alignment(RECORD_HEADER_SIZE, 8) = 8
      let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
      Some((header, key_matches))
  }
  ```

### 2.4 Add software prefetch hints ([C-1])
- After computing current record's size, prefetch next record's header region:
  ```rust
  #[cfg(target_arch = "x86_64")]
  unsafe {
      core::arch::x86_64::_mm_prefetch(
          page_bytes.as_ptr().add(offset + record_size) as *const i8,
          core::arch::x86_64::_MM_HINT_T0
      );
  }
  ```
- Gate behind `#[cfg(target_arch = "x86_64")]` — no-op on other architectures

### 2.5 Update scanner tests
- Add tests with `Vec<u8>` keys and values of varying sizes
- Add page-boundary edge case tests for variable-length records
- Add hash-collision test: two keys of different sizes in same bucket, verify correct classification ([MF-1])
- Add page-end gap tests: 1, 4, 8, 11 bytes remaining ([MF-2])
- Add corruption test: inject `u32::MAX` prefix, verify Err returned ([MF-3])
- Preserve all existing fixed-size tests (backward compatibility)
- Add `debug_assert!(step <= page_size)` to `advance()` function

### Success Criteria
- Scanner correctly discovers sizes for records with varying key/value lengths
- Page boundary gaps handled correctly (skip to next page when record doesn't fit)
- **Hash-collision chains**: two keys of different sizes → correct liveness classification
- **Page-end safety**: scanner never panics on partial page-end data
- **Corruption safety**: corrupted prefix → Err, not panic
- **Tombstone collection**: plan.tombstone_records populated during scan
- All existing scanner tests pass unchanged
- New variable-length tests cover: uniform sizes, mixed sizes, zero-length, page boundary, corruption

## Phase 3: Address Updater Variable-Length Support

**Goal**: Enable the address updater to work with variable-length records. Key insight: `key_offset` is always 8 (invariant), so key reading works regardless of value layout ([SF-1]).

**Dependency**: Phase 3 depends on Phase 2 (tombstone records collected during scan) ([MF-5]).

### 3.1 Extend AddressMapping with record_size (compaction/copier.rs)
- Add `record_size: usize` to `AddressMapping`:
  ```rust
  pub struct AddressMapping {
      pub old_address: LogicalAddress,
      pub new_address: LogicalAddress,
      pub record_size: usize,  // NEW
  }
  ```
- Copier already has `record.record_size` — just propagate it

### 3.2 Update swing() signature (compaction/address_update.rs)
- Change from `swing<K: Key>(..., key_size: usize, value_size: usize)` to `swing<K: Key, V: Value>(...)`
- Remove fixed `key_size`/`value_size` parameters
- For key reading in `swing_one()`: use `key_offset=8` (constant) and `Key::eq_from_bytes` for matching
- **key_offset is always RECORD_HEADER_SIZE (8)** — this is invariant for all record types ([SF-1])
- If full RecordLayout is ever needed (e.g., for value reading), re-read key length prefix at the address

### 3.3 Per-record layout in swing_one() and remove_tombstone()
- For `swing_one()`: read key at new address using constant key_offset=8 + Key::eq_from_bytes
- For `remove_tombstone()`:
  - Read tombstone key at old address (old addresses remain readable under epoch guard during K1-K3)
  - Use `plan.tombstone_records` populated by scanner in Phase 2 ([MF-5])
  - key_offset=8 is sufficient for key reading; value_offset ambiguity doesn't matter
- Add `const_assert!(std::mem::size_of::<RecordInfo>() == 8)` in copier ([C-5])

### 3.4 Update address updater tests
- Add variable-length swing tests with records of different key/value sizes
- Add tombstone removal tests with variable-length tombstoned records
- Test: 3+ different key sizes in same compaction cycle

### Success Criteria
- Pointer swings work correctly for variable-length records of different sizes
- Tombstone removal works for variable-length tombstoned records using scanner-collected data
- All existing address updater tests pass unchanged

## Phase 4: Orchestrator and Public API Generalization

**Goal**: Remove fixed-size trait bounds; maintain backward compatibility with existing callers.

### 4.1 Generalize orchestrator.run() (compaction/orchestrator.rs)
- Change signature from `run<K: FixedSizeKey, V: FixedSizeValue>` to `run<K: Key, V: Value>`
- Remove `let key_size = K::SIZE; let value_size = V::SIZE;`
- Update scanner call: `scanner.scan::<K, V>(begin, until)`
- Update address updater call: `updater.swing::<K, V>(&copy_result, &plan.tombstone_records)`

### 4.2 Update FasterKv::compact() — keep generic params ([SF-4])
- **Keep type parameters with relaxed bounds** (turbofish backward compat):
  ```rust
  pub fn compact<K: Key, V: Value>(&self) -> Result<CompactionResult, CompactionError>
  where
      F::Key: Into<K>,  // or simply use F::Key, F::Value directly
  {
      // ... lock, compute range ...
      orch.run::<F::Key, F::Value>(begin, until)
  }
  ```
- Alternative: keep old signature `compact<K: Key, V: Value>()` where `u64` implements `Key`
  - Existing callers: `store.compact::<u64, u64>()` still compiles because u64: Key
- Verify SC-008: run all existing compaction tests WITHOUT modification before changing anything

### 4.3 Update maybe_compact() if it exists
- Propagate the same signature change to policy-triggered compaction

### 4.4 Update integration tests
- Verify existing tests compile and pass FIRST (SC-008)
- Add new integration tests:
  - Variable-length CRUD → compact → verify
  - Mixed-size records → compact → verify
  - Tombstones with variable-length → compact → verify
  - Concurrent reads during variable-length compaction
  - Hash-collision chains with different key sizes → compact → verify all data intact

### Success Criteria
- `compact()` works with both fixed-size and variable-length types
- **Existing turbofish callers compile unchanged** (SC-008) ([SF-4])
- New variable-length integration tests pass all SC-001 through SC-008

## Phase 5: Documentation, Benchmarks, and Quality

**Goal**: Update documentation, establish performance baselines, run full quality gate.

### 5.1 Update scanner module docs
- Remove "Variable-length record scanning is future work" comment (scanner.rs:29-33)
- Document the variable-stride scanning approach
- Document corruption abort behavior and recovery strategy

### 5.2 Update public API documentation
- Document that `compact()` now supports all Key/Value types
- Add examples with variable-length types in doc comments
- Document `serialized_size_from_bytes` and `eq_from_bytes` contract and override patterns

### 5.3 Performance benchmark gate ([SF-3])
- **SC-009**: Fixed-size compaction throughput does not regress by more than 1%
  - Benchmark `u64/u64` compaction before and after changes
  - Verify LLVM const-folds `serialized_size_from_bytes` for fixed-size types
- **SC-005 (revised)**: Variable-length compaction throughput within 2× for records ≥ 200 bytes; within 4× for smaller records
  - Benchmark at record sizes: [16B, 64B, 256B, 1KB, 10KB]
  - Document actual overhead curve
- Examine generated assembly for fixed-size monomorphization to verify const-folding

### 5.4 Run full quality gate
- `rust/scripts/precheckin` — fmt, clippy, doctests, nextest
- Verify zero regressions in existing 1,525+ tests
- Verify all new tests pass

### Success Criteria
- All documentation is current and accurate
- **SC-009 passes**: fixed-size compaction ≤1% regression
- **SC-005 passes**: variable-length within targets by record size
- Full quality gate passes with zero warnings
- No regression in existing test suite

## Dependency Graph (Revised)

```
Phase 1 (Size Resolution)
    └── Phase 2 (Scanner + Tombstones)
            └── Phase 3 (Address Updater)  ← depends on Phase 2 [MF-5]
                    └── Phase 4 (Orchestrator + API)
                            └── Phase 5 (Docs + Benchmarks + Quality)
```

**Phase 3 depends on Phase 2** — tombstone records must be collected during scanning before the address updater can use them. Phases 2 and 3 are NOT parallel.

## Risk Assessment (Revised)

| Risk | Phase | Mitigation | SoT Finding |
|------|-------|------------|-------------|
| Length prefix corruption causes panic | 1 | Bounds validation in `serialized_size_from_bytes`; `record_size_from_bytes` returns Result | MF-3 |
| Corrupted prefix cascades to entire page | 2 | Abort compaction on corruption; preserve original data | MF-3, T-2 |
| Version chain walk with different key sizes | 2 | Per-hop layout via `eq_from_bytes` with page-remaining accessor | MF-1 |
| Page-end gap causes panic | 2 | MIN_READABLE guard before size discovery | MF-2 |
| Breaking trait change for downstream | 1 | Default implementations on Key/Value traits | MF-4 |
| Tombstone phase ordering mismatch | 2-3 | Collect in scanner (Phase 2), use in updater (Phase 3) | MF-5 |
| Scanner performance regression for fixed-size | 2 | SC-009 benchmark gate; LLVM const-folds inline SIZE returns | SF-3, T-3 |
| Heap allocation storm for Vec<u8> keys | 2 | `eq_from_bytes` zero-copy comparison | SF-2 |
| API breaking change (turbofish removal) | 4 | Keep generic params with relaxed bounds (Key instead of FixedSizeKey) | SF-4 |
| Tombstone vector memory for high-delete workloads | 2 | Log warning if > 100MB; document Option B as alternative | C-3 |
| Hardware prefetch defeat | 2 | Software prefetch hints (x86_64) | C-1 |
| Epoch hold duration for large stores | 4 | Document expectations; consider batch-release in future | C-2 |

## Estimated Scope (Revised)

- **New code**: ~400-500 lines (size resolver with validation, scanner changes, chain walk variant, eq_from_bytes, prefetch)
- **Modified code**: ~200-250 lines (orchestrator, public API, existing methods)
- **New tests**: ~500-600 lines (variable-length scanner, corruption, hash-collision chains, tombstone, integration, benchmarks)
- **Total**: ~1100-1350 lines of changes

## SoT Review Findings Cross-Reference

All must-fix (MF-1 through MF-5) and should-fix (SF-1 through SF-5) findings from REVIEW-SYNTHESIS.md are addressed above. Trade-offs T-1 through T-3 are resolved. Consider items C-1 through C-7 are addressed inline where applicable or deferred to implementation-time decisions.
