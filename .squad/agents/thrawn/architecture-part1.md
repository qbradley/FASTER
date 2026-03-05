# Rust FASTER Architecture — Part 1 of 3

**Author:** Thrawn (Lead / System Architect)
**Date:** 2026-03-05
**Status:** North-Star Architecture Document
**Scope:** Sections 1–5 (Vision, Crate Structure, Core Data Structures, Concurrency, Storage)

---

## 1. Vision & Scope

### 1.1 What We Are Building

Rust FASTER KV is a production-grade, concurrent, durable key-value store built on the hybrid log architecture pioneered by Microsoft Research. The system provides:

- **Latch-free concurrent hash index** with epoch-based memory reclamation
- **Hybrid log** spanning DRAM and persistent storage as a unified address space
- **In-place mutable updates** in the hot region, with automatic copy-on-write for cold data
- **Concurrent Prefix Recovery (CPR)** checkpointing — non-blocking, consistent snapshots while the system continues serving reads and writes
- **Read-Modify-Write (RMW)** as a first-class primitive, not a client-side read+write
- **Session-based concurrency model** where each session is single-threaded, and cross-session safety is provided by epochs — not locks

This is not a wrapper around the C++ or C# implementations. It is a ground-up Rust implementation that preserves the algorithms and behavioral contracts of FASTER while leveraging Rust's ownership model, type system, and zero-cost abstractions to achieve safety guarantees that neither the C++ nor C# implementations can provide.

### 1.2 What We Are NOT Building in v1

The following are explicitly **out of scope** for v1. The architecture must remain extensible toward them, but they must not influence v1 API shape or introduce complexity:

| Feature | Rationale for Deferral |
|---------|----------------------|
| **FasterLog** (standalone append-only log) | Separate use case (~120KB in C#). Can be built atop the same allocator/device abstractions later. |
| **F2 Two-Tier Storage** (hot/cold stores + ColdIndex) | Complex coordination (~5 files in C++). Requires nested async index lookups. Design traits to accommodate it. |
| **Remote Server** (TCP/gRPC/WebSocket) | Large surface area (~80 files in C#). Orthogonal to core KV. |
| **Distributed Recovery (DPR)** | Research-grade feature. Uncertain production demand. |
| **Incremental Snapshots** (delta log) | C#-only feature. Valuable but not MVP-critical. |
| **Read Cache** | Important for read-heavy workloads but adds MSB-tag complexity. Phase 2. |
| **ColdIndex** (disk-based 2-level index) | F2 prerequisite. Deferred with F2. |

### 1.3 Target Users

Systems programmers building mission-critical infrastructure:

- **Database storage engines** that need an embedded, concurrent, durable KV layer
- **Caching tiers** that spill to SSD when DRAM is insufficient
- **Stream processing systems** that maintain large state tables with checkpoint/recovery
- **Cloud-native services** at planetary scale (millions of keys, terabytes of data, thousands of concurrent sessions)

These users expect: zero undefined behavior, predictable latency, crash-consistent durability, and the ability to integrate with their chosen async runtime (or no runtime at all).

### 1.4 Quality Bar

**Correctness first, then performance.** Specific quality requirements:

1. **Zero undefined behavior.** All `unsafe` blocks must have documented safety invariants. Miri-clean where feasible. Extensive property-based testing.
2. **No data loss.** Checkpoint/recovery must be crash-consistent. A completed checkpoint must survive power loss. This is non-negotiable for planetary-scale cloud workloads.
3. **No silent corruption.** Checksums on checkpoint metadata. Validation on recovery. Fail loudly.
4. **Performance target:** Match C++ FASTER throughput within 10% on equivalent hardware. This is achievable — Rust's ownership model eliminates the deep-copy overhead that plagues C++'s async context allocation (see §4.4).
5. **Deterministic resource usage.** No unbounded allocations. Memory consumption must be predictable from configuration parameters (page size, memory size, hash table size).
6. **Graceful degradation.** When disk I/O is the bottleneck, the system should back-pressure allocation rather than OOM.

### 1.5 Relationship to C++ and C# Implementations

**Behavioral compatibility, not format compatibility.**

The Rust implementation must produce identical observable behavior for the same sequence of operations against the same logical state. Specifically:

- **Same CRUD semantics:** Read, Upsert, RMW, and Delete must follow the exact behavioral contracts documented in the cross-implementation analysis (§3.1 of Cassian's spec). Upsert never goes pending. Delete never goes pending. Read and RMW go pending only when the record is on disk.
- **Same checkpoint guarantees:** CPR with fold-over and snapshot modes. Per-session commit points with exclusion lists. Recovery restores consistent state.
- **Same epoch-based coordination:** Phase transitions (REST → PREPARE → IN_PROGRESS → WAIT_FLUSH → PERSISTENCE_CALLBACK → REST) must follow the same state machine.
- **Same hash index semantics:** 48-bit logical addresses, tag-based collision filtering, overflow bucket chaining.

**Not required:**

- Binary-compatible on-disk format. Rust will define its own serialization for checkpoint metadata (likely using a structured binary format, not C#'s text-based `info.dat` or C++'s raw struct dumps). Cross-language checkpoint migration is a non-goal.
- API-compatible function signatures. Rust's trait system and ownership model demand a different API shape (see Dooku's recommendations: ~5 trait methods vs C#'s 19, associated types vs 6 generic parameters, RAII epoch guards vs manual Resume/Suspend).
- Thread-ID ceilings. C++ hard-caps at 96 threads. Rust should support configurable limits with a higher default (e.g., 256), backed by the same atomic-reservation algorithm.

### 1.6 Future Extensibility Toward Tsavorite/Garnet

Tsavorite is Microsoft's fork of C# FASTER, extended for the Garnet Redis-compatible cache-store. While not a v1 target, our architecture must not close doors:

- **Trait-based extensibility:** `IndexProvider`, `LogProvider`, `DeviceProvider` traits must be abstract enough that a Tsavorite-style two-level index or object log could implement them.
- **Record metadata extensibility:** The `RecordInfo` header reserves bits for future use. The trait system should allow custom metadata without changing the core record format.
- **Pluggable serialization:** Garnet stores Redis-compatible data types. The `Key`/`Value` trait bounds must support variable-length, schema-aware serialization — not just `Copy` types.
- **Transaction support hooks:** Tsavorite adds manual locking contexts. Our session model should have extension points for lock-table integration without core changes.
- **No premature abstraction:** We do NOT add Tsavorite features. We ensure the trait boundaries are in the right places so that a future `faster-tsavorite` crate could extend the system without forking `faster-core`.

---

## 2. Crate Structure

### 2.1 Workspace Layout

```
faster-rs/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── faster-core/              # Core KV engine (ZERO async deps)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── index/            # Hash index, bucket, overflow allocator
│   │       ├── log/              # Hybrid log, page allocator, record format
│   │       ├── epoch/            # Epoch-based reclamation
│   │       ├── session/          # Session, execution context, pending ops
│   │       ├── checkpoint/       # CPR state machine, metadata, recovery
│   │       ├── ops/              # Read, Upsert, RMW, Delete internal logic
│   │       ├── alloc/            # Arena allocator, aligned buffers, overflow bucket pool
│   │       └── util/             # Address, hash, status codes, config
│   │
│   ├── faster-device/            # Device trait + built-in implementations
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # Device trait definition
│   │       ├── null.rs           # NullDevice (testing, benchmarks)
│   │       ├── file.rs           # Synchronous file I/O (portable baseline)
│   │       ├── io_uring.rs       # Linux io_uring (feature-gated)
│   │       ├── direct_io.rs      # O_DIRECT support (Linux)
│   │       ├── buffer.rs         # Aligned buffer pool for I/O
│   │       └── segmented.rs      # Segmented file management (log.0, log.1, ...)
│   │
│   ├── faster-async-tokio/       # Tokio async adapter
│   │   ├── Cargo.toml            # Depends on: faster-core, faster-device, tokio
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── session.rs        # AsyncSession wrapping core Session
│   │       ├── device.rs         # Tokio-backed async Device impl
│   │       └── runtime.rs        # Runtime integration helpers
│   │
│   ├── faster-async-compio/      # compio async adapter (io_uring-native)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   │
│   ├── faster-async-monoio/      # monoio adapter (thread-per-core)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   │
│   ├── faster-ffi/               # C FFI bindings
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       └── c_api.rs          # extern "C" functions
│   │
│   └── faster-bench/             # Benchmarks and YCSB workload generator
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           └── ycsb.rs           # YCSB-compatible benchmark harness
│
├── tests/                        # Integration tests
│   ├── correctness/              # Deterministic correctness tests
│   ├── recovery/                 # Crash-recovery tests (checkpoint + kill + recover)
│   └── concurrency/              # Multi-threaded stress tests
│
└── examples/
    ├── basic_kv.rs               # Simple in-memory KV usage
    ├── persistent.rs             # Disk-backed with checkpointing
    └── tokio_integration.rs      # Async usage with Tokio
```

### 2.2 Crate Boundaries and Dependency Rules

The crate structure enforces a strict layering discipline:

```
                    ┌─────────────────────┐
                    │   faster-bench      │
                    └────────┬────────────┘
                             │
            ┌────────────────┼────────────────┐
            │                │                │
   ┌────────▼──────┐  ┌─────▼──────┐  ┌──────▼─────────┐
   │ faster-async- │  │faster-async│  │ faster-async-  │
   │    tokio      │  │  -compio   │  │    monoio      │
   └───────┬───────┘  └─────┬──────┘  └──────┬─────────┘
           │                │                 │
           └────────────────┼─────────────────┘
                            │
                    ┌───────▼───────┐
                    │  faster-core  │ ← NO async deps. NO runtime deps.
                    └───────┬───────┘
                            │ (uses trait, not impl)
                    ┌───────▼───────┐
                    │ faster-device │ ← Device trait + sync impls
                    └───────────────┘
```

**Critical rules:**

1. **`faster-core` has ZERO async runtime dependencies.** No `tokio`, no `async-std`, no `futures`. It may use `std` threading primitives (`thread`, `atomic`, `Mutex`, `Condvar`) and nothing more.
2. **`faster-core` depends on `faster-device` for the `Device` trait only.** The trait is defined in `faster-device` and is completion-callback-based (see §5.1). `faster-core` never instantiates a concrete device — that's the caller's responsibility.
3. **Async adapter crates are thin translation layers.** They wrap `faster-core`'s callback-based API in `Future`/`async fn` syntax specific to one runtime. Each adapter is ~500–1000 LOC. They implement the `Device` trait using their runtime's I/O primitives.
4. **`faster-ffi` depends on `faster-core` + `faster-device`.** It exposes a C ABI for embedding in other languages. It may bundle a synchronous `Device` implementation for simplicity.
5. **No circular dependencies.** The graph is a strict DAG.

### 2.3 The `faster-core` Crate: Dependency Budget

`faster-core` is the fortress. Its `Cargo.toml` dependencies are limited to:

| Crate | Purpose | Justification |
|-------|---------|---------------|
| **None (std only)** | Atomics, threads, alloc | Core must work with only `std`. |
| `crossbeam-utils` | `CachePadded<T>` for cache-line alignment | Tiny crate, no runtime. Avoids false sharing in epoch table and hash buckets. Could vendor if needed. |
| `cfg-if` | Conditional compilation helpers | Zero-cost, build-time only. |

**Explicitly forbidden in `faster-core`:**
- `tokio`, `async-std`, `smol`, `compio`, `monoio` — any async runtime
- `futures`, `pin-project` — any async machinery
- `serde` — serialization is handled via traits, not serde. Serde can be feature-gated in adapter crates.
- `parking_lot` — we use `std::sync` only. `parking_lot` is acceptable in adapter crates.

**Rationale:** The core must compile to a pure synchronous library that can be linked into any context — bare-metal, WASM (future), or exotic async runtimes that don't exist yet. Every external dependency is a potential portability hazard.

### 2.4 Feature Flags Strategy

```toml
# faster-core/Cargo.toml
[features]
default = []
# Enable runtime assertions and extra validation (debug builds always enable this)
paranoid = []
# Enable metrics collection (counters for ops, cache hits, disk reads)
metrics = []
# Enable tracing integration (tracing crate spans for operations)
tracing = ["dep:tracing"]

# faster-device/Cargo.toml
[features]
default = ["file-io"]
file-io = []           # Synchronous file I/O (always available)
io-uring = ["dep:io-uring"]  # Linux io_uring support
direct-io = []         # O_DIRECT bypass (Linux)

# faster-async-tokio/Cargo.toml
[features]
default = ["rt-multi-thread"]
rt-multi-thread = ["tokio/rt-multi-thread"]
io-uring = ["tokio-uring"]
```

**Design principle:** Feature flags control *implementations*, not *interfaces*. The `Device` trait is always the same regardless of features. Features select which concrete `Device` implementations are available.

### 2.5 Estimated Crate Sizes

| Crate | Estimated LOC | Complexity |
|-------|--------------|------------|
| `faster-core` | 8,000–12,000 | High — all algorithms live here |
| `faster-device` | 1,500–2,500 | Medium — trait + sync impls + io_uring |
| `faster-async-tokio` | 500–1,000 | Low — thin adapter |
| `faster-async-compio` | 500–1,000 | Low — thin adapter |
| `faster-async-monoio` | 500–1,000 | Low — thin adapter |
| `faster-ffi` | 500–800 | Low — C ABI surface |
| `faster-bench` | 1,000–1,500 | Medium — YCSB + custom benchmarks |
| **Total** | ~14,000–20,000 | |

---


## 3. Core Data Structures

### 3.1 Hash Index: Latch-Free Concurrent Hash Table

The hash index is the primary lookup structure. It maps key hashes to logical addresses in the hybrid log. The design must be latch-free (no locks on the read/write path), cache-friendly (bucket fits in one cache line), and support concurrent insert/update via CAS.

#### 3.1.1 Hash Bucket Entry (8 bytes)

Both C++ and C# encode a hash bucket entry into a single 64-bit word. This is essential — it enables atomic CAS operations on the entry without any locking.

**Bit layout:**

```
  63    62    61     60..48          47..0
┌─────┬─────┬─────┬──────────────┬───────────────────────────┐
│  0  │  0  │ Ten │   Tag (14)   │   Address (48)            │
└─────┴─────┴─────┴──────────────┴───────────────────────────┘
        │      │       │                   │
        │      │       │                   └─ Logical address in hybrid log
        │      │       └─ Hash fingerprint for fast rejection
        │      └─ Tentative bit: entry is being inserted (not yet committed)
        └─ Reserved for read-cache bit (v2)
```

**Rust representation:**

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct HashBucketEntry(u64);

impl HashBucketEntry {
    const ADDRESS_BITS: u32 = 48;
    const TAG_BITS: u32 = 14;
    const TENTATIVE_BIT: u64 = 1 << 61;
    const ADDRESS_MASK: u64 = (1u64 << 48) - 1;
    const TAG_MASK: u64 = ((1u64 << 14) - 1) << 48;

    const INVALID: Self = Self(0);

    #[inline(always)]
    fn new(address: LogicalAddress, tag: u16) -> Self {
        debug_assert!(address.0 < (1u64 << 48));
        debug_assert!(tag < (1u16 << 14));
        Self(address.0 | ((tag as u64) << 48))
    }

    #[inline(always)]
    fn address(self) -> LogicalAddress {
        LogicalAddress(self.0 & Self::ADDRESS_MASK)
    }

    #[inline(always)]
    fn tag(self) -> u16 {
        ((self.0 & Self::TAG_MASK) >> 48) as u16
    }

    #[inline(always)]
    fn is_tentative(self) -> bool {
        (self.0 & Self::TENTATIVE_BIT) != 0
    }

    #[inline(always)]
    fn with_tentative(self) -> Self {
        Self(self.0 | Self::TENTATIVE_BIT)
    }

    #[inline(always)]
    fn without_tentative(self) -> Self {
        Self(self.0 & !Self::TENTATIVE_BIT)
    }
}
```

**Design decisions:**

- **`#[repr(transparent)]`** ensures `HashBucketEntry` has the same layout as `u64`, allowing safe transmutation to/from `AtomicU64`.
- **14-bit tag** provides 1-in-16384 false positive rate per bucket slot. The tag is extracted from the upper bits of the key's hash. This means most hash collisions (same bucket, different tag) are filtered without dereferencing the log address — critical for cache performance.
- **Tentative bit** is used during two-phase insert: (1) CAS entry with tentative set, (2) write record to log, (3) CAS entry without tentative. If a thread crashes between steps 1 and 2, other threads skip tentative entries during lookup.
- **48-bit address** is sufficient for 256 TB of addressable log space (2^48 bytes if byte-addressed, or 2^48 × alignment if slot-addressed). We use page+offset encoding (see §3.4).

#### 3.1.2 Hash Bucket (64 bytes = 1 cache line)

```rust
const BUCKET_ENTRIES: usize = 7;
const OVERFLOW_ENTRY: usize = 7; // 8th slot is overflow pointer

#[repr(C, align(64))]
struct HashBucket {
    entries: [AtomicU64; 8], // 7 data entries + 1 overflow pointer
}
```

**Why 7+1?** A cache line is 64 bytes. Each entry is 8 bytes. That gives us 8 slots. We use 7 for data entries and 1 for the overflow chain pointer. This matches both C++ and C# designs.

**Lookup algorithm:**

```
fn find_entry(bucket: &HashBucket, tag: u16) -> Option<HashBucketEntry> {
    // Scan 7 entries in this bucket
    for i in 0..BUCKET_ENTRIES {
        let entry = HashBucketEntry(bucket.entries[i].load(Ordering::Acquire));
        if entry.tag() == tag && !entry.is_tentative() && entry.address().is_valid() {
            return Some(entry);
        }
    }
    // Check overflow chain
    let overflow_addr = bucket.entries[OVERFLOW_ENTRY].load(Ordering::Acquire);
    if overflow_addr != 0 {
        // Follow overflow pointer to next bucket (allocated from overflow pool)
        let next_bucket: &HashBucket = unsafe { &*(overflow_addr as *const HashBucket) };
        return find_entry(next_bucket, tag); // recursive, but chains are short
    }
    None
}
```

**Rationale for overflow chains vs. open addressing:**
- Open addressing (Robin Hood, linear probing) would spread entries across cache lines, destroying locality.
- Overflow chains keep the primary bucket hot (one cache line) and spill to cold overflow buckets only when >7 keys hash to the same bucket. With a properly-sized table (load factor < 1.0), overflow is rare.
- C++ and C# both use this design. It is proven at scale.

#### 3.1.3 Overflow Bucket Allocator

Overflow buckets must be allocated from a pool, not the system allocator, for two reasons:
1. **Performance:** Overflow allocation happens on the insert hot path. System `malloc` is too slow.
2. **Determinism:** Overflow memory must be bounded and predictable.

```rust
struct OverflowBucketAllocator {
    // Pre-allocated chunks of 64-byte-aligned buckets
    chunks: Vec<Box<[HashBucket]>>,  // Each chunk is e.g. 1024 buckets
    // Lock-free free list (Treiber stack)
    free_list: AtomicPtr<HashBucket>,
    // Bump pointer for fresh allocation within current chunk
    next_offset: AtomicUsize,
    chunk_size: usize,
}
```

**Allocation:** Pop from free list (fast path) or bump-allocate from current chunk (slow path). If chunk exhausted, allocate new chunk from system allocator (rare path).

**Deallocation:** Push to free list during GC/compaction. Never returned to system allocator during normal operation.

**Memory ordering:** Free list operations use `Ordering::AcqRel` for CAS. Bump pointer uses `Ordering::Relaxed` with a follow-up `Ordering::Release` fence after initialization.

#### 3.1.4 Atomic Operations and Memory Ordering

The hash index is the most contention-sensitive data structure. Memory ordering must be precise:

| Operation | Ordering | Rationale |
|-----------|----------|-----------|
| **Read entry** (lookup) | `Acquire` | Must see the record that was written before the entry was published |
| **CAS entry** (insert/update) | `AcqRel` | Acquire to read current; Release to publish new entry+record |
| **Read overflow pointer** | `Acquire` | Must see fully-initialized overflow bucket |
| **Write overflow pointer** | `Release` | Overflow bucket must be fully initialized before pointer is visible |
| **Tentative bit set** | `AcqRel` | Claim slot atomically |
| **Tentative bit clear** | `Release` | Record is now committed; make visible |

**Why not `SeqCst` everywhere?** `SeqCst` imposes a total order across all atomic operations on all variables. This is unnecessary and expensive (full memory barrier on x86, `dmb ish` on ARM). The hash index only needs per-entry consistency: "if you see this address, the record at that address is valid." `Acquire`/`Release` provides exactly this guarantee with less overhead.

**Exception:** The epoch system (§4.1) does use `SeqCst` in specific places where a total ordering across thread-local epochs and the global epoch is required for safety.

#### 3.1.5 Hash Table Sizing and Growth

The hash table size must be a power of 2 (for fast modular arithmetic via bitmask). Bucket index is computed as:

```rust
fn bucket_index(hash: u64, size_mask: u64) -> usize {
    (hash & size_mask) as usize
}

fn tag_from_hash(hash: u64) -> u16 {
    ((hash >> 48) & 0x3FFF) as u16  // bits 48..61
}
```

**Initial sizing heuristic:** `num_buckets = expected_key_count / 4` (targeting ~57% load factor with 7 entries per bucket). The user provides this via the builder pattern.

**Growth** is handled by the grow state machine (§4.6). The table doubles in size, and entries are redistributed. Growth is a coordinated multi-phase operation across all threads, gated by the epoch system.

---

### 3.2 Record Format

Records are the fundamental unit of storage in the hybrid log. They must be readable without deserialization (zero-copy from mmap'd pages), support variable-length keys/values, and carry metadata for version chaining, tombstones, and checkpoint coordination.

#### 3.2.1 RecordInfo Header (8 bytes)

The first 8 bytes of every record encode metadata as a packed `u64`:

```
  63    62    61     60..48          47..0
┌─────┬─────┬─────┬──────────────┬───────────────────────────┐
│ Fin │ Tom │ Inv │ Version (13) │ Previous Address (48)     │
└─────┴─────┴─────┴──────────────┴───────────────────────────┘
```

```rust
#[derive(Clone, Copy)]
#[repr(transparent)]
struct RecordInfo(u64);

impl RecordInfo {
    const PREVIOUS_ADDR_MASK: u64 = (1u64 << 48) - 1;
    const VERSION_SHIFT: u32 = 48;
    const VERSION_MASK: u64 = 0x1FFF_0000_0000_0000; // 13 bits
    const INVALID_BIT: u64 = 1 << 61;
    const TOMBSTONE_BIT: u64 = 1 << 62;
    const FINAL_BIT: u64 = 1 << 63;

    fn previous_address(self) -> LogicalAddress {
        LogicalAddress(self.0 & Self::PREVIOUS_ADDR_MASK)
    }

    fn version(self) -> u16 {
        ((self.0 & Self::VERSION_MASK) >> Self::VERSION_SHIFT) as u16
    }

    fn is_invalid(self) -> bool { self.0 & Self::INVALID_BIT != 0 }
    fn is_tombstone(self) -> bool { self.0 & Self::TOMBSTONE_BIT != 0 }
}
```

**Field semantics:**
- **Previous Address (48 bits):** Points to the prior version of the same key in the log. Forms a reverse linked list (version chain). Used during Read to walk back to the correct version, and during CPR to preserve pre-checkpoint values.
- **Version (13 bits):** Checkpoint version when this record was written. Used during CPR to determine whether a record belongs to version `v` or `v+1`. Supports up to 8191 checkpoint cycles before wrapping (more than sufficient — wrapping is handled by modular comparison).
- **Invalid (1 bit):** Record has been sealed. Set when an in-place update in the mutable region is superseded by a newer record at the tail. Readers skip invalid records.
- **Tombstone (1 bit):** Logical deletion marker. The key was deleted. Readers treat this as "not found."
- **Final (1 bit):** Reserved. Used during CPR for certain coordination scenarios. May be repurposed for read-cache flags in v2.

**Atomicity:** `RecordInfo` is 8 bytes, which means it can be read/written atomically on 64-bit platforms. When updating the invalid or tombstone bits in the mutable region, we use `AtomicU64::fetch_or` with `Ordering::Release`.

#### 3.2.2 Record Layout in Memory

```
Offset 0:   [ RecordInfo (8 bytes)                    ]
Offset 8:   [ padding to align(Key)                   ]
Offset K0:  [ Key data (key_size bytes)                ]
Offset K1:  [ padding to align(Value)                  ]
Offset V0:  [ Value data (value_size bytes)            ]
Offset V1:  [ padding to record alignment (8 bytes)    ]
            ← Total record size (multiple of 8)
```

**Alignment rules:**
- Keys are aligned to `align_of::<K>()` (but capped at 8 bytes for simplicity in v1)
- Values are aligned to `align_of::<V>()` (capped at 8 bytes)
- Total record size is rounded up to the next multiple of 8 bytes
- Records do NOT need cache-line alignment — they are packed sequentially in log pages

**Rationale for 8-byte alignment cap:** C++ uses up to 64-byte alignment, which wastes enormous space for small keys/values. For v1, 8-byte alignment is sufficient for all primitive types and most structs. If a user needs wider alignment, they can pad their value type.

#### 3.2.3 Key and Value Traits

```rust
/// Trait for types that can be used as FASTER keys.
/// Keys must be immutable once written (the log never modifies a key).
pub trait FasterKey: Sized + Eq + Clone {
    /// Compute a 64-bit hash. Must be deterministic and well-distributed.
    fn hash(&self) -> u64;

    /// Serialized size in bytes.
    fn serialized_size(&self) -> usize;

    /// Write the key into a byte buffer at the given offset.
    /// # Safety
    /// Caller must ensure `dst` has at least `serialized_size()` bytes available.
    unsafe fn serialize_to(&self, dst: *mut u8);

    /// Read a key from a byte buffer.
    /// # Safety
    /// Caller must ensure `src` points to a valid serialized key.
    unsafe fn deserialize_from(src: *const u8) -> Self;
}

/// Trait for types that can be used as FASTER values.
pub trait FasterValue: Sized + Clone {
    fn serialized_size(&self) -> usize;
    unsafe fn serialize_to(&self, dst: *mut u8);
    unsafe fn deserialize_from(src: *const u8) -> Self;
}
```

**Why not just `Copy + Pod`?** We need to support variable-length keys (e.g., byte strings) and values (e.g., serialized protobufs). The trait-based approach allows:
- Fixed-size types: `serialized_size` returns a constant. `serialize_to`/`deserialize_from` are memcpy. Zero cost.
- Variable-length types: `serialized_size` reads a length prefix. Serialization writes length + data. Slightly more expensive but fully general.

**Blanket impl for `Copy` types** (convenience for the common case):

```rust
impl<T: Copy + Eq + Hash + Sized + 'static> FasterKey for T {
    fn hash(&self) -> u64 {
        // Use a high-quality hash (e.g., aHash or FxHash) for the default
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
    fn serialized_size(&self) -> usize { std::mem::size_of::<T>() }
    unsafe fn serialize_to(&self, dst: *mut u8) {
        std::ptr::copy_nonoverlapping(self as *const T as *const u8, dst, size_of::<T>());
    }
    unsafe fn deserialize_from(src: *const u8) -> Self {
        std::ptr::read(src as *const T)
    }
}
```

#### 3.2.4 Variable-Length Record Support

For variable-length keys/values, the record layout includes inline length prefixes:

```
Offset 0:   [ RecordInfo (8 bytes)                  ]
Offset 8:   [ key_len: u32 (4 bytes)                ]
Offset 12:  [ key_data (key_len bytes)               ]
Offset K:   [ padding to 4-byte alignment            ]
Offset V0:  [ value_len: u32 (4 bytes)              ]
Offset V0+4:[ value_data (value_len bytes)           ]
Offset V1:  [ padding to 8-byte alignment            ]
```

This matches the C++ `VarLenKey` design (inline, single allocation) rather than the C# `GenericAllocator` design (dual-log with separate object log). The single-allocation approach has better locality and simpler recovery.

---

### 3.3 Hybrid Log

The hybrid log is the central innovation of FASTER. It is a unified, append-only log that spans DRAM and persistent storage, with distinct regions that govern what operations are permitted.

#### 3.3.1 Memory Regions

```
┌─────────────────────────────────────────────────────────────────────┐
│                        HYBRID LOG                                   │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  [DISK]           [IN-MEMORY, READ-ONLY]       [IN-MEMORY, MUTABLE]│
│                                                                     │
│  BeginAddr  HeadAddr  SafeReadOnlyAddr  ReadOnlyAddr    TailAddr   │
│      ↓         ↓            ↓               ↓              ↓       │
│  ┌───────┬──────────┬───────────────┬──────────────┬──────────┐    │
│  │Evicted│ Closed/  │  Read-Only    │   Mutable    │ Alloc    │    │
│  │(disk  │ Flushed  │  (flushing    │  (in-place   │ frontier │    │
│  │ only) │ (disk+   │   to disk)    │   updates)   │          │    │
│  │       │  memory) │               │              │          │    │
│  └───────┴──────────┴───────────────┴──────────────┴──────────┘    │
│                                                                     │
│  Invariant: BeginAddr ≤ HeadAddr ≤ SafeReadOnlyAddr                │
│             ≤ ReadOnlyAddr ≤ TailAddr                              │
└─────────────────────────────────────────────────────────────────────┘
```

**Address semantics:**

| Address | Meaning | Who advances it |
|---------|---------|-----------------|
| **BeginAddress** | Earliest valid log address. Everything before is truncated. | GC / compaction |
| **HeadAddress** | Everything below is on disk only (not in memory). | Page eviction |
| **SafeHeadAddress** | Epoch-protected version of HeadAddress. Safe to evict pages below this after all threads pass the epoch. | Epoch drain callback |
| **SafeReadOnlyAddress** | Epoch-protected version of ReadOnlyAddress. All threads have observed the read-only transition. | Epoch drain callback |
| **ReadOnlyAddress** | Below this: read-only (no in-place updates). Above this: mutable. | Log shift when mutable region fills |
| **TailAddress** | Next allocation point. Monotonically increasing. | Atomic bump on every insert |

**Why two versions of some addresses (e.g., HeadAddress vs SafeHeadAddress)?**

The "safe" variants exist because address shifts are lazy — when we decide to move ReadOnlyAddress forward, some threads may still be in the middle of an in-place update to the region that's about to become read-only. The epoch system provides the bridge: we set `ReadOnlyAddress` immediately, then register an epoch drain callback. When all threads have exited the current epoch (meaning they've observed the new ReadOnlyAddress), the callback sets `SafeReadOnlyAddress` to match. Only then can we flush those pages to disk.

#### 3.3.2 Page Structure and Allocation

```rust
struct HybridLog<K: FasterKey, V: FasterValue> {
    // Page table: maps page index → page frame
    pages: Vec<AtomicPtr<Page>>,   // Circular buffer of page pointers
    num_pages: usize,              // Total in-memory pages (power of 2)
    page_size: usize,              // Bytes per page (e.g., 32 MB, power of 2)
    page_size_bits: u32,           // log2(page_size)

    // Address state (all atomic)
    tail_page_offset: AtomicU64,   // Packed: page_index (32) | offset (32)
    head_address: AtomicU64,
    safe_head_address: AtomicU64,
    read_only_address: AtomicU64,
    safe_read_only_address: AtomicU64,
    begin_address: AtomicU64,
    flushed_until_address: AtomicU64,

    // Per-page flush status
    page_status: Vec<PageStatus>,

    // Device for persistent storage
    device: Arc<dyn Device>,

    // Buffer pool for I/O operations
    buffer_pool: BufferPool,

    _marker: PhantomData<(K, V)>,
}

struct Page {
    data: [u8; PAGE_SIZE], // Raw byte storage for records
}

struct PageStatus {
    // Last address flushed to disk for this page
    last_flushed_until: AtomicU64,
    // Flush state: Open | Closed | Flushing | Flushed
    state: AtomicU32,
}
```

**Allocation (hot path):**

```rust
fn allocate(&self, record_size: u32) -> LogicalAddress {
    loop {
        let current = self.tail_page_offset.load(Ordering::Relaxed);
        let page = (current >> 32) as u32;
        let offset = current as u32;

        if offset + record_size <= self.page_size as u32 {
            // Fast path: bump offset within current page
            let new = ((page as u64) << 32) | ((offset + record_size) as u64);
            if self.tail_page_offset.compare_exchange_weak(
                current, new, Ordering::AcqRel, Ordering::Relaxed
            ).is_ok() {
                return LogicalAddress::new(page, offset);
            }
            // CAS failed — another thread allocated; retry
            continue;
        }

        // Slow path: page boundary — need new page
        self.allocate_new_page(page, record_size);
        // Retry from top (new page now active)
    }
}
```

**Page lifecycle:**
1. **Allocate:** Created when TailAddress crosses a page boundary. Zero-initialized.
2. **Fill:** Threads bump-allocate records within the page via atomic CAS on `tail_page_offset`.
3. **Seal:** When ReadOnlyAddress advances past this page, it becomes immutable.
4. **Flush:** Async write to device. Tracked via `PageStatus::last_flushed_until`.
5. **Evict:** When HeadAddress advances past this page, the in-memory frame is freed (returned to a page pool or dropped).
6. **Truncate:** When BeginAddress advances past this page's segment, the on-disk segment file is deleted.

**Page pool:** We maintain a pool of pre-allocated page frames to avoid allocation jitter. When a page is evicted, its frame returns to the pool. When a new page is needed, we pop from the pool. Pool size = `num_pages` (circular buffer reuse).

#### 3.3.3 Mutable Fraction and ReadOnly Shift Trigger

The mutable region is sized as a fraction of total in-memory log space:

```
mutable_size = total_memory_size * mutable_fraction    // default: 0.9
read_only_threshold = total_memory_size - mutable_size  // default: 0.1
```

When `TailAddress - ReadOnlyAddress > mutable_size`, the system shifts `ReadOnlyAddress` forward. This triggers:
1. Epoch bump with drain callback
2. Pages in the newly read-only region begin flushing to disk
3. After flush completes and HeadAddress can advance, old pages are evicted

**Why 90% mutable?** Most workloads update recent data. A large mutable region maximizes in-place update opportunities (avoiding copy-to-tail). The 10% read-only buffer provides time for disk flushes to complete before pages must be evicted. Tunable via configuration.

---

### 3.4 Address Space: Logical Addressing Scheme

#### 3.4.1 LogicalAddress Encoding

Every location in the hybrid log is identified by a `LogicalAddress`: a 48-bit value encoding a page index and an offset within that page.

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
struct LogicalAddress(u64);

impl LogicalAddress {
    const INVALID: Self = Self(0);
    const MIN_VALID: Self = Self(1); // C++ uses 1, not 0, as minimum valid

    // Encoding: lower `page_size_bits` = offset, upper bits = page
    #[inline(always)]
    fn new(page: u32, offset: u32) -> Self {
        Self(((page as u64) << PAGE_SIZE_BITS) | (offset as u64))
    }

    #[inline(always)]
    fn page(self) -> u32 {
        (self.0 >> PAGE_SIZE_BITS) as u32
    }

    #[inline(always)]
    fn offset(self) -> u32 {
        (self.0 & ((1u64 << PAGE_SIZE_BITS) - 1)) as u32
    }

    #[inline(always)]
    fn is_valid(self) -> bool {
        self.0 > 0
    }
}
```

**With default 25-bit page size (32 MB pages):**
- **Offset:** bits 0..24 (25 bits) → 0 to 33,554,431 bytes within a page
- **Page:** bits 25..47 (23 bits) → 0 to 8,388,607 pages
- **Total addressable:** 8,388,608 × 32 MB = **256 TB**

This matches the C++ implementation exactly. The 256 TB address space is sufficient for any realistic single-node deployment.

#### 3.4.2 Address → Physical Location Mapping

A logical address maps to a physical location depending on where it falls relative to the address boundaries:

```rust
fn resolve_address(&self, addr: LogicalAddress) -> AddressResolution {
    if addr >= self.tail_address() {
        AddressResolution::Invalid // Beyond allocation frontier
    } else if addr >= self.read_only_address() {
        // Mutable region: in-memory, direct pointer
        let page_frame = self.page_frame(addr.page());
        let ptr = unsafe { page_frame.add(addr.offset() as usize) };
        AddressResolution::Mutable(ptr)
    } else if addr >= self.head_address() {
        // Read-only or closed region: in-memory but immutable
        let page_frame = self.page_frame(addr.page());
        let ptr = unsafe { page_frame.add(addr.offset() as usize) };
        AddressResolution::ReadOnly(ptr)
    } else if addr >= self.begin_address() {
        // On-disk: must issue I/O
        AddressResolution::OnDisk {
            segment: addr.page() >> self.segment_size_shift,
            offset_in_segment: /* ... */,
        }
    } else {
        AddressResolution::Truncated // Below BeginAddress, data deleted
    }
}
```

**Circular buffer mapping:** In-memory pages use a circular buffer. The page frame for logical page `P` is:

```rust
fn page_frame(&self, logical_page: u32) -> *const u8 {
    let frame_index = (logical_page as usize) % self.num_pages;
    self.pages[frame_index].load(Ordering::Acquire) as *const u8
}
```

**Segment mapping for disk:** On-disk pages are organized into segments (default 1 GB = 32 pages of 32 MB). Segment `S` contains pages `S * pages_per_segment` through `(S+1) * pages_per_segment - 1`. Each segment is a separate file (`log.0`, `log.1`, ...) enabling efficient truncation (delete entire segment file).

#### 3.4.3 AtomicLogicalAddress

For addresses that are shared across threads (HeadAddress, TailAddress, etc.):

```rust
#[repr(transparent)]
struct AtomicLogicalAddress(AtomicU64);

impl AtomicLogicalAddress {
    fn load(&self, order: Ordering) -> LogicalAddress {
        LogicalAddress(self.0.load(order))
    }

    fn store(&self, addr: LogicalAddress, order: Ordering) {
        self.0.store(addr.0, order);
    }

    fn compare_exchange(
        &self,
        current: LogicalAddress,
        new: LogicalAddress,
        success: Ordering,
        failure: Ordering,
    ) -> Result<LogicalAddress, LogicalAddress> {
        self.0.compare_exchange(current.0, new.0, success, failure)
            .map(LogicalAddress)
            .map_err(LogicalAddress)
    }
}
```

---


## 4. Concurrency Architecture

### 4.1 Epoch-Based Reclamation

#### 4.1.1 Why Epochs, Not Locks

FASTER achieves latch-free read/write operations by replacing locks with epoch-based coordination. The insight: instead of preventing concurrent access to shared state, we ensure that *deferred actions* (page eviction, GC, checkpoint transitions) only execute when *no thread could be referencing* the affected state.

This is fundamentally different from reference counting (ARC) or hazard pointers:
- **Reference counting** requires an atomic increment/decrement on every access — unacceptable overhead on the hot path.
- **Hazard pointers** require per-thread publication of the protected address — complex and limits how many addresses can be protected simultaneously.
- **Epochs** require a single atomic write per epoch-protected critical section (store current epoch to thread-local slot) and an amortized scan to compute the safe-to-reclaim epoch. The per-operation overhead is ~1 atomic store.

#### 4.1.2 Custom Implementation vs. crossbeam-epoch

**Decision: Custom implementation.**

**Trade-off analysis:**

| Factor | crossbeam-epoch | Custom |
|--------|----------------|--------|
| **Correctness** | Battle-tested, widely used | Must be carefully validated |
| **API fit** | Designed for general GC; `Guard` protects individual pointers | FASTER needs epoch to gate *phase transitions* and *drain callbacks*, not pointer-level protection |
| **Drain list** | Not built in — crossbeam uses `defer()` for individual deferred destructors | FASTER needs `BumpCurrentEpoch(callback)` that fires when all threads pass an epoch boundary |
| **Phase coordination** | No concept of phases | FASTER's checkpoint state machine requires per-thread `phase_finished` tracking, tightly coupled to epoch advancement |
| **Thread table** | crossbeam uses a global linked list of `Participant` nodes | FASTER uses a fixed-size cache-line-aligned array (no allocation on the hot path) |
| **Performance** | General-purpose — crossbeam pins are ~3ns each | FASTER's epoch acquire is a single relaxed store to a known offset (< 1ns) |

**Conclusion:** crossbeam-epoch solves a different problem (safe deferred deallocation of individual objects). FASTER's epoch system is a *coordination mechanism* for multi-phase state machines. Trying to shoehorn FASTER's semantics into crossbeam's API would be more complex than building the ~300-line custom epoch system.

However, we do **use `crossbeam-utils::CachePadded`** for cache-line padding of epoch table entries. This is a data structure utility, not a runtime dependency.

#### 4.1.3 Epoch System Design

```rust
const MAX_THREADS: usize = 256; // Configurable at compile time

struct LightEpoch {
    // Global epoch counter. Monotonically increasing.
    current_epoch: CachePadded<AtomicU64>,
    // Safe-to-reclaim: no thread is referencing anything at or before this epoch.
    safe_to_reclaim_epoch: CachePadded<AtomicU64>,
    // Per-thread epoch entries (cache-line aligned to avoid false sharing)
    table: Box<[CachePadded<EpochEntry>; MAX_THREADS]>,
    // Drain list: deferred actions keyed by epoch
    drain_list: DrainList,
}

#[repr(C)]
struct EpochEntry {
    // The epoch this thread last observed. 0 = thread not active.
    local_current_epoch: AtomicU64,
    // Reentrance counter (nested Protect/Unprotect calls)
    reentrant: AtomicU32,
    // Per-phase completion markers (for checkpoint state machine)
    phase_finished: [AtomicBool; Phase::COUNT],
    // Thread ID occupying this slot (0 = free)
    thread_id: AtomicU64,
}
```

**Cache-line alignment is critical.** Without it, two threads writing to adjacent `EpochEntry` slots would contend on the same cache line (false sharing), destroying scalability. `CachePadded` from crossbeam-utils ensures each entry is 64-byte aligned.

**Workflow:**

```rust
impl LightEpoch {
    /// Enter an epoch-protected region. Must be paired with `unprotect()`.
    fn protect(&self, thread_index: usize) {
        let entry = &self.table[thread_index];
        let reentrant = entry.reentrant.fetch_add(1, Ordering::Relaxed);
        if reentrant == 0 {
            // First entry: publish current epoch to our slot
            let epoch = self.current_epoch.load(Ordering::Relaxed);
            entry.local_current_epoch.store(epoch, Ordering::Release);
        }
    }

    /// Exit an epoch-protected region.
    fn unprotect(&self, thread_index: usize) {
        let entry = &self.table[thread_index];
        let reentrant = entry.reentrant.fetch_sub(1, Ordering::Relaxed);
        if reentrant == 1 {
            // Last exit: clear our epoch slot
            entry.local_current_epoch.store(0, Ordering::Release);
            // Try to drain pending actions
            self.try_drain();
        }
    }

    /// Advance the global epoch. Queue `callback` to run when all threads
    /// have moved past the current epoch.
    fn bump_current_epoch<F: FnOnce() + Send + 'static>(&self, callback: F) {
        let prior_epoch = self.current_epoch.fetch_add(1, Ordering::SeqCst);
        self.drain_list.push(prior_epoch, Box::new(callback));
        self.try_drain();
    }

    /// Compute safe-to-reclaim epoch and execute ready drain actions.
    fn try_drain(&self) {
        let min_epoch = self.compute_safe_epoch();
        let old_safe = self.safe_to_reclaim_epoch.load(Ordering::Relaxed);
        if min_epoch > old_safe {
            self.safe_to_reclaim_epoch.store(min_epoch, Ordering::Release);
            self.drain_list.drain_up_to(min_epoch);
        }
    }

    fn compute_safe_epoch(&self) -> u64 {
        let current = self.current_epoch.load(Ordering::SeqCst);
        let mut min = current;
        for entry in self.table.iter() {
            let epoch = entry.local_current_epoch.load(Ordering::Acquire);
            if epoch != 0 && epoch < min {
                min = epoch;
            }
        }
        min - 1 // Safe to reclaim everything *before* the oldest active epoch
    }
}
```

**Memory ordering rationale for `bump_current_epoch`:**
- `fetch_add` uses `SeqCst` because it must be totally ordered with respect to other threads' `local_current_epoch` stores. Without `SeqCst`, a thread could read a stale `current_epoch` and store it to its local slot *after* another thread bumps the epoch, creating a race where the safe epoch is computed incorrectly.
- `compute_safe_epoch` reads the global epoch with `SeqCst` and each thread's epoch with `Acquire`. This ensures we see the latest published value from each thread.

#### 4.1.4 RAII Epoch Guards

Rust's ownership model gives us something C++ and C# lack: guaranteed epoch release via RAII.

```rust
pub struct EpochGuard<'a> {
    epoch: &'a LightEpoch,
    thread_index: usize,
}

impl<'a> EpochGuard<'a> {
    pub fn new(epoch: &'a LightEpoch, thread_index: usize) -> Self {
        epoch.protect(thread_index);
        Self { epoch, thread_index }
    }
}

impl Drop for EpochGuard<'_> {
    fn drop(&mut self) {
        self.epoch.unprotect(self.thread_index);
    }
}
```

In C#, forgetting to call `Suspend()` after `Resume()` causes resource leaks. In C++, forgetting to call epoch release causes the safe-to-reclaim epoch to stall, eventually blocking all other threads. In Rust, the guard's destructor makes this impossible.

---

### 4.2 Thread Model: Thread-Per-Session

FASTER uses a **thread-per-session** model (matching the C++ design). Each session is bound to a single thread. Multiple sessions execute concurrently on separate threads, coordinated via the epoch system.

**Why not work-stealing / thread pool?**
- FASTER's hot path (hash lookup → log access → record read/write) touches thread-local state: the session's execution context, pending operation buffers, and the epoch table entry. Thread migration between operations would require either (a) re-acquiring epoch protection on every operation, or (b) using thread-safe (atomic) data structures for session state — both of which add overhead.
- The C++ implementation hard-codes this model with per-thread `ThreadContext`. Sessions are `!Send` (cannot be moved between threads). This eliminates an entire class of concurrency bugs.
- Async runtimes that use thread-per-core (monoio) or work-stealing (Tokio) can still integrate: the async adapter creates a `Session` per runtime task and pins it to a specific thread.

```rust
pub struct Session<K, V, F> {
    // Index into epoch table. Unique per session.
    thread_index: usize,
    // Execution contexts (double-buffered for checkpoint transitions)
    contexts: [ExecutionContext; 2],
    current_context: usize, // 0 or 1
    // Pending operations awaiting I/O completion
    pending_ops: PendingOperationQueue,
    // I/O response queue (filled by device completion callbacks)
    io_responses: VecDeque<IoResponse>,
    // Reference to the shared FasterKv state
    store: Arc<FasterKvInner<K, V>>,
    // User's callback functions
    functions: F,
    // !Send: compile-time prevention of cross-thread use
    _not_send: PhantomData<*const ()>,
}
```

**`!Send` enforcement:** The `PhantomData<*const ()>` marker makes `Session` non-`Send`. Any attempt to move a session to another thread or use it from an async task that may migrate threads will fail at compile time. This is a strict improvement over C#, where mono-threaded session usage is documented but not enforced.

---

### 4.3 No Async/Await — Callback/Completion Model

**This is the defining architectural constraint** (per qbradley's directive). The core crate uses ZERO async/await syntax and depends on ZERO async runtimes.

#### 4.3.1 Why Not Async/Await in Core

1. **Runtime coupling.** `async fn` in Rust requires a runtime to poll the returned `Future`. Choosing Tokio locks out monoio, compio, and every future runtime. The core must be runtime-agnostic.
2. **Performance ceiling.** Async runtimes add overhead: task scheduling, waker registration, potential allocations for `Future` state machines. FASTER's hot path (in-memory read/upsert) completes in <100ns. Even ~10ns of async overhead is significant at this scale.
3. **Compatibility.** The core must be embeddable in non-async contexts: FFI bindings, WASM, bare-metal. `async` pollutes the API surface — every caller must be async.
4. **The C++ model works.** C++ FASTER uses callbacks for async I/O completions and achieves world-class performance. The callback model is proven for this use case.

#### 4.3.2 How Completion Callbacks Work

When an operation encounters a record on disk, it cannot complete synchronously. Instead:

```rust
/// Status returned by core operations.
pub enum OperationStatus {
    /// Operation completed successfully.
    Success,
    /// Key not found.
    NotFound,
    /// Record is on disk. I/O has been issued. Call `complete_pending()` later.
    Pending,
    /// Retry due to concurrent state change (CPR shift, lock contention).
    RetryLater,
}

/// Callback invoked when an async I/O completes.
pub type IoCompletionCallback = fn(
    context: *mut u8,       // Opaque pointer to the pending context
    status: IoStatus,       // Success or error
    bytes_transferred: u32, // How many bytes were read
);

/// Trait for pending operation contexts.
/// Allocated on the heap when an operation goes pending.
trait PendingContext {
    /// Resume the operation after I/O completion.
    fn complete(&mut self, io_result: &IoResult, store: &FasterKvInner) -> OperationStatus;
}
```

**Flow for a Read that hits disk:**

```
1. session.read(&key)
   ├── Hash lookup → find entry with address A
   ├── A < HeadAddress → record is on disk
   ├── Allocate PendingReadContext on heap (Box<PendingReadContext>)
   ├── Allocate aligned I/O buffer from buffer pool
   ├── Call device.read_async(disk_offset, buffer, completion_callback, context_ptr)
   ├── Push PendingReadContext to session.pending_ops
   └── Return OperationStatus::Pending

2. [Later: device completes I/O on its I/O thread]
   ├── completion_callback(context_ptr, IoStatus::Success, bytes)
   └── Enqueue IoResponse { context_ptr, status, buffer } into session.io_responses

3. session.complete_pending(wait: bool)
   ├── For each IoResponse in io_responses:
   │   ├── Recover PendingReadContext from context_ptr
   │   ├── Parse record from I/O buffer
   │   ├── Call user's read callback with the record value
   │   ├── Return I/O buffer to pool
   │   └── Free PendingReadContext
   └── If wait=true, block until all pending ops complete (spin + yield)
```

#### 4.3.3 How This Maps to Async Runtime Integration

The async adapter crates provide the bridge between the callback model and `async`/`await`:

```rust
// In faster-async-tokio/src/session.rs

pub struct AsyncSession<K, V, F> {
    inner: Session<K, V, F>,
}

impl<K: FasterKey, V: FasterValue, F: FasterFunctions<K, V>> AsyncSession<K, V, F> {
    pub async fn read(&mut self, key: &K) -> Result<F::Output, FasterError> {
        let status = self.inner.read(key);
        match status {
            OperationStatus::Success => Ok(self.inner.take_last_output()),
            OperationStatus::NotFound => Err(FasterError::NotFound),
            OperationStatus::Pending => {
                // The I/O has been issued by the core. We need to wait for it.
                // Bridge: poll the session's io_responses via a Tokio Notify.
                loop {
                    // Yield to Tokio runtime, allowing other tasks to run
                    tokio::task::yield_now().await;
                    // Check if the I/O response has arrived
                    if self.inner.has_pending_responses() {
                        self.inner.complete_pending(false);
                        return Ok(self.inner.take_last_output());
                    }
                }
            }
            OperationStatus::RetryLater => {
                tokio::task::yield_now().await;
                // Retry via recursion (bounded by epoch advancement)
                Box::pin(self.read(key)).await
            }
        }
    }
}
```

**The key insight:** The core issues the I/O and registers a callback. The adapter wraps the "wait for callback" step in a `Future` that yields to the runtime. This is a thin translation — ~20 lines per operation — and can be implemented for any runtime that supports yielding.

**For thread-per-core runtimes (monoio):** The adapter would use the runtime's I/O primitives directly in the `Device` implementation, and the completion callback would wake a thread-local notifier. No cross-thread communication needed.

**For io_uring-native runtimes (compio):** The adapter implements `Device` using `compio`'s submission queue. Completions arrive on the same thread, matching FASTER's session-per-thread model perfectly.

---

### 4.4 Synchronization Primitives Inventory

The following synchronization primitives are needed in `faster-core`. All are from `std` — no external crate dependencies.

| Primitive | Usage | Location |
|-----------|-------|----------|
| `AtomicU64` | Hash bucket entries, address pointers, epoch counters, RecordInfo headers | Everywhere |
| `AtomicU32` | Page status, tail offset, overflow allocator bump pointer | HybridLog, OverflowAllocator |
| `AtomicBool` | Phase-finished markers in epoch table, thread-active flags | LightEpoch |
| `AtomicPtr<T>` | Page table entries (pointers to page frames), overflow free list | HybridLog, OverflowAllocator |
| `Mutex<T>` | Checkpoint metadata serialization (off hot path), configuration | Checkpoint, FasterKv builder |
| `Condvar` | `complete_pending(wait=true)` blocking wait, grow coordination | Session, GrowStateMachine |
| `std::thread::spawn` | Background flush thread (optional), I/O completion thread | Device implementations |
| `thread_local!` | Thread-index cache (avoid re-looking up epoch table slot) | Session |

**What we explicitly do NOT use:**
- `RwLock` — the hash index is latch-free; the hybrid log uses atomic addresses. No read-write locks anywhere on the data path.
- `std::sync::mpsc` — I/O responses are delivered via direct callback + lock-free queue, not channels. Channels add allocation and synchronization overhead.
- `Barrier` — checkpoint phase coordination uses the epoch system, not barriers.

---

### 4.5 Session Model: Thread-Local State and Operation Contexts

#### 4.5.1 Execution Context (Double-Buffered)

Each session maintains two execution contexts, swapped during checkpoint phase transitions:

```rust
struct ExecutionContext {
    // Checkpoint version this context was created in
    version: u32,
    // Phase of the state machine this context is operating in
    phase: Phase,
    // Monotonically increasing serial number for operations
    serial_number: u64,
    // Map of pending operations keyed by I/O ID
    pending_ops: HashMap<u64, Box<dyn PendingContext>>,
    // Count of outstanding I/O operations
    io_pending_count: u32,
    // Retry queue: operations that returned RetryLater
    retry_queue: VecDeque<RetryEntry>,
}
```

**Double-buffering rationale:** During a checkpoint transition (version `v` → `v+1`), a thread may have pending operations from version `v` that haven't completed. The thread switches to context `1` for `v+1` operations while context `0` drains `v` operations. When all `v` operations complete, context `0` is recycled.

```rust
fn swap_contexts(&mut self) {
    self.current_context = 1 - self.current_context;
    // New context inherits the new version and phase
    self.contexts[self.current_context].version = /* new version */;
    self.contexts[self.current_context].phase = Phase::InProgress;
}
```

#### 4.5.2 Pending Operation Queue

When an operation goes pending (record on disk), the session tracks it:

```rust
struct PendingReadContext<K, V, F: FasterFunctions<K, V>> {
    key: K,
    entry: HashBucketEntry,
    io_id: u64,
    user_context: F::Context,
    serial_number: u64,
}

struct PendingRmwContext<K, V, F: FasterFunctions<K, V>> {
    key: K,
    input: F::Input,
    entry: HashBucketEntry,
    io_id: u64,
    user_context: F::Context,
    serial_number: u64,
}
```

**Heap allocation:** Pending contexts are `Box`-allocated when the operation goes pending. This replaces the C++ pattern of `DeepCopy` (stack → heap) with a direct heap allocation. The overhead is acceptable because going pending is the slow path — it implies a disk read that takes microseconds to milliseconds.

**Arena optimization (future):** For workloads with many pending operations, we could use a per-session arena allocator (bump allocator) for pending contexts, recycled when the session calls `complete_pending`. This avoids individual `Box` allocations/deallocations.

---

### 4.6 Grow (Resize) State Machine

Hash table growth doubles the table size and redistributes entries. This is a coordinated multi-phase operation:

```
Phase 1: GROW_PREPARE
  ├── Initiating thread allocates new hash table (2× size)
  ├── Bumps epoch with drain callback
  └── All threads must acknowledge the grow-prepare phase

Phase 2: GROW_IN_PROGRESS
  ├── Each thread splits its bucket range:
  │   For each bucket in old_table[start..end]:
  │     For each entry in bucket:
  │       Compute new bucket index (using one more bit of hash)
  │       CAS entry into new_table[new_index]
  ├── Thread marks its range as complete
  └── When all threads complete, swap old_table → new_table

Phase 3: REST
  ├── Old table is freed (via epoch drain callback — safe because
  │   all threads have moved past the grow epoch)
  └── New table is now the active table
```

**Key invariants during grow:**
- **Reads still work:** A read that starts before grow completes may use the old table. The old table remains valid until all threads have transitioned to the new table.
- **Inserts during grow:** New entries are inserted into the *new* table (detected by checking a `growing` flag). This ensures no entries are lost.
- **No locks:** The entire grow operation uses CAS operations to move entries. If a CAS fails (another thread moved the entry first), the current thread skips it.

```rust
enum GrowPhase {
    Idle,
    Preparing { new_table: Box<[HashBucket]> },
    InProgress {
        old_table: Box<[HashBucket]>,
        new_table: Box<[HashBucket]>,
        thread_progress: Box<[AtomicBool]>, // One per thread
    },
}
```

**Thread participation:** During `GROW_IN_PROGRESS`, each thread splits a proportional range of buckets (total_buckets / num_active_threads) on each `Refresh()` call. This amortizes the work across normal operation. A dedicated grow-completion check runs after each batch.

**Rust advantage:** The old table is freed via an epoch drain callback, which is a `Box<dyn FnOnce()>`. Rust's ownership system ensures the `Box<[HashBucket]>` is dropped exactly once, when the drain fires. No manual deallocation, no use-after-free risk.

---


## 5. Storage Layer

### 5.1 Device Trait: Abstract I/O

The `Device` trait is the abstraction boundary between the core KV engine and all I/O. It lives in the `faster-device` crate and is the sole I/O dependency of `faster-core`.

#### 5.1.1 Trait Definition

```rust
/// Completion callback for async I/O operations.
/// Called by the device when a read or write completes.
///
/// # Safety
/// `context` must be a valid pointer to the context passed to `read_async`/`write_async`.
/// The callback is responsible for interpreting and freeing the context.
pub type IoCompletionCallback = unsafe fn(
    context: *mut u8,       // Opaque user context pointer
    status: IoStatus,       // Success, Error(code), Cancelled
    bytes_transferred: u32, // Actual bytes read/written
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoStatus {
    Success,
    Error(i32),  // OS error code
    Cancelled,
}

/// Abstract storage device for FASTER's hybrid log.
///
/// # Design Principles
/// - **Completion-based, not async/await.** The caller provides a callback; the device
///   invokes it when I/O completes. This enables zero-async-runtime integration.
/// - **Sector-aligned I/O.** All offsets and lengths must be multiples of `sector_size()`.
///   This enables O_DIRECT on Linux and unbuffered I/O on Windows.
/// - **Segmented.** The device manages multiple segments (files) internally. The caller
///   uses a linear offset space; the device maps it to segment + offset.
///
/// # Thread Safety
/// All methods must be safe to call from any thread. The device handles internal
/// synchronization (e.g., submission queue locks for io_uring).
pub trait Device: Send + Sync {
    /// Sector size for aligned I/O (typically 512 or 4096 bytes).
    fn sector_size(&self) -> u32;

    /// Maximum concurrent I/O operations the device can handle.
    fn max_outstanding_io(&self) -> u32;

    /// Issue an asynchronous read.
    ///
    /// Reads `length` bytes from `source_offset` in the log into `dest_buffer`.
    /// When complete, invokes `callback(context, status, bytes_read)`.
    ///
    /// # Safety
    /// - `dest_buffer` must be valid for writes of `length` bytes and must remain
    ///   valid until the callback is invoked.
    /// - `context` must remain valid until the callback is invoked.
    /// - `source_offset` and `length` must be multiples of `sector_size()`.
    unsafe fn read_async(
        &self,
        source_offset: u64,
        dest_buffer: *mut u8,
        length: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult;

    /// Issue an asynchronous write.
    ///
    /// Writes `length` bytes from `source_buffer` to `dest_offset` in the log.
    /// When complete, invokes `callback(context, status, bytes_written)`.
    ///
    /// # Safety
    /// - `source_buffer` must be valid for reads of `length` bytes and must remain
    ///   valid until the callback is invoked.
    /// - `context` must remain valid until the callback is invoked.
    /// - `dest_offset` and `length` must be multiples of `sector_size()`.
    unsafe fn write_async(
        &self,
        source_buffer: *const u8,
        dest_offset: u64,
        length: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult;

    /// Truncate the device, removing all data before `offset`.
    /// Used during log truncation (advancing BeginAddress).
    fn truncate(&self, offset: u64);

    /// Returns the current logical size of the device (highest written offset).
    fn size(&self) -> u64;

    /// Close the device and release all resources.
    fn close(&self);
}

#[derive(Debug)]
pub enum IoRequestResult {
    /// I/O was submitted successfully. Callback will be invoked later.
    Submitted,
    /// I/O completed synchronously (e.g., NullDevice). Callback was already invoked.
    CompletedSync,
    /// I/O queue is full. Caller should retry after draining completions.
    QueueFull,
    /// Error during submission.
    Error(std::io::Error),
}
```

#### 5.1.2 Why Completion-Based, Not Async/Await

The device trait uses raw function pointer callbacks (`IoCompletionCallback`) instead of returning `impl Future` for three reasons:

1. **No async runtime dependency.** A `Future`-based trait would require `async_trait` (heap allocation per call) or GATs (unstable, complex). A function pointer is zero-cost and runtime-agnostic.
2. **Matches the hardware model.** All high-performance I/O interfaces (io_uring, IOCP, SPDK) are completion-based. The callback model maps 1:1 to these interfaces without an adapter layer.
3. **Enables the async adapter pattern.** An async adapter wraps the callback in a `oneshot::channel()` or `Notify` to bridge to `Future`. This is a one-way translation — going from `Future` to callback is harder and requires spawning.

**The opaque `*mut u8` context pointer** is an intentional design choice. It avoids baking any specific context type into the trait (which would require generics or trait objects with dynamic dispatch). The caller — `faster-core` — knows the concrete type and casts accordingly. The `unsafe` contract is simple: the pointer must remain valid until the callback fires.

#### 5.1.3 Built-In Device Implementations

| Device | Crate | Purpose |
|--------|-------|---------|
| `NullDevice` | `faster-device` | Discards writes, returns zeros on read. For benchmarking pure in-memory throughput. |
| `SyncFileDevice` | `faster-device` | Synchronous `pread`/`pwrite` with a background thread pool for "async" behavior. Portable baseline. |
| `DirectIoDevice` | `faster-device` (feature: `direct-io`) | Linux `O_DIRECT` + thread pool. Bypasses page cache for large sequential I/O. |
| `IoUringDevice` | `faster-device` (feature: `io-uring`) | Linux io_uring. True kernel-bypass async I/O. Highest performance on modern Linux. |
| `TokioDevice` | `faster-async-tokio` | Tokio's `AsyncFd` or `tokio-uring` for I/O. Integrates with Tokio's event loop. |
| `CompioDevice` | `faster-async-compio` | compio's completion-based I/O. Native io_uring on Linux, IOCP on Windows. |
| `MonoioDevice` | `faster-async-monoio` | monoio's thread-per-core I/O. Optimal for pinned-thread architectures. |

---

### 5.2 File I/O Strategy

#### 5.2.1 Linux: io_uring for Async, O_DIRECT for Bypass

**Primary target:** Linux with `io_uring` (kernel 5.1+) and `O_DIRECT`.

**Why io_uring?**
- **Zero system calls on the hot path.** Submissions and completions go through shared memory rings, not `read()`/`write()` syscalls. At planetary scale, syscall overhead is measurable.
- **Batching.** Multiple I/O operations can be submitted in a single `io_uring_enter()` call. FASTER's page flush naturally produces batches (multiple pages flushed simultaneously).
- **Kernel-side polling (SQPOLL).** Optional: the kernel polls the submission queue without any userspace syscalls at all. Useful for ultra-low-latency workloads.
- **Fixed buffers and files.** io_uring supports pre-registered buffers and file descriptors, eliminating per-I/O setup overhead.

**Why O_DIRECT?**
- FASTER manages its own page cache (the hybrid log's in-memory pages). The OS page cache is redundant and harmful:
  - **Double caching:** The same data exists in both FASTER's pages and the OS page cache, wasting memory.
  - **Unpredictable eviction:** The OS may evict hot pages from its cache, causing unexpected latency spikes.
  - **Write amplification:** The OS page cache may write-back dirty pages at inopportune times.
- `O_DIRECT` bypasses the OS page cache entirely. FASTER controls all caching.

**Alignment requirement:** `O_DIRECT` requires all I/O offsets, lengths, and buffer addresses to be aligned to the device's sector size (typically 512 bytes or 4096 bytes). The `BufferPool` (§5.4) ensures all I/O buffers meet this constraint.

**Implementation sketch:**

```rust
struct IoUringDevice {
    ring: io_uring::IoUring,
    files: Vec<SegmentedFile>,
    sector_size: u32,
    // Pending I/O contexts (keyed by user_data field in CQE)
    pending: Slab<IoContext>,
}

struct IoContext {
    callback: IoCompletionCallback,
    user_context: *mut u8,
    buffer: *mut u8,
}

impl IoUringDevice {
    fn submit_read(&self, offset: u64, buffer: *mut u8, len: u32,
                   callback: IoCompletionCallback, ctx: *mut u8) -> IoRequestResult {
        let (segment, seg_offset) = self.map_offset(offset);
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fd(segment.fd()),
            buffer,
            len,
        ).offset(seg_offset).build();

        let io_ctx_id = self.pending.insert(IoContext { callback, user_context: ctx, buffer });
        unsafe { self.ring.submission().push(&sqe.user_data(io_ctx_id as u64)) }
            .map_err(|_| IoRequestResult::QueueFull)?;
        self.ring.submit().ok();
        IoRequestResult::Submitted
    }

    fn poll_completions(&self) {
        for cqe in self.ring.completion() {
            let io_ctx = self.pending.remove(cqe.user_data() as usize);
            let status = if cqe.result() >= 0 {
                IoStatus::Success
            } else {
                IoStatus::Error(-cqe.result())
            };
            unsafe {
                (io_ctx.callback)(io_ctx.user_context, status, cqe.result() as u32);
            }
        }
    }
}
```

#### 5.2.2 Cross-Platform Fallback

For non-Linux platforms or older kernels without io_uring:

**Strategy 1: Thread-pool + pread/pwrite (primary fallback)**
- A small thread pool (e.g., 4 threads) services I/O requests from a lock-free queue.
- Each I/O thread calls `pread()`/`pwrite()` (which is synchronous) and then invokes the callback.
- This is the model used by C++ FASTER's `ThreadPoolIoHandler`.
- Pros: Works everywhere. Simple. Predictable.
- Cons: Thread overhead, context switch per I/O.

**Strategy 2: mmap (not recommended for primary path)**
- Memory-map the log files. Reads become pointer dereferences. Writes become page faults that the OS flushes.
- Pros: Zero-copy reads. Simple API.
- Cons: OS controls flushing (unpredictable latency). `munmap` is expensive. Page faults on read can stall the calling thread. `O_DIRECT` is incompatible with mmap. FASTER's entire value proposition is *managing its own caching* — mmap gives that control back to the OS.
- **Decision:** mmap is available as a `MmapDevice` for specific use cases (e.g., read-only recovery of existing log files) but is NOT the default path.

**Platform detection:**

```rust
// In faster-device/src/lib.rs
pub fn default_device(path: &Path, config: &DeviceConfig) -> Box<dyn Device> {
    #[cfg(all(target_os = "linux", feature = "io-uring"))]
    {
        if io_uring_available() {
            return Box::new(IoUringDevice::new(path, config));
        }
    }
    #[cfg(all(target_os = "linux", feature = "direct-io"))]
    {
        return Box::new(DirectIoDevice::new(path, config));
    }
    // Universal fallback
    Box::new(SyncFileDevice::new(path, config))
}
```

#### 5.2.3 Why the Device Abstraction Must Be Completion-Based

Consider the alternatives:

| Abstraction Style | How it works | Problem |
|-------------------|-------------|---------|
| **Blocking** (`fn read(&self, ...) -> Result<()>`) | Caller blocks until I/O completes | FASTER needs to overlap I/O with computation (flush pages while serving reads). Blocking I/O requires one thread per outstanding I/O. |
| **Future-based** (`async fn read(&self, ...)`) | Returns a `Future` that resolves when I/O completes | Requires an async runtime to poll the future. Core crate cannot depend on a runtime. |
| **Completion-based** (`fn read_async(&self, ..., callback)`) | Caller provides callback; device invokes it when done | No runtime dependency. Maps directly to io_uring/IOCP. Caller continues immediately. |
| **Polling** (`fn read_async(...) -> IoToken` + `fn poll(token) -> Option<Result>`) | Caller polls for completion | Viable but more complex API surface. Requires the caller to manage poll scheduling. |

The completion-based model wins because:
1. It maps directly to hardware (io_uring CQE, IOCP GetQueuedCompletionStatus)
2. It requires zero runtime infrastructure
3. It's trivially wrappable in `Future` by async adapters (oneshot channel + callback)
4. It's what C++ FASTER already uses (proven model)

The polling model is a secondary option worth supporting via an adapter: the callback simply sets a flag that a poller checks. This may be useful for `complete_pending(wait=true)` spin loops.

---

### 5.3 Page Management: Flush and Eviction

#### 5.3.1 Page Flush Pipeline

When the mutable region fills and ReadOnlyAddress advances, pages must be flushed to disk:

```rust
fn flush_pages(&self, from_address: LogicalAddress, to_address: LogicalAddress) {
    let from_page = from_address.page();
    let to_page = to_address.page();

    for page_idx in from_page..=to_page {
        let frame_idx = (page_idx as usize) % self.num_pages;
        let page_ptr = self.pages[frame_idx].load(Ordering::Acquire);

        // Determine flush range within this page
        let flush_start = if page_idx == from_page {
            from_address.offset()
        } else {
            0
        };
        let flush_end = if page_idx == to_page {
            to_address.offset()
        } else {
            self.page_size as u32
        };

        let disk_offset = self.page_to_disk_offset(page_idx);
        let length = flush_end - flush_start;

        // Allocate I/O context (tracks completion for this page)
        let ctx = Box::into_raw(Box::new(FlushContext {
            page_index: page_idx,
            log: self as *const HybridLog<K, V>,
        })) as *mut u8;

        unsafe {
            self.device.write_async(
                page_ptr.add(flush_start as usize),
                disk_offset + flush_start as u64,
                length,
                flush_completion_callback,
                ctx,
            );
        }
    }
}

unsafe fn flush_completion_callback(context: *mut u8, status: IoStatus, _bytes: u32) {
    let ctx = Box::from_raw(context as *mut FlushContext);
    if status == IoStatus::Success {
        // Update per-page flushed-until address
        let log = &*ctx.log;
        let page_status = &log.page_status[ctx.page_index as usize % log.num_pages];
        page_status.last_flushed_until.fetch_max(/* ... */, Ordering::Release);
        // Check if all pages in the flush batch are done
        log.check_flush_complete();
    } else {
        // I/O error: log and propagate. The system must not silently lose data.
        log.report_io_error(ctx.page_index, status);
    }
}
```

#### 5.3.2 Page Eviction

After pages are flushed and HeadAddress advances (via epoch drain), the in-memory page frames can be reclaimed:

```rust
fn evict_pages(&self, from_page: u32, to_page: u32) {
    for page_idx in from_page..to_page {
        let frame_idx = (page_idx as usize) % self.num_pages;
        // The page frame will be reused by a future page (circular buffer)
        // Zero it to prevent stale data reads
        let page_ptr = self.pages[frame_idx].load(Ordering::Acquire);
        if !page_ptr.is_null() {
            unsafe {
                std::ptr::write_bytes(page_ptr, 0, self.page_size);
            }
        }
        // Mark page as available for reuse
        self.page_status[frame_idx].state.store(
            PageState::Free as u32, Ordering::Release
        );
    }
}
```

**Eviction is epoch-gated.** We cannot evict a page frame while any thread might be reading from it. The epoch drain callback ensures all threads have moved past the address range before eviction occurs.

#### 5.3.3 Page Read-Back

When a Read or RMW needs a record that's on disk:

```rust
fn read_page_from_disk(
    &self,
    address: LogicalAddress,
    callback: IoCompletionCallback,
    context: *mut u8,
) {
    let disk_offset = self.page_to_disk_offset(address.page());
    let aligned_offset = align_down(
        disk_offset + address.offset() as u64,
        self.device.sector_size() as u64,
    );
    let aligned_length = align_up(
        /* record size + alignment slack */,
        self.device.sector_size() as u64,
    ) as u32;

    // Acquire aligned buffer from pool
    let buffer = self.buffer_pool.acquire(aligned_length as usize);

    unsafe {
        self.device.read_async(aligned_offset, buffer.as_mut_ptr(), aligned_length, callback, context);
    }
}
```

---

### 5.4 Buffer Pool Design

All I/O operations require sector-aligned buffers. Allocating and deallocating these per-I/O is expensive (mmap/munmap overhead, TLB flushes). A buffer pool amortizes this cost.

```rust
pub struct BufferPool {
    // Free lists bucketed by size class (powers of 2)
    // size_class 0 = sector_size, 1 = 2*sector_size, ..., N = page_size
    free_lists: Vec<Mutex<Vec<AlignedBuffer>>>,
    sector_size: usize,
    max_pool_size_per_class: usize,
}

pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

impl AlignedBuffer {
    fn allocate(size: usize, alignment: usize) -> Self {
        let layout = Layout::from_size_align(size, alignment)
            .expect("invalid buffer layout");
        let ptr = unsafe { std::alloc::alloc(layout) };
        let ptr = NonNull::new(ptr).expect("allocation failed");
        Self { ptr, len: size, layout }
    }

    pub fn as_ptr(&self) -> *const u8 { self.ptr.as_ptr() }
    pub fn as_mut_ptr(&mut self) -> *mut u8 { self.ptr.as_ptr() }
    pub fn len(&self) -> usize { self.len }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout); }
    }
}

// SAFETY: AlignedBuffer owns its allocation and can be sent between threads.
unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}
```

**Pool operations:**

```rust
impl BufferPool {
    pub fn acquire(&self, min_size: usize) -> AlignedBuffer {
        let size = align_up(min_size, self.sector_size).next_power_of_two();
        let class = size.trailing_zeros() - self.sector_size.trailing_zeros();

        if let Some(buf) = self.free_lists[class as usize].lock().unwrap().pop() {
            return buf;
        }
        AlignedBuffer::allocate(size, self.sector_size)
    }

    pub fn release(&self, buffer: AlignedBuffer) {
        let class = buffer.len().trailing_zeros() - self.sector_size.trailing_zeros();
        let mut list = self.free_lists[class as usize].lock().unwrap();
        if list.len() < self.max_pool_size_per_class {
            list.push(buffer);
        }
        // else: drop the buffer (pool is full for this size class)
    }
}
```

**Rationale for size-class bucketing:** I/O operations need buffers of varying sizes (single record read = 512–4096 bytes; full page flush = 32 MB). Size classes avoid fragmentation and enable O(1) allocation from the pool (just pop from the right bucket).

**Mutex overhead:** The `Mutex` on each free list is acceptable because buffer acquire/release happens on the I/O path (slow path — microseconds per operation). The fast path (in-memory reads/writes) never touches the buffer pool.

---

### 5.5 Platform Abstraction Approach

Rather than a single `#[cfg]` monolith, platform differences are isolated at the `Device` trait boundary:

```
┌─────────────────────────────────────────────────────────┐
│                   faster-core                            │
│  (platform-agnostic: uses Device trait, AtomicU64, std) │
└────────────────────────┬────────────────────────────────┘
                         │ trait Device
┌────────────────────────▼────────────────────────────────┐
│                  faster-device                           │
│  ┌──────────────┐ ┌──────────────┐ ┌─────────────────┐ │
│  │ NullDevice   │ │SyncFileDevice│ │ IoUringDevice   │ │
│  │ (all platforms│ │(all platforms│ │ (linux, feature) │ │
│  │  no I/O)     │ │  pread/pwrite│ │  io_uring crate) │ │
│  └──────────────┘ └──────────────┘ └─────────────────┘ │
│  ┌──────────────┐                                       │
│  │DirectIoDevice│                                       │
│  │(linux, O_DIR)│                                       │
│  └──────────────┘                                       │
└─────────────────────────────────────────────────────────┘
```

**Platform-specific code is limited to:**
1. `Device` implementations (file I/O, io_uring) — in `faster-device`
2. `O_DIRECT` flag setting — Linux-only, in `DirectIoDevice`
3. Aligned allocation — uses `std::alloc` (portable) with platform-specific alignment requirements

**No platform-specific code in `faster-core`.** The core crate uses only `std` primitives that are available on all Rust targets. This guarantees that `faster-core` compiles on any platform Rust supports, even if no `Device` implementation exists for that platform (the user can provide their own).

**Windows support path:** The `SyncFileDevice` works on Windows immediately. For high-performance Windows I/O, a future `IocpDevice` would use Windows IOCP (I/O Completion Ports) via the `windows` crate. This is a device implementation, not a core change.

**Cloud storage path:** A future `AzureBlobDevice` or `S3Device` would implement the `Device` trait using HTTP-based blob I/O. The completion callback model naturally accommodates high-latency network I/O — the callback fires when the HTTP response arrives, potentially hundreds of milliseconds later. The core doesn't care about the latency; it just processes the callback.

---

