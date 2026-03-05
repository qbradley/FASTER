# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

### 2026-03-05: Cross-Function Team Update

**Team Decisions Merged (from inbox):**

1. **Async Model:** async/await (not manual callbacks) approved for Rust implementation
   - Rationale: simpler, safer, idiomatic, composable, standard ecosystem expectation
   - Implication: All operations return `impl Future<Output = Result<T>>`, Tokio runtime required

2. **Feature Scope:** MVP (C++ baseline, 3-4 months) → Full (add FasterLog, incremental, 6-8 months) → Deferred (F2, Remote, v2+)
   - MVP includes: Core KV, hybrid log, epochs, CPR recovery, fold-over/snapshot, sync+async ops
   - Full adds: Incremental snapshots, read cache, FasterLog, Azure storage, scan iteration
   - Deferred: F2 two-tier (complex, uncertain), Remote server (large surface), Distributed recovery

3. **API Design:** Simplified callbacks (5 methods vs C#'s 19), reduced generics (3-4 vs 6 type params)
   - Trait FasterFunctions with associated types (Input, Output, Context)
   - Natural Rust borrowing instead of ref parameters
   - RAII guards for epoch protection (!Send compile-time safety)
   - Unified builder pattern for configuration
   - Estimated ergonomics: 8/10 vs C#'s 5.7/10

**Awaiting Team Consensus:**
- [ ] Thrawn (Architect): Architecture accommodates deferred features (F2 extensibility)?
- [ ] Cassian (Cross-Impl): Implementation blockers for MVP timeline?
- [ ] Qui-Gon (Rust Expert): API idioms and performance implications?

### 2026-03-05: C# FASTER Architecture Analysis

**Core Architecture Patterns:**
- FasterKV uses lock-free operations via epoch-based memory reclamation (LightEpoch)
- Hybrid log has three-phase lifecycle: mutable → read-only → disk
- Three allocator types: BlittableAllocator (fixed structs), GenericAllocator (objects), VarLenBlittableAllocator (variable-length)
- Session-based concurrency: ClientSession provides thread-independent access; sessions are mono-threaded
- IFunctions callback interface (19 methods) defines user semantics for all operations

**Key File Locations:**
- Main entry: `cs/src/core/Index/FASTER/FASTER.cs`
- Sessions: `cs/src/core/ClientSession/ClientSession.cs`
- Allocators: `cs/src/core/Allocator/AllocatorBase.cs`, `BlittableAllocator.cs`, `GenericAllocator.cs`, `VarLenBlittableAllocator.cs`
- Epoch protection: `cs/src/core/Epochs/LightEpoch.cs`
- Checkpointing: `cs/src/core/Index/Recovery/Checkpoint.cs`, `Recovery.cs`
- Device abstraction: `cs/src/core/Device/IDevice.cs`
- FasterLog: `cs/src/core/FasterLog/FasterLog.cs`

**API Surface Analysis:**
- FasterKV<Key, Value> is main entry point with checkpoint/recovery methods
- ClientSession<K,V,Input,Output,Context,Functions> provides Read/Upsert/RMW/Delete operations
- Four context types: BasicContext (auto epoch), UnsafeContext (manual epoch), LockableContext (locks + auto epoch), LockableUnsafeContext (locks + manual epoch)
- Async API uses ValueTask pattern with two-phase execution (fast path → slow path)
- Configuration via FasterKVSettings<K,V> with defaults for in-memory and disk-backed modes

**Behavioral Contracts for Rust:**
- Sessions must be mono-threaded (cannot call methods concurrently from different threads)
- Epoch protection required for all operations (auto in BasicContext, manual in UnsafeContext)
- IFunctions callbacks must handle: SingleReader/ConcurrentReader, SingleWriter/ConcurrentWriter, InitialUpdater/CopyUpdater/InPlaceUpdater
- Checkpoint types: Snapshot (separate log copy), FoldOver (flush current log for incremental)
- Recovery guarantees: Index + Hybrid Log checkpoints, optional delta logs for incremental recovery

**Rust Translation Opportunities:**
- Replace ref-heavy API with natural Rust borrowing
- Simplify IFunctions (19 methods → trait with default impls + associated types)
- Unify async patterns (ValueTask + callbacks → async/await only)
- Compile-time session safety (!Send marker prevents cross-thread usage)
- Builder pattern for configuration (vs. mixed high/low-level APIs)
- RAII guards for epoch protection (vs. manual Resume/Suspend)

**Ergonomics Assessment:**
- C# API rated 5.7/10: powerful but steep learning curve
- Pain points: ref parameters, 6 generic type params, mixed async patterns, 19-method callback interface
- Pleasant aspects: SimpleFunctions helper, status-based errors (no exceptions), IDisposable pattern, direct device control
- Rust can improve: ownership model, simpler traits, unified async, compile-time safety

---

## 2026-03-05T18:33: Thrawn Rust FASTER Architecture Finalized

**What:** Thrawn completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Thrawn-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/thrawn/rust-faster-architecture.md`

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Kenobi:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Mando:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Chirrut:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Jyn:** Simulation framework must model callback completion ordering for testing.
- **Maul:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Cassian:** Phase roadmap and implementation sequence encoded in Part 3.
- **Grievous:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Jyn lead), unsafe audit planning (Maul lead), Phase 1 sprint (Cassian lead), async adapter spike (Kenobi lead).

