# C# FASTER Implementation - Comprehensive Analysis
**Analyst:** Dooku (C# Expert)  
**Date:** 2026-03-05  
**Purpose:** Architectural reference for Rust implementation

---

## Executive Summary

Microsoft's C# FASTER implementation is a sophisticated concurrent key-value store combining lock-free hash indexing with a hybrid log (memory + disk). The architecture centers on **epoch-based memory reclamation**, **callback-driven I/O**, and **session-based concurrency control**.

**Key Design Pillars:**
1. **Lock-free operations** via epoch protection (LightEpoch)
2. **Hybrid log** with three-phase lifecycle (mutable → read-only → disk)
3. **Callback-driven API** (IFunctions interface) for user-defined semantics
4. **Session-based isolation** preventing intra-session concurrency
5. **Device abstraction** enabling multi-tier storage (memory/SSD/cloud)

**Critical Insight for Rust:** The C# implementation heavily relies on GC for lifetime management and `ref` parameters for zero-copy operations. Rust's ownership model offers opportunity to improve safety and ergonomics while preserving the core lock-free algorithms.

---

## 1. Core Architecture

### 1.1 Namespace Organization

```
cs/src/core/
├── Allocator/           # Hybrid log management (3 allocator types)
├── Async/               # Async operation state machines
├── ClientSession/       # Session contexts and lifecycle
├── Compaction/          # Log compaction utilities
├── Device/              # Storage device abstraction (IDevice)
├── Epochs/              # LightEpoch lock-free reclamation
├── FasterLog/           # Append-only log implementation
├── Index/               # Hash index, checkpoint, recovery
│   ├── CheckpointManagement/
│   ├── Common/          # Settings, contexts, interfaces
│   ├── FASTER/          # FasterKV main class
│   ├── Interfaces/      # IFunctions, IDevice, etc.
│   ├── Recovery/        # Checkpoint/recovery state machines
│   └── Synchronization/ # Threading primitives
├── Utilities/           # Helpers (hash, buffers, etc.)
└── VarLen/              # Variable-length type support
```

### 1.2 Class Hierarchy Overview

```
FasterBase (abstract)
└── FasterKV<Key, Value> : IFasterKV<Key, Value>
    ├── Manages: Hash Index + Hybrid Log + Read Cache
    ├── Creates: ClientSession<K, V, Input, Output, Context, Functions>
    └── Lifecycle: Checkpoint → Recovery → Dispose

AllocatorBase<Key, Value> (abstract)
├── BlittableAllocator<K, V>           # Fixed-size structs
├── GenericAllocator<K, V>             # Objects with serialization
└── VarLenBlittableAllocator<K, V>     # Variable-length structs

IFunctions<Key, Value, Input, Output, Context> (interface)
└── FunctionsBase<K, V, I, O, C> (19 virtual methods)
    └── SimpleFunctions<K, V, C> (common-case defaults)
```

### 1.3 Key Subsystems

| Subsystem | Location | Purpose |
|-----------|----------|---------|
| **Hash Index** | `Index/FASTER/` | Concurrent hash table (cache-line sized buckets) |
| **Hybrid Log** | `Allocator/AllocatorBase.cs` | Circular log with memory/disk tiers |
| **Epoch System** | `Epochs/LightEpoch.cs` | Lock-free reclamation via per-thread epochs |
| **Sessions** | `ClientSession/ClientSession.cs` | Thread-independent access handles |
| **Checkpointing** | `Index/Recovery/Checkpoint.cs` | Fuzzy checkpointing with delta logs |
| **Device Layer** | `Device/IDevice.cs` | Async I/O abstraction (local/cloud/null) |

---

## 2. FASTER KV API Surface

### 2.1 FasterKV - Main Entry Point

**Location:** `cs/src/core/Index/FASTER/FASTER.cs`

**Class Signature:**
```csharp
public partial class FasterKV<Key, Value> : FasterBase, IFasterKV<Key, Value>
```

**Key Public Properties:**
- `EntryCount` (long) — Active entries in hash index
- `IndexSize` (long) — Hash index size in cache lines (64 bytes)
- `OverflowBucketCount` (long) — Overflow bucket usage
- `Log` (LogAccessor<Key, Value>) — Main hybrid log accessor
- `ReadCache` (LogAccessor<Key, Value>) — Optional read cache
- `Comparer` (IFasterEqualityComparer<Key>) — Key comparison

**Key Public Methods:**

| Method | Purpose |
|--------|---------|
| `NewSession<Functions>(...)` | Create new client session |
| `For<Input, Output, Context>()` | Alternative session builder |
| `TryInitiateFullCheckpoint(...)` | Start index + log checkpoint |
| `TakeFullCheckpointAsync(...)` | Async checkpoint with token |
| `TryInitiateIndexCheckpoint(...)` | Index-only checkpoint |
| `TryInitiateHybridLogCheckpoint(...)` | Log-only checkpoint |
| `Recover(...)` / `RecoverAsync(...)` | Restore from checkpoint |
| `GrowIndex()` | Double hash index size |
| `Dispose()` | Cleanup resources |

**Checkpoint Types:**
- **Snapshot:** Takes separate log snapshot (safe, slower)
- **FoldOver:** Flushes current log (incremental-friendly, faster)

---

### 2.2 ClientSession - User Interaction Model

**Location:** `cs/src/core/ClientSession/ClientSession.cs`

**Class Signature:**
```csharp
public sealed class ClientSession<Key, Value, Input, Output, Context, Functions> 
    : IClientSession, IFasterContext<Key, Value, Input, Output, Context>, IDisposable
    where Functions : IFunctions<Key, Value, Input, Output, Context>
```

**Generic Parameters (6 total):**
- `Key`, `Value` — Storage types (matches FasterKV)
- `Input` — User input for operations (passed to IFunctions)
- `Output` — Result output from operations
- `Context` — User context for async continuations
- `Functions` — Callback implementation (IFunctions)

**Key Operations (Sync):**
```csharp
Status Read(ref Key key, ref Input input, ref Output output, Context ctx, long serialNo)
Status Upsert(ref Key key, ref Input input, ref Value value, Context ctx, long serialNo)
Status RMW(ref Key key, ref Input input, Context ctx, long serialNo)
Status Delete(ref Key key, Context ctx, long serialNo)
```

**Async Variants:**
```csharp
ValueTask<FasterKV.ReadAsyncResult<...>> ReadAsync(...)
ValueTask<FasterKV.UpsertAsyncResult<...>> UpsertAsync(...)
ValueTask<FasterKV.RmwAsyncResult<...>> RMWAsync(...)
ValueTask<FasterKV.DeleteAsyncResult<...>> DeleteAsync(...)
```

**Contexts (struct wrappers for different safety models):**
- **BasicContext** — Auto epoch management (default, safest)
- **UnsafeContext** — Manual epoch control (performance)
- **LockableContext** — Manual locking + auto epoch (transactions)
- **LockableUnsafeContext** — Manual locking + manual epoch (expert mode)

**Lifecycle:**
```csharp
using var session = store.NewSession(new MyFunctions());
// Operations...
// Auto-disposed: flushes pending, releases epoch
```

---

### 2.3 IFunctions Callback Interface

**Location:** `cs/src/core/Index/Interfaces/IFunctions.cs`

**Purpose:** User-defined semantics for all operations. FASTER is a generic engine; IFunctions defines application behavior.

**Interface Signature:**
```csharp
public interface IFunctions<Key, Value, Input, Output, Context>
```

**Callback Categories:**

#### **Read Callbacks (3 methods):**
```csharp
// Called when reading from immutable region
bool SingleReader(ref Key key, ref Input input, ref Value value, 
                  ref Output dst, ref ReadInfo readInfo)

// Called when reading from mutable region (concurrent)
bool ConcurrentReader(ref Key key, ref Input input, ref Value value, 
                      ref Output dst, ref ReadInfo readInfo)

// Async completion for disk reads
void ReadCompletionCallback(ref Key key, ref Input input, ref Output output, 
                            Context ctx, Status status, RecordMetadata recordMetadata)
```

#### **Upsert Callbacks (3 methods):**
```csharp
// Insert new record or RCU (read-copy-update) from immutable
bool SingleWriter(ref Key key, ref Input input, ref Value src, ref Value dst, 
                  ref Output output, ref UpsertInfo upsertInfo, WriteReason reason)

// Called after successful insert
void PostSingleWriter(ref Key key, ref Input input, ref Value src, ref Value dst, 
                      ref Output output, ref UpsertInfo upsertInfo, WriteReason reason)

// Update in-place in mutable region
bool ConcurrentWriter(ref Key key, ref Input input, ref Value src, ref Value dst, 
                      ref Output output, ref UpsertInfo upsertInfo)
```

#### **RMW Callbacks (9 methods):**
```csharp
// Decision: need to initialize new record?
bool NeedInitialUpdate(ref Key key, ref Input input, ref Output output, ref RMWInfo rmwInfo)

// Create new record (key not found)
bool InitialUpdater(ref Key key, ref Input input, ref Value value, 
                    ref Output output, ref RMWInfo rmwInfo)
void PostInitialUpdater(...)  // Post-insert callback

// Decision: need to copy from immutable to mutable?
bool NeedCopyUpdate(ref Key key, ref Input input, ref Value oldValue, 
                    ref Output output, ref RMWInfo rmwInfo)

// RCU: copy old value to new location + apply update
bool CopyUpdater(ref Key key, ref Input input, ref Value oldValue, ref Value newValue, 
                 ref Output output, ref RMWInfo rmwInfo)
void PostCopyUpdater(...)  // Post-RCU callback

// Update in-place (mutable region)
bool InPlaceUpdater(ref Key key, ref Input input, ref Value value, 
                    ref Output output, ref RMWInfo rmwInfo)

// Async completion
void RMWCompletionCallback(ref Key key, ref Input input, ref Output output, 
                           Context ctx, Status status, RecordMetadata recordMetadata)
```

#### **Delete Callbacks (3 methods):**
```csharp
// Delete in immutable region (marks tombstone)
bool SingleDeleter(ref Key key, ref Value value, ref DeleteInfo deleteInfo)
void PostSingleDeleter(...)

// Delete in mutable region
bool ConcurrentDeleter(ref Key key, ref Value value, ref DeleteInfo deleteInfo)
```

#### **Disposal Callbacks (3 methods):**
```csharp
// Called when records are evicted/checkpointed to release resources
void DisposeSingleWriter(...)
void DisposeCopyUpdater(...)
void DisposeInitialUpdater(...)
```

**Total:** 19 callback methods. Most have default no-op implementations in `FunctionsBase<K, V, I, O, C>`.

**Usage Pattern:**
```csharp
public class MyFunctions : FunctionsBase<long, long, long, long, Empty>
{
    public override bool SingleReader(ref long key, ref long input, ref long value, 
                                      ref long output, ref ReadInfo readInfo)
    {
        output = value;  // Copy value to output
        return true;     // Success
    }

    public override bool InPlaceUpdater(ref long key, ref long input, ref long value, 
                                        ref long output, ref RMWInfo rmwInfo)
    {
        value += input;  // Increment value by input
        return true;
    }
}
```

---

### 2.4 Context Objects and Flow

**Context Info Structs (passed to IFunctions):**

| Struct | Contains | Purpose |
|--------|----------|---------|
| **ReadInfo** | Address, version, recSrc (disk/memory/cache) | Read operation metadata |
| **UpsertInfo** | Address, version, sessionID | Upsert operation metadata |
| **RMWInfo** | Address, version, sessionID, recSrc | RMW operation metadata |
| **DeleteInfo** | Address, version, sessionID | Delete operation metadata |

**Context Flow:**
```
User calls session.Read(ref key, ref input, ref output, userContext, serialNo)
    ↓
FASTER internal logic determines: mutable vs immutable, disk vs memory
    ↓
Calls functions.SingleReader(ref key, ref input, ref value, ref output, ref readInfo)
    ↓
User code populates output from value
    ↓
Returns status to user (Found, NotFound, Pending)
    ↓
If Pending: functions.ReadCompletionCallback(ref key, ref input, ref output, userContext, ...)
```

**User Context:** Arbitrary type (e.g., `TaskCompletionSource`, callback state) passed through async pipeline.

---

### 2.5 Async API Execution Model

**Location:** `cs/src/core/Async/`

**Two-Phase Pattern:**
1. **Fast Path:** Synchronous attempt within epoch protection
2. **Slow Path:** Async wait for I/O completion (flush or disk read)

**ValueTask Pattern:**
```csharp
var result = await session.ReadAsync(ref key, ref input, userContext, serialNo, token);

// May need to await again if still pending (rare)
while (result.Status.IsPending)
    result = await result.CompleteAsync(token);

// Access output
if (result.Status.Found)
    Console.WriteLine(result.Output);
```

**Internal State Machine:**
```
AsyncOperationInternal<TAsyncOperation, TAsyncResult>
    ├── CompletionComputeStatus (atomic: Pending → Completed)
    ├── DoFastOperation() — Retry sync path after I/O
    └── DoSlowOperation() — Async wait on flushEvent or ioCompletionSource
```

**Key Constraint:** Session is **mono-threaded**. Cannot call methods on same session from multiple threads concurrently. Async operations must complete before next operation.

---

### 2.6 Configuration - FasterKVSettings

**Location:** `cs/src/core/Index/Common/FasterKVSettings.cs`

**Class Signature:**
```csharp
public sealed class FasterKVSettings<Key, Value> : IDisposable
```

**Key Properties:**

| Property | Type | Default | Purpose |
|----------|------|---------|---------|
| **IndexSize** | long | 64MB | Hash index size (cache lines) |
| **LogDevice** | IDevice | null | Main hybrid log device |
| **ObjectLogDevice** | IDevice | null | Object serialization device (GenericAllocator) |
| **PageSize** | long | 32MB | Log page size |
| **SegmentSize** | long | 1GB | Log segment size (group of pages) |
| **MemorySize** | long | 16GB | Total in-memory log size |
| **MutableFraction** | double | 0.9 | Fraction for in-place updates |
| **ReadCacheEnabled** | bool | false | Enable separate read cache |
| **ReadCacheMemorySize** | long | 32MB | Read cache memory |
| **PreallocateLog** | bool | false | Preallocate pages (faster startup) |
| **EqualityComparer** | IFasterEqualityComparer | null | Custom key comparer |
| **TryRecoverLatest** | bool | false | Auto-recover on init |
| **CheckpointDir** | string | null | Checkpoint storage path |
| **RemoveOutdatedCheckpoints** | bool | true | Auto-cleanup old checkpoints |
| **ConcurrencyControlMode** | enum | None | LockTable or RecordIsolation |

**Constructor Shortcuts:**
```csharp
// In-memory (null path)
var settings = new FasterKVSettings<long, long>(null);

// Disk-backed
var settings = new FasterKVSettings<long, long>("/data/faster") 
{ 
    TryRecoverLatest = true 
};
```

---

## 3. Allocator & Hybrid Log Architecture

### 3.1 AllocatorBase - Core Abstraction

**Location:** `cs/src/core/Allocator/AllocatorBase.cs`

**Role:** Orchestrates the hybrid log lifecycle with three address-bounded regions.

**Address Semantics:**

```
┌─────────────────────────────────────────────────────────────┐
│ TailAddress                      (next write position)      │
├─────────────────────────────────────────────────────────────┤
│ MUTABLE REGION                   (in-place updates)         │
│ • Concurrent reads/writes                                   │
│ • Lock-free allocation via CAS on TailPageOffset            │
├─────────────────────────────────────────────────────────────┤
│ ReadOnlyAddress / SafeReadOnlyAddress                       │
├─────────────────────────────────────────────────────────────┤
│ READ-ONLY REGION                 (sealed, flushing)         │
│ • No writes; concurrent reads only                          │
│ • Async flush to disk in progress                           │
├─────────────────────────────────────────────────────────────┤
│ HeadAddress / SafeHeadAddress                               │
├─────────────────────────────────────────────────────────────┤
│ CLOSED REGION                    (persisted, evictable)     │
│ • Pages flushed to disk                                     │
│ • In-memory copy can be freed                               │
├─────────────────────────────────────────────────────────────┤
│ BeginAddress                     (earliest valid address)   │
└─────────────────────────────────────────────────────────────┘
```

**Key Methods:**

| Method | Purpose |
|--------|---------|
| `ShiftReadOnlyAddress(long newAddr)` | Seal mutable region; trigger flushing |
| `OnPagesMarkedReadOnly(long addr)` | Epoch callback; notify observers; queue flush |
| `AsyncFlushPages(long fromAddr, long toAddr)` | Write pages to device |
| `ShiftHeadAddress(long newAddr)` | Mark pages evictable |
| `OnPagesClosed(long addr)` | Free pages from memory |
| `ShiftBeginAddress(long addr, bool truncate)` | Truncate disk segments |

**Page Status Tracking:**
```csharp
// Per-page metadata
public struct FullPageStatus {
    public long LastFlushedUntilAddress;  // Highest persisted address
    public long Dirty;                     // Dirty version marker
}

// Pending flush operations (linked list per page)
internal readonly PendingFlushList[] PendingFlush;
```

---

### 3.2 Three Allocator Implementations

#### **A. BlittableAllocator<Key, Value>**
**Location:** `cs/src/core/Allocator/BlittableAllocator.cs`

**Use Case:** Fixed-size blittable structs (primitive types, PODs)

**Storage Layout:**
```
byte[][] pages
  ├── Page 0: [RecordInfo | Key | Value][RecordInfo | Key | Value]...
  ├── Page 1: [RecordInfo | Key | Value][RecordInfo | Key | Value]...
  └── ...
```

**Record Format:**
```csharp
struct Record<Key, Value> {
    RecordInfo info;  // 16 bytes (atomic flags, address chain)
    Key key;          // Fixed size
    Value value;      // Fixed size
}
```

**Performance:** Fastest. Zero serialization. Direct memory access.

---

#### **B. GenericAllocator<Key, Value>**
**Location:** `cs/src/core/Allocator/GenericAllocator.cs`

**Use Case:** Object types (classes, complex types)

**Storage Layout:**
```
Record<Key, Value>[][] pages  (object arrays)
objectLogDevice               (separate device for serialized objects)
```

**Two-Device Architecture:**
1. **Main device:** Record structure + object pointers
2. **Object device:** Serialized object data (100MB chunks)

**Serialization:**
- Uses `SerializerSettings<Key>` and `SerializerSettings<Value>`
- Fallback: `DataContractSerializer`
- Custom: `IObjectSerializer<T>` interface

**Challenge:** Slowest due to serialization + dual I/O.

---

#### **C. VarLenBlittableAllocator<Key, Value>**
**Location:** `cs/src/core/Allocator/VarLenBlittableAllocator.cs`

**Use Case:** Variable-length blittable types (strings as byte[], variable structs)

**Storage Layout:**
```
[RecordInfo (8-byte aligned)][Variable Key Data][Variable Value Data]
```

**Alignment:** 8-byte boundaries for atomic access.

**Configuration:**
```csharp
public interface IVariableLengthStruct<T> {
    int GetLength(ref T t);
    void Serialize(ref T source, void* destination);
    void Deserialize(void* source, ref T target);
}
```

**Performance:** Fast. No serialization overhead. Packed layout.

---

### 3.3 Hybrid Log State Transitions

**Lifecycle Pipeline:**

```
┌─────────────────────────────────────────────────────────────┐
│ 1. MUTABLE REGION                                           │
│ • Threads allocate via CAS on TailPageOffset                │
│ • Write records directly                                    │
└────────────────┬────────────────────────────────────────────┘
                 │ ShiftReadOnlyAddress(addr)
                 ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. TRANSITION: Epoch Bump                                   │
│ • epoch.BumpCurrentEpoch(() => OnPagesMarkedReadOnly(addr)) │
│ • All active threads exit current epoch                     │
│ • Callback fires when safe                                  │
└────────────────┬────────────────────────────────────────────┘
                 ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. READ-ONLY REGION                                         │
│ • OnPagesMarkedReadOnly() → AsyncFlushPages()               │
│ • device.WriteAsync() for each page                         │
│ • PendingFlushList tracks active flushes                    │
└────────────────┬────────────────────────────────────────────┘
                 │ AsyncFlushPageCallback(errorCode, numBytes, context)
                 ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. CLOSED REGION                                            │
│ • ShiftHeadAddress(addr) marks pages evictable              │
│ • OnPagesClosed() → OnPagesClosedWorker()                   │
│ • Epoch-protected iteration per page                        │
│ • OnEvictionObserver called                                 │
│ • FreePage() returns page to pool                           │
└────────────────┬────────────────────────────────────────────┘
                 │ ShiftBeginAddress(addr, truncateLog=true)
                 ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. DISK-ONLY                                                │
│ • In-memory pages released                                  │
│ • device.TruncateUntilAddressAsync() frees disk segments    │
└─────────────────────────────────────────────────────────────┘
```

**Key Invariants:**
- `BeginAddress ≤ ClosedUntilAddress ≤ FlushedUntilAddress`
- `FlushedUntilAddress ≤ SafeHeadAddress ≤ HeadAddress`
- `HeadAddress ≤ SafeReadOnlyAddress ≤ ReadOnlyAddress ≤ TailAddress`

---

### 3.4 Page Management

**MallocFixedPageSize<T>**
**Location:** `cs/src/core/Allocator/MallocFixedPageSize.cs`

**Purpose:** Fixed-page allocator for metadata structures (not log records).

**Architecture:**
```csharp
const int PageSizeBits = 16;  // 64KB pages
const int LevelSize = 1 << 12;  // 4K levels

private T[][] values;  // Jagged array for scalability
private GCHandle[] handles;  // Pinned for native access (.NET < 5)
```

**Usage:**
- Allocates pointers array for hybrid log pages
- Maintains GC pin for safe interop with native code
- Multi-level array scales to billions of objects

---

### 3.5 Flush Mechanisms

**PendingFlushList**
**Location:** `cs/src/core/Allocator/PendingFlushList.cs`

**Structure:** Linked list of `PageAsyncFlushResult` per page.

**Coalescing:**
- `RemoveNextAdjacent()` / `RemovePreviousAdjacent()` merge overlapping flush regions
- Reduces I/O ops when multiple flushes target same page

**Callback Flow:**
```
AsyncFlushPages(fromAddr, toAddr)
  ├─ For each page in range:
  │   ├─ Create PageAsyncFlushResult
  │   ├─ Add to PendingFlush[page]
  │   └─ device.WriteAsync(..., AsyncFlushPageCallback, asyncResult)
  └─ Wait on flushEvent
      ↓
AsyncFlushPageCallback(errorCode, numBytes, context)
  ├─ Update PageStatusIndicator[page].LastFlushedUntilAddress
  ├─ Remove from PendingFlush[page]
  └─ Signal flushEvent if all complete
```

---

### 3.6 Iterators

**Types:** 6 iterator implementations for different allocators and modes.

| Iterator | File | Use Case |
|----------|------|----------|
| **BlittableScanIterator** | BlittableScanIterator.cs | Fixed-size K/V traversal |
| **GenericScanIterator** | GenericScanIterator.cs | Object K/V with lazy deser |
| **VarLenBlittableScanIterator** | VarLenBlittableScanIterator.cs | Variable-length K/V |
| **MemoryPageScanIterator** | MemoryPageScanIterator.cs | In-memory only |

**Two Iteration Modes:**

**1. Pull-Based:**
```csharp
while (iterator.GetNext(out var recordInfo)) {
    var key = iterator.GetKey();
    var value = iterator.GetValue();
    ProcessRecord(key, value);
}
```

**2. Push-Based:**
```csharp
iterator.Scan<MyScanFunctions>(store, beginAddr, endAddr, 
                               ref scanFunctions, buffering);
// Calls scanFunctions.SingleReader() for each record
```

**Buffering Modes:**
- `SinglePageBuffering` — 1 page at a time
- `DoublePageBuffering` — 2 pages (prefetch)
- `NoBuffering` — On-demand reads

---

## 4. Epoch Protection System

### 4.1 LightEpoch Implementation

**Location:** `cs/src/core/Epochs/LightEpoch.cs`

**Core Concept:** Lock-free garbage collection via per-thread epoch tracking.

**Architecture:**

```
┌──────────────────────────────────────────────────────────┐
│ Epoch Table (cache-aligned 64-byte entries)             │
├──────────────────────────────────────────────────────────┤
│ Entry 0: [localCurrentEpoch | threadId | markers[6]]     │
│ Entry 1: [localCurrentEpoch | threadId | markers[6]]     │
│ ...                                                      │
│ Entry N: [localCurrentEpoch | threadId | markers[6]]     │
└──────────────────────────────────────────────────────────┘
       ↓
Thread enters → writes CurrentEpoch to its table entry
       ↓
System computes SafeToReclaimEpoch = min(all active epochs) - 1
       ↓
Memory from epochs ≤ SafeToReclaimEpoch can be freed
```

**Key Components:**

| Component | Purpose |
|-----------|---------|
| **CurrentEpoch** | Global monotonic counter (volatile long) |
| **Epoch Table** | Per-thread slots (64-byte aligned) |
| **SafeToReclaimEpoch** | Cached minimum epoch across all threads |
| **DrainList** | Queue of `(epoch, Action)` deferred actions |

**Thread Lifecycle:**

```
Resume()
  ├─ Acquire table entry (hash-based probe)
  ├─ Increment reentrance counter
  └─ Write CurrentEpoch to entry

ProtectAndDrain()
  ├─ Update localCurrentEpoch
  ├─ Execute pending drains if their epoch is safe
  └─ Return

Suspend()
  ├─ Clear table entry (set epoch = 0)
  ├─ Decrement reentrance counter
  └─ Run suspended drains if last suspend
```

**BumpCurrentEpoch(Action):**
```csharp
public void BumpCurrentEpoch(Action onDrain)
{
    Interlocked.Increment(ref CurrentEpoch);
    AddToDrainList(onDrain, CurrentEpoch);
    
    // Try to execute drains if safe
    ComputeNewSafeToReclaimEpoch();
    ExecuteDrainList();
}
```

---

### 4.2 Lock-Free Guarantees

**No Locks Because:**
1. **Atomic Operations:** All epoch updates use `Interlocked.CompareExchange()`
2. **Per-Thread Isolation:** Each thread modifies only its own table entry
3. **Cache-Line Alignment:** 64-byte entries prevent false sharing
4. **Read-Only Scanning:** SafeToReclaimEpoch computed via consistent snapshot read

**Wait-Free Forward Progress:**
- Table entry reservation: Hash-based probe with two offsets (Murmur3)
- Guaranteed O(1) slot acquisition (no unbounded retries)

**Reclamation Safety:**
- Actions deferred to epoch `E` execute only when all threads have moved past `E`
- Epoch gate pattern: `epoch.BumpCurrentEpoch(() => OnPagesMarkedReadOnly())`

---

### 4.3 Integration with FASTER

**Usage Pattern:**
```csharp
// Enter protected region
fht.epoch.Resume();

try {
    // Read/write operations (safe from concurrent reclamation)
    InternalRead(ref key, ref input, ref output, ...);
} finally {
    // Exit protected region
    fht.epoch.Suspend();
}
```

**Deferred Actions:**
```csharp
// Queue action for safe execution
epoch.BumpCurrentEpoch(() => {
    // This runs when all threads exit current epoch
    OnPagesMarkedReadOnly(newReadOnlyAddress);
});
```

**Reentrance Support:**
- `threadEntryIndexCount` tracks nested `Resume()` calls
- Only releases slot when counter reaches 0
- Prevents "unsafe re-entrance" errors

---

## 5. Checkpointing and Recovery

### 5.1 Checkpoint Types

**Location:** `cs/src/core/Index/Common/CheckpointSettings.cs`

**Enum:**
```csharp
public enum CheckpointType {
    Snapshot,  // Separate log snapshot (safe, slower)
    FoldOver   // Flush current log (incremental-friendly, faster)
}
```

**Three Checkpoint Scopes:**
1. **Index Checkpoint** — Hash table only
2. **Hybrid Log Checkpoint** — Log only (snapshot or delta)
3. **Full Checkpoint** — Index + Hybrid Log

---

### 5.2 Checkpoint State Machines

**Location:** `cs/src/core/Index/Common/Contexts.cs`

#### **A. IndexCheckpointInfo**
```csharp
public struct IndexCheckpointInfo {
    Guid token;
    long table_size;
    long num_ht_bytes;      // Main hash table bytes
    long num_ofb_bytes;     // Overflow buckets bytes
    long num_buckets;
    long startLogicalAddress;
    long finalLogicalAddress;
}
```

**Persisted Files:**
```
{checkpointDir}/index-checkpoints/{token}/
├── info.dat  (IndexRecoveryInfo metadata, text format)
├── ht.dat    (Hash table)
└── ofb.dat   (Overflow buckets)
```

---

#### **B. HybridLogCheckpointInfo**
```csharp
public struct HybridLogCheckpointInfo {
    Guid guid;
    long version;
    long nextVersion;
    long flushedLogicalAddress;
    long snapshotStartFlushedLogicalAddress;
    long snapshotFinalLogicalAddress;
    long startLogicalAddress;
    long finalLogicalAddress;
    long headAddress;
    long beginAddress;
    long deltaTailAddress;  // -1 if no delta log
    Dictionary<string, long> checkpointTokens;  // Per-session continuations
    long[] objectLogSegmentOffsets;  // For GenericAllocator
}
```

**Persisted Files:**
```
{checkpointDir}/cpr-checkpoints/{token}/
├── info.dat          (HybridLogRecoveryInfo metadata, text format)
├── snapshot.dat      (Log snapshot, if CheckpointType.Snapshot)
├── snapshot.obj.dat  (Object log, if GenericAllocator)
└── delta.dat         (Incremental delta log, if CheckpointType.FoldOver)
```

---

### 5.3 Recovery Flow

**Location:** `cs/src/core/Index/Recovery/Recovery.cs`

**Entry Points:**
```csharp
// Synchronous
void Recover(int numPagesToPreload = -1, 
             bool undoNextVersion = true, 
             long recoverTo = -1)

// Asynchronous
Task RecoverAsync(int numPagesToPreload = -1, 
                  bool undoNextVersion = true, 
                  long recoverTo = -1, 
                  CancellationToken cancellationToken = default)
```

**Recovery Steps:**

```
1. GetLatestCheckpointTokens()
   ├─ Query ICheckpointManager for latest or version-specific tokens
   └─ Returns: (indexToken, logToken)

2. Load Index Checkpoint
   ├─ IndexCheckpointInfo.Recover(indexToken)
   ├─ Read info.dat metadata
   ├─ GetIndexDevice(indexToken) from checkpoint manager
   └─ Restore hash table + overflow buckets

3. Load Hybrid Log Checkpoint
   ├─ HybridLogCheckpointInfo.Recover(logToken)
   ├─ Read info.dat metadata
   ├─ GetSnapshotLogDevice(logToken) from checkpoint manager
   └─ Restore log addresses

4. Async Page Loading
   ├─ Create ReadStatus[numPages] (Pending → Done)
   ├─ Issue device.ReadAsync() for each page
   ├─ Wait on semaphores for completion
   └─ Parallel page reads (batched)

5. Async Page Flushing (to memory)
   ├─ Create FlushStatus[numPages]
   ├─ Move page data to allocator
   ├─ Wait on semaphores
   └─ Update allocator addresses

6. Delta Log Scanning (if incremental)
   ├─ Scan delta.dat for DeltaLogEntryType.CHECKPOINT_METADATA
   ├─ Find latest version in delta stream
   └─ Apply delta entries

7. Cleanup
   ├─ Delete tentative entries (from incomplete checkpoints)
   ├─ Close recovery devices
   └─ Restore session continuations (checkpointTokens)
```

**Recovery Status State Machine:**
```csharp
internal struct ReadStatus {
    long status;  // Pending = 0, Done = 1, Error = -1
    ManualResetEventSlim handle;  // Signaled on completion
}

// Same for FlushStatus
```

---

### 5.4 Incremental Checkpointing (Delta Log)

**Location:** `cs/src/core/Index/Recovery/DeltaLog.cs`

**Delta Log Entry Types:**
```csharp
public enum DeltaLogEntryType {
    DELTA = 0,               // Individual record changes
    CHECKPOINT_METADATA = 1  // Checkpoint boundary marker
}
```

**Entry Format:**
```
[8-byte Checksum][4-byte Length][4-byte Type][Payload]
```

**Purpose:**
- Enables recovering to specific versions without full log rescan
- Reduces checkpoint size (only changes since last checkpoint)
- Supports fast rollback to any checkpoint version

---

### 5.5 Checkpoint Manager Interface

**Location:** `cs/src/core/Index/Recovery/ICheckpointManager.cs`

**Interface Methods:**

| Method | Purpose |
|--------|---------|
| `InitializeIndexCheckpoint(Guid token)` | Prepare index checkpoint storage |
| `GetIndexDevice(Guid token)` | Get device for reading index checkpoint |
| `CommitIndexCheckpoint(Guid token, byte[] metadata)` | Finalize index checkpoint |
| `InitializeLogCheckpoint(Guid token)` | Prepare log checkpoint storage |
| `GetSnapshotLogDevice(Guid token)` | Get device for snapshot log |
| `GetSnapshotObjectLogDevice(Guid token)` | Get device for object log |
| `CommitLogCheckpoint(Guid token, byte[] metadata)` | Finalize log checkpoint |
| `GetLogCheckpointTokens()` | List available checkpoints |
| `PurgeAll()` | Delete all checkpoints |

**Default Implementation:**
- **DeviceLogCommitCheckpointManager** — Local file-based storage
- **NullCheckpointManager** — No-op (for testing)

---

## 6. FasterLog - Append-Only Log

### 6.1 Architecture Overview

**Location:** `cs/src/core/FasterLog/`

**Class Signature:**
```csharp
public sealed class FasterLog : IDisposable
```

**Key Differences from FasterKV:**

| Aspect | FasterLog | FasterKV |
|--------|-----------|----------|
| **Data Model** | Append-only byte streams | Key-value pairs |
| **Access** | Sequential (FIFO) | Random (hash-based) |
| **Semantics** | Immutable entries | Mutable values (update-in-place) |
| **Allocator** | `BlittableAllocator<Empty, byte>` | Hash-indexed hybrid log |
| **Use Case** | Event logs, WAL, messaging | Caching, in-memory DB |

---

### 6.2 Commit Semantics

**Commit Types:**

| Method | Behavior |
|--------|----------|
| `Commit(bool spinWait = false)` | Soft commit (may batch with others) |
| `CommitStrongly(...)` | Enforced unique commit (returns monotonic commitNum) |
| `CommitAsync()` | Async commit with task chaining |
| `WaitForCommit(untilAddr)` | Spin-wait until address persisted |
| `WaitForCommitAsync(untilAddr)` | Async wait for commit |

**Commit Policy:**
```csharp
public abstract class LogCommitPolicy {
    public abstract void OnAppend(...);
    public abstract void OnCommit(...);
}

// Implementations:
DefaultCommitPolicy     // Batches commits per time window
MaxParallelCommitPolicy // Limits concurrent commits
RateLimitCommitPolicy   // Throttles commit rate
```

**Commit Chain:**
```csharp
var commitTask = log.CommitAsync();
var linkedCommit = await commitTask;
// linkedCommit.NextTask points to next commit in chain
```

---

### 6.3 Recovery Guarantees

**FasterLogRecoveryInfo:**
```csharp
public struct FasterLogRecoveryInfo {
    long BeginAddress;
    long UntilAddress;
    long CommitNum;
    Dictionary<string, long> Iterators;  // Named iterator positions
    byte[] Cookie;                        // User metadata
    long Checksum;                        // XOR of all fields
}
```

**Recovery Process:**
```csharp
log.Recover(requestedCommitNum);
// Or:
log.RecoverReadOnly();  // For read replicas
```

**Guarantees:**
- **Idempotent Recovery:** Checksum validation prevents corruption
- **Iterator Persistence:** Named iterators resume from last position
- **Cookie Support:** Application metadata (e.g., transaction ID) persisted
- **Point-in-Time Recovery:** Recover to specific commitNum

---

### 6.4 Iterator Patterns

**Creation:**
```csharp
var iterator = log.Scan(
    beginAddress: log.BeginAddress,
    endAddress: log.TailAddress,
    name: "my-iterator",  // Persisted position
    recover: true,
    scanBufferingMode: ScanBufferingMode.DoublePageBuffering,
    scanUncommitted: false  // Don't read beyond CommittedUntilAddress
);
```

**Reading Patterns:**

**1. Async Enumerable:**
```csharp
await foreach (var (entry, length, addr, nextAddr) in iterator.GetAsyncEnumerable())
{
    ProcessEntry(entry, length);
}
```

**2. Consumer Pattern:**
```csharp
public class MyConsumer : ILogEntryConsumer {
    public void Consume(byte[] entry, int length, long currentAddress, long nextAddress) {
        // Process entry
    }
}

await iterator.ConsumeAllAsync(new MyConsumer());
```

**3. Manual Iteration:**
```csharp
while (iterator.GetNext(out byte[] entry, out int length, out long addr, out long nextAddr))
{
    ProcessEntry(entry, length);
}
```

**Key Features:**
- **Named Iterators:** Tracked in recovery metadata
- **Scan Uncommitted:** Optional read beyond commit boundary
- **Buffering:** Single/double page buffering modes
- **Completion Detection:** `LogCompleted` flag when log finalized

---

### 6.5 Key API Surface

**Enqueue Operations:**
```csharp
// Basic
long Enqueue(byte[] entry)
long Enqueue(ReadOnlySpan<byte> entry)

// Async
ValueTask<long> EnqueueAsync(byte[] entry, CancellationToken token = default)

// Batch
long Enqueue<T>(IEnumerable<T> entries) where T : ILogEnqueueEntry

// With header
long Enqueue<THeader>(THeader header, SpanByte body) where THeader : unmanaged

// Try (non-blocking)
bool TryEnqueue(byte[] entry, out long logicalAddress)

// Enqueue + commit
void EnqueueAndWaitForCommit(byte[] entry)
```

**Lifecycle Operations:**
```csharp
// Initialize
void Initialize(long beginAddress, long committedUntilAddress, long lastCommitNum)

// Recovery
void Recover(long requestedCommitNum = -1)
void RecoverReadOnly()

// Finalization
void CompleteLog(bool spinWait = false)
Task CompleteLogAsync(CancellationToken token = default)

// Cleanup
void Reset()  // Clear log state
void Dispose()
```

---

## 7. Device Abstraction Layer

### 7.1 IDevice Interface

**Location:** `cs/src/core/Device/IDevice.cs`

**Core Contract:**
```csharp
public interface IDevice {
    // Properties
    uint SectorSize { get; }
    long Capacity { get; }
    long SegmentSize { get; }
    int StartSegment { get; }
    int EndSegment { get; }
    int ThrottleLimit { get; set; }
    
    // Async I/O (callback-based, not Task-based)
    void ReadAsync(int segmentId, ulong offset, IntPtr buffer, 
                   uint readLength, DeviceIOCompletionCallback callback, object context);
    void WriteAsync(IntPtr buffer, int segmentId, ulong offset, 
                    uint numBytes, DeviceIOCompletionCallback callback, object context);
    
    // Lifecycle
    void Initialize(long segmentSize, LightEpoch epoch = null, bool omitSegmentIdFromFilename = false);
    void Reset();
    void Dispose();
    
    // Truncation
    void TruncateUntilAddress(long toAddress);
    void TruncateUntilAddressAsync(long toAddress, AsyncCallback callback, IAsyncResult result);
}

public delegate void DeviceIOCompletionCallback(uint errorCode, uint numBytes, object context);
```

---

### 7.2 Key Implementations

#### **A. LocalStorageDevice (Windows)**
**Location:** `cs/src/core/Device/LocalStorageDevice.cs`

**Architecture:**
- **Native I/O Completion Ports** or **Overlapped I/O**
- Worker threads poll completion ports
- Zero-copy via `IntPtr` (pinned memory)

**Flow:**
```
WriteAsync(IntPtr source, segmentId, offset, numBytes, callback, context)
  ├─ Create NativeOverlapped
  ├─ Issue Windows native WriteFile()
  ├─ Worker thread polls completion port
  ├─ Unpack NativeOverlapped → SimpleAsyncResult
  └─ Invoke callback(errorCode, numBytes, context)
```

---

#### **B. ManagedLocalStorageDevice (Cross-Platform)**
**Location:** `cs/src/core/Device/ManagedLocalStorageDevice.cs`

**Architecture:**
- **AsyncPool<Stream>** — Manages file handle pool (prevents exhaustion)
- **Task-based async** via .NET Stream APIs
- Callback invoked after Task completion

**Flow:**
```
WriteAsync(IntPtr source, segmentId, offset, numBytes, callback, context)
  ├─ Get Stream from AsyncPool
  ├─ Fire stream.WriteAsync() on Task thread pool
  ├─ Await Task completion
  ├─ Return Stream to pool
  └─ Invoke callback(errorCode, numBytes, context)
```

---

#### **C. NullDevice**
**Location:** `cs/src/core/Device/NullDevice.cs`

**Purpose:** Testing/benchmarking (no actual I/O).

**Behavior:**
```csharp
public void WriteAsync(..., DeviceIOCompletionCallback callback, object context) {
    callback(0, numBytes, context);  // Immediate success
}
```

---

### 7.3 Async I/O Patterns

**Why Callback-Based (Not Task-Based)?**
1. **Zero Allocation:** No Task<T> overhead
2. **Ultra-Low Latency:** Direct callback invocation
3. **High Throughput:** Millions of I/Os without GC pressure

**Integration with Allocator:**
```csharp
// Write path (flush)
device.WriteAsync(
    alignedSourceAddress: pagePointer,
    segmentId: pageIndex % numSegments,
    offset: pageOffset,
    numBytes: pageSize,
    callback: AsyncFlushPageCallback,
    context: new PageAsyncFlushResult(...)
);

// Read path (page fault)
device.ReadAsync(
    segmentId: pageIndex % numSegments,
    offset: pageOffset,
    buffer: destinationPointer,
    readLength: pageSize,
    callback: PageReadCallback,
    context: new AsyncIOContext<Key, Value>(...)
);
```

---

### 7.4 Configuration & Lifecycle

**Initialization:**
```csharp
device.Initialize(
    segmentSize: 1L << 30,  // 1GB
    epoch: fht.epoch,       // For safe segment deletion
    omitSegmentIdFromFilename: false
);
```

**Throttling:**
```csharp
// Check before issuing I/O
if (device.Throttle())
{
    // Too many pending I/Os; backpressure
    Thread.Yield();
}

device.WriteAsync(...);
```

**Truncation:**
```csharp
// Remove old segments (epoch-protected)
device.TruncateUntilAddressAsync(oldBeginAddress, callback, asyncResult);
```

---

## 8. Session and Locking Model

### 8.1 ClientSession Lifecycle

**Creation:**
```csharp
// Pattern 1: Direct
using var session = store.NewSession(new MyFunctions());

// Pattern 2: Builder (supports Input/Output/Context types)
using var session = store.For<MyInput, MyOutput, MyContext>()
                          .NewSession<MyFunctions>(new MyFunctions());

// Pattern 3: Resume (from checkpoint)
using var session = store.For<MyInput, MyOutput, MyContext>()
                          .ResumeSession<MyFunctions>(new MyFunctions(), "session-name", 
                                                      out commitPoint);
```

**Disposal:**
```csharp
public void Dispose()
{
    this.completedOutputs?.Dispose();  // Flush output iterator
    CompletePending(true);             // Complete all pending ops
    fht.DisposeClientSession(ID, ctx.phase);  // Release session ID
}
```

---

### 8.2 Context Types (Safety Models)

| Context | Epoch Management | Locking Support | Thread Safety | Use Case |
|---------|------------------|-----------------|---------------|----------|
| **BasicContext** | Automatic | No | Auto | Default (safest) |
| **UnsafeContext** | Manual | No | Manual | Performance |
| **LockableContext** | Automatic | Yes | Auto | Transactions |
| **LockableUnsafeContext** | Manual | Yes | Manual | Expert mode |

**All contexts are structs wrapping ClientSession:**
```csharp
public struct BasicContext : IFasterContext<Key, Value, Input, Output, Context> {
    private readonly ClientSession<Key, Value, Input, Output, Context, Functions> clientSession;
    // Auto epoch wrapping
}
```

---

### 8.3 BasicContext (Default)

**Location:** `cs/src/core/ClientSession/BasicContext.cs`

**Usage:**
```csharp
var status = session.BasicContext.Read(ref key, ref input, ref output, userContext, serialNo);
```

**Epoch Wrapping:**
```csharp
internal Status Read(ref Key key, ref Input input, ref Output output, ...)
{
    clientSession.UnsafeResumeThread();  // Enter epoch
    try {
        return clientSession.InternalRead(ref key, ref input, ref output, ...);
    } finally {
        clientSession.UnsafeSuspendThread();  // Exit epoch
    }
}
```

**Guarantees:**
- Thread-safe (automatic epoch protection)
- No manual epoch management required
- Can call async methods safely

---

### 8.4 UnsafeContext (Performance)

**Location:** `cs/src/core/ClientSession/UnsafeContext.cs`

**Usage:**
```csharp
session.UnsafeContext.BeginUnsafe();
try {
    var status = session.UnsafeContext.Read(ref key, ref input, ref output, userContext, serialNo);
    // More operations...
} finally {
    session.UnsafeContext.EndUnsafe();
}
```

**Manual Epoch Control:**
```csharp
public void BeginUnsafe() => clientSession.UnsafeResumeThread();
public void EndUnsafe() => clientSession.UnsafeSuspendThread();

internal Status Read(ref Key key, ...)
{
    Debug.Assert(clientSession.fht.epoch.ThisInstanceProtected());  // Must be in epoch
    return clientSession.InternalRead(ref key, ...);
}
```

**Constraints:**
- **Cannot call async methods** while in unsafe context (must `EndUnsafe()` first)
- Faster (no per-operation epoch overhead)
- Risk: Forgetting `EndUnsafe()` blocks reclamation

---

### 8.5 LockableContext (Transactions)

**Location:** `cs/src/core/ClientSession/LockableContext.cs`

**Usage:**
```csharp
session.LockableContext.BeginLockable();
try {
    // Sort keys to prevent deadlock
    session.LockableContext.Lock(ref key1, LockType.Exclusive);
    session.LockableContext.Lock(ref key2, LockType.Exclusive);
    
    // Multi-key atomic operations
    session.LockableContext.Upsert(ref key1, ref value1);
    session.LockableContext.Upsert(ref key2, ref value2);
    
    // Unlock in reverse order
    session.LockableContext.Unlock(ref key2, LockType.Exclusive);
    session.LockableContext.Unlock(ref key1, LockType.Exclusive);
} finally {
    session.LockableContext.EndLockable();
}
```

**Lock Acquisition:**
```csharp
internal void AcquireLockable()
{
    while (true)
    {
        // Wait for checkpoint to complete (not in PREPARE phase)
        while (IsInPreparePhase()) { Thread.Yield(); }
        
        fht.IncrementNumLockingSessions();
        isAcquiredLockable = true;
        
        if (!IsInPreparePhase()) break;  // Success
        
        InternalReleaseLockable();
        Thread.Yield();
    }
}
```

**Deadlock Prevention:**
```csharp
// Must sort keys by hash before locking
if (session.LockableContext.NeedKeyHash) {
    session.LockableContext.SortKeyHashes<TLockableKey>(ref keys);
}
```

**Lock Types:**
```csharp
public enum LockType {
    None = 0,
    Shared = 1,     // Read lock
    Exclusive = 2   // Write lock
}
```

**Lock Tracking:**
```csharp
internal ulong sharedLockCount;
internal ulong exclusiveLockCount;

// EndLockable() enforces all locks released
if (TotalLockCount > 0)
    throw new FasterException("EndLockable called with locks held");
```

---

### 8.6 Thread Safety Guarantees

**Session-Level:**
- **Mono-threaded:** Cannot call methods on same session from multiple threads concurrently
- **Multi-session:** Multiple sessions can operate concurrently (different threads)

**Operation-Level:**

| Operation | BasicContext | UnsafeContext | LockableContext |
|-----------|--------------|---------------|-----------------|
| **Concurrent Reads** | Yes | Yes | Yes |
| **Concurrent Writes** | Yes (in mutable region) | Yes | Yes (with locks) |
| **Multi-Key Atomicity** | No | No | Yes (with locks) |
| **Async-Safe** | Yes | No (must EndUnsafe()) | Yes |

**Checkpoint Coordination:**
- `AcquireLockable()` waits for `IsInPreparePhase()` to complete
- Prevents lock acquisition during checkpoint PREPARE phase
- Ensures checkpoints see consistent lock states

---

## 9. User-Facing API Ergonomics

### 9.1 What Makes It Pleasant

**1. SimpleFunctions for Simple Cases:**
```csharp
var funcs = new SimpleFunctions<long, long>((a, b) => a + b);
var session = store.NewSession(funcs);
```
- Lambda-based merging
- Reduces boilerplate for common patterns

**2. Status-Based Error Handling:**
```csharp
var status = session.Read(ref key, ref output);
if (status.Found) { /* success */ }
else if (status.NotFound) { /* not found */ }
else if (status.IsPending) { /* async completion needed */ }
```
- No exceptions on hot path
- Clear operation results

**3. IDisposable Pattern:**
```csharp
using var session = store.NewSession(funcs);
using var iterator = log.Scan(...);
```
- Resource cleanup automatic
- RAII-style lifecycle

**4. Direct Device Control:**
```csharp
var logDevice = Devices.CreateLogDevice("/data/log");
var settings = new FasterKVSettings<K, V> { LogDevice = logDevice };
```
- Fine-grained control over storage
- Pluggable device implementations

**5. Clear Memory Control:**
```csharp
store.Log.FlushAndEvict(true);
store.Log.DisposeFromMemory();
```
- Explicit memory management
- Predictable resource usage

---

### 9.2 What Makes It Awkward

**1. Ref-Heavy API (Unfamiliar to Most C# Developers):**
```csharp
session.Upsert(ref key, ref value);  // Not: session.Upsert(key, value)
session.Read(ref key, ref input, ref output, context, serialNo);
```
- Verbose and unusual for C#
- Easy to forget `ref` keyword

**2. Complex IFunctions Interface (19 Methods):**
```csharp
public class MyFunctions : FunctionsBase<K, V, I, O, C>
{
    // Must understand: SingleReader vs ConcurrentReader
    // InitialUpdater vs CopyUpdater vs InPlaceUpdater
    // When is each called? Unclear from signatures alone.
}
```
- Large surface area
- Difficult to know which methods to override
- No clear guidance on callback semantics

**3. Generic Parameter Hell:**
```csharp
ClientSession<Key, Value, Input, Output, Context, Functions>
```
- Five type parameters for custom scenarios
- Type inference rarely works
- Verbose type declarations

**4. Mixed Async Patterns:**
```csharp
// Async/await for slow path
var result = await session.ReadAsync(ref key, ref input, context, serialNo, token);

// But also manual completion
if (result.Status.IsPending)
    result = await result.CompleteAsync(token);

// And callback-based for completions
public override void ReadCompletionCallback(ref Key key, ...) { /* handle */ }
```
- Three async patterns mixed (ValueTask, pending completion, callbacks)
- Confusing control flow

**5. Manual Pending Completion:**
```csharp
var status = session.Read(ref key, ref output);
if (status.IsPending) {
    session.CompletePendingWithOutputs(out var iter, true);
    while (iter.Next()) {
        // Process completed outputs
    }
    iter.Dispose();
}
```
- Verbose iterator pattern
- Easy to forget disposal

**6. Configuration Inconsistencies:**
```csharp
// High-level: object initializer
var settings = new FasterKVSettings<K, V>(path) { PageSizeBits = 12 };

// Low-level: positional parameters
var store = new FasterKV<K, V>(size, logSettings, checkpointSettings, serializerSettings, comparer);
```
- Two configuration APIs (not unified)
- Magic numbers (`PageSizeBits = 12` — no constants)

**7. Session Creation Ambiguity:**
```csharp
// Pattern 1
var session = store.NewSession(funcs);

// Pattern 2
var session = store.For<Input, Output, Context>().NewSession<Functions>(funcs);
```
- Two creation patterns (confusing)
- Unclear when to use each

---

### 9.3 API Maturity Assessment

| Category | Rating | Notes |
|----------|--------|-------|
| **Configuration** | 6/10 | Mixed high/low-level; magic numbers; inconsistent patterns |
| **Session Management** | 5/10 | Two creation APIs; no type safety on concurrency |
| **IFunctions** | 5/10 | Large interface; unclear semantics; steep learning curve |
| **Error Handling** | 7/10 | Status pattern good; async mixing awkward |
| **Type System** | 4/10 | Heavy `ref` parameters; 6 generic params; verbose |
| **Documentation** | 7/10 | Good samples; API docs adequate; patterns underexplained |
| **Overall** | **5.7/10** | Powerful but requires expertise; not beginner-friendly |

---

### 9.4 Rust API Opportunities

**Improvements for Rust:**

1. **Replace `ref` with Ownership:**
```rust
// Instead of: session.read(&mut key, &mut input, &mut output, ctx, serial_no)
let output = session.read(&key, &input, ctx, serial_no)?;
```

2. **Trait-Based Callbacks (Not 19 Methods):**
```rust
trait FasterFunctions<K, V> {
    fn read(&self, key: &K, value: &V) -> V::Output;
    fn upsert(&self, key: &K, input: &V::Input, old_value: Option<&V>) -> V;
    fn rmw(&self, key: &K, input: &V::Input, value: &mut V) -> RmwResult;
    // Fewer, clearer methods with default trait impls
}
```

3. **Unified Configuration:**
```rust
let faster = FasterKV::builder()
    .index_size(1 << 26)
    .memory_size(16 * GB)
    .page_size(32 * MB)
    .log_device(LocalDevice::new("/data/log"))
    .build()?;
```

4. **Type-Safe Contexts:**
```rust
// Instead of generic contexts, use Rust's type system
let basic = session.basic();  // Returns BasicContext<'a, K, V>
let unsafe_ = session.unsafe_();  // Returns UnsafeContext<'a, K, V> (unsafe fn)
```

5. **Single Async Pattern:**
```rust
// No mixed callbacks/ValueTask/CompletePending
let output = session.read(&key, &input).await?;
// Just async/await (Tokio-compatible)
```

6. **Compile-Time Session Safety:**
```rust
// Sessions bound to threads via !Send marker
// Prevents accidental cross-thread usage at compile time
```

---

## 10. Key Patterns for Rust Translation

### 10.1 IFunctions → Rust Trait Design

**C# Pattern:**
```csharp
public interface IFunctions<Key, Value, Input, Output, Context> {
    bool SingleReader(ref Key key, ref Input input, ref Value value, ref Output dst, ...);
    bool ConcurrentReader(ref Key key, ref Input input, ref Value value, ref Output dst, ...);
    // ... 17 more methods
}
```

**Rust Translation Options:**

**Option A: Multiple Traits (Compositional):**
```rust
pub trait ReadFunctions<K, V> {
    type Input;
    type Output;
    
    fn read(&self, key: &K, value: &V, input: &Self::Input) -> Self::Output;
}

pub trait UpsertFunctions<K, V> {
    type Input;
    
    fn upsert(&self, key: &K, old: Option<&V>, input: &Self::Input) -> V;
}

pub trait RmwFunctions<K, V> {
    type Input;
    
    fn rmw(&self, key: &K, value: &mut V, input: &Self::Input) -> RmwResult;
}

// User composes
impl ReadFunctions<u64, u64> for MyFuncs { ... }
impl UpsertFunctions<u64, u64> for MyFuncs { ... }
impl RmwFunctions<u64, u64> for MyFuncs { ... }
```

**Option B: Single Trait with Default Impls:**
```rust
pub trait FasterFunctions<K, V> {
    type Input;
    type Output;
    type Context;
    
    fn read(&self, key: &K, value: &V, input: &Self::Input) -> Self::Output {
        // Default: clone value
        value.clone().into()
    }
    
    fn upsert(&self, key: &K, old: Option<&V>, input: &Self::Input) -> V {
        // Default: use input as new value
        input.clone().into()
    }
    
    fn rmw(&self, key: &K, value: &mut V, input: &Self::Input) -> RmwResult {
        // Default: replace value
        *value = input.clone().into();
        RmwResult::Updated
    }
}
```

**Recommendation:** Option B with default impls. Simpler than 19 methods; users override only what they need.

---

### 10.2 Generic Type Parameters

**C# Approach:**
```csharp
FasterKV<Key, Value>
ClientSession<Key, Value, Input, Output, Context, Functions>
IFunctions<Key, Value, Input, Output, Context>
```

**Rust Approach (Reduced Generics):**
```rust
pub struct FasterKV<K, V> { ... }

pub struct Session<'a, K, V, F>
where
    F: FasterFunctions<K, V>,
{
    store: &'a FasterKV<K, V>,
    functions: F,
    // Context is F::Context (associated type, not generic param)
}

pub trait FasterFunctions<K, V> {
    type Input;
    type Output;
    type Context;  // Associated type (not param)
    // ...
}
```

**Benefits:**
- Fewer generic parameters (3 instead of 6)
- Type inference works better
- Associated types hide complexity

---

### 10.3 Disposable Pattern → Drop

**C# Pattern:**
```csharp
using var session = store.NewSession(funcs);
// Auto-disposed: CompletePending, release epoch, cleanup
```

**Rust Translation:**
```rust
impl<'a, K, V, F> Drop for Session<'a, K, V, F> {
    fn drop(&mut self) {
        self.complete_pending_sync();  // Flush pending ops
        self.store.epoch.suspend();     // Exit epoch
        self.store.release_session(self.id);
    }
}

// Usage
{
    let session = store.new_session(funcs);
    // ...
}  // Auto-dropped
```

**Challenge:** Async drop not supported in Rust (RFC pending). Must provide explicit async cleanup:
```rust
impl Session {
    pub async fn close(self) -> Result<()> {
        self.complete_pending_async().await?;
        // Then drop
        Ok(())
    }
}
```

---

### 10.4 GC Lifetimes → Rust Ownership

**C# Pattern (GC Hides Lifetimes):**
```csharp
public Status Read(ref Key key, ref Input input, ref Output output, ...)
{
    // Key, input, output references can live arbitrarily long (GC manages)
    return InternalRead(ref key, ref input, ref output, ...);
}
```

**Rust Translation (Explicit Lifetimes):**
```rust
pub fn read<'a>(
    &'a self,
    key: &'a K,
    input: &'a F::Input,
) -> Result<F::Output>
where
    F::Output: 'a,  // Output tied to session lifetime
{
    // Borrow checker enforces:
    // - key/input valid during call
    // - output cannot outlive session
    self.internal_read(key, input)
}
```

**Pinning for Async:**
```rust
// C# uses GC to prevent premature collection during async
// Rust uses Pin<Box<T>> for async state

pub async fn read_async<'a>(
    &'a self,
    key: &'a K,
    input: &'a F::Input,
) -> Result<F::Output> {
    let mut state = Box::pin(ReadAsyncState { key, input, ... });
    poll_fn(|cx| self.poll_read(&mut state, cx)).await
}
```

---

### 10.5 Ref Parameters → Rust Borrowing

**C# Pattern (Ref for Zero-Copy):**
```csharp
public Status Upsert(ref Key key, ref Value value, Context ctx, long serialNo)
{
    // ref allows zero-copy pass-by-reference
    return InternalUpsert(ref key, ref value, ctx, serialNo);
}
```

**Rust Translation (Natural Borrowing):**
```rust
pub fn upsert(&self, key: &K, value: &V, ctx: F::Context, serial_no: u64) -> Status {
    // Rust borrows are zero-copy by default
    self.internal_upsert(key, value, ctx, serial_no)
}

// For mutable: &mut V
pub fn rmw(&self, key: &K, input: &F::Input, ctx: F::Context) -> Status {
    // Internal can modify value in-place
    self.internal_rmw(key, input, ctx)
}
```

**Benefit:** No `ref` keyword pollution; borrowing is default.

---

### 10.6 Epoch Protection in Rust

**C# Pattern:**
```csharp
epoch.Resume();  // Enter protected region
try {
    // Operations safe from reclamation
} finally {
    epoch.Suspend();  // Exit
}
```

**Rust Translation (RAII Guard):**
```rust
pub struct EpochGuard<'a> {
    epoch: &'a LightEpoch,
}

impl<'a> Drop for EpochGuard<'a> {
    fn drop(&mut self) {
        self.epoch.suspend();
    }
}

impl LightEpoch {
    pub fn resume(&self) -> EpochGuard<'_> {
        self.acquire();
        EpochGuard { epoch: self }
    }
}

// Usage
{
    let _guard = epoch.resume();
    // Protected region
}  // Auto-suspended on drop
```

**Benefit:** Cannot forget to suspend (compile error if not dropped).

---

### 10.7 Async Context Passing

**C# Pattern:**
```csharp
// User context passed through async pipeline
var result = await session.ReadAsync(ref key, ref input, userContext, serialNo, token);
// userContext arrives in ReadCompletionCallback
```

**Rust Translation (Closure Capture):**
```rust
// User context captured in async closure
let user_ctx = MyContext { ... };
let result = session.read_async(&key, &input).await?;

// Callback via async block closure
async {
    let output = session.read_async(&key, &input).await?;
    user_ctx.handle(output);  // Captured context
}
```

**Or Explicit Context:**
```rust
pub async fn read_async_with_ctx<Ctx>(
    &self,
    key: &K,
    input: &F::Input,
    ctx: Ctx,
) -> Result<(F::Output, Ctx)> {
    // Returns context along with output
    let output = self.internal_read_async(key, input).await?;
    Ok((output, ctx))
}
```

---

### 10.8 Thread-Local Storage

**C# Pattern:**
```csharp
[ThreadStatic]
private static int threadId;

[ThreadStatic]
private static int threadEntryIndex;
```

**Rust Translation:**
```rust
thread_local! {
    static THREAD_ID: Cell<u32> = Cell::new(0);
    static ENTRY_INDEX: Cell<usize> = Cell::new(0);
}

// Access
THREAD_ID.with(|id| id.get());
ENTRY_INDEX.with(|idx| idx.set(42));
```

**Or with `thread_local` crate:**
```rust
use thread_local::ThreadLocal;

pub struct LightEpoch {
    entries: Vec<EpochEntry>,
    thread_data: ThreadLocal<ThreadEpochData>,
}
```

---

### 10.9 Callback-Based I/O → Async Rust

**C# Pattern:**
```csharp
device.ReadAsync(segmentId, offset, buffer, readLength, callback, context);
// callback(errorCode, numBytes, context) fires on completion
```

**Rust Translation (Async/Await):**
```rust
pub trait Device {
    async fn read(
        &self,
        segment_id: u32,
        offset: u64,
        buffer: &mut [u8],
    ) -> io::Result<usize>;
    
    async fn write(
        &self,
        segment_id: u32,
        offset: u64,
        buffer: &[u8],
    ) -> io::Result<usize>;
}

// Implementation (Tokio)
impl Device for LocalDevice {
    async fn read(&self, segment_id: u32, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let file = self.get_segment_file(segment_id).await?;
        file.read_at(buffer, offset).await
    }
}
```

**Benefit:** Native async/await (no callback indirection).

---

### 10.10 Atomic Operations

**C# Pattern:**
```csharp
Interlocked.CompareExchange(ref location, value, comparand);
Interlocked.Increment(ref location);
Thread.MemoryBarrier();
```

**Rust Translation:**
```rust
use std::sync::atomic::{AtomicU64, Ordering};

let location = AtomicU64::new(0);
location.compare_exchange(comparand, value, Ordering::SeqCst, Ordering::SeqCst)?;
location.fetch_add(1, Ordering::SeqCst);
std::sync::atomic::fence(Ordering::SeqCst);
```

**Ordering Choices:**
- `Ordering::SeqCst` — C# Interlocked equivalent (safest)
- `Ordering::AcqRel` — Acquire/release (lighter)
- `Ordering::Relaxed` — No ordering (fastest, careful)

---

## Conclusion

The C# FASTER implementation demonstrates a sophisticated **lock-free, epoch-protected, hybrid log architecture** suitable for high-performance key-value storage. Its design centers on:

1. **Callback-driven extensibility** (IFunctions)
2. **Session-based concurrency** (epoch protection, no intra-session parallelism)
3. **Three-phase log lifecycle** (mutable → read-only → disk)
4. **Device abstraction** (callback-based async I/O)
5. **Fuzzy checkpointing** (concurrent with operations)

**For Rust Translation:**
- **Simplify IFunctions** (fewer methods, default impls, associated types)
- **Leverage ownership** (replace `ref` parameters, lifetime safety)
- **Unify async** (async/await only, no callbacks)
- **Improve ergonomics** (builder pattern, RAII guards, compile-time safety)
- **Preserve core algorithms** (epoch protection, hybrid log, hash index)

The Rust implementation can **improve usability** while maintaining the **performance and correctness** of the C# original by using Rust's type system and ownership model to encode invariants at compile time.

---

**End of Analysis**
