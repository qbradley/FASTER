# Squad Decisions

## Active Decisions

### 1. Rust Async Model Should Use async/await, Not Callback Chains

**Agent:** Grievous (C++ Expert)  
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
- [ ] Thrawn: Validate architecture alignment
- [ ] Cassian: Confirm no blockers for implementation

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
- **Overrides Decision #1** (Grievous async/await recommendation)

**Override Resolution:**
- Grievous recommended async/await for ergonomics
- User directive takes precedence: core must remain async runtime-agnostic
- Async ergonomics achieved via adapter layer (Kenobi responsibility)
- See Decision #5 (Async Integration Strategy) for bridge pattern

---

### 3. Async Integration: Callback Bridge to Future Pattern

**Agent:** Thrawn (Lead Architect, Part 2)  
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
- Kenobi leads async adapter design and implementation
- Zero performance cost for sync path (direct callback)
- Minimal overhead for async path (channel or Waker call)
- Testing: adapter contracts verified against mock I/O

---

### 4. Rust Implementation Scope: MVP → Full → Deferred Phases

**Agent:** Cassian (Cross-Implementation Expert)  
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

**Agent:** Dooku (C# Expert)  
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

**Original Agent:** Grievous (C++ Expert)  
**Original Date:** 2026-03-05  
**Original Status:** Recommendation for Team Discussion  
**Override Date:** 2026-03-05T17:10:00Z  

**What Changed:**
- Grievous recommended async/await (Decision #1)
- User directive (qbradley) overrides: core must be callback-based, no async runtime
- See Decision #2 for full directive rationale

**Resolution Path:**
- **Core:** Callback-based per user directive (non-negotiable)
- **Ergonomics:** Async adapter crates (Decision #3) provide Future/async/await to users
- **Best Practice:** Grievous's insights on safety/composability addressed in adapter design

**Status:** RESOLVED — No team action needed; architecture accommodates both user constraint and Grievous's ergonomic concerns.

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
- Cassian: Tag in Full/Deferred phase review for potential Phase 2 evaluation

---

### 9. Thrawn Rust FASTER Architecture: 12 Key Decisions

**Lead Architect:** Thrawn  
**Date:** 2026-03-05T18:33:00Z  
**Status:** DOCUMENTED — Binding architectural decisions  
**Impact:** Binding for all implementation phases

**Summary of 12 Decisions:**

1. **No async/await in core** (user directive, overrides Grievous #1) — Callback/completion model. Async adapter crates per runtime.

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
- **Kenobi:** Async adapter design must bridge callback→Future (oneshot channels or Waker integration)
- **Mando:** All core code sync-only, no tokio dependency
- **Chirrut:** Device trait implementation is completion-based, not Future-based
- **Jyn:** Simulation framework must model callback completion ordering
- **Maul:** Unsafe audit scope includes hash index atomics, record pointer arithmetic, FFI boundary

**Risk Register:** 25 identified risks across 5 categories. 5 critical (score ≥ 15), 8 high (score 10-14). Primary mitigations: deterministic simulation testing, Miri verification, security audit, cross-implementation comparison.

**Artifact Location:** `.squad/agents/thrawn/rust-faster-architecture.md` (288 KB, 6101 lines, 14 sections)

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
- Thrawn approved PAW for 5 complex MVP items (1f, 1g, 1h, 2d, 2e)
- Direct implementation for 11 simpler items
- Further integration to be determined post-MVP-Phase-1

---

### 13. MVP Iteration 1 Implementation Plan Approved

**Agent:** Thrawn (Lead Architect)  
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
- **Mando:** Primary on 7 items (1b, 1c, 1d, 1f, 1g, 2a, 2c, 2d, 2e)
- **Chirrut:** Primary on 2 items (1h, 2b), support on atomics/alignment
- **Rex:** Primary on 3 items (1a, 1e, 1i, 2f)
- **Maul:** Unsafe audit on all PAW items
- **Cassian:** Behavioral spec extraction from C++/C# for validation
- **Ahsoka:** Benchmark design and early performance analysis
- **Thrawn:** Review authority on all PRs

**Artifact:** `.squad/agents/thrawn/mvp-iteration-1-plan.md` (46KB)

**Implications:**
- All team members should read the plan before starting work
- Work items are to be executed in dependency order per the graphs in the plan
- No work item merges without passing the stated success criteria
- Thrawn has final review authority on all PRs per architecture governance

---

### 10. Typed Page/Offset Newtypes for LogicalAddress API

**Agent:** Mando (Rust Expert)  
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

**Agent:** Mando (Rust Expert)  
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

**Agent:** Chirrut (Systems Programming Expert)  
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

**Agent:** Mando (Rust Expert)  
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

**Agent:** Rex (QA Engineer)  
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
