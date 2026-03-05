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

## Governance

- All meaningful changes require team consensus
- Document architectural decisions here
- Keep history focused on work, decisions focused on direction
- Archive decisions older than 30 days when file exceeds ~20KB
