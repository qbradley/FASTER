# Squad Decisions Archive

> Archived from decisions.md on 2026-03-09T17:02Z. Contains decisions prior to the 2026-03-07 cross-implementation performance baseline. See decisions.md for recent entries.

---


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

---

## Recent Decisions (Merged from Inbox)

# Decision: cache_store.rs Rust sample

**Author:** aragorn
**Date:** 2025-01-01
**Status:** implemented

## Context

qbradley requested a Rust equivalent of the C# `cs/samples/CacheStore` sample to demonstrate FASTER's disk-backed cache/KV usage pattern with pending I/O handling.

## Decisions

### Inline PRNG instead of `rand` dependency
The `rand` crate is not in faster-core's dependencies. Rather than adding a dev-dependency for a single example, used a 3-line xorshift64 PRNG. This keeps the dependency tree lean and the example self-contained.

### CLI args over stdin prompts
The C# sample uses `Console.ReadLine()` to select random vs. interactive mode. For Rust, command-line flags (`--evict`, `--interactive`) are more idiomatic and composable. Interactive mode still uses stdin for key input.

### Kept same numerical parameters as C# sample
1M keys, 2^19 progress interval, 100-op pending drain threshold — preserves the teaching value and allows direct comparison with the C# version.
# Decision: Mandatory `checkin` Script for All Commits

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Status:** Proposed
**Impact:** All agents — workflow change for every Rust commit

## Decision

All agents MUST use `rust/scripts/checkin` instead of raw `git commit` for any
commit touching Rust code. The script enforces the full quality gate
(fmt → clippy → doctests → tests) before allowing the commit through.

## Rationale

1. **No more "oops, forgot clippy"** — the script runs every gate automatically
2. **Consistent ordering** — gates run in a fixed sequence (fast checks first)
3. **Co-authored-by trailer** — auto-appended, no manual copy-paste needed
4. **Fail-fast** — first failure aborts; clear error messages with fix hints
5. **Works from anywhere** — auto-detects repo root, `cd`s to `rust/`

## Usage Examples

```bash
# Full quality gate + commit
rust/scripts/checkin -m "feat: add epoch framework"

# Fast iteration (fmt + clippy only, no tests)
rust/scripts/checkin --skip-tests -m "wip: refactor allocator"

# Multiple -m flags work too
rust/scripts/checkin -m "fix: address overflow" -m "Fixes edge case in page_offset calculation"
```

## Rules

- **ALWAYS** use `checkin` instead of `git commit` for Rust changes
- The script runs `git add -A` automatically — no need to stage manually
- Use `--skip-tests` only during rapid iteration; final commits must pass all gates
- If the script fails, fix the reported issue and re-run — do not bypass

## What It Enforces

| Order | Gate | Purpose |
|-------|------|---------|
| 1 | `cargo fmt --check` | Code formatting |
| 2 | `cargo clippy -D warnings` | Lint-free code |
| 3 | `cargo test --doc` | Doc examples compile and pass |
| 4 | `cargo nextest run` | All unit + integration tests pass |

## Location

`rust/scripts/checkin` (executable, works from any directory in the repo)
# Decision: Compaction Scanner Module (Wave 1, K1)

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** IMPLEMENTED
**Impact:** New internal module — no public API changes

## Decision

Add `compaction::CompactionScanner` that walks a hybrid log region and classifies each record as live, dead, or tombstoned by consulting the hash index. Returns a `CompactionPlan` containing per-record liveness info plus aggregate counts.

## Rationale

Compaction needs to know which records are still the current version before it can copy live data and reclaim space. The scanner is the read-only analysis phase that feeds into later compaction stages (copy, address-update, truncation).

## Design Choices

### Stateless function over iterator
`scan()` is a standalone function, not a streaming iterator. Compaction is a batch operation — the full plan is needed before any records are moved. This avoids lifetime complexity and makes the API trivial to use.

### Conservative liveness classification
Key invariant: **never classify a live record as dead**. False positives (dead classified as live) waste copy bandwidth but are safe. False negatives (live classified as dead) cause data loss. When the hash index has no entry for a key or chain walk fails to resolve, the scanner assumes LIVE.

### Hash index chain walk
Mirrors `store::operations::find_record_for_key()` — walks the version chain from the hash entry, skipping invalid records, until it finds the first key-matching record. If that record's address matches the scanned record's address, it's LIVE; otherwise DEAD.

### No null-record skipping
`RecordInfo(0)` (version 0, no flags, previous_address = ZERO) is indistinguishable from zeroed memory. Rather than trying to detect unwritten pages, the scanner requires callers to provide a record-aligned begin address via `FasterKv::first_data_address()`, which skips the 8-byte sentinel.

### `#[deny(unsafe_code)]` on module
The compaction module is pure safe Rust. All unsafe operations (physical address translation, page mapping) are encapsulated in `RecordAccessor` and `HybridLogAllocator`.

## Key Types

- `CompactionPlan { live, dead, tombstoned, records: Vec<LiveRecord> }`
- `LiveRecord { address, record_size, status }` where status ∈ {Live, Dead, Tombstone}
- `CompactionScanner` — namespace struct with `scan<K: Key>()` method

## Test Coverage

- 10 unit tests: empty log, single record, multiple records, updates creating dead records, tombstones, mixed updates+deletes, subranges, plan computed fields
- 1 proptest: random upsert/delete sequences → live count matches hash index entry count
- Proptest regression seed committed for shrunk failure case

## Risks

- Scanner assumes epoch protection is held by caller (documented but not enforced at type level)
- Classification accuracy depends on hash index state at scan time — concurrent mutations during scan may cause stale classifications (acceptable for compaction which runs on frozen regions)
# Final Unsafe Footprint — faster-core

**Agent:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Phase:** 5 — Module-Level Safety Gates (Final)

## Summary

After 5 phases of the unsafe audit, the `faster-core` crate has:

| Metric | Count |
|--------|-------|
| **Unsafe blocks** | 100 |
| **Unsafe fns** | 30 |
| **Unsafe impls** | 15 |
| **Total unsafe constructs** | 145 |
| **Files with unsafe** | 18 of 60 (.rs files) |
| **Files gated with `#[deny(unsafe_code)]`** | 42 modules |

## Safety-Gated Modules (compiler-enforced zero unsafe)

These modules have `#[deny(unsafe_code)]` applied. Any future `unsafe` addition triggers a compile error.

### Top-level (gated in lib.rs)

| Module | Scope |
|--------|-------|
| `address` | Logical/physical address types |
| `error` | Error types |
| `grow` | Online hash table resize (manager, splitter, state_machine) |
| `metrics` | Counters and instrumentation |
| `record` | Record layout, RecordInfo, traits |
| `status` | Operation status enums |
| `sync` | Synchronization primitives (pub(crate)) |
| `instrument` | Tracing macros (private) |

### Sub-modules (gated in parent mod.rs)

| Parent | Gated Sub-modules | Ungated (has unsafe) |
|--------|--------------------|----------------------|
| `checkpoint` | log_writer, manager, metadata, metadata_store, orchestrator, participant, session_state, snapshot_writer, state_machine (9/10) | index_writer |
| `epoch` | entry, guard, table (3/4) | drain |
| `hash` | bucket, hash, overflow (3/5) | index, table |
| `hybrid_log` | eviction, regions (2/7) | flush, log_allocator, page, record_ops, scan |
| `recovery` | log_recovery, session_recovery (2/3) | index_recovery |
| `store` | builder, session (2/6) | functions, kv, operations, pending_io |

## Per-File Unsafe Breakdown

| File | Blocks | Fns | Impls | Notes |
|------|--------|-----|-------|-------|
| `allocator.rs` | 18 | 3 | 3 | Lock-free Treiber stack, page allocation |
| `buffer_pool.rs` | 4 | 0 | 2 | Raw memory pool with Send/Sync |
| `checkpoint/index_writer.rs` | 2 | 0 | 0 | Index serialization slice |
| `device.rs` | 13 | 12 | 0 | I/O device trait + FFI callbacks |
| `epoch/drain.rs` | 8 | 2 | 2 | Drain list raw pointer callbacks |
| `hash/index.rs` | 1 | 0 | 0 | Bucket serialization slice |
| `hash/table.rs` | 2 | 0 | 0 | Unchecked indexing (hot path) |
| `hybrid_log/flush.rs` | 4 | 1 | 1 | I/O completion callback |
| `hybrid_log/log_allocator.rs` | 3 | 0 | 0 | Raw pointer arithmetic |
| `hybrid_log/page.rs` | 15 | 2 | 4 | PageFrame raw memory, Send/Sync |
| `hybrid_log/record_ops.rs` | 8 | 2 | 0 | Record accessor raw pointers |
| `hybrid_log/scan.rs` | 1 | 0 | 0 | Scan iterator slice |
| `recovery/index_recovery.rs` | 1 | 0 | 0 | Index deserialization slice |
| `store/functions.rs` | 6 | 4 | 0 | Functions trait callback dispatch |
| `store/kv.rs` | 0 | 0 | 2 | unsafe impl Send/Sync for FasterKv |
| `store/operations.rs` | 5 | 0 | 0 | MutableRecordAccessor construction |
| `store/pending_io.rs` | 3 | 1 | 0 | I/O completion callback |
| `sync_file_device.rs` | 6 | 3 | 1 | File I/O thread pool |

## Crate-Level Lint Configuration

```rust
// lib.rs
#![deny(unsafe_op_in_unsafe_fn)]        // Require explicit unsafe blocks inside unsafe fns
#![forbid(clippy::undocumented_unsafe_blocks)] // Every unsafe block must have a SAFETY comment
```

```toml
# Cargo.toml
[lints.clippy]
undocumented_unsafe_blocks = "deny"  # Also covers tests and benchmarks
```

## What Remains Unsafe and Why

All remaining unsafe falls into 5 categories:

1. **Raw pointer construction** — `RecordAccessor`, `MutableRecordAccessor`, `PageFrame` own raw pointers to caller-provided memory. The constructors are inherently unsafe.
2. **Send/Sync impls** — `FasterKv`, `PageFrame`, `BufferPool`, `DrainList` need manual Send/Sync because they contain raw pointers with lifetime contracts that the compiler can't verify.
3. **I/O callbacks** — Device completion callbacks receive `*mut u8` from the OS; reconstruction into typed pointers is inherently unsafe. `TypedIoContext<T>` centralizes this.
4. **Lock-free data structures** — The allocator's Treiber stack and epoch drain list use CAS on raw pointers for lock-free operation.
5. **Hot-path indexing** — `HashTable::bucket()` uses `get_unchecked()` where the index is guaranteed valid by construction (mask-based).

## Test Results

- **1161 tests passed, 3 skipped**
- **Clippy clean** (`-D warnings`, all targets)
- **Zero regressions** from safety gate additions
# Design Decisions — HashIndex (Work Item 2e)

## 1. Epoch Discipline: Caller-Managed Guards

**Decision:** `find`, `find_or_create`, and `update` do NOT acquire epoch guards internally.
They document that the caller must hold an `EpochGuard`.

**Rationale:** FASTER's session model acquires epoch protection once per operation batch,
not per-operation. Acquiring per-operation would be wasteful and break the reentrant
semantics — the caller's guard already covers the entire critical section.

## 2. `invalidate_entries_in_range` Does NOT Require Epoch Protection

**Decision:** This method is called from epoch drain callbacks (after safe-epoch advances),
so it runs outside epoch protection. It CAS's entries to EMPTY; failures are harmless
(another thread modified the entry concurrently).

**Rationale:** Matches C++ `InternalHashTable::InvalidateEntries()` which is invoked
from page eviction callbacks. No epoch guard needed because:
- We're not following addresses into the log — just zeroing hash index entries.
- CAS failure is benign (entry was concurrently modified, which is fine).

## 3. HashIndex Owns HashTable and EpochTable by Value

**Decision:** `HashIndex { table: HashTable, epoch: Arc<EpochTable> }`. The epoch table
is `Arc`-wrapped because `EpochTable::register()` requires `&Arc<Self>`.

**Rationale:** `register()` returns `EpochThread` which holds `Arc<EpochTable>`.
Storing the epoch as `Arc<EpochTable>` directly avoids a second Arc wrap at every
`register_thread()` call.

## 4. `cleanup_tentative_entries` Is O(n) Maintenance

**Decision:** Scan all buckets + overflow, CAS any tentative entry to EMPTY.
Not on the hot path. Called during recovery or periodic maintenance.

**Rationale:** Stale tentative entries from crashed inserts must be cleaned up.
In C++ FASTER, this is done during recovery. We expose it as a public method
for flexibility.
# Decision: Hash Table Core Design

**Agent:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Task:** 2d — Hash Table Core
**Status:** Implemented

## Decision 1: Tentative Duplicate Detection is Tag-Level Only

**Choice:** `find_or_create_entry` re-scans for *committed* duplicates after tentative CAS. It does NOT detect concurrent tentative duplicates.

**Rationale:**
- Two threads can both create tentative entries with the same tag in different bucket slots. This is correct behavior matching C++ FASTER.
- The hash table is a tag-based filter, not a unique-key store. Tag collision (same tag, different key) is expected.
- True key-level deduplication happens at the log layer: the caller reads the record at the address and compares the actual key.
- Attempting to detect tentative duplicates would require waiting for the other thread to commit or abort, breaking lock-freedom.

**Impact:** Callers (FasterKv operations) must handle the case where two entries with the same tag exist in the bucket chain. The log-level key comparison resolves this.

## Decision 2: `get_unchecked` for Bucket Lookup

**Choice:** Used `unsafe { self.buckets.get_unchecked(idx) }` in `bucket()` instead of safe indexing.

**Rationale:**
- `bucket()` is THE hot path — called on every hash table operation.
- The index is always `hash & (num_buckets - 1)` which is mathematically guaranteed to be `< num_buckets`.
- Eliminating the bounds check avoids a branch on every probe.
- Safety invariant is trivial and well-documented: `buckets.len() == num_buckets` (set in constructor), `hash.index(n)` returns `hash & (n-1)` which is always `< n` for `n > 0`.

**Impact:** This is the only `unsafe` block in the hash table module. Galadriel should verify the invariant during unsafe audit.

## Decision 3: FindOrCreateResult Returns Slot Reference

**Choice:** `find_or_create_entry` returns `FindOrCreateResult { entry, slot: &AtomicHashBucketEntry, created }` — a reference to the atomic slot, not the bucket+index.

**Rationale:**
- The caller needs the slot reference for subsequent CAS operations (commit tentative, update address, abort).
- Returning `&AtomicHashBucketEntry` directly is more ergonomic than returning `(bucket_ref, slot_index)` and forcing the caller to re-index.
- The lifetime ties the result to the hash table's lifetime, preventing use-after-free.

**Impact:** All callers interact with the slot via `table.update_entry(result.slot, old, new)`. No need to re-look up the bucket.

## Decision 4: No Separate `delete_entry` API

**Choice:** Deletion is handled via `update_entry(slot, entry, EMPTY)` rather than a dedicated `delete_entry` method.

**Rationale:**
- In C++ FASTER, deletion sets a tombstone in the *log record*, not in the hash table entry. The hash entry stays pointing to the tombstoned record.
- Entry invalidation (CAS to EMPTY) is used for aborting tentative inserts, not for key deletion.
- A dedicated delete API would be misleading — it suggests key-level deletion, which is a higher-level operation.

**Impact:** Task 2e (concurrent ops) and the FasterKv layer handle deletion semantics via record tombstones.
# Safe Alternatives & Abstraction Design for Unsafe Code

**Author:** Aragorn (Rust Expert)  
**Date:** 2025-01-27  
**Context:** FASTER Rust implementation unsafe code audit and remediation strategy

---

## Executive Summary

The FASTER Rust codebase (`rust/crates/faster-core/src/`, ~41K LOC) contains **157 total unsafe sites**:
- **115 unsafe blocks** (`unsafe { }`)
- **27 unsafe functions** (`unsafe fn`)
- **15 unsafe impls** (`unsafe impl Send/Sync`)

After systematic analysis, I've identified opportunities to **eliminate ~60% of unsafe code** (94 sites) through safe abstractions and direct replacements, with **zero performance impact**. The remaining 40% (63 sites) are genuinely necessary but can be better isolated and documented.

**Key insight:** Most unsafe usage falls into repeating patterns (pointer dereference, slice construction, atomic pointer loads) that can be encapsulated into well-audited safe abstractions.

---

## Catalog of Unsafe Usage by Category

### 1. **Memory Allocator (`allocator.rs`)** — 42 sites
**Patterns:**
- Raw pointer arithmetic for page directory traversal (15 sites)
- `alloc::alloc` / `alloc::dealloc` for page allocation (7 sites)
- `ptr::write` / `ptr::read` for free list management (10 sites)
- `Box::from_raw` / `Box::into_raw` for directory lifecycle (8 sites)
- `NonNull::new_unchecked` (2 sites)

**Why it's unsafe:** Lock-free allocator with manual memory management, pointer-tagged free lists, and growable directory.

### 2. **Page Management (`hybrid_log/page.rs`)** — 18 sites
**Patterns:**
- `slice::from_raw_parts` / `from_raw_parts_mut` for page buffer views (8 sites)
- Manual sector-aligned allocation via `alloc::alloc` (4 sites)
- `PageFrame` pointer casting and lifecycle (6 sites)

**Why it's unsafe:** Direct manipulation of sector-aligned page frames, zero-copy views into raw memory.

### 3. **Record Operations (`hybrid_log/record_ops.rs`)** — 22 sites
**Patterns:**
- `slice::from_raw_parts` for zero-copy record views (12 sites)
- Raw pointer casting to `AtomicRecordInfo` (4 sites)
- Pointer arithmetic for record layout traversal (6 sites)

**Why it's unsafe:** Zero-copy deserialization from untyped page memory.

### 4. **Hash Index (`hash/table.rs`, `hash/bucket.rs`, `hash/index.rs`)** — 8 sites
**Patterns:**
- `get_unchecked` for hot-path bucket access (3 sites — **critical for performance**)
- `slice::from_raw_parts` for bucket array views (3 sites)
- Packed 64-bit entry bit manipulation (2 sites)

**Why it's unsafe:** Hot-path optimization — bounds checks are provably redundant but compiler can't eliminate them.

### 5. **Device I/O (`device.rs`, `sync_file_device.rs`)** — 25 sites
**Patterns:**
- Raw callback function pointers (`unsafe fn` × 12)
- `slice::from_raw_parts_mut` for I/O buffer construction (8 sites)
- Context pointer casting in callbacks (5 sites)

**Why it's unsafe:** C-style completion callback ABI, raw buffer pointers for async I/O.

### 6. **Epoch System (`epoch/drain.rs`, `epoch/table.rs`)** — 12 sites
**Patterns:**
- Treiber stack node manipulation with `Box::from_raw` (6 sites)
- `AtomicPtr` loads with raw pointer dereference (4 sites)
- Manual node linking (2 sites)

**Why it's unsafe:** Lock-free epoch-based garbage collection with manual node lifecycle.

### 7. **Send/Sync Impls** — 15 sites
**Locations:**
- `allocator.rs`: `MallocFixedPageSize<T>`, `FreeListPush`
- `hybrid_log/page.rs`: `PageFrame`, `PageTable`
- `store/kv.rs`: `FasterKv<F>`
- `buffer_pool.rs`: `AlignedBuffer`
- `epoch/drain.rs`: `DrainList`
- Others: `FlushCallbackContext`, `IoRequest`

**Why it's unsafe:** Manual Send/Sync for types containing raw pointers that are actually safe to send/share (ownership/invariant-based reasoning).

---

## Safe Abstraction Design

### Wave 1: Direct Replacements (Easy Wins) — 35 sites, 0 PRs needed

These can be replaced immediately with safe equivalents:

#### 1.1 **`slice::from_raw_parts` → Safe Slice Construction**

**Pattern:**
```rust
// CURRENT (unsafe)
let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
```

**Replacement:** When the underlying type has a safe accessor (e.g., `PageFrame`, `AlignedBuffer`), expose `as_slice()` methods directly:

```rust
// NEW (safe)
impl PageFrame {
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: Already encapsulated in the type
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.size) }
    }
}
// Caller code becomes safe:
let slice = page_frame.as_slice();
```

**Impact:** Eliminates **18 sites** in:
- `hybrid_log/record_ops.rs` (12 sites)
- `hybrid_log/page.rs` (4 sites)
- `buffer_pool.rs` (2 sites — already done correctly!)

**Performance:** Zero-cost — same assembly, safety moved to type boundary.

---

#### 1.2 **`NonNull::new_unchecked` → `NonNull::new().expect()`**

**Pattern:**
```rust
// CURRENT (unsafe)
let old_nn = unsafe { NonNull::new_unchecked(old_dir) };
```

**Replacement:**
```rust
// NEW (safe)
let old_nn = NonNull::new(old_dir).expect("directory pointer must be non-null");
```

**Rationale:** These pointers are never null by construction (just came from a valid Box or AtomicPtr load). The `expect()` compiles to a no-op in release builds (optimized away), adding only a debug-mode assertion.

**Impact:** Eliminates **2 sites** in `allocator.rs`.

**Performance:** Zero-cost (optimized away in release).

---

#### 1.3 **`get_unchecked` with Provable Bounds → Keep Unsafe BUT Document**

**Current code (`hash/table.rs:248`):**
```rust
#[inline(always)]
pub fn bucket(&self, hash: KeyHash) -> &HashBucket {
    let idx = hash.index(self.num_buckets) as usize;
    unsafe { self.buckets.get_unchecked(idx) }
}
```

**Analysis:** 
- `hash.index(n)` returns `hash & (n - 1)`, which is **always < n** for power-of-two `n`.
- The bounds check is provably redundant.
- Replacing with `self.buckets[idx]` adds ~1 cycle per lookup (measured in C++ version).

**Decision:** **KEEP UNSAFE** — this is the hot path (every hash table operation). The existing SAFETY comment is excellent.

**Action:** Add `#[cfg_attr(debug_assertions, track_caller)]` for better panic messages in debug mode.

**Impact:** 0 sites eliminated (3 sites remain unsafe but well-documented).

---

#### 1.4 **`AtomicPtr::load` → Encapsulate in Safe Methods**

**Pattern:**
```rust
// CURRENT (unsafe)
let dir = unsafe { &*self.dir.load(Ordering::Acquire) };
```

**Replacement:** Encapsulate the invariant ("dir pointer is always valid") into a method:

```rust
impl<T> MallocFixedPageSize<T> {
    fn current_dir(&self) -> &PageDir<T> {
        // SAFETY: self.dir is always a valid pointer to a PageDir that was
        // created by new() or expand_directory(). Load with Acquire to
        // synchronize with Release store in expand_directory.
        unsafe { &*self.dir.load(Ordering::Acquire) }
    }
}

// Caller code becomes safe:
let dir = self.current_dir();
```

**Impact:** Eliminates **15 sites** across `allocator.rs`, `epoch/drain.rs`.

**Performance:** Zero-cost (inlined).

---

### Wave 2: Safe Abstraction Types (Medium Effort) — 47 sites

These require designing new types to encapsulate invariants:

#### 2.1 **`RecordView` / `RecordViewMut` — Zero-Copy Record Access**

**Problem:** `hybrid_log/record_ops.rs` has 22 unsafe sites doing pointer casts and slice construction.

**Current code:**
```rust
pub struct RecordAccessor {
    ptr: *const u8,
    record_size: u32,
}

impl RecordAccessor {
    pub unsafe fn new(ptr: *const u8, record_size: u32) -> Self { ... }
    
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.record_size as usize) }
    }
}
```

**Safe design:**
```rust
// Lifetime-bound view into page memory
pub struct RecordView<'page> {
    slice: &'page [u8],  // Safe slice reference
}

impl<'page> RecordView<'page> {
    // Safe constructor — caller proves page memory is valid for 'page lifetime
    pub fn new(page_slice: &'page [u8], offset: usize, record_size: usize) -> Option<Self> {
        let end = offset.checked_add(record_size)?;
        if end > page_slice.len() {
            return None;
        }
        Some(Self {
            slice: &page_slice[offset..end],
        })
    }
    
    pub fn as_slice(&self) -> &[u8] {
        self.slice  // Safe!
    }
    
    pub fn record_info(&self) -> RecordInfo {
        layout_read_record_info(self.slice)
    }
}

// Mutable variant
pub struct RecordViewMut<'page> {
    slice: &'page mut [u8],
}
```

**Usage:**
```rust
// OLD (unsafe):
let accessor = unsafe { RecordAccessor::new(ptr, size) };
let info = accessor.record_info();

// NEW (safe):
let page_slice = page_frame.as_slice();  // Safe method from Wave 1
let view = RecordView::new(page_slice, offset, size)?;
let info = view.record_info();  // Safe!
```

**Invariant:** The lifetime `'page` proves that the page frame is pinned (by epoch guard) for the view's entire lifetime. No raw pointers exposed.

**Impact:** Eliminates **22 sites** in `hybrid_log/record_ops.rs`.

**Performance:** Zero-cost — slice references compile to identical code as raw pointers. Bounds checks are needed regardless (record size is dynamic).

---

#### 2.2 **`PageBuffer` — Safe Page Frame Wrapper**

**Problem:** `hybrid_log/page.rs` manually allocates sector-aligned buffers with raw `alloc::alloc`.

**Current code:**
```rust
impl PageFrame {
    pub fn allocate(size: usize, sector_size: usize) -> Self {
        let layout = Layout::from_size_align(size, sector_size).unwrap();
        let data = unsafe { alloc::alloc_zeroed(layout) };
        // ... manual null check, NonNull wrapping ...
    }
}
```

**Safe design:** Use `AlignedBuffer` (already exists in `buffer_pool.rs`!):

```rust
pub struct PageFrame {
    buffer: AlignedBuffer,  // Encapsulates the unsafe allocation
    state: AtomicPageState,
}

impl PageFrame {
    pub fn allocate(size: usize, sector_size: usize) -> Self {
        Self {
            buffer: AlignedBuffer::new(size, sector_size),  // Safe!
            state: AtomicPageState::new(PageState::Free),
        }
    }
    
    pub fn as_slice(&self) -> &[u8] {
        self.buffer.as_slice()  // Safe!
    }
    
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buffer.as_mut_slice()  // Safe!
    }
}
```

**Note:** `AlignedBuffer` already does this correctly! Just need to refactor `PageFrame` to use it.

**Impact:** Eliminates **12 sites** in `hybrid_log/page.rs`, `hybrid_log/log_allocator.rs`.

**Performance:** Zero-cost — same allocation mechanism, just better encapsulated.

---

#### 2.3 **`PageDirectory<T>` — Safe Directory Access**

**Problem:** `allocator.rs` does manual pointer arithmetic through a growable array of `AtomicPtr<Page<T>>`.

**Current design issues:**
- Raw `Box::into_raw` / `Box::from_raw` for directory lifecycle (8 sites)
- Raw pointer dereference for page access (10 sites)

**Safe refactor:**
```rust
pub struct PageDirectory<T> {
    // Owned allocation — no raw pointers exposed
    pages: Box<[AtomicPtr<T>]>,
}

impl<T> PageDirectory<T> {
    pub fn new(capacity: usize) -> Self {
        let pages = (0..capacity)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { pages }
    }
    
    pub fn get_page(&self, index: usize) -> *mut T {
        // SAFETY: Atomic load is always safe. The returned pointer validity
        // is the caller's responsibility (documented in method contract).
        self.pages[index].load(Ordering::Acquire)
    }
    
    pub fn capacity(&self) -> usize {
        self.pages.len()
    }
    
    // No manual Drop impl needed — Box<[AtomicPtr]> is safe to drop
}
```

**Remaining unsafe:** The `get_page()` caller must still dereference the returned pointer, but at least the directory structure itself is safe.

**Impact:** Eliminates **13 sites** in `allocator.rs` (directory management).

**Performance:** Zero-cost — `Box<[T]>` has identical layout to raw pointer + length.

---

#### 2.4 **`EpochDrainList` — Safe Callback Queue**

**Problem:** `epoch/drain.rs` implements a Treiber stack with manual `Box::from_raw` node lifecycle (6 sites).

**Safe refactor:** Use `crossbeam-epoch`'s `Owned` / `Shared` pointer types OR std's `Arc`:

```rust
struct DrainNode {
    epoch: u64,
    action: Option<Box<dyn FnOnce() + Send>>,
    next: Option<Arc<DrainNode>>,  // Safe reference counting
}

pub struct DrainList {
    head: AtomicOption<Arc<DrainNode>>,  // Lock-free ArcSwap pattern
}
```

**Trade-off:** Adds atomic refcount overhead on push/pop. For the epoch drain list (infrequent operation — only on epoch bump), this is acceptable.

**Alternative (zero-cost):** Keep current implementation but encapsulate the unsafe into private methods with clear contracts:

```rust
impl DrainList {
    unsafe fn push_raw(&self, node: Box<DrainNode>) { ... }
    unsafe fn take_all(&self) -> Option<Box<DrainNode>> { ... }
    
    // Safe public API:
    pub fn push(&self, epoch: u64, action: impl FnOnce() + Send + 'static) {
        let node = Box::new(DrainNode { epoch, action: Some(Box::new(action)), next: ptr::null_mut() });
        unsafe { self.push_raw(node) }
    }
}
```

**Impact:** Eliminates **6 sites** (public API becomes safe, unsafe isolated to 2 private methods).

**Performance:** Zero-cost if we choose the "encapsulate" approach over Arc.

---

### Wave 3: Device I/O — Necessary Unsafe (12 sites remain)

**Location:** `device.rs`, `sync_file_device.rs` — 25 sites total.

**Analysis:** The completion callback ABI is inherently unsafe:
```rust
pub type IoCompletionCallback = unsafe fn(context: *mut u8, status: IoStatus, bytes_transferred: u32);
```

**Why it can't be made safe:**
1. **FFI boundary** — this matches platform I/O APIs (io_uring, IOCP, libaio).
2. **Type erasure** — context is a raw `*mut u8` because different callbacks need different context types.
3. **Lifetime escape** — the callback outlives the function call (async).

**Mitigation strategy:**
1. ✅ Keep the raw callback API for `Device` trait (genuinely necessary).
2. ✅ Provide a **safe wrapper layer** for common cases:

```rust
// Safe wrapper for callbacks with known context types
pub struct TypedIoContext<T> {
    inner: Box<T>,
}

impl<T> TypedIoContext<T> {
    pub fn new(data: T) -> Self {
        Self { inner: Box::new(data) }
    }
    
    pub fn into_raw(self) -> *mut u8 {
        Box::into_raw(self.inner) as *mut u8
    }
    
    pub unsafe fn from_raw(ptr: *mut u8) -> Self {
        Self { inner: Box::from_raw(ptr as *mut T) }
    }
}

// Safe callback wrapper
pub fn make_typed_callback<T, F>(handler: F) -> (IoCompletionCallback, TypedIoContext<T>)
where
    F: FnOnce(&mut T, IoStatus, u32) + Send + 'static,
    T: Send + 'static,
{
    unsafe fn callback_wrapper<T, F>(ctx: *mut u8, status: IoStatus, bytes: u32) {
        let typed_ctx = TypedIoContext::<T>::from_raw(ctx);
        // ... invoke handler with &mut T ...
    }
    // ... return wrapper and context ...
}
```

**Impact:** 
- 12 sites remain unsafe (Device trait + callback wrappers).
- **13 sites** can use safe wrappers at call sites.

**Performance:** Zero-cost — wrapper compiles away.

---

### Wave 4: Send/Sync Impls — Keep But Justify (15 sites)

**Current impls:**
```rust
unsafe impl<T: Send> Send for MallocFixedPageSize<T> {}
unsafe impl<T: Send> Sync for MallocFixedPageSize<T> {}
```

**Analysis:** These are **correct** — the types contain raw pointers but maintain ownership invariants that make them thread-safe.

**Action:** Keep all 15 impls, but **enhance documentation**:

```rust
// SAFETY: `MallocFixedPageSize<T>` is `Send` because:
// 1. The raw pointer `dir: AtomicPtr<PageDir<T>>` is accessed only via atomic
//    loads/stores with appropriate orderings (Acquire/Release).
// 2. Page allocations are protected by the grow_lock mutex.
// 3. All page memory is owned exclusively by this allocator (no aliasing).
// 4. T: Send is required — items may be moved between threads.
unsafe impl<T: Send> Send for MallocFixedPageSize<T> {}

// SAFETY: `MallocFixedPageSize<T>` is `Sync` because:
// 1. All methods that access pages go through atomic operations.
// 2. The free list uses a lock-free Treiber stack with ABA protection.
// 3. Concurrent allocate/free operations are safe by design.
unsafe impl<T: Send> Sync for MallocFixedPageSize<T> {}
```

**Impact:** 0 sites eliminated, but **15 sites** get comprehensive SAFETY comments.

**Performance:** N/A (compile-time only).

---

## Performance Impact Assessment

| Wave | Sites Eliminated | Performance Impact | Risk Level |
|------|------------------|-------------------|-----------|
| **Wave 1** | 35 | **Zero** (same codegen) | Low |
| **Wave 2** | 47 | **Zero** (abstractions inline) | Medium |
| **Wave 3** | 13 (wrapper adoption) | **Zero** (wrapper overhead is compile-time) | Low |
| **Wave 4** | 0 (documentation only) | N/A | N/A |
| **Total** | **95 sites safer** | **Zero regression** | — |

**Measurement strategy:**
1. Benchmark before/after on `faster-bench` suite.
2. Compare assembly for hot paths (hash lookup, record access).
3. Use `cargo-asm` to verify inlining.

---

## Prioritized Action Plan

### Phase 1: Quick Wins (1 week, 0 performance risk)

**PR 1: Safe Slice Constructors**
- Add `as_slice()` / `as_mut_slice()` methods to `PageFrame`, `RecordAccessor`, etc.
- Replace 18 call sites.
- **Test:** Existing unit tests should pass unchanged.

**PR 2: AtomicPtr Encapsulation**
- Add `current_dir()` method to `MallocFixedPageSize`.
- Replace 15 call sites.
- **Test:** Allocator stress test under ThreadSanitizer.

**PR 3: NonNull Safe Construction**
- Replace `new_unchecked` with `new().expect()`.
- 2 sites in `allocator.rs`.
- **Test:** Existing tests + debug assertions.

**Estimated reduction:** **35 sites** (22% of total unsafe).

---

### Phase 2: Abstraction Types (2-3 weeks, requires design review)

**PR 4: RecordView Safe API**
- Implement `RecordView` / `RecordViewMut` with lifetime-bound slices.
- Refactor `RecordAccessor` / `MutableRecordAccessor` to use them internally.
- Update all hybrid log code.
- **Test:** Record serialization round-trip tests, scan iterator tests.
- **Benchmark:** Measure `faster-bench ycsb-read` before/after.

**PR 5: PageFrame Uses AlignedBuffer**
- Refactor `PageFrame` to wrap `AlignedBuffer` instead of raw `alloc::alloc`.
- Update page allocation/deallocation.
- **Test:** Page lifecycle tests, eviction tests.

**PR 6: PageDirectory Safe Refactor**
- Replace raw `Box::into_raw` with safe `Box<[AtomicPtr]>`.
- Encapsulate directory growth logic.
- **Test:** Allocator growth test (force multiple directory expansions).

**PR 7: DrainList Encapsulation**
- Isolate unsafe node manipulation into 2 private methods.
- Make public API entirely safe.
- **Test:** Epoch drain test (queue 1000 callbacks, verify all execute).

**Estimated reduction:** **47 sites** (30% of total unsafe).

---

### Phase 3: Device I/O Safe Wrappers (1 week)

**PR 8: TypedIoContext Wrapper**
- Implement safe callback wrapper for common cases.
- Refactor `FlushCallbackContext` to use it.
- **Test:** Flush test with async writes.

**Estimated reduction:** **13 sites** (8% of total unsafe).

---

### Phase 4: Documentation Sweep (3 days)

**PR 9: Enhanced SAFETY Comments**
- Audit all remaining 63 unsafe sites.
- Ensure every site has:
  - `// SAFETY:` comment explaining why it's safe.
  - Invariants documented in type-level docs.
- Add `#![deny(unsafe_op_in_unsafe_fn)]` to enforce explicit unsafe blocks.

**Estimated sites documented:** **63 sites** (40% of total unsafe, necessary but audited).

---

### Phase 5: Module-Level Safety Boundaries (Stretch Goal)

**PR 10: `#[forbid(unsafe_code)]` for Safe Modules**

Modules that can become 100% safe after above PRs:
- ✅ `hash/index.rs` (3 sites → 0 after Wave 1)
- ✅ `checkpoint/index_writer.rs` (2 sites → 0 after Wave 2)
- ✅ `store/operations.rs` (0 sites already)
- ✅ `metrics.rs` (0 sites already)

Add to Cargo.toml:
```toml
[lints.rust]
unsafe_code = "warn"  # Require justification for new unsafe

[lints.clippy]
undocumented_unsafe_blocks = "deny"  # Enforce SAFETY comments
```

---

## Dependency Evaluation

**Question:** Should we adopt `zerocopy` or `bytemuck` for safe transmutation?

| Crate | Use Case | Pro | Con | Verdict |
|-------|----------|-----|-----|---------|
| `zerocopy` | Safe byte ↔ struct casts | Zero-cost, well-audited | Adds dependency | **Maybe** — only if we have many transmute sites (currently 0) |
| `bytemuck` | Pod type casting | Lighter than zerocopy | Less feature-complete | **Maybe** — same as above |
| `crossbeam-epoch` | Epoch-based GC | Battle-tested, safe API | Heavier than our custom impl | **No** — our custom impl is simpler and faster |

**Current verdict:** **No external dependencies needed**. We have 0 transmute sites, and our custom epoch impl is already optimized. Re-evaluate if we add serialization features later.

---

## Success Metrics

**Before (baseline):**
- 157 total unsafe sites
- 115 unsafe blocks in hot paths
- No module-level safety guarantees

**After (target):**
- ≤63 unsafe sites (60% reduction)
- All hot-path unsafe well-documented
- 4+ modules with `#[forbid(unsafe_code)]`
- Zero performance regression (<1% variance on benchmarks)

**Tracking:**
```bash
# Measure progress
grep -r "unsafe {" crates/faster-core/src/ --include="*.rs" | wc -l

# Verify no regression
cargo bench --bench faster-bench -- --baseline before
```

---

## Risks & Mitigations

### Risk 1: Performance Regression in Hot Path
**Hot paths:**
- `hash/table.rs::bucket()` — `get_unchecked` (keep unsafe)
- `record_ops.rs` — zero-copy record access (verify with `cargo-asm`)

**Mitigation:**
- Benchmark before/after on every PR.
- Use `#[inline(always)]` on safe wrappers.
- Accept PR only if perf is within 1% (noise margin).

### Risk 2: Lifetime Complexity in RecordView
**Issue:** Lifetime-bound views can complicate APIs (e.g., returning `RecordView<'_>` from iterators).

**Mitigation:**
- Start with simple cases (direct record access).
- Use `RecordAccessor` (unsafe) for complex lifetime scenarios.
- Gradual migration — don't force refactors that harm ergonomics.

### Risk 3: Maintenance Burden of Abstractions
**Issue:** More types = more code to maintain.

**Mitigation:**
- Only add abstractions that eliminate ≥5 unsafe sites.
- Comprehensive tests for each new type.
- Document intended usage in module-level docs.

---

## Related Work & References

**Inspiration from other Rust projects:**
1. **Tokio** — `UnsafeCell` wrappers for lock-free data structures.
2. **Crossbeam** — Epoch-based GC with safe API over unsafe internals.
3. **Bytes** — Safe zero-copy buffer management (similar to our `AlignedBuffer`).

**Papers:**
- "Safe Systems Programming in Rust" (Jung et al., 2020) — formal verification of unsafe abstractions.
- "Ownership Types for Safe Programming" (Clarke et al., 1998) — foundational work.

---

## Appendix: Full Unsafe Site List (By File)

<details>
<summary>Click to expand (157 sites)</summary>

### allocator.rs (42 sites)
- Line 196: `Self::dealloc_page(new_page)` — page cleanup on allocation failure
- Line 214: `alloc::alloc_zeroed(layout)` — page allocation
- Line 235: `alloc::dealloc(page, layout)` — page deallocation
- Line 356: `push_free_list_raw(...)` — Treiber stack push
- Line 374: `&*free_list` — atomic pointer load
- Line 381-389: `ptr::write(item_ptr, ...)` — free list node linking
- Line 431: `(*dir).get_or_add_page(0)` — directory initialization
- Line 518: `&*page_ptr.add(item_idx)` — page indexing
- Line 537-541: `unsafe fn get_mut(...)` — mutable page access
- Line 630: `&*self.dir.load(...)` — directory pointer load
- Line 666: `&*self.dir.load(...)` — directory pointer load (bump path)
- Line 694: `&*self.dir.load(...)` — directory pointer load (expand check)
- Line 708: `NonNull::new_unchecked(old_dir)` — **[Wave 1 target]**
- Line 715: `&*new_dir` — new directory access
- Line 736: `page_ptr.add(item_idx)` — page offset calculation
- Line 772: `ptr::read(head_ptr)` — free list node read
- Line 813: `&*dir` — directory access in Drop
- Line 818: `PageDir::dealloc_page(page)` — page cleanup in Drop
- Line 822: `Box::from_raw(dir)` — directory deallocation
- Line 834: `Box::from_raw(old_dir.as_ptr())` — retired directory cleanup
- Line 942: `get_mut(addr).value = 42` — test code
- Line 979: `get_mut(addr).value = 99` — test code
- **Wave 1:** 2 sites (NonNull)
- **Wave 2:** 15 sites (directory encapsulation), 5 sites (page access)

### hybrid_log/page.rs (18 sites)
- Line 167: `unsafe impl Send for PageFrame`
- Line 170: `unsafe impl Sync for PageFrame`
- Line 235: `from_raw_parts(self.data.as_ptr(), ...)` — **[Wave 1 target]**
- Line 249: `from_raw_parts_mut(self.data.as_ptr(), ...)` — **[Wave 1 target]**
- Line 259: `unsafe fn zero(&self)` — page zeroing
- Line 336: `unsafe impl Send for PageTable`
- Line 338: `unsafe impl Sync for PageTable`
- Allocation/deallocation sites (8 sites) — **[Wave 2 target: use AlignedBuffer]**

### hybrid_log/record_ops.rs (22 sites)
- Line 59: `unsafe fn new(ptr, record_size)` — RecordAccessor constructor
- Line 88: `&*(self.ptr as *const AtomicRecordInfo)` — atomic info cast
- Line 126: `from_raw_parts(self.ptr, ...)` — **[Wave 2 target]**
- Line 167: `unsafe fn new(ptr, record_size)` — MutableRecordAccessor constructor
- Line 194: `&*(self.ptr as *const AtomicRecordInfo)` — atomic info cast
- Line 213-214: `from_raw_parts(self.ptr, ...)` — **[Wave 2 target]**
- Line 224: `from_raw_parts(self.ptr, ...)` — **[Wave 2 target]**
- Line 241: `from_raw_parts(self.ptr, ...)` — **[Wave 2 target]**
- Line 309: `from_raw_parts_mut(self.ptr, ...)` — **[Wave 2 target]**
- Line 437: `from_raw_parts(ptr, RECORD_HEADER_SIZE)` — **[Wave 2 target]**
- Line 478: `from_raw_parts(ptr, RECORD_HEADER_SIZE)` — **[Wave 2 target]**
- ... (more slice construction sites)

### hash/table.rs, hash/index.rs, hash/bucket.rs (8 sites)
- table.rs:215: `get_unchecked(index)` — **[Keep: hot path]**
- table.rs:248: `get_unchecked(idx)` — **[Keep: hot path]**
- index.rs:715: `from_raw_parts(first, len)` — **[Wave 1 target]**
- checkpoint/index_writer.rs:153, 690: bucket byte casts — **[Wave 1 target]**

### device.rs, sync_file_device.rs (25 sites)
- device.rs:31: `unsafe fn(context, status, bytes)` — IoCompletionCallback type
- device.rs:93: `unsafe fn read_async(...)` — Device trait
- device.rs:110: `unsafe fn write_async(...)` — Device trait
- ... (12 callback function declarations)
- ... (8 buffer slice constructions) — **[Wave 3 target: safe wrappers]**
- ... (5 context pointer casts) — **[Wave 3 target: TypedIoContext]**

### epoch/drain.rs (12 sites)
- Line 61: `unsafe impl Send for DrainList`
- Line 67: `unsafe impl Sync for DrainList`
- Line 100-120: Treiber stack push/pop (6 sites) — **[Wave 2 target: encapsulate]**
- Box::from_raw / into_raw for node lifecycle (4 sites) — **[Wave 2 target]**

### store/kv.rs, buffer_pool.rs, etc. (15 Send/Sync impls)
- **[Wave 4 target: enhance SAFETY comments]**

**Total: 157 sites**
</details>

---

## Conclusion

This audit demonstrates that the FASTER Rust codebase can achieve **60% unsafe reduction** (157 → 63 sites) through systematic abstraction design, with **zero performance cost**. The remaining 40% of unsafe code is genuinely necessary (allocator internals, I/O callbacks, hot-path optimizations) but will be well-documented and isolated.

**Next steps:** Begin Phase 1 (Quick Wins) to build confidence with low-risk refactors, then proceed to abstraction design in Phase 2.

**Sign-off:** Aragorn, 2025-01-27

# Decision: UnsafeContext Epoch-Amortized Batch API

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-07-17
**Status:** IMPLEMENTED
**Impact:** Public API addition — affects all crate consumers

## Decision

Add `UnsafeContext<'a, F>` as an epoch-amortized batch API. Users create it via `FasterKv::unsafe_context(&mut session)`, perform multiple operations without per-op epoch enter/exit, and call `refresh()` periodically.

## Rationale

Per-operation epoch protect/unprotect/try_drain costs ~124ns — 43% of a 285ns upsert and >80% of a read or RMW. The `UnsafeContext` pattern (matching C# FASTER's `UnsafeContext`) holds epoch protection across batches, eliminating this overhead.

## Benchmark Evidence

| Operation | Per-op | Batch | Speedup |
|-----------|--------|-------|---------|
| Upsert | 3.56 Mops/s | 10.07 Mops/s | 2.83× |
| Read | 4.85 Mops/s | 47.33 Mops/s | 9.76× |
| Mixed 50/50 | 3.37 Mops/s | 13.55 Mops/s | 4.03× |
| RMW | 4.83 Mops/s | 47.45 Mops/s | 9.83× |

## API Shape

```rust
let mut ctx = store.unsafe_context(&mut session);
for i in 0..batch_size {
    ctx.upsert(&store, &key, &value, context);
    if i % 64 == 0 { ctx.refresh(); }
}
drop(ctx); // releases epoch protection
```

## Implications

- **All agents:** The per-operation API remains unchanged; this is additive.
- **FFI (Sam):** Will need a C FFI wrapper for `unsafe_context` / batch ops.
- **Async (Elrond):** Async adapter should support batch mode via UnsafeContext.
- **Testing (Éowyn):** Batch API needs fuzz testing for refresh interval edge cases.
- **Safety (Galadriel):** No new `unsafe` code — all safety is through Rust's type system.

## Key Design Choices

1. **Methods on `UnsafeContext` take `&FasterKv<F>`** — avoids storing a store reference in the context, keeping lifetimes simple.
2. **`FasterKv` fields `hash_index`, `allocator`, `functions` changed to `pub(crate)`** — needed for session.rs batch methods.
3. **`dispatch_pending_io` changed to `pub(crate)`** — same reason.
4. **Refresh calls try_drain() explicitly** — allows epoch advancement without leaving protection.
# Decision: Quality Gate and Documentation Conventions

**Agent:** Arwen (Developer Advocate)
**Date:** 2026-03-06
**Status:** Implemented
**Trigger:** Retrospective action items AI-2, AI-3, AI-6

## What Changed

Created `rust/CONTRIBUTING.md` capturing three new conventions:

1. **Quality Gate** — Four mandatory steps before every commit:
   `cargo nextest run`, `cargo test --doc`, `cargo clippy -D warnings`, `cargo fmt --check`.
   The explicit `cargo test --doc` step closes the gap that let broken doctests slip through.

2. **Doc Example Rules** — Public API types only, copy-pasteable by external users,
   no internal test types (`TestFunctions`). `SimpleFunctions::<K, V>` is the canonical
   example type.

3. **Documentation Timing** — Inline doc-comments ship with code; user-facing docs
   (QUICKSTART, guides) are batched into a trailing documentation phase after features
   stabilize.

Also updated `rust/README.md` to point contributors to the quality gate.

## Why

Doctests broke silently because the CI and contributor workflow didn't include
`cargo test --doc`. Internal test types leaked into doc examples, making them
unusable for external developers. User-facing docs were being written too early,
leading to a rewrite cycle.

## Files

- `rust/CONTRIBUTING.md` (new)
- `rust/README.md` (minor addition)
# Decision: Foundation Test Architecture

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-06
**Status:** DOCUMENTED

## Context

Item 1i requires integration tests, stress tests, property tests, and benchmarks that validate Phase 1 modules work together correctly.

## Decisions

1. **Integration test file structure:** Three files instead of the plan's two (`foundation_smoke.rs` + `foundation_props.rs`). Split into `integration_basics.rs` (cross-module correctness), `concurrent_stress.rs` (multi-threaded), and `property_tests.rs` (proptest). This gives clearer failure isolation and allows running categories independently.

2. **`#[ignore]` tags on heavy stress tests:** Tests with 16 threads × 100K+ cycles are tagged `#[ignore]` for CI nightly runs only. Default `cargo test` runs 4-thread and 8-thread variants (~10K cycles each) which complete in <1s.

3. **Property test case counts:** 256-1024 cases per property (exceeds the spec's 256 minimum). Higher counts for simpler properties (address/hash), lower for allocation-heavy tests.

4. **Benchmark organization:** New benchmarks added to existing `core_benchmarks.rs` (keeps one benchmark binary per crate). `faster-bench/benches/main.rs` gets end-to-end pipeline benchmarks since it depends on `faster-core`.

5. **No Miri for stress tests:** The concurrent stress tests use OS threads and standard atomics, which Miri supports poorly. These tests are validated via ThreadSanitizer in CI instead. Property tests and integration basics are Miri-compatible.

## Impact

- All agents: The test suite is the quality gate for Phase 2. Any Phase 2 PR must pass `cargo test --workspace`.
- Legolas: Benchmark baselines established. Future performance changes should be compared against these.
- Aragorn: Integration tests document the intended API usage patterns. Refactoring must not break these contracts.
# Decision: No thread::sleep in Test Code

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-06
**Task:** AI-7 — Audit thread::sleep in Tests
**Status:** Convention confirmed

## Context

The retrospective noted that `write_pending_completion` tests originally took 18.5s due to sleep-based synchronization and were sped up 20× to 0.9s. Convention established: no `thread::sleep` in tests unless explicitly justified.

## Audit Results

### Test & Benchmark Code: ✅ Clean
Zero `thread::sleep` occurrences in any test or benchmark file under `crates/faster-core/`.

### Production Code: 2 Justified Sites

| File | Line | Duration | Context | Verdict |
|------|------|----------|---------|---------|
| `src/checkpoint/orchestrator.rs` | 282 | 1ms | Poll loop waiting for flush I/O completion, bounded by `max_wait_flush` deadline | **Justified** — I/O polling with timeout |
| `src/store/kv.rs` | 263 | 1ms | Poll loop in `Drop` impl waiting for in-flight I/O before device close | **Justified** — cleanup path, no signal available |

## Convention

**For test code:** Use `Barrier`, `Condvar`, `mpsc::channel`, or `notify`-style primitives instead of `thread::sleep`. Sleep-based synchronization in tests is prohibited unless a comment documents the specific justification.

**For production code:** Short (≤1ms) bounded poll loops are acceptable when the underlying API provides no callback or notification mechanism. Always pair with a deadline/timeout to prevent unbounded waits.
### 2026-03-06T17:05:25Z: User directive
**By:** qbradley (via Copilot)
**What:** Always use claude-opus-4.6 (or the strongest Claude model) when spawning sub-agents. Never use sonnet or haiku. We are not budget restricted.
**Why:** User request — captured for team memory
### 2026-03-06T19:26:51Z: User directive
**By:** qbradley (via Copilot)
**What:** All agents must use the `checkin` script instead of `git commit` for all commits. The script enforces quality gates (fmt, clippy, tests, doctests) before allowing a commit.
**Why:** User request — prevent broken commits from ever landing. Captured for team memory.
### 2026-03-06T20:13:32Z: User directive — Defer Wave 2 (Read Cache)
**By:** qbradley (via Copilot)
**What:** Skip Wave 2 (Read Cache: RC1–RC4) in Iteration 4. FASTER is inherently a read cache; need to understand the value prop before building a separate read cache layer.
**Why:** User decision — defer to understand value before investing effort
# Decision: Callback→Future Bridge Pattern

**Agent:** Elrond (Tokio/Async Expert)
**Date:** 2026-03-06
**Status:** IMPLEMENTED
**Impact:** Foundation for all async FASTER operations

## Decision

Implement the callback→Future bridge using `Arc<Mutex<SharedState<T>>>` shared between a `PendingFuture<T>` and a `CompletionSender<T>`, using only `std` types.

## Rationale

1. **Runtime-agnostic:** Uses only `std::future::Future` and `std::task::Waker` — no Tokio, no channels, no runtime-specific types. Any executor can drive these futures.
2. **Minimal allocation:** Single `Arc<Mutex<SharedState>>` per pending operation. No boxing of futures, no channel overhead.
3. **Cancel-safe by construction:** If the future is dropped, `CompletionSender::complete()` silently discards the result. No panics, no leaks.
4. **`MaybePending<T>` for zero-cost sync path:** When FASTER operations complete synchronously (hot path — in-memory reads), `MaybePending::Ready(T)` avoids the `Arc<Mutex>` allocation entirely.

## Alternatives Considered

- **tokio::sync::oneshot:** Adds Tokio dependency to the bridge; violates runtime-agnostic requirement.
- **futures::channel::oneshot:** External dependency for a 30-line abstraction; unnecessary.
- **Custom lock-free channel:** Over-engineering for a single-producer-single-consumer one-shot pattern.
- **Mutex-free approach (AtomicPtr + UnsafeCell):** Possible but `Mutex` contention is zero (only two participants, at-most-once wake). Not worth the unsafe.

## Implications

- T2 (AsyncSession) wraps `Session` operations: sync result → `MaybePending::Ready`, pending → `MaybePending::Pending` with sender given to callback.
- T3/T4 (TokioFileDevice) will need `tokio` as a real dependency, but the bridge layer remains runtime-agnostic.
- Future performance optimization: if profiling shows `Mutex` overhead matters, can swap to lock-free with same public API.

## Files

- `rust/crates/faster-tokio/src/bridge.rs` — Implementation + 9 unit tests + 4 doc-tests
- `rust/crates/faster-tokio/src/lib.rs` — Module declaration
- `rust/crates/faster-tokio/Cargo.toml` — tokio in dev-dependencies only
# Comprehensive Unsafe Audit — FASTER Rust Core
**Auditor:** Galadriel (Security Expert)  
**Date:** 2026-03-05  
**Scope:** `rust/crates/faster-core/src/` — all unsafe usage  
**Context:** Pre-implementation security audit per Gandalf architecture (Phase 1 preparation)

---

## Executive Summary

**Total unsafe surface:** 90 sites (71 blocks, 10 functions, 9 impls)  
**SAFETY comments:** 46 present (~51% coverage)  
**Missing comments:** ~44 sites (49% — critical gap)

**Category breakdown:**
- **Category A (Eliminable):** 3 sites (3%)
- **Category B (Abstractable):** 22 sites (24%)
- **Category C (Necessary):** 65 sites (72%)

**Top 3 highest-risk modules:**
1. **allocator.rs** — 28 unsafe sites, lock-free memory management
2. **hybrid_log/page.rs** — 21 unsafe sites, raw page frame manipulation
3. **hybrid_log/record_ops.rs** — 16 unsafe sites, pointer-based record access

**Critical findings:**
- 49% of unsafe code lacks `// SAFETY:` justifications (SF-7 violation)
- Lock-free free list (Treiber stack) has subtle ABA risks if epoch integration breaks
- Raw pointer arithmetic in record accessors bypasses Rust's borrow checker
- FFI callback invocations assume context pointer validity with no runtime validation

---

## Detailed Findings by File

### 1. `allocator.rs` (28 unsafe sites)

#### L196: `unsafe { Self::dealloc_page(new_page) }`
- **What:** Deallocates page after losing CAS race in `add_page`
- **Category:** C (necessary)
- **Why:** Page was allocated with custom layout, requires unsafe dealloc
- **SAFETY comment:** ✅ Present (lines 194-195)
- **Verdict:** Correct. Guarantees page was freshly allocated and unshared.

#### L214: `unsafe { alloc::alloc_zeroed(layout) }`
- **What:** Allocates zeroed cache-line-aligned page
- **Category:** C (necessary)
- **Why:** Custom alignment/layout requires unsafe allocator API
- **SAFETY comment:** ✅ Present (line 213)
- **Verdict:** Correct. Non-zero size checked, layout validated.

#### L235: `unsafe { alloc::dealloc(page as *mut u8, layout) }`
- **What:** Deallocates page in `dealloc_page`
- **Category:** C (necessary)
- **Why:** Paired unsafe dealloc for unsafe alloc
- **SAFETY comment:** ✅ Present (line 234)
- **Verdict:** Correct. Caller contract enforces matching layout.

#### L317-323: `unsafe impl Send/Sync for MallocFixedPageSize<T>`
- **What:** Manual Send/Sync impls for lock-free allocator
- **Category:** B (abstractable — could use `#[derive]` if refactored)
- **Why:** Raw pointers break auto-derive; atomics/Mutex provide safety
- **SAFETY comment:** ✅ Present (lines 312-322, comprehensive)
- **Verdict:** Correct. Extensive justification covers all shared state.
- **Refactor opportunity:** Extract `PageDir` into `Arc` wrapper, eliminate manual impls.

#### L348: `unsafe impl Send for FreeListPush`
- **What:** Marks deferred free-list callback as Send
- **Category:** C (necessary)
- **Why:** Raw pointers for epoch-deferred ops; Drop ensures validity
- **SAFETY comment:** ✅ Present (lines 343-347)
- **Verdict:** Correct. Drop contract enforces pointer validity lifetime.

#### L356: `unsafe { push_free_list_raw(...) }`
- **What:** Executes deferred free-list push from epoch callback
- **Category:** C (necessary)
- **Why:** Callback runs on different thread, needs raw pointer access
- **SAFETY comment:** ✅ Present (lines 353-355)
- **Verdict:** Correct. Relies on Drop flushing epoch drains before dealloc.

#### L372-390: `unsafe fn push_free_list_raw`
- **What:** Low-level Treiber stack push using raw pointers
- **Category:** C (necessary)
- **Why:** Epoch callbacks don't have allocator `&self`
- **SAFETY comment:** ✅ Present (function header + line 373-383)
- **Verdict:** Correct. Contract documented; caller enforces alignment/validity.
- **Risk:** ABA potential if 16-bit tag overflows. Epoch deferral mitigates this.

#### L431: `unsafe { (*dir).get_or_add_page(0) }`
- **What:** Pre-allocates page 0 during allocator init
- **Category:** A (eliminable)
- **Why:** `dir` was just created; could use `&mut` or safe accessor
- **SAFETY comment:** ✅ Present (line 430)
- **Verdict:** Eliminable. Rewrite: `let dir = Box::new(...); dir.get_or_add_page(0); Box::into_raw(dir)`
- **Fix:** Store `dir: &mut PageDir<T>` temporarily, eliminate raw pointer dereference.

#### L518: `unsafe { &*page_ptr.add(item_idx) }`
- **What:** Converts raw pointer to shared reference in `get`
- **Category:** C (necessary)
- **Why:** Two-level indirection requires pointer arithmetic
- **SAFETY comment:** ✅ Present (lines 513-518)
- **Verdict:** Correct. Bounds/alignment checked, lifetime tied to `&self`.

#### L537-541: `pub unsafe fn get_mut` + `unsafe { &mut *page_ptr.add(item_idx) }`
- **What:** Returns mutable reference to allocated item
- **Category:** C (necessary)
- **Why:** Exclusive access requires unsafe; aliasing contract enforced externally
- **SAFETY comment:** ✅ Present (lines 525-530, 539-540)
- **Verdict:** Correct. Caller contract is explicit. Session ownership enforces exclusivity.

#### L552: `unsafe { page_ptr.add(item_idx) }`
- **What:** Returns raw pointer in `get_ptr`
- **Category:** C (necessary)
- **Why:** Pointer arithmetic within page allocation
- **SAFETY comment:** ✅ Present (line 551)
- **Verdict:** Correct. Arithmetic within valid allocation bounds.

#### L630: `unsafe { &*self.dir.load(Ordering::Acquire) }`
- **What:** Dereferences atomic directory pointer in `resolve`
- **Category:** C (necessary)
- **Why:** Atomic load returns raw pointer; needs dereference
- **SAFETY comment:** ✅ Present (lines 627-629)
- **Verdict:** Correct. Acquire synchronizes with Release in `expand_directory`.

#### L666: `unsafe { &*self.dir.load(Ordering::Acquire) }`
- **What:** Loads directory in `bump_allocate`
- **Category:** C (necessary)
- **Why:** Same as L630
- **SAFETY comment:** ✅ Present (line 665)
- **Verdict:** Correct.

#### L694: `unsafe { &*self.dir.load(Ordering::Acquire) }`
- **What:** Re-checks directory under grow lock in `expand_directory`
- **Category:** C (necessary)
- **Why:** Same as L630
- **SAFETY comment:** ✅ Present (line 693)
- **Verdict:** Correct.

#### L708: `unsafe { NonNull::new_unchecked(old_dir) }`
- **What:** Wraps old directory pointer for retirement
- **Category:** B (abstractable)
- **Why:** Pointer known non-null from swap, but unchecked variant skips check
- **SAFETY comment:** ✅ Present (line 707)
- **Verdict:** Correct but eliminable. Use `NonNull::new(old_dir).unwrap()` for debug validation.
- **Refactor:** Replace with checked `NonNull::new` + `expect` for self-documenting panic.

#### L715: `unsafe { &*new_dir }`
- **What:** Returns reference to newly installed directory
- **Category:** C (necessary)
- **Why:** `new_dir` is `*mut PageDir<T>` from `Box::into_raw`
- **SAFETY comment:** ✅ Present (line 714)
- **Verdict:** Correct. Fresh allocation, still valid.

#### L736-738: `unsafe { ptr::write(item_ptr as *mut u64, old_addr_raw) }`
- **What:** Writes next-pointer into freed item (Treiber stack linkage)
- **Category:** C (necessary)
- **Why:** Free list embeds pointer in recycled memory
- **SAFETY comment:** ✅ Present (lines 734-735)
- **Verdict:** Correct. Caller contract ensures item is 8-byte aligned, ≥8 bytes, exclusive.

#### L772: `unsafe { ptr::read(head_ptr as *const u64) }`
- **What:** Reads next-pointer from free-list head item
- **Category:** C (necessary)
- **Why:** Treiber stack pop requires reading linkage
- **SAFETY comment:** ✅ Present (lines 768-772)
- **Verdict:** Correct. CAS protects against use-after-free; stale read is benign (CAS fails).

#### L813: `unsafe { &*dir }`
- **What:** Dereferences current directory in Drop
- **Category:** C (necessary)
- **Why:** `dir` is raw pointer; need reference to iterate pages
- **SAFETY comment:** ✅ Present (line 812)
- **Verdict:** Correct. `&mut self` in Drop ensures exclusive access.

#### L818: `unsafe { PageDir::<T>::dealloc_page(page) }`
- **What:** Frees each non-null page during Drop
- **Category:** C (necessary)
- **Why:** Paired dealloc for `alloc_page`
- **SAFETY comment:** ✅ Present (line 817)
- **Verdict:** Correct.

#### L822: `drop(unsafe { Box::from_raw(dir) })`
- **What:** Reclaims directory box in Drop
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw` from init
- **SAFETY comment:** ✅ Present (line 821)
- **Verdict:** Correct.

#### L834: `drop(unsafe { Box::from_raw(old_dir.as_ptr()) })`
- **What:** Frees retired directories in Drop
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw` from `expand_directory`
- **SAFETY comment:** ✅ Present (lines 832-833)
- **Verdict:** Correct.

#### L942: `unsafe { alloc.get_mut(addr).value = 42 }`
- **What:** Test uses `get_mut` to write to allocated item
- **Category:** C (necessary — test code)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ✅ Present (line 941)
- **Verdict:** Test code; correct usage.

#### L979: `unsafe { alloc.get_mut(addr).value = 99 }`
- **What:** Test writes to allocated item
- **Category:** C (necessary — test code)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ✅ Present (line 978)
- **Verdict:** Test code; correct usage.

---

### 2. `device.rs` (12 unsafe sites)

#### L31: `pub type IoCompletionCallback = unsafe fn(...)`
- **What:** FFI-style callback signature
- **Category:** C (necessary)
- **Why:** Raw pointers for context; inherently unsafe
- **SAFETY comment:** ✅ Present (lines 25-29)
- **Verdict:** Correct. Trait contract documents caller obligations.

#### L93-100: `unsafe fn read_async`
- **What:** Trait method for async read
- **Category:** C (necessary)
- **Why:** Raw pointer to dest buffer, FFI-style context
- **SAFETY comment:** ✅ Present (lines 86-92)
- **Verdict:** Correct. Contract requires buffer validity until callback.

#### L110-117: `unsafe fn write_async`
- **What:** Trait method for async write
- **Category:** C (necessary)
- **Why:** Raw pointer to source buffer, FFI-style context
- **SAFETY comment:** ✅ Present (lines 103-109)
- **Verdict:** Correct. Same contract as read_async.

#### L177-193: `unsafe fn read_async` (NullDevice impl)
- **What:** Zeroes dest buffer, invokes callback
- **Category:** C (necessary)
- **Why:** Implements unsafe trait method
- **SAFETY comment:** ✅ Present (lines 185, 189)
- **Verdict:** Correct. Upholds trait contract.

#### L186-188: `unsafe { core::ptr::write_bytes(dest, 0, len as usize) }`
- **What:** Fills dest with zeros
- **Category:** C (necessary)
- **Why:** Raw pointer write; trait guarantees dest validity
- **SAFETY comment:** ✅ Present (line 185)
- **Verdict:** Correct.

#### L190-192: `unsafe { callback(context, IoStatus::Success, len) }`
- **What:** Invokes completion callback
- **Category:** C (necessary)
- **Why:** Callback signature is unsafe fn
- **SAFETY comment:** ✅ Present (line 189)
- **Verdict:** Correct. Trait contract guarantees context validity.

#### L196-212: `unsafe fn write_async` (NullDevice impl)
- **What:** Updates high-water mark, invokes callback
- **Category:** C (necessary)
- **Why:** Implements unsafe trait method
- **SAFETY comment:** ✅ Present (line 208)
- **Verdict:** Correct.

#### L209-211: `unsafe { callback(context, IoStatus::Success, len) }`
- **What:** Invokes completion callback
- **Category:** C (necessary)
- **Why:** Same as L190-192
- **SAFETY comment:** ✅ Present (line 208)
- **Verdict:** Correct.

#### L301-332: `unsafe fn read_async` (InMemoryDevice impl)
- **What:** Copies from internal Vec to dest buffer
- **Category:** C (necessary)
- **Why:** Implements unsafe trait method
- **SAFETY comment:** ✅ Present (lines 313, 328)
- **Verdict:** Correct.

#### L314-325: `unsafe { copy_nonoverlapping / write_bytes }`
- **What:** Copies data or zero-fills on short read
- **Category:** C (necessary)
- **Why:** Raw pointer write to dest buffer
- **SAFETY comment:** ✅ Present (line 313)
- **Verdict:** Correct. Trait contract guarantees dest validity.

#### L329-331: `unsafe { callback(context, IoStatus::Success, len) }`
- **What:** Invokes completion callback
- **Category:** C (necessary)
- **Why:** Same as above
- **SAFETY comment:** ✅ Present (line 328)
- **Verdict:** Correct.

#### L335-359: `unsafe fn write_async` (InMemoryDevice impl)
- **What:** Copies from source buffer to internal Vec
- **Category:** C (necessary)
- **Why:** Implements unsafe trait method
- **SAFETY comment:** ✅ Present (lines 349, 355)
- **Verdict:** Correct.

#### L350-352: `unsafe { copy_nonoverlapping }`
- **What:** Copies source to Vec
- **Category:** C (necessary)
- **Why:** Raw pointer read from source buffer
- **SAFETY comment:** ✅ Present (line 349)
- **Verdict:** Correct.

#### L356-358: `unsafe { callback(context, IoStatus::Success, len) }`
- **What:** Invokes completion callback
- **Category:** C (necessary)
- **Why:** Same as above
- **SAFETY comment:** ✅ Present (line 355)
- **Verdict:** Correct.

#### L442: `unsafe fn on_complete` (test)
- **What:** Test callback signature
- **Category:** C (necessary — test)
- **Why:** Matches `IoCompletionCallback` signature
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; benign but should add comment.

#### L455: `unsafe { dev.read_async(...) }`
- **What:** Test invokes read_async
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ✅ Present (lines 452-453)
- **Verdict:** Test code; correct.

#### L465: `unsafe fn on_complete` (test)
- **What:** Test callback signature
- **Category:** C (necessary — test)
- **Why:** Same as L442
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; benign but should add comment.

#### L477: `unsafe { dev.write_async(...) }`
- **What:** Test invokes write_async
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ✅ Present (line 475)
- **Verdict:** Test code; correct.

#### L550: `unsafe fn on_read` (test)
- **What:** Test callback signature
- **Category:** C (necessary — test)
- **Why:** Same as L442
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; benign but should add comment.

#### L555: `unsafe fn on_write` (test)
- **What:** Test callback signature
- **Category:** C (necessary — test)
- **Why:** Same as L442
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; benign but should add comment.

#### L563-578: `unsafe { dev.write_async / dev.read_async }` (tests)
- **What:** Test invocations
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ✅ Present (line 562, 569)
- **Verdict:** Test code; correct.

---

### 3. `sync_file_device.rs` (10 unsafe sites)

#### L213: `unsafe impl Send for IoRequest`
- **What:** Marks I/O request as Send for thread pool
- **Category:** C (necessary)
- **Why:** Raw pointers; Device trait contract guarantees validity
- **SAFETY comment:** ✅ Present (lines 209-212)
- **Verdict:** Correct. Contract defers to Device trait's guarantee.

#### L243: `unsafe { std::slice::from_raw_parts_mut(request.buffer.add(buf_pos), chunk) }`
- **What:** Creates mutable slice for read destination
- **Category:** C (necessary)
- **Why:** Raw pointer from Device trait contract
- **SAFETY comment:** ✅ Present (lines 239-241)
- **Verdict:** Correct. Bounds checked (`buf_pos + chunk <= request.len`).

#### L250-252: `unsafe { std::slice::from_raw_parts(request.buffer.cast_const().add(buf_pos), chunk) }`
- **What:** Creates const slice for write source
- **Category:** C (necessary)
- **Why:** Raw pointer from Device trait contract
- **SAFETY comment:** ✅ Present (lines 247-249)
- **Verdict:** Correct. Same bounds guarantee as read.

#### L297-299: `unsafe { (request.callback)(request.context, status, bytes) }`
- **What:** Invokes completion callback
- **Category:** C (necessary)
- **Why:** FFI-style callback with raw context pointer
- **SAFETY comment:** ✅ Present (lines 295-296)
- **Verdict:** Correct. Device trait contract guarantees context validity.

#### L415-444: `unsafe fn read_async` (SyncFileDevice impl)
- **What:** Queues read request to thread pool
- **Category:** C (necessary)
- **Why:** Implements Device trait's unsafe method
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing contract documentation. Should state: "Upholds Device trait contract; passes pointers to thread pool worker."

#### L445-473: `unsafe fn write_async` (SyncFileDevice impl)
- **What:** Queues write request to thread pool
- **Category:** C (necessary)
- **Why:** Implements Device trait's unsafe method
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing contract documentation. Same as read_async.

#### L660: `unsafe fn test_callback`
- **What:** Test callback signature
- **Category:** C (necessary — test)
- **Why:** Matches IoCompletionCallback signature
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should add comment.

#### L662: `unsafe { &*context.cast::<CallbackState>() }`
- **What:** Casts context pointer to test state struct
- **Category:** C (necessary — test)
- **Why:** Test callback needs to access its state
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should document: "context was allocated as Box<CallbackState> and remains valid."

#### L713-731: `unsafe { dev.read_async / dev.write_async }` (tests)
- **What:** Test invocations
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ❌ Missing (both calls)
- **Verdict:** Test code; should add: "buffers valid for duration of test; context valid until callback."

---

### 4. `hybrid_log/record_ops.rs` (16 unsafe sites)

#### L59-70: `pub unsafe fn new` (RecordAccessor)
- **What:** Constructor for immutable record accessor
- **Category:** B (abstractable)
- **Why:** Lifetime/validity contract could be enforced by safe builder
- **SAFETY comment:** ✅ Present (lines 49-57)
- **Verdict:** Abstractable. Create safe `from_log_read(allocator, addr)` wrapper.
- **Refactor:** New module `record_accessor` with safe entry points; keep raw constructor internal.

#### L88: `unsafe { &*(self.ptr as *const AtomicRecordInfo) }`
- **What:** Reinterprets record header as AtomicRecordInfo
- **Category:** C (necessary)
- **Why:** CAS on record header requires atomic view
- **SAFETY comment:** ✅ Present (lines 85-87)
- **Verdict:** Correct. Alignment/size/transparency guarantees upheld.

#### L126: `unsafe { core::slice::from_raw_parts(self.ptr, self.record_size as usize) }`
- **What:** Creates immutable slice from record pointer
- **Category:** C (necessary)
- **Why:** Accessor owns raw pointer; needs slice conversion
- **SAFETY comment:** ✅ Present (line 125)
- **Verdict:** Correct. Constructor enforces validity.

#### L167-178: `pub unsafe fn new` (MutableRecordAccessor)
- **What:** Constructor for mutable record accessor
- **Category:** B (abstractable)
- **Why:** Same as RecordAccessor::new
- **SAFETY comment:** ✅ Present (lines 159-165)
- **Verdict:** Abstractable. Same refactor as RecordAccessor.

#### L194: `unsafe { &*(self.ptr as *const AtomicRecordInfo) }`
- **What:** Atomic view of mutable record header
- **Category:** C (necessary)
- **Why:** Same as L88
- **SAFETY comment:** ✅ Present (lines 192-193)
- **Verdict:** Correct.

#### L213-215: `unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize) }`
- **What:** Immutable slice from mutable accessor
- **Category:** C (necessary)
- **Why:** Key read-only access
- **SAFETY comment:** ✅ Present (line 212)
- **Verdict:** Correct.

#### L223-225: `unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize) }`
- **What:** Immutable slice for value read
- **Category:** C (necessary)
- **Why:** Same as L213-215
- **SAFETY comment:** ✅ Present (line 222)
- **Verdict:** Correct.

#### L234: `unsafe { self.ptr.add(layout.value_offset()) }`
- **What:** Pointer to value region
- **Category:** C (necessary)
- **Why:** Pointer arithmetic for value location
- **SAFETY comment:** ✅ Present (lines 231-233)
- **Verdict:** Correct. Offset < record_size guaranteed by layout.

#### L241: `unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize) }`
- **What:** Full record as immutable slice
- **Category:** C (necessary)
- **Why:** Debug/serialization needs
- **SAFETY comment:** ✅ Present (line 240)
- **Verdict:** Correct.

#### L309: `unsafe { core::slice::from_raw_parts_mut(self.ptr, self.record_size as usize) }`
- **What:** Mutable slice from mutable accessor
- **Category:** C (necessary)
- **Why:** Write access to full record
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Constructor ensures exclusive access and valid memory."

#### L368: `unsafe { MutableRecordAccessor::new(ptr, size) }`
- **What:** Creates mutable accessor in allocator
- **Category:** C (necessary)
- **Why:** Freshly allocated record; caller has exclusive access
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "ptr freshly allocated by bump allocator; size valid; no aliases."

#### L420: `Some(unsafe { RecordAccessor::new(ptr as *const u8, record_size) })`
- **What:** Creates immutable accessor from page read
- **Category:** C (necessary)
- **Why:** Page read guarantees valid record
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Page pinned in memory; record_size validated from header."

#### L437: `unsafe { core::slice::from_raw_parts(ptr as *const u8, RECORD_HEADER_SIZE) }`
- **What:** Reads record header for size calculation
- **Category:** C (necessary)
- **Why:** Need header to determine full size
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Page pinned; ptr aligned; RECORD_HEADER_SIZE bytes available."

#### L478: `unsafe { core::slice::from_raw_parts(ptr as *const u8, RECORD_HEADER_SIZE) }`
- **What:** Reads header in version chain iterator
- **Category:** C (necessary)
- **Why:** Same as L437
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Same justification as L437.

#### L486: `unsafe { RecordAccessor::new(ptr as *const u8, record_size) }`
- **What:** Creates accessor in version chain iterator
- **Category:** C (necessary)
- **Why:** Walking version chain requires record access
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Version chain pointer validated as non-invalid; page pinned."

#### L675: `unsafe { MutableRecordAccessor::new(ptr, layout.total_size() as u32) }`
- **What:** Test creates mutable accessor
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should add comment.

---

### 5. `hybrid_log/page.rs` (21 unsafe sites)

#### L167-168: `unsafe impl Send for PageFrame` / `unsafe impl Sync for PageFrame`
- **What:** Marks page frame as thread-safe
- **Category:** C (necessary)
- **Why:** Raw pointer to page data; atomics provide synchronization
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document: "Raw pointer exclusively owned; all shared access synchronized via atomic refcount."

#### L194: `let ptr = unsafe { std::alloc::alloc_zeroed(layout) }`
- **What:** Allocates page frame memory
- **Category:** C (necessary)
- **Why:** Custom layout for large page (32MB default)
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Layout has non-zero size (checked), valid alignment."

#### L235: `unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.size) }`
- **What:** Immutable slice from page frame
- **Category:** C (necessary)
- **Why:** Read access to page data
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "data allocated for self.size bytes; pointer valid; shared access safe (read-only)."

#### L246-249: `pub unsafe fn as_mut_slice`
- **What:** Mutable slice from page frame
- **Category:** B (abstractable)
- **Why:** Could be safe method with `&mut self` receiver
- **SAFETY comment:** ❌ Missing
- **Verdict:** Abstractable. Change signature to `pub fn as_mut_slice(&mut self) -> &mut [u8]`.
- **Refactor:** Remove unsafe; rely on exclusive `&mut` borrow.

#### L259-267: `pub unsafe fn zero`
- **What:** Zeroes page contents
- **Category:** B (abstractable)
- **Why:** Could be safe with `&mut self`
- **SAFETY comment:** ❌ Missing
- **Verdict:** Abstractable. Change to `pub fn zero(&mut self)`.
- **Refactor:** Remove unsafe; `&mut` guarantees exclusive access.

#### L262-265: `unsafe { core::ptr::write_bytes / as_mut_slice }`
- **What:** Zero-fills page data
- **Category:** C (necessary — internal to unsafe fn)
- **Why:** Zeroing via raw pointer or slice
- **SAFETY comment:** ❌ Missing
- **Verdict:** If fn becomes safe (via `&mut self`), these become safe too.

#### L289-292: `unsafe { core::ptr::write_bytes / as_mut_slice }`
- **What:** Zeroes page in `zero_fill_until`
- **Category:** C (necessary)
- **Why:** Zeroing partial page
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "offset < self.size checked; exclusive access assumed (caller contract)."

#### L336-338: `unsafe impl Send/Sync for PageTable`
- **What:** Marks page table as thread-safe
- **Category:** C (necessary)
- **Why:** Contains AtomicPtr to page frames; lock-free page cache
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document: "All frames accessed via AtomicPtr CAS; refcount protects lifetime."

#### L397: `Some(unsafe { &*ptr })`
- **What:** Dereferences page frame pointer
- **Category:** C (necessary)
- **Why:** Atomic load returns raw pointer
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "ptr validated non-null; refcount ensures frame stays live."

#### L417: `let frame = unsafe { &*existing }`
- **What:** Dereferences existing frame in `get_or_add`
- **Category:** C (necessary)
- **Why:** CAS winner; need reference
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "CAS guarantees existing is valid frame; refcount prevents dealloc."

#### L426: `unsafe { frame.zero() }`
- **What:** Zeroes newly added page
- **Category:** B (becomes safe if `zero` is fixed)
- **Why:** Depends on L259 refactor
- **SAFETY comment:** ❌ Missing
- **Verdict:** Will become safe after L259 refactor.

#### L448: `unsafe { &*raw }`
- **What:** Dereferences new frame after successful CAS
- **Category:** C (necessary)
- **Why:** Fresh frame from CAS win
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "raw freshly allocated, CAS published it; refcount active."

#### L454: `let _ = unsafe { Box::from_raw(raw) }`
- **What:** Frees losing allocation after CAS loss
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw`
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "raw freshly allocated, unpublished; safe to reclaim."

#### L457: `unsafe { &*winner }`
- **What:** Dereferences winner frame after CAS loss
- **Category:** C (necessary)
- **Why:** CAS loser uses winner's frame
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "winner validated by CAS; refcount ensures lifetime."

#### L478: `let frame = unsafe { &*ptr }`
- **What:** Dereferences frame in `try_evict`
- **Category:** C (necessary)
- **Why:** Load from atomic to check refcount
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "ptr loaded with Acquire; refcount check ensures frame valid during deref."

#### L492: `let _ = unsafe { Box::from_raw(ptr) }`
- **What:** Reclaims evicted frame
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw` after refcount reached zero
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "refcount==1 confirmed; CAS swapped to null; exclusive ownership reclaimed."

#### L529: `let _ = unsafe { Box::from_raw(ptr) }`
- **What:** Reclaims frame during Drop
- **Category:** C (necessary)
- **Why:** Cleanup all frames
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "&mut self in Drop; all frames still valid; reclaim via Box::from_raw."

#### L570: `let slice = unsafe { frame.as_mut_slice() }`
- **What:** Test uses mutable slice
- **Category:** C (necessary — test, but becomes safe after refactor)
- **Why:** Testing page write
- **SAFETY comment:** ❌ Missing
- **Verdict:** Will become safe after L246 refactor.

#### L589: `let slice = unsafe { frame.as_mut_slice() }`
- **What:** Test uses mutable slice
- **Category:** C (necessary — test, but becomes safe after refactor)
- **Why:** Same as L570
- **SAFETY comment:** ❌ Missing
- **Verdict:** Same as L570.

#### L594: `unsafe { frame.zero() }`
- **What:** Test zeros frame
- **Category:** C (necessary — test, but becomes safe after refactor)
- **Why:** Same as L426
- **SAFETY comment:** ❌ Missing
- **Verdict:** Same as L426.

---

### 6. `hybrid_log/scan.rs` (1 unsafe site)

#### L243: `let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, record_size) }`
- **What:** Creates slice from record pointer in scan iterator
- **Category:** C (necessary)
- **Why:** Scanning records requires pointer-to-slice conversion
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Page pinned; ptr validated; record_size from header."

---

### 7. `hybrid_log/log_allocator.rs` (2 unsafe sites)

#### L181: `Some(unsafe { frame.as_mut_ptr().add(offset) })`
- **What:** Returns pointer to allocated record location
- **Category:** C (necessary)
- **Why:** Bump allocator returns raw pointer for record write
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "frame pinned; offset < frame.size checked; exclusive access via session ownership."

#### L447: `let buf = unsafe { core::slice::from_raw_parts_mut(frame.as_mut_ptr(), frame.size()) }`
- **What:** Creates mutable slice for page flush
- **Category:** C (necessary)
- **Why:** Flush needs to read page contents
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "frame frozen for flush; exclusive access during flush op."

#### L641: `unsafe { frame.zero_fill_until(valid) }`
- **What:** Zeroes unused region before flush
- **Category:** C (necessary)
- **Why:** Partial page flush needs clean tail
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "frame exclusive during flush; valid < size guaranteed."

---

### 8. `hybrid_log/flush.rs` (6 unsafe sites)

#### L126: `unsafe impl Send for FlushCallbackContext`
- **What:** Marks flush context as Send for async I/O
- **Category:** C (necessary)
- **Why:** Raw pointer to page table; device callback crosses threads
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document: "page_table Arc outlives callback; device guarantees callback invocation."

#### L141: `let ctx = unsafe { Box::from_raw(context as *mut FlushCallbackContext) }`
- **What:** Reclaims context in completion callback
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw` from flush submission
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "context allocated as Box<FlushCallbackContext>; callback invoked exactly once."

#### L145: `let page_table = unsafe { &*ctx.page_table }`
- **What:** Dereferences page table pointer in callback
- **Category:** C (necessary)
- **Why:** Need page table to unpin frame
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Arc in ctx keeps page_table alive; pointer valid."

#### L239-269: `let result = unsafe { device.write_async(...) }` + inner unsafe blocks
- **What:** Issues async write with context pointer
- **Category:** C (necessary)
- **Why:** Device trait requires unsafe call
- **SAFETY comment:** ✅ Present (line 239, 258, 269)
- **Verdict:** Correct. Context lifetime managed via Box.

#### L315: `let source = unsafe { std::slice::from_raw_parts(frame.as_ptr(), write_size as usize) }`
- **What:** Creates slice for sync write
- **Category:** C (necessary)
- **Why:** Sync write needs source buffer
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "frame pinned for flush; write_size <= frame.size() validated."

#### L509: `unsafe { device.write_async(...) }`
- **What:** Test invokes write_async
- **Category:** C (necessary — test)
- **Why:** Testing flush logic
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should add comment.

---

### 9. `checkpoint/index_writer.rs` (2 unsafe sites)

#### L153: `unsafe { &*(bucket as *const HashBucket as *const [u8; BUCKET_SIZE]) }`
- **What:** Reinterprets bucket as byte array for serialization
- **Category:** A (eliminable)
- **Why:** Can use `std::slice::from_ref` or `bytemuck::bytes_of`
- **SAFETY comment:** ❌ Missing
- **Verdict:** Eliminable. Use `bytemuck::bytes_of(bucket)` (zero-cost, safe).
- **Refactor:** Add `bytemuck` dep, replace with `bytes_of`.

#### L690: `unsafe { &*(bucket as *const HashBucket as *const [u8; BUCKET_SIZE]) }`
- **What:** Same as L153
- **Category:** A (eliminable)
- **Why:** Same as L153
- **SAFETY comment:** ❌ Missing
- **Verdict:** Eliminable. Same refactor as L153.

---

### 10. `recovery/index_recovery.rs` (1 unsafe site)

#### L280: `unsafe { std::slice::from_raw_parts_mut(bucket_ptr as *mut u8, BUCKET_SIZE) }`
- **What:** Creates mutable slice for bucket deserialization
- **Category:** C (necessary)
- **Why:** Reading bucket data from checkpoint
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "bucket_ptr from hash table; aligned; BUCKET_SIZE == size_of::<HashBucket>()."

---

### 11. `hash/index.rs` (1 unsafe site)

#### L715: `unsafe { std::slice::from_raw_parts(first, len) }`
- **What:** Creates slice of all hash buckets for checkpoint
- **Category:** C (necessary)
- **Why:** Checkpoint serialization needs bucket array view
- **SAFETY comment:** ✅ Present (lines 710-714)
- **Verdict:** Correct. Layout guarantees stride correctness.

---

### 12. `hash/table.rs` (2 unsafe sites)

#### L215: `unsafe { self.buckets.get_unchecked(index as usize) }`
- **What:** Unchecked bucket access (debug path)
- **Category:** B (abstractable)
- **Why:** Bounds check just performed; `get_unchecked` eliminates redundant check
- **SAFETY comment:** ✅ Present (lines 213-214)
- **Verdict:** Correct but abstractable. Consider: always use checked access (negligible cost), or wrap in `debug_assert!`.
- **Refactor:** Replace with `&self.buckets[index as usize]` (debug builds check twice; release optimizes to same code).

#### L248: `unsafe { self.buckets.get_unchecked(idx) }`
- **What:** Unchecked bucket access (hot path)
- **Category:** C (necessary)
- **Why:** Hash index computation guarantees bounds; eliminating check is performance-critical
- **SAFETY comment:** ✅ Present (lines 242-247)
- **Verdict:** Correct. Hot path; bounds mathematically guaranteed by masking.

---

### 13. `epoch/drain.rs` (8 unsafe sites)

#### L61-62: `unsafe impl Send for DrainList` / `unsafe impl Sync for DrainList`
- **What:** Marks drain list as thread-safe
- **Category:** C (necessary)
- **Why:** Lock-free Treiber stack; atomic CAS synchronizes
- **SAFETY comment:** ✅ Present (lines 57-66)
- **Verdict:** Correct. Comprehensive justification.

#### L94: `unsafe { (*node).next = head }`
- **What:** Links new node in push CAS loop
- **Category:** C (necessary)
- **Why:** Lock-free list construction
- **SAFETY comment:** ✅ Present (lines 91-93)
- **Verdict:** Correct. Node exclusively owned pre-publication.

#### L138-206: `unsafe { (*current).next }` / `unsafe { (*node_ptr).epoch }` / `unsafe { Box::from_raw }` / `unsafe { (*node_ptr).next = head }`
- **What:** Multiple pointer dereferences in drain logic
- **Category:** C (necessary)
- **Why:** Walking/partitioning lock-free linked list
- **SAFETY comment:** ✅ Present (inline justifications)
- **Verdict:** Correct. Clear ownership transfer semantics.

---

### 14. `store/operations.rs` (3 unsafe sites)

#### L339: `let accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) }`
- **What:** Creates mutable accessor for RMW
- **Category:** C (necessary)
- **Why:** In-place update in mutable region
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Session owns record; epoch protects page; exclusive access guaranteed."

#### L347-349: `unsafe { accessor.as_slice_mut() }`
- **What:** Gets mutable slice for value update
- **Category:** C (necessary)
- **Why:** In-place write
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Mutable accessor guarantees exclusive access."

#### L567-573: Same as L339 + L347
- **What:** Same pattern in different operation
- **Category:** C (necessary)
- **Why:** Same as above
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Same justification.

#### L817: Same as L339
- **What:** Same pattern in another operation
- **Category:** C (necessary)
- **Why:** Same as above
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Same justification.

---

### 15. `store/functions.rs` (7 unsafe sites)

#### L122-155: `unsafe fn upsert_in_place_raw` / `unsafe fn rmw_in_place_raw`
- **What:** Trait methods for in-place updates
- **Category:** C (necessary)
- **Why:** Raw pointer to value region; user-supplied functions operate on bytes
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document contract: "value_ptr valid for value_len; exclusive access; alignment correct."

#### L280-305: `unsafe fn upsert_in_place_raw` / `unsafe fn rmw_in_place_raw` (impls)
- **What:** Implements in-place update for fixed-size values
- **Category:** C (necessary)
- **Why:** Uses `ptr::write` for direct memory write
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "value_ptr from accessor; alignment matches V; exclusive access."

#### L291: `unsafe { core::ptr::write(value_ptr as *mut V, *input) }`
- **What:** Writes value in-place
- **Category:** C (necessary)
- **Why:** Direct memory write
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "Pointer valid, aligned, exclusive."

#### L304: `unsafe { core::ptr::write(value_ptr as *mut V, *input) }`
- **What:** Same as L291
- **Category:** C (necessary)
- **Why:** Same as above
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Same justification.

#### L665-693: `unsafe { f.upsert_in_place_raw / f.rmw_in_place_raw }` (tests)
- **What:** Test invocations
- **Category:** C (necessary — test)
- **Why:** Testing the unsafe API
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should add comments.

---

### 16. `store/kv.rs` (2 unsafe sites)

#### L197-199: `unsafe impl Send/Sync for FasterKv<F>`
- **What:** Marks FASTER KV as thread-safe
- **Category:** C (necessary)
- **Why:** All internal state synchronized via atomics/locks
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document: "All shared state (hash table, log, epoch) is Send+Sync; operations use epoch protection."

---

### 17. `store/pending_io.rs` (5 unsafe sites)

#### L169: `unsafe fn read_completion_callback`
- **What:** I/O completion callback signature
- **Category:** C (necessary)
- **Why:** Device callback interface
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should document contract.

#### L173: `let ctx = unsafe { Box::from_raw(context as *mut ReadCallbackContext) }`
- **What:** Reclaims callback context
- **Category:** C (necessary)
- **Why:** Reverses `Box::into_raw`
- **SAFETY comment:** ❌ Missing
- **Verdict:** Missing. Should state: "context allocated as Box; callback invoked exactly once."

#### L488-523: `unsafe { device.read_async }` + inner unsafe blocks
- **What:** Issues async read
- **Category:** C (necessary)
- **Why:** Device trait requires unsafe
- **SAFETY comment:** ✅ Present (line 488, 513, 523)
- **Verdict:** Correct.

#### L808: `unsafe { device.write_async }` (test)
- **What:** Test write
- **Category:** C (necessary — test)
- **Why:** Testing I/O logic
- **SAFETY comment:** ❌ Missing
- **Verdict:** Test code; should add comment.

---

### 18. `buffer_pool.rs` (3 unsafe sites)

#### L43-46: `unsafe impl Send/Sync for AlignedBuffer`
- **What:** Marks aligned buffer as thread-safe
- **Category:** C (necessary)
- **Why:** Exclusive ownership of heap allocation
- **SAFETY comment:** ✅ Present (lines 41-45)
- **Verdict:** Correct.

#### L68: `let raw = unsafe { alloc(layout) }`
- **What:** Allocates aligned buffer
- **Category:** C (necessary)
- **Why:** Custom alignment requires unsafe allocator
- **SAFETY comment:** ✅ Present (lines 65-67)
- **Verdict:** Correct.

#### L108: `unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }`
- **What:** Creates immutable slice from buffer
- **Category:** C (necessary)
- **Why:** Pointer-to-slice conversion
- **SAFETY comment:** ✅ Present (lines 105-107)
- **Verdict:** Correct.

#### L117: `unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }`
- **What:** Creates mutable slice from buffer
- **Category:** C (necessary)
- **Why:** Exclusive mutable access
- **SAFETY comment:** ✅ Present (lines 114-116)
- **Verdict:** Correct.

#### L125: `unsafe { dealloc(self.ptr.as_ptr(), self.layout) }`
- **What:** Deallocates buffer in Drop
- **Category:** C (necessary)
- **Why:** Paired dealloc for alloc
- **SAFETY comment:** ✅ Present (lines 123-124)
- **Verdict:** Correct.

---

## Summary Statistics

| Metric | Count |
|--------|-------|
| **Total unsafe blocks** | 71 |
| **Total unsafe functions** | 10 (trait methods + test helpers) |
| **Total unsafe impls** | 9 (Send/Sync markers) |
| **Total unsafe sites** | 90 |
| **SAFETY comments present** | 46 (51%) |
| **SAFETY comments missing** | 44 (49%) |
| | |
| **Category A (Eliminable)** | 3 (3%) |
| **Category B (Abstractable)** | 22 (24%) |
| **Category C (Necessary)** | 65 (72%) |

### Files by Unsafe Density

| File | Unsafe Sites | Lines | Density (sites/KLOC) |
|------|--------------|-------|----------------------|
| allocator.rs | 28 | 1,358 | 20.6 |
| hybrid_log/page.rs | 21 | 836 | 25.1 |
| hybrid_log/record_ops.rs | 16 | 867 | 18.5 |
| device.rs | 12 | 599 | 20.0 |
| sync_file_device.rs | 10 | 815 | 12.3 |
| epoch/drain.rs | 8 | 243 | 32.9 |
| store/functions.rs | 7 | 725 | 9.7 |
| hybrid_log/flush.rs | 6 | 647 | 9.3 |
| store/pending_io.rs | 5 | 831 | 6.0 |
| buffer_pool.rs | 3 | 238 | 12.6 |
| store/operations.rs | 3 | 1,204 | 2.5 |
| checkpoint/index_writer.rs | 2 | 743 | 2.7 |
| hash/table.rs | 2 | 414 | 4.8 |
| store/kv.rs | 2 | 621 | 3.2 |
| hash/index.rs | 1 | 865 | 1.2 |
| hybrid_log/log_allocator.rs | 3 | 771 | 3.9 |
| hybrid_log/scan.rs | 1 | 417 | 2.4 |
| recovery/index_recovery.rs | 1 | 362 | 2.8 |

**Highest-risk module:** `epoch/drain.rs` (32.9 sites/KLOC) — lock-free linked list manipulation.

---

## Critical Recommendations

### 1. **Mandatory SAFETY Comments (Priority: CRITICAL)**
49% of unsafe sites lack justifications. This violates Rust safety best practices and the team's code quality bar.

**Action:** Add `// SAFETY:` comments to all 44 missing sites before Phase 1 implementation begins.

**Responsible:** Aragorn (core coder)  
**Timeline:** Before any feature work starts  
**Blocker:** Yes — no new unsafe code without justification

### 2. **Eliminate Category A Sites (Priority: HIGH)**
3 eliminable unsafe sites in `checkpoint/index_writer.rs` and `allocator.rs`.

**Actions:**
- L153, L690 (`index_writer.rs`): Replace `&*(ptr as *const [u8; N])` with `bytemuck::bytes_of(bucket)`.
- L431 (`allocator.rs`): Restructure init to avoid `(*dir)` dereference; use safe accessor before `Box::into_raw`.

**Responsible:** Aragorn  
**Timeline:** Phase 0 cleanup (pre-implementation)  
**Effort:** ~30 minutes  

### 3. **Abstract Category B Sites (Priority: MEDIUM)**
22 abstractable sites, mostly in `page.rs` and `record_ops.rs`.

**Key refactors:**
- **`PageFrame::as_mut_slice` / `::zero`:** Change to safe methods with `&mut self` receiver. Eliminates 5 unsafe sites.
- **`RecordAccessor::new` / `MutableRecordAccessor::new`:** Create safe factory methods (`from_log_read`, `from_allocator`) that encapsulate safety checks. Keep raw constructors private.
- **`MallocFixedPageSize` Send/Sync:** Extract `PageDir` into `Arc`-wrapped struct; eliminate manual impls.

**Responsible:** Aragorn (code refactor), Frodo (task planning)  
**Timeline:** Phase 1 implementation (parallel with feature work)  
**Effort:** ~2-3 days  

### 4. **Miri Verification (Priority: HIGH)**
Lock-free allocator (Treiber stack with 16-bit ABA tag) is the highest-risk component.

**Action:** Run Miri on allocator tests under stress (concurrent alloc/free churn).

**Command:**
```bash
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-symbolic-alignment-check" \
cargo +nightly miri test --package faster-core allocator::
```

**Responsible:** Éowyn (simulation expert)  
**Timeline:** Phase 0 (before Aragorn writes dependent code)  
**Blocker:** Yes — allocator must pass Miri before use in log/index

### 5. **FFI Callback Hardening (Priority: MEDIUM)**
Device callbacks assume `context` pointer validity with no runtime validation.

**Action:** Add debug assertions in callback preambles:
```rust
unsafe fn callback(context: *mut u8, ...) {
    debug_assert!(!context.is_null(), "FFI callback received null context");
    // ... existing code
}
```

**Responsible:** Sam (device layer expert)  
**Timeline:** Phase 1  
**Effort:** ~1 hour  

### 6. **ABA Risk Audit (Priority: CRITICAL)**
Treiber stacks in `allocator.rs` and `epoch/drain.rs` use 16-bit ABA tags.

**Risk:** If epoch deferral breaks or is disabled, high-churn workloads could wrap the tag and trigger ABA.

**Mitigation:**
- Verify epoch deferral is always active for `MallocFixedPageSize::free` (currently enforced by `set_epoch` call).
- Add compile-time assertion that epoch table attachment happens before any `free` calls.
- Document tag overflow risk in `allocator.rs` header comments.

**Responsible:** Galadriel (this audit) → Frodo (verify enforcement)  
**Timeline:** Phase 0 (documentation) + ongoing verification  

### 7. **Record Accessor Lifetime Tracking (Priority: MEDIUM)**
`RecordAccessor` / `MutableRecordAccessor` hold raw pointers with no lifetime tracking.

**Risk:** If page is evicted while accessor is live, pointer dangles.

**Current mitigation:** Epoch guards prevent eviction while accessors exist (session-level discipline).

**Improvement:** Consider phantom lifetime parameter or builder pattern that ties accessor lifetime to epoch guard:
```rust
pub struct RecordAccessor<'guard> {
    ptr: *const u8,
    _guard: PhantomData<&'guard EpochGuard>,
}
```

**Responsible:** Elrond (API design expert)  
**Timeline:** Phase 2 (API stabilization)  
**Optional:** Not blocking for Phase 1  

---

## Audit Verdict

**Overall assessment:** The unsafe usage is **appropriate but under-documented**.

- **Strengths:**
  - 72% of unsafe sites are genuinely necessary (FFI, lock-free algorithms, custom allocators).
  - Most unsafe blocks have clear intent (even if comment is missing).
  - Lock-free algorithms (Treiber stacks, CAS loops) follow standard patterns.
  - No obvious memory safety bugs detected.

- **Weaknesses:**
  - 49% missing SAFETY comments is unacceptable for production code.
  - 3 eliminable sites should be trivial to fix.
  - 22 abstractable sites represent refactoring opportunities to reduce unsafe surface.
  - ABA tag overflow risk is documented but not runtime-enforced.

**Blockers for Phase 1:**
1. Add SAFETY comments to all 44 missing sites.
2. Run Miri on allocator + epoch drain tests.
3. Fix 3 Category A (eliminable) sites.

**Non-blockers (can proceed in parallel):**
- Category B refactors (nice-to-have for Phase 1, required for Phase 2).
- Lifetime improvements for record accessors.
- FFI callback debug assertions.

---

## Appendix: Quick Reference

### Category A (Eliminable) — Fix Immediately
1. `checkpoint/index_writer.rs:153` — Use `bytemuck::bytes_of`
2. `checkpoint/index_writer.rs:690` — Use `bytemuck::bytes_of`
3. `allocator.rs:431` — Refactor init to avoid raw pointer deref

### Category B (Abstractable) — Refactor in Phase 1
1. `page.rs:246-249` — Change `as_mut_slice` to `&mut self`
2. `page.rs:259-267` — Change `zero` to `&mut self`
3. `record_ops.rs:59-70` — Add safe factory for `RecordAccessor`
4. `record_ops.rs:167-178` — Add safe factory for `MutableRecordAccessor`
5. `allocator.rs:317-323` — Refactor to eliminate manual Send/Sync impls
6. `hash/table.rs:215` — Replace `get_unchecked` with checked access (debug builds)
7. _(15 more in page.rs, record_ops.rs — see detailed findings)_

### Missing SAFETY Comments — Add Before Phase 1
- `page.rs`: 15 sites
- `record_ops.rs`: 7 sites
- `sync_file_device.rs`: 6 sites
- `store/functions.rs`: 5 sites
- `hybrid_log/flush.rs`: 4 sites
- `store/operations.rs`: 3 sites
- _(Full list: 44 sites total — see detailed findings)_

---

**Audit completed:** 2026-03-05  
**Next review:** After Phase 1 implementation (Aragorn's code additions)  
**Continuous monitoring:** All new unsafe code requires pre-commit SAFETY justification.
# Retrospective Action Items — Iteration 3 Session

**Facilitator:** Gandalf (Lead / System Architect)  
**Date:** 2026-03-05  
**Scope:** All work on `squad` branch this session (Iterations 1–3 + Iter 4 planning)

---

## ⚡ Action Items

### AI-1: Fix 4 Failing Doctests in `builder.rs` and `kv.rs`
**Priority:** P0 — broken CI  
**Owner:** Next available agent  
**Description:** Four doctests in `crates/faster-core/src/store/builder.rs` and `kv.rs` reference `TestFunctions` which is not in scope for doc examples. These should use `SimpleFunctions::<u64, u64>` instead. This was missed because `cargo test --lib` and integration tests pass — doctests aren't in the default test flow.  
**Acceptance:** `cargo test --doc -p faster-core` passes with 0 failures.

### AI-2: Add `cargo test --doc` to Pre-Commit Quality Gate
**Priority:** P1  
**Owner:** Gandalf / qbradley  
**Description:** The 4 broken doctests went undetected because our testing flow runs unit + integration tests but not doc tests. Either add `cargo test --doc` to the standard test command or switch to `cargo test` (which includes doctests) as the gate.  
**Acceptance:** CI/quality gate command explicitly includes doc tests.

### AI-3: Enforce "Doc Examples Must Compile" Convention
**Priority:** P1  
**Owner:** All agents  
**Description:** Doc examples that reference internal test types (`TestFunctions`) are not real examples — they mislead users. Convention: every `/// # Example` block must use only public API types and be copy-pasteable by an external user. Add `#![deny(rustdoc::broken_intra_doc_links)]` to `lib.rs` if not already present.  
**Acceptance:** Convention documented; rustdoc lint enabled.

### AI-4: Investigate 10M ops/sec Target Gap
**Priority:** P2  
**Owner:** Performance-focused agent  
**Description:** Iteration 3 peaked at 8.11M single-threaded upserts/sec on shared infrastructure. The 10M target was set in the iteration plan. Either (a) validate on dedicated hardware, (b) profile the remaining ~20% gap (likely epoch overhead, hash computation, or memory allocation), or (c) revise the target with justification.  
**Acceptance:** Root-cause analysis with profiling data, or validated benchmark on dedicated hardware.

### AI-5: Audit `unsafe` Footprint Before Iteration 4
**Priority:** P2  
**Owner:** Galadriel (Safety Auditor) or equivalent  
**Description:** 23 files contain `unsafe` code (~191 occurrences). Before adding io_uring and C FFI (Iteration 4), audit current unsafe sites for soundness documentation. Each `unsafe` block should have a `// SAFETY:` comment explaining the invariant. Several were added during fast iteration without full documentation.  
**Acceptance:** Every `unsafe` block in `faster-core` has a `// SAFETY:` comment.

### AI-6: Write-Path Pending I/O Completion — Close the Loop
**Priority:** P1  
**Owner:** Implementation agent  
**Description:** Write-path pending I/O (`bfeea1a3`) was delivered but QUICKSTART.md was updated (`6b94ce3c`) to be "honest about pending I/O semantics" — meaning the mental model had to be corrected after the docs were written. The lesson: write the doc *after* the feature stabilizes, not during. For Iteration 4, batch docs into a single phase after all features land.  
**Acceptance:** Iteration 4 plan structures docs as a trailing phase.

### AI-7: Reduce Test Execution Time Sensitivity
**Priority:** P3  
**Owner:** Any  
**Description:** The `write_pending_completion` tests originally took 18.5s and were sped up 20× to 0.9s (`6a2d4ebd`). This suggests timeouts and sleep-based synchronization were used initially. Convention for Iteration 4: no `thread::sleep` in tests unless explicitly justified. Use condition variables, barriers, or event-driven synchronization.  
**Acceptance:** Convention documented; new tests reviewed for sleep-based waits.

---

## Decision Proposals (from retrospective discussion)

### DP-1: Iteration 4 Should Gate on `cargo test` (not `cargo test --lib`)
All test types (unit, integration, doc, example) must pass before any commit is considered green.

### DP-2: Documentation Phase Should Trail Feature Phases
Quickstart and public docs written once per iteration, after all features stabilize. Internal doc-comments written inline with code.

### DP-3: Benchmark Validation on Dedicated Hardware
Performance targets should be validated on dedicated (non-shared) infrastructure before claiming or missing targets.
# Decision: ARM Memory Ordering — No Conditional Fences Needed

**Agent:** Gandalf (Lead Architect)
**Date:** 2026-03-06
**Status:** DECIDED
**Impact:** Cross-platform correctness — affects all atomic operations

## Decision

No `#[cfg(target_arch = "aarch64")]` conditional fences are required in faster-core.

## Rationale

1. **Rust's `Ordering` enum is the abstraction boundary.** The compiler emits correct barrier instructions per-target (e.g., `dmb` on ARM for Acquire/Release). We never need to manually insert platform-specific fences.

2. **All `Ordering::Relaxed` uses are advisory counters** — metrics, `live_entry_count`, `high_water`, `allocator.count`. These are never read to make synchronization decisions. Individual 64-bit atomicity (guaranteed by hardware) is sufficient.

3. **All synchronization-critical paths use proper orderings:**
   - DrainList CAS: `AcqRel`/`Acquire`
   - Page state transitions: `AcqRel` via `compare_exchange`
   - Epoch table: `Acquire`/`Release`
   - Hash index: `AcqRel` CAS, `Acquire` loads
   - Allocator tail: `AcqRel` CAS, `Acquire` loads

4. **`Metrics::snapshot()` non-atomicity is documented and acceptable.** Cross-field inconsistency is inherent to advisory counters and explicitly documented.

## Implications

- No platform-specific code in faster-core's concurrency layer.
- ARM correctness is verified by `cargo check --target aarch64-unknown-linux-gnu`.
- Future atomic additions should follow the same pattern: use `Relaxed` only for advisory data, never for synchronization.
# Decision: Adaptive Submission Batching (U4)

**Agent:** Gimli (Database/Storage Expert)  
**Date:** 2026-03-08  
**Status:** Implemented (Wave 5, U4)  
**Impact:** I/O thread performance — reduces syscall overhead under load

## Decision

Added configurable submission coalescing to the `UringDevice` I/O thread. Under high load, SQEs are accumulated before calling `io_uring_submit()` (up to 32 entries or 1µs window). Under low load, the batch window expires quickly to keep latency low. An adaptive tuner auto-adjusts the window based on observed throughput.

## API Surface

- `BatchPolicy` — configures max_batch (default 32), batch_window_us (default 1µs), adaptive flag
- `BatchPolicy::default()` — adaptive batching with 32/1µs
- `BatchPolicy::immediate()` — no coalescing (max_batch=1, window=0)
- `UringDeviceConfig::batch_policy` — new required field

## Ring Changes

- `Ring::unsubmitted()` — getter for SQEs pushed but not yet submitted to kernel
- `Ring::try_reap_completions()` — non-blocking CQ drain for use during batch accumulation
- `Ring::submit()` now resets the unsubmitted counter

## Design Rationale

1. **Spin-wait over `recv_timeout`**: The 1µs default window is shorter than OS scheduler quantum, so `recv_timeout` would overshoot badly. A `try_recv` + `spin_loop` approach gives microsecond-precision batch windows.
2. **Adaptive tuning via EMA**: Exponential moving average of batch utilisation (ratio of actual batch size to max_batch) is tracked with α=0.1. Every 64 submits, the window doubles if utilisation > 75% or halves if < 25%.
3. **Sync ops unaffected**: `read_sync`/`write_sync` bypass the ring entirely (pread/pwrite on caller thread), so batching has no effect on the recovery/metadata path.

## Implications

- **Legolas (Perf):** Benchmark test `bench_batched_vs_immediate` compares both modes. Under high concurrent load, batching reduces syscalls ~32x.
- **All agents:** `UringDeviceConfig` now requires `batch_policy` field. Use `BatchPolicy::default()` for standard behavior.
# Decision: faster-uring Crate — Safe io_uring Ring Wrapper

**Agent:** Gimli (Database/Storage Expert)
**Date:** 2026-03-07
**Status:** Implemented (Wave 5, U1)
**Impact:** New crate — foundation for io_uring-based device I/O

## Decision

Created `rust/crates/faster-uring/` with a safe `Ring` wrapper around Linux io_uring. The public API exposes zero unsafe code; all io_uring syscalls are encapsulated internally.

## API Surface

- `UringConfig` — queue depth (power of 2), SQPOLL mode, direct I/O flag
- `Ring::new(config)` — create io_uring instance
- `Ring::submit_read/write/fsync` — queue I/O operations
- `Ring::submit()` — flush SQ to kernel
- `Ring::reap_completions()` — wait and collect CQEs as `(user_data, result)` pairs
- `Ring::drain()` — block until all in-flight ops complete

## Crash Safety Properties

1. **Write ≠ durable.** A completed write only means data reached the kernel. Only a completed fsync guarantees persistence.
2. **Fsync semantics.** After fsync completion, all prior writes to that fd are on stable storage.
3. **Drop safety.** `Ring::drop()` calls `drain()` to wait for in-flight I/O, preventing kernel use-after-free of caller buffers.

## Buffer Lifetime Contract (U1)

The kernel holds raw pointers to caller buffers between submit and completion. In U1, buffer lifetime is the **caller's responsibility** — documented in both module-level and method-level docs. U2 will add a registered buffer pool to enforce this at the type level.

## Rationale

- **Completion-based model** aligns with FASTER's callback architecture (Decision #2: no async/await in core)
- **io-uring crate v0.7.11** is the best-maintained Rust binding; provides safe SQE construction
- **Linux-only** via `#[cfg(target_os = "linux")]` — io_uring is a Linux kernel feature
- **Direct I/O support** is essential for FASTER's page-aligned, unbuffered workloads

## Dependencies

- `io-uring = "0.7"` (new external dependency)
- `faster-core` (path dependency, for future type integration)
- `tempfile = "3"` (dev-dependency for tests)

## Implications

- **Sam (Device):** The `Ring` API is designed to plug into the `Device` trait's completion-callback model. A `UringDevice` adapter will wrap `Ring` in a future wave.
- **Galadriel (Safety):** The `unsafe` blocks in `ring.rs` (3 total: read, write, fsync SQE pushes) all have `// SAFETY:` comments. The buffer-lifetime unsafety is documented but not yet type-enforced.
- **All agents:** `faster-uring` is auto-discovered by the workspace (`members = ["crates/*"]`).

## Next Steps

- [ ] U2: Registered buffer pool (type-safe kernel buffer management)
- [ ] Integration with `Device` trait as `UringDevice`
- [ ] SQPOLL benchmarking (requires `CAP_SYS_ADMIN` or `IORING_SETUP_SQPOLL` sysctl)
# AI-4 Performance Analysis: Epoch Amortization & 10M ops/sec Target

**Author:** Legolas (Performance Guru)  
**Date:** 2026-03-06  
**Status:** Analysis Complete  
**Machine:** Standard_D4ds_v5 equivalent (20 vCPU dev machine, 3.9 GHz)

---

## Executive Summary

Per-operation epoch protection is the single largest bottleneck in FASTER Rust, consuming **~128 ns (33–44%) of every operation's cost**. The existing `UnsafeContext` API already eliminates this overhead — it just isn't used by the default API paths. With epoch amortization:

- **Single-thread reads already exceed 10M ops/sec** (12.7M measured)
- **4-thread amortized upserts hit 10M total ops/sec** (measured)
- **Single-thread upsert reaches 6.1–6.6M ops/sec** (limited by hash index cache misses)
- **10M single-thread upserts are NOT achievable** without hash table prefetching or batched operations

---

## 1. Measured Component Costs (ns/op)

### 1.1 Isolated Components

| Component | Cost (ns) | Notes |
|-----------|-----------|-------|
| Hash computation (u64 key) | **0.9** | Negligible — FASTER's multiply-rotate hash |
| Bare epoch protect/unprotect | **97–100** | Dominated by `compute_safe_epoch` scanning 256 entries |
| Epoch with drain list active | **97–102** | Drain list not a significant overhead |
| Pipeline insert (epoch+hash+alloc+write) | **118** | Criterion benchmark, single bucket |
| Pipeline lookup (epoch+find+read) | **123** | Criterion benchmark, 7-entry bucket |

### 1.2 Full Store Operations (Per-Op Epoch — Current Behavior)

| Operation | ns/op | M ops/sec |
|-----------|-------|-----------|
| Upsert insert (sequential) | **293** | 3.4 |
| Upsert update (key exists) | **277** | 3.6 |
| Point read (key exists) | **276** | 3.6 |
| RMW (from YCSB criterion) | ~170 | 5.9 |

### 1.3 Full Store Operations (Amortized Epoch — UnsafeContext)

| Operation | ns/op | M ops/sec | Speedup |
|-----------|-------|-----------|---------|
| Upsert insert (sequential) | **165** | 6.1 | **1.8×** |
| Upsert update (key exists) | **151** | 6.6 | **1.8×** |
| Point read (key exists) | **79** | 12.7 | **3.5×** |

### 1.4 Derived Cost Breakdown (per upsert insert, 293 ns total)

| Component | Estimated Cost | % of Total | Method |
|-----------|---------------|------------|--------|
| **Epoch protect/unprotect** | **~128 ns** | **44%** | 293 - 165 = 128 |
| Hash index traversal (cold) | **~80 ns** | **27%** | 165 - 118 + 97 (pipeline has epoch too) |
| Hash computation | **~1 ns** | **<1%** | Direct measurement |
| Allocate at tail | **~20 ns** | **7%** | Atomic bump + page math |
| Record write + CAS commit | **~20 ns** | **7%** | memcpy + CAS |
| Overhead / misc | **~44 ns** | **15%** | Version chain walk, snapshot, layout |

---

## 2. Why Epoch Is So Expensive: The `compute_safe_epoch` Scan

The 97–128 ns epoch cost is NOT from the `protect()` or `unprotect()` atomics themselves (those are ~5 ns each). It's from `try_drain()` → `compute_safe_epoch()`:

```rust
fn compute_safe_epoch(&self) -> u64 {
    let current = self.current_epoch.load(Ordering::SeqCst);  // ← SeqCst fence
    let mut min = current;
    for entry in self.table.iter() {                           // ← 256 iterations!
        let epoch = entry.local_current_epoch.load(Ordering::Acquire);
        // ...
    }
    min.saturating_sub(1)
}
```

Every `unprotect()` (when reentrant goes 1→0) triggers this scan of **all 256 epoch table entries** (256 × 64-byte cache lines = 16 KB). The `Acquire` loads on each entry create a pipeline stall.

### Hardware Evidence (perf stat)

| Counter | Value | Assessment |
|---------|-------|------------|
| IPC (instructions per cycle) | **0.73** | Very low — memory bound |
| L1 dcache miss rate | **43%** | Extremely high |
| LLC cache miss rate | **17%** | Significant DRAM traffic |
| Branch miss rate | **0.39%** | Excellent (not a branch predictor problem) |

The 43% L1 dcache miss rate confirms this is a **memory-access-bound** workload, not a compute-bound one.

---

## 3. Multi-Thread Scaling Analysis

### 3.1 Per-Op Epoch (Current)

| Threads | Total M ops/sec | Per-thread M ops/sec | Scaling efficiency |
|---------|----------------|---------------------|-------------------|
| 1 | 2.8–3.0 | 2.8–3.0 | 100% (baseline) |
| 2 | 4.2–4.5 | 2.1–2.3 | 75% |
| 4 | 6.3 | 1.6 | **53%** |

### 3.2 Amortized Epoch (UnsafeContext)

| Threads | Total M ops/sec | Per-thread M ops/sec | Scaling efficiency |
|---------|----------------|---------------------|-------------------|
| 1 | 4.6–4.7 | 4.6–4.7 | 100% (baseline) |
| 2 | 7.1–7.7 | 3.6–3.9 | 78% |
| 4 | **10.0** | 2.5 | **54%** |

### 3.3 Why Scaling Is Poor (Both Paths)

Even with amortized epoch, scaling is ~54% efficient at 4 threads. Remaining contention sources:

1. **Hash index CAS contention** — `find_or_create` uses CAS on bucket entries. Different keys in the same bucket contend.
2. **Log tail allocation** — all threads compete for the atomic tail pointer. Even with cache-line padding, the `fetch_add` on the tail address bounces between cores.
3. **CPU cache coherence** — adjacent hash buckets in the same cache line cause false sharing.

### 3.4 Epoch Contention (Criterion Numbers)

| Background threads | Epoch ns/op | Slowdown |
|-------------------|-------------|----------|
| 0 | 99 | 1.0× |
| 2 | 252 | 2.5× |
| 4 | 378 | 3.8× |

This explains why per-op epoch scaling is so bad — the epoch table entries bounce between cores. Amortized epoch eliminates this per-operation cost entirely.

---

## 4. Refresh Interval Tuning

| Refresh Interval | ns/op (update) | M ops/sec |
|-----------------|----------------|-----------|
| Every 32 ops | 197 | 5.1 |
| Every 64 ops | 179 | 5.6 |
| **Every 128 ops** | **176** | **5.7** |
| Every 256 ops | 196 | 5.1 |
| Every 512 ops | 202 | 5.0 |
| Every 1024 ops | 193 | 5.2 |

**Sweet spot: 64–128 operations between refreshes.** Too frequent → refresh overhead (epoch scan); too infrequent → cache pollution from stale epoch entries.

The U-shaped curve at 256+ is surprising — likely due to increased memory pressure from deferred drain callbacks accumulating without timely cleanup.

---

## 5. Projected Throughput with Optimizations

### 5.1 Optimization Impact Estimates

| Optimization | Single-thread Upsert | Single-thread Read | Effort |
|-------------|---------------------|-------------------|--------|
| **Baseline (current per-op)** | **3.4M** | **3.6M** | — |
| + Epoch amortization (UnsafeContext) | **6.1M (+80%)** | **12.7M (+253%)** | Low — API already exists |
| + Hash prefetching (batch lookahead) | ~8–9M (+30%) | ~15M+ | Medium |
| + Reduce epoch table to registered-only scan | ~6.5M (+7%) | ~13M (+3%) | Low |
| + Smaller hash entries (48-bit packed) | ~7M (+15%) | ~14M (+10%) | Medium |
| + All combined | ~9–10M | ~16M+ | High |

### 5.2 Can We Reach 10M ops/sec Single-Thread?

| Workload | Achievable? | Path |
|----------|-------------|------|
| Point reads | **YES — already done** | 12.7M with UnsafeContext |
| Read-heavy (95/5) | **YES** | Projected ~11M with UnsafeContext |
| Mixed (50/50) | **MAYBE** | ~8M projected, 10M with prefetching |
| Pure upsert (insert) | **NO** without hash prefetching | 6.1M ceiling with current hash index |
| Pure upsert (update) | **UNLIKELY** | 6.6M, maybe 8M with prefetching |

**Key insight:** Reads are 2× faster than upserts because reads only do `find()` (no CAS, no allocation, no record write). The hash index find is the dominant cost for reads at 79ns — this is almost entirely cache-miss latency on the bucket lookup.

---

## 6. Optimization Priority Recommendations

### Priority 1: Promote UnsafeContext as Primary API (Impact: +80–250%)
**Effort: Low (1–2 days)**

The `UnsafeContext` API already exists and works. The gains are massive:
- Upsert: 293→165 ns (1.8× faster)
- Read: 276→79 ns (3.5× faster)

**Actions:**
- Add `unsafe_context` examples to all documentation
- Consider making amortized epoch the default in YCSB benchmarks
- Add batch operation helpers that auto-refresh every N ops
- Consider a `BatchSession` wrapper that handles refresh automatically

### Priority 2: Reduce `compute_safe_epoch` Scan (Impact: +5–10%)
**Effort: Low (1 day)**

Currently scans all 256 `MAX_THREADS` entries even if only 1–4 are registered. Track a `max_registered_index` to bound the scan.

```rust
// Instead of scanning all 256 entries:
for entry in &self.table[..self.max_registered_index.load(Relaxed) + 1] {
```

### Priority 3: Hash Index Prefetching (Impact: +20–40%)
**Effort: Medium (3–5 days)**

The hash index bucket lookup causes an L1 cache miss on every operation (43% miss rate confirms this). Prefetching the next key's bucket while processing the current key can hide memory latency.

**Design:** Add a `prefetch_bucket(key_hash)` method and batch operation APIs:
```rust
// Pseudo-code for batched upsert
for window in keys.windows(LOOKAHEAD) {
    store.prefetch_for_upsert(&window[LOOKAHEAD-1]);
    store.upsert_internal(&window[0], ...);
}
```

### Priority 4: Log Tail Allocation Optimization (Impact: +5–10%)
**Effort: Medium (2–3 days)**

Per-thread page-level allocation pools could eliminate atomic contention on the global tail. Each thread gets a "slab" from the tail, then allocates locally within it.

### Priority 5: Hash Table Layout Optimization (Impact: +10–15%)
**Effort: High (1–2 weeks)**

Current buckets: 7 entries × 8 bytes + 8 bytes overflow = 64 bytes (1 cache line). Could experiment with:
- 2-cache-line buckets (14 entries, fewer overflow chains)
- Separate tag array (better SIMD potential)
- Cuckoo hashing (no overflow chains)

### Priority 6: Multi-Thread Tail Contention (Impact: better scaling)
**Effort: Medium (3–5 days)**

Partition the log tail per-thread (each thread has its own allocation region within the same page or across pages). This eliminates the `fetch_add` contention on the global tail pointer.

---

## 7. Summary Table

| Metric | Current (Per-Op) | With UnsafeContext | Projected (All Opts) |
|--------|------------------|--------------------|---------------------|
| Single-thread upsert | 3.4M | **6.1M** | ~9–10M |
| Single-thread read | 3.6M | **12.7M** | ~15M+ |
| 4-thread upsert | 6.3M | **10.0M** | ~15M+ |
| 4-thread read | ~8M est. | **~30M+ est.** | ~40M+ |

---

## 8. Key Architectural Observations

1. **Epoch amortization is the C# `UnsafeContext` equivalent** — Rust FASTER already has this API. The current per-operation API (`FasterKv::upsert` etc.) is the simple/safe path; `UnsafeContext` is the fast path. This matches the C# FASTER design exactly.

2. **The 10M single-thread upsert target requires hash prefetching** — after epoch amortization, the hash index cache miss (~80ns per lookup) is the dominant remaining cost. Without prefetching, the CPU stalls on every operation waiting for the L1 miss to resolve.

3. **Read performance is excellent with amortization** — 12.7M single-thread reads exceeds the 10M target by 27%. Reads have a lower cache-miss cost because they don't write (no store buffer pressure, no cache line invalidation).

4. **Multi-thread scaling is limited by cache coherence** — even with perfect epoch amortization, shared data structures (hash index, log tail) create cross-core cache traffic. Partitioned approaches (per-thread allocation, sharded hash buckets) would improve scaling.

---

## Appendix: Benchmark Artifacts

- **Custom benchmark:** `rust/crates/faster-core/benches/perf_analysis.rs`
- **Criterion pipeline benchmarks:** `rust/crates/faster-bench/benches/main.rs`
- **YCSB benchmarks:** `rust/crates/faster-core/benches/ycsb.rs`
- **Run command:** `cd rust && cargo bench --bench perf_analysis -p faster-core -- --nocapture`

### Raw Benchmark Output (Representative Run)

```
Bare epoch:          97.4 ns/op  (10.3M ops/sec)
Hash only:            0.9 ns/op  (1153M ops/sec)
Per-op upsert ins:  293.2 ns/op  (3.4M ops/sec)
Per-op upsert upd:  277.0 ns/op  (3.6M ops/sec)
Amortized ins:      165.2 ns/op  (6.1M ops/sec)
Amortized upd:      151.4 ns/op  (6.6M ops/sec)
Per-op read:        275.5 ns/op  (3.6M ops/sec)
Amortized read:      78.8 ns/op  (12.7M ops/sec)
4T per-op upsert:     6.3M total ops/sec
4T amortized upsert: 10.0M total ops/sec
```
# Decision: FFI Opaque Handle Design

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-07-25
**Status:** Implemented (Wave 3, F1)
**Impact:** FFI boundary design — affects all C/language bindings

## Decision

Use a `RwLock<HashMap<u64, Box<dyn Any + Send + Sync>>>` handle table with monotonically increasing `AtomicU64` handles and callback-based access (`with`/`with_mut` closures).

## Rationale

1. **Monotonic handles (never reuse)** — Prevents ABA bugs from C callers caching stale handles. The 64-bit counter won't wrap in any realistic workload (would take 584 years at 1 billion handles/sec).
2. **`RwLock<HashMap>` over lock-free** — FFI calls are session-setup/teardown level, not hot-path. The simplicity of `RwLock` eliminates entire classes of lock-free bugs while allowing concurrent read access.
3. **Callback-based `with`/`with_mut`** — The `RwLock` read-guard must be held while the reference is live. A closure API scopes guard lifetime without leaking lock internals to callers. Alternative (returning a guard wrapper) would complicate the FFI layer.
4. **Poisoned-lock recovery** — At the FFI boundary, unwinding through C frames is UB. We use `unwrap_or_else(|e| e.into_inner())` to recover poisoned locks.
5. **Type-mismatch preserves entries** — `remove<WrongType>()` returns `None` without destroying the stored object. The type-check + remove is atomic under the write lock.
6. **`Relaxed` ordering on handle counter** — Only uniqueness is required (monotonic increment). The `RwLock` write-lock provides the happens-before for HashMap visibility.

## Implications

- F2-F5 will use `HandleTable::insert` to wrap `FasterKv`, sessions, etc. behind opaque `u64` handles.
- The `with`/`with_mut` closure pattern should be used consistently across all FFI functions that access stored objects.
- `FasterStatus` discriminant values are ABI-stable — new variants may be added but existing values must never change.
- The `faster-ffi` crate already has `cdylib` + `staticlib` crate types configured for shared/static library output.

## Alternatives Considered

- **Slab allocator with generation counters** — More complex, slight advantage for use-after-free detection, but monotonic handles already prevent ABA and the generation counter adds no benefit when handles are never reused.
- **Lock-free concurrent HashMap (dashmap, etc.)** — Overkill for FFI call frequency. Adds a dependency and complexity for no measurable gain.
- **Returning raw pointers directly** — Unsafe, no type safety, no thread safety, invites UB from C callers.
# Decision: Overflow Bucket Pool Design

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-07
**Status:** DECIDED — Implemented
**Task:** 2b (Overflow Bucket Pool)

## Decision

Overflow bucket allocation, chaining, and traversal implemented as:
1. `OverflowBucketPool` — thin wrapper around `MallocFixedPageSize<HashBucket>` in `overflow.rs`
2. Chain operations — methods on `HashBucket` in `hash_bucket.rs`

## Key Design Choices

### 1. Re-zeroing on allocate (not on free)

The pool re-zeroes buckets in `allocate()` rather than in `free()`. Rationale: fresh bump-allocated pages are already zeroed, so only free-list reuse needs re-zeroing. Zeroing at allocate-time means we zero unconditionally (safe) rather than relying on callers to free cleanly. Cost: ~7 relaxed stores per allocate — negligible compared to the allocation itself and the subsequent cold cache line fetch.

### 2. Overflow CAS: AcqRel/Acquire ordering

`get_or_create_overflow()` uses `compare_exchange(ZERO, new_addr, AcqRel, Acquire)`:
- **AcqRel on success:** Release publishes the zeroed bucket content (entries + overflow pointer) before the pointer becomes visible. Acquire ensures we see any prior writes to this overflow slot.
- **Acquire on failure:** We must see the winner thread's fully-initialized bucket.

This matches Architecture §3.1.4: "Write overflow pointer: Release" / "Read overflow pointer: Acquire".

### 3. Chain methods on HashBucket, pool in separate module

Chain traversal, search, and insert live on `HashBucket` because they access the private `overflow_address` field and are core bucket semantics. The `OverflowBucketPool` lives in `overflow.rs` to avoid bloating `hash_bucket.rs` (which was already 1591 lines).

### 4. Iterative traversal, not recursive

`for_each_bucket()` and `find_entry_in_chain()` use iterative loops rather than recursion. Overflow chains are typically short (load factor < 1.0 means most chains are depth 0), but correctness shouldn't depend on stack depth assumptions.

`insert_in_chain()` has one recursive tail-call for the rare case where a freshly-allocated overflow is immediately filled by concurrent threads. This is bounded by the number of concurrent inserters.

## Implications

- **Gimli (storage):** Checkpoint must serialize overflow buckets. `OverflowBucketPool` addresses are `LogicalAddress`es that can be persisted.
- **Aragorn (API):** Hash table (task 2d) can now use `insert_in_chain` and `find_entry_in_chain` directly — no manual overflow management needed.
- **Legolas (perf):** Benchmarks added for chain traversal at depth 0/1/2/5. Primary bucket lookup (depth=0) is the hot path and has zero overhead from overflow machinery.

---

## 38. FFI Panic Safety Is Mandatory

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-06
**Status:** DECIDED — Applied
**Impact:** FFI boundary safety — prevents undefined behavior from Rust panics

### Decision

All `extern "C"` FFI functions MUST be wrapped in `std::panic::catch_unwind` to prevent Rust panics from unwinding across the FFI boundary.

### Rationale

Unwinding across an FFI boundary is instant undefined behavior per the Rust Reference. The FASTER FFI crate exposes 12 `extern "C"` functions that invoke complex Rust store operations (allocations, CAS loops, hash index lookups). Any of these could panic under extreme conditions (OOM, poisoned locks, debug assertions). Without `catch_unwind`, a single panic corrupts the C caller's stack.

### Implementation

- Handle-returning functions: return `INVALID_HANDLE` on panic
- Status-returning functions: return `FasterStatus::InternalError` on panic
- Pointer validation happens BEFORE the `catch_unwind` boundary (pointer UB → abort is acceptable)
- Use `AssertUnwindSafe` where the closure captures mutable state (correct for FFI error recovery)

### Rule

Any new `extern "C"` function added to `faster-ffi` MUST include `catch_unwind`. PR reviewers should reject FFI functions without it.

### Verification

70 FFI tests pass after the change. Clippy clean.

---

## 39. Rust FASTER Benchmark Baseline Established

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-06
**Status:** Informational
**Impact:** Performance regression testing and cross-implementation comparison baseline

### Context

Cross-implementation benchmark suite (`cross-impl-bench`) is now available for standardized YCSB performance testing. Results establish the Rust baseline.

### Key Numbers (i9-10900K, UnsafeContext, 1M keys, uniform)

| Metric | 1T | 8T | 16T |
|--------|---:|---:|----:|
| Workload A ops/sec | 3.42M | 31.58M | 56.42M |
| Workload C ops/sec | 3.50M | 31.17M | 59.77M |
| P50 latency | 300ns | 200ns | 200ns |
| P99.9 latency | 1000ns | 900ns | 900ns |

### Implications

- **All:** 60M ops/sec at 16T is the Rust baseline to beat or defend
- **Aragorn/Frodo:** Any core changes must not regress below these numbers
- **Legolas:** Hash prefetching is the #1 optimization target for single-thread gains
- **Éowyn:** These numbers should be tracked in CI regression tests

---

## 40. io_uring Async Read Beyond EOF Semantics

**Author:** Gimli (Database/Storage Expert)
**Date:** 2026-03-07
**Status:** Observation (informational)
**Impact:** UringDevice API semantics documentation

### Context

During battle testing of UringDevice, discovered that io_uring's `read` opcode returns a short read (0 bytes) when reading beyond the end of a file, rather than filling the buffer with zeros.

The sync path (`read_sync` → `read_complete_at`) explicitly handles this by zero-filling the remainder of the buffer on short reads/EOF.

The async path does not — the io_uring CQE result indicates the number of bytes actually read, and the callback reports success with partial bytes.

### Implication

Any caller using `read_async` at offsets beyond what was written may receive partial or zero-length reads. The hybrid log allocator and recovery code must account for this if they rely on zero-initialized unwritten regions.

### Recommendation

If FASTER's higher layers depend on unwritten regions returning zeros, the UringDevice should add a zero-fill step in the completion handler for short reads (matching the sync path behavior). This is a future enhancement — the current behavior is correct per the Device trait contract which makes no promise about unwritten regions.

---

# Decision: Batch Operation API Design (A12)

**Author:** Aragorn (Rust Expert)  
**Date:** 2025-07-25  
**Status:** Implemented  

## Context

FASTER's session API performs epoch enter/exit on every individual operation.
For bulk workloads, this per-operation overhead dominates. We needed a batch
API that amortizes epoch protection and leverages hash bucket prefetching.

## Decision

**Slice-based API** with `impl UnsafeContext` batch methods in a dedicated
`store::batch` module.

### API Surface

| Method | Signature |
|--------|-----------|
| `batch_read` | `(&self, session, keys, outputs) -> BatchResult` |
| `batch_upsert` | `(&self, session, keys, inputs) -> BatchResult` |
| `batch_rmw` | `(&self, session, keys, inputs, outputs) -> BatchResult` |
| `batch_delete` | `(&self, session, keys) -> BatchResult` |
| `batch_execute` | `(&self, session, ops, outputs) -> BatchResult` |

All methods also available directly on `UnsafeContext` for manual epoch control.

### Key Design Choices

1. **Slice-based over builder pattern**: More idiomatic Rust, zero allocation
   overhead, natural borrow semantics for parallel key/value slices.

2. **UnsafeContext impl in batch.rs**: Keeps batch logic self-contained.
   Required making `UnsafeContext.session` field `pub(super)`.

3. **No atomicity**: Each operation independently succeeds/fails. BatchResult
   tracks per-operation OperationStatus with aggregate helpers.

4. **BATCH_REFRESH_INTERVAL = 256**: Epoch refresh every 256 ops. ~50ns per
   refresh, amortized to < 0.2ns per op.

5. **Hash-prefetch-all then execute-all**: Simple two-phase approach. All hash
   buckets prefetched before any operations execute.

## Alternatives Considered

- **Builder pattern**: More flexible but heavier API, unnecessary allocation.
- **Batch methods directly in session.rs**: Would bloat an already large file.
- **Async/stream API**: Premature; batch API covers the primary use case.

## Consequences

- New public types: `BatchOp`, `BatchResult` (re-exported at crate root)
- `UnsafeContext.session` field changed from private to `pub(super)`
- All batch methods require `F::Context: Default`

---

# Decision: FFI Callback Function Pointers Architecture

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Tasks:** W2-06 through W2-11

## Context

The C++ wrapper feasibility analysis (Saruman) identified that FASTER's FFI layer lacked callback function pointers for RMW, Upsert, and Read operations. Without these, C++ consumers cannot customize operation semantics — they're stuck with byte-slice replacement.

## Decision

Use thread-local storage with RAII guards to dispatch C function pointers during `_ex()` FFI calls.

### Why Thread-Local?

1. **Sessions are thread-affine** — thread-local is the natural scope
2. **Functions trait takes `&self`** (immutable) — can't store mutable callback state in the struct
3. **Raw C function pointers aren't Send+Sync** — thread-local avoids the need for these bounds
4. **RAII guards** ensure cleanup even on panic, preventing callback leaks

### Why CallbackFunctions replaces ByteSliceFunctions?

Rather than maintaining two separate Functions implementations, CallbackFunctions serves both roles:
- When thread-local callbacks are installed (by `_ex()` functions): dispatches to C callbacks
- When no callbacks installed: falls back to byte-slice replacement (same as ByteSliceFunctions)

This means `faster_rmw()` and `faster_rmw_ex()` operate on the same store instance seamlessly.

## Alternatives Considered

1. **Store callbacks in the Functions struct** — Can't; trait takes `&self` (immutable)
2. **Use Arc<Mutex<>> for callback state** — Overhead; unnecessary since sessions are single-threaded
3. **Separate store types for callback vs non-callback** — Would require separate handles, sessions, and stores

## Status

Implemented on branch `aragorn/ffi-callbacks`. 102 tests pass (83 original + 19 new).

---

# Decision: C FFI Layer — Thread-Affinity & Missing Functions

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Status:** Implemented

## Context

The FFI layer (`faster-ffi`) had `unsafe impl Send + Sync` on `SessionCell` 
with no runtime enforcement — the safety contract was purely documentary 
(C caller must not cross threads). Three FFI functions were also missing 
from the spec.

## Decision

### Thread-Affinity (FFI-02)
Record the creating thread's `ThreadId` in `SessionCell` and check it on 
every CRUD operation via `with_store_session()`. Return `ThreadMismatch = 105` 
on violation instead of silently invoking UB.

**Trade-off:** One `std::thread::current().id()` call per FFI operation 
(~5ns overhead). This is negligible compared to the epoch refresh and I/O 
that follows.

### Missing Functions
- `faster_destroy()` — alias for `faster_close()` (C API convention)
- `faster_session_refresh()` — epoch refresh + pending completion
- `faster_wait_for_all_pending()` — synchronous drain of all pending I/O

### Unwrap Audit (Part 3)
Audited `store/functions.rs` — all unwraps are in `#[cfg(test)]` code only. 
No production unwraps found. Audit finding was a false positive.

## Alternatives Considered

1. **Compile-time enforcement only:** Not possible across FFI boundary; 
   C has no type system to prevent cross-thread handle passing.
2. **`thread_local!` session storage:** Would change the API contract 
   (sessions become implicit). Rejected for compatibility with C patterns.
3. **`log` crate for thread-mismatch warnings:** Considered logging instead 
   of returning an error. Rejected because silent UB is worse than a 
   deterministic error code.

## Impact

- `FasterStatus` enum gains `ThreadMismatch = 105` (ABI addition, not breaking)
- `faster.h` header regenerated with new functions and enum value
- 83 tests pass (13 new), clippy clean with `--all-targets -D warnings`

## Follow-Up

- Config handle API (`faster_config_*`) not yet implemented — store works 
  with `FasterKvConfig::default()`. Can be added when C callers need 
  custom configuration.

---

# Decision: Key::hash_from_bytes trait method

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Status:** Implemented
**Impact:** Key trait API, compaction performance

## Decision

Added `fn hash_from_bytes(buf: &[u8]) -> KeyHash` to the `Key` trait with a default implementation that deserializes and hashes. Optimized overrides provided for all built-in types.

## Rationale

During compaction, `AddressUpdater::swing_one()` and `remove_tombstone()` were deserializing keys just to compute their hash for hash-index lookups. For variable-length types (Vec<u8>, String), this allocates on the heap unnecessarily. The new method enables zero-copy hash computation directly from serialized bytes.

## API Pattern

Follows the established `*_from_bytes` pattern on the Key trait:
- `serialized_size_from_bytes` — size without deserialization
- `eq_from_bytes` — comparison without deserialization
- `hash_from_bytes` — hashing without deserialization (NEW)

All three have default implementations that deserialize (correct but slow), with optimized overrides for built-in types.

## Implications

- **Custom Key implementors:** No action needed — default impl provides correctness. Override for performance if the type is used with compaction.
- **Compaction code:** Now uses `K::hash_from_bytes()` instead of `K::deserialize().hash()` in pointer swing and tombstone removal.
- **Performance:** Zero-copy for variable-length types eliminates heap allocation per record during compaction scan.

---

# Decision: Mutation Testing Strategy for FASTER Rust

**Author:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** Proposed
**Impact:** Testing quality, release readiness

---

## Summary

Use `cargo-mutants` (v27.0.0) for mutation testing across the FASTER Rust workspace. Run a 2–3 week phased campaign before public release to identify test suite gaps, targeting ≥80% kill rate overall and ≥85% for critical modules (store, compaction, hash).

## Key Decision Points

### 1. Tool: `cargo-mutants` ✅

- Only viable option. mutagen is unmaintained and requires nightly + source annotations.
- Works with our stable toolchain, edition 2024, MSRV 1.85.0.
- Zero source modifications required.
- Installed and validated: v27.0.0 runs correctly on our workspace.

### 2. Scope: 3,307 mutants (excluding samples)

- 4,454 total workspace mutants; 1,147 are in sample crates (skip).
- faster-core alone has 2,598 mutants across 10 modules.
- Prioritize by risk: store (416) → compaction (200) → hash (250) → hybrid_log (369) → epoch (72).

### 3. Unsafe Code Policy: Accept Timeouts as "Tested"

- Mutating arithmetic/bitwise ops inside unsafe blocks creates UB → segfaults/hangs → timeouts.
- Observed: 8/53 mutations in address.rs timed out (all unsafe-adjacent bitwise ops).
- These are NOT test gaps. Classify timeouts in unsafe code as acceptable.
- Atomic ordering correctness must be verified separately (loom/Miri, not mutation testing).

### 4. Kill Rate Target: 80% overall, 85% critical modules

- 90%+ is diminishing returns for a systems crate with significant unsafe code.
- Equivalent mutants (Display, logging, capacity hints) inflate survivors without indicating real gaps.
- Focus effort on zero missed mutants in safety-critical paths.

### 5. CI Integration: Weekly + Per-PR (Advisory Only)

- Weekly full run sharded across 4 CI workers (~30–60 min wall-clock).
- Per-PR: `--in-diff` on changed files only (~5–15 min).
- Advisory, not blocking. Don't gate merges on mutation testing.

### 6. Timeline: 2–3 Weeks Pre-Release

- ~30–50 hours total compute time across all phases.
- Human effort: ~50% running/triaging, ~50% writing targeted tests.

## Decisions Needed from Team

1. **Approve timeline:** Can we allocate 2–3 weeks before release for this campaign?
2. **CI budget:** Do we want weekly mutation testing in CI? Requires 16-core runners.
3. **Kill rate target:** Is 80%/85% the right bar, or should we aim higher for a v1.0?
4. **Ownership:** Should this be Aragorn-led or distributed across the team?

## Full Plan

See `.squad/plans/mutation-testing-plan.md` for the complete plan with phased execution, operational playbook, and experimental results.

---

# Decision: Precheckin Nextest Must Not Use --all-targets

**Author:** Aragorn  
**Date:** 2026-03-09  
**Status:** Implemented (commit 74ce041f)

## Context

The precheckin script (`rust/scripts/precheckin`) used `cargo nextest run --workspace --all-targets`, which includes Criterion bench binaries. Criterion's custom `main()` does not understand nextest's `--list --format terse` flag and enters an infinite benchmark loop, blocking all precheckin runs indefinitely.

## Decision

Remove `--all-targets` from the nextest invocation in precheckin. The clippy gate already runs with `--all-targets`, which verifies bench code compiles. Nextest only needs to run `--workspace` (lib + bin + test targets).

## Consequences

- Precheckin now completes in ~45 seconds instead of hanging forever.
- Bench code is still compile-checked by clippy.
- If a future Criterion version supports `--list`, we can re-add `--all-targets`.

## Related

- Root cause of formatting drift: nightly-only `imports_granularity`/`group_imports` in rustfmt.toml silently degrade on stable toolchain. All agents should use nightly rustfmt or the same toolchain version.

---

# Decision: Checkpoint-Based Testing Pattern for Compaction & Hybrid Log

**Author:** Boromir (QA)
**Date:** 2025-06-24
**Status:** Proposed

## Context

Testing the compaction pipeline and hybrid log management requires data in the read-only region. With 32MB pages and small test records (~24 bytes), natural region advancement via `maintenance()` is impractical — you'd need ~1.4M records to fill a single page.

## Decision

Use `checkpoint(tempdir, CheckpointType::FoldOver)` as the canonical method to force data into the read-only region for integration tests.

### Why This Works

`checkpoint()` internally calls `shift_read_only_to_tail()` + synchronous flush, but does NOT evict pages from memory. Pages end up in `Flushed` state — still readable by the compaction scanner.

This is superior to alternatives:
- `flush_and_evict()` evicts pages, making them unreadable by the scanner
- `maintenance()` requires filling entire 32MB pages before it shifts RO
- Directly calling `shift_read_only_to_tail()` is impossible — `allocator` is `pub(crate)`

### Helper Pattern

```rust
fn force_read_only_u64(store: &FasterKv<U64Key, U64Value, InMemoryDevice>) {
    let dir = tempfile::tempdir().unwrap();
    store.checkpoint(dir.path(), CheckpointType::FoldOver).unwrap();
}
```

## Impact

- Enables realistic compaction testing (K1→K4 pipeline) with small record counts
- Enables hybrid log region boundary testing without massive datasets
- Pattern should be reused by any future tests that need read-only region data

## Risks

- Depends on `checkpoint()` internal behavior (shift RO + no eviction) — if that changes, tests break
- Creates temp directories (cleaned up automatically by `tempfile`)

---

# Decision: SF-3/5/8/9 Compaction Scanner Runtime Warnings & Tests

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-07
**Status:** Implemented
**Commit:** 4c11175b

## Summary

Implemented 4 should-fix items from the final review synthesis of the variable-length compaction feature. These add runtime warning instrumentation and edge-case test coverage to the compaction scanner.

## Changes

### SF-3: Tombstone vector memory warning
- Warns via `log::warn!` when tombstone_records exceeds ~100 MB
- Checked every 1024 pushes (amortized cost)

### SF-5: Hash collision hop metrics  
- `is_current_version()` now returns `(bool, usize)` with hop count
- New `total_chain_hops` field on `CompactionPlan`
- Warns when cumulative hops exceed 1,000,000

### SF-8: Post-scan byte accounting validation
- Scanned bytes vs address-range span check (>5% threshold)
- Classified bytes vs total bytes exact equality check

### SF-9: Five new edge-case tests
- `record_nearly_fills_page` — record leaving <MIN_READABLE gap
- `moderate_corruption_plausible_wrong_size` — bit-flip corruption detection
- `version_chain_variable_length_different_sizes` — multi-depth chains with Vec<u8>
- `empty_key_record_size` — zero-length key handling
- `eq_from_bytes_trailing_garbage` — trailing bytes and boundary conditions

## Design Decisions

1. **`log` crate over `tracing`:** Added `log` as non-optional dependency. These are safety warnings that should always emit regardless of feature flags. `tracing` bridges to `log` so no conflict.

2. **`(bool, usize)` return type:** Breaking internal API change to `is_current_version()`. All call sites updated. Conservative — returns 0 hops for fast-path and early-exit cases.

3. **5% threshold for scan-vs-range check:** Generous because page-end gaps cause legitimate divergence. Exact equality for classified-bytes check.

## Risks

- `log::warn!` in hot scan path — negligible cost when no log subscriber is active (log facade compiles to no-op).
- Extra `usize` fields on `CompactionPlan` — trivial memory overhead. `#[non_exhaustive]` makes this non-breaking.

---

### 2026-03-08T21:01: User directive — Pre-release quality prioritization
**By:** qbradley (via Copilot)
**What:** Three-phase quality approach before release:
1. FIRST: Fill known test gaps (compaction, loom, miri, memory pressure)
2. THEN (parallel): Mutation testing to find remaining gaps + FoundationDB-style deterministic simulation testing (use PAW workflow for simulation, it's a big project)
3. AFTER: Reassess before implementing other work items (fault injection, release management, etc.)

Mutation testing needs a plan covering technology choice, execution strategy, and operational process (iterative, when to stop, how to improve hit rate). Simulation testing via PAW workflow or broken into phases with PAW per phase. Benchmarks never run locally — always VM via sub-agent.
**Why:** User request — strategic prioritization for pre-release quality gate

---

# Decision: read-cache-sim-tokio — Async Benchmark Sample

**Agent:** Elrond (Tokio/Async Expert)
**Date:** 2026-03-07
**Status:** Implemented — committed on `squad` branch
**Impact:** Performance comparison, async integration patterns

## Decision

Created `read-cache-sim-tokio` as an async/Tokio mirror of the existing sync `read-cache-sim` sample, using `spawn_blocking` for workers instead of `AsyncSession`.

## Rationale

1. `AsyncSession` is `!Send` due to thread-affine epoch protection — cannot cross Tokio task boundaries.
2. `spawn_blocking` with direct `FasterKv` sessions keeps data-plane identical to sync, enabling fair comparison.
3. `TokioFileDevice` + `AsyncFasterKv` demonstrate the async control-plane benefits (maintenance, shutdown, stats) without penalizing throughput.
4. Pattern is reusable: any CPU-bound FASTER workload on Tokio should use `spawn_blocking` + short-lived sessions.

## Follow-up

- Run extended benchmarks comparing sync vs. tokio versions on real storage devices
- Document the `spawn_blocking` pattern in the crate-level docs for `faster-tokio`
- Consider a channel-based worker pool pattern for production use (long-lived sessions, fewer session create/destroy cycles)

---

# Decision: CI Fuzz Integration (Iteration 6 A7)

**Agent:** Éowyn (Deterministic Simulation Testing Expert)
**Date:** 2026-03-09
**Status:** DECIDED
**Impact:** CI pipeline — PR validation and nightly testing

## Decision

Fuzz testing is now CI-ready via two scripts (`fuzz-ci`, `fuzz-minimize`) with a checked-in seed corpus and comprehensive documentation.

## Key Choices

### 1. Smoke mode duration: 10 seconds per target

**Rationale:** 10s gives libFuzzer enough time to load the corpus and execute millions of iterations for simple targets (record parsing: ~7M runs, record layout: ~8.5M runs) and hundreds for complex targets (store ops: ~300 runs). Total wall-clock for all 5 targets is ~60s, acceptable for PR validation.

**Alternative considered:** 5s per target was too short for store ops to exercise meaningful code paths.

### 2. Seed corpus tracked in Git

**Rationale:** Reproducibility is paramount for fuzz testing. Checked-in corpus means every CI run starts from the same baseline. Fuzzer-discovered inputs from smoke runs are also included for richer initial coverage.

**Trade-off:** Adds ~100 small binary files to the repo. Corpus can be minimized with `fuzz-minimize` to keep size manageable.

### 3. Artifact directory separate from corpus

**Rationale:** Crash artifacts (`artifacts/`) are runtime-only investigation aids, not reproducible seeds. Keeping them gitignored prevents accidental commit of crash files that may contain sensitive memory contents.

### 4. Scripts support shorthand target names

**Rationale:** `--target hash` is easier to type than `--target fuzz_hash`. The `fuzz_` prefix is auto-prepended when missing. This reduces friction for developers investigating specific targets.

## Implications

- **Frodo:** Wire `./scripts/fuzz-ci --smoke` into PR validation (e.g., Azure Pipelines `pr:` trigger or GitHub Actions `pull_request` event). Wire `FUZZ_DURATION=300 ./scripts/fuzz-ci` into nightly schedule.
- **All agents:** When adding a new fuzz target, update `ALL_TARGETS` in both `fuzz-ci` and `fuzz-minimize`, add corpus seeds, and update `rust/fuzz/README.md`.
- **Galadriel:** Fuzz targets exercise unsafe-adjacent code paths. Any new unsafe code should have a corresponding fuzz target.

## Pre-existing Issues Noted

- `cargo clippy` fails on `faster-ffi` (29 errors: `undocumented_unsafe_blocks`) and `faster-core` (`clone_on_copy`). These pre-date this work and blocked `precheckin` from passing. Only fuzz-related files were committed.

---

# Decision: Fuzz Testing Infrastructure Design

**Agent:** Éowyn (Deterministic Simulation Testing Expert)  
**Date:** 2026-03-08  
**Status:** IMPLEMENTED  
**Impact:** Testing infrastructure — affects CI pipeline and quality assurance

## Decision

Implemented cargo-fuzz testing framework at `rust/fuzz/` with 5 fuzz targets, a CI runner script, and documentation.

## Key Design Choices

### 1. Standalone Workspace for Fuzz Crate

The fuzz crate declares its own `[workspace]` in `Cargo.toml` rather than relying on `exclude = ["fuzz"]` in the parent workspace. This provides build isolation and avoids conflicts with concurrent workspace modifications.

### 2. Fuzz Target Selection

Five high-value attack surfaces were selected:
- **Record parsing** — catches deserialization panics on malformed input
- **Record layout** — validates arithmetic safety in size/offset computations  
- **Compaction record size** — ensures graceful corruption handling (SF-8 related)
- **Hash functions** — verifies no-panic guarantee for all Hashable types
- **Store operations** — random CRUD sequences catch stateful bugs

### 3. String Deserialization Excluded from Arbitrary Fuzzing

`String::deserialize` panics by design on invalid UTF-8. The fuzz target validates UTF-8 before calling it, focusing fuzzer effort on discovering *unexpected* panics rather than exercising known-panic paths.

### 4. Store Ops Resource Limits

The store operations target constrains hash table size (4-14 log2) and operation count (≤2048) to avoid fuzzer timeouts while still achieving meaningful coverage.

## Implications

- **CI Integration:** Add `./scripts/fuzz` to CI pipeline with appropriate duration (60s default for fast CI, 300s+ for nightly)
- **New Targets:** Anyone can add targets by creating `fuzz_targets/fuzz_<name>.rs` and registering in Cargo.toml
- **Nightly Required:** Fuzz builds require nightly Rust toolchain
- **No Precheckin Impact:** Fuzz crate is fully excluded from `./scripts/precheckin`

---

# Decision: Loom Tests Re-implement Algorithms (Not Wrapping Production Types)

**Agent:** Éowyn (Deterministic Simulation Testing Expert)
**Date:** 2026-03-10
**Status:** Implemented
**Impact:** Testing strategy — affects how loom tests are maintained

## Decision

Loom tests re-implement FASTER's concurrent algorithms from scratch using `loom::sync::atomic` primitives, rather than wrapping the production types.

## Rationale

1. Production code uses `std::sync::atomic` directly throughout — the `crate::sync` loom shim exists in `sync.rs` but is not wired into any production types.
2. Wiring loom into production code is a large refactor touching ~25 files with `use std::sync::atomic`.
3. The existing 7 loom tests already use this re-implementation pattern, establishing precedent.
4. Re-implementations faithfully mirror exact bit layouts, orderings, and protocols from production.

## Implications

- **When `crate::sync` is wired in:** All D-series loom tests can be updated to use actual production types, eliminating the re-implementation layer. This is the preferred future state.
- **Maintenance burden:** Any change to production atomic orderings or CAS protocols must be manually reflected in loom tests until the shim is wired in.
- **Coverage gap:** Loom tests verify the *algorithm* but not the *exact production code path*. Bugs introduced by typos in production (e.g., wrong constant value) would not be caught.

## Next Steps

- [ ] Wire `crate::sync` re-exports into production code (replaces `use std::sync::atomic` in ~25 files)
- [ ] Update loom tests to use production types directly once shim is active

---

# Decision: Miri Test Coverage Standard for Unsafe Code

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-08
**Status:** Recommendation
**Impact:** Testing discipline — all unsafe code paths

## Decision

Every module containing `unsafe` code blocks in `faster-core` must have
corresponding Miri tests that exercise the unsafe paths. The Miri test suite
(`miri_tests.rs`) is the safety net that catches undefined behavior the
compiler cannot.

## Coverage Achieved

| Module | Miri Status | Tests |
|--------|------------|-------|
| `allocator.rs` | ✅ Covered | 7 tests |
| `buffer_pool.rs` | ✅ Covered | 2 tests |
| `device.rs` | ✅ Covered (partial — callback paths only) | 2 tests |
| `hash/bucket.rs` | ✅ Covered | 11 tests |
| `hash/table.rs` | ✅ Covered | 4 tests |
| `hash/prefetch.rs` | ✅ Covered (no-op intrinsics) | 1 test |
| `hash/index.rs` | ✅ Covered (via compaction tests) | — |
| `hybrid_log/page.rs` | ✅ Covered | 1 test |
| `hybrid_log/log_allocator.rs` | ✅ Covered | 1 test |
| `hybrid_log/record_ops.rs` | ✅ Covered | 4+4 tests |
| `hybrid_log/scan.rs` | ✅ Covered | 4 tests |
| `epoch/drain.rs` | ✅ Covered (indirect via EpochTable) | 3 tests |
| `compaction/scanner.rs` | ✅ Covered | 3 tests |
| `compaction/copier.rs` | ✅ Covered | 3 tests |
| `compaction/address_update.rs` | ✅ Covered | 1 test |
| `store/operations.rs` | ✅ Covered (via FasterKv CRUD) | 6 tests |
| `store/functions.rs` | ✅ Covered (via FasterKv CRUD) | — |
| `record/record_info.rs` | ✅ Covered (atomics) | 4 tests |
| `state/mod.rs` | ✅ Covered (EPVS packing) | 4 tests |
| `recovery/index_recovery.rs` | ❌ Skipped — requires file I/O | — |
| `hybrid_log/flush.rs` | ❌ Skipped — async device I/O | — |
| `store/pending_io.rs` | ❌ Skipped — async device callbacks | — |
| `sync_file_device.rs` | ❌ Skipped — file system operations | — |

## Modules That Cannot Be Tested Under Miri

These require real file I/O or async device callbacks that Miri cannot execute:

1. **recovery/index_recovery.rs** — Reads bucket data from checkpoint files
2. **hybrid_log/flush.rs** — Async page flush with device callbacks
3. **store/pending_io.rs** — I/O completion callback reconstruction
4. **sync_file_device.rs** — Direct file system operations

**Alternative:** These should be covered by proptest fuzzing and integration
tests running under AddressSanitizer (`-Zsanitizer=address`).

## Rationale

Miri catches classes of bugs that no other tool reliably detects in safe
Rust tests: use-after-free, uninitialized reads, alignment violations,
invalid pointer arithmetic, and provenance violations. With 71 Miri tests
running in ~42s, this is a practical safety net for CI.

---

# Decision: EPVS (Epoch-Protected Version Scheme) Architecture

**Author:** Gandalf  
**Date:** 2026-03-06  
**Task:** W2-03  
**Status:** Proposed  

## Key Architectural Decisions

### D-EPVS-1: Pack (phase: u8, version: u56) into AtomicU64

**Decision:** System state is a single 64-bit atomic word with phase in bits 56–63 and version in bits 0–55. All state machine transitions are single CAS operations.

**Rationale:** Eliminates torn reads between phase and version. Matches C# FASTER's SystemState layout exactly. Reduces atomic operation count per checkpoint cycle.

**Impact:** Replaces `CheckpointStateMachine::phase (AtomicU8)` and `HashIndex::version (AtomicU32)` with a shared `AtomicSystemState`.

### D-EPVS-2: Unified Phase Enum

**Decision:** Merge `CheckpointPhase` and `GrowPhase` into a single `Phase` enum. Checkpoint phases: 0–7. Grow phases: 8–15. Bit 7 (0x80) of the phase byte is the INTERMEDIATE marker.

**Rationale:** A single state word can only hold one phase, so grow and checkpoint share the same word. This enforces mutual exclusion (only one active at a time), matching C# behavior.

**Impact:** Grow and checkpoint cannot run concurrently. This is the correct design for now; P-09 (composable tasks) can relax this later.

### D-EPVS-3: Two-Phase Intermediate Transition Protocol

**Decision:** Every state transition uses `current → intermediate → next` with two CAS operations. The intermediate state (phase bit 7 set) blocks other threads from observing inconsistent mid-transition state.

**Rationale:** Matches C# FASTER's `MakeTransition` pattern. Provides a safe window for before-entering hooks (epoch bumps, metadata capture) without races.

### D-EPVS-4: Checkpoint Metadata Format Version

**Decision:** Add `format_version` field to `IndexRecoveryInfo` and `LogRecoveryInfo`. New format (version 2) stores version as `u64`. Recovery code detects legacy format (no `format_version`) and widens `u32 → u64`.

**Rationale:** Soft migration — no hard break. New code reads old checkpoints. Old code cannot read new checkpoints (acceptable — no downgrade support).

### D-EPVS-5: RecordInfo Unchanged

**Decision:** The 13-bit per-record version field in RecordInfo is NOT changed in the EPVS PR. It will be addressed in P-22 / W2-04 after EPVS stabilizes.

**Rationale:** Minimizes blast radius. RecordInfo changes affect every record in every log page and every operation path. Decoupling keeps each PR reviewable and testable.

## Binding For

- **Sam:** Implement per this design. Module structure: `src/state/{mod,phase,system_state}.rs`.
- **All:** The unified `Phase` enum replaces both `CheckpointPhase` and `GrowPhase`.
- **Frodo:** W2-04 (RecordInfo) depends on EPVS landing first.

---

# Decision: Hash Table Layout — Keep Multi-Slot Buckets

**Agent:** Gandalf (Lead Architect)
**Date:** 2026-03-06
**Status:** DECIDED — Research task A8 complete
**Impact:** Hash index architecture — confirmed current approach is correct

## Decision

Keep the current multi-slot bucket design. Do NOT pursue open addressing with inline data.

## Key Finding

The hypothesis that C# uses "open addressing with inline keys" was **incorrect**. Both C# and Rust FASTER use identical hash table architectures: 64-byte cache-line-aligned buckets with 7 data entries, each storing an 8-byte packed word (14-bit tag + 48-bit logical address). Neither stores keys or values inline.

## Rationale

1. True open addressing (inline keys/values) is fundamentally incompatible with FASTER's hybrid log architecture, which requires records to flow through mutable → read-only → disk tiers via address indirection.
2. The ~14% Workload C read gap lives in the record access path (page table translation, physical address resolution, key comparison), NOT in the hash index.
3. Changing the hash index would be high risk for zero gain on the actual bottleneck.

## Next Steps

1. Profile full read path with `perf record` to identify actual cycle distribution
2. Benchmark record access isolation (logical address → value bytes)
3. Evaluate `Ordering::Relaxed` for read-path loads behind `#[cfg(target_arch = "x86_64")]`

## Artifacts

- `.squad/agents/gandalf/hash-table-layout-analysis.md`
- `rust/crates/faster-core/benches/hash_layout_bench.rs`

---

# Decision: Iteration 6 Merge Consolidation

**Author:** Gandalf  
**Date:** 2026-03-06  
**Status:** Executed  

## Context

All 9 Iteration 6 feature branches needed to be consolidated into `squad` before the Quality Wave.

## Decisions Made During Merge

### 1. Batch API Documentation: Aragorn's style over Legolas's

When resolving the kv.rs conflict between `legolas/two-level-prefetch` and `aragorn/batch-api`, took Aragorn's version with full doc comments over Legolas's terse one-liners. The public API should be well-documented regardless of which optimization (two-level prefetch) powers it internally.

### 2. Revivified FFI Mapping: InPlaceUpdated

The `OperationStatus::Revivified` variant (from sam/revivification) was not in aragorn's batch-api branch. Mapped it to `FasterStatus::InPlaceUpdated` in the FFI layer since revivification is semantically an in-place update of a sealed record. If C callers need to distinguish revivification, a new FFI status variant should be added.

### 3. Eowyn's fuzz CI subsumption

`eowyn/ci-fuzz-integration` was already reachable from `legolas/disk-io-bench-fix` (shared lineage). No separate merge needed. This is fine — the commits are correctly attributed.

## Impact

- `squad` branch now contains all Iteration 6 features
- 1,456 tests pass, 0 failures
- Ready for Quality Wave

---

# Decision: Cross-Implementation Performance Baseline Established

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Status:** Active

## Context

Ran apples-to-apples YCSB benchmarks across Rust, C#, and C++ FASTER implementations.

## Key Numbers (16T, in-memory, 2.5M keys, uniform)

| Workload | Rust | C# | C++ |
|----------|------|-----|-----|
| A (50/50 R/W) | **39.21M** | 31.02M | 38.91M |
| B (95/5 R/W) | 41.37M | **49.56M** | 45.78M |
| C (100% Read) | 41.33M | **51.65M** | 48.07M |
| F (50/50 R/RMW) | 38.15M | 42.46M | **42.52M** |

## Decision

1. **Hash index prefetching** is the #1 optimization priority — closes the 40-77% single-thread gap with C#/C++.
2. Rust's write scaling is already best-in-class (102% efficiency at 16T on Workload A).
3. C# and C++ numbers provide concrete targets for future optimization work.

## Impact

- **Aragorn**: Hash index prefetching should be the next implementation task.
- **All**: Performance claims in docs/API must reference these measured numbers, not estimates.
- **Gandalf**: Architecture allows for prefetching — no structural changes needed.

---

# Decision: Disk I/O Benchmark Methodology Fix

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Branch:** `legolas/disk-io-bench-fix`
**Addresses:** Iteration 6 Action Items A2, A11

## Context

The W3-02 disk I/O benchmark showed 0% pending rate across all workloads with buffer_pages=128 on a 32GB VM. This means every operation completed synchronously from the in-memory portion of the hybrid log — we were measuring in-memory ceiling performance and mislabeling it as "disk I/O."

## Decisions

### D1: Default buffer_pages reduced from 128-256 to 16-32

Rationale: With 10M keys x 1KiB values, the dataset is ~10GB. A 128-page buffer (4MB) still allows the hybrid log to keep hot data in-memory. Reducing to 16 pages (512KB) creates a 20,000:1 dataset-to-buffer ratio, making disk reads far more likely once the OS page cache is cold.

### D2: I/O Classification Thresholds

- `pending_rate < 0.1%` → IN-MEMORY BASELINE
- `0.1% <= pending_rate < 5%` → MIXED
- `pending_rate >= 5%` → DISK-BOUND

These thresholds are empirically motivated. Anything below 0.1% means virtually no disk ops occurred. The 5% threshold reflects meaningful disk participation.

### D3: Statistical Rigor — 5 iterations, t-distribution CI

Default 5 iterations with mean, sample stddev, and 95% confidence interval using t-distribution critical values. HIGH VARIANCE flag at stddev > 10% of mean. This aligns with standard benchmarking practice (e.g., SPECjbb, MLPerf).

### D4: --force-disk Auto-Tuning

Starts at buffer_pages=16, halves until pending_rate > 0.1%. Uses short 5s probe runs to minimize auto-tune wall time. Reports final effective buffer_pages so results are reproducible.

## Impact

- **All future disk I/O benchmarks** should use the new defaults or --force-disk mode
- **Previous W3-02 results** should be relabeled as "IN-MEMORY BASELINE" in any comparison tables
- **CI benchmark jobs** should include --iterations 3 at minimum for regression detection

## Files Changed

- `rust/crates/samples/disk-io-bench/src/main.rs` — CLI args, multi-iteration, force-disk, validation gate, output formats
- `rust/crates/samples/disk-io-bench/src/stats.rs` — IterationSummary with mean/stddev/CI
- `rust/crates/samples/disk-io-bench/src/workload.rs` — Reduced default buffer_pages, documented memory budget

---

# Decision: Disk I/O Benchmark Suite

**Agent:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Status:** Implemented — ready for VM execution
**Impact:** Performance measurement infrastructure

## Decision

Created `disk-io-bench` — a dedicated benchmark binary for comparing FASTER's
three I/O device implementations (SyncFileDevice, UringDevice, TokioFileDevice)
under disk-bound workloads with both sync and Tokio client modes.

## Rationale

1. Our existing cross-impl-bench uses NullDevice (in-memory only) — it cannot
   measure disk I/O performance at all.
2. We need head-to-head device comparison data to validate architectural
   decisions (io_uring investment, Tokio adapter overhead, thread pool sizing).
3. The benchmark VM (20.59.58.117) has NVMe local storage ideal for this.

## What Was Built

- **Crate:** `rust/crates/samples/disk-io-bench/` (binary, not library)
- **Design doc:** `.squad/agents/legolas/disk-io-benchmark-design.md`
- **Runner script:** `rust/scripts/disk-io-bench-matrix.sh`

### Benchmark Matrix
- 3 devices × 2 client types × 4 workloads × 4 thread counts = 96 configurations
- Workloads: overcommit (cold reads), write-heavy (flush pressure), mixed (Zipfian), scan (sequential)
- Output: table, JSON, or CSV with ops/sec, latency percentiles, pending rate

### CLI Interface
```
disk-io-bench --device uring --client tokio --workload mixed --threads 8
disk-io-bench --run-all --output json --output-file results.json
```

## Pre-existing Build Issues Fixed

While implementing, found and fixed pre-existing compilation errors:
- `CompactionPlan` struct initializer missing 3 new fields in `scanner.rs`
- Duplicate lines in `drain.rs` (double `let mut drained`, double `fetch_sub`)
- Added `#[allow(dead_code)]` for `DrainList::has_pending()` (not yet called)

## Next Steps

- [ ] Run full matrix on bench VM (20.59.58.117) with NVMe storage
- [ ] Compare io_uring vs sync device overhead
- [ ] Use results to prioritize device optimization work
- [ ] Consider adding O_DIRECT comparison on VM

---

# Decision: Criterion Benchmarks Must Use Custom main() for Nextest Compatibility

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Status:** Implemented

## Context

All three criterion bench binaries (`hash_layout_bench`, `ycsb`, `core_benchmarks`) used `criterion_main!()` which runs full statistical benchmarks when invoked by `cargo nextest run --all-targets`. The `hash_layout_bench` alone took 12+ minutes, blocking every precheckin run.

## Decision

**All `harness = false` criterion benchmarks MUST use a custom `main()` instead of `criterion_main!()`.**

Pattern:
```rust
criterion_group!(benches, bench_foo, bench_bar);

fn main() {
    if std::env::args().any(|a| a == "--bench") {
        benches(); // Full criterion run (cargo bench)
    } else {
        // Optional smoke test for nextest/cargo test
    }
}
```

## Rationale

- `criterion_main!()` does not implement nextest's test discovery protocol
- When nextest invokes the binary without `--bench`, criterion defaults to full benchmark mode
- Full mode runs 100 samples × auto-tuned iterations × warmup for EVERY benchmark function
- This is a category error: nextest wants a quick pass/fail test, not a 12-minute statistical analysis

## Impact

- Precheckin time: 12+ minutes (hanging) → 38 seconds
- `cargo bench` continues to work identically
- Any new bench files must follow this pattern

## Who Needs to Know

- **All agents adding benchmarks:** Follow the custom `main()` pattern
- **Aragorn:** Precheckin is unblocked

---

