# FASTER Cross-Implementation Analysis
**Author:** Frodo (Reverse Engineer)  
**Date:** 2026-03-05  
**Purpose:** Behavioral specification for Rust implementation of FASTER

---

## Executive Summary

This analysis compares the C++ (cc/) and C# (cs/) implementations of Microsoft FASTER to extract the precise behavioral contracts required for a Rust implementation. FASTER is a hybrid log-structured concurrent key-value store with latch-free concurrency, epoch-based protection, and Concurrent Prefix Recovery (CPR) checkpointing.

**Key Findings:**
- Core algorithms are **highly consistent** between C++ and C# implementations
- C# has **additional features** (FasterLog, remote server) not in C++
- C++ has **F2 two-tier storage** and cold indexing not in C#
- Rust MVP should target **C++ feature set** (core KV + checkpointing)
- Full Rust should add **C# FasterLog** and selective F2 features

---

## 1. FASTER Core Concepts

### 1.1 Hybrid Log — The Key Innovation

**What It Is:**
The hybrid log is a unified data structure spanning DRAM and disk as a circular buffer. It eliminates the traditional separation between in-memory cache and on-disk storage, treating them as a continuum.

**Memory Layout:**
```
┌──────────────────────────────────────────────────────┐
│                  HYBRID LOG                          │
├──────────────────────────────────────────────────────┤
│ [DISK] ←────────────────→ [MEMORY]                  │
│                                                      │
│ BeginAddress   HeadAddress   ReadOnlyAddress  TailAddress
│      ↓              ↓              ↓              ↓
│  [Disk Only]  [Immutable]    [Mutable (90%)]  [TailPg]
│                                                      │
│  Flushed to     Read-only     In-place updates      │
│  storage        region        allowed here          │
└──────────────────────────────────────────────────────┘
```

**Key Parameters:**
- **PageSizeBits (P)**: Page size = 2^P bytes (default 25 = 32MB)
- **MemorySizeBits (M)**: Total log memory = 2^M bytes
- **MutableFraction (F)**: Fraction for in-place updates (default 0.9)
- **SegmentSizeBits (S)**: Disk segment size = 2^S bytes (default 30 = 1GB)

**Why It Matters:**
- **Single API**: Operations work uniformly regardless of data location
- **Automatic Tier Management**: Data ages naturally from mutable → read-only → disk
- **Zero Copy Reads**: Disk records read directly into log structure (no separate buffer)
- **Incremental Flush**: Only read-only pages flush; mutable region stays hot

---

### 1.2 Epoch-Based Synchronization

**Concept:**
FASTER uses epochs instead of locks for synchronization. Threads register their current epoch, and critical sections (checkpointing, GC, hash table growth) wait for all threads to pass an epoch boundary.

**Mechanism:**
```
LightEpoch {
  current_epoch: atomic<uint64>
  thread_entries: [ThreadEntry; MAX_THREADS]
}

ThreadEntry {
  local_epoch: atomic<uint64>
  phase_finished: [bool; NUM_PHASES]
}
```

**Operation:**
1. Thread enters critical section → `Protect(epoch)`
2. Sets `local_epoch = current_epoch`
3. Global checkpoint → `BumpCurrentEpoch()`
4. Waits until all threads have `local_epoch > checkpoint_epoch`
5. Safe to proceed (all threads see consistent state)

**Benefits:**
- **Latch-Free**: No lock contention on hot paths
- **Scalable**: Constant overhead regardless of thread count
- **Composable**: Multiple protected regions can coexist

---

### 1.3 Latch-Free Concurrent Hash Index

**Structure:**
```
Hash Index = Array of HashBuckets (64 bytes each)

HashBucket (64 bytes = 1 cache line):
  [Entry 0..6]: 8 bytes each = 48-bit address + 14-bit tag + 2 status bits
  [Entry 7]:    Pointer to overflow bucket (or address if no overflow)

Tag Extraction:
  C#: bits [50..63] of 64-bit hash → 14-bit tag
  C++: bits [48..61] of 64-bit hash → 14-bit tag
```

**Collision Handling:**
- Keys with same bucket + tag form a **reverse linked list** in hybrid log
- RecordInfo.previous_address (48 bits) points to prior version
- Hash lookup walks chain via previous pointers

**Concurrency:**
- **Read**: No lock (atomic read of entry)
- **Insert**: CAS on hash bucket entry (retry on failure)
- **Overflow**: Allocated in pages, never freed (leak acceptable)

**Worst Case:**
- 2^14 (16,384) different tags per bucket max
- Chains can be long if hash collisions frequent
- **Mitigation**: Reduce tag bits or increase table size

---

### 1.4 Session Model & Threading

**Session Semantics:**
- Each **session is mono-threaded** (no internal concurrency)
- Multiple **sessions execute concurrently** (cross-session safety via epochs)
- No thread affinity required (but recommended for performance)

**API Pattern:**
```rust
let store = FasterKv::new(...);
let session = store.new_session(functions);

// Within session: sequential operation semantics
session.upsert(key, value);
session.read(key, ...);
session.rmw(key, input, ...);

// Across sessions: concurrent operations safe
```

**Thread Context:**
- Dual execution context buffers (prev/cur swap during checkpoints)
- Epoch protection per thread
- Pending operation tracking per session

---

## 2. Cross-Implementation Comparison

### 2.1 Subsystem Parity Matrix

| Subsystem | C++ | C# | Notes |
|-----------|-----|-----|-------|
| **Core KV Operations** | ✅ Read, Upsert, RMW, Delete | ✅ Read, Upsert, RMW, Delete | **Identical semantics** |
| **Hybrid Log** | ✅ BlittableAllocator (inline) | ✅ BlittableAllocator + GenericAllocator | C# has separate object log for classes |
| **Hash Index** | ✅ InternalHashTable | ✅ FASTER class (index component) | Both 48-bit address + 14-bit tag |
| **Checkpointing** | ✅ Snapshot + Fold-Over | ✅ Snapshot + Fold-Over + Incremental | C# adds incremental snapshots (delta log) |
| **Recovery (CPR)** | ✅ Session-based CPR | ✅ Session-based CPR with exclusions | Both support commit points |
| **Epoch Protection** | ✅ LightEpoch | ✅ LightEpoch + FastThreadLocal | Same algorithm, C# has optimized TLS |
| **Read Cache** | ✅ Separate immutable cache | ✅ Separate immutable cache | Both support read promotion |
| **Async Operations** | ✅ Callback-based | ✅ Task-based (async/await) | Different styles, same semantics |
| **Variable-Length** | ✅ Inline varlen (no object log) | ✅ SpanByte + object log | C++ single-allocation, C# dual-log |
| **Compaction** | ✅ Log compaction | ✅ Log compaction + read cache | Similar implementation |
| **F2 Two-Tier** | ✅ Hot/Cold stores + ColdIndex | ❌ Not present | **C++ exclusive** |
| **FasterLog** | ❌ Not present | ✅ Standalone append log | **C# exclusive** |
| **Remote Server** | ❌ Not present | ✅ TCP/WebSocket server | **C# exclusive** |
| **Locking** | ✅ Optional per-record locks | ✅ Optional per-record locks | Same mechanism (RecordInfo bit) |
| **Grow/Resize** | ✅ Hash table doubling | ✅ Hash table doubling | Both support runtime growth |
| **Azure Storage** | ✅ Azure Blob device | ✅ Azure Page Blob device | Both support cloud storage |

---

### 2.2 Feature Parity Summary

**Present in BOTH:**
- Core CRUD operations (Read, Upsert, RMW, Delete)
- Hybrid log with mutable/immutable regions
- Latch-free hash index with overflow buckets
- Epoch-based concurrency control
- CPR checkpointing (snapshot + fold-over)
- Session-based recovery with commit points
- Read cache for read-heavy workloads
- Log compaction
- Hash table growth (resize)
- Azure cloud storage backend
- Variable-length keys/values (different approaches)

**C# Exclusive:**
- **FasterLog**: Standalone append-only log (120KB, 12 files)
- **Remote Server**: Client-server networking with pub-sub (~80 files)
- **Incremental Snapshots**: Delta log for differential checkpoints
- **Managed Types**: Generic allocator for C# classes (GenericAllocator)
- **Async/Await**: Native .NET Task-based async model

**C++ Exclusive:**
- **F2 Two-Tier Storage**: Hot/Cold KV with separate indexes (~5 files)
- **ColdIndex**: Disk-based secondary index (2-level hash, 1 byte/key overhead)
- **Lazy Compaction**: 2-second timeout before hot→cold migration
- **Inline Varlen**: Single-allocation variable-length records (no object log)

---

### 2.3 Implementation Metrics

| Metric | C++ | C# |
|--------|-----|-----|
| **Core Headers** | 58 .h files | ~175 .cs files |
| **LOC (approx)** | ~15K core | ~30K core + 20K remote + 5K log |
| **Projects** | Single monorepo | 3 projects (core, remote, devices) |
| **Dependencies** | STL + OS threads | .NET runtime + async machinery |
| **Platforms** | Linux (QueueIoHandler), Windows (ThreadPoolIoHandler) | Cross-platform .NET |

---

## 3. Behavioral Contracts (The Spec for Rust)

### 3.1 CRUD Operations: Exact Semantics

#### **Read(key) → Status**

**Behavior:**
1. Hash `key` → bucket index + 14-bit tag
2. Lookup hash bucket for matching tag
3. Walk chain via `previous_address` pointers
4. If found:
   - **Mutable region**: Call `ConcurrentReader` (ephemeral lock)
   - **Read-only region**: Call `SingleReader` (transient lock)
   - **On disk**: Issue async I/O, return `RECORD_ON_DISK` (pending)
5. If not found or tombstone: Return `NOTFOUND`

**Return Codes:**
- `SUCCESS` (Found = 1): Record found, value read
- `NOTFOUND` (2): Key doesn't exist or tombstone
- `RECORD_ON_DISK` (Pending): Async I/O issued, call `CompletePending`
- `RETRY_LATER`: Lock contention or CPR shift, retry with epoch refresh
- `Expired`: Record expired (if expiration callback returns Expire)

**Edge Cases:**
- **Tombstone**: Treated as not found (returns NOTFOUND)
- **Expired Record**: May return NOTFOUND | Expired flag
- **CPR Version Mismatch**: Returns CPR_SHIFT_DETECTED, retry
- **IsClosed Record**: Returns RETRY_LATER

**Pending Completion:**
```rust
// Synchronous pending
let status = session.read(&key, &mut output);
if status == Status::Pending {
    session.complete_pending(true); // wait=true
    // output now contains result
}

// Async pending
let result = session.read_async(&key).await;
```

---

#### **Upsert(key, value) → Status**

**Behavior:**
1. Hash `key` → bucket index + tag
2. Lookup existing record:
   - **Found in mutable**: Call `ConcurrentWriter` → in-place update
   - **Found in read-only/disk**: Create new record at tail
   - **Not found**: Create new record at tail
3. If creating new record:
   - Call `SingleWriter` to initialize value
   - CAS new record into hash chain
   - Call `PostSingleWriter` callback
   - Seal old record if in mutable region

**Return Codes:**
- `InPlaceUpdatedRecord`: Updated existing record in-place
- `CreatedRecord | NOTFOUND`: Created new record (key didn't exist)
- `CreatedRecord`: Created new version (key existed but in read-only)
- `RETRY_LATER`: Lock contention, retry

**Critical Invariant:**
✅ **Upsert NEVER goes pending** — always completes immediately by creating at tail

**Edge Cases:**
- **Tombstone Ignored**: Creates new record, ignores tombstone
- **Old Record in Mutable**: Sealed (set invalid bit, adjust chain)
- **CPR IN_PROGRESS Phase**: V+1 thread creates V+1 record (version separation)

---

#### **RMW(key, input) → Status**

**Behavior (3-phase):**
1. Hash `key` → lookup existing record
2. **Phase 1: In-Place Update** (if mutable region):
   - Call `InPlaceUpdater(key, input, &mut value)`
   - If returns true → update in-place, return `InPlaceUpdatedRecord`
   - If returns false → proceed to phase 2/3
3. **Phase 2/3: Copy-Update or Initial-Update**:
   - If record exists (read-only or disk):
     - Call `NeedCopyUpdate` → if true, call `CopyUpdater`
     - Reads disk record if necessary (goes pending)
   - If record not found:
     - Call `NeedInitialUpdate` → if true, call `InitialUpdater`
   - CAS new record into hash chain
   - Call `PostCopyUpdater` or `PostInitialUpdater`

**Return Codes:**
- `InPlaceUpdatedRecord`: Updated in-place (mutable region)
- `CopyUpdatedRecord`: Created new version (copy-update path)
- `CreatedRecord | NOTFOUND`: Created new record (initial-update path)
- `RECORD_ON_DISK`: Async I/O pending (if record on disk)
- `RETRY_LATER`: Lock contention or CPR shift

**Critical Invariant:**
✅ **RMW can go pending** if record is on disk (must read before modify)

**Edge Cases:**
- **Expired Record**: `ReinitializeExpiredRecord` → calls `InitialUpdater`
- **CopyUpdater Refuses Expired**: Falls back to `InitialUpdater`
- **CPR Fuzzy Region**: Returns RETRY_LATER (lost-update anomaly prevention)
- **Tombstone**: Treated as not found, calls `InitialUpdater`

---

#### **Delete(key) → Status**

**Behavior:**
1. Hash `key` → lookup existing record
2. If found:
   - **Mutable region**: Call `ConcurrentDeleter` → set tombstone bit in-place
   - **Read-only/disk**: Create tombstone record at tail
3. If not found: Return `NOTFOUND` (idempotent)
4. Attempt hash chain elision (remove entry if previous is invalid)

**Return Codes:**
- `InPlaceUpdatedRecord`: Set tombstone in-place (mutable)
- `CreatedRecord`: Created tombstone record (read-only/disk)
- `NOTFOUND`: Key doesn't exist (idempotent delete)

**Critical Invariant:**
✅ **Delete NEVER goes pending** — always completes immediately by creating tombstone

**Edge Cases:**
- **Double Delete**: Second delete returns NOTFOUND (idempotent)
- **Hash Elision**: Clears hash entry if record becomes unreachable
- **CPR IN_PROGRESS**: V+1 thread creates V+1 tombstone record

---

### 3.2 Pending Operation Flow

**When Operations Go Pending:**

| Operation | Mutable | Read-Only | Disk |
|-----------|---------|-----------|------|
| **Read** | Immediate | Immediate | **PENDING** |
| **Upsert** | Immediate | Immediate | Immediate (creates at tail) |
| **RMW** | Immediate | Immediate | **PENDING** |
| **Delete** | Immediate | Immediate | Immediate (creates tombstone) |

**Pending Context Lifecycle:**
```rust
// 1. Operation returns Pending
let status = session.read(&key, &mut output);
assert_eq!(status, Status::Pending);

// 2. Context created internally:
PendingContext {
    operation_type: Read,
    key: key.clone(),
    output: &mut output,
    callback: user_callback,
    async_io_context: AsyncIOContext { ... }
}

// 3. I/O issued to disk device
device.read_async(address, page_buffer, callback);

// 4. I/O completes → enqueued in readyResponses

// 5. User calls CompletePending
session.complete_pending(wait=true);
// → Dequeues from readyResponses
// → Invokes ReadCompletionCallback with output
// → Removes from ioPendingRequests

// 6. Output now valid
assert!(output.is_some());
```

**Async Pattern (C#-style):**
```rust
// Single operation
let result = session.read_async(&key).await?;

// Batch operations
let read1 = session.read_async(&key1);
let read2 = session.read_async(&key2);
let (result1, result2) = tokio::join!(read1, read2);
```

---

### 3.3 Checkpoint Semantics

#### **Checkpoint Types**

**1. Fold-Over Checkpoint**
- **Action**: Flushes mutable log to disk, makes it read-only
- **Metadata**: Writes small `info.dat` (addresses, versions, session state)
- **Incremental**: New updates go to fresh tail (log continues)
- **Recovery**: Loads index + replays log from `flushedAddress` to `tailAddress`
- **Cost**: Lowest (no log copying), but creates multiple data versions

**2. Snapshot Checkpoint**
- **Action**: Copies in-memory log to separate snapshot file
- **Metadata**: Writes `info.dat` + snapshot file
- **Incremental**: Main log continues; snapshot isolated
- **Recovery**: Loads index + reads snapshot back into memory
- **Cost**: Higher (log copy), but single data version

**3. Incremental Snapshot (C# only)**
- **Action**: Delta log captures changes since last snapshot
- **Metadata**: Writes `info.dat` + delta file
- **Recovery**: Loads base snapshot + applies delta
- **Cost**: Medium (only diffs), efficient for update-heavy workloads

**4. Index Checkpoint**
- **Action**: Persists entire hash table to disk
- **Metadata**: Writes index metadata + hash bucket array
- **Frequency**: Coarse (every N log checkpoints)
- **Recovery**: Avoids rebuilding index from log

---

#### **Checkpoint State Machine**

**Phases (C++ & C#):**
```
REST
  ↓ [User calls TakeCheckpoint]
PREP_INDEX_CHECKPOINT
  ↓ [Initialize checkpoint tokens]
PREPARE (v version)
  ↓ [Capture tail, begin, start addresses]
IN_PROGRESS (v+1 version)
  ↓ [Version shift: threads enter v+1, create v+1 records]
WAIT_FLUSH
  ↓ [Flush pages to disk, wait I/O completion]
PERSISTENCE_CALLBACK
  ↓ [Write metadata, invoke user callbacks]
REST
```

**Key Actions:**
1. **PREPARE**: Captures `checkpoint_start_address = tailAddress`
2. **IN_PROGRESS**: Bumps version (v → v+1), threads cannot update v records
3. **WAIT_FLUSH**: Flushes pages from `HeadAddress` to `checkpoint_start_address`
4. **PERSISTENCE_CALLBACK**: Writes `info.dat` with:
   - `flushedLogicalAddress`
   - `finalLogicalAddress`
   - `startLogicalAddress`, `beginAddress`, `headAddress`
   - Session commit points (sessionID, serialNumber, exclusions)

---

#### **Guarantees**

✅ **Atomicity**: Checkpoint captures consistent snapshot at version boundary  
✅ **Durability**: All operations before commit point are durable after checkpoint  
✅ **Prefix Durability**: Session can commit subset (operations 0..N durable, N+1..M not)  
✅ **Non-Blocking**: Threads continue operating during checkpoint (v+1 records)  
✅ **Multi-Session**: Each session has independent commit point  

**Commit Point Structure:**
```rust
struct CommitPoint {
    session_id: Guid,
    session_name: String,
    serial_number: u64,         // Last committed operation
    excluded_serial_nums: Vec<u64>, // Operations that went pending, didn't complete
}
```

---

### 3.4 Recovery Semantics

#### **Recovery Process**

**Step 1: Find Checkpoint**
```rust
// Scans checkpoint tokens, finds latest valid checkpoint
let (hlog_info, index_info) = get_closest_checkpoint()?;

// Validates: index.finalLogicalAddress <= hlog.finalLogicalAddress
if index_info.final_address > hlog_info.final_address {
    return Err("Index checkpoint too new");
}
```

**Step 2: Load Index**
```rust
// Reads hash table from disk
let hash_table = read_index_checkpoint(&index_info)?;
store.overwrite_index(hash_table);
```

**Step 3: Recover Log (Fold-Over)**
```rust
// Sets addresses from checkpoint metadata
store.set_head_address(hlog_info.head_address);
store.set_readonly_address(hlog_info.flushed_address);
store.set_tail_address(hlog_info.final_address);

// Replays log to undo v+1 records (created after checkpoint started)
let recover_from = hlog_info.checkpoint_start_address;
store.recover_hybrid_log(recover_from, hlog_info.final_address);
```

**Step 4: Recover Log (Snapshot)**
```rust
// Loads snapshot file into memory
store.recover_from_snapshot(&hlog_info.snapshot_file)?;

// If incremental: applies delta log
if hlog_info.delta_tail_address != -1 {
    store.apply_delta_log(&hlog_info.delta_file)?;
}
```

**Step 5: Restore Sessions**
```rust
for session_info in hlog_info.sessions {
    let commit_point = CommitPoint {
        session_id: session_info.id,
        serial_number: session_info.serial_number,
        exclusions: session_info.excluded_serial_nums,
    };
    
    // User can resume session with same ID
    let session = store.resume_session(
        session_info.id,
        functions,
        commit_point
    )?;
    
    // Session knows last committed operation
    assert_eq!(session.serial_num(), commit_point.serial_number);
}
```

---

#### **Guarantees**

✅ **Consistency**: Recovered state matches checkpoint snapshot  
✅ **Session Continuity**: Resumed sessions know exact commit point  
✅ **Exclusion Handling**: Operations that went pending are marked as not durable  
✅ **Version Correction**: v+1 records created during checkpoint are invalidated  
✅ **Crash Recovery**: System recovers to last completed checkpoint  

**Exclusion Example:**
```
Session performs:
  Op 0: Upsert(k1, v1)  → completes
  Op 1: Read(k2)        → goes pending (on disk)
  Op 2: Upsert(k3, v3)  → completes
  Op 3: Read(k4)        → goes pending
  
Checkpoint starts at version v.
Op 1 and Op 3 complete AFTER checkpoint starts (in v+1).

Checkpoint records:
  serialNumber = 3 (last issued)
  exclusions = [1, 3] (didn't complete in v)
  
Recovery:
  Recovered commit point = 2 (Op 0, Op 2 durable)
  Op 1 and Op 3 must be retried by application
```

---

## 4. On-Disk Formats

### 4.1 RecordInfo 8-Byte Header

**Bit Layout:**
```
 63  62  61    60..48         47..0
┌───┬───┬───┬─────────────┬──────────────────────────┐
│ F │ T │ I │  Version    │   Previous Address       │
└───┴───┴───┴─────────────┴──────────────────────────┘
 │   │   │   │             │
 │   │   │   │             └─ 48 bits: chain pointer
 │   │   │   └─ 13 bits: checkpoint version (0..8191)
 │   │   └─ 1 bit: invalid (record sealed)
 │   └─ 1 bit: tombstone (logically deleted)
 └─ 1 bit: final (unused/reserved)

With read cache enabled:
 63  62  61    60..48    47      46..0
┌───┬───┬───┬─────────┬─────┬──────────────────────┐
│ F │ T │ I │ Version │ RC  │  Previous Address    │
└───┴───┴───┴─────────┴─────┴──────────────────────┘
                       │     │
                       │     └─ 47 bits: chain pointer
                       └─ 1 bit: read cache flag
```

**Implications for Rust:**
```rust
#[repr(C)]
struct RecordInfo {
    data: u64,
}

impl RecordInfo {
    const PREVIOUS_ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    const VERSION_MASK: u64 = 0x1FFF_0000_0000_0000;
    const INVALID_BIT: u64 = 0x2000_0000_0000_0000;
    const TOMBSTONE_BIT: u64 = 0x4000_0000_0000_0000;
    const FINAL_BIT: u64 = 0x8000_0000_0000_0000;
    
    fn previous_address(&self) -> u64 {
        self.data & Self::PREVIOUS_ADDRESS_MASK
    }
    
    fn version(&self) -> u16 {
        ((self.data & Self::VERSION_MASK) >> 48) as u16
    }
    
    fn is_tombstone(&self) -> bool {
        (self.data & Self::TOMBSTONE_BIT) != 0
    }
    
    fn is_invalid(&self) -> bool {
        (self.data & Self::INVALID_BIT) != 0
    }
}
```

---

### 4.2 Full Record Structure

**Layout:**
```
Offset  Size              Field
──────────────────────────────────────────────
0       8 bytes           RecordInfo header
8       align(K)          Padding to key alignment
8+pad   key.size()        Key data
K_end   align(V)          Padding to value alignment
K+pad   value.size()      Value data
V_end   align(64)         Padding to next record

Total record size = 8 + align(key) + key.size() + align(value) + value.size() + align(record)
```

**Alignment Rules:**
- Keys: Aligned to `alignof(Key)` (up to 64 bytes)
- Values: Aligned to `alignof(Value)` (up to 64 bytes)
- Records: Start at 64-byte boundaries (cache line)

**Variable-Length (C++ inline):**
```cpp
struct VarLenKey {
    uint32_t size;
    char data[];  // Flexible array member
};

Record layout:
  RecordInfo (8)
  VarLenKey { size (4), data[size] }
  VarLenValue { size (4), data[size] }
```

**Variable-Length (C# dual-log):**
```csharp
// Main log: fixed-size record
struct LogRecord {
    RecordInfo info;
    long object_pointer;  // Points to object log
}

// Object log: serialized object
byte[] serialized_key;
byte[] serialized_value;
```

---

### 4.3 Checkpoint Metadata Format

**C++ IndexMetadata (Binary, 56 bytes):**
```cpp
struct IndexMetadata {
    uint32_t version;                 // 4 bytes
    uint64_t table_size;              // 8 bytes
    uint64_t num_ht_bytes;            // 8 bytes
    uint64_t num_ofb_bytes;           // 8 bytes
    uint64_t ofb_count;               // 8 bytes
    uint64_t log_begin_address;       // 8 bytes
    uint64_t checkpoint_start_address;// 8 bytes
};  // Total: 52 bytes + padding → 56 bytes
```

**C++ LogMetadata (Binary, variable size):**
```cpp
struct LogMetadata {
    bool use_snapshot_file;               // 1 byte
    uint32_t version;                     // 4 bytes
    uint32_t num_threads;                 // 4 bytes
    uint64_t flushed_address;             // 8 bytes
    uint64_t final_address;               // 8 bytes
    uint64_t monotonic_serial_nums[MAX_THREADS]; // 8 * N bytes
    Guid guids[MAX_THREADS];              // 16 * N bytes
};
```

**C# HybridLogRecoveryInfo (Text, info.dat):**
```
CheckpointVersion: 5
Checksum: 1234567890123456
Guid: 550e8400-e29b-41d4-a716-446655440000
useSnapshotFile: 0
version: 1
nextVersion: 2
flushedLogicalAddress: 134217728
snapshotStartFlushedLogicalAddress: 134217728
startLogicalAddress: 0
finalLogicalAddress: 268435456
snapshotFinalLogicalAddress: -1
headAddress: 67108864
beginAddress: 0
deltaTailAddress: -1
manualLockingActive: False
numSessions: 2
  sessionID: a1b2c3d4-e5f6-7890-abcd-ef1234567890
  sessionName: worker-1
  serialNumber: 12345
  numExclusions: 2
    exclusionSerialNum: 100
    exclusionSerialNum: 200
  sessionID: b2c3d4e5-f678-90ab-cdef-123456789012
  sessionName: 
  serialNumber: 67890
  numExclusions: 0
objectLogSegmentOffsets: []
```

**C# IndexRecoveryInfo (Text, info.dat):**
```
CheckpointVersion: 1
Checksum: 9876543210987654
Guid: 123e4567-e89b-12d3-a456-426614174000
table_size: 1048576
num_ht_bytes: 67108864
num_ofb_bytes: 0
num_buckets: 1048576
startLogicalAddress: 0
finalLogicalAddress: 268435456
```

---

### 4.4 File Organization

**Checkpoint Directory Structure:**
```
store_base_dir/
├── index-checkpoints/
│   ├── {guid1}/
│   │   ├── info.dat                 # Index metadata
│   │   └── ht.dat                   # Hash table binary
│   └── {guid2}/
│       └── ...
├── log-checkpoints/
│   ├── {guid1}/
│   │   ├── info.dat                 # Log metadata
│   │   ├── snapshot.dat (optional)  # Snapshot file
│   │   └── delta.dat (optional)     # Incremental delta
│   └── {guid2}/
│       └── ...
└── log/
    ├── log.0                        # Main log segment 0
    ├── log.1                        # Segment 1
    └── ...
```

**File Naming:**
- **Checkpoint folders**: Named by GUID token (e.g., `550e8400-e29b-41d4-a716-446655440000`)
- **Log segments**: Sequential (log.0, log.1, ...) up to SegmentSize (default 1GB)
- **Metadata**: Always `info.dat` (text in C#, binary in C++)

---

### 4.5 Versioning & Compatibility

**Version Numbers:**
| Component | C# Version | C++ Version | Compatibility |
|-----------|------------|-------------|---------------|
| HybridLogRecoveryInfo | 5 | 1 | V4→V5 translation supported |
| IndexRecoveryInfo | 1 | 1 | Current version |
| RecordInfo | N/A (implicit) | N/A (implicit) | Version field in header (13 bits) |

**Backward Compatibility:**
- C# V4 → V5: Missing `snapshotStartFlushedLogicalAddress` defaults to `flushedLogicalAddress`
- C++ has no version migration logic (single version)
- Checksum validation prevents corrupted metadata loading

**Checksum Formula (C#):**
```csharp
long checksum = guid[0..8] ^ guid[8..16]
              ^ version ^ flushed ^ snapshot_start ^ start ^ final
              ^ snapshot_final ^ head ^ begin ^ session_count ^ offset_count;
```

---

## 5. State Machines

### 5.1 Checkpoint State Machine

**Full State Diagram:**
```
┌─────────────────────────────────────────────────────┐
│              CHECKPOINT FSM (Full)                   │
├─────────────────────────────────────────────────────┤
│                                                      │
│  REST (v)                                           │
│    ↓ [TakeFullCheckpoint() called]                  │
│  PREP_INDEX_CHECKPOINT                              │
│    ↓ [Initialize index/log checkpoint tokens]       │
│  PREPARE (v)                                        │
│    ↓ [Capture tailAddress, version v]               │
│  IN_PROGRESS (v+1)                                  │
│    ↓ [Version shift: threads enter v+1]             │
│    │ [Threads create v+1 records]                   │
│    │ [v records become immutable]                   │
│  WAIT_INDEX_CHECKPOINT (C# only)                    │
│    ↓ [Wait for index snapshot completion]           │
│  WAIT_FLUSH                                         │
│    ↓ [Flush pages to disk]                          │
│    │ [Wait for I/O completion]                      │
│  PERSISTENCE_CALLBACK                               │
│    ↓ [Write metadata files]                         │
│    │ [Invoke user callbacks]                        │
│  REST (v+1)                                         │
│    ↓ [Checkpoint complete, ready for next]          │
│                                                      │
└─────────────────────────────────────────────────────┘
```

**Phase Actions:**

| Phase | Global Actions | Per-Thread Actions |
|-------|----------------|-------------------|
| **PREP_INDEX_CHECKPOINT** | Create checkpoint tokens (GUIDs) | — |
| **PREPARE** | Capture `tailAddress`, `headAddress`, `beginAddress`, version v | — |
| **IN_PROGRESS** | Bump version (v → v+1), notify threads | Threads check version before each operation; create v+1 records if version changed |
| **WAIT_INDEX_CHECKPOINT** | — | Wait for index snapshot I/O to complete |
| **WAIT_FLUSH** | Flush pages from `headAddress` to `checkpoint_start_address` | Wait for flush completion semaphore |
| **PERSISTENCE_CALLBACK** | Write `info.dat`, invoke `CheckpointCallback` | User can trigger post-checkpoint actions |
| **REST** | Reset checkpoint state, version now v+1 | Resume normal operations |

**Version Separation Invariant:**
- **v threads**: Cannot see or modify v+1 records
- **v+1 threads**: Cannot modify v records (create new v+1 versions instead)
- **Fuzzy Region**: Between `safeReadOnlyAddress` and `readOnlyAddress`, RMW returns RETRY_LATER (lost-update prevention)

---

### 5.2 Grow (Hash Table Resize) State Machine

**State Diagram:**
```
┌───────────────────────────────────┐
│       GROW/RESIZE FSM             │
├───────────────────────────────────┤
│                                   │
│  REST                             │
│    ↓ [Grow() called]              │
│  PREPARE_GROW                     │
│    ↓ [Wait all threads]           │
│    │ [Epoch bump]                 │
│    │ [NoActiveLockingSessions]    │
│  IN_PROGRESS_GROW                 │
│    ↓ [Allocate new table (2x)]    │
│    │ [Threads copy chunks]        │
│    │ [Atomic chunk counter]       │
│  REST                             │
│    ↓ [New table active]           │
│                                   │
└───────────────────────────────────┘
```

**Mechanism:**
1. **PREPARE_GROW**: Waits for all threads to finish pending operations
2. **IN_PROGRESS_GROW**:
   - Allocates new hash table with 2x size
   - Divides old table into chunks (typically 16KB = 16,384 entries/chunk)
   - Threads atomically claim chunks via `next_chunk.fetch_add(1)`
   - Each thread re-hashes chunk entries into new table
3. **REST**: Swaps `hash_table` pointer, old table freed

**Chunk-Based Parallelism:**
```rust
struct GrowState {
    num_chunks: usize,
    next_chunk: AtomicUsize,
    num_pending_chunks: AtomicUsize,
}

// Thread execution
loop {
    let chunk_id = state.next_chunk.fetch_add(1, Ordering::AcqRel);
    if chunk_id >= state.num_chunks { break; }
    
    let start = chunk_id * ENTRIES_PER_CHUNK;
    let end = min((chunk_id + 1) * ENTRIES_PER_CHUNK, old_size);
    
    for i in start..end {
        let entry = old_table[i];
        if entry.is_valid() {
            let new_bucket = hash(entry.key) % new_size;
            new_table[new_bucket].insert(entry);
        }
    }
    
    state.num_pending_chunks.fetch_sub(1, Ordering::AcqRel);
}
```

---

### 5.3 Garbage Collection State Machine (C++)

**State Diagram:**
```
┌────────────────────────────────┐
│      GC FSM (C++)              │
├────────────────────────────────┤
│                                │
│  REST                          │
│    ↓ [ShiftBeginAddress()]     │
│  GC_IO_PENDING                 │
│    ↓ [Issue truncate callback] │
│    │ [Wait I/O completion]     │
│  GC_IN_PROGRESS                │
│    ↓ [Clean hash chains]       │
│    │ [Process chunks]          │
│  REST                          │
│    ↓ [Issue complete callback] │
│                                │
└────────────────────────────────┘
```

**GC Actions:**
1. **GC_IO_PENDING**: User truncate callback can flush old data
2. **GC_IN_PROGRESS**:
   - Scans hash table in chunks
   - For each chain, removes entries below `new_begin_address`
   - Updates chain pointers (CAS)
3. **REST**: Invokes completion callback

**GcState Structure (C++):**
```cpp
struct GcState {
    Address new_begin_address;
    truncate_callback_t truncate_callback;
    complete_callback_t complete_callback;
    IAsyncContext* callback_context;
};

struct GcStateInMemIndex : GcState {
    uint64_t num_chunks = 16384;
    atomic<uint64_t> next_chunk;
};
```

---

## 6. Divergences and Decision Points

### 6.1 Semantic Differences

| Aspect | C++ | C# | Rust Decision |
|--------|-----|-----|---------------|
| **Varlen Storage** | Inline (single allocation) | Dual-log (fixed + object) | **Inline** (more cache-friendly) |
| **Async Model** | Callback-based (IAsyncContext) | Task-based (async/await) | **Both** (callback + Future trait) |
| **Object Log** | Not present | Separate device for classes | **Not needed** (Rust has sized traits) |
| **Incremental Snapshot** | Not present | Delta log support | **Include** (useful optimization) |
| **Tag Bits** | [48..61] = 14 bits | [50..63] = 14 bits | **[48..61]** (match C++) |
| **Status Codes** | 8-bit enum + InternalStatus | 16-bit flags | **Use Rust Result + flags** |
| **Metadata Format** | Binary structs | Text serialization | **Binary** (faster, smaller) |
| **F2 Two-Tier** | Present | Absent | **Optional feature** (defer to v2) |
| **FasterLog** | Absent | Present | **Include** (standalone log valuable) |
| **Remote Server** | Absent | Present | **Defer** (not core KV) |

---

### 6.2 Critical Design Choices for Rust

**Choice 1: Variable-Length Storage**
- **Option A**: C++ inline (single allocation, key+value contiguous)
- **Option B**: C# dual-log (separate object storage)
- **Decision**: **Inline** (simpler, better cache locality, no separate GC)
- **Rationale**: Rust's `&[u8]` and `Box<[u8]>` work naturally with inline storage

**Choice 2: Async Model**
- **Option A**: Callback-only (C++ style)
- **Option B**: Future-only (pure async/await)
- **Option C**: **Both** (callbacks + Future impl)
- **Decision**: **Option C**
- **Rationale**: Support both sync (no runtime) and async (Tokio integration)

**Choice 3: Metadata Format**
- **Option A**: Binary (C++ style)
- **Option B**: Text (C# style)
- **Decision**: **Binary with version field**
- **Rationale**: Faster, smaller, easier to parse; add version for forward compat

**Choice 4: Checkpoint Types**
- **Option A**: Fold-over only (simplest)
- **Option B**: Snapshot only (cleanest)
- **Option C**: **Both + Incremental**
- **Decision**: **Option C**
- **Rationale**: Different workloads favor different modes; incremental is valuable

**Choice 5: F2 Two-Tier**
- **Option A**: Omit entirely
- **Option B**: Port from C++
- **Decision**: **Defer to v2, but design for extensibility**
- **Rationale**: Complex feature, not needed for MVP; leave hooks for future

**Choice 6: FasterLog**
- **Option A**: Omit (KV-only)
- **Option B**: Port from C#
- **Decision**: **Port (simplified)**
- **Rationale**: Append-only log is common use case, relatively self-contained

---

### 6.3 Behavioral Ambiguities (Resolved)

**Ambiguity 1**: What if RMW callback returns false in InPlaceUpdater but also in CopyUpdater?
- **Resolution**: Falls back to InitialUpdater (treat as not found)
- **Source**: C# InternalRMW.cs line 487

**Ambiguity 2**: Can Delete go pending?
- **Resolution**: **No, never pending** (always creates tombstone at tail)
- **Source**: Both C++ and C# implementations confirm this

**Ambiguity 3**: What if checkpoint starts during pending operation?
- **Resolution**: Operation completes in v+1, recorded in `exclusions` list
- **Source**: C# Recovery.cs commit point serialization

**Ambiguity 4**: How are expired records handled in read-only region?
- **Resolution**: Read doesn't copy expired records; RMW calls InitialUpdater instead of CopyUpdater
- **Source**: C# InternalRMW.cs `ReinitializeExpiredRecord`

**Ambiguity 5**: Can hash table shrink?
- **Resolution**: **No, only grows** (shrinking would invalidate addresses)
- **Source**: Both implementations only support doubling

---

## 7. Rust Implementation Roadmap

### 7.1 MVP (Minimum Viable Product)

**Core Features:**
- [x] Hybrid log with mutable/read-only regions
- [x] Latch-free hash index (48-bit address + 14-bit tag)
- [x] Epoch-based concurrency (LightEpoch)
- [x] CRUD operations (Read, Upsert, RMW, Delete)
- [x] Pending operation handling (sync + async)
- [x] Fold-over checkpoint
- [x] Snapshot checkpoint
- [x] CPR recovery with session commit points
- [x] Hash table growth (resize)
- [x] Log compaction
- [x] Local storage device (FileSystemDevice)
- [x] Null storage (in-memory only)
- [x] Optional per-record locking

**API Surface:**
```rust
// Core types
struct FasterKv<K, V> { ... }
struct Session<'a, K, V, F> { ... }

// Operations (sync)
fn read(&self, key: &K) -> Result<Option<V>, Status>;
fn upsert(&self, key: K, value: V) -> Result<(), Status>;
fn rmw(&self, key: &K, input: &Input) -> Result<(), Status>;
fn delete(&self, key: &K) -> Result<(), Status>;
fn complete_pending(&self, wait: bool) -> Result<(), Status>;

// Operations (async)
async fn read_async(&self, key: &K) -> Result<Option<V>, Status>;
async fn upsert_async(&self, key: K, value: V) -> Result<(), Status>;
async fn rmw_async(&self, key: &K, input: &Input) -> Result<(), Status>;
async fn delete_async(&self, key: &K) -> Result<(), Status>;

// Checkpointing
async fn take_full_checkpoint(&self) -> Result<Guid, Error>;
async fn take_hybrid_log_checkpoint(&self, checkpoint_type: CheckpointType) -> Result<Guid, Error>;
async fn take_index_checkpoint(&self) -> Result<Guid, Error>;
fn recover(path: &Path) -> Result<Self, Error>;
fn recover_latest(path: &Path) -> Result<Self, Error>;

// Session management
fn new_session<F: IFunctions<K, V>>(&self, functions: F) -> Session<K, V, F>;
fn resume_session<F>(&self, session_id: Guid, functions: F) -> Result<(Session<K, V, F>, CommitPoint), Error>;
```

**Deliverable:**
- Core KV store with checkpointing
- No F2, no FasterLog, no remote
- ~10K LOC estimate
- **Target: 3-4 months**

---

### 7.2 Full Feature Set

**Additional Features:**
- [x] Incremental snapshot (delta log)
- [x] Read cache (immutable secondary cache)
- [x] Azure storage device (Blob backend)
- [x] Variable-length keys/values (inline)
- [x] FasterLog (standalone append log)
- [x] Scan/iteration over log
- [x] Async iterator (Stream trait)
- [x] Per-record expiration callbacks
- [x] Manual compaction triggers
- [x] Runtime address marker shifting

**Extended API:**
```rust
// FasterLog
struct FasterLog { ... }
async fn enqueue(&self, data: &[u8]) -> Result<u64, Error>;
async fn enqueue_batch(&self, batch: &[&[u8]]) -> Result<u64, Error>;
async fn wait_for_commit(&self) -> Result<u64, Error>;
fn scan(&self, start: u64, end: u64) -> LogIterator;

// Iteration
fn scan_log(&self, start_address: u64, end_address: u64) -> LogIterator<K, V>;
async fn iter(&self) -> impl Stream<Item = (K, V)>;

// Compaction
async fn compact(&self, until_address: u64) -> Result<(), Error>;
fn shift_begin_address(&self, new_begin: u64) -> Result<(), Error>;

// Advanced
fn set_read_only_address(&self, address: u64);
fn set_head_address(&self, address: u64);
```

**Deliverable:**
- Full-featured KV + Log
- Production-grade
- ~20K LOC estimate
- **Target: 6-8 months**

---

### 7.3 Deferred Features (v2+)

**Phase 2 (Tiered Storage):**
- [ ] F2 two-tier (hot/cold stores)
- [ ] ColdIndex (disk-based secondary index)
- [ ] Lazy compaction (timeout-based migration)
- [ ] Tiered storage device abstraction

**Phase 3 (Distributed):**
- [ ] Remote server (TCP/gRPC)
- [ ] Remote client
- [ ] Pub-sub protocol
- [ ] Distributed recovery (DPR)

**Phase 4 (Advanced):**
- [ ] RDMA storage device
- [ ] 64-bit logical addresses (>48 bits)
- [ ] Page checksums (CRC-32)
- [ ] Encryption at rest
- [ ] Compression (LZ4/Zstd)

---

### 7.4 C FFI Interface

**Required for C/C++ Interop:**
```rust
// Opaque handles
pub struct FasterKvHandle;
pub struct SessionHandle;

// C API
#[no_mangle]
pub extern "C" fn faster_kv_new(
    index_size: u64,
    log_size_bits: u32,
    page_size_bits: u32,
    segment_size_bits: u32,
    path: *const c_char,
) -> *mut FasterKvHandle;

#[no_mangle]
pub extern "C" fn faster_kv_new_session(
    kv: *mut FasterKvHandle,
    functions: *const CFunctions,
) -> *mut SessionHandle;

#[no_mangle]
pub extern "C" fn session_read(
    session: *mut SessionHandle,
    key: *const u8,
    key_len: usize,
    output: *mut u8,
    output_len: usize,
) -> i32;  // Status code

#[no_mangle]
pub extern "C" fn session_upsert(
    session: *mut SessionHandle,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32;

// ... (full API surface)
```

**Deliverable:**
- C header file (`faster.h`)
- Rust FFI implementation
- Example C/C++ usage
- **Target: +2 months after MVP**

---

## 8. Key Invariants & Guarantees

### 8.1 Correctness Invariants

**Hash Index:**
✅ **Unique Tag per Bucket**: At most 2^14 different tags per bucket  
✅ **Valid Chain Pointers**: `previous_address` always points to valid record or 0  
✅ **Atomic Updates**: Hash entry CAS ensures no torn reads  
✅ **Overflow Leak**: Overflow buckets never freed (acceptable memory leak)  

**Hybrid Log:**
✅ **Region Ordering**: BeginAddress ≤ HeadAddress ≤ ReadOnlyAddress ≤ TailAddress  
✅ **Immutable Reads**: Records in read-only region never modified in-place  
✅ **Version Monotonicity**: Records at higher addresses have ≥ checkpoint version  
✅ **Page Alignment**: Records respect page boundaries (no cross-page structs)  

**Epoch Protection:**
✅ **Grace Period**: Old data not freed until all threads pass epoch boundary  
✅ **Phase Completion**: Global phase transition waits for all threads  
✅ **Entry Validity**: Thread entries always point to valid epoch or retired  

**Checkpoint:**
✅ **Version Separation**: v threads cannot modify v+1 records, v+1 threads cannot modify v records  
✅ **Prefix Consistency**: All operations ≤ commit point are durable, operations > commit point are not  
✅ **Metadata Integrity**: Checksum validates metadata on recovery  
✅ **Atomicity**: Checkpoint appears atomic (no partial state visible)  

---

### 8.2 Performance Guarantees

**Read (in-memory):**
- **Best Case**: O(1) hash lookup + O(1) record read (no chain walk)
- **Worst Case**: O(2^14) chain walk (if all keys hash to same bucket+tag)
- **Typical**: O(log N) chain length for well-distributed hashes

**Upsert:**
- **In-Place**: O(1) (mutable region, no version change)
- **Copy-on-Write**: O(log N) allocation + O(1) CAS
- **Never Blocks**: Always completes immediately (may spin on CAS)

**RMW:**
- **In-Place**: O(1) (mutable region, InPlaceUpdater succeeds)
- **Copy-Update**: O(log N) allocation + O(1) CAS
- **Disk Read**: O(device latency) (typically 5-10ms SSD, 100-200µs NVMe)

**Checkpoint:**
- **Fold-Over**: O(flush latency) ≈ O(dirty pages × 16ms) (SSD)
- **Snapshot**: O(memory size / bandwidth) ≈ O(GB/s) (RAM → disk)
- **Non-Blocking**: Threads continue operating during checkpoint (v+1 records)

**Recovery:**
- **Index Load**: O(index size / bandwidth) ≈ O(GB/s)
- **Log Replay**: O(flushed → final range size)
- **Snapshot Load**: O(snapshot size / bandwidth)

---

### 8.3 Durability Guarantees

**Group Commit Semantics:**
- ✅ Operations completed before `WaitForCommitAsync()` returns are durable
- ✅ Session commit points are independent (different sessions can have different durability)
- ✅ Exclusion list allows partial session commits

**Checkpoint Types:**
- **Fold-Over**: Durable up to `flushedLogicalAddress`
- **Snapshot**: Durable up to `finalLogicalAddress` (includes in-flight operations)
- **Incremental**: Durable up to base snapshot + delta

**Failure Modes:**
- ✅ **Crash Before Checkpoint**: Recover to last completed checkpoint
- ✅ **Crash During Checkpoint**: Incomplete checkpoint ignored, recover to prior
- ✅ **Partial Disk Write**: Checksum detects corruption, recovery fails safely
- ✅ **Lost Segment File**: Treated as truncation, data below BeginAddress lost

---

## 9. Testing Strategy

### 9.1 Unit Tests

**Hash Index:**
- Insert/lookup/chain walking
- Overflow bucket allocation
- Tag collision handling
- Concurrent insertion (stress test)

**Hybrid Log:**
- Page allocation/deallocation
- Address calculations
- Region boundaries (mutable/read-only/disk)
- Record serialization/deserialization

**Operations:**
- Read: found, not found, tombstone, expired
- Upsert: new, in-place, copy-on-write
- RMW: in-place, copy-update, initial-update, pending
- Delete: in-place tombstone, create tombstone, double-delete

**Checkpointing:**
- Fold-over checkpoint + recovery
- Snapshot checkpoint + recovery
- Incremental snapshot + recovery
- Multi-session commit points
- Exclusion handling

**Epoch:**
- Thread registration/deregistration
- Epoch advancement
- Phase tracking
- Grace period validation

---

### 9.2 Integration Tests

**Correctness:**
- CRUD workload (50% reads, 25% upserts, 20% RMW, 5% deletes)
- Checkpoint during active workload
- Recovery correctness (compare state before/after)
- Concurrent sessions (10 threads, independent operations)
- Large dataset (100M keys, 1TB log)

**Crash Resilience:**
- Crash during checkpoint (kill -9 mid-checkpoint)
- Corrupt checkpoint file (flip random bits)
- Missing segment file (delete log.5)
- Partial write (truncate log file)

**Performance:**
- Throughput benchmark (YCSB workloads A, B, C, D, F)
- Latency percentiles (p50, p95, p99, p99.9)
- Scalability (1, 4, 8, 16, 32 threads)
- Comparison with C++ FASTER (should be within 10%)

---

### 9.3 Compliance Tests

**Verify against C++ reference:**
- [ ] On-disk format matches (RecordInfo, checkpoint metadata)
- [ ] Can recover from C++ checkpoint
- [ ] C++ can recover from Rust checkpoint
- [ ] Behavioral parity (same operations produce same results)

**Verify against C# reference:**
- [ ] Checkpoint metadata compatible (parse C# info.dat)
- [ ] Session commit point semantics match
- [ ] Exclusion list handling matches

---

## 10. Open Questions

### 10.1 Resolved

**Q1**: Do operations ever fail permanently?  
**A**: No, all operations return RETRY_LATER or Pending; never permanent failure (unless device error)

**Q2**: Can two sessions have the same ID?  
**A**: No, session IDs must be unique per store instance; recovery requires exact ID match

**Q3**: What happens if `previous_address` points to invalid record?  
**A**: Chain walk terminates (treat as end of chain); GC ensures invalid records are unreachable

**Q4**: Can checkpoint run concurrently with resize?  
**A**: No, both use epoch barriers; one completes before other starts

**Q5**: How are 48-bit addresses extended to 64-bit?  
**A**: Future work; would require new RecordInfo format (breaking change)

---

### 10.2 For Rust Design

**Q1**: Should we use `unsafe` for hash table CAS, or atomic pointers?  
**Recommendation**: Atomic pointers (`AtomicU64`) for safety; unsafe only in proven hot paths

**Q2**: How to handle varlen keys/values in Rust?  
**Recommendation**: `trait Serializable` with `size()`, `serialize(&mut [u8])`, `deserialize(&[u8])`

**Q3**: Should we support `!Send` types in values?  
**Recommendation**: No, require `Send + Sync` for concurrency safety

**Q4**: How to integrate with Tokio runtime?  
**Recommendation**: `impl Future` for async operations, thread pool for blocking I/O

**Q5**: Should we expose raw pointers in FFI, or opaque handles?  
**Recommendation**: Opaque handles (`*mut FasterKvHandle`) for safety; hide internal structure

---

## 11. References

### 11.1 Research Papers

1. **FASTER Core**: Badrish Chandramouli et al., "FASTER: A Concurrent Key-Value Store with In-Place Updates", SIGMOD 2018  
   - Hybrid log architecture, latch-free concurrency

2. **CPR Recovery**: Guna Prasaad et al., "Concurrent Prefix Recovery: Performing CPR on a Database", SIGMOD 2019  
   - Group commit, session-based recovery, exclusion lists

3. **Epoch Protection**: Tianyu Li et al., "Performant Almost-Latch-Free Data Structures Using Epoch Protection", DaMoN 2022  
   - Optimized epoch implementation, thread-local caching

4. **Distributed Recovery**: Tianyu Li et al., "Asynchronous Prefix Recoverability for Fast Distributed Stores", SIGMOD 2021  
   - Multi-node recovery (out of scope for MVP)

---

### 11.2 Source Files

**C++ Core:**
- `cc/src/core/faster.h` — Main KV store template
- `cc/src/core/phase.h` — Checkpoint FSM
- `cc/src/core/light_epoch.h` — Epoch protection
- `cc/src/index/hash_table.h` — Hash index
- `cc/src/core/record.h` — RecordInfo header

**C# Core:**
- `cs/src/core/Index/FASTER/FASTER.cs` — Main KV store
- `cs/src/core/Index/FASTER/Implementation/Internal*.cs` — Operations
- `cs/src/core/Index/Synchronization/FullCheckpointStateMachine.cs` — Checkpoint
- `cs/src/core/Allocator/BlittableAllocator.cs` — Hybrid log
- `cs/src/core/Epochs/LightEpoch.cs` — Epoch protection

---

### 11.3 Documentation

- `docs/_docs/01-quick-start-guide.md` — High-level overview
- `docs/_docs/20-fasterkv-basics.md` — Core concepts
- `docs/_docs/25-fasterkv-recovery.md` — Checkpoint/recovery
- `docs/_docs/23-fasterkv-tuning.md` — Performance tuning
- `docs/_docs/29-fasterkv-cpp.md` — C++ specifics
- `docs/_docs/95-research-papers.md` — Academic papers

---

## 12. Conclusion

FASTER is a **highly consistent system** across C++ and C# implementations. The core algorithms (hybrid log, hash index, epoch protection, CPR) are nearly identical. Differences lie primarily in:
- **Language features**: C# has managed memory, C++ has manual control
- **Extended features**: C# has FasterLog + remote, C++ has F2 two-tier

For the **Rust implementation**:
- **Target C++ semantics** for core KV (proven, simpler)
- **Add C# FasterLog** (valuable standalone feature)
- **Defer F2 and remote** (complex, not MVP-critical)
- **Use binary metadata** (faster than text)
- **Support both sync and async** (no-runtime + Tokio)

This specification provides the **exact behavioral contracts** needed to build a FASTER-compatible Rust implementation that can interoperate with existing C++/C# checkpoints and match their performance characteristics.

---

**End of Cross-Implementation Analysis**
