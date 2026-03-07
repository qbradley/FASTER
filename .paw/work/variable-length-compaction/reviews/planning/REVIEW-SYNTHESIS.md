# REVIEW-SYNTHESIS.md

## Review Summary
- **Mode**: society-of-thought (debate — round 1 complete)
- **Specialists**: correctness, architecture, performance, edge-cases, assumptions
- **Perspectives**: premortem (applied to all specialists)
- **Perspective cap**: 1
- **Selection rationale**: All 5 built-in specialists selected (full roster) — variable-length compaction touches correctness-critical chain walking, public API surface, performance-sensitive scan loops, numerous boundary conditions, and multiple unstated assumptions
- **Rounds**: 1 (initial sweep)
- **Threads**: 36 raw findings → 19 clustered threads; strong convergence on top 5

## Perspective Diversity
- Perspectives applied: premortem → [correctness, architecture, performance, edge-cases, assumptions]
- Selection mode: auto
- Selection rationale: Premortem perspective is the strongest fit for a planning review of safety-critical infrastructure — forces specialists to reason backward from plausible production failures
- Perspectives skipped: baseline, red-team, retrospective (premortem alone provides sufficient evaluative framing for planning artifacts)

---

## Must-Fix Findings

### MF-1: Version Chain Walking Uses Fixed Layout for Variable-Length Records

**Severity**: CRITICAL — Data loss or panic  
**Confidence**: HIGH (unanimous — all 5 specialists)  
**Grounding**: Direct (scanner.rs:180-233, record_ops.rs:520-532)  
**Specialists**: correctness-F1, architecture-F2, performance-F2, edge-cases-F1, assumptions-F1

**Finding**: `is_current_version()` passes a single `RecordLayout` to `read_header_and_match_key()` at every hop in the version chain. For hash-collision chains where different keys share a bucket, keys have different serialized sizes → the layout's `value_offset` is wrong → key deserialization either panics (buffer too small) or silently miscompares (data loss via false dead classification).

**Evidence synthesis**: 
- Correctness demonstrated the exact byte arithmetic: `"ab"` (value_offset=16) vs. 64-byte key (value_offset=80)
- Edge-cases traced the full failure cascade: wrong layout → truncated accessor → silent mismatch → dead classification → data loss
- Performance specialist partially rebutted: for same-key chains, `value_offset` depends only on `key_size` (constant). **This rebuttal holds for same-key version chains but NOT for hash-collision chains with different keys.**
- Architecture confirmed the plan acknowledges the issue ("Phase 2.3: per-record layout") but provides no design

**Required plan revision**: Phase 2.3 must specify one of:
1. **Minimal layout approach**: Since `key_offset` is always 8 and `K::deserialize` self-delimits via length prefix, chain walking can use `RecordLayout { key_offset: 8, value_offset: 8 + key_serialized_size, total_size: remaining }` — compute `key_serialized_size` by reading the 4-byte length prefix at each hop
2. **New chain-walk variant**: `read_header_and_match_key_varlen` that reads the key length prefix at each hop address before deserializing

**Verification**: Test with two `Vec<u8>` keys of different sizes that hash-collide, verify `is_current_version` correctly classifies both.

---

### MF-2: Scanner Reads Size Prefix at Page-End Without Minimum Bytes Guard

**Severity**: CRITICAL — Panic (out-of-bounds access)  
**Confidence**: HIGH  
**Grounding**: Direct (scanner.rs:122-127 pattern, Plan Phase 2.2 pseudocode)  
**Specialists**: correctness-F2, edge-cases-F4, edge-cases-F10

**Finding**: `record_size_from_bytes` reads bytes at `offset+8..offset+12` to discover key length. If offset is near page end (< 12 bytes remaining), this panics. The existing fixed-size scanner avoids this because `record_size` is a pre-computed constant. The variable-length scanner has a chicken-and-egg: needs size to check fit, needs bytes to compute size.

**Required plan revision**: Phase 2.2 must add before `record_size_from_bytes`:
```rust
const MIN_READABLE: usize = RECORD_HEADER_SIZE + LENGTH_PREFIX_SIZE; // 12
if remaining < MIN_READABLE { skip to next page; continue; }
```
Also remove the `offset > 0` guard (edge-cases-F10): apply the fit check universally including at offset 0.

**Verification**: Test with records leaving 1, 4, 8, and 11 bytes of gap at page end.

---

### MF-3: Corrupted Length Prefix — No Validation or Recovery Strategy

**Severity**: CRITICAL — Panic and cascading misalignment  
**Confidence**: HIGH  
**Grounding**: Direct (traits.rs:186-193 deserialize, Plan Phase 1.3)  
**Specialists**: correctness-F6, architecture-F4, performance-F6, edge-cases-F2, edge-cases-F5, edge-cases-F6

**Finding**: Three related sub-issues:
1. **Panic in deserialize**: `Vec<u8>::deserialize()` indexes `buf[4..4+len]` with no bounds check — a corrupted prefix causes unconditional panic
2. **Cascade**: A corrupted prefix that passes bounds checks (plausible but wrong size) misaligns all subsequent records on the page — the "self-healing" property of fixed-stride scanning is lost
3. **Arithmetic overflow**: Length prefix is u32, record_size can exceed u32 range, existing `advance()` takes u32 step — wrapping creates infinite loops

**Required plan revision**:
- Phase 1.2: `serialized_size_from_bytes` MUST validate `len ≤ buf.len() - LENGTH_PREFIX_SIZE`
- Phase 1.3: `record_size_from_bytes` MUST validate computed size ≤ `page_size - offset`
- Phase 2.2: On invalid size detection, **abort compaction with error** (safest — preserves original data, compaction retries later). Alternative: skip to next page boundary.
- Audit all `record_size as u32` casts for truncation risk

**Verification**: Inject corrupted length prefix (e.g., `u32::MAX`) into a page, verify scanner does not panic and no data loss.

---

### MF-4: Adding Required Method to Public `Key`/`Value` Traits Is a Breaking Change

**Severity**: HIGH — Compile failure for all downstream consumers  
**Confidence**: HIGH  
**Grounding**: Direct (traits.rs:40, traits.rs:73, Plan Phase 1.2)  
**Specialists**: correctness-F5, architecture-F1, assumptions-F2

**Finding**: Adding `serialized_size_from_bytes` as a required method to the public `Key` and `Value` traits is a semver-breaking change. Downstream crates implementing these traits will fail to compile. This contradicts FR-007 ("full backward compatibility") and SC-008 ("existing tests pass without modification").

**Disagreement resolution**: Architecture rated CRITICAL, Correctness rated HIGH. Since this is a compile-time error (not data loss), HIGH is the appropriate severity — but it's still a must-fix because it violates stated compatibility requirements.

**Required plan revision**: Provide a default implementation:
```rust
fn serialized_size_from_bytes(buf: &[u8]) -> usize {
    Self::deserialize(buf).serialized_size()
}
```
This is inefficient (full deserialization) but backward-compatible. Built-in types (`Vec<u8>`, `String`, fixed-size) provide optimized overrides. Add doc comment establishing the invariant: result must equal `Self::deserialize(buf).serialized_size()`.

---

### MF-5: Tombstone Collection Missing from Scanner Phase; Dependency Graph Wrong

**Severity**: HIGH — Tombstone hash entries never cleared  
**Confidence**: HIGH  
**Grounding**: Direct (scanner.rs:145-146, orchestrator.rs:189, Plan Phase 2.2/3.3)  
**Specialists**: correctness-F3, architecture-F3, edge-cases-F7, assumptions-F6

**Finding**: Plan Phase 3.3 proposes `tombstone_records: Vec<LiveRecord>` in CompactionPlan, but the scanner (Phase 2.2) only collects live records. Tombstone addresses must be collected during scanning. Additionally, the dependency graph shows Phases 2 and 3 as parallel, but Phase 3 depends on Phase 2's tombstone data.

**Required plan revision**:
- Move tombstone address/size collection into Phase 2.2 (scanner)
- Update dependency graph: Phase 3 depends on Phase 2 (not parallel)
- Add `tombstone_records: Vec<LiveRecord>` to `CompactionPlan` in Phase 2 scope

---

## Should-Fix Findings

### SF-1: `record_size` Alone Cannot Reconstruct Full RecordLayout

**Severity**: HIGH (partially rebutted to MEDIUM)  
**Confidence**: HIGH  
**Grounding**: Direct (address_update.rs:137-180, mod.rs:48)  
**Specialists**: correctness-F4, edge-cases-F7

**Finding**: `AddressMapping` stores only `record_size`, not the full `RecordLayout`. Multiple (key_size, value_size) combinations produce the same total_size but different value_offset.

**Partial rebuttal (edge-cases)**: Since `key_offset` is always 8 and the address updater only needs to read the key (for hash lookup during swing), key reads work regardless of `value_offset`. The tombstone removal path has the same property.

**Required plan revision**: Document explicitly in Phase 3 that `key_offset` is always `RECORD_HEADER_SIZE` (8) and key reading works with any layout. If value reading is ever needed, store full layout or re-derive from bytes.

---

### SF-2: Heap Allocation Storm in Scanner Hot Path

**Severity**: HIGH  
**Confidence**: HIGH  
**Grounding**: Direct (traits.rs:192, scanner.rs:151, record_ops.rs:530)  
**Specialists**: performance-F1

**Finding**: Every `Vec<u8>` key deserialization allocates on the heap. For 50M records with average chain depth 1.5: ~75M allocations in the scan phase alone. At 20-60ns per allocation: 1.5-4.5 seconds of pure allocator overhead.

**Required plan revision**: Add to Phase 1 or 2: `Key::eq_from_bytes(&self, buf: &[u8]) -> bool` — zero-copy key comparison that avoids deserialization. For `Vec<u8>`: compare length prefix + bytes directly. Provide default implementation: `Self::deserialize(buf) == *self`.

---

### SF-3: SC-005 (2× Overhead Budget) Unrealistic for Small Records

**Severity**: MEDIUM  
**Confidence**: HIGH  
**Grounding**: Inferential (quantitative cost model, no code change yet)  
**Specialists**: performance-F4, architecture-F5, assumptions-F5

**Finding**: For small records (<200B), cumulative per-record overhead (length-prefix reads, heap allocation, layout computation, prefetch defeat) makes 2× throughput ratio unachievable. Performance specialist provided quantitative model showing ~2.6× for 24-byte records.

**Required plan revision**: 
- Qualify SC-005: "within 2× for records ≥ 200 bytes; within 4× for smaller records"
- Add SC-009: "Fixed-size compaction throughput does not regress by more than 1%"
- Add benchmark gate in Phase 5 at multiple record sizes

---

### SF-4: `compact()` API — Turbofish Backward Compatibility

**Severity**: MEDIUM  
**Confidence**: HIGH  
**Grounding**: Direct (kv.rs:1407-1433, Plan Phase 4.2)  
**Specialists**: architecture (aspects caveat), assumptions-F7

**Finding**: Removing `<K, V>` type params from `compact()` means `store.compact::<u64, u64>()` won't compile. The plan's risk table incorrectly claims "turbofish still compiles."

**Required plan revision**: Keep generic params with relaxed bounds: `pub fn compact<K: Key, V: Value>(&self) -> ...` where `u64` implements `Key`. Or provide deprecated wrapper. Verify SC-008 by running existing tests unmodified.

---

### SF-5: Records-Don't-Span-Pages Needs Runtime Verification

**Severity**: MEDIUM  
**Confidence**: MEDIUM  
**Grounding**: Inferential (assumptions-F4)  
**Specialists**: assumptions-F4

**Finding**: The "records don't span page boundaries" invariant is load-bearing for the scanner but only stated in documentation, not verified at runtime.

**Required plan revision**: Add `debug_assert!(offset + record_size <= page_size)` in the scanner after computing record_size. Add a defensive check: if `record_size > page_size - offset`, skip to next page.

---

## Consider

### C-1: Software Prefetch for Variable-Stride Scanning
**Specialists**: performance-F3 | **Severity**: HIGH | **Confidence**: HIGH  
Data-dependent stride defeats hardware prefetcher. Add `_mm_prefetch` of next record's header after computing current stride. Implement during Phase 2 scanner work.

### C-2: Epoch Hold Duration for Large Variable-Length Stores
**Specialists**: correctness-F7 | **Severity**: MEDIUM | **Confidence**: MEDIUM  
Variable-stride scanning may take significantly longer, holding the epoch guard. Document expected duration or add batch-release. Address during Phase 4 orchestrator work.

### C-3: Tombstone Vector Memory Scaling
**Specialists**: performance-F5 | **Severity**: MEDIUM | **Confidence**: MEDIUM  
`tombstone_records: Vec<LiveRecord>` scales linearly. Prefer Option B (re-read from old addresses during K3) if memory is constrained. Decide during Phase 2 implementation.

### C-4: Zero-Length Key Behavior
**Specialists**: edge-cases-F3 | **Severity**: MEDIUM | **Confidence**: MEDIUM  
Zero-length `Vec<u8>` keys are valid by the type system. All empty keys hash identically → long chains → compaction inefficiency. Clarify in spec whether supported; add test.

### C-5: Copier RecordInfo Size Assertion
**Specialists**: assumptions-F3 | **Severity**: LOW | **Confidence**: MEDIUM  
Add `const_assert!(std::mem::size_of::<RecordInfo>() == 8)` in copier to catch future breakage.

### C-6: Interrupted Compaction Space Amplification
**Specialists**: edge-cases-F8 | **Severity**: LOW | **Confidence**: MEDIUM  
Variable-length records amplify space impact of crash between K2 (copy) and K3 (swing). Acknowledge in documentation.

### C-7: Page Access Pattern — Use Existing RecordAccessor
**Specialists**: architecture-F6 | **Severity**: MEDIUM | **Confidence**: MEDIUM  
Plan pseudocode introduces `get_page_bytes()` which doesn't exist. Use `RecordAccessor::from_log(allocator, addr, page_remaining)` to stay within existing safety patterns. Specify in Phase 2.2.

---

## Trade-offs Requiring Decision

### T-1: Separate Trait vs Default Implementation for Size Discovery

**Axis**: API cleanliness vs backward compatibility  
**Positions**:
- Architecture: Use a separate `VariableLengthKey` trait (cleaner separation, no default impl needed)
- Correctness/Assumptions: Provide default impl on existing `Key` trait (simpler, fewer traits to implement)

**Resolution** (conservative default): Provide default implementation on `Key`/`Value` traits — backward compatibility takes priority over API purity (Priority: Correctness > Maintainability). The default falls back to `Self::deserialize(buf).serialized_size()`. Built-in types provide optimized overrides. This avoids both semver breakage AND trait proliferation.

### T-2: Abort vs Skip-to-Next-Page on Corruption

**Axis**: Safety vs availability  
**Positions**:
- Correctness: Abort compaction entirely (safest — preserves all original data)
- Architecture/Performance: Skip to next page (continues compaction, loses at most one page of records)

**Resolution** (conservative default): **Abort on corruption** (Priority: Correctness > Reliability). Original data is preserved; compaction retries later. Records in the compaction region remain readable at their original addresses. Add a configurable policy for operators who prefer skip-to-next-page in availability-critical deployments.

### T-3: Dual-Path Scanner vs Unified Scanner with Compiler Optimization

**Axis**: Architectural guarantee vs code simplicity  
**Positions**:
- Architecture/Assumptions: Keep dual-path (fixed-stride for FixedSizeKey, variable-stride otherwise) to architecturally guarantee zero overhead
- Performance: Trust LLVM const-folding for monomorphized fixed-size types, but add benchmark gate

**Resolution** (conservative default): **Unified scanner with benchmark gate** (Priority: Maintainability > Performance micro-optimization). A dual-path scanner doubles maintenance surface for a likely-zero benefit (LLVM reliably const-folds `#[inline] fn() -> usize { 8 }` through monomorphization). Add SC-009 benchmark gate to catch regression. If regression detected, add dual-path as a targeted fix.

---

## Observations

These findings are contextual (beyond the immediate plan) and retained as reference:

1. **Concurrent read safety during copy phase** (edge-cases-F9, LOW): K3 ordering appears to prevent readers from discovering in-progress copies. Verify in integration testing.
2. **`serialized_size_from_bytes` assumes universal 4-byte LE prefix** (assumptions-F2, MEDIUM): The method is per-type (each implementor defines their own format). The concern about universal format is mitigated by making it a trait method. Document the contract clearly.
3. **Tombstone epoch protection during K3** (assumptions-F6, MEDIUM): Old addresses remain readable under epoch guard during K1-K3. Same guarantee as existing fixed-size compaction. Verify in integration test.

---

## Dissent Log

### D-1: Version Chain Layout — Actual Severity
- **Correctness, Architecture, Edge-Cases**: CRITICAL — hash-collision chains with different key sizes guarantee panic or data loss
- **Performance**: Partially rebutted to LOW — same-key chains are safe because `value_offset` depends only on key_size
- **Assumptions**: HIGH — agrees with correctness but notes the `key_offset=8` constant mitigates some scenarios
- **Resolution**: CRITICAL is correct for the general case (hash collisions are inevitable). Performance's rebuttal is valid but only covers same-key chains, not the full threat model. **Classified as CRITICAL.**

### D-2: Breaking Trait Change — Severity Classification
- **Architecture**: CRITICAL
- **Correctness**: HIGH
- **Assumptions**: MEDIUM
- **Resolution**: HIGH — this is a compile-time error (not data loss or runtime failure). It's serious for library consumers but doesn't affect correctness of running systems. **Classified as HIGH (must-fix).**

### D-3: Corrupted Length Prefix — Cascade vs Panic
- **Edge-Cases**: CRITICAL (immediate panic in deserialize)
- **Architecture**: HIGH (cascading misalignment)
- **Performance**: HIGH (loss of self-healing)
- **Correctness**: MEDIUM (recovery strategy missing)
- **Resolution**: The immediate panic (edge-cases) is higher severity than the cascade (which requires the prefix to pass bounds checks). Both aspects must be addressed. **Classified as CRITICAL (encompassing both panic and cascade).**

---

## Debate Trace

### Round 1 Summary

All 5 specialists completed initial sweep with premortem perspective. 36 total findings produced.

**Strong convergence** (≥4 specialists agree):
- Version chain layout (5/5 flagged, unanimous CRITICAL after synthesis)
- Corrupted length prefix handling (4/5, CRITICAL)
- Breaking trait change (3/5, HIGH)
- Tombstone phase ordering (3/5, HIGH)

**Specialist-unique findings** (valuable, no disagreement):
- Heap allocation storm (performance only) — HIGH, actionable
- Hardware prefetch defeat (performance only) — HIGH, implementation-time
- Zero-length key degenerate hash (edge-cases only) — MEDIUM, spec clarification
- u32 truncation in record_size (edge-cases only) — MEDIUM, validation
- advance() wrapping (edge-cases only) — MEDIUM, assertion

**No threads contested** — all findings either converged or were additive. The performance specialist's self-rebuttal on version chain layout (Finding 2) demonstrates healthy specialist deliberation, but it only narrowed the scope (same-key chains safe), not the overall finding (hash-collision chains unsafe).

**Recommendation**: No Round 2 needed. Findings are clear, convergent, and actionable. Proceed to plan revision.

---

## Synthesis Trace

| Finding | Source Specialists | Grounding | Conflict Resolution |
|---------|-------------------|-----------|-------------------|
| MF-1 Version chain layout | correctness-F1, architecture-F2, performance-F2, edge-cases-F1, assumptions-F1 | Direct | Performance rebuttal accepted for same-key chains; overall CRITICAL maintained for hash-collision chains |
| MF-2 Page-end bounds | correctness-F2, edge-cases-F4, edge-cases-F10 | Direct | Merged: MIN_READABLE guard + universal offset check |
| MF-3 Corruption validation | correctness-F6, architecture-F4, performance-F6, edge-cases-F2/F5/F6 | Direct | Merged panic + cascade + overflow into single finding |
| MF-4 Breaking trait change | correctness-F5, architecture-F1, assumptions-F2 | Direct | Severity disagreement: CRITICAL→HIGH (compile error, not data loss) |
| MF-5 Tombstone phase ordering | correctness-F3, architecture-F3, edge-cases-F7, assumptions-F6 | Direct | Unanimous |
| SF-1 RecordLayout | correctness-F4, edge-cases-F7 | Direct | Partial rebuttal accepted: key reads work at offset 8 |
| SF-2 Heap allocations | performance-F1 | Direct | No disagreement |
| SF-3 SC-005 qualification | performance-F4, architecture-F5, assumptions-F5 | Inferential | Merged quantitative model + const-fold concern |
| SF-4 Turbofish compat | architecture-caveat, assumptions-F7 | Direct | No disagreement |
| SF-5 Page-span verification | assumptions-F4 | Inferential | No disagreement |
| C-1 through C-7 | Various | Mixed | No disagreement |
| T-1 through T-3 | Various | Inferential | Resolved via priority hierarchy |
