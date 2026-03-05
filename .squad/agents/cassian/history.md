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

### 2026-03-05: Cross-Implementation Analysis Complete

**Architectural Insights:**
- **Hybrid Log**: Single unified structure spanning DRAM + disk; 90% mutable (in-place updates), 10% read-only (aging), disk (flushed). Key innovation eliminating cache/storage separation.
- **Epoch Protection**: Lock-free synchronization via `LightEpoch`; threads register current epoch, global operations wait for epoch bump. Enables non-blocking checkpoints/GC.
- **Latch-Free Hash Index**: 64-byte buckets (8 entries × 8 bytes), 48-bit addresses + 14-bit tags, reverse linked lists via RecordInfo.previous_address. Overflow buckets leak (acceptable).
- **CPR Recovery**: Group commit with session-based commit points, exclusion lists for pending operations. Fold-over (flush to disk) vs Snapshot (copy to file) vs Incremental (delta log).
- **Version Separation**: Checkpoint bumps version (v → v+1); v threads see only v records, v+1 threads create v+1 records. Prevents lost-update anomalies during checkpoint.

**Key File Paths:**
- C++ Core: `cc/src/core/faster.h`, `phase.h`, `light_epoch.h`, `record.h`, `internal_contexts.h`
- C++ Index: `cc/src/index/hash_table.h`, `hash_bucket.h`, `mem_index.h`, `cold_index.h` (F2)
- C++ F2: `cc/src/core/f2.h`, `checkpoint_state_f2.h`, `internal_contexts_f2.h`
- C# Core: `cs/src/core/Index/FASTER/FASTER.cs`, `Implementation/Internal{Read,Upsert,RMW,Delete}.cs`
- C# Checkpoint: `cs/src/core/Index/Synchronization/FullCheckpointStateMachine.cs`, `Recovery/Recovery.cs`
- C# Log: `cs/src/core/Allocator/BlittableAllocator.cs`, `GenericAllocator.cs`
- C# FasterLog: `cs/src/core/FasterLog/FasterLog.cs` (standalone append log, ~120KB, 12 files)

**Feature Parity:**
- **Both C++/C#**: Core CRUD, hybrid log, hash index, epochs, CPR, compaction, resize, read cache, Azure storage
- **C# Exclusive**: FasterLog (append log), Remote server (TCP/WebSocket), Incremental snapshots (delta log), GenericAllocator (managed objects)
- **C++ Exclusive**: F2 two-tier (hot/cold stores), ColdIndex (disk-based 2-level index, 1 byte/key), Lazy compaction (2s timeout)

**Behavioral Contracts:**
- **Upsert & Delete**: NEVER go pending (always complete immediately by creating at tail)
- **Read & RMW**: Can go pending if record on disk (async I/O required)
- **RecordInfo Header**: 8 bytes = 48-bit previous_address + 13-bit version + 3 flag bits (invalid, tombstone, final)
- **Status Codes**: C++ uses 8-bit enum, C# uses 16-bit flags with advanced codes (CreatedRecord, Expired, etc.)
- **Checkpoint FSM**: REST → PREP_INDEX → PREPARE(v) → IN_PROGRESS(v+1) → WAIT_FLUSH → PERSISTENCE_CALLBACK → REST(v+1)

**Design Decisions for Rust:**
1. **Varlen Storage**: Inline (C++ style) not dual-log (simpler, better cache locality)
2. **Async Model**: Support both callbacks (no-runtime) and Future trait (Tokio integration)
3. **Metadata Format**: Binary structs (C++ style) with versioning for forward compat
4. **Checkpoint Types**: All three (fold-over, snapshot, incremental) for workload flexibility
5. **F2 Two-Tier**: Defer to v2 (complex, not MVP-critical), design for extensibility
6. **FasterLog**: Include in full version (valuable standalone feature, relatively simple)
7. **Remote Server**: Defer to v3 (not core KV, large surface area)

**MVP Scope (3-4 months):**
- Core KV (Read, Upsert, RMW, Delete)
- Hybrid log + hash index + epochs
- Fold-over + snapshot checkpoints
- CPR recovery with session commit points
- Sync + async operations
- Local + null storage devices
- ~10K LOC estimate

**Full Feature Scope (6-8 months):**
- Add: Incremental snapshots, read cache, Azure storage, FasterLog, scan/iteration
- ~20K LOC estimate
