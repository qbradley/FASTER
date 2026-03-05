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


---

<!-- Architecture Part 2 of 3: Sections 6–10 -->
<!-- Author: Thrawn (Lead / System Architect) -->
<!-- Assembled with Parts 1 and 3 into the final Rust FASTER architecture document -->

## 6. Operations Design

This section specifies the complete operation flow for the four fundamental FASTER
operations — Read, Upsert, RMW (Read-Modify-Write), and Delete — translated into
idiomatic Rust with the **no-async-in-core** constraint. Every operation follows a
two-path model: a *fast path* that completes synchronously when the record is in
the mutable in-memory region, and a *slow path* that issues asynchronous I/O and
delivers results through a completion callback when the record resides on disk or
in the read-only region.

### 6.1 Operation Result Model

Before describing individual operations, we establish the unified result type that
every operation returns:

```rust
/// The outcome of a FASTER operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Operation completed successfully.
    /// Contains fine-grained sub-status for callers who need it.
    Ok(OkKind),

    /// Record is on disk; I/O has been issued and a completion callback will fire.
    /// The caller MUST NOT access output buffers until the pending operation completes.
    Pending,

    /// Key was not found (Read/Delete on absent key).
    NotFound,

    /// Operation must be retried (epoch shift, CAS contention, CPR phase transition).
    /// The session internally retries a bounded number of times before surfacing this.
    RetryLater,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OkKind {
    /// Read: value was copied to output.
    ReadSuccess,
    /// Upsert/RMW: record was updated in-place in the mutable region.
    InPlaceUpdated,
    /// Upsert/RMW: a new record was created at the log tail (copy-update or insert).
    CreatedRecord,
    /// Delete: tombstone was written.
    Deleted,
}
```

**Rationale:** A flat `Result<Status, FasterError>` separates *operational outcomes*
(NotFound, Pending) from *system errors* (I/O failure, corruption). `Status` is
not an error — it's a control-flow signal. True errors (device failure, OOM,
invariant violation) use `FasterError`:

```rust
pub type OperationResult = Result<Status, FasterError>;

#[derive(Debug, thiserror::Error)]
pub enum FasterError {
    #[error("I/O error on device: {0}")]
    Io(#[from] std::io::Error),

    #[error("Record corruption detected at address {address:#x}")]
    Corruption { address: u64 },

    #[error("Session is not active (dropped or not yet created)")]
    SessionInactive,

    #[error("Epoch protection violated: thread not registered")]
    EpochViolation,

    #[error("Checkpoint error: {0}")]
    Checkpoint(String),
}
```

### 6.2 Fast Path vs. Slow Path

Every operation enters through the session, which holds epoch protection for the
duration of the call. The internal dispatch determines the path based on where
the target record lives relative to the hybrid log address boundaries:

```
┌──────────────────────────────────────────────────────────────────┐
│  HYBRID LOG ADDRESS SPACE                                        │
│                                                                  │
│  BeginAddress          HeadAddress      ReadOnlyAddress   Tail   │
│  ──────┼───────────────┼────────────────┼──────────────────┼──── │
│        │  ON DISK       │  READ-ONLY     │  MUTABLE         │    │
│        │  (evicted)     │  (in-memory    │  (in-memory      │    │
│        │                │   immutable)   │   mutable)       │    │
│        │                │                │                  │    │
│        │  SLOW PATH:    │  MEDIUM PATH:  │  FAST PATH:      │    │
│        │  async I/O +   │  copy to tail  │  in-place op     │    │
│        │  callback      │  (no I/O)      │  (no allocation) │    │
└──────────────────────────────────────────────────────────────────┘
```

**Fast path** (mutable region, `address >= read_only_address`):
- Record is in memory and mutable.
- Read: direct copy to output. Upsert: in-place write. RMW: in-place modify. Delete: set tombstone bit.
- No allocation, no I/O. Returns `Status::Ok(...)` immediately.
- Concurrent access is safe: records in the mutable region are accessed under epoch protection, and individual record headers are updated via atomic CAS.

**Medium path** (read-only region, `head_address <= address < read_only_address`):
- Record is in memory but immutable (flushed to disk or being checkpointed).
- Read: direct copy (still in memory). Upsert/RMW/Delete: allocate a new record at the log tail, copy-update, CAS the hash entry to the new address.
- No disk I/O needed, but requires allocation and a CAS loop.
- Returns `Status::Ok(OkKind::CreatedRecord)`.

**Slow path** (on-disk, `address < head_address`):
- Record has been evicted from memory.
- Issue an asynchronous read to the device, register a completion callback.
- Return `Status::Pending` to the caller.
- When I/O completes, the callback fires, the operation retries internally against the now-loaded page, and the result is delivered to the caller's completion context.

### 6.3 Read Operation

```
Read(key, &mut output) → OperationResult
```

**Flow:**

1. **Hash lookup:** Compute `key.hash()`, extract bucket index and 14-bit tag. Scan the bucket (8 entries + overflow chain) for a matching tag.
2. **No match:** Return `Ok(Status::NotFound)`.
3. **Match found:** Extract the 48-bit logical address from the hash entry.
4. **Walk the version chain:** Follow `previous_address` pointers in record headers until we find a record whose key actually matches (tag collisions are possible). If the chain terminates without a match, return `NotFound`.
5. **Address classification:**
   - `address >= read_only_address` → **Fast path.** Call `Functions::read()` with a shared reference to the in-memory value. Copy result to `output`. Return `Ok(ReadSuccess)`.
   - `head_address <= address < read_only_address` → **Medium path** (same as fast path for reads — the record is still in memory, just immutable). Call `Functions::read()`. Return `Ok(ReadSuccess)`.
   - `address < head_address` → **Slow path.** Record is on disk. Allocate a `PendingContext::Read { key, output_slot, user_context }`. Issue `device.read_async(page_address, aligned_buffer, completion_callback)`. Return `Pending`.
6. **Tombstone check:** If the matched record has the tombstone bit set, return `NotFound`.
7. **Read cache (future):** If read cache is enabled, check it before going to disk. On a read cache miss after disk I/O completes, optionally insert the loaded record into the read cache.

**Pending completion for Read:**
When the device I/O completes, the core invokes the registered callback with the
loaded page. The callback:
- Locates the record within the loaded page.
- Calls `Functions::read()` with the on-disk record's value.
- Copies the result into the pending context's output slot.
- Marks the pending operation as complete.

### 6.4 Upsert Operation

```
Upsert(key, value, &mut output) → OperationResult
```

**Flow:**

1. **Hash lookup:** `FindOrCreateEntry(hash)` — returns existing entry or creates a new empty entry in the bucket.
2. **Key not found (new entry):**
   - Allocate a new record at the log tail: `hlog.allocate(record_size)`.
   - Write `RecordInfo` header (version, previous_address = invalid).
   - Write key and value via `Functions::upsert()` (which handles serialization).
   - CAS the hash entry from empty → new address. On CAS failure (another thread inserted first), retry from step 1.
   - Return `Ok(CreatedRecord)`.
3. **Key found, address in mutable region:**
   - Call `Functions::upsert()` with `old_value: Some(&existing_value)`.
   - The function writes the new value in-place.
   - Return `Ok(InPlaceUpdated)`.
4. **Key found, address in read-only or on-disk:**
   - Allocate a new record at the tail.
   - Set `previous_address` to the old record's address (version chain).
   - Write key and new value.
   - CAS the hash entry to point to the new record. On CAS failure, retry.
   - Return `Ok(CreatedRecord)`.

**Critical invariant: Upsert NEVER goes pending.** Even if the old record is on
disk, we don't need to read it — we simply create a new record at the tail. The
old on-disk record becomes unreachable once the hash entry is updated and will
eventually be garbage-collected or compacted. This is a fundamental property
inherited from both C++ and C# implementations.

**CPR interaction:** During checkpoint `IN_PROGRESS` phase, if the existing record
belongs to version `v` and the current thread is in version `v+1`, the new record
must be created with version `v+1` and the old record must not be modified (it
belongs to the checkpoint). The `previous_address` chain preserves the `v` record
for recovery.

### 6.5 RMW (Read-Modify-Write) Operation

```
Rmw(key, input, &mut output) → OperationResult
```

RMW is the most complex operation because it has three sub-paths (initial, in-place,
copy-update) and is the only mutation that can go pending.

**Flow:**

1. **Hash lookup:** `FindOrCreateEntry(hash)`.
2. **Key not found → Initial-update path:**
   - Call `Functions::rmw_need_initial_update(key, input)`. If `false`, return `NotFound` (the user declines to create a default).
   - Allocate a new record at the tail.
   - Call `Functions::rmw_initial(key, input, &mut new_value, &mut output)` to initialize the value (e.g., set a counter to 0, then apply the modification).
   - CAS into hash table. On failure, retry.
   - Return `Ok(CreatedRecord)`.
3. **Key found, address in mutable region → In-place update path:**
   - Call `Functions::rmw_in_place(key, input, &mut existing_value, &mut output)`.
   - If the function returns `InPlaceOk`, the value was modified atomically in place. Return `Ok(InPlaceUpdated)`.
   - If the function returns `NeedsNewRecord` (e.g., variable-length value grew), fall through to the copy-update path.
4. **Key found, address in read-only region → Copy-update path:**
   - Call `Functions::rmw_need_copy_update(key, input, &old_value)`. If `false`, skip (treat as not-found or no-op per user logic).
   - Allocate a new record at the tail.
   - Call `Functions::rmw_copy_update(key, input, &old_value, &mut new_value, &mut output)`.
   - CAS hash entry. On failure, retry.
   - Return `Ok(CreatedRecord)`.
5. **Key found, address on disk → Slow path (pending):**
   - The old value must be read from disk before we can apply the modification.
   - Allocate `PendingContext::Rmw { key, input, user_context }`.
   - Issue async I/O to load the page containing the record.
   - Return `Pending`.
   - On I/O completion, the callback reads the old value from the loaded page, then executes the copy-update sub-path (step 4) against it.

**Tombstone handling:** If the matched record is a tombstone, treat it as key-not-found
and go to the initial-update path (step 2).

**Expired record handling (future):** If an expiration callback is configured and
the record is expired, treat it as key-not-found even if a value exists. This
enables TTL semantics without separate cleanup.

### 6.6 Delete Operation

```
Delete(key) → OperationResult
```

**Flow:**

1. **Hash lookup:** `FindEntry(hash)`.
2. **Key not found:** Return `Ok(Status::NotFound)`. Delete is idempotent.
3. **Key found, address in mutable region:**
   - Set the tombstone bit in the record header via atomic CAS on the `RecordInfo` u64.
   - Optionally call `Functions::delete(key, &mut value)` for cleanup callbacks.
   - Attempt hash chain elision: if the tombstoned record's `previous_address` is invalid, clear the hash entry entirely (saves future lookups).
   - Return `Ok(Deleted)`.
4. **Key found, address in read-only or on-disk:**
   - Allocate a new tombstone record at the log tail (key + tombstone header, no value).
   - CAS hash entry to point to the tombstone.
   - Return `Ok(Deleted)`.

**Critical invariant: Delete NEVER goes pending.** Like Upsert, we never need to
read the old value from disk — we simply create a tombstone at the tail. The
old record becomes unreachable.

### 6.7 Pending Operation Model

This is the crux of the no-async-in-core design. When an operation goes to the
slow path (record on disk), the core does NOT use `async`/`await` or `Future`.
Instead, it uses a **completion callback** model that is runtime-agnostic.

#### Pending Context Lifecycle

```
1. Operation discovers record is on disk (address < head_address).

2. Core allocates a PendingContext on the heap:
   ┌─────────────────────────────────────┐
   │ PendingContext                       │
   │   operation: PendingOp::Read { .. } │
   │   key: K (owned clone)              │
   │   hash: KeyHash                     │
   │   output_slot: *mut Output          │
   │   completion: CompletionSlot        │
   │   user_context: C (user data)       │
   │   retry_count: u32                  │
   └─────────────────────────────────────┘

3. Core issues async I/O to the Device trait:
   device.read_async(
       segment_id,
       offset,
       aligned_buffer,
       io_callback,       // function pointer: fn(*mut u8, IoResult)
       pending_context,   // passed as opaque pointer
   )

4. Core returns Status::Pending to the caller.

5. [Time passes — I/O completes in the device layer]

6. The device calls io_callback(buffer, result, pending_context).
   The callback:
   a. Locates the record in the loaded buffer.
   b. Re-executes the operation logic (now with the record in memory).
   c. Writes the result into the pending context.
   d. Enqueues the completed context into the session's completion queue.

7. Caller retrieves results via one of:
   a. session.complete_pending(wait: true)  — blocks until all pending ops complete.
   b. session.complete_pending(wait: false) — processes only already-completed ops.
   c. session.try_complete_pending()        — non-blocking, returns count completed.
```

#### CompletionSlot: The Runtime-Agnostic Callback

The `CompletionSlot` is how the core communicates completion without knowing about
any async runtime:

```rust
/// A slot that can be signaled when a pending operation completes.
/// This is the core's ONLY interface to the outside world for pending ops.
pub enum CompletionSlot {
    /// No notification requested. Caller will poll via complete_pending().
    None,

    /// A raw callback function pointer + data pointer.
    /// Used by the C FFI layer and by sync callers who want a callback.
    Callback {
        func: unsafe extern "C" fn(result: *const OperationResult, context: *mut c_void),
        context: *mut c_void,
    },

    /// A waker to be notified. Used by async runtime adapters.
    /// The adapter stores a std::task::Waker here. When the operation completes,
    /// core calls waker.wake() — this is the ONLY async-runtime-touching code,
    /// and it lives in the adapter, not in core.
    Waker(std::task::Waker),
}
```

**Rationale for this design:**

1. **No async runtime dependency:** `std::task::Waker` is in the standard library,
   not in Tokio or any runtime. Core merely calls `waker.wake()` — it has no idea
   what happens next.
2. **Naturally maps to blocking:** A sync adapter can use a `Condvar`-based waker
   that blocks the calling thread until completion.
3. **Naturally maps to Futures:** An async adapter wraps the pending operation in
   a `Future` that registers its `Waker` in the `CompletionSlot`. When `wake()` is
   called, the runtime polls the `Future`, which reads the result from the
   `PendingContext`.
4. **C FFI compatible:** The callback variant accepts a C function pointer, enabling
   C callers to get async notifications.
5. **Zero-cost for fast path:** When the operation completes on the fast path (no
   I/O), no `CompletionSlot` is ever allocated or consulted.

#### Thread-Local Completion Queue

Each session maintains a completion queue where finished pending operations are
enqueued by I/O callbacks:

```rust
struct SessionPendingState {
    /// Pending operations awaiting I/O completion.
    /// Keyed by a monotonically increasing serial number.
    pending: HashMap<u64, Box<PendingContext<K, V, C>>>,

    /// Completed operations ready for caller retrieval.
    /// Populated by I/O completion callbacks.
    completed: VecDeque<Box<PendingContext<K, V, C>>>,

    /// Number of outstanding I/O requests (for backpressure).
    io_outstanding: AtomicU32,
}
```

`complete_pending(wait: bool)`:
- Dequeues from `completed`.
- For each completed context, invokes the user's `Functions::read_completion()` or
  `Functions::rmw_completion()` callback (if provided).
- If `wait == true` and `pending` is non-empty, spins/parks until all pending ops
  complete (using the device's polling mechanism).

### 6.8 User-Facing Functions Trait

The `Functions` trait is the Rust equivalent of C#'s `IFunctions` and C++'s
template callback context. It defines user-provided semantics for all operations.

**Design principle:** Collapse C#'s 19 methods into ~8 core methods with sensible
defaults. Use associated types to reduce generic parameter explosion.

```rust
/// User-defined operation semantics for a FASTER store.
///
/// This trait defines how records are read, written, modified, and deleted.
/// Every method has a default implementation that handles the common case
/// (simple value copy / overwrite). Override only what you need.
pub trait Functions {
    /// The key type. Must be hashable and comparable.
    type Key: Hash + Eq + Clone;

    /// The value type stored in the log.
    type Value: Clone;

    /// Caller-provided input passed to RMW and Read operations.
    /// For simple KV stores, this is often `()`.
    type Input;

    /// Output produced by Read and RMW operations.
    /// Typically the same as Value, but can be a projection/aggregation.
    type Output: Default;

    /// Opaque user context carried through pending operations.
    /// Returned to the user on completion. Use `()` if not needed.
    type Context;

    // ── Read ──────────────────────────────────────────────────────────────

    /// Read a record's value and produce output.
    ///
    /// Called on the fast path (record in memory) and on the slow path
    /// (after I/O completion brings the record into memory).
    ///
    /// Default: clones the value into output (requires Value: Into<Output>).
    fn read(
        &self,
        key: &Self::Key,
        value: &Self::Value,
        input: &Self::Input,
        output: &mut Self::Output,
    );

    /// Called when a pending Read completes (optional notification).
    ///
    /// Default: no-op. Override to get notified of async completions
    /// with the user context that was passed to the original operation.
    fn read_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: Status,
    ) {}

    // ── Upsert ────────────────────────────────────────────────────────────

    /// Write a value into a record.
    ///
    /// `old_value` is `Some` when updating an existing record in the mutable
    /// region (in-place update). It is `None` when creating a new record.
    ///
    /// Default: overwrites with the input value (requires Input = Value).
    fn upsert(
        &self,
        key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        old_value: Option<&Self::Value>,
        output: &mut Self::Output,
    );

    // ── RMW ───────────────────────────────────────────────────────────────

    /// Decide whether to create a new record when the key is not found.
    ///
    /// Default: returns true (always create).
    fn rmw_need_initial_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
    ) -> bool {
        true
    }

    /// Initialize a new record for a key that doesn't exist yet.
    ///
    /// Called when the key is not found and `rmw_need_initial_update` returned true.
    /// Write the initial value into `value`.
    fn rmw_initial(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Update a record in-place in the mutable region.
    ///
    /// Returns `InPlaceOk` if the update succeeded, or `NeedsNewRecord` if the
    /// value needs to grow (e.g., variable-length append).
    ///
    /// Default: applies the same logic as `rmw_copy_update`.
    fn rmw_in_place(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    ) -> RmwInPlaceResult;

    /// Decide whether to copy a read-only record before updating.
    ///
    /// Default: returns true (always copy-update).
    fn rmw_need_copy_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
        _old_value: &Self::Value,
    ) -> bool {
        true
    }

    /// Create a new record by copying and modifying an existing read-only record.
    ///
    /// Read from `old_value`, apply the modification, write to `new_value`.
    fn rmw_copy_update(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Called when a pending RMW completes (optional notification).
    fn rmw_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: Status,
    ) {}

    // ── Delete ────────────────────────────────────────────────────────────

    /// Called when a record is being deleted (optional cleanup).
    ///
    /// Default: no-op.
    fn delete(
        &self,
        _key: &Self::Key,
        _value: &mut Self::Value,
    ) {}
}

/// Result of an in-place RMW update.
pub enum RmwInPlaceResult {
    /// Update applied successfully in-place.
    InPlaceOk,
    /// Value needs to be relocated (e.g., variable-length growth).
    /// Core will allocate a new record and call rmw_copy_update.
    NeedsNewRecord,
}
```

**Rationale for the simplification (19 → 8+2 completion callbacks):**

| C# Method | Rust Equivalent | Why Collapsed |
|------------|----------------|---------------|
| `SingleReader` / `ConcurrentReader` | `read()` | Rust's borrow checker enforces shared-ref safety; no need for two variants |
| `SingleWriter` / `ConcurrentWriter` | `upsert()` with `old_value: Option` | Unified via the `Option` parameter |
| `PostSingleWriter` / `PostCopyUpdater` / `PostInitialUpdater` | Integrated into the main methods | Separate "post" hooks add complexity without clear Rust benefit; the main method can do cleanup |
| `NeedInitialUpdate` / `InitialUpdater` | `rmw_need_initial_update()` + `rmw_initial()` | Same pattern, cleaner naming |
| `NeedCopyUpdate` / `CopyUpdater` | `rmw_need_copy_update()` + `rmw_copy_update()` | Same |
| `InPlaceUpdater` | `rmw_in_place()` | Returns enum instead of bool for clarity |
| `SingleDeleter` / `ConcurrentDeleter` | `delete()` | Unified; tombstone mechanics are internal |
| `DisposeSingleWriter` / `DisposeCopyUpdater` / `DisposeInitialUpdater` | Rust `Drop` trait on Value | RAII handles cleanup automatically |
| `ReadCompletionCallback` / `RMWCompletionCallback` | `read_completion()` / `rmw_completion()` | Same pattern, optional |

### 6.9 Error Handling Strategy

Operations return `Result<Status, FasterError>`:

- **`Ok(Status::Ok(...))`** — Operation completed successfully (fast or medium path).
- **`Ok(Status::Pending)`** — Operation issued I/O; result will be delivered via completion callback or `complete_pending()`.
- **`Ok(Status::NotFound)`** — Key does not exist (for Read/Delete). This is not an error.
- **`Ok(Status::RetryLater)`** — Transient condition (CAS contention, epoch shift). The session retries internally up to a configurable limit before surfacing this.
- **`Err(FasterError::Io(...))`** — Device I/O failure. The operation did not complete.
- **`Err(FasterError::Corruption(...))`** — Record integrity check failed.

**Pending operation errors** are delivered through the completion callback or retrievable
from `complete_pending()`. The pending context carries the error:

```rust
/// Result delivered when a pending operation completes.
pub struct PendingResult<O, C> {
    pub status: Result<Status, FasterError>,
    pub output: O,
    pub context: C,
}
```

---

## 7. Checkpoint & Recovery

FASTER's checkpoint/recovery system provides durability and crash recovery through
a cooperative, non-blocking state machine that coordinates all active threads. The
Rust implementation preserves the proven CPR (Concurrent Prefix Recovery) algorithm
from C++/C# while expressing the state machine as an idiomatic Rust enum with
explicit transitions.

### 7.1 Checkpoint Types

FASTER supports three checkpoint types, each with different trade-offs:

#### 7.1.1 Fold-Over Checkpoint

```
Before checkpoint:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD          READ-ONLY           MUTABLE     TAIL   │
  │  ├──────────────┼─────────────────────┼─────────┤    │
  └──────────────────────────────────────────────────────┘

After fold-over:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD     FLUSHED (was mutable)  NEW-MUTABLE   TAIL   │
  │  ├──────────────────────────────────┼──────────┤     │
  └──────────────────────────────────────────────────────┘
  The previously mutable region is flushed to disk and becomes read-only.
  New writes go to the fresh tail region.
```

- **Mechanism:** Flush the current in-memory mutable region to disk. The mutable
  region becomes read-only. New writes proceed at the tail.
- **Persisted:** Hybrid log (on-disk portion + flushed in-memory pages) + hash index pages + metadata.
- **Cost:** Lowest — no data copying, just flushing dirty pages.
- **Trade-off:** Recovery requires replaying the log from `flushedAddress` to `finalAddress`
  to reconstruct the hash index for records that were in the mutable region.
- **When to use:** High-throughput workloads where checkpoint frequency is high and
  recovery time is acceptable. Best for write-heavy workloads.

#### 7.1.2 Snapshot Checkpoint

```
During snapshot:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD          READ-ONLY           MUTABLE     TAIL   │
  │  ├──────────────┼─────────────────────┼─────────┤    │
  └──────────────────────────────────────────────────────┘
                         │
                    snapshot.dat ← copy of in-memory pages
                         │
  Concurrent writes continue in the mutable region.
  CPR ensures snapshot captures a consistent point-in-time view.
```

- **Mechanism:** Copy the in-memory log pages to a separate snapshot file. The main
  log continues accepting writes concurrently (version separation via CPR).
- **Persisted:** Snapshot file + hash index pages + metadata.
- **Cost:** Higher — requires copying the in-memory log (proportional to memory size).
- **Trade-off:** Recovery is faster because the snapshot is a complete point-in-time
  image; no log replay needed.
- **When to use:** When fast recovery is critical and checkpoint frequency is low.

#### 7.1.3 Incremental Snapshot (MVP+)

- **Mechanism:** After an initial snapshot, subsequent checkpoints capture only the
  records that changed (delta log). Recovery applies deltas on top of the base snapshot.
- **Persisted:** Delta log file + metadata referencing the base snapshot.
- **Cost:** Medium — proportional to the number of changes since the last checkpoint,
  not the total log size.
- **When to use:** Update-heavy workloads where most records are unchanged between
  checkpoints. Dramatically reduces checkpoint I/O for large stores with low churn.

**MVP scope:** Fold-over and snapshot checkpoints. Incremental snapshot is deferred
to Full phase per the roadmap (Section 2, decisions.md) but the metadata format
reserves fields for it.

### 7.2 Checkpoint State Machine

The checkpoint state machine coordinates all threads to take a consistent checkpoint
without stopping operations. Each thread independently detects phase transitions
by comparing the global `SystemState` to its local copy, then executes
phase-specific logic before advancing.

#### State Representation

```rust
/// Global system state, packed into a single AtomicU64 for lock-free transitions.
///
/// Layout:
///   [63..56] Action (8 bits)  — what kind of state machine is running
///   [55..48] Phase  (8 bits)  — current phase within that state machine
///   [47..16] Version (32 bits) — monotonically increasing checkpoint version
///   [15..0]  Reserved (16 bits) — future use (e.g., sub-phase)
#[repr(transparent)]
pub struct SystemState(AtomicU64);

/// The type of coordinated action in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    /// No coordinated action. Normal operation.
    None = 0,
    /// Full checkpoint (index + log).
    CheckpointFull = 1,
    /// Index-only checkpoint.
    CheckpointIndex = 2,
    /// Log-only checkpoint.
    CheckpointLog = 3,
    /// Hash table grow (double).
    GrowIndex = 4,
    /// Garbage collection / log compaction.
    GC = 5,
}

/// Phase within a checkpoint action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// No active phase. Normal operation.
    Rest = 0,
    /// Prepare index checkpoint (allocate metadata, tokens).
    PrepIndexCheckpoint = 1,
    /// Write hash index pages to disk asynchronously.
    IndexCheckpoint = 2,
    /// Threads acknowledge checkpoint start; capture addresses.
    Prepare = 3,
    /// Version bump (v → v+1). Threads enter new version.
    /// Records created in v+1 are not part of the v checkpoint.
    InProgress = 4,
    /// Wait for all pending I/O operations to complete.
    WaitPending = 5,
    /// Wait for all page flushes to reach disk.
    WaitFlush = 6,
    /// Write metadata, invoke user completion callback.
    PersistenceCallback = 7,
}
```

#### Phase Transition Diagram (Full Checkpoint)

```
                    User calls store.checkpoint(CheckpointType::FoldOver)
                                          │
                                          ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  REST (version = v)                                                  │
  │    Global state: Action::None, Phase::Rest, version=v               │
  └──────────────┬───────────────────────────────────────────────────────┘
                 │ Coordinator sets global state to:
                 │   Action::CheckpointFull, Phase::PrepIndexCheckpoint
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PREP_INDEX_CHECKPOINT                                               │
  │    • Allocate unique checkpoint token (UUID).                        │
  │    • Initialize IndexCheckpointInfo and LogCheckpointInfo structs.   │
  │    • Each thread acknowledges by updating its local phase.           │
  │    • When all threads have acknowledged → advance to next phase.     │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  INDEX_CHECKPOINT                                                    │
  │    • Write hash table pages to disk asynchronously (page by page).   │
  │    • Write overflow buckets to disk.                                 │
  │    • Track outstanding I/O count via AtomicU32.                      │
  │    • Operations continue normally during this phase.                 │
  │    • When all index I/O completes → advance.                         │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PREPARE (version = v)                                               │
  │    • Each thread records:                                            │
  │      - checkpoint_start_address = current tail_address               │
  │      - Its current serial number (operation counter)                 │
  │    • Threads close their mutable-region page references.             │
  │    • Thread acknowledges phase; when all acknowledge → advance.      │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  IN_PROGRESS (version = v+1)                                         │
  │    • Version bumps from v to v+1.                                    │
  │    • Threads entering this phase start creating v+1 records.         │
  │    • Any update to a v record triggers CPR (Copy-on-Write):          │
  │      The v record is preserved, a new v+1 record is created.         │
  │    • This ensures the checkpoint captures a consistent v snapshot.   │
  │    • Thread acknowledges; when all in v+1 → advance.                 │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  WAIT_PENDING                                                        │
  │    • Each thread completes all its pending I/O operations.           │
  │    • No new pending operations are created for version v.            │
  │    • When all threads report zero pending v ops → advance.           │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  WAIT_FLUSH                                                          │
  │    • Flush dirty pages from HeadAddress to checkpoint_start_address  │
  │      to the device (fold-over) or to a snapshot file (snapshot).     │
  │    • Track outstanding flush I/O via AtomicU32.                      │
  │    • When all flushes complete → advance.                            │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PERSISTENCE_CALLBACK                                                │
  │    • Write checkpoint metadata to disk:                              │
  │      - IndexCheckpointInfo → info.dat + ht.dat                      │
  │      - LogCheckpointInfo → info.dat (+ snapshot.dat if snapshot)     │
  │    • Invoke user's on_checkpoint_complete callback (if registered).  │
  │    • Each thread acknowledges; when all acknowledge → advance.       │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  REST (version = v+1)                                                │
  │    Global state: Action::None, Phase::Rest, version=v+1             │
  │    Checkpoint complete. System resumes normal operation.             │
  └──────────────────────────────────────────────────────────────────────┘
```

#### Thread Coordination via Epoch Actions

Threads do not poll a shared "all threads done" counter. Instead, each thread
registers an epoch action that fires when it refreshes its epoch:

```rust
impl Session {
    /// Called on every operation (or periodically) to check for phase transitions.
    fn refresh_phase(&mut self) {
        let global = self.store.system_state.load(Ordering::Acquire);
        if global != self.local_system_state {
            self.handle_phase_transition(global);
        }
    }

    fn handle_phase_transition(&mut self, new_state: SystemState) {
        match (self.local_system_state.phase(), new_state.phase()) {
            (Phase::Rest, Phase::PrepIndexCheckpoint) => {
                // Acknowledge transition. No special action needed.
            }
            (Phase::PrepIndexCheckpoint, Phase::Prepare) => {
                // Record checkpoint start address and serial number.
                self.checkpoint_start_address = self.store.log.tail_address();
                self.checkpoint_serial_num = self.current_serial_num;
            }
            (Phase::Prepare, Phase::InProgress) => {
                // Enter new version. All subsequent records are v+1.
                self.version = new_state.version();
            }
            (Phase::InProgress, Phase::WaitPending) => {
                // Complete all pending operations for version v.
                self.complete_pending_for_version(new_state.version() - 1);
            }
            // ... other transitions
            _ => {}
        }
        // Mark this thread as having completed the transition.
        self.store.epoch.mark_phase_complete(self.thread_id);
        self.local_system_state = new_state;
    }
}
```

The coordinator (which can be any thread that initiated the checkpoint) advances
the global phase only after all registered threads have completed the current phase.
This is detected via the epoch system: when a safe epoch is reached where all
threads have acknowledged, the coordinator atomically advances the global state.

### 7.3 What Gets Persisted

A complete checkpoint produces the following on-disk artifacts:

```
{checkpoint_dir}/
├── index-checkpoints/
│   └── {token}/                    # UUID-named directory
│       ├── info.dat                # IndexCheckpointInfo (binary)
│       └── ht.dat                  # Hash table pages (raw binary)
│
├── log-checkpoints/
│   └── {token}/                    # UUID-named directory
│       ├── info.dat                # LogCheckpointInfo (binary)
│       ├── snapshot.dat            # (snapshot type only) Log snapshot
│       └── delta.dat               # (incremental only, future) Delta log
│
└── log/
    ├── log.0                       # Main log segment 0
    ├── log.1                       # Main log segment 1
    └── ...                         # Segments up to current tail
```

#### IndexCheckpointInfo (binary format)

```rust
/// Persisted index checkpoint metadata.
/// Written as a packed binary struct for fast serialization.
#[repr(C, packed)]
pub struct IndexCheckpointInfo {
    /// Format version (for forward compatibility). Currently 1.
    pub format_version: u32,
    /// Unique checkpoint token.
    pub token: Uuid,
    /// Number of main hash table buckets (power of 2).
    pub table_size: u64,
    /// Size of main hash table in bytes.
    pub num_ht_bytes: u64,
    /// Size of overflow buckets in bytes.
    pub num_ofb_bytes: u64,
    /// Number of overflow buckets allocated.
    pub ofb_count: u64,
    /// Log begin address at checkpoint time.
    pub log_begin_address: u64,
    /// Log address where checkpoint scan started.
    pub checkpoint_start_address: u64,
    /// Checksum (XOR of all preceding fields cast to u64).
    pub checksum: u64,
}
```

#### LogCheckpointInfo (binary format)

```rust
/// Persisted hybrid log checkpoint metadata.
#[repr(C)]
pub struct LogCheckpointInfo {
    /// Format version. Currently 1. Will increment for incremental snapshot support.
    pub format_version: u32,
    /// Unique checkpoint token.
    pub token: Uuid,
    /// Checkpoint type (fold-over = 0, snapshot = 1, incremental = 2).
    pub checkpoint_type: u8,
    /// Checkpoint version (v).
    pub version: u32,
    /// Next version (v+1) — for CPR recovery.
    pub next_version: u32,
    /// Address up to which the log has been flushed to the main device.
    pub flushed_logical_address: u64,
    /// Final logical address at checkpoint time (tail at PREPARE phase).
    pub final_logical_address: u64,
    /// Start logical address (begin of addressable log).
    pub start_logical_address: u64,
    /// Begin address (oldest non-truncated address).
    pub begin_address: u64,
    /// Head address at checkpoint time.
    pub head_address: u64,
    /// Snapshot-specific: final address in the snapshot file (−1 if not snapshot).
    pub snapshot_final_logical_address: i64,
    /// Incremental-specific: tail address in the delta log (−1 if not incremental).
    pub delta_tail_address: i64,
    /// Number of sessions with commit points.
    pub num_sessions: u32,
    // Followed by: num_sessions × SessionCommitInfo (variable-length)
    /// Checksum (XOR-based).
    pub checksum: u64,
}

/// Per-session checkpoint commit point.
#[repr(C)]
pub struct SessionCommitInfo {
    /// Session identifier (UUID).
    pub session_id: Uuid,
    /// Last committed operation serial number.
    pub serial_number: u64,
    /// Number of excluded (pending-at-checkpoint-time) operations.
    pub num_exclusions: u32,
    // Followed by: num_exclusions × u64 (excluded serial numbers)
}
```

**Format choice: binary, not text.**

Rationale:
- **Performance:** Binary is faster to serialize/deserialize (no parsing, no allocation for strings).
- **Size:** Binary is smaller (matters for metadata checksum computation).
- **Versioning:** The `format_version` field enables forward-compatible evolution.
  When adding fields, bump the version and append new fields at the end.
- **Trade-off vs. C#'s text format:** C#'s text `info.dat` was chosen for human
  debuggability. In Rust, we provide a `faster-inspect` CLI tool that can dump
  checkpoint metadata in human-readable form, achieving the same debuggability
  without runtime overhead.

**Cross-language compatibility:** The Rust on-disk format is NOT compatible with
C++ or C# checkpoint files. This is intentional:
- The Rust implementation may use different alignment, different metadata fields,
  and a different hash function.
- Cross-language migration would require an explicit export/import tool, which is
  out of scope for v1.
- The format is fully documented here and in the `faster-inspect` tool for
  third-party tooling.

### 7.4 Recovery Flow

Recovery reconstructs a `FasterKv` from a checkpoint. The process is deterministic
and single-threaded (no concurrent operations during recovery).

```
Recovery(checkpoint_token) → Result<RecoveryInfo, FasterError>
```

**Step 1: Locate checkpoint.**
```rust
// Find the latest valid checkpoint, or use a specific token.
let (index_info, log_info) = match token {
    Some(t) => load_checkpoint_metadata(checkpoint_dir, t)?,
    None    => find_latest_checkpoint(checkpoint_dir)?,
};

// Validate: index checkpoint must not be newer than log checkpoint.
if index_info.checkpoint_start_address > log_info.final_logical_address {
    return Err(FasterError::Checkpoint(
        "Index checkpoint is newer than log checkpoint".into()
    ));
}

// Validate checksums.
index_info.verify_checksum()?;
log_info.verify_checksum()?;
```

**Step 2: Restore hash index.**
```rust
// Read ht.dat into memory.
let ht_bytes = device.read_sync(index_path.join("ht.dat"))?;
hash_table.restore_from_bytes(&ht_bytes, index_info.table_size)?;

// Restore overflow buckets if present.
if index_info.num_ofb_bytes > 0 {
    let ofb_bytes = device.read_sync(index_path.join("ofb.dat"))?;
    hash_table.restore_overflow(&ofb_bytes)?;
}
```

**Step 3: Restore hybrid log.**

For **fold-over** checkpoints:
```rust
// Set log address boundaries from metadata.
log.set_begin_address(log_info.begin_address);
log.set_head_address(log_info.head_address);
log.set_read_only_address(log_info.flushed_logical_address);
log.set_tail_address(log_info.final_logical_address);

// Read log pages from flushed_address to final_address into memory.
// These pages were in the mutable region at checkpoint time and were
// flushed to disk as part of the fold-over.
let num_pages = pages_between(log_info.flushed_logical_address,
                              log_info.final_logical_address);
for page_idx in 0..num_pages {
    let page_address = log_info.flushed_logical_address + page_idx * page_size;
    let buffer = log.allocate_page(page_address);
    device.read_sync(log_segment_file(page_address), buffer)?;
}

// Scan recovered pages to undo v+1 records.
// Records with version > log_info.version were created after the checkpoint
// started (during IN_PROGRESS phase) and must be invalidated.
undo_next_version(&mut log, &hash_table, log_info.version)?;
```

For **snapshot** checkpoints:
```rust
// Read the snapshot file into memory.
let snapshot_data = device.read_sync(log_path.join("snapshot.dat"))?;
log.restore_from_snapshot(&snapshot_data, &log_info)?;

// For incremental: apply delta log on top of base snapshot.
if log_info.delta_tail_address >= 0 {
    let delta_data = device.read_sync(log_path.join("delta.dat"))?;
    log.apply_delta(&delta_data, &log_info)?;
}
```

**Step 4: Restore session commit points.**
```rust
let mut commit_points = Vec::with_capacity(log_info.num_sessions as usize);
for session_info in log_info.sessions() {
    commit_points.push(CommitPoint {
        session_id: session_info.session_id,
        serial_number: session_info.serial_number,
        exclusions: session_info.exclusions().to_vec(),
    });
}
```

**Step 5: Return recovery info to caller.**
```rust
/// Information about a completed recovery, returned to the caller.
pub struct RecoveryInfo {
    /// The checkpoint token that was recovered.
    pub token: Uuid,
    /// Per-session commit points. The caller uses these to resume sessions
    /// and determine which operations are durable.
    pub commit_points: Vec<CommitPoint>,
    /// The recovered version number.
    pub version: u32,
}

/// A session commit point from a recovered checkpoint.
pub struct CommitPoint {
    pub session_id: Uuid,
    /// The last operation serial number that is durable.
    pub serial_number: u64,
    /// Serial numbers of operations that went pending during the checkpoint
    /// and are NOT durable. The application must retry these.
    pub exclusions: Vec<u64>,
}
```

The caller can then resume sessions using the commit points:
```rust
let recovery_info = store.recover(Some(checkpoint_token))?;
for cp in &recovery_info.commit_points {
    let session = store.resume_session(cp.session_id, my_functions)?;
    // session.serial_number() == cp.serial_number
    // Application must replay operations after cp.serial_number,
    // excluding cp.exclusions.
}
```

### 7.5 On-Disk Format Versioning

All checkpoint metadata structures include a `format_version` field at offset 0.
This enables non-breaking format evolution:

- **Version 1** (MVP): Current format as specified above.
- **Future versions:** New fields are appended at the end. Readers that encounter
  a higher version than they support will read only the fields they understand,
  with unknown trailing bytes ignored. This is safe because:
  1. The checksum covers all bytes, so corruption is detected.
  2. New fields are additive (no reordering of existing fields).
  3. Readers validate that `format_version >= MINIMUM_SUPPORTED_VERSION`.

If a breaking format change is ever needed (e.g., field reordering), the
`format_version` is bumped to a new major number and the reader includes migration
logic.

---

## 8. Public API Design

This section specifies the complete public API surface of the Rust FASTER core crate.
The design prioritizes safety, ergonomics, and zero-cost abstraction while remaining
agnostic to any async runtime. Every type, trait, and method documented here is part
of the stable public contract.

### 8.1 Top-Level Type Architecture

```rust
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// The core crate's public type hierarchy
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// The FASTER key-value store.
///
/// Owns the hash index, hybrid log, epoch system, and device.
/// Thread-safe: can be shared across threads via Arc<FasterKv<K, V, D>>.
/// Operations are performed through Session handles, not directly on the store.
pub struct FasterKv<K, V, D: Device> { /* ... */ }

/// Builder for configuring and constructing a FasterKv instance.
pub struct FasterKvBuilder<K, V, D: Device> { /* ... */ }

/// A per-thread session handle for performing operations.
///
/// NOT Send — must be used on the thread that created it.
/// This is enforced at compile time via PhantomData<*const ()>.
/// Sessions borrow the FasterKv (via Arc) and hold epoch protection.
pub struct Session<'store, K, V, F: Functions<Key = K, Value = V>> { /* ... */ }

/// User-defined operation callbacks (see Section 6.8 for full definition).
pub trait Functions { /* ... */ }

/// Storage device abstraction.
pub trait Device: Send + Sync + 'static { /* ... */ }

/// Operation result (see Section 6.1).
pub type OperationResult = Result<Status, FasterError>;
```

**Relationship diagram:**

```
    Arc<FasterKv<K, V, D>>
         │
         │  store.new_session(functions)
         │
         ▼
    Session<'_, K, V, F>          ◄── NOT Send (thread-local)
         │
         │  session.read(&key, &input, &mut output)
         │  session.upsert(&key, &value, &mut output)
         │  session.rmw(&key, &input, &mut output)
         │  session.delete(&key)
         │  session.complete_pending(wait)
         │
         ▼
    OperationResult
         │
         ├── Ok(Status::Ok(OkKind))        Fast-path completion
         ├── Ok(Status::Pending)            Slow-path, I/O issued
         ├── Ok(Status::NotFound)           Key absent
         └── Err(FasterError)               System error
```

### 8.2 Generic Type Parameters

#### Key Bounds

```rust
/// Trait bound for keys stored in FASTER.
///
/// Keys must be hashable, comparable, cloneable (for pending operations),
/// and serializable to bytes (for on-disk storage).
pub trait Key: Hash + Eq + Clone + Send + Sync + 'static {
    /// Serialize this key into a byte buffer.
    /// Returns the number of bytes written.
    ///
    /// The buffer is guaranteed to be at least `serialized_size()` bytes.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Return the exact serialized size of this key in bytes.
    fn serialized_size(&self) -> usize;

    /// Deserialize a key from a byte buffer.
    fn deserialize(buf: &[u8]) -> Self;
}
```

**Rationale:** We use a custom `Key` trait rather than `AsRef<[u8]>` because:
1. `AsRef<[u8]>` doesn't cover deserialization.
2. Fixed-size keys (u64, u128) can implement zero-copy serialization.
3. Variable-length keys (String, Vec<u8>) need explicit size tracking.
4. We provide blanket implementations for common types:

```rust
// Blanket implementation for fixed-size numeric types.
macro_rules! impl_key_for_numeric {
    ($($t:ty),*) => {
        $(impl Key for $t {
            fn serialize(&self, buf: &mut [u8]) -> usize {
                let bytes = self.to_le_bytes();
                buf[..bytes.len()].copy_from_slice(&bytes);
                bytes.len()
            }
            fn serialized_size(&self) -> usize { std::mem::size_of::<$t>() }
            fn deserialize(buf: &[u8]) -> Self {
                Self::from_le_bytes(buf[..std::mem::size_of::<$t>()].try_into().unwrap())
            }
        })*
    }
}
impl_key_for_numeric!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

// Implementation for byte slices and strings.
impl Key for Vec<u8> { /* length-prefixed serialization */ }
impl Key for String  { /* length-prefixed UTF-8 serialization */ }
```

#### Value Handling: Fixed-Size vs. Variable-Length

```rust
/// Trait bound for values stored in FASTER.
///
/// Values must be cloneable and serializable. Unlike keys, values can
/// be variable-length and may be modified in-place.
pub trait Value: Clone + Send + Sync + 'static {
    /// Serialize this value into a byte buffer.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Return the exact serialized size.
    fn serialized_size(&self) -> usize;

    /// Deserialize a value from a byte buffer.
    fn deserialize(buf: &[u8]) -> Self;

    /// Whether this value type has a fixed serialized size.
    /// If true, the log can optimize allocation by pre-computing record sizes.
    ///
    /// Default: false (variable-length).
    fn is_fixed_size() -> bool { false }

    /// For fixed-size values, return the constant size.
    /// Panics if `is_fixed_size()` returns false.
    fn fixed_size() -> usize {
        panic!("fixed_size() called on variable-length value")
    }
}
```

**Design decision: inline storage (C++ model), not dual-log (C# model).**

All values are stored inline in the hybrid log — key and value are contiguous in the
same record. This is the C++ approach and is simpler, more cache-friendly, and avoids
the complexity of a separate object log with its own GC.

For variable-length values, the record size includes the serialized value bytes.
When a value grows (e.g., appending to a vector), the `rmw_in_place` callback
returns `NeedsNewRecord`, and the core allocates a new, larger record at the tail.

#### Device Trait

```rust
/// Abstraction over storage I/O backends.
///
/// Device implementations are provided per-platform and per-runtime:
/// - `FileDevice` — synchronous file I/O (default, works everywhere)
/// - `NullDevice` — no-op (for testing and benchmarking)
/// - `faster_tokio::TokioDevice` — Tokio-based async file I/O (in adapter crate)
/// - `faster_compio::CompioDevice` — io_uring-based async I/O (in adapter crate)
///
/// The core crate only depends on this trait, not on any concrete implementation.
pub trait Device: Send + Sync + 'static {
    /// Read `buf.len()` bytes from the given segment at the given offset.
    ///
    /// For synchronous devices: blocks until I/O completes, then calls `callback`.
    /// For async-capable devices: issues I/O and calls `callback` on completion.
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,  // opaque pointer passed through to callback
    );

    /// Write `buf.len()` bytes to the given segment at the given offset.
    fn write_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &[u8],
        callback: IoCallback,
        context: *mut u8,
    );

    /// Synchronous read (blocking). Used during recovery.
    fn read_sync(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), std::io::Error>;

    /// Synchronous write (blocking). Used during checkpoint metadata writes.
    fn write_sync(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &[u8],
    ) -> Result<(), std::io::Error>;

    /// Physical sector size (for alignment). Typically 512 or 4096.
    fn sector_size(&self) -> u32;

    /// Maximum segment size in bytes.
    fn segment_size(&self) -> u64;

    /// Truncate all segments before the given address.
    /// Used during log compaction and old checkpoint cleanup.
    fn truncate_until(&self, segment_id: u64);
}

/// I/O completion callback signature.
///
/// `result`: Ok(bytes_transferred) or Err(io::Error).
/// `context`: the opaque pointer that was passed to read_async/write_async.
pub type IoCallback = unsafe extern "C" fn(
    result: IoResult,
    context: *mut u8,
);

pub type IoResult = Result<u32, std::io::Error>;
```

**Rationale for `unsafe extern "C" fn` callback:**
- Maximum interoperability with C FFI and with async runtime adapters.
- No allocation overhead (no `Box<dyn FnOnce>`).
- The `unsafe` is contained within the core — public API users never see it.
- The `context: *mut u8` opaque pointer pattern is standard for C-style callbacks.

### 8.3 FasterKv Store

```rust
impl<K: Key, V: Value, D: Device> FasterKv<K, V, D> {
    /// Create a new store using the builder (preferred).
    /// See FasterKvBuilder for configuration options.

    /// Create a new session for performing operations.
    ///
    /// Each thread should create its own session. Sessions are NOT Send.
    /// The session borrows the store via Arc, so the store outlives all sessions.
    pub fn new_session<F: Functions<Key = K, Value = V>>(
        self: &Arc<Self>,
        functions: F,
    ) -> Session<'_, K, V, F>;

    /// Resume a session from a previous checkpoint.
    ///
    /// `session_id` must match a session ID from the recovered checkpoint.
    /// Returns the session and the commit point (last durable serial number + exclusions).
    pub fn resume_session<F: Functions<Key = K, Value = V>>(
        self: &Arc<Self>,
        session_id: Uuid,
        functions: F,
    ) -> Result<(Session<'_, K, V, F>, CommitPoint), FasterError>;

    // ── Checkpointing ────────────────────────────────────────────────────

    /// Initiate a checkpoint. Returns the checkpoint token.
    ///
    /// This is non-blocking: the checkpoint proceeds in the background
    /// as threads cooperate through the state machine.
    /// Use `checkpoint_complete()` or the callback to wait for completion.
    pub fn checkpoint(
        &self,
        checkpoint_type: CheckpointType,
    ) -> Result<Uuid, FasterError>;

    /// Check if the most recent checkpoint has completed.
    pub fn is_checkpoint_complete(&self) -> bool;

    /// Block until the current checkpoint completes.
    pub fn wait_for_checkpoint(&self) -> Result<(), FasterError>;

    /// Register a callback invoked when a checkpoint completes.
    pub fn on_checkpoint_complete<F: Fn(Uuid) + Send + Sync + 'static>(
        &self,
        callback: F,
    );

    // ── Recovery ─────────────────────────────────────────────────────────

    /// Recover the store from a checkpoint.
    ///
    /// If `token` is None, recovers from the latest checkpoint.
    /// Must be called before any sessions are created.
    pub fn recover(&self, token: Option<Uuid>) -> Result<RecoveryInfo, FasterError>;

    // ── Index Management ─────────────────────────────────────────────────

    /// Double the hash index size.
    ///
    /// This is a coordinated operation (like checkpoint) that proceeds
    /// through a state machine as threads cooperate.
    pub fn grow_index(&self) -> Result<(), FasterError>;

    /// Return the number of entries in the hash index.
    pub fn entry_count(&self) -> u64;

    /// Return the hash index size in buckets.
    pub fn index_size(&self) -> u64;

    // ── Log Management ───────────────────────────────────────────────────

    /// Return a read-only accessor to the hybrid log.
    /// Used for iteration/scanning.
    pub fn log(&self) -> &LogAccessor<K, V>;

    /// Current tail address (next write position).
    pub fn tail_address(&self) -> u64;

    /// Current head address (oldest in-memory address).
    pub fn head_address(&self) -> u64;

    /// Current read-only address (boundary between mutable and read-only).
    pub fn read_only_address(&self) -> u64;

    /// Begin address (oldest non-truncated address).
    pub fn begin_address(&self) -> u64;
}
```

### 8.4 Session Lifecycle and Operations

```rust
/// A per-thread session for performing FASTER operations.
///
/// # Thread Safety
///
/// Sessions are NOT Send — they must be used on the thread that created them.
/// This is enforced at compile time. The reason is that sessions hold per-thread
/// epoch state and a mutable cursor into the pending operation queue. Sharing
/// across threads would violate epoch invariants.
///
/// # Lifecycle
///
/// Sessions follow RAII: when dropped, they complete all pending operations,
/// release epoch protection, and deregister from the store.
///
/// # Epoch Protection
///
/// Every operation call automatically enters/exits epoch protection (the
/// "BasicContext" model from C#). For performance-critical code paths that
/// perform many operations in a batch, use `unsafe_enter_epoch()` /
/// `unsafe_exit_epoch()` to amortize the overhead — but you must ensure
/// the epoch is exited before blocking.
pub struct Session<'store, K, V, F: Functions<Key = K, Value = V>> {
    // PhantomData<*const ()> makes this !Send + !Sync
    _not_send: PhantomData<*const ()>,
    // ... internal state
}

impl<'store, K: Key, V: Value, F: Functions<Key = K, Value = V>>
    Session<'store, K, V, F>
{
    // ── Core Operations ──────────────────────────────────────────────────

    /// Read the value for a key.
    ///
    /// On success, the result is written to `output` via `Functions::read()`.
    /// If the record is on disk, returns `Status::Pending`. Call
    /// `complete_pending()` to retrieve the result.
    pub fn read(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Insert or update a value.
    ///
    /// Never goes pending (always completes immediately).
    pub fn upsert(
        &mut self,
        key: &K,
        value: &V,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Read-modify-write: atomically read and update a value.
    ///
    /// May go pending if the record is on disk (must read before modify).
    pub fn rmw(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Delete a key.
    ///
    /// Never goes pending (always completes immediately via tombstone).
    /// Idempotent: deleting a non-existent key returns NotFound.
    pub fn delete(&mut self, key: &K) -> OperationResult;

    // ── Pending Operation Management ─────────────────────────────────────

    /// Complete pending operations.
    ///
    /// If `wait` is true, blocks until ALL pending operations complete.
    /// If `wait` is false, processes only already-completed operations.
    ///
    /// Returns the number of operations completed.
    pub fn complete_pending(&mut self, wait: bool) -> Result<usize, FasterError>;

    /// Non-blocking: returns the number of pending operations that have
    /// completed since the last call to complete_pending / try_complete_pending.
    pub fn try_complete_pending(&mut self) -> Result<usize, FasterError>;

    /// Return the number of outstanding pending operations.
    pub fn pending_count(&self) -> usize;

    // ── Epoch Management (Advanced) ──────────────────────────────────────

    /// Manually enter epoch protection (unsafe context).
    ///
    /// When in this mode, operations skip the per-operation epoch enter/exit.
    /// You MUST call `unsafe_exit_epoch()` before blocking or yielding.
    ///
    /// # Safety
    /// Failing to exit the epoch before blocking will stall the epoch system
    /// and prevent garbage collection and checkpoint progress.
    pub unsafe fn unsafe_enter_epoch(&mut self);

    /// Exit epoch protection after a batch of operations.
    pub unsafe fn unsafe_exit_epoch(&mut self);

    // ── Metadata ─────────────────────────────────────────────────────────

    /// The session's unique identifier.
    pub fn id(&self) -> Uuid;

    /// The monotonically increasing operation serial number.
    /// Useful for tracking checkpoint commit points.
    pub fn serial_number(&self) -> u64;

    /// Refresh the session's view of the global state.
    /// Called implicitly by operations, but can be called explicitly.
    pub fn refresh(&mut self);
}

impl<'store, K, V, F: Functions<Key = K, Value = V>> Drop
    for Session<'store, K, V, F>
{
    fn drop(&mut self) {
        // 1. Complete all pending operations (best-effort).
        let _ = self.complete_pending(true);
        // 2. Release epoch protection.
        // 3. Deregister session from the store.
    }
}
```

### 8.5 FasterKvBuilder

```rust
/// Builder for constructing a configured FasterKv instance.
///
/// All settings have sensible defaults. The only required parameter is
/// the device (or a path for the default FileDevice).
pub struct FasterKvBuilder<K, V, D: Device> {
    _phantom: PhantomData<(K, V, D)>,
    // ... configuration fields
}

impl<K: Key, V: Value, D: Device> FasterKvBuilder<K, V, D> {
    /// Create a new builder with the given device.
    pub fn new(device: D) -> Self;

    /// Set the initial hash index size in number of buckets.
    /// Must be a power of 2. Default: 2^20 (1M buckets = 64 MB).
    pub fn index_size(mut self, num_buckets: u64) -> Self;

    /// Set the log page size in bytes.
    /// Must be a power of 2. Default: 2^25 (32 MB).
    pub fn page_size(mut self, bytes: u64) -> Self;

    /// Set the total in-memory log size in bytes.
    /// Must be a power of 2 and >= 2 * page_size.
    /// Default: 2^34 (16 GB).
    pub fn memory_size(mut self, bytes: u64) -> Self;

    /// Set the log segment size in bytes.
    /// Controls how large each on-disk log file grows before rolling.
    /// Must be a power of 2 and >= page_size.
    /// Default: 2^30 (1 GB).
    pub fn segment_size(mut self, bytes: u64) -> Self;

    /// Set the mutable fraction of the in-memory log.
    /// Must be in (0.0, 1.0]. Default: 0.9.
    ///
    /// A higher fraction means more records can be updated in-place
    /// (better write performance) but less read-only buffer space
    /// (more frequent page flushes).
    pub fn mutable_fraction(mut self, fraction: f64) -> Self;

    /// Set the checkpoint directory path.
    /// If not set, checkpoints are stored alongside the log device.
    pub fn checkpoint_dir(mut self, path: impl Into<PathBuf>) -> Self;

    /// Enable or disable pre-allocation of log pages.
    /// When true, all in-memory log pages are allocated at startup.
    /// Default: false (allocate lazily).
    pub fn preallocate_log(mut self, enable: bool) -> Self;

    /// Set the maximum number of concurrent pending I/O operations per session.
    /// Default: 4096.
    pub fn max_pending_per_session(mut self, count: u32) -> Self;

    /// Build the FasterKv instance.
    /// Returns an Arc-wrapped instance ready for session creation.
    pub fn build(self) -> Result<Arc<FasterKv<K, V, D>>, FasterError>;
}
```

**Validation in `build()`:** The builder validates all parameters at construction time
and returns a descriptive error if any constraint is violated (e.g., non-power-of-2
sizes, mutable_fraction out of range). This is fail-fast: no runtime panics.

### 8.6 Iterator / Scan API

FASTER supports scanning the hybrid log for all records. This is useful for
offline analytics, data export, and consistency verification.

```rust
/// Read-only accessor for the hybrid log.
pub struct LogAccessor<K, V> { /* ... */ }

impl<K: Key, V: Value> LogAccessor<K, V> {
    /// Scan all live records in the log from `begin_address` to `until_address`.
    ///
    /// Records are yielded in log order (oldest to newest).
    /// Tombstoned and invalidated records are skipped.
    /// For records that appear multiple times (version chain), only the latest is yielded.
    ///
    /// This is a heavyweight operation — it reads the entire on-disk log.
    pub fn scan(
        &self,
        begin_address: u64,
        until_address: u64,
    ) -> LogScanIterator<'_, K, V>;
}

/// Iterator over log records.
///
/// Yields (Key, Value, RecordMetadata) tuples.
/// Automatically issues I/O for on-disk pages and caches loaded pages.
pub struct LogScanIterator<'log, K, V> { /* ... */ }

impl<'log, K: Key, V: Value> Iterator for LogScanIterator<'log, K, V> {
    type Item = Result<ScanRecord<K, V>, FasterError>;

    fn next(&mut self) -> Option<Self::Item>;
}

pub struct ScanRecord<K, V> {
    pub key: K,
    pub value: V,
    pub metadata: RecordMetadata,
}

pub struct RecordMetadata {
    /// Logical address in the hybrid log.
    pub address: u64,
    /// Checkpoint version that created this record.
    pub version: u32,
    /// Whether this record is a tombstone (should not appear in scan, but
    /// available if the caller explicitly requests tombstones).
    pub is_tombstone: bool,
}
```

**Rationale for synchronous Iterator (not async Stream):** The scan API lives in
the core crate, which has no async dependencies. Async adapter crates can wrap
`LogScanIterator` in a `Stream` implementation that yields pages asynchronously.
The synchronous iterator blocks on I/O internally when it needs to load a page
from disk — this is acceptable for offline/batch operations.

### 8.7 Idiomatic Rust Patterns

#### Ownership Model

```
Arc<FasterKv<K, V, D>>
     │
     │  Shared ownership: multiple threads hold Arc clones
     │
     ├── Thread 1: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
     │       │
     │       ├── read(&key, ...)    → borrows &mut self (exclusive)
     │       └── upsert(&key, ...) → borrows &mut self (exclusive)
     │
     ├── Thread 2: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
     │       └── rmw(&key, ...)
     │
     └── Thread 3: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
             └── delete(&key)
```

- **FasterKv** is `Send + Sync` (safe to share via Arc).
- **Session** is `!Send + !Sync` (enforced via `PhantomData<*const ()>`).
  - Operations take `&mut self`, preventing concurrent use of the same session.
  - This matches the mono-threaded session model from C#/C++ and is essential
    for epoch correctness.
- **Functions** implementors must be `Send + Sync` (shared across sessions if
  the same instance is reused, but typically each session gets its own).

#### No Raw Pointers in Public API

The public API exposes zero raw pointers. All pointer-based operations (hash table
CAS, record access, I/O buffers) are internal to the crate and wrapped in safe
abstractions. The `Device` trait uses raw pointers for I/O callbacks, but this is
an implementation detail — public users of the core API (including async adapter
crate authors) interact only with safe Rust types.

The C FFI layer (Section 9) necessarily uses raw pointers, but that is a separate
API surface explicitly marked `unsafe`.

#### Error Handling: Result, Not Panics

The core crate NEVER panics in production code paths. All fallible operations
return `Result`. The only panics are:
- Debug assertions (`debug_assert!`) for internal invariant checks (stripped in release).
- Explicit `unreachable!()` for logically impossible code paths.

#### Send + Sync Bounds

| Type | Send | Sync | Rationale |
|------|------|------|-----------|
| `FasterKv<K,V,D>` | ✅ | ✅ | Shared state protected by atomics and epochs |
| `Session<K,V,F>` | ❌ | ❌ | Thread-local epoch state; must stay on creator thread |
| `FasterKvBuilder` | ✅ | ✅ | Configuration only, no mutable shared state |
| `LogAccessor` | ✅ | ✅ | Read-only view of the log |
| `LogScanIterator` | ❌ | ❌ | Holds mutable iteration state and page cache |
| `Status` | ✅ | ✅ | Value type (Copy) |
| `FasterError` | ✅ | ✅ | Value type |

---

## 9. C FFI Layer

The `faster-ffi` crate exposes FASTER's functionality to C, C++, and any language
with C FFI support. The design uses opaque handles, integer error codes, and explicit
memory ownership to provide a safe, well-documented C API.

### 9.1 Opaque Handle Pattern

All Rust types are hidden behind opaque pointers. C callers never see the internal
layout — they hold only a pointer-sized handle and pass it to API functions.

```rust
// ── Opaque types (never dereferenced by C) ────────────────────────────

/// Opaque handle to a FasterKv store instance.
/// Created by `faster_kv_open()`, freed by `faster_kv_close()`.
#[repr(C)]
pub struct faster_kv_t {
    _opaque: [u8; 0],
}

/// Opaque handle to a session.
/// Created by `faster_session_open()`, freed by `faster_session_close()`.
#[repr(C)]
pub struct faster_session_t {
    _opaque: [u8; 0],
}

/// Opaque handle to a pending operation context.
/// Returned by operations that go pending. Freed after completion retrieval.
#[repr(C)]
pub struct faster_pending_t {
    _opaque: [u8; 0],
}
```

**Implementation pattern:** Each opaque pointer is actually a `Box<RealType>` cast
to a raw pointer via `Box::into_raw()`. On free, it is reconstituted via
`Box::from_raw()` and dropped. This ensures proper cleanup:

```rust
// Internal: create
fn wrap_handle<T>(value: T) -> *mut faster_kv_t {
    Box::into_raw(Box::new(value)) as *mut faster_kv_t
}

// Internal: free
unsafe fn unwrap_handle<T>(handle: *mut faster_kv_t) -> Box<T> {
    Box::from_raw(handle as *mut T)
}
```

### 9.2 Function Naming Convention

All exported functions follow the pattern:
```
faster_{noun}_{verb}[_{qualifier}]()
```

Examples:
- `faster_kv_open()` — open a store
- `faster_kv_close()` — close a store
- `faster_session_open()` — create a session
- `faster_session_read()` — perform a read
- `faster_session_complete_pending()` — complete pending operations
- `faster_error_message()` — get error details

### 9.3 Core API Functions

```c
/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Store lifecycle
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// Open a FASTER store.
///
/// @param path         Null-terminated UTF-8 path for log storage.
///                     Pass NULL for in-memory-only (NullDevice).
/// @param index_size   Hash index size in buckets (must be power of 2).
/// @param log_size     Total in-memory log size in bytes.
/// @param page_size    Log page size in bytes.
/// @param out_store    On success, receives the store handle.
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_open(
    const char* path,
    uint64_t index_size,
    uint64_t log_size,
    uint64_t page_size,
    faster_kv_t** out_store
);

/// Close a FASTER store and free all resources.
/// All sessions must be closed before calling this.
///
/// @param store        Store handle from faster_kv_open().
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_close(faster_kv_t* store);

/// Initiate a checkpoint.
///
/// @param store        Store handle.
/// @param type         0 = fold-over, 1 = snapshot.
/// @param out_token    On success, receives 16-byte UUID token.
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_checkpoint(
    faster_kv_t* store,
    int32_t type,
    uint8_t out_token[16]
);

/// Recover from a checkpoint.
///
/// @param store        Store handle (must be freshly opened, no sessions).
/// @param token        16-byte UUID token, or NULL for latest checkpoint.
/// @param out_info     On success, receives recovery info. Caller must
///                     free with faster_recovery_info_free().
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_recover(
    faster_kv_t* store,
    const uint8_t token[16],
    faster_recovery_info_t** out_info
);

/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Session lifecycle
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// User-provided function callbacks for C API.
/// C callers fill in this struct with function pointers.
typedef struct faster_functions_t {
    /// Read callback: copy value to output buffer.
    /// @param key, key_len     Key bytes.
    /// @param value, value_len Value bytes (from stored record).
    /// @param input, input_len User input bytes.
    /// @param output           Output buffer to write to.
    /// @param output_capacity  Max bytes available in output buffer.
    /// @param out_output_len   Actual bytes written to output.
    /// @param user_data        Opaque pointer from faster_session_open().
    void (*read)(
        const uint8_t* key, size_t key_len,
        const uint8_t* value, size_t value_len,
        const uint8_t* input, size_t input_len,
        uint8_t* output, size_t output_capacity, size_t* out_output_len,
        void* user_data
    );

    /// Upsert callback: produce the value to write.
    void (*upsert)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        const uint8_t* old_value, size_t old_value_len,  /* NULL if new record */
        uint8_t* new_value, size_t new_value_capacity, size_t* out_new_value_len,
        void* user_data
    );

    /// RMW initial callback: create initial value for missing key.
    void (*rmw_initial)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        uint8_t* value, size_t value_capacity, size_t* out_value_len,
        void* user_data
    );

    /// RMW in-place callback: update value in place.
    /// Returns 0 if update succeeded, 1 if needs new record (value grew).
    int32_t (*rmw_in_place)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        uint8_t* value, size_t value_len, size_t value_capacity,
        size_t* out_new_value_len,
        void* user_data
    );

    /// RMW copy-update callback: produce new value from old + input.
    void (*rmw_copy_update)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        const uint8_t* old_value, size_t old_value_len,
        uint8_t* new_value, size_t new_value_capacity, size_t* out_new_value_len,
        void* user_data
    );

    /// Optional: pending operation completion notification.
    /// Called from faster_session_complete_pending() for each completed op.
    void (*completion)(
        int32_t operation_type,  /* 0=read, 1=upsert, 2=rmw, 3=delete */
        int32_t status_code,
        const uint8_t* key, size_t key_len,
        const uint8_t* output, size_t output_len,
        void* user_context,
        void* user_data
    );
} faster_functions_t;

/// Open a session.
///
/// @param store        Store handle.
/// @param functions    User-provided callbacks. The struct is copied — the
///                     caller may free it after this call.
/// @param user_data    Opaque pointer passed to all callbacks.
/// @param out_session  On success, receives the session handle.
/// @return             0 on success, negative error code.
int32_t faster_session_open(
    faster_kv_t* store,
    const faster_functions_t* functions,
    void* user_data,
    faster_session_t** out_session
);

/// Close a session, completing all pending operations.
int32_t faster_session_close(faster_session_t* session);

/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Operations
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// Perform a read.
///
/// @param session      Session handle.
/// @param key          Key bytes.
/// @param key_len      Key length.
/// @param input        Input bytes (passed to read callback).
/// @param input_len    Input length.
/// @param output       Buffer for output (filled by read callback).
/// @param output_capacity  Max bytes available in output.
/// @param out_output_len   Actual output bytes written (if status >= 0).
/// @param user_context Opaque pointer for pending completion callback.
/// @return             Status code (see faster_status_* constants).
int32_t faster_session_read(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* input, size_t input_len,
    uint8_t* output, size_t output_capacity, size_t* out_output_len,
    void* user_context
);

/// Perform an upsert. Never goes pending.
int32_t faster_session_upsert(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* value, size_t value_len,
    const uint8_t* input, size_t input_len,
    void* user_context
);

/// Perform an RMW. May go pending (returns FASTER_STATUS_PENDING).
int32_t faster_session_rmw(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* input, size_t input_len,
    uint8_t* output, size_t output_capacity, size_t* out_output_len,
    void* user_context
);

/// Perform a delete. Never goes pending.
int32_t faster_session_delete(
    faster_session_t* session,
    const uint8_t* key, size_t key_len
);

/// Complete pending operations.
///
/// @param session      Session handle.
/// @param wait         If non-zero, block until all pending ops complete.
/// @return             Number of completed operations, or negative error code.
int32_t faster_session_complete_pending(
    faster_session_t* session,
    int32_t wait
);
```

### 9.4 Error Model

```c
/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Status codes (returned by all functions)
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

#define FASTER_OK                    0   /* Success */
#define FASTER_OK_IN_PLACE           1   /* Updated in-place */
#define FASTER_OK_CREATED            2   /* Created new record */
#define FASTER_OK_DELETED            3   /* Tombstone written */
#define FASTER_STATUS_PENDING        10  /* I/O pending, call complete_pending */
#define FASTER_STATUS_NOT_FOUND      11  /* Key not found */
#define FASTER_STATUS_RETRY          12  /* Transient, retry (internal) */

#define FASTER_ERR_IO               -1   /* I/O error */
#define FASTER_ERR_CORRUPTION       -2   /* Data corruption */
#define FASTER_ERR_INVALID_ARG      -3   /* Invalid argument */
#define FASTER_ERR_SESSION_INACTIVE -4   /* Session not active */
#define FASTER_ERR_EPOCH            -5   /* Epoch violation */
#define FASTER_ERR_CHECKPOINT       -6   /* Checkpoint error */
#define FASTER_ERR_NULL_POINTER     -7   /* NULL pointer argument */
#define FASTER_ERR_UNKNOWN          -99  /* Unknown error */

/// Get a human-readable error message for the last error.
///
/// @param buf          Buffer to write the message into.
/// @param buf_len      Size of the buffer.
/// @return             Number of bytes written (excluding null terminator),
///                     or negative if buffer is too small.
///
/// Thread-local: each thread has its own last-error message.
int32_t faster_error_message(char* buf, size_t buf_len);
```

**Rationale:** Integer error codes are universally understood by C callers. The
`faster_error_message()` function provides details when needed (analogous to
`strerror(errno)` or Win32's `FormatMessage`). The last error is stored in
thread-local storage, so it is safe to call from multiple threads.

### 9.5 Memory Ownership Rules

Clear ownership is critical for a C FFI. The rules are:

| Resource | Allocated By | Freed By | Rule |
|----------|-------------|----------|------|
| `faster_kv_t*` | `faster_kv_open()` | `faster_kv_close()` | Library owns |
| `faster_session_t*` | `faster_session_open()` | `faster_session_close()` | Library owns |
| `faster_recovery_info_t*` | `faster_kv_recover()` | `faster_recovery_info_free()` | Library allocates, caller frees via API |
| Key/value/input byte buffers | Caller | Caller | Caller owns; library reads during call only |
| Output byte buffer | Caller | Caller | Caller provides buffer; library writes into it |
| `faster_functions_t` | Caller | Caller | Library copies the struct; caller may free |
| `user_data` pointer | Caller | Caller | Library passes through to callbacks unchanged |
| `user_context` pointer | Caller | Caller | Library passes through to completion callback |

**Key principle:** The library NEVER frees memory it didn't allocate. The caller
NEVER frees memory it didn't allocate (except via library-provided free functions).
All buffers passed to the library are valid only for the duration of the call (the
library copies what it needs).

### 9.6 Callback Model for Pending Operations in C

When a C operation returns `FASTER_STATUS_PENDING`, the result will be delivered
through the `completion` callback in `faster_functions_t`:

```c
// Example C usage:
void my_completion(int32_t op_type, int32_t status,
                   const uint8_t* key, size_t key_len,
                   const uint8_t* output, size_t output_len,
                   void* user_context, void* user_data)
{
    if (op_type == 0 && status == FASTER_OK) {
        // Read completed. output contains the result.
        printf("Read completed: %.*s\n", (int)output_len, output);
    }
}

// Register the completion callback:
faster_functions_t funcs = {
    .read = my_read,
    .upsert = my_upsert,
    .rmw_initial = my_rmw_initial,
    .rmw_in_place = my_rmw_in_place,
    .rmw_copy_update = my_rmw_copy_update,
    .completion = my_completion,  // ← pending completion callback
};

// Issue a read (might go pending):
int32_t status = faster_session_read(session, key, key_len,
                                     NULL, 0,
                                     output_buf, sizeof(output_buf),
                                     &output_len,
                                     my_user_context);
if (status == FASTER_STATUS_PENDING) {
    // Result will arrive via my_completion callback
    // when we call complete_pending:
    faster_session_complete_pending(session, 1 /* wait */);
}
```

### 9.7 Header Generation

**Decision: use `cbindgen` with hand-curated overrides.**

- **`cbindgen`** generates the C header file from Rust source annotations. This
  ensures the header always matches the implementation.
- **Hand-curated overrides** are used for:
  - The `faster_functions_t` struct (complex callback signatures).
  - Documentation comments (cbindgen's doc generation is basic).
  - Platform-specific `#ifdef` guards.
- The generated header is checked into the repository and CI verifies it matches
  the Rust source (fail if stale).

```
crate: faster-ffi/
├── src/
│   ├── lib.rs          # FFI function implementations
│   ├── handles.rs      # Opaque handle management
│   └── error.rs        # Error code mapping + thread-local error message
├── cbindgen.toml       # cbindgen configuration
├── include/
│   └── faster.h        # Generated (+ hand-curated) C header
└── Cargo.toml
```

### 9.8 Thread Safety Documentation

Every FFI function documents its thread safety in the C header:

```c
/**
 * @brief Open a FASTER store.
 *
 * @thread_safety Thread-safe. May be called from any thread.
 * The returned handle may be used from multiple threads (each thread
 * should open its own session).
 */
int32_t faster_kv_open(...);

/**
 * @brief Perform a read operation.
 *
 * @thread_safety NOT thread-safe per session. Each session handle must
 * be used from a single thread at a time. Multiple sessions may be used
 * concurrently on different threads.
 */
int32_t faster_session_read(...);

/**
 * @brief Close a FASTER store.
 *
 * @thread_safety NOT thread-safe. All sessions must be closed before
 * calling this function. Must not be called concurrently with any other
 * operation on the same store.
 */
int32_t faster_kv_close(...);
```

---

## 10. Async Runtime Integration Layer

This is the keystone of the "no-async-in-core" architecture. The core crate is
purely synchronous, using completion callbacks and `std::task::Waker` (from the
standard library, NOT from any async runtime) as its only concession to the async
world. Thin adapter crates bridge the gap to specific async runtimes.

### 10.1 Architecture Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                        APPLICATION LAYER                             │
│                                                                      │
│  Sync caller          Tokio caller          Monoio caller            │
│  (blocking)           (async/await)         (async/await)            │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  faster (core)       faster-tokio           faster-monoio            │
│  Session::read()     AsyncSession::read()   AsyncSession::read()     │
│      │                    │                     │                    │
│      │                    │ wraps in Future      │ wraps in Future    │
│      │                    │ + Waker              │ + Waker            │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │                  CORE: CompletionSlot                         │    │
│  │                                                              │    │
│  │  CompletionSlot::None     → sync: caller polls               │    │
│  │  CompletionSlot::Waker(w) → async: w.wake() on completion    │    │
│  │  CompletionSlot::Callback → C FFI: fn ptr invoked            │    │
│  └──────────────────────────────────────────────────────────────┘    │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │                  DEVICE TRAIT                                 │    │
│  │                                                              │    │
│  │  FileDevice        TokioDevice           MonoioDevice         │    │
│  │  (sync I/O)        (tokio::fs)           (io_uring)           │    │
│  │  (threadpool)      (epoll/IOCP)          (io_uring)           │    │
│  └──────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

### 10.2 Adapter Crates

Each async runtime gets its own thin adapter crate. These crates are small (~500-1500
lines each) and have exactly two responsibilities:

1. **Provide a runtime-native `Device` implementation** that uses the runtime's
   async I/O primitives.
2. **Provide an `AsyncSession` wrapper** that converts the core's `Status::Pending`
   + `CompletionSlot::Waker` pattern into `Future`s the runtime can `.await`.

```
Workspace layout:
  faster/                       # Core crate (0 async deps)
  faster-tokio/                 # Tokio adapter
  faster-compio/                # Compio adapter (io_uring)
  faster-monoio/                # Monoio adapter (thread-per-core + io_uring)
  faster-kimojio/               # Kimojio adapter (when available)
  faster-ffi/                   # C FFI crate
```

#### faster-tokio

```toml
[package]
name = "faster-tokio"

[dependencies]
faster = { path = "../faster" }
tokio = { version = "1", features = ["fs", "io-util", "rt", "sync"] }
```

**Provides:**
- `TokioDevice` — uses `tokio::fs::File` for async I/O via Tokio's threadpool
  (epoll on Linux, IOCP on Windows).
- `AsyncSession` — wraps `Session` operations in `Future`s.
- `AsyncLogScanStream` — wraps `LogScanIterator` in a `Stream`.

#### faster-compio

```toml
[package]
name = "faster-compio"

[dependencies]
faster = { path = "../faster" }
compio = { version = "0.x", features = ["runtime", "fs"] }
```

**Provides:**
- `CompioDevice` — uses compio's io_uring-based async I/O (Linux-only, zero-copy).
- `AsyncSession` and `AsyncLogScanStream` wrappers.

**Rationale for compio over tokio-uring:** compio provides a cross-platform io_uring
abstraction that also works on Windows (via IOCP) and macOS (via kqueue). It is
actively maintained and designed for the completion-based I/O model that FASTER
naturally fits.

#### faster-monoio

```toml
[package]
name = "faster-monoio"

[dependencies]
faster = { path = "../faster" }
monoio = { version = "0.x" }
```

**Provides:**
- `MonoioDevice` — uses monoio's thread-per-core io_uring backend.
- `AsyncSession` and `AsyncLogScanStream` wrappers.

**Rationale:** Monoio's thread-per-core model aligns perfectly with FASTER's
session-per-thread design. Each monoio worker thread owns a session and a
device — no cross-thread coordination for I/O.

#### faster-kimojio (Future)

Kimojio is a planned/emerging async runtime. The adapter crate will be created
when the runtime stabilizes. The architecture is designed to accommodate it without
any changes to the core crate — only a new adapter is needed.

### 10.3 How Adapters Work: The Future Bridge

The adapter pattern converts the core's synchronous-with-callbacks model into
`async`/`await`-compatible `Future`s. Here is the complete mechanism:

#### Step 1: AsyncSession Wraps Session

```rust
// In faster-tokio/src/session.rs

/// Async-capable session wrapper.
///
/// Wraps a core Session and provides async versions of all operations.
/// Must be used within a Tokio runtime context.
pub struct AsyncSession<'store, K, V, F: Functions<Key = K, Value = V>> {
    inner: Session<'store, K, V, F>,
}

impl<'store, K: Key, V: Value, F: Functions<Key = K, Value = V>>
    AsyncSession<'store, K, V, F>
{
    /// Async read. If the record is in memory, completes immediately
    /// (no runtime interaction). If on disk, suspends the Future until
    /// I/O completes.
    pub async fn read(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult {
        // Fast path: try synchronous operation first.
        let result = self.inner.read(key, input, output);
        match result {
            Ok(Status::Pending) => {
                // Slow path: await the pending operation.
                PendingFuture::new(&mut self.inner).await
            }
            other => other,
        }
    }

    /// Complete all pending operations asynchronously.
    /// Yields to the runtime between completions to avoid starving
    /// other tasks.
    pub async fn complete_pending(&mut self) -> Result<usize, FasterError> {
        let mut completed = 0;
        while self.inner.pending_count() > 0 {
            completed += self.inner.try_complete_pending()?;
            if self.inner.pending_count() > 0 {
                // Yield to runtime, resume when more I/O completes.
                tokio::task::yield_now().await;
            }
        }
        Ok(completed)
    }
}
```

#### Step 2: PendingFuture Registers a Waker

```rust
// In faster-tokio/src/pending.rs

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// A Future that resolves when a pending FASTER operation completes.
///
/// This is the bridge between the core's callback model and Tokio's
/// Future/Waker model. It works by registering the task's Waker in
/// the core's CompletionSlot. When I/O completes, the core calls
/// waker.wake(), the runtime re-polls this Future, and it reads the
/// result from the pending context.
pub struct PendingFuture<'session, 'store, K, V, F: Functions<Key = K, Value = V>> {
    session: &'session mut Session<'store, K, V, F>,
    serial_number: u64,  // identifies which pending op we're waiting for
    registered: bool,
}

impl<'session, 'store, K, V, F: Functions<Key = K, Value = V>> Future
    for PendingFuture<'session, 'store, K, V, F>
{
    type Output = OperationResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if the operation has already completed.
        if let Some(result) = self.session.try_get_pending_result(self.serial_number) {
            return Poll::Ready(result);
        }

        // Register the waker so the core can notify us on I/O completion.
        if !self.registered {
            self.session.register_waker(self.serial_number, cx.waker().clone());
            self.registered = true;
        } else {
            // Waker may have changed (runtime may reassign tasks).
            self.session.update_waker(self.serial_number, cx.waker().clone());
        }

        // Check again (race: I/O may have completed between our first check
        // and the waker registration).
        if let Some(result) = self.session.try_get_pending_result(self.serial_number) {
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
```

#### Step 3: Core Wakes the Future on I/O Completion

Inside the core (NOT in the adapter), the I/O completion callback does:

```rust
// In faster/src/internal/pending.rs (CORE crate)

fn on_io_complete(pending_ctx: &mut PendingContext) {
    // 1. Re-execute the operation with the loaded page.
    let result = retry_operation_with_loaded_record(pending_ctx);

    // 2. Store the result.
    pending_ctx.result = Some(result);

    // 3. Signal completion via the CompletionSlot.
    match &pending_ctx.completion {
        CompletionSlot::None => {
            // Sync caller will pick this up via complete_pending().
        }
        CompletionSlot::Waker(waker) => {
            // Async adapter: wake the Future.
            waker.wake_by_ref();
        }
        CompletionSlot::Callback { func, context } => {
            // C FFI: invoke the callback.
            unsafe { func(&result as *const _, *context) };
        }
    }

    // 4. Move to completed queue for retrieval.
    session_pending.completed.push_back(pending_ctx);
}
```

**Key insight:** `std::task::Waker` is in the Rust standard library (`core::task`),
NOT in Tokio or any runtime. The core crate calls `waker.wake_by_ref()` without
knowing anything about Tokio, monoio, or any other runtime. This is the critical
design property that enables runtime-agnostic async support.

### 10.4 Device Trait Implementations Per Runtime

Each adapter provides a `Device` implementation using the runtime's native I/O
primitives:

#### TokioDevice

```rust
// In faster-tokio/src/device.rs

pub struct TokioDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
    runtime_handle: tokio::runtime::Handle,
}

impl Device for TokioDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let handle = self.runtime_handle.clone();
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Spawn a Tokio task to perform the read.
        // When it completes, invoke the core's callback.
        handle.spawn(async move {
            let result = match tokio::fs::File::open(&path).await {
                Ok(mut file) => {
                    file.seek(std::io::SeekFrom::Start(offset)).await?;
                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(buf_ptr, buf_len)
                    };
                    file.read_exact(slice).await.map(|_| buf_len as u32)
                }
                Err(e) => Err(e),
            };
            // Invoke the core's completion callback from within Tokio.
            unsafe { callback(result, context) };
        });
    }

    // write_async: similar pattern with file.write_all()

    fn sector_size(&self) -> u32 { self.sector_size }
    fn segment_size(&self) -> u64 { self.segment_size }
    // ...
}
```

#### CompioDevice (io_uring-based)

```rust
// In faster-compio/src/device.rs

pub struct CompioDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
}

impl Device for CompioDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Submit an io_uring read via compio.
        // compio handles SQE submission and CQE polling.
        compio::runtime::spawn(async move {
            let file = compio::fs::File::open(&path).await.unwrap();
            let read_buf = Vec::with_capacity(buf_len);
            let (result, read_buf) = file.read_at(read_buf, offset).await;
            match result {
                Ok(n) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            read_buf.as_ptr(), buf_ptr, n
                        );
                        callback(Ok(n as u32), context);
                    }
                }
                Err(e) => {
                    unsafe { callback(Err(e), context) };
                }
            }
        });
    }
    // ...
}
```

**Key difference from TokioDevice:** Compio uses completion-based I/O (io_uring)
natively, which is a natural fit for FASTER's callback model. The buffer ownership
semantics of io_uring (kernel owns the buffer during I/O) require careful handling
but enable true zero-copy I/O on Linux ≥ 5.1.

#### FileDevice (Synchronous, in core crate)

The core crate includes a basic synchronous device for callers who don't use any
async runtime:

```rust
// In faster/src/device/file_device.rs (CORE crate, no async deps)

pub struct FileDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
    thread_pool: ThreadPool,  // simple internal thread pool for async I/O
}

impl Device for FileDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Dispatch to internal thread pool for non-blocking behavior.
        self.thread_pool.execute(move || {
            let result = (|| {
                let mut file = std::fs::File::open(&path)?;
                file.seek(std::io::SeekFrom::Start(offset))?;
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(buf_ptr, buf_len)
                };
                file.read_exact(slice)?;
                Ok(buf_len as u32)
            })();
            unsafe { callback(result, context) };
        });
    }
    // ...
}
```

This thread pool is intentionally minimal (no work stealing, no queue depth
optimization). It exists solely to provide baseline async I/O capability for the
core crate without any external dependencies. Production deployments should use a
runtime-native device for optimal performance.

### 10.5 Key Design Principle: No Runtime Types in Core's Public API

The core crate's public API contains ZERO references to Tokio, monoio, compio,
or any other async runtime. Specifically:

| Core API Element | Runtime-Specific Types | Status |
|-----------------|----------------------|--------|
| `FasterKv` | None | ✅ Clean |
| `Session` | None | ✅ Clean |
| `Functions` trait | None | ✅ Clean |
| `Status` enum | None | ✅ Clean |
| `Device` trait | None (`IoCallback` is `extern "C" fn`) | ✅ Clean |
| `CompletionSlot` | `std::task::Waker` (stdlib, not runtime) | ✅ Clean |
| `PendingContext` | None | ✅ Clean |

The **only** type that bridges the async world is `std::task::Waker`, which is
part of the Rust standard library (`core::task::Waker`). It has been stable since
Rust 1.36 and is runtime-agnostic by design.

**What this enables:**

1. **The core compiles with `#![no_std]` + `alloc`** (future goal, not MVP) because
   it has no dependency on `std::net`, `std::fs`, or any I/O library. The `FileDevice`
   is behind a `std` feature flag.
2. **New runtimes can be supported** by writing a ~500-line adapter crate. No changes
   to the core are needed. When kimojio ships, we write `faster-kimojio` and it
   works immediately.
3. **The core can be tested in isolation** using `NullDevice` (immediate callback,
   no real I/O) without any runtime dependency.
4. **C FFI callers** use the core directly (with `FileDevice` or their own `Device`
   implementation) without any Rust async runtime overhead.

### 10.6 PendingOperation Handle Pattern

For ergonomic async usage, adapters provide a `PendingOperation` handle that
implements `IntoFuture`:

```rust
// In faster-tokio/src/session.rs

/// A handle to a potentially-pending operation.
///
/// If the operation completed on the fast path, `.await` returns immediately.
/// If the operation went pending, `.await` suspends until I/O completes.
pub enum PendingOperation<O> {
    /// Operation completed synchronously (fast path).
    Complete(OperationResult, O),
    /// Operation is pending I/O (slow path).
    Pending(PendingFuture</* ... */>),
}

impl<O> IntoFuture for PendingOperation<O> {
    type Output = (OperationResult, O);
    type IntoFuture = impl Future<Output = Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        async move {
            match self {
                PendingOperation::Complete(result, output) => (result, output),
                PendingOperation::Pending(future) => future.await,
            }
        }
    }
}
```

**Usage from application code:**

```rust
// Tokio application code — clean, idiomatic async/await
use faster_tokio::{AsyncSession, TokioDevice};

#[tokio::main]
async fn main() {
    let device = TokioDevice::new("/data/faster", 1 << 30, 512);
    let store = FasterKvBuilder::new(device)
        .index_size(1 << 20)
        .memory_size(1 << 30)
        .build()
        .unwrap();

    let mut session = store.new_async_session(MyFunctions);

    // Read — might complete instantly or await I/O
    let mut output = 0u64;
    let status = session.read(&42u64, &(), &mut output).await?;
    println!("Read: {:?}, output: {}", status, output);

    // Upsert — always completes immediately (no .await needed, but
    // the API is uniform)
    let status = session.upsert(&42u64, &100u64, &(), &mut output).await?;

    // RMW — might await if record is on disk
    let status = session.rmw(&42u64, &1u64, &mut output).await?;

    // Batch: multiple concurrent reads
    let read1 = session.read(&1u64, &(), &mut out1);
    let read2 = session.read(&2u64, &(), &mut out2);
    let (r1, r2) = tokio::join!(read1, read2);
}
```

**Sync usage (no runtime):**

```rust
// No Tokio, no monoio — just the core crate + FileDevice
use faster::{FasterKvBuilder, Session, FileDevice};

fn main() {
    let device = FileDevice::new("/data/faster", 1 << 30, 512, 4);
    let store = FasterKvBuilder::new(device)
        .index_size(1 << 20)
        .memory_size(1 << 30)
        .build()
        .unwrap();

    let mut session = store.new_session(MyFunctions);

    // Sync read
    let mut output = 0u64;
    let status = session.read(&42u64, &(), &mut output)?;
    if status == Status::Pending {
        session.complete_pending(true)?;  // blocks until I/O done
    }
}
```

### 10.7 Runtime Adapter Comparison

| Feature | faster-tokio | faster-compio | faster-monoio | faster-kimojio |
|---------|-------------|---------------|---------------|----------------|
| I/O model | epoll/IOCP (poll-based) | io_uring (completion-based) | io_uring (completion-based) | TBD |
| Platform | Linux, Windows, macOS | Linux (≥5.1), Windows | Linux (≥5.1) | TBD |
| Threading | Multi-thread work-stealing | Flexible | Thread-per-core | TBD |
| FASTER fit | Good (mature, broad platform) | Excellent (native completion) | Excellent (session=thread) | TBD |
| Zero-copy I/O | No (copies through buffer pool) | Yes (io_uring buffers) | Yes (io_uring buffers) | TBD |
| Crate maturity | Production-ready | Growing | Growing | Planned |
| Recommended for | Default / cross-platform | Linux high-performance | Linux thread-per-core | Future |

**Recommendation:** Start with `faster-tokio` as the reference adapter (broadest
platform support, most mature ecosystem). Implement `faster-compio` second for
Linux io_uring performance. `faster-monoio` follows for thread-per-core deployments.
`faster-kimojio` is created when the runtime is available.

<!-- End of Part 2 (Sections 6–10) -->


---

# Rust FASTER Architecture — Part 3: Verification, Phasing, Decisions, and Risks

**Author:** Thrawn (Lead / System Architect)
**Date:** 2026-03-05
**Scope:** Sections 11–14 of the Rust FASTER architecture document
**Status:** North-star reference — all implementation work derives from this document

---

## 11. Testing Strategy

The testing strategy is layered: fast unit tests gate every commit, integration and property tests run on every PR, deterministic simulation and fuzzing run nightly, and benchmarks run on dedicated hardware weekly. The goal is not merely "coverage" but **confidence in correctness under adversarial conditions at planetary scale**.

### 11.1 Unit Testing (Per-Module)

Every module gets a `#[cfg(test)] mod tests` block co-located with the implementation. Unit tests are pure, deterministic, and fast (< 1 second each).

| Module | Key Unit Tests |
|--------|---------------|
| **Epoch** | Acquire/release semantics; safe-to-reclaim advances correctly; drain list callbacks fire at correct epoch; reentrant acquire works; thread entry allocation/deallocation; max thread limit enforcement |
| **Hash Index** | Bucket entry packing/unpacking (48-bit address + 14-bit tag + 2 status bits round-trip); overflow bucket allocation and linking; tag extraction from 64-bit hash; `FindEntry` / `FindOrCreateEntry` on single-threaded index; tentative bit CAS semantics |
| **Record Format** | `RecordInfo` bitfield packing (previous_address, checkpoint_version, invalid, tombstone, final bits); key/value alignment padding calculation; serialization round-trip for fixed-size and variable-length records; record size computation |
| **Address** | `Address` newtype: page/offset extraction, special address constants (`kInvalidAddress`, `kMaxAddress`); read-cache bit manipulation; address comparison and ordering |
| **Hybrid Log (Allocator)** | Page allocation and tail-address bumping; mutable/read-only/head address boundary tracking; page-to-physical-address translation; buffer pool acquire/release |
| **Device Trait** | `NullDevice` passes all trait methods; `FileDevice` basic read/write round-trip; alignment enforcement (512-byte sector); segment file creation and naming |
| **Status/Error** | `Status` flag composition (Found, Pending, InPlaceUpdated, etc.); `Error` variant construction; `Result<Status, Error>` ergonomics |
| **Checkpoint Metadata** | `info.dat` serialization/deserialization round-trip; version field forward compatibility; session commit point encoding |
| **MallocFixedPageSize** | Block allocation/deallocation; free list correctness; page growth; alignment guarantees |
| **C FFI** | Opaque handle creation and destruction; status code translation; null pointer safety checks |

**Conventions:**
- Use `#[test]` for synchronous unit tests. No async runtime in core tests.
- Use `assert_eq!` with descriptive messages. Prefer structured assertions over raw `assert!`.
- Test both the happy path and every documented error condition.
- Bitfield tests must cover boundary values: 0, 1, max, max-1, and random mid-range values.

### 11.2 Integration Testing (End-to-End)

Integration tests live in `tests/` at the crate root and exercise the full `FasterKv` stack: hash index → hybrid log → device → checkpoint/recovery.

#### Core CRUD Integration

```
test_upsert_read_single_key
test_upsert_overwrite_returns_latest
test_rmw_counter_increment
test_rmw_initial_value_on_missing_key
test_delete_then_read_returns_not_found
test_delete_then_upsert_resurrects_key
test_read_on_disk_returns_pending_then_completes
test_upsert_never_goes_pending
test_mixed_operations_1000_keys
```

#### Multi-Threaded Concurrent Operations

```
test_concurrent_upsert_disjoint_keys_{2,4,8,16}_threads
test_concurrent_upsert_overlapping_keys
test_concurrent_read_while_upsert
test_concurrent_rmw_same_key_{2,4,8,16}_threads
test_concurrent_delete_while_read
test_concurrent_grow_index_during_operations
test_concurrent_checkpoint_during_operations
test_session_isolation_no_cross_contamination
```

#### Checkpoint/Recovery Integration

```
test_foldover_checkpoint_and_recover
test_snapshot_checkpoint_and_recover
test_incremental_snapshot_checkpoint_and_recover
test_recover_with_session_resume
test_recover_commit_point_exclusions
test_multiple_checkpoints_recover_latest
test_index_checkpoint_avoids_log_replay
test_checkpoint_during_active_operations
```

#### Storage Integration

```
test_operations_with_file_device
test_operations_with_null_device
test_page_flush_and_readback
test_segmented_file_growth
test_io_completion_callbacks_fire
test_pending_operations_complete_after_io
```

**Conventions:**
- Each integration test creates a fresh `FasterKv` instance in a `tempdir`.
- Multi-threaded tests use `std::thread::scope` (no async runtime).
- Tests that exercise disk I/O clean up temp directories on success; leave them on failure for debugging.
- Timeout: integration tests must complete within 30 seconds. Tests that need longer are moved to the nightly suite.

### 11.3 Property-Based Testing (proptest / quickcheck)

Property tests express invariants that must hold for **all** valid inputs, not just hand-picked examples. We use `proptest` for its shrinking capabilities.

#### Hash Index Invariants

```rust
proptest! {
    // Every inserted key is findable
    #[test]
    fn insert_then_find(keys in prop::collection::vec(any::<u64>(), 1..10000)) {
        let index = HashIndex::new(1 << 20);
        for &k in &keys {
            index.find_or_create_entry(hash(k), tag(k));
        }
        for &k in &keys {
            assert!(index.find_entry(hash(k), tag(k)).is_some());
        }
    }

    // Concurrent inserts: no entry is lost
    #[test]
    fn concurrent_insert_no_loss(
        keys in prop::collection::vec(any::<u64>(), 1..5000),
        num_threads in 2..=8usize,
    ) {
        // Partition keys across threads, insert concurrently, verify all findable
    }

    // Overflow chains: length bounded by insert count for distinct tags
    #[test]
    fn overflow_chain_bounded(keys in prop::collection::vec(any::<u64>(), 1..1000)) {
        // Insert keys, verify no overflow chain exceeds expected length
    }
}
```

#### Hybrid Log Address Space Consistency

```rust
proptest! {
    // tail_address >= readonly_address >= head_address >= begin_address (always)
    #[test]
    fn address_ordering_invariant(ops in vec(arbitrary_operation(), 1..10000)) {
        let store = FasterKv::new(config);
        for op in ops {
            apply_operation(&store, op);
            assert!(store.tail_address() >= store.readonly_address());
            assert!(store.readonly_address() >= store.head_address());
            assert!(store.head_address() >= store.begin_address());
        }
    }

    // Every allocated address is within [begin_address, tail_address)
    #[test]
    fn allocated_address_in_range(ops in vec(arbitrary_upsert(), 1..5000)) {
        // Track all allocated addresses, verify bounds
    }
}
```

#### Serialization Round-Trip

```rust
proptest! {
    // RecordInfo: pack → unpack = identity
    #[test]
    fn record_info_roundtrip(
        prev_addr in 0u64..((1 << 48) - 1),
        version in 0u16..((1 << 13) - 1),
        invalid in any::<bool>(),
        tombstone in any::<bool>(),
    ) {
        let info = RecordInfo::new(prev_addr, version, invalid, tombstone);
        let packed = info.to_u64();
        let unpacked = RecordInfo::from_u64(packed);
        assert_eq!(info, unpacked);
    }

    // Address: page/offset extraction round-trip
    #[test]
    fn address_roundtrip(page in 0u32..((1 << 23) - 1), offset in 0u32..((1 << 25) - 1)) {
        let addr = Address::new(page, offset);
        assert_eq!(addr.page(), page);
        assert_eq!(addr.offset(), offset);
    }

    // Variable-length record: serialize → deserialize = identity
    #[test]
    fn varlen_record_roundtrip(
        key in prop::collection::vec(any::<u8>(), 0..1024),
        value in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let mut buf = vec![0u8; record_size(&key, &value)];
        serialize_record(&mut buf, &key, &value);
        let (k, v) = deserialize_record(&buf);
        assert_eq!(k, &key[..]);
        assert_eq!(v, &value[..]);
    }
}
```

#### Epoch Safety Properties

```rust
proptest! {
    // No callback fires before all threads have advanced past its epoch
    #[test]
    fn epoch_drain_safety(
        thread_count in 2..=16usize,
        ops in vec(arbitrary_epoch_op(), 1..1000),
    ) {
        // Model: track which epochs each thread protects
        // Verify: no drain callback fires while any thread holds an earlier epoch
    }
}
```

### 11.4 Deterministic Simulation Testing

This is the crown jewel of our testing strategy. Led by **Jyn** (Deterministic Simulation Testing Expert), the simulation framework provides reproducible exploration of concurrent interleavings, fault injection, and crash recovery scenarios.

#### Architecture

```
┌─────────────────────────────────────────────┐
│         Deterministic Test Harness           │
├─────────────────────────────────────────────┤
│  DeterministicScheduler                     │
│    - Seed-based PRNG for thread scheduling  │
│    - Single-threaded executor with virtual  │
│      thread contexts                        │
│    - Controllable: round-robin, random,     │
│      adversarial (priority inversion, etc.) │
├─────────────────────────────────────────────┤
│  SimulatedDevice                            │
│    - Implements Device trait                 │
│    - Controllable latency, bandwidth        │
│    - Fault injection points:                │
│      · Read errors (EIO)                    │
│      · Write errors (ENOSPC, EIO)           │
│      · Partial writes (torn pages)          │
│      · Latency spikes                       │
│      · Reordered completions                │
├─────────────────────────────────────────────┤
│  CrashSimulator                             │
│    - Captures full store state at any point │
│    - Simulates power loss (drop all         │
│      in-flight I/O, truncate partial writes)│
│    - Restores from checkpoint, verifies     │
│      consistency                            │
├─────────────────────────────────────────────┤
│  LinearizabilityChecker                     │
│    - Records operation history              │
│    - Verifies linearizable ordering exists  │
│    - Reports witness (linearization order)  │
│      or counterexample on failure           │
└─────────────────────────────────────────────┘
```

#### Custom Deterministic Scheduler

The scheduler replaces real thread scheduling with a seed-controlled PRNG. All "threads" execute cooperatively in a single OS thread, yielding at explicit yield points (atomic operations, I/O submissions, epoch acquire/release).

```rust
struct DeterministicScheduler {
    rng: StdRng,                          // Seeded PRNG
    threads: Vec<SimThread>,              // Virtual thread contexts
    schedule: SchedulePolicy,             // Random, round-robin, adversarial
    yield_points_executed: u64,           // For progress tracking
    history: Vec<ScheduleEvent>,          // For replay on failure
}

enum SchedulePolicy {
    Random,                               // Uniform random next-thread
    RoundRobin,                           // Deterministic ordering
    Adversarial(AdversarialConfig),       // Maximize contention
    Replay(Vec<usize>),                   // Replay exact schedule
}
```

**Yield point injection:** The core library's atomic operations and I/O calls are parameterized over a `Scheduler` trait. In production, `NoopScheduler` compiles away. In simulation, `DeterministicScheduler` intercepts every CAS, load, store, and I/O call.

#### Fault Injection Scenarios

| Fault | Injection Point | Verification |
|-------|----------------|--------------|
| **Disk read error** | `SimulatedDevice::read_async` returns `Err(IoError)` | Operation returns error; store remains consistent; retry succeeds |
| **Disk write error** | `SimulatedDevice::write_async` returns `Err(IoError)` | Checkpoint fails gracefully; no data corruption; retry succeeds |
| **Partial write (torn page)** | `SimulatedDevice::write_async` writes only first N bytes | Recovery detects torn page via checksum; falls back to prior checkpoint |
| **Power loss during checkpoint** | `CrashSimulator` kills store mid-WAIT_FLUSH phase | Recovery from last complete checkpoint succeeds; no operations lost beyond commit point |
| **Power loss during normal operation** | `CrashSimulator` kills store at random point | Recovery succeeds; all checkpointed data intact; pending ops may be lost (documented) |
| **I/O latency spike** | `SimulatedDevice` adds 100ms+ delay | Operations complete correctly; no timeout-induced corruption |
| **I/O completion reordering** | `SimulatedDevice` delivers completions out of submission order | Correct behavior (FASTER must tolerate this) |
| **Out-of-disk-space** | `SimulatedDevice::write_async` returns `ENOSPC` | Graceful failure; store can resume after space freed |

#### Crash Recovery Verification Protocol

```
For each seed S in {0..10000}:
  1. Create FasterKv with SimulatedDevice(seed=S)
  2. Execute N random operations (upsert, read, rmw, delete)
  3. Take checkpoint
  4. Execute M more random operations (post-checkpoint)
  5. Record expected state: all ops through checkpoint commit point
  6. CrashSimulator.crash()  // Kill store, discard in-flight I/O
  7. Recover from checkpoint
  8. For each key: verify value matches expected state at commit point
  9. Post-checkpoint operations: verify either present (if completed before crash)
     or absent (if in-flight) — no partial or corrupted state
```

#### Linearizability Checking

For concurrent operation histories, we verify **linearizability**: every concurrent execution is equivalent to some sequential execution that respects real-time ordering.

```rust
struct LinearizabilityChecker {
    history: Vec<Operation>,  // (thread_id, op_type, key, invoke_time, response_time, result)
}

impl LinearizabilityChecker {
    /// Returns Ok(linearization) or Err(counterexample)
    fn check(&self) -> Result<Vec<Operation>, CounterExample> {
        // Wing & Gong (WGL) algorithm or Lowe's algorithm
        // Exponential worst-case but fast in practice for FASTER-style workloads
    }
}
```

We check linearizability for:
- Concurrent reads and upserts to overlapping key sets
- Concurrent RMW operations on the same key
- Operations spanning mutable/read-only/disk boundaries
- Operations during checkpoint phase transitions
- Operations during hash table grow

### 11.5 Benchmark Suite

Led by **Ahsoka** (Performance Guru). Benchmarks are not tests — they measure, they don't assert. But benchmark regressions block releases.

#### YCSB-Style Workloads

We implement the six standard YCSB (Yahoo! Cloud Serving Benchmark) workloads:

| Workload | Mix | Description | Key Distribution |
|----------|-----|-------------|-----------------|
| **A** | 50% read, 50% update | Update-heavy | Zipfian |
| **B** | 95% read, 5% update | Read-mostly | Zipfian |
| **C** | 100% read | Read-only | Zipfian |
| **D** | 95% read, 5% insert | Read-latest | Latest |
| **E** | 95% scan, 5% insert | Short ranges | Zipfian |
| **F** | 50% read, 50% RMW | Read-modify-write | Zipfian |

**Configuration matrix:**
- Key size: 8 bytes (fixed), 16 bytes, variable (16–256 bytes)
- Value size: 8 bytes, 100 bytes, 1 KB, variable (100 bytes–10 KB)
- Record count: 10M, 100M, 1B
- Thread count: 1, 2, 4, 8, 16, 32, 64
- In-memory fraction: 100% (all in DRAM), 50%, 10% (mostly on disk)

#### Comparison Benchmarks

Run identical workloads on:
1. **Rust FASTER** (this implementation)
2. **C++ FASTER** (`cc/` in this repo)
3. **C# FASTER** (`cs/` in this repo)

**Output:** Side-by-side throughput (ops/sec) and latency distribution. Target: Rust within 10% of C++ throughput, lower tail latency.

#### Latency Distribution Tracking

Every benchmark captures the full latency distribution using HdrHistogram:

```
P50:    typical operation latency
P90:    start of tail
P99:    tail latency (SLA-relevant)
P99.9:  deep tail (incident-relevant)
P99.99: extreme tail (capacity planning)
Max:    worst-case single operation
```

**Tracked per operation type:** read (in-memory), read (pending/disk), upsert, rmw (in-place), rmw (copy-update), delete.

#### Throughput Scaling

Measure throughput vs. thread count to identify:
- **Linear scaling range** (expected: 1–8 threads)
- **Plateau point** (expected: 16–32 threads, depending on NUMA topology)
- **Contention cliff** (if any — indicates design flaw)
- **NUMA effects** (cross-socket penalty, if applicable)

Plot: throughput (Mops/sec) on Y-axis, thread count on X-axis, one line per workload.

#### Micro-Benchmarks

| Benchmark | What It Measures |
|-----------|-----------------|
| `bench_hash_function` | Hash computation throughput (keys/sec) |
| `bench_epoch_acquire_release` | Epoch overhead per operation |
| `bench_hash_index_lookup` | Index lookup latency (cache-hot vs. cache-cold) |
| `bench_record_serialize` | Record serialization throughput (bytes/sec) |
| `bench_page_allocate` | Hybrid log allocation throughput |
| `bench_cas_contention` | CAS retry rate under contention (2–64 threads) |
| `bench_device_read_write` | Raw device I/O throughput |
| `bench_checkpoint_latency` | Time to complete a full checkpoint |
| `bench_recovery_time` | Time to recover from checkpoint |

### 11.6 Correctness Verification

#### Cross-Implementation Comparison

Run an identical operation sequence on Rust, C++, and C# FASTER. Compare:
- Final state of every key (value equality)
- Operation return codes (status parity)
- Checkpoint file contents (metadata equivalence, not byte-for-byte)
- Recovery behavior (same keys recoverable, same commit points)

**Test harness:** A shared test specification in JSON or protobuf defines the operation sequence. Each implementation reads the spec, executes operations, and writes results. A comparator checks equivalence.

```
test_spec.json:
  { "ops": [
    {"type": "upsert", "key": 42, "value": 100},
    {"type": "read", "key": 42, "expected_status": "found"},
    {"type": "rmw", "key": 42, "input": 5},
    {"type": "checkpoint", "type": "snapshot"},
    {"type": "delete", "key": 42},
    {"type": "recover"},
    {"type": "read", "key": 42, "expected_status": "found", "expected_value": 105}
  ]}
```

#### Fuzzing

Use `cargo-fuzz` (libFuzzer) and `afl` for coverage-guided fuzzing of:

| Fuzz Target | Input | Looking For |
|-------------|-------|-------------|
| `fuzz_record_deserialize` | Arbitrary byte slices | Panics, buffer overruns in record parsing |
| `fuzz_address_from_u64` | Arbitrary `u64` values | Invalid address construction, overflow |
| `fuzz_checkpoint_metadata_parse` | Arbitrary byte slices | Panics in metadata deserialization |
| `fuzz_operation_sequence` | Sequence of (op_type, key, value) | Assertion failures, panics, deadlocks |
| `fuzz_hash_bucket_entry` | Arbitrary `u64` values | Bitfield extraction errors |
| `fuzz_varlen_record` | Arbitrary (key_len, value_len, bytes) | Buffer overflow in variable-length layout |
| `fuzz_concurrent_ops` | Seed + operation sequence | Data races (under Miri or ThreadSanitizer) |

**Miri integration:** Run unit tests under Miri (`cargo +nightly miri test`) to detect undefined behavior in unsafe code. Not all tests will pass under Miri (I/O operations won't work), but all pure-logic tests must.

### 11.7 CI Pipeline

#### On Every Commit (< 5 minutes)

```yaml
- cargo fmt --check                    # Formatting
- cargo clippy -- -D warnings          # Linting
- cargo build                          # Compile check
- cargo test --lib                     # Unit tests only
- cargo test --test integration_basic  # Smoke test (small subset)
```

#### On Every PR (< 15 minutes)

```yaml
- cargo fmt --check
- cargo clippy -- -D warnings
- cargo build --release                # Release build (catches optimization-related issues)
- cargo test                           # All unit + integration tests
- cargo test --features proptest       # Property-based tests (reduced iteration count)
- cargo +nightly miri test --lib       # Miri on unit tests (unsafe verification)
```

#### Nightly (< 4 hours)

```yaml
- cargo test --features proptest -- --proptest-cases 100000   # Deep property testing
- cargo test --test deterministic_sim                         # Full simulation suite (10K seeds)
- cargo fuzz run fuzz_operation_sequence -- -max_total_time=3600  # 1 hour fuzzing
- cargo fuzz run fuzz_record_deserialize -- -max_total_time=1800
- cargo bench                                                 # Full benchmark suite
- cross-implementation comparison tests
```

#### Weekly (dedicated hardware)

```yaml
- Full YCSB benchmark suite (all workloads × all configurations)
- Throughput scaling tests (1–64 threads)
- Comparison benchmarks (Rust vs C++ vs C#)
- Extended fuzzing campaign (24 hours)
- Deterministic simulation (100K seeds)
- Valgrind/AddressSanitizer run on C FFI examples
```

---


## 12. Implementation Phases

Each phase has explicit deliverables, success criteria, team assignments, dependencies, and parallelism opportunities. Phases are designed so that each produces a testable, demonstrable artifact — no phase is "infrastructure only" without a visible outcome.

**Team Key:**
- **Mando** — Rust Expert (core implementation)
- **Chirrut** — Systems Programming Expert (low-level memory, I/O, alignment)
- **Tarkin** — Database/Storage Expert (hybrid log, checkpoint, recovery)
- **Kenobi** — Tokio/Async Expert (async adapters, runtime integration)
- **Jyn** — Deterministic Simulation Testing Expert (simulation framework, crash testing)
- **Rex** — QA Engineer (test infrastructure, integration tests, CI)
- **Ahsoka** — Performance Guru (benchmarks, profiling, optimization)
- **Leia** — Developer Advocate (API review, documentation, examples)
- **Maul** — Security Expert (unsafe audit, security review)
- **Grievous** — C++ Expert (reference implementation consultation)
- **Dooku** — C# Expert (reference implementation consultation)
- **Cassian** — Reverse Engineer (behavioral specification, cross-impl comparison)
- **Thrawn** — Lead / System Architect (architecture oversight, review authority)

### Phase 1: Foundation

**Duration:** 3–4 weeks
**Dependencies:** None (this is the root)
**Parallelism:** Runs before all other phases. Some sub-tasks can run in parallel internally.

**Deliverables:**
1. **Crate workspace structure:**
   ```
   faster-rs/
   ├── Cargo.toml              (workspace root)
   ├── faster-core/            (core library — zero async dependencies)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-ffi/             (C FFI bindings)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-tokio/           (Tokio async adapter)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-bench/           (benchmarks)
   │   ├── Cargo.toml
   │   └── src/
   └── tests/                  (integration tests)
   ```
2. **Build system:** `cargo build`, `cargo test`, `cargo bench` all work. CI pipeline (GitHub Actions) running on every commit.
3. **Epoch-based reclamation:** Full `LightEpoch` implementation with per-thread epoch tracking, drain list, safe-to-reclaim computation, and scope guard (`EpochGuard`). No crossbeam dependency in v1 — custom implementation for full control over drain semantics and checkpoint integration.
4. **Basic memory allocator:** `MallocFixedPageSize<T>` equivalent — fixed-size block allocator for overflow buckets and internal structures. Page-based growth, free list, alignment guarantees.
5. **Record format and serialization:** `RecordInfo` (8-byte header with bitfields), fixed-size record layout with alignment padding, `Address` newtype (48-bit, page/offset extraction). Variable-length record format defined (size-prefixed key + value, contiguous inline storage — C++ model).
6. **Core type newtypes:** `Address`, `PageOffset`, `Epoch`, `HashTag`, `BucketIndex` — all with `From`/`Into` conversions and `Debug`/`Display` implementations.

**Success Criteria:**
- `cargo test` passes all epoch unit tests including concurrent acquire/release with 16 threads
- `MallocFixedPageSize` allocates and frees 1M blocks without leak (checked by drop count)
- `RecordInfo` round-trip property test passes 100K iterations
- CI pipeline green on Linux (x86_64) and macOS (aarch64)
- `cargo clippy` zero warnings; `cargo fmt` clean

**Primary:** Mando (Rust Expert), Chirrut (Systems Programming)
**Supporting:** Thrawn (architecture review), Rex (CI setup)
**Consulting:** Grievous (C++ epoch/allocator reference)

---

### Phase 2: Hash Index

**Duration:** 3–4 weeks
**Dependencies:** Phase 1 (epoch, allocator, address types)
**Parallelism:** Can overlap with Phase 1's final week (address types finalized early)

**Deliverables:**
1. **Hash bucket structure:** 64-byte cache-line-aligned bucket with 7 entries (8 bytes each: 48-bit address + 14-bit tag + 2 status bits) + 1 overflow pointer. Packed into `[AtomicU64; 8]`.
2. **Hash table:** Power-of-2 bucket array. `FindEntry(hash, tag) → Option<Address>` and `FindOrCreateEntry(hash, tag) → &AtomicU64` with CAS-based insertion.
3. **Overflow bucket management:** Overflow buckets allocated from `MallocFixedPageSize`. Linked list from bucket[7]. Overflow chains walkable without locks.
4. **Concurrent correctness:** All operations are latch-free. Insertions use CAS with tentative bit protocol. Deletions invalidate entries atomically.
5. **Hash function integration:** Pluggable hash function trait. Default: high-quality 64-bit hash (e.g., `ahash` or `xxhash`). Tag extraction: bits [48..61] of hash (matching C++ convention).
6. **GC integration hooks:** Interface for invalidating entries pointing to reclaimed log regions (called during epoch drain).

**Success Criteria:**
- Single-threaded: 100M insert + lookup in < 10 seconds
- Concurrent: 8 threads × 10M inserts, zero lost entries (verified by `FindEntry` on all keys)
- Property test: `insert_then_find` passes 100K iterations with shrinking
- Property test: concurrent insert under `proptest` passes 10K iterations
- Overflow chain length bounded: average < 2 for load factor < 0.7
- No `unsafe` beyond the atomic CAS operations and bucket layout

**Primary:** Mando (Rust Expert)
**Supporting:** Chirrut (cache-line alignment, atomics), Thrawn (API review)
**Consulting:** Grievous (C++ hash index reference), Cassian (behavioral spec)

---

### Phase 3: Hybrid Log (In-Memory)

**Duration:** 4–5 weeks
**Dependencies:** Phase 1 (epoch, allocator), Phase 2 (hash index)
**Parallelism:** Can begin design work during Phase 2; implementation starts after Phase 2 core is stable

**Deliverables:**
1. **Page-based allocator** (`PersistentMemoryMalloc` equivalent): Circular buffer of pages (default: 32 MB per page). Atomic tail-address bump for concurrent allocation. Buffer pool with page reuse.
2. **Address regions:** Four address pointers maintained atomically:
   - `begin_address`: oldest valid address (monotonically increasing)
   - `head_address`: boundary between disk-only and in-memory
   - `readonly_address`: boundary between immutable and mutable regions
   - `tail_address`: next allocation point (monotonically increasing)
   - **Invariant:** `begin_address ≤ head_address ≤ readonly_address ≤ tail_address` (always)
3. **Record operations:**
   - **Allocate:** Bump tail atomically, return address
   - **Read:** Translate address to page + offset, return reference
   - **In-place update:** Write through mutable reference (mutable region only)
   - **Seal:** Set invalid bit on old record when superseded
4. **CRUD implementation (in-memory only):**
   - `Read`: hash lookup → chain walk → return value (or `NotFound`)
   - `Upsert`: hash lookup → in-place update (mutable) or append new record
   - `RMW`: hash lookup → in-place modify (mutable) or copy-update or initial-insert
   - `Delete`: append tombstone record, update hash chain
5. **Pending operation infrastructure:** `PendingContext` struct for operations that require I/O. Completion callback registration. `CompletePending(wait: bool)` method.
6. **Session model:** `Session<K, V, F>` struct marked `!Send` (compile-time thread-affinity enforcement). Session-local pending queue. Epoch protection via `EpochGuard` on every operation. Serial number tracking per session.
7. **Functions trait:** Core callback interface:
   ```rust
   pub trait Functions<K, V> {
       type Input;
       type Output;
       type Context;

       fn read(&self, key: &K, value: &V, input: &Self::Input) -> Self::Output;
       fn upsert(&self, key: &K, old: Option<&V>, new: &V, input: &Self::Input) -> V;
       fn rmw(&self, key: &K, value: &mut V, input: &Self::Input) -> RmwAction;
       fn rmw_initial(&self, key: &K, input: &Self::Input) -> V;
       fn delete(&self, key: &K, old_value: &V) -> bool;
   }
   ```

**Success Criteria:**
- Single-threaded CRUD: 10M operations correct (insert all, read all, update all, delete subset, verify)
- Concurrent CRUD: 8 threads × 1M operations, linearizability verified for overlapping keys
- Address ordering invariant holds across all operations (property test, 100K iterations)
- `!Send` on `Session` — compile error if moved across threads
- In-memory throughput: > 10M ops/sec single-threaded (comparable to C++ in-memory path)

**Primary:** Mando (Rust Expert), Tarkin (Database/Storage)
**Supporting:** Chirrut (memory management, alignment), Thrawn (API design review)
**Consulting:** Grievous (C++ hybrid log), Dooku (C# allocator variants), Cassian (behavioral contracts)

---

### Phase 4: Storage Layer

**Duration:** 4–5 weeks
**Dependencies:** Phase 3 (hybrid log, pending operations)
**Parallelism:** Device trait design can begin during Phase 3. Jyn can begin simulation framework design in parallel.

**Deliverables:**
1. **Device trait:** Completion-based (callback model, not Future-based — per user directive). Runtime-agnostic:
   ```rust
   pub trait Device: Send + Sync {
       fn read_async(
           &self,
           offset: u64,
           buf: &mut [u8],
           callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>,
       );
       fn write_async(
           &self,
           offset: u64,
           buf: &[u8],
           callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>,
       );
       fn flush(&self);
       fn truncate(&self, new_len: u64) -> Result<(), IoError>;
       fn sector_size(&self) -> usize;  // Typically 512
   }
   ```
2. **NullDevice:** In-memory only (no persistence). For testing and benchmarking pure in-memory performance.
3. **FileDevice:** Platform-specific file I/O with O_DIRECT (Linux) / unbuffered (Windows). Segmented file management (1 GB segments by default). Threaded I/O completion: dedicated I/O thread(s) poll for completions and invoke callbacks.
4. **Aligned buffer management:** `AlignedBuffer` type ensuring 512-byte alignment for O_DIRECT I/O. Buffer pool to avoid allocation on the I/O hot path.
5. **Page flush pipeline:** When `readonly_address` advances, pages in [old_readonly, new_readonly) are flushed to device. Flush tracking via `LastFlushedUntilAddress`. Flush callback updates head address when safe.
6. **Page read-back:** When a read targets address < `head_address`, issue async read from device. On completion, invoke pending operation callback.
7. **Pending operation completion flow:**
   - Operation encounters record on disk → create `PendingContext` → issue device read
   - Device callback fires → `InternalContinuePending{Read,Rmw,Delete}`
   - Complete user callback or store result for `CompletePending`
8. **I/O completion thread model:** Separate thread (or thread pool) polls device for completions. Completions enqueued to per-session completion queues. `CompletePending` drains the session's queue.

**Success Criteria:**
- File device: write 1 GB → read back → byte-for-byte identical
- Segmented files: log grows through 5+ segments correctly
- Operations with 50% of data on disk: correct results, no data corruption
- Pending operation flow: 10K read operations on disk-resident data complete correctly
- Benchmark: device throughput within 5% of raw `pread`/`pwrite`
- Cross-platform: tests pass on Linux (x86_64) and macOS (aarch64). Windows support documented as future work.

**Primary:** Chirrut (Systems Programming), Tarkin (Storage)
**Supporting:** Mando (Rust trait design), Thrawn (device trait review)
**Consulting:** Grievous (C++ device layer reference), Kenobi (future async adapter requirements — ensure trait is adaptable)

---

### Phase 5: Checkpoint & Recovery

**Duration:** 5–6 weeks
**Dependencies:** Phase 4 (storage layer, device trait, page flush)
**Parallelism:** State machine design can begin during Phase 4. Jyn begins crash recovery simulation tests.

**Deliverables:**
1. **Checkpoint state machine:** Phase transitions: `REST → PREP_INDEX_CHECKPOINT → PREPARE → IN_PROGRESS → WAIT_FLUSH → PERSISTENCE_CALLBACK → REST`. Coordinated via epoch system (threads observe phase transition at epoch boundary).
2. **Fold-over checkpoint:** Flush mutable log to disk, record addresses and version in `info.dat`. Cheapest checkpoint type.
3. **Snapshot checkpoint:** Copy in-memory log pages to separate snapshot file. Main log continues uninterrupted.
4. **Incremental snapshot:** Delta log captures changes since last snapshot. Recovery applies base + delta.
5. **Index checkpoint:** Persist entire hash table to disk. Avoids full log replay on recovery.
6. **Checkpoint metadata (`info.dat`):** Binary format with version field for forward compatibility. Contains: all address pointers, checkpoint version, session commit points (session_id, serial_number, exclusion list), checkpoint type, timestamp.
7. **Recovery flow:**
   - Scan for latest valid checkpoint token pair (index + log)
   - Load index from disk (or rebuild from log if index checkpoint missing)
   - Restore log address pointers from metadata
   - Fold-over: replay log from `checkpoint_start_address` to `final_address`
   - Snapshot: load snapshot file into memory; apply delta if incremental
   - Restore sessions with commit points
8. **Version management:** CPR (Concurrent Prefix Recovery) protocol. Version bumps at checkpoint boundary. Threads creating records in v+1 cannot modify v records. Dual execution contexts (prev/cur) per session.
9. **Checkpoint token management:** UUID-based tokens. Token directory structure for multi-checkpoint retention.

**Success Criteria:**
- Fold-over checkpoint + recovery: 100% data integrity for 1M records
- Snapshot checkpoint + recovery: identical behavior
- Incremental snapshot: delta correctly captures only post-base changes
- Session resume: recovered session has correct serial number and exclusions
- Crash recovery (deterministic simulation): 1000 seeds × crash at random point during checkpoint → recovery succeeds
- Crash recovery: crash during normal operation (no checkpoint in progress) → last checkpoint fully recoverable
- Cross-implementation: checkpoint metadata readable by comparison tool (not binary-compatible with C++/C#, but semantically equivalent)

**Primary:** Tarkin (Database/Storage), Mando (Rust Expert)
**Supporting:** Jyn (crash recovery simulation tests), Chirrut (I/O correctness)
**Consulting:** Cassian (cross-impl checkpoint semantics), Grievous (C++ CPR), Dooku (C# incremental snapshots)

---

### Phase 6: Public API & Ergonomics

**Duration:** 3–4 weeks
**Dependencies:** Phase 5 (full functionality available for API surface)
**Parallelism:** API design sketches can begin during Phase 3. Leia begins documentation planning during Phase 4.

**Deliverables:**
1. **`FasterKvBuilder`:** Builder pattern for all configuration:
   ```rust
   let store = FasterKv::builder()
       .hash_index_size(1 << 20)
       .log_page_size(1 << 25)        // 32 MB
       .log_memory_size(1 << 30)      // 1 GB
       .log_mutable_fraction(0.9)
       .log_segment_size(1 << 30)     // 1 GB
       .device(FileDevice::new("/path/to/data")?)
       .build()?;
   ```
2. **Session API:** Ergonomic, zero-boilerplate session usage:
   ```rust
   let session = store.new_session(MyFunctions::default());
   session.upsert(&key, &value)?;
   let output = session.read(&key, &input)?;
   session.complete_pending(true)?;
   drop(session);  // RAII: flushes pending, releases epoch
   ```
3. **Functions trait with defaults:** `SimpleFunctions<K, V>` for the common case (direct value read, direct value write, no RMW). Users only implement what they need.
4. **Error handling polish:** Clear error messages with context. `thiserror`-derived error types. Status codes documented with "when does this occur" and "what should I do."
5. **Documentation:** `#[doc]` on every public type, method, and trait. Module-level docs explaining the subsystem. `README.md` with quick-start, installation, and feature overview.
6. **Examples:**
   - `examples/basic.rs` — simple key-value CRUD
   - `examples/counter.rs` — RMW counter pattern
   - `examples/checkpoint.rs` — checkpoint and recovery
   - `examples/concurrent.rs` — multi-threaded concurrent access
   - `examples/variable_length.rs` — variable-length keys/values
7. **API review:** Leia conducts API review with focus on discoverability, naming, and Rust idiom compliance.

**Success Criteria:**
- All examples compile and run without modification
- `cargo doc --no-deps` generates clean documentation with no broken links
- API review by Leia: score ≥ 8/10 on ergonomics rubric
- New user (simulated): can write a working CRUD program in < 15 minutes using only docs
- Zero `pub unsafe fn` in the public API (all unsafety internal)

**Primary:** Leia (Developer Advocate), Mando (Rust Expert)
**Supporting:** Thrawn (API design authority), Rex (example verification)
**Consulting:** Dooku (C# API ergonomics insights)

---

### Phase 7: C FFI

**Duration:** 2–3 weeks
**Dependencies:** Phase 6 (stable public API)
**Parallelism:** Can run in parallel with Phase 8 (async adapters)

**Deliverables:**
1. **`faster-ffi` crate:** Separate crate with `crate-type = ["cdylib", "staticlib"]`.
2. **C header generation:** Auto-generated `faster.h` via `cbindgen`. Manually curated if `cbindgen` output is suboptimal.
3. **Opaque handle API:**
   ```c
   typedef struct FasterKv FasterKv;
   typedef struct FasterSession FasterSession;

   FasterKv* faster_kv_open(const FasterKvConfig* config);
   void faster_kv_close(FasterKv* kv);

   FasterSession* faster_session_new(FasterKv* kv, const FasterFunctions* funcs);
   void faster_session_destroy(FasterSession* session);

   FasterStatus faster_read(FasterSession* s, const void* key, size_t key_len,
                            void* value_out, size_t* value_len_out);
   FasterStatus faster_upsert(FasterSession* s, const void* key, size_t key_len,
                              const void* value, size_t value_len);
   FasterStatus faster_rmw(FasterSession* s, const void* key, size_t key_len,
                           const void* input, size_t input_len);
   FasterStatus faster_delete(FasterSession* s, const void* key, size_t key_len);
   FasterStatus faster_complete_pending(FasterSession* s, int wait);

   FasterStatus faster_checkpoint(FasterKv* kv, FasterCheckpointType type,
                                  FasterGuid* token_out);
   FasterStatus faster_recover(FasterKv* kv, const FasterGuid* token);
   ```
4. **Memory ownership documentation:** Clear documentation in header comments: who allocates, who frees, what happens on error.
5. **C example programs:**
   - `examples/c/basic.c` — CRUD operations
   - `examples/c/checkpoint.c` — checkpoint and recovery
   - `examples/c/Makefile` — build instructions
6. **Error handling:** All C functions return `FasterStatus`. Errors retrievable via `faster_last_error()` (thread-local error string).

**Success Criteria:**
- C examples compile with `gcc` and `clang`, link against the Rust library, and run correctly
- Valgrind: zero memory leaks in C example programs
- AddressSanitizer: zero errors
- Header: compiles cleanly under `-Wall -Wextra -pedantic` in C11 mode
- Double-free protection: calling `faster_kv_close` twice doesn't crash (sets handle to NULL)

**Primary:** Chirrut (Systems Programming — FFI expertise)
**Supporting:** Mando (Rust FFI patterns), Maul (safety review)
**Consulting:** Grievous (C API design patterns from C++ FASTER)

---

### Phase 8: Async Adapters

**Duration:** 3–4 weeks
**Dependencies:** Phase 6 (stable public API), Phase 4 (Device trait)
**Parallelism:** Runs in parallel with Phase 7 (C FFI). Kenobi begins design during Phase 4.

**Deliverables:**
1. **`faster-tokio` crate:** Adapter wrapping `faster-core` for Tokio runtime integration.
   - Implements `Device` trait using Tokio's threaded runtime for I/O
   - Exposes async session API:
     ```rust
     let result = session.read_async(&key, &input).await?;
     let status = session.upsert_async(&key, &value).await?;
     ```
   - Bridges callback-based core to `Future`-based API via `oneshot` channels or `Waker` integration
   - Zero overhead for operations that complete synchronously (in-memory hits return `Poll::Ready` immediately)
2. **Second runtime adapter (compio or monoio):** At least one additional runtime adapter to validate the multi-runtime design.
   - Demonstrates that `Device` trait and callback model genuinely support alternative runtimes
   - Documents the adapter authoring pattern for third-party runtime support
3. **Async device implementations:**
   - `TokioFileDevice`: Uses Tokio's `spawn_blocking` or `tokio-uring` for async file I/O
   - `CompioFileDevice` or `MonoioFileDevice`: Native async I/O for the second runtime
4. **Adapter pattern documentation:** How to write a new runtime adapter in < 200 lines.

**Success Criteria:**
- Tokio adapter: all integration tests pass under `#[tokio::test]`
- Second adapter: basic CRUD + checkpoint/recovery tests pass
- Benchmark: async path adds < 5% overhead vs. sync path for in-memory operations
- Benchmark: async I/O throughput matches or exceeds sync threaded I/O
- Compile-time: `faster-core` builds with zero async runtime dependencies
- Feature isolation: `faster-tokio` depends on `tokio`; `faster-core` does not

**Primary:** Kenobi (Tokio/Async Expert)
**Supporting:** Mando (trait design), Chirrut (I/O path), Thrawn (architecture validation)
**Consulting:** Grievous (C++ async I/O model)

---

### Phase 9: Hardening

**Duration:** 4–6 weeks
**Dependencies:** All prior phases complete
**Parallelism:** Security audit (Maul) and simulation testing (Jyn) can run in parallel with performance optimization (Ahsoka)

**Deliverables:**
1. **Full deterministic simulation test suite (Jyn):**
   - 10,000+ seed configurations
   - All fault injection scenarios from §11.4
   - Crash at every checkpoint phase transition
   - Concurrent operations during checkpoint, grow, GC
   - 48-hour continuous simulation run with no failures
2. **Security audit of all `unsafe` code (Maul):**
   - Catalog every `unsafe` block with justification comment
   - Verify safety invariants are documented and tested
   - Check for: use-after-free, double-free, data races, buffer overflows, uninitialized memory
   - Audit C FFI boundary for memory safety
   - Report: list of findings, severity, remediation
3. **Performance optimization pass (Ahsoka):**
   - Profile under YCSB workloads with `perf`, `flamegraph`, `cachegrind`
   - Identify and eliminate: cache misses in hot path, unnecessary allocations, contention points
   - Optimize: hash function, epoch acquire/release, CAS retry loops, page allocation
   - Target: match or exceed C++ FASTER throughput
4. **Fuzzing campaign:**
   - 1 week continuous fuzzing on all fuzz targets
   - Fix all findings
   - Integrate fuzz corpus into CI (regression tests)
5. **ThreadSanitizer run:** All concurrent tests under ThreadSanitizer to detect data races in unsafe code.
6. **Miri verification:** All pure-logic unit tests pass under Miri.

**Success Criteria:**
- Zero correctness bugs found by simulation (after fixes)
- Zero safety issues in `unsafe` audit (after remediation)
- Performance: Rust FASTER ≥ 90% of C++ FASTER throughput on YCSB-A (update-heavy)
- Performance: Rust FASTER ≥ 95% of C++ FASTER throughput on YCSB-C (read-only)
- Fuzzing: no crashes after 1 week of continuous fuzzing
- ThreadSanitizer: zero data race reports
- Miri: all targeted tests pass

**Primary (parallel tracks):**
- Jyn (simulation testing)
- Maul (security audit)
- Ahsoka (performance optimization)
**Supporting:** Mando (fix implementation issues), Rex (regression test integration)
**Consulting:** Grievous (C++ performance reference), Chirrut (systems-level optimization)

---

### Phase 10: Release Preparation

**Duration:** 2–3 weeks
**Dependencies:** Phase 9 (hardening complete, all tests green)
**Parallelism:** Documentation and benchmark publication can overlap

**Deliverables:**
1. **Documentation complete (Leia):**
   - `README.md`: installation, quick-start, feature overview, comparison with C++/C#
   - `ARCHITECTURE.md`: this document, finalized
   - `API.md`: generated from `cargo doc`, with supplementary guides
   - `MIGRATION.md`: guide for users migrating from C++ or C# FASTER
   - `UNSAFE.md`: catalog of all unsafe usage with safety arguments
   - `CHANGELOG.md`: version history
2. **Benchmark publication (Ahsoka):**
   - YCSB results on standardized hardware (document specs)
   - Comparison charts: Rust vs C++ vs C#
   - Throughput scaling graphs
   - Latency distribution plots (P50, P99, P99.9)
3. **Migration guides:**
   - From C++ FASTER: API mapping table, behavioral differences, checkpoint incompatibilities
   - From C# FASTER: API mapping table, async pattern differences
4. **crates.io preparation:**
   - `Cargo.toml` metadata: description, license (MIT), repository, keywords, categories
   - Crate naming: `faster-kv` (core), `faster-kv-tokio` (async adapter), `faster-kv-ffi` (C bindings)
   - Version: `0.1.0` (initial public release — semver pre-1.0 signals API may evolve)
   - `cargo publish --dry-run` succeeds
5. **Release checklist:**
   - [ ] All CI pipelines green
   - [ ] All benchmarks documented
   - [ ] All examples verified on Linux and macOS
   - [ ] C FFI examples verified with gcc and clang
   - [ ] Security audit findings resolved
   - [ ] CHANGELOG up to date
   - [ ] License file present
   - [ ] `cargo publish` succeeds

**Success Criteria:**
- Clean `cargo publish --dry-run` for all publishable crates
- Documentation review by Leia: complete and accurate
- All migration guide examples compile and run
- No open P0/P1 issues
- Team sign-off: Thrawn (architecture), Mando (implementation), Maul (security), Leia (documentation)

**Primary:** Leia (Developer Advocate), Thrawn (release authority)
**Supporting:** Ahsoka (benchmarks), Mando (final fixes), Rex (release verification)

---

### Phase Dependency Graph

```
Phase 1: Foundation ─────────────────────────────────┐
    │                                                 │
    ▼                                                 │
Phase 2: Hash Index ──────────────┐                   │
    │                              │                   │
    ▼                              ▼                   │
Phase 3: Hybrid Log (In-Memory) ──┤                   │
    │                              │                   │
    ▼                              │                   │
Phase 4: Storage Layer ────────────┤                   │
    │                              │                   │
    ▼                              │                   │
Phase 5: Checkpoint & Recovery ────┤                   │
    │                              │                   │
    ├──────────────┐               │                   │
    ▼              ▼               │   (Jyn: simulation│
Phase 6: API    Phase 7: C FFI    │    framework dev   │
    │              │               │    starts Phase 4) │
    ├──────────────┤               │                   │
    ▼              ▼               │                   │
Phase 8: Async  (parallel w/ 7)   │                   │
    │                              │                   │
    ▼                              │                   │
Phase 9: Hardening ◄──────────────┘                   │
    │                                                  │
    ▼                                                  │
Phase 10: Release ◄───────────────────────────────────┘
```

**Critical path:** 1 → 2 → 3 → 4 → 5 → 6 → 9 → 10 (~28–37 weeks)
**Parallelism savings:** Phases 7 & 8 parallel with each other and with Phase 6's later stages. Jyn's simulation framework built during Phases 4–5. Ahsoka's benchmark suite built during Phases 3–5. ~4–6 weeks saved.
**Estimated total:** 6–9 months to v0.1.0 release.



---

## 13. Key Decisions & Rationale

Every major architectural decision is documented here with the decision, alternatives considered, rationale, and trade-offs accepted. These are binding unless revisited through a formal Design Review.

### Decision 1: No async/await in Core — Callback/Completion Model

**Status:** DECIDED — User directive (non-negotiable)
**Date:** 2026-03-05
**Authority:** qbradley (project owner)

**Decision:** The `faster-core` crate must not depend on any async runtime (tokio, async-std, smol, etc.) and must not use `async`/`await` syntax. All asynchronous operations use a callback/completion model. Async runtime integration is provided via separate adapter crates (`faster-tokio`, `faster-compio`, etc.).

**Rationale:**
1. **Runtime freedom:** Tokio is dominant but not universal. Emerging runtimes (kimojio, compio, monoio) offer io_uring-native and thread-per-core models that may outperform Tokio for storage workloads. The core must not pick a winner.
2. **Scaling limitations:** Tokio's work-stealing scheduler adds overhead for latency-sensitive, CPU-bound operations like CAS loops and epoch management. A callback model lets the application control thread placement.
3. **C FFI compatibility:** Callbacks are the natural async model for C consumers. Futures are meaningless across the FFI boundary.
4. **Deterministic testing:** Callback-based code is easier to test deterministically — no need to mock an async runtime.

**Alternatives rejected:**
- **async/await in core (Grievous's recommendation):** Simpler Rust code, better ergonomics, but locks the implementation to a specific runtime model. Overridden by user directive.
- **Poll-only model (no callbacks):** Too low-level for most consumers. Callbacks provide a usable sync API.

**Trade-offs accepted:**
- Slightly more verbose core code (explicit callback threading vs. `.await` chains)
- Adapter crates must bridge callback-to-Future, adding a thin translation layer
- State machines for multi-step operations (pending-to-IO-to-complete) are explicit, not compiler-generated

**Implementation pattern:**
```rust
// Core: callback-based
fn read(&self, key: &K, callback: impl FnOnce(ReadResult<V>) + Send + 'static) {
    // ... synchronous path or register callback for I/O completion
}

// Adapter: bridges to Future
async fn read_async(&self, key: &K) -> Result<V, Error> {
    let (tx, rx) = oneshot::channel();
    self.inner.read(key, move |result| { let _ = tx.send(result); });
    rx.await.map_err(|_| Error::SessionDropped)?
}
```

---

### Decision 2: Custom Epoch Implementation (Not crossbeam-epoch)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Mando (implementation)

**Decision:** Implement a custom `LightEpoch` modeled on C++/C# FASTER's epoch system rather than using `crossbeam-epoch`.

**Rationale:**
1. **Drain list semantics:** FASTER's epoch uses a drain list for deferred actions (GC, checkpoint phase transitions, grow-index). `crossbeam-epoch` provides `defer()` but without the epoch-tagged scheduling that FASTER requires.
2. **Checkpoint integration:** The epoch system is tightly coupled with the checkpoint state machine — phase transitions are triggered via epoch callbacks. This coupling is fundamental to CPR (Concurrent Prefix Recovery).
3. **Thread entry management:** FASTER tracks per-thread epoch entries with explicit registration/deregistration and `phase_finished` flags. `crossbeam-epoch` has a different thread management model.
4. **Behavioral equivalence:** A custom implementation ensures identical behavior to C++/C# for cross-implementation testing.
5. **Code size:** FASTER's epoch is ~500 lines. The custom implementation is comparable. Adding crossbeam as a dependency saves no complexity.

**Alternatives considered:**
- **crossbeam-epoch:** Mature, well-tested, but semantically different. Would require a wrapper that reimplements most of FASTER's epoch logic anyway.
- **seize:** Lighter weight than crossbeam, but same semantic mismatch.

**Trade-offs accepted:**
- Must write and maintain our own epoch implementation (~500 lines)
- Must write thorough tests (unit, property, simulation) to match crossbeam's maturity
- No free ride on crossbeam's fuzz testing and battle-hardened codebase

**Mitigation:** Extensive property-based testing and deterministic simulation to achieve equivalent confidence.

---

### Decision 3: Record Format — Inline Variable-Length (C++ Model)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Cassian (cross-impl analysis)

**Decision:** Use inline variable-length records where key and value are stored contiguously in a single allocation within the hybrid log. This follows the C++ model, not the C# dual-log model.

**Record layout:**
```
[ RecordInfo (8B) | padding | Key (variable) | padding | Value (variable) ]
```

For variable-length types, keys and values are prefixed with a 4-byte length:
```
[ RecordInfo (8B) | key_len (4B) | key_data[key_len] | pad | value_len (4B) | value_data[value_len] | pad ]
```

**Rationale:**
1. **Cache locality:** Single contiguous allocation means key and value are likely in the same cache line (for small records). The C# dual-log approach splits fixed-size pointers from variable-length data, causing an extra cache miss per access.
2. **Simplicity:** No secondary "object log" device. One log, one set of address pointers, one flush pipeline.
3. **Rust alignment:** Rust's `&[u8]` and `Box<[u8]>` work naturally with inline byte sequences. No GC or separate heap needed.
4. **I/O efficiency:** Entire records read/written in a single I/O operation.

**Alternatives rejected:**
- **C# dual-log (GenericAllocator):** Needed in C# for managed objects (classes on the GC heap). Rust doesn't have this constraint. The extra indirection adds latency and complexity.
- **Fixed-size only:** Too restrictive. Many real-world workloads have variable-length keys or values.

**Trade-offs accepted:**
- In-place updates of variable-length values require the new value to fit in the same space, or fall back to copy-update (append new record). This is acceptable — it matches C++ behavior.
- Record size computation requires knowing key and value sizes upfront.
- Slightly more complex serialization logic than fixed-size records.

---

### Decision 4: Hash Index — Rust Atomic Patterns for Latch-Free Operations

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Mando (implementation)

**Decision:** Implement the hash index using Rust's `std::sync::atomic` types with compare-and-swap (CAS) loops for all mutations. The hash bucket is represented as `[AtomicU64; 8]` (64 bytes = 1 cache line).

**Atomic patterns:**
- **Insert:** CAS on the target entry slot. If tentative entry exists from another thread, retry. Use tentative bit (bit 0 of the entry) to reserve a slot before writing the full entry.
- **Delete:** Atomic store of `Address::kInvalidAddress` into the entry (or set invalid bit).
- **Lookup:** Relaxed load of entry, then verify via tag comparison. If match, follow address into hybrid log.
- **Overflow:** Allocate new overflow bucket from `MallocFixedPageSize`. CAS the overflow pointer (bucket[7]) to link the new bucket.

**Memory ordering:**
- **CAS operations:** `Ordering::AcqRel` (acquire on success for visibility of the stored address; release for publishing the new entry to other threads)
- **Lookups:** `Ordering::Acquire` (need to see the address that was stored)
- **Tag-only checks:** `Ordering::Relaxed` is sufficient (worst case: false miss, retry after epoch refresh)
- **Epoch-protected operations:** Epoch acquire provides the necessary memory fence; operations within an epoch can use `Relaxed` where the epoch fence provides ordering

**Rationale:**
1. **Direct mapping:** C++'s `compare_exchange_strong` maps directly to Rust's `AtomicU64::compare_exchange`.
2. **Cache-line alignment:** `#[repr(align(64))]` on the bucket struct ensures no false sharing.
3. **Bitfield packing:** 48-bit address + 14-bit tag + 2 status bits packed into `u64` via shift/mask operations in a `HashBucketEntry` newtype.
4. **No locks:** The entire hash index is latch-free. No `Mutex`, no `RwLock`, no spinlocks.

**Trade-offs accepted:**
- CAS loops can spin under high contention (same key, same bucket). Mitigation: exponential backoff hint (`std::hint::spin_loop`).
- `unsafe` required for the `#[repr(align(64))]` bucket array and pointer arithmetic. Mitigation: encapsulated in the hash index module with extensive testing.

---

### Decision 5: Hybrid Log Page Size and Alignment

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Tarkin (storage)

**Decision:** Default page size of 2^25 = 32 MB. Configurable via builder. All pages aligned to sector size (512 bytes) for O_DIRECT I/O compatibility.

**Parameters (matching C++ defaults):**
- `PageSizeBits`: 25 (32 MB pages)
- `MemorySizeBits`: configurable (default: enough for 4-16 pages = 128 MB - 512 MB in-memory)
- `MutableFraction`: 0.9 (90% of in-memory pages are mutable)
- `SegmentSizeBits`: 30 (1 GB disk segments)

**Rationale:**
1. **32 MB pages:** Large enough to amortize page management overhead. Small enough to avoid excessive memory waste on partial pages. Matches C++ and C# defaults.
2. **512-byte sector alignment:** Required for O_DIRECT on Linux. Ensures no read-modify-write cycles at the storage layer.
3. **0.9 mutable fraction:** Most records are in the mutable region for in-place updates. The 10% read-only region provides a buffer for records aging out before flush.
4. **1 GB segments:** Large enough to reduce file count. Small enough to allow independent management (deletion of old segments during compaction).

**Alternatives considered:**
- **Smaller pages (4 KB - 4 MB):** Higher page management overhead, more frequent page transitions. Viable for memory-constrained environments; offered as a configuration option but not the default.
- **Larger pages (64 MB - 256 MB):** Higher memory waste, longer flush times. Not recommended but not prevented.

**Trade-offs accepted:**
- 32 MB minimum memory footprint per in-memory page. For small datasets, this wastes memory. Mitigation: configurable page size.
- Large pages mean longer flush durations (32 MB I/O per page). Mitigation: flush pipeline can issue multiple concurrent I/Os per page if the device supports it.

---

### Decision 6: Device Trait — Completion-Based for Runtime-Agnostic I/O

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), per user directive on no-async-in-core

**Decision:** The `Device` trait uses completion callbacks (`Box<dyn FnOnce(Result<usize, IoError>) + Send>`) rather than returning `Future`s.

**Trait design:**
```rust
pub trait Device: Send + Sync {
    fn read_async(&self, offset: u64, buf: &mut [u8],
                  callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>);
    fn write_async(&self, offset: u64, buf: &[u8],
                   callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>);
    fn read_sync(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError>;
    fn write_sync(&self, offset: u64, buf: &[u8]) -> Result<usize, IoError>;
    fn flush(&self);
    fn truncate(&self, new_len: u64) -> Result<(), IoError>;
    fn sector_size(&self) -> usize;
    fn max_concurrent_ios(&self) -> usize;
}
```

**Rationale:**
1. **Runtime agnostic:** Callbacks work with any execution model — bare threads, Tokio, monoio, io_uring direct, IOCP, etc. No `Future` or `async` in the trait.
2. **C++ alignment:** Matches C++ FASTER's `AsyncIOCallback` pattern. Behavioral equivalence is straightforward.
3. **Implementor freedom:** A Tokio-based device calls the callback from a Tokio task. A threaded device calls it from an I/O completion thread. A simulation device calls it synchronously. The core doesn't care.
4. **Sync path included:** `read_sync` and `write_sync` for blocking callers. Implementations can delegate to the async path or use platform-native sync I/O.

**Alternatives rejected:**
- **`async fn` in trait:** Requires `async-trait` or Rust's native async trait support (RPITIT). Couples the trait to a specific async model.
- **`Future`-returning trait:** Similar to async fn but manual. Still requires an executor to poll.
- **Poll-based trait:** Too low-level for most implementors. Callbacks are simpler.

**Trade-offs accepted:**
- `Box<dyn FnOnce(...)>` incurs a heap allocation per I/O operation. Mitigation: for the hot path (in-memory operations), no I/O is issued and no callback is allocated. For the cold path (disk I/O), the allocation is dwarfed by I/O latency. If profiling shows this matters, we can switch to a pre-allocated callback pool.
- Implementors must manage their own completion threading. Mitigation: provide reference implementations for common patterns (threaded, Tokio, io_uring).

---

### Decision 7: C FFI — Opaque Handles, Not Translated Types

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Chirrut (FFI)

**Decision:** The C FFI exposes opaque pointers (`typedef struct FasterKv FasterKv;`) rather than translating Rust types into C-compatible structs.

**Rationale:**
1. **Encapsulation:** Internal representation can change without breaking the C ABI. The C consumer never sees `RecordInfo`, `Address`, or `HashBucket` — only handles and byte buffers.
2. **Safety:** Opaque handles prevent C code from directly manipulating internal state. All mutations go through the FFI function API, which validates inputs.
3. **Simplicity:** No need to maintain parallel C struct definitions for every Rust type. The FFI surface is small: ~15 functions.
4. **Precedent:** This is the standard pattern for Rust FFI (see: rusqlite, lmdb-rkv, tantivy-ffi).

**Alternatives considered:**
- **Translated types:** Expose C-compatible versions of `RecordInfo`, `Address`, etc. Provides more flexibility for C consumers but creates a maintenance burden and ABI stability risk.
- **Flat C API with value types:** Pass all data as raw byte buffers. Too low-level; loses type safety.

**Trade-offs accepted:**
- C consumers cannot inspect internal state for debugging. Mitigation: provide `faster_debug_info()` function returning a human-readable string.
- Every operation crosses the FFI boundary (function call overhead). Mitigation: batch operations for bulk use cases.

---

### Decision 8: Checkpoint Format — Own Format (Not C++/C# Compatible)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Tarkin (storage), Cassian (cross-impl)

**Decision:** The Rust implementation uses its own checkpoint format. Checkpoints are NOT binary-compatible with C++ or C# FASTER. Semantic equivalence is verified via cross-implementation comparison tests.

**Format:**
- Binary format with a version header (4-byte magic + 4-byte version number)
- Record layout may differ (different alignment rules, different padding)
- Metadata (`info.dat`) uses a defined binary schema with explicit field widths
- Forward-compatible: unknown fields are skipped (length-prefixed sections)

**Rationale:**
1. **Alignment differences:** Rust's alignment rules differ from C++ (`#pragma pack`) and C# (`StructLayout.Pack`). Forcing binary compatibility would require unsafe casting and platform-specific code.
2. **Record format evolution:** Our inline variable-length format may not match C++ or C# exactly. Forcing compatibility constrains future optimization.
3. **Version independence:** Checkpoint format can evolve independently of C++/C# releases.
4. **Migration path:** If cross-format compatibility is needed later, a conversion tool is simpler than constraining the format upfront.

**Alternatives considered:**
- **Binary-compatible with C++:** Would enable zero-downtime migration. But imposes significant constraints on record layout and requires matching C++ struct packing exactly.
- **Binary-compatible with C#:** Same issues plus C#'s managed object log has no Rust equivalent.
- **Portable format (protobuf, flatbuffers):** Overhead for checkpoint metadata is minimal, but data pages are raw memory — no serialization format applies.

**Trade-offs accepted:**
- No zero-downtime migration from C++/C# to Rust. Users must export/import data.
- Cross-implementation comparison tests must compare semantics (key-value pairs), not bytes.

---

### Decision 9: Error Handling — `Result<Status, Error>` Taxonomy

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Mando (Rust idioms)

**Decision:** Operations return `Result<Status, Error>` where `Status` is a bitflag-style success type and `Error` is a `thiserror`-derived error enum.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status(u16);

impl Status {
    pub const OK: Status = Status(0);
    pub const FOUND: Status = Status(1 << 0);
    pub const NOT_FOUND: Status = Status(1 << 1);
    pub const PENDING: Status = Status(1 << 2);
    pub const IN_PLACE_UPDATED: Status = Status(1 << 3);
    pub const CREATED_RECORD: Status = Status(1 << 4);
    pub const COPY_UPDATED: Status = Status(1 << 5);
    pub const EXPIRED: Status = Status(1 << 6);

    pub fn found(&self) -> bool { self.0 & Self::FOUND.0 != 0 }
    pub fn is_pending(&self) -> bool { self.0 & Self::PENDING.0 != 0 }
    // ... etc
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store is closed")]
    StoreClosed,
    #[error("session already disposed")]
    SessionDisposed,
    #[error("checkpoint failed: {reason}")]
    CheckpointFailed { reason: String },
    #[error("recovery failed: {reason}")]
    RecoveryFailed { reason: String },
    #[error("hash index full (overflow limit reached)")]
    IndexFull,
    #[error("record too large: {size} bytes (max: {max})")]
    RecordTooLarge { size: usize, max: usize },
}
```

**Rationale:**
1. **Rust idiom:** `Result<T, E>` is the standard error handling pattern. Using it enables `?` operator and standard error propagation.
2. **Status vs. Error:** `Status` represents normal operational outcomes (found, not found, pending). `Error` represents exceptional conditions (I/O failure, invalid state). This separation prevents conflating "key not found" (normal) with "disk failed" (exceptional).
3. **Bitflags on Status:** Allows combinations (e.g., `CREATED_RECORD | NOT_FOUND` = "created because key didn't exist"). Matches C++/C# status semantics.
4. **thiserror:** Generates `Display` and `From` impls automatically. Zero runtime overhead.

**Alternatives considered:**
- **Single enum for everything:** `enum Result { Found(V), NotFound, Pending, Error(E) }`. Loses composability and `?` integration.
- **C-style error codes:** Not idiomatic Rust. Forces manual error checking.

---

### Decision 10: Session Model — Thread-Affine (!Send)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Dooku (C# insights)

**Decision:** Sessions are marked `!Send` (cannot be moved between threads). This enforces compile-time thread affinity matching FASTER's mono-threaded session contract.

**Implementation:**
```rust
pub struct Session<'a, K, V, F: Functions<K, V>> {
    // ... fields
    _not_send: PhantomData<*const ()>,  // Makes Session !Send + !Sync
}
```

**Rationale:**
1. **Safety:** FASTER sessions are not thread-safe. Concurrent use of a single session is undefined behavior in C++/C#. In Rust, `!Send` makes this a compile-time error.
2. **Performance:** No synchronization overhead within a session. Epoch protection, pending queues, serial numbers — all thread-local with no atomic operations.
3. **C# lesson learned:** Dooku's analysis notes that C# sessions are mono-threaded "but nothing prevents misuse at compile time." Rust's type system eliminates this class of bugs.

**Alternatives considered:**
- **Send + Sync sessions with internal locking:** Enables sharing across threads but defeats the performance purpose. Sessions are designed for thread-local use.
- **Send but not Sync:** Allows moving sessions between threads but not sharing. Could be useful for work-stealing runtimes, but FASTER's epoch registration is thread-specific. Moving sessions requires re-registering, which adds complexity for marginal benefit.

**Trade-offs accepted:**
- Users who want multi-threaded access must create multiple sessions (one per thread). This is the intended usage pattern.
- Async runtimes that move tasks between threads (Tokio's work-stealing) need special handling in the adapter layer (pin session to a specific thread).

---

### Decision 11: Variable-Length Key/Value Strategy

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Cassian (cross-impl analysis)

**Decision:** Support both fixed-size and variable-length keys/values through a unified `Key`/`Value` trait system. Variable-length records use inline storage (size-prefixed, contiguous with record header).

```rust
pub trait Key: Eq + Hash {
    fn serialized_size(&self) -> usize;
    fn serialize_into(&self, buf: &mut [u8]);
    fn deserialize_from(buf: &[u8]) -> &Self;
}

pub trait Value {
    fn serialized_size(&self) -> usize;
    fn serialize_into(&self, buf: &mut [u8]);
    fn deserialize_from(buf: &[u8]) -> &Self;
}

// Blanket implementations for fixed-size types
impl Key for u64 { ... }  // 8 bytes, no length prefix
impl Key for [u8; 16] { ... }  // 16 bytes, no length prefix
impl Key for Vec<u8> { ... }  // 4-byte length prefix + data
impl Value for Vec<u8> { ... }
```

**Rationale:**
1. **Unified approach:** One trait for both fixed and variable. No separate allocator types (unlike C#'s `BlittableAllocator` vs. `VarLenBlittableAllocator`).
2. **Inline storage:** C++ model. Better cache locality than C#'s separate object log.
3. **Zero-copy reads:** `deserialize_from` returns a reference into the record buffer. No allocation for reads.
4. **Extensible:** Users can implement `Key`/`Value` for their own types.

**Trade-offs accepted:**
- In-place update of variable-length values only works if new value is less than or equal to old value size. Otherwise, copy-update (append new record).
- Record size must be known before allocation. This requires computing serialized sizes before calling allocate.

---

### Decision 12: Grow (Resize) Protocol

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Thrawn (architecture), Grievous (C++ reference)

**Decision:** Hash index growth doubles the table size, coordinated through the epoch system. Growth is a multi-phase operation similar to checkpointing.

**Protocol:**
1. **Trigger:** User calls `grow_index()` or automatic trigger at load factor threshold (configurable, default: disabled).
2. **Allocate:** New table of 2x size allocated (does not replace old yet).
3. **Split phase:** Epoch-coordinated. Each thread processes its share of old buckets:
   - For each old bucket, rehash entries to determine new bucket (old_index vs. old_index + old_size)
   - CAS entries into new table
   - Old bucket marked as "split complete"
4. **Swap:** Once all buckets split (verified via epoch drain), atomically swap old-to-new table pointer.
5. **Reclaim:** Old table memory freed via epoch drain (deferred until all threads have observed the new table).

**Key constraint:** Hash table only grows, never shrinks. Shrinking would invalidate addresses stored in the hash table entries (addresses encode a bucket index implicitly via the hash function).

**Rationale:**
1. **Non-blocking:** Operations continue during growth. Threads that encounter a "being split" bucket assist or wait based on epoch state.
2. **Epoch coordination:** Same mechanism as checkpoint phase transitions. Reuses existing infrastructure.
3. **No address invalidation:** Growing preserves the property that `hash(key) % old_size` maps to either `hash(key) % new_size` or `hash(key) % new_size + old_size`.

**Trade-offs accepted:**
- 2x memory spike during growth (old + new tables coexist). Mitigation: growth is infrequent; size table appropriately at construction.
- Growth is relatively slow (proportional to table size). Mitigation: operations continue during growth; growth is background work.
- No shrink. Mitigation: if memory pressure is a concern, reconstruct the store with a smaller table.


---

## 14. Risk Register

This register catalogs every significant risk to the Rust FASTER implementation, with likelihood (L), impact (I), and specific mitigations. Risks are organized by category and rated on a 5-point scale (1=Very Low, 2=Low, 3=Medium, 4=High, 5=Very High). Risk score = L x I.

### 14.1 Correctness Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| C1 | **Lock-free algorithm bugs in hash index** — CAS loops have subtle ABA problems, lost updates, or ordering violations that manifest only under specific thread interleavings | 4 | 5 | 20 | Deterministic simulation testing (Jyn) with adversarial scheduling. Linearizability checking on all concurrent operations. Model-check critical CAS loops. Property-based tests with high iteration counts. |
| C2 | **Epoch-based reclamation: use-after-free** — Thread accesses memory freed by another thread that has already advanced its epoch. Possible if drain list callback fires prematurely or epoch tracking has an off-by-one error | 3 | 5 | 15 | Miri verification of epoch unit tests. Custom property tests that model epoch state per-thread. Simulation testing with epoch boundary stress scenarios. AddressSanitizer on all integration tests. |
| C3 | **Crash recovery data loss or corruption** — Checkpoint metadata inconsistent with log contents after power failure. Partial writes corrupt recovery. | 3 | 5 | 15 | Deterministic crash simulation at every checkpoint phase transition. Checksums on metadata. Write-ahead-of-metadata pattern (data flushed before metadata written). Test with 10,000+ crash points. |
| C4 | **CPR version boundary bugs** — Threads in different versions (v, v+1) interact incorrectly during checkpoint. Records created in wrong version. Dual execution context swap has edge cases. | 3 | 5 | 15 | Direct port of C++/C# CPR state machine with behavioral equivalence testing. Cross-implementation comparison of checkpoint/recovery results. Simulation testing of version boundary transitions. |
| C5 | **Epoch drain list ordering** — Callbacks execute in wrong order or at wrong epoch, causing premature memory reclamation or missed phase transitions | 2 | 5 | 10 | Unit tests for drain list ordering with multiple queued actions at different epochs. Property tests verifying monotonic epoch advancement. |
| C6 | **Hash table grow correctness** — During resize, entries are lost, duplicated, or placed in wrong bucket. Concurrent operations during growth see inconsistent state. | 3 | 4 | 12 | Dedicated grow-under-concurrency test suite. Verify all entries present after grow. Linearizability check during grow operations. |
| C7 | **Pending operation completion ordering** — Callbacks for pending operations fire in wrong order or are dropped, causing silent data loss or stale reads | 2 | 4 | 8 | Integration tests that force all operations to go pending (small in-memory region). Verify callback counts match operation counts. Timeout detection for lost callbacks. |

### 14.2 Performance Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| P1 | **Cache miss amplification** — Rust's ownership model forces extra indirection or copying that C++ avoids with placement new and raw pointers, increasing cache misses on the hot path | 3 | 4 | 12 | Profile early (Phase 3) with `cachegrind`. Use `unsafe` pointer arithmetic for record access where cache performance demands it. Benchmark against C++ continuously. |
| P2 | **CAS contention under high thread counts** — Hot buckets experience excessive CAS retries, reducing throughput superlinearly as thread count increases beyond 16 | 3 | 3 | 9 | Benchmark CAS retry rates explicitly. Exponential backoff with `spin_loop` hints. Consider per-bucket combining if contention exceeds threshold. NUMA-aware bucket placement. |
| P3 | **I/O callback overhead** — `Box<dyn FnOnce>` allocation on every I/O operation adds per-operation overhead that degrades throughput for disk-heavy workloads | 2 | 3 | 6 | Profile I/O path. If significant, switch to pre-allocated callback pool or typed callback slots (no Box). On in-memory hot path, no callbacks allocated. |
| P4 | **Epoch acquire/release overhead** — Per-operation atomic store for epoch tracking adds latency to every operation, even in-memory | 2 | 3 | 6 | Benchmark epoch overhead in micro-benchmarks. Batch epoch protection across multiple operations within a session. Use `Relaxed` ordering where safe. |
| P5 | **Page allocation contention** — Atomic tail-address bump becomes a bottleneck when many threads allocate concurrently | 2 | 3 | 6 | Use `fetch_add` (single atomic) not CAS loop for tail bump. Consider per-thread allocation buffers if contention is measured. |
| P6 | **Rust monomorphization bloat** — Heavy use of generics (K, V, F) causes code bloat from monomorphization, increasing instruction cache misses | 2 | 2 | 4 | Use trait objects for cold paths. Keep generic-parameterized code minimal. Measure binary size and icache miss rates. |

### 14.3 Complexity Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| X1 | **Checkpoint state machine complexity** — Multi-phase state machine with epoch coordination, version management, and I/O tracking is the most complex subsystem. Bugs here are hard to find and reproduce. | 4 | 4 | 16 | Dedicated Phase 5 with 5-6 week budget. Jyn's simulation testing framework specifically targets checkpoint phases. State machine formally documented with transition table. |
| X2 | **Unsafe code volume** — Hash index, record access, page management, and FFI all require `unsafe`. High `unsafe` surface area increases audit burden and risk of soundness bugs. | 3 | 4 | 12 | Maul's security audit (Phase 9). Every `unsafe` block has a SAFETY comment. Miri verification for non-I/O paths. Minimize `unsafe` surface: encapsulate in small, well-tested modules. Target < 5% of total LOC. |
| X3 | **Callback threading model complexity** — Without async/await, multi-step operations (pending read -> I/O -> continue -> user callback) require manual state threading through callbacks. Error-prone and hard to debug. | 3 | 3 | 9 | Define clear state machine for each pending operation type. Exhaustive integration tests for all pending paths. Document callback flow with sequence diagrams. |
| X4 | **Dual execution context (prev/cur) during CPR** — Sessions maintain two execution contexts that swap during checkpoint. Incorrect swap timing or context selection causes operations to use wrong version. | 3 | 4 | 12 | Port directly from C++/C# with behavioral equivalence tests. Simulation testing of context swap timing. Unit tests for every phase transition. |
| X5 | **Feature interaction complexity** — Checkpoint + grow + GC + compaction can all be in progress simultaneously. Interactions between these subsystems create exponential state space. | 3 | 4 | 12 | Phase 9 simulation testing explores concurrent subsystem interactions. Limit concurrent background operations in v1 (e.g., no grow during checkpoint). Document allowed combinations. |

### 14.4 Compatibility Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| K1 | **Cross-platform I/O differences** — O_DIRECT behavior, alignment requirements, and file system semantics differ between Linux, macOS, and Windows. io_uring availability varies by kernel version. | 3 | 3 | 9 | Abstract all platform-specific I/O behind `Device` trait. Test on Linux (primary) and macOS (secondary). Windows: document as best-effort. Use conditional compilation (`#[cfg(target_os)]`) for platform-specific code. |
| K2 | **C FFI portability** — Different C compilers (gcc, clang, MSVC) have different ABI conventions, alignment rules, and calling conventions. | 2 | 3 | 6 | Use `#[repr(C)]` for all FFI-visible structs. Test with gcc and clang on CI. Use `cbindgen` for header generation. Keep FFI surface minimal (opaque handles). |
| K3 | **Async runtime compatibility** — Future Rust async runtimes may have different APIs or conventions that our adapter pattern doesn't accommodate. | 2 | 2 | 4 | Design adapter trait with minimal assumptions. Validate with at least 2 different runtimes (Tokio + one other). Document adapter authoring guide. |
| K4 | **Minimum Supported Rust Version (MSRV)** — Using nightly features or very recent stable features limits adoption. | 2 | 2 | 4 | Target stable Rust (latest - 2 versions). No nightly features in core crate. Document MSRV in Cargo.toml. Test on MSRV in CI. |

### 14.5 Scope Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| S1 | **Feature creep toward Tsavorite/Garnet** — Pressure to include Tsavorite features (revivification, record locking enhancements, new index types) before v1 is stable | 3 | 4 | 12 | User directive: Tsavorite/Garnet is explicitly NOT for v1. Captured in decisions.md. Architecture designed for extensibility (trait-based index, log, device) but features deferred to v2+. Thrawn has veto authority on scope expansion. |
| S2 | **F2 two-tier scope expansion** — F2 (hot/cold stores, ColdIndex) is complex (~5 files in C++) and could consume months if pulled into scope prematurely | 2 | 4 | 8 | Explicitly deferred. Architecture uses `trait IndexProvider` and `trait LogProvider` to allow F2 extension without core changes. Phase 2 (hash index) does not preclude ColdIndex addition later. |
| S3 | **FasterLog scope** — FasterLog is a standalone subsystem (~120KB in C#) that could distract from core KV work | 2 | 3 | 6 | Deferred to Full phase (post-MVP). FasterLog is relatively self-contained and can be implemented independently. |
| S4 | **Remote server / distributed features** — Requests for TCP/gRPC server, pub-sub, or distributed recovery before core is mature | 2 | 4 | 8 | Explicitly deferred to Phase 3+. Documented in roadmap. Not architecturally precluded but not prioritized. |
| S5 | **Timeline pressure** — 6-9 month estimate may face pressure to compress, leading to quality shortcuts | 3 | 4 | 12 | User directive: unlimited budget/time, quality is everything. Document this in all planning artifacts. Thrawn enforces quality gates at each phase boundary. No phase completes without success criteria met. |

### 14.6 Risk Heat Map Summary

**Critical (Score >= 15) — Require active mitigation:**
- C1: Lock-free algorithm bugs (20)
- C2: Epoch use-after-free (15)
- C3: Crash recovery corruption (15)
- C4: CPR version boundary (15)
- X1: Checkpoint state machine (16)

**High (Score 10-14) — Require monitoring:**
- C6: Hash table grow (12)
- P1: Cache miss amplification (12)
- X2: Unsafe code volume (12)
- X4: Dual execution context (12)
- X5: Feature interaction (12)
- S1: Tsavorite scope creep (12)
- S5: Timeline pressure (12)
- C5: Epoch drain ordering (10)

**Medium (Score 6-9) — Addressed by standard practices:**
- P2: CAS contention (9)
- K1: Cross-platform I/O (9)
- X3: Callback threading (9)
- C7: Pending operation ordering (8)
- S2: F2 scope expansion (8)
- S4: Remote/distributed (8)
- P3: I/O callback overhead (6)
- P4: Epoch overhead (6)
- P5: Page allocation contention (6)
- K2: C FFI portability (6)
- S3: FasterLog scope (6)

**Low (Score < 6) — Accept:**
- P6: Monomorphization bloat (4)
- K3: Async runtime compat (4)
- K4: MSRV (4)

---

*End of Part 3. Sections 1-10 are in Parts 1 and 2.*
