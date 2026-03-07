# Squad Decisions

## Active Decisions

### 1. Rust Async Model Should Use async/await, Not Callback Chains

**Agent:** Saruman (C++ Expert)  
**Date:** 2026-03-05  
**Status:** Recommendation for Team Discussion  
**Impact:** Core API design — affects all async operations

**Decision:** Use async/await instead of manual callback chains.

**Rationale:**
1. Simpler — no manual context allocation/deallocation
2. Safer — borrow checker prevents UAF bugs
3. Composable — natural chaining of async operations
4. Debuggable — stack traces work with async/await
5. Standard — Tokio ecosystem expects async/await

**Implications:**
- All I/O operations return `impl Future<Output = Result<T>>`
- No CompletePending; Tokio runtime handles polling
- Simpler testing with `#[tokio::test]`
- Must use Tokio runtime, not bare threads

**Next Steps:**
- [ ] Gandalf: Validate architecture alignment
- [ ] Frodo: Confirm no blockers for implementation

---

### 2. Rust Core Must Use Callbacks, Not async/await (User Directive)

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T17:10:00Z  
**Status:** DECIDED — Binding override  
**Impact:** Core architectural constraint — async runtime independence

**Decision:** The core Rust FASTER implementation MUST NOT take any dependencies on an async runtime or use async/await syntax. Manual callbacks are the right model.

**Rationale:**
1. Tokio has scaling limitations
2. Other async runtimes (kimojio, compio, monoio) are not standardized
3. For highest performance, implementation must adapt to multiple async runtimes
4. Core API/architecture should be synchronous, amenable to seamless integration via thin adapter layers

**Why:** Architectural constraint from project owner — non-negotiable design requirement.

**Implications:**
- Core crate: zero async runtime dependencies, no async/await syntax
- Completion/pending operations: callback-based or polling-based, not Future-based
- Async adapters: separate crates/feature flags per runtime (tokio, kimojio, compio, monoio)
- I/O traits: must abstract over sync, threaded, and various async I/O models
- **Overrides Decision #1** (Saruman async/await recommendation)

**Override Resolution:**
- Saruman recommended async/await for ergonomics
- User directive takes precedence: core must remain async runtime-agnostic
- Async ergonomics achieved via adapter layer (Elrond responsibility)
- See Decision #5 (Async Integration Strategy) for bridge pattern

---

### 3. Async Integration: Callback Bridge to Future Pattern

**Agent:** Gandalf (Lead Architect, Part 2)  
**Date:** 2026-03-05T18:33:00Z  
**Status:** DECIDED  
**Impact:** Bridges callback core to Rust async ecosystem

**Decision:** Implement callback→Future adapter pattern via oneshot channels and Waker integration.

**Rationale:**
- Core must remain callback-based (user directive)
- Rust ecosystem expects Futures/async/await
- Adapter crate translates between models at boundary
- Per-runtime adapters (tokio, kimojio, compio, monoio) use appropriate integration points

**Patterns:**
1. Oneshot channel: callback writes completion to sender, Future polls receiver
2. Waker integration: callback invokes Waker::wake() for runtime notification
3. Thread-spawning adapters: callback delegates to thread pool for blocking Device I/O

**Implications:**
- Elrond leads async adapter design and implementation
- Zero performance cost for sync path (direct callback)
- Minimal overhead for async path (channel or Waker call)
- Testing: adapter contracts verified against mock I/O

---

### 4. Rust Implementation Scope: MVP → Full → Deferred Phases

**Agent:** Frodo (Cross-Implementation Expert)  
**Date:** 2026-03-05  
**Status:** Critical for Team Consensus  
**Impact:** Defines entire 6-12 month roadmap

**Decision:** Three-phase feature roadmap based on architectural complexity and value.

**MVP (3-4 months) — C++ Baseline with Binary Metadata:**
✅ Core KV (Read, Upsert, RMW, Delete)
✅ Hybrid log + hash index + epochs
✅ Fold-over + snapshot checkpoints
✅ CPR recovery with session commit points
✅ Sync + async operations (callback + Future)
✅ Local + null storage devices
✅ Optional per-record locking, hash table growth, log compaction

**Full (6-8 months) — Add Valuable C# Features:**
✅ Incremental snapshots (delta log)
✅ Read cache (immutable secondary)
✅ Azure storage device
✅ FasterLog (standalone append log)
✅ Scan/iteration over log
✅ Async iterators (Stream trait)

**Deferred (v2+) — Complex, Uncertain Value:**
⏸️ F2 two-tier (hot/cold stores) — Phase 2
⏸️ ColdIndex (disk-based 2-level index) — Phase 2
⏸️ Remote server (TCP/gRPC) — Phase 3
⏸️ Distributed recovery (DPR) — Phase 3

**Rationale:**
- **MVP:** C++ proven in production, simpler implementation, no managed memory
- **Full:** FasterLog is standalone use case (~120KB, 12 files), C# incremental snapshots valuable
- **Deferred:** F2 complex (5 files, coordination), Remote large surface (~80 files), uncertain demand

**Implications:**
- Design traits to allow F2 extension later (trait IndexProvider, trait LogProvider)
- MVP ~10K LOC, Full ~20K LOC
- Performance target: match C++ within 10%
- Cross-language checkpoint compatibility required

---

### 5. C# API Insights for Rust Design

**Agent:** Faramir (C# Expert)  
**Date:** 2026-03-05  
**Status:** Design Recommendations  
**Impact:** Public API ergonomics

**Key Simplifications:**

1. **Simplify Callback Interface:**
   - C# has 19 methods (SingleReader, ConcurrentReader, InitialUpdater, etc.)
   - Rust: ~5 core methods + default implementations
   ```rust
   trait FasterFunctions<K, V> {
       fn read(&self, key: &K, value: &V, input: &Self::Input) -> Self::Output;
       fn upsert(&self, key: &K, old: Option<&V>, input: &Self::Input) -> V;
       fn rmw(&self, key: &K, value: &mut V, input: &Self::Input) -> RmwResult;
       fn delete(&self, key: &K, value: &mut V) -> bool;
   }
   ```

2. **Reduce Generic Parameters:**
   - C# Session: 6 type parameters (Key, Value, Input, Output, Context, Functions)
   - Rust: 3-4 parameters (K, V, F) with associated types (F::Input, F::Output, F::Context)

3. **Replace ref Parameters with Borrowing:**
   - C#: `session.Upsert(ref key, ref value)` (verbose)
   - Rust: `session.upsert(&key, &value)` (natural ownership)

4. **Unify Async Patterns:**
   - C#: Three async patterns (ValueTask, CompletePending, callbacks)
   - Rust: Single async/await pattern with no manual pending

5. **RAII for Epoch Protection:**
   - C#: Manual Resume()/Suspend() (easy to forget)
   - Rust: Scope guards auto-suspend on drop

6. **Compile-Time Session Safety:**
   - C#: Sessions mono-threaded but nothing prevents misuse at compile time
   - Rust: Use !Send marker for compile-time prevention

7. **Builder Pattern for Configuration:**
   - C#: Two APIs (FasterKVSettings vs constructor) with magic numbers
   - Rust: Unified builder pattern

**Ergonomics Improvement:** 5.7/10 (C#) → 8/10 (estimated Rust)

---

### 7. Rust Async Model Overridden by User Directive (Decision #1 Superseded)

**Original Agent:** Saruman (C++ Expert)  
**Original Date:** 2026-03-05  
**Original Status:** Recommendation for Team Discussion  
**Override Date:** 2026-03-05T17:10:00Z  

**What Changed:**
- Saruman recommended async/await (Decision #1)
- User directive (qbradley) overrides: core must be callback-based, no async runtime
- See Decision #2 for full directive rationale

**Resolution Path:**
- **Core:** Callback-based per user directive (non-negotiable)
- **Ergonomics:** Async adapter crates (Decision #3) provide Future/async/await to users
- **Best Practice:** Saruman's insights on safety/composability addressed in adapter design

**Status:** RESOLVED — No team action needed; architecture accommodates both user constraint and Saruman's ergonomic concerns.

---

### 8. Future Investigation: Tsavorite/Garnet Features

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T17:26:00Z  
**Status:** Deferred — NOT for v1, forward-looking roadmap  
**Impact:** No v1 work; keep architecture extensible for future

**Decision:** Do NOT incorporate Tsavorite features in v1. Flag for v2+ investigation.

**What:** Tsavorite is a fork of C# FASTER with features supporting Garnet (Redis-compatible cache-store by Microsoft).

**Why:** Forward-looking roadmap item for team memory.

**References:**
- Tsavorite intro: https://microsoft.github.io/garnet/docs/dev/tsavorite/intro
- Garnet docs: https://microsoft.github.io/garnet/
- Garnet repo: https://github.com/microsoft/garnet

**Implications:**
- Do NOT block or influence v1 architecture
- DO keep architecture extensible enough that Tsavorite features could be added later (e.g., trait-based storage provider pattern)
- Frodo: Tag in Full/Deferred phase review for potential Phase 2 evaluation

---

### 9. Gandalf Rust FASTER Architecture: 12 Key Decisions

**Lead Architect:** Gandalf  
**Date:** 2026-03-05T18:33:00Z  
**Status:** DOCUMENTED — Binding architectural decisions  
**Impact:** Binding for all implementation phases

**Summary of 12 Decisions:**

1. **No async/await in core** (user directive, overrides Saruman #1) — Callback/completion model. Async adapter crates per runtime.

2. **Custom epoch implementation** (not crossbeam-epoch) — FASTER's drain list semantics, checkpoint integration, thread entry management require purpose-built system. ~500 lines, extensively tested.

3. **Inline variable-length records** (C++ model) — Contiguous key+value storage in hybrid log. No dual-log. Better cache locality.

4. **Rust atomic patterns for hash index** — `[AtomicU64; 8]` cache-line buckets, CAS with tentative bit, `AcqRel` ordering on mutations, `Acquire` on lookups. Zero locks.

5. **32 MB default page size** — Matching C++/C# defaults. 512-byte sector alignment for O_DIRECT. Configurable via builder.

6. **Completion-based Device trait** — Callbacks, not Futures. Runtime-agnostic. Includes sync path. Box<dyn FnOnce> for callbacks (profile before optimizing).

7. **Opaque handles for C FFI** — No translated types. ~15 function API surface. cbindgen for header generation.

8. **Own checkpoint format** (not C++/C# compatible) — Binary with version header. Forward-compatible (length-prefixed sections). Semantic equivalence verified via cross-impl tests.

9. **Result<Status, Error> taxonomy** — Bitflag Status for operational outcomes, thiserror Error for exceptional conditions. Composable via `?` operator.

10. **Thread-affine sessions (!Send)** — Compile-time enforcement of FASTER's mono-threaded session contract. PhantomData<*const ()> marker.

11. **Unified Key/Value traits** — Single trait system for fixed and variable-length. Inline storage, zero-copy reads, user-extensible.

12. **Epoch-coordinated grow protocol** — Table doubles, split phase via epoch coordination, no shrink. Reuses checkpoint-style phase transitions.

**Team Implications:**
- **Elrond:** Async adapter design must bridge callback→Future (oneshot channels or Waker integration)
- **Aragorn:** All core code sync-only, no tokio dependency
- **Sam:** Device trait implementation is completion-based, not Future-based
- **Éowyn:** Simulation framework must model callback completion ordering
- **Galadriel:** Unsafe audit scope includes hash index atomics, record pointer arithmetic, FFI boundary

**Risk Register:** 25 identified risks across 5 categories. 5 critical (score ≥ 15), 8 high (score 10-14). Primary mitigations: deterministic simulation testing, Miri verification, security audit, cross-implementation comparison.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md` (288 KB, 6101 lines, 14 sections)

---

### 6. User Directive: Unlimited Budget, Best Model

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T16:50:00Z  
**Status:** Active  

**Directive:** Use unlimited budget. Deploy the best available model (claude-opus-4.6) for every task.

**Application:** All squad tasks moving forward prioritize quality and capability over cost.

---

### 10. User Directive: Iterative MVP Process

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T19:08:00Z  
**Status:** Active — Binding development process  
**Impact:** Team-wide development methodology

**Directive:** Develop using an iterative process. Create an MVP first, then iterate until full feature set. At each iteration, ensure the implementation is performant, secure, functionally correct, maintainable, and useful within the limits of features implemented so far. Use bottom-up approach for implementation given the top-down architecture design.

**Implications:**
- MVP is the foundation — no half-finished features
- Quality gate on every iteration (clippy, fmt, tests, Miri)
- Each phase produces a working, performant system
- Architecture supports incremental feature addition

---

### 11. User Directive: Kimojio Async Runtime Context

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T19:08:00Z  
**Status:** Reference Information  
**Impact:** Team knowledge base

**What:** Kimojio async runtime is at https://github.com/Azure/kimojio-rs. qbradley is the designer and implementer. Team can ask qbradley directly about kimojio design questions.

**Implications:**
- First-party knowledge source for async runtime integration
- Consider kimojio as adapter target (alongside tokio, compio, monoio)
- Direct communication channel for design questions

---

### 12. User Directive: Leverage PAW Workflow for Implementation

**User:** qbradley (via Copilot)  
**Date:** 2026-03-05T19:08:00Z  
**Status:** Investigation Phase  
**Impact:** Development process optimization

**Directive:** Consider leveraging PAW (phased-agent-workflow) for building features and PAW-Review with Society of Thought for code review within Squad workflow. Investigate integration.

**Status:** PAW available via task agent types:
- `paw-workflow/PAW` — implementation workflow
- `paw-workflow/PAW Review` — review workflow with multi-model deliberation

**Team Action:**
- Gandalf approved PAW for 5 complex MVP items (1f, 1g, 1h, 2d, 2e)
- Direct implementation for 11 simpler items
- Further integration to be determined post-MVP-Phase-1

---

### 13. MVP Iteration 1 Implementation Plan Approved

**Agent:** Gandalf (Lead Architect)  
**Date:** 2026-03-05T19:08:00Z  
**Status:** APPROVED FOR EXECUTION  
**Impact:** Team-wide — assigns all Phase 1 and Phase 2 work

**Decision:** Execute MVP Iteration 1 as 16 work items across Phases 1 (Foundation, 9 items) and 2 (Hash Index, 6 items), using bottom-up ordering with maximum parallelism.

**Key Points:**
1. **Phase 1** (Weeks 1–4): Workspace, newtypes, status/error, hash traits, CI, record format, epoch system, memory allocator, foundation tests.
2. **Phase 2** (Weeks 5–8, overlapping Phase 1 Week 4): Bucket entry, overflow pool, bucket structure, hash table core, concurrent ops, hash index tests.
3. **PAW workflow** for 5 complex items: record format (1f), epoch system (1g), memory allocator (1h), hash table core (2d), concurrent ops (2e).
4. **Direct implementation** for 11 simpler items (newtypes, enums, CI, thin wrappers, tests).
5. **Quality bar:** Every work item must pass clippy, fmt, tests, and Miri (where applicable) before merge.
6. **thiserror** added as acceptable `faster-core` dependency (zero runtime cost proc-macro for error types).

**Assignments:**
- **Aragorn:** Primary on 7 items (1b, 1c, 1d, 1f, 1g, 2a, 2c, 2d, 2e)
- **Sam:** Primary on 2 items (1h, 2b), support on atomics/alignment
- **Boromir:** Primary on 3 items (1a, 1e, 1i, 2f)
- **Galadriel:** Unsafe audit on all PAW items
- **Frodo:** Behavioral spec extraction from C++/C# for validation
- **Legolas:** Benchmark design and early performance analysis
- **Gandalf:** Review authority on all PRs

**Artifact:** `.squad/agents/gandalf/mvp-iteration-1-plan.md` (46KB)

**Implications:**
- All team members should read the plan before starting work
- Work items are to be executed in dependency order per the graphs in the plan
- No work item merges without passing the stated success criteria
- Gandalf has final review authority on all PRs per architecture governance

---

### 10. Typed Page/Offset Newtypes for LogicalAddress API

**Agent:** Aragorn (Rust Expert)  
**Date:** 2026-03-05  
**Status:** Implemented  
**Impact:** All code that constructs or destructures `LogicalAddress`

**Decision:** `LogicalAddress::new()` takes `Page` and `Offset` newtypes rather than raw `u32` parameters. Page/offset extraction methods return these newtypes.

```rust
// What we did:
LogicalAddress::new(Page(10), Offset(256))
addr.page()   // → Page(10)
addr.offset()  // → Offset(256)

// What we didn't do:
LogicalAddress::new(10u32, 256u32)  // Would compile even if swapped
```

**Rationale:**
1. Prevents argument transposition — `new(offset_val, page_val)` won't compile
2. Self-documenting at call sites — `Page(10)` is clearer than bare `10u32`
3. Zero runtime cost — both `#[repr(transparent)]` over `u32`, identical codegen
4. Composable — downstream code can accept `Page` or `Offset` directly

**Implications:**
- All consumers of `LogicalAddress` must use `Page(n)` / `Offset(n)` syntax
- Pattern matching on inner `u32` via `.0` field (public)
- Arithmetic not yet implemented; add `impl Add<u32> for Offset` when needed

---

### 11. Status and Error Type Design (Task 1c)

**Agent:** Aragorn (Rust Expert)  
**Date:** 2026-03-05  
**Status:** Implemented  
**Impact:** All operations in the system use these types

**Decision:** Flat `OperationStatus` enum (8 variants) instead of nested Status/OkKind. Manual error impls instead of thiserror. `OperationResult<T>` struct for operation returns.

**Details:**
- `OperationStatus`: Ok, Pending, NotFound, Created, InPlaceUpdated, CopyUpdated, Deleted, Aborted
- `FasterError`: ObjectNotFound, ObjectExists, ConcurrentModification, OutOfMemory, IoError, Corruption
- Manual impls: Display, Error, From<std::io::Error> (~40 lines, avoids proc-macro dependency)
- `OperationResult<T>`: struct with `status: OperationStatus` and `output: Option<T>`

**Rationale:**
1. Flat enum more ergonomic — callers match directly on `Created` instead of `Ok(OkKind::CreatedRecord)`
2. Manual error impls: Only 6 variants; keeping core dependencies minimal (crossbeam-utils, cfg-if)
3. OperationResult matches C# pattern of `(Status, Output)` pairs

**Implications:**
- All downstream modules import `OperationStatus` and `FasterError`
- OperationResult types are Send + Sync safe for concurrent use
- If error enum grows >10 variants, thiserror can be added later without breaking API

---

### 12. Hash Function and Tag Layout for Rust FASTER

**Agent:** Sam (Systems Programming Expert)  
**Date:** 2026-03-05  
**Status:** Implemented  
**Impact:** Hash index correctness, C++ behavioral compatibility

**Decision:** Use exact C++ `FasterHash` algorithm as default. Tag extraction: bits 48..61 (14 bits), matching C++ `kTagBits = 14`.

**Details:**
- `faster_hash_u64`: 4 × 16-bit chunks with polynomial accumulation (magic = 40343), finishes with `rotr64(h, 43)`
- `faster_hash_bytes`: Rolling polynomial over bytes, seeded by length, finishes with `rotr64(h, 6)`
- Tag: `((hash >> 48) & 0x3FFF) as u16` — 14-bit fingerprint
- Bucket index: `hash & (table_size - 1)` — power-of-two masking

**Rationale:**
1. Behavioral compatibility — same hash distribution as C++
2. Production-tested quality (Chi-squared validated on 1M keys)
3. No external dependency; simple arithmetic
4. Hashable trait unsealed for user customization

**Implications:**
- Hash index implementations use `KeyHash`, `tag_from_hash`, `bucket_index` directly
- Future: ahash/xxhash can be added behind feature flag
- Benchmarks registered for throughput regression monitoring

---

### 13. Workspace Directory Named `rust/` (Not `faster-rs/`)

**Agent:** Aragorn (Rust Expert)  
**Date:** 2026-03-05  
**Status:** DECIDED  
**Impact:** Repository layout convention

**Decision:** The Rust workspace lives at `rust/` in the repo root, not `faster-rs/` as originally suggested.

**Rationale:** Repository uses language-name directories: `cc/` for C++, `cs/` for C#. Following convention, Rust implementation goes in `rust/`. Explicit directive from qbradley.

**Implications:**
- All documentation references `rust/`, not `faster-rs/`
- rustfmt nightly features (imports_granularity, group_imports) require `rustup run nightly cargo fmt --check` or accept warnings on stable

---

### 14. Rust CI Architecture (Task 1e)

**Agent:** Boromir (QA Engineer)  
**Date:** 2026-03-05  
**Status:** Implemented  
**Impact:** All Rust development — CI pipeline for workspace

**Decision:** Use GitHub Actions (not Azure Pipelines) for Rust CI, with 5 separate jobs: fmt, ci (3-OS matrix), miri, bench, deny.

**Details:**
1. **fmt** — Nightly rustfmt (imports_granularity, group_imports)
2. **ci** — 3-OS matrix (ubuntu, windows, macos) with stable Rust; depends on fmt
3. **miri** — Undefined behavior detection (continue-on-error: true; weekly + manual)
4. **bench** — Criterion benchmarks (main-branch pushes only)
5. **deny** — License audit + security advisory scan

**Rationale:**
1. GitHub Actions isolated from Azure Pipelines (C#/C++ toolchain overlap is zero)
2. Nightly only for rustfmt (other jobs on stable for stability)
3. Path filtering (`rust/**`) avoids firing on C++/C# changes
4. Miri informational (expect unsafe code to appear; should inform, not block)
5. Swatinem/rust-cache keeps CI fast with per-OS cache keys

**Implications:**
- All Rust PRs gated on fmt + clippy + test (debug/release) × 3 OSes
- Bench provides regression baseline on main branch
- Miri runs weekly (Sundays 06:00 UTC) + manual dispatch
- cargo-deny continuous license/advisory monitoring

---

## Governance

- All meaningful changes require team consensus
- Document architectural decisions here
- Keep history focused on work, decisions focused on direction
- Archive decisions older than 30 days when file exceeds ~20KB
# Decision: Record Format Design Choices

**Agent:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Status:** DECIDED
**Task:** 1f — Record Format and Key/Value Traits
**Impact:** Core data representation — affects all record I/O, hybrid log, hash index

---

## 1. Safe Serialization API (Slice-Based)

**Decision:** The `Key` and `Value` traits use safe `&mut [u8]` / `&[u8]` slice-based serialization rather than unsafe `*mut u8` / `*const u8` raw pointer serialization from Section 3.2.3.

**Rationale:**
- Section 8.2 (Public API) specifies the safe slice-based interface
- Zero unsafe code in the serialization layer — all safety validated at compile time
- Can add unsafe low-level pointer-based accessors later when the hybrid log allocator needs direct memory-mapped access
- Callers don't need to worry about buffer sizing invariants beyond "buf must be ≥ serialized_size()"

**Trade-off:** Slightly less flexible for zero-copy mmap scenarios. When the allocator module needs it, we can add `unsafe serialize_to(*mut u8)` as an extension trait.

---

## 2. 8-Byte Alignment Cap

**Decision:** All record padding uses 8-byte alignment (RECORD_ALIGNMENT = 8) regardless of the key/value type's natural alignment.

**Rationale:**
- Architecture v1 caps alignment at 8 bytes (§3.2.2)
- Simplifies layout computation — no need for alignment metadata in traits
- Sufficient for all primitive types (u8..u128, i8..i128, f32, f64)
- At most 7 bytes of padding waste per field — acceptable for v1
- Can be refined later by adding alignment info to the trait system

---

## 3. `#[repr(transparent)]` for RecordInfo

**Decision:** RecordInfo is `#[repr(transparent)]` over `u64`, not `#[repr(C)]`.

**Rationale:**
- `#[repr(transparent)]` is the correct annotation for a single-field newtype
- Guarantees identical ABI to `u64` (same size, alignment, passing convention)
- `#[repr(C)]` is for multi-field structs — overkill and slightly misleading for a newtype

---

## 4. Little-Endian On-Disk Format

**Decision:** RecordInfo header and all numeric serializations use little-endian byte order via `to_le_bytes()` / `from_le_bytes()`.

**Rationale:**
- FASTER is designed for x86 (little-endian) — this matches native behavior on the primary platform
- Correct on big-endian platforms too (explicit conversion)
- On-disk format is deterministic regardless of host endianness

---

## 5. Marker Traits for Fixed-Size Types

**Decision:** `FixedSizeKey` and `FixedSizeValue` are separate marker traits with `const SIZE: usize`, rather than adding `is_fixed_size()` / `fixed_size()` methods to the base traits.

**Rationale:**
- Compile-time specialization: the hybrid log can use `<K as FixedSizeKey>::SIZE` at compile time for record sizing, not runtime checks
- More idiomatic Rust — marker traits enable conditional impl blocks and where bounds
- Avoids runtime panics from calling `fixed_size()` on variable-length types

# Decision: Tentative Bit at Bit 63 (Not Bit 61)

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-06
**Task:** 2a — Hash Bucket Entry Types
**Status:** DECIDED — follows C++ source

## Context

The architecture doc §3.1.1 shows the tentative bit at bit 61 with reserved bits at 62-63:
```
  63    62    61     60..48          47..0
│  0  │  0  │ Ten │   Tag (14)   │   Address (48)            │
```

However, the C++ source (`cc/src/index/hash_bucket.h`, `HotLogIndexBucketEntryDef`) defines the bitfield order as:
```cpp
uint64_t address    : 48;            // bits [47:0]
uint64_t tag        : 14;            // bits [61:48]
uint64_t reserved   : 1;            // bit [62]
uint64_t tentative  : 1;            // bit [63]
```

## Decision

Follow the C++ source code as the authoritative reference. Tentative is bit 63, reserved is bit 62.

## Rationale

1. The C++ implementation is the production-proven reference. Behavioral compatibility requires matching its exact bit layout.
2. The architecture doc may have simplified the diagram (swapping reserved/tentative for visual clarity). The code is the single source of truth.
3. Property tests verify round-trip correctness, and explicit bit-position tests lock down the layout.

## Impact

- All downstream code (hash bucket, hash table core, concurrent ops) must use bit 63 for tentative.
- The architecture doc §3.1.1 diagram should be updated to match C++ (low priority — code is the source of truth).
- Tag extraction in `hash.rs` and `hash_bucket.rs` use the same bit range [61:48], which is correct in both docs and code.

# Decision: HashBucket — Typed Fields vs Raw AtomicU64 Array

**Author:** Sam  
**Date:** 2026-03-06  
**Task:** 2c (Hash Bucket Structure)  
**Status:** Decided

## Context

The architecture doc §3.1.2 sketches `HashBucket` as `entries: [AtomicU64; 8]` where slots 0–6 are data entries and slot 7 is the overflow pointer. This is a flat, untyped layout.

## Decision

Use strongly-typed fields instead:

```rust
#[repr(C, align(64))]
pub struct HashBucket {
    entries: [AtomicHashBucketEntry; 7],
    overflow_address: AtomicLogicalAddress,
}
```

## Rationale

1. **Type safety:** The overflow slot stores a `LogicalAddress`, not a `HashBucketEntry`. Using `AtomicLogicalAddress` makes this explicit. No risk of accidentally treating the overflow pointer as a hash entry or vice versa.

2. **API clarity:** `bucket.overflow_address()` returns `&AtomicLogicalAddress` with its own `load`/`store`/`compare_exchange` methods. `bucket.entry(i)` returns `&AtomicHashBucketEntry`. No index confusion possible.

3. **Same layout guarantee:** Both `AtomicHashBucketEntry` and `AtomicLogicalAddress` are `#[repr(transparent)]` over `AtomicU64`. With `#[repr(C)]` on the struct, the layout is identical to `[AtomicU64; 8]`. Compile-time asserts verify `size_of == 64` and `align_of == 64`.

4. **No performance impact:** Zero-cost at runtime — same memory representation, same instructions.

## Impact

- **Aragorn (2d: Hash Table Core):** Use `bucket.entry(i)` and `bucket.overflow_address()` for the lookup/insert algorithms. The overflow pointer is an `AtomicLogicalAddress`, not a raw pointer — you'll encode overflow bucket locations as `LogicalAddress` values into the overflow allocator's address space.
- **Sam (future overflow allocator):** The overflow allocator will map `LogicalAddress` values to `&HashBucket` references. This is the only unsafe boundary in the hash index.

# Decision: Epoch System Design Choices

**Agent:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Task:** 1g — Epoch-Based Reclamation System
**Status:** Implemented

## Decision 1: Custom Epoch System (Not crossbeam-epoch)

**Choice:** Built a ~400-line custom `EpochTable` rather than using `crossbeam-epoch`.

**Rationale:**
- crossbeam-epoch provides deferred deallocation of individual objects via `Guard` + `defer()`
- FASTER needs a fundamentally different primitive: epoch-keyed drain callbacks that fire when *all threads* pass a given epoch, plus per-thread phase markers for checkpoint coordination
- The custom system is simpler (~400 lines) than an adapter wrapping crossbeam would be
- We still use `crossbeam-utils::CachePadded` for cache-line padding (utility, not runtime)

**Impact:** All modules depending on epoch protection use `EpochTable` directly.

## Decision 2: Mutex-Based DrainList

**Choice:** `Mutex<Vec<DrainAction>>` rather than a lock-free queue.

**Rationale:**
- `push` occurs only on `bump_current_epoch` (not per-operation)
- `drain_up_to` occurs only on `try_drain` (also rare)
- Critical sections are microsecond-short (Vec manipulation only)
- Actions execute *outside* the lock (collected, lock released, then executed) to prevent deadlocks when callbacks call `bump_current_epoch`
- A lock-free queue adds complexity for negligible benefit on a cold path

**Impact:** Drain list contention is not a scalability bottleneck.

## Decision 3: Memory Ordering Strategy

**Choices:**
| Operation | Ordering | Why |
|-----------|----------|-----|
| `protect → reentrant.fetch_add` | Relaxed | Single-writer: only the owning thread modifies |
| `protect → current_epoch.load` | Relaxed | Stale reads are conservative (lower epoch → lower safe → safe) |
| `protect → local_epoch.store` | Release | Pairs with Acquire in `compute_safe_epoch` |
| `unprotect → local_epoch.store(0)` | Release | Makes protected-region writes visible |
| `bump → current_epoch.fetch_add` | SeqCst | Total ordering with thread-local stores required |
| `compute_safe → current_epoch.load` | SeqCst | Consistent with bump ordering |
| `compute_safe → per-thread load` | Acquire | Sees latest Release store from protect |

**Impact:** Matches C++ FASTER ordering semantics. The SeqCst on bump is the conservative choice — could potentially be relaxed to AcqRel with careful analysis, but the bump path is not hot enough to warrant the risk.

## Decision 4: EpochGuard is !Send (Thread-Affine)

**Choice:** `PhantomData<*const ()>` makes EpochGuard `!Send` and `!Sync`.

**Rationale:**
- Matches FASTER's session model (Decision #10: thread-affine sessions)
- A guard must be dropped on the same thread that created it (it unprotects a specific thread entry)
- Sending a guard to another thread would break epoch protection invariants
- The borrow checker also prevents dropping EpochThread while a guard exists (lifetime bound)

**Impact:** Users must use guards within a single thread. EpochThread itself is Send (can be moved to a thread pool), but guards created from it are pinned to that thread.

# Decision: MallocFixedPageSize Allocator Design

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-07
**Status:** Implemented
**Impact:** All overflow bucket allocation, future hybrid log page management

## Decision

Implemented `MallocFixedPageSize<T>` as a lock-free, two-level page allocator using `LogicalAddress` for item addressing.

## Key Design Choices

### 1. Treiber Stack Free List (not per-thread deques)

**C++ uses:** Per-thread `std::deque<FreeAddress>` with epoch-gated reuse.
**Rust uses:** Global lock-free Treiber stack with 16-bit ABA tags.

**Rationale:** The C++ per-thread approach requires `Thread::id()` which maps to FASTER's custom thread registry. Since the Rust epoch system doesn't yet have thread-local free list integration, a global CAS-based free list is correct and simple. When epoch integration arrives, we can add per-thread free lists as an optimization without changing the public API.

### 2. 16-bit ABA Tags on Free List Head

The Treiber stack head uses bits 48..63 as a generation counter, preventing ABA problems during concurrent push/pop. This works because `LogicalAddress` only uses 48 bits, leaving 16 bits free.

### 3. LogicalAddress Reuse (no separate FixedPageAddress)

**C++ uses:** A separate `FixedPageAddress` type with different bit widths (20-bit offset, 28-bit page).
**Rust uses:** The existing `LogicalAddress` (25-bit offset, 23-bit page) with `ITEMS_PER_PAGE = 2^20`.

**Rationale:** Using one address type simplifies the codebase. The 25-bit offset field can represent item indices up to ~33M, easily accommodating our 1M items per page. The hash bucket overflow pointer (`AtomicLogicalAddress`) works directly with these addresses.

### 4. Flat Counter with Bit Manipulation (not encoded address increment)

The bump counter is a flat u64 index, mapped to page/offset via shift and mask. This avoids the C++ approach of incrementing an encoded address (which relies on exact bit-width overflow semantics).

### 5. `get_mut` is `unsafe`, `get` is safe

`get(&self) -> &T` is safe because the primary use case (hash bucket lookup) uses atomic interior mutability. `get_mut` is `unsafe` because it creates `&mut T` from `&self`, requiring the caller to prove exclusive access.

## Implications

- **Aragorn:** The allocator API is ready for hash index overflow bucket allocation. Use `allocate()` → `get()` for bucket lifecycle.
- **Gandalf:** Address encoding matches C++ semantics. Phase 2 hash table can use `MallocFixedPageSize<HashBucket>` directly.
- **Future epoch integration:** Add `free_at_epoch(addr, epoch)` method that defers the `free()` call until the epoch is safe to reclaim. The Treiber stack mechanism stays unchanged.
- **Galadriel:** 10 unsafe blocks, all with SAFETY comments. Key audit points: `get`/`get_mut` aliasing contract, Treiber stack CAS correctness, `Drop` implementation.
