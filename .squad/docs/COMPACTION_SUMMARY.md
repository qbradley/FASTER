# FASTER Rust Codebase - Complete Compaction File Inventory

## File Listing

### Compaction Core Module (7 files)

| File | Lines | Purpose |
|------|-------|---------|
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/mod.rs` | 106 | Module documentation & public types (LiveRecord, CompactionPlan) |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/scanner.rs` | 1,202 | **K1 Phase**: Log page traversal & record classification |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/copier.rs` | 564 | **K2 Phase**: Copy live records to log tail |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/address_update.rs` | 833 | **K3 Phase**: Hash index pointer swing (CAS operations) |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/begin_address.rs` | 366 | **K4 Phase**: Begin-address advance & device truncation |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/policy.rs` | 649 | Compaction triggers & policies |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/compaction/orchestrator.rs` | 498 | **Orchestration**: Wires K1-K4 into single cycle |

### Supporting Files (2 files)

| File | Lines | Purpose |
|------|-------|---------|
| `/home/azureuser/FASTER/rust/crates/faster-core/src/hybrid_log/scan.rs` | 780 | Log iterator (contains 1 unsafe block) |
| `/home/azureuser/FASTER/rust/crates/faster-core/src/store/kv.rs` | 3,111 | Store integration: maintenance() & maybe_compact() |

### Example Files (1 file)

| File | Purpose |
|------|---------|
| `/home/azureuser/FASTER/rust/crates/faster-core/examples/stress_disk.rs` | Stress test example with LogSizeBudgetPolicy |

---

## Unsafe Blocks in Compaction Path

### ✓ Single Unsafe Block Found

**Location:** `/home/azureuser/FASTER/rust/crates/faster-core/src/hybrid_log/scan.rs:243`

```rust
let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, record_size) };
```

**Safety Justification (lines 237-242):**
```
SAFETY: `get_physical_address` returned a non-null pointer into an allocated
page frame. The offset within the page was validated by `align_to_record_boundary`
to have at least `record_size` bytes remaining. The pointer is aligned to
RECORD_ALIGNMENT (8 bytes) because all record offsets are multiples of 8 and
pages start at sector-aligned addresses.
```

**Context:**
- Inside LogScanIterator::get() method
- Creates slice from raw page data for record access
- Protected by epoch guards ensuring pages stay in memory
- Pointer and size validated before the unsafe block

### ✗ No Unsafe Blocks in Compaction Files

```
✓ compaction/mod.rs                  - No unsafe
✓ compaction/scanner.rs              - No unsafe (notes prefetch would need it)
✓ compaction/orchestrator.rs         - No unsafe
✓ compaction/copier.rs               - No unsafe
✓ compaction/address_update.rs       - No unsafe
✓ compaction/begin_address.rs        - No unsafe (explicitly notes no unsafe)
✓ compaction/policy.rs               - No unsafe
✓ store/kv.rs                        - No unsafe
```

**Note:** All compaction files use `session.begin_unsafe()` from epoch guards
instead of raw unsafe blocks for memory safety.

---

## Key Functions

### 1. `maintenance()` - store/kv.rs:1676-1776

Called from various allocation failure points and as a background maintenance task.

```rust
pub fn maintenance(&self) {
    // 1. Shift read-only boundary if mutable region is too large
    // 2. Flush sealed pages to the device
    // 2b. Poll device for completed I/O (callbacks → page state transitions)
    // 3. Evict pages if buffer pressure detected
    // 4. Yield to I/O threads if back-pressure detected
    // 5. Auto-compaction: Check policy and compact if warranted
    if self.config.auto_compact {
        self.maybe_compact();  // ← Called at line 1774
    }
}
```

### 2. `maybe_compact()` - store/kv.rs:1841-1854

Entry point for automatic compaction. Checks config and policy before triggering.

```rust
pub fn maybe_compact(&self) -> Option<Result<CompactionResult, CompactionError>> {
    if !self.config.auto_compact {
        return None;
    }
    
    let policy = self.compaction_policy.as_ref()?;
    let stats = collect_stats(&self.allocator);
    
    if !policy.should_compact(&stats) {
        return None;
    }
    
    Some(self.compact())
}
```

**Returns:**
- `None` if auto_compact disabled or policy says no
- `Some(Ok(result))` if compaction succeeds
- `Some(Err(error))` if compaction fails

### 3. `CompactionScanner::scan()` - compaction/scanner.rs:135-300+

**The main log page traversal function that performs record classification.**

```rust
pub fn scan<K: Key, V: Value>(
    &self,
    begin_address: LogicalAddress,
    until_address: LogicalAddress,
) -> Result<CompactionPlan, RecordSizeError>
```

**What it does:**
- Walks region of hybrid log from begin_address to until_address
- Classifies each record as LIVE, DEAD, or TOMBSTONED
- Uses hash index lookups to determine record status
- Returns CompactionPlan with:
  - `live_records: Vec<LiveRecord>` - addresses to keep
  - `tombstone_records: Vec<LiveRecord>` - tombstoned records
  - Dead/tombstone counts and bytes

**Key Features:**
- **MF-2 (line ~165):** Universal page boundary check
- **Variable-stride scanning:** Discovers per-record sizes via `record_size_from_bytes`
- **MF-3 (line ~194):** Corruption abort on invalid length prefixes (T-2 decision)
- **C-1 (lines 251-254):** Software prefetch note - omitted due to `#[deny(unsafe_code)]`
- **Variable-length support:** Handles both fixed and variable-length records without zero-copy overhead

---

## Compaction Pipeline (4 Phases + Orchestration)

```
Maintenance Loop
        ↓
  maybe_compact()
        ↓
   ┌────────────────────────────────────────────────────┐
   │  CompactionOrchestrator (under epoch guard K1-K3)  │
   ├────────────────────────────────────────────────────┤
   │                                                    │
   │  K1: SCAN (scanner.rs)                            │
   │  ├─ Walk contiguous address range                 │
   │  ├─ Classify records as live/dead/tombstoned      │
   │  ├─ Consult hash index for version chain          │
   │  └─ → CompactionPlan                              │
   │                                                    │
   │  K2: COPY (copier.rs)                             │
   │  ├─ Read live records via RecordAccessor          │
   │  ├─ Allocate space at log tail                    │
   │  ├─ Copy raw record bytes to new location         │
   │  ├─ Reset version-chain pointer to INVALID        │
   │  └─ → CopyResult (old→new address mappings)       │
   │                                                    │
   │  K3: POINTER SWING (address_update.rs)            │
   │  ├─ Iterate CopyResult address mappings           │
   │  ├─ CAS hash index entries from old→new           │
   │  ├─ Remove tombstoned records from hash index     │
   │  └─ Atomic, lock-free operations                  │
   │                                                    │
   │  [Bump epoch, wait for drain]                     │
   │                                                    │
   │  K4: BEGIN-ADDRESS ADVANCE (begin_address.rs)     │
   │  ├─ Advance log begin_address past region         │
   │  ├─ Truncate device below new begin_address       │
   │  └─ Free on-disk storage                          │
   │                                                    │
   └────────────────────────────────────────────────────┘
        ↓
   Return CompactionResult
```

---

## Safety Invariants

### Critical Invariant (compaction/mod.rs:19-25)

```
"A compaction scanner MUST NEVER classify a live record as dead.
 False positives (retaining dead records) waste space but are safe.
 False negatives (dropping live records) lose data. When uncertain
 (e.g., concurrent modifications, out-of-memory chain), the scanner
 conservatively classifies records as live."
```

### Record Classification Rules (scanner.rs:7-22)

For each non-null record at address A with key K:

1. **TOMBSTONED** — record's tombstone flag is set
2. **DEAD (INVALID)** — record's invalid flag is set (superseded in mutable region)
3. **LIVE / DEAD (HASH CHECK)** — walk version chain:
   - If first non-invalid, key-matching record in chain is at A → **LIVE**
   - If different address → **DEAD** (newer version exists)
   - If chain unresolvable (page not in memory, no entry) → conservatively **LIVE**

### Epoch Requirement

All compaction phases require epoch protection:
- Caller **must** hold epoch protection for scan/copy duration
- Ensures pages not evicted while scanner reads them
- Typical pattern: `session.begin_unsafe()` / `begin_unsafe()` guard
- Without epoch guard: pages could be evicted mid-scan, causing data loss

---

## Configuration Options

### FasterKvConfig Fields (store/kv.rs)

| Field | Type | Default | Purpose |
|-------|------|---------|---------|
| `auto_compact` | bool | true | Enable auto-compaction in maintenance() |
| `compaction_policy` | Option<Box<dyn CompactionPolicy>> | ManualPolicy | Policy deciding when to trigger |
| `lossy` | bool | false | Affects begin_address advancement |

### Set Compaction Policy

```rust
pub fn set_compaction_policy(&self, policy: Box<dyn CompactionPolicy>) -> Result<(), CompactionError>
```

### Built-in Policies (policy.rs)

| Policy | Trigger | Config |
|--------|---------|--------|
| **SpaceAmplificationPolicy** | When total_bytes / live_bytes > threshold | Default: 2.0x |
| **TombstonePercentPolicy** | When tombstone % > threshold | Default: 25% |
| **ManualPolicy** | Never auto-trigger | Manual `compact()` only |
| **AnyPolicy** | Triggers if ANY policy triggers | Composable OR |
| **AllPolicy** | Triggers if ALL policies trigger | Composable AND |

---

## Stress Disk Example

**File:** `/home/azureuser/FASTER/rust/crates/faster-core/examples/stress_disk.rs`

**Features:**
- Forever-runnable disk-backed stress test
- Uses SyncFileDevice on real disk (NVMe)
- Lossless mode with compaction enabled
- Periodic key deletion to keep working set ≤50% of key range
- Can run indefinitely without OOM or disk exhaustion
- Uses LogSizeBudgetPolicy for auto-compaction

**Usage:**
```bash
# Default: 16 threads, run forever, 1M key range
cargo run --release -p faster-core --example stress_disk

# Bounded 5-minute run with custom parameters
cargo run --release -p faster-core --example stress_disk -- \
  --threads 16 --duration-secs 300 --key-range 1000000 \
  --max-live-keys 500000 --storage-dir /tmp/faster-stress/ \
  --report-interval 10 --log-size-mb 1024

# Keep data after exit
cargo run --release -p faster-core --example stress_disk -- --keep-data
```

---

## Summary Statistics

- **Total files:** 10
- **Total lines:** ~8,109 lines in core compaction files
- **Unsafe blocks in compaction path:** 1 (in scan.rs, well-justified)
- **Compaction phases:** 4 (K1-K4)
- **Safety invariants:** Conservative: never drop live records

