# FASTER Rust — Post-0.1.0 Backlog

Items investigated and deferred from 0.1.0 release. Each entry includes rationale, pointers, and estimated scope to make pickup efficient.

---

## 1. Grow State Machine Integration (0.2.0 Flagship)

**What:** Online, latch-free hash table resizing — doubling bucket count while concurrent operations continue. The foundation (3,115 LOC, 200+ unit tests) is complete; integration with the live store is not.

**Why deferred:** 15–23 days of work touching critical hot paths (epoch, checkpoint, hash table, compaction, recovery). Zero integration tests exist. Too risky pre-0.1.0.

**Current state:**
- `src/hash/grow/mod.rs` — GrowState, bucket_index_for_version(), splits_right()
- `src/hash/grow/state_machine.rs` — GrowPhase enum, GrowStateMachine with CAS
- `src/hash/grow/splitter.rs` — BucketSplitter (core split algorithm)
- `src/hash/grow/manager.rs` — GrowManager orchestrator, GrowConfig, GrowProgress
- `src/state/phase.rs` — Phase enum with grow slots reserved (PrepareGrow=8, InProgressGrow=9, WaitCompletionGrow=10)

**Blocking dependency chain (must be done in order):**
1. **Remove `HashIndex::version` field** (`src/hash/index.rs:112`) — version must be atomic with phase in a single CAS word. ~20 call sites to update.
2. **Wire grow FSM to shared `AtomicSystemState`** — replace `GrowStateMachine::phase: AtomicU8` with the shared 64-bit state word. This gives checkpoint↔grow mutual exclusion.
3. **Session API + epoch integration** — expose `session_count()`, bump epoch on PrepareGrow, sessions check phase before critical sections.
4. **Chunk processing in hot paths** — add `process_chunks()` calls to Read/Write/RMW/Delete. Amortize ~4-8 chunks per operation.
5. **Atomic table swap + finalize** — CAS from WaitCompletionGrow→Rest, version increment.
6. **Recovery metadata** — add `table_version` to checkpoint metadata so recovery loads the correct hash table version.
7. **Integration tests** — concurrent writes during grow, reads never see mixed old/new, checkpoint+grow mutual exclusion, Loom epoch+grow races, DST grow scenarios.

**C#/C++ reference patterns:**
- C# `IndexResizeTask.cs` — session quiescence check pattern
- C# `FASTER.core/Index/` — chunk-based parallel splitting
- Both use the same single-bit trick: `hash & old_size` determines stay-or-move

**User workaround (0.1.0):** Pre-tune `hash_index_size_log2` in `FasterKvConfig`.

**Estimated scope:** 15–23 days (foundation + integration + testing + validation)

---

## 2. Deferred Architecture Items

### 2a. `FasterKv::entry_count()` Returns 0
- Intentionally deferred — accurate count requires scanning all buckets or maintaining an atomic counter on every insert/delete
- Not usable for assertions; callers should not depend on it
- **File:** `src/store/kv.rs`

### 2b. C# Read Performance Gap (-14%)
- Down from -60% pre-epoch, -20% post-epoch
- Closing requires structural changes: open-addressing hash table layout
- Investigation prototype exists: `.squad/agents/gandalf/` has open-addressing research notes
- **Blocked by:** Grow state machine (open addressing changes hash table layout fundamentally)

---

## 3. Testing Gaps

### 3a. Fuzz Testing on Deserialization/Recovery Paths
- Checkpoint recovery parses CRC-validated binary data from disk
- Untrusted bytes hitting those paths without fuzz coverage is a real gap
- **Files:** `src/store/checkpoint.rs`, `src/store/recovery.rs`
- **Action:** Add fuzz targets for recovery parsing, corrupt checkpoint injection

### 3b. DST Framework Not in CI
- Runs locally only (via `cargo test -p faster-dst`)
- Needs GitHub Actions integration for nightly or pre-merge runs
- **File:** `.github/workflows/` — add DST job

### 3c. Coverage Report (`cargo-llvm-cov`) Not in CI
- Decision recorded in `.squad/decisions.md` (item A8)
- Primary targets: FFI, async bridge, compaction GC
- **Action:** Add `cargo-llvm-cov` job to CI workflow

### 3d. Cross-Platform CI (macOS + Windows)
- Tier 4 release gate requirement, not yet built
- **File:** `.github/workflows/rust-release.yml` — add matrix

### 3e. Sample Crate READMEs
- 5 sample crates missing README.md files
- P0 documentation gap from audit
- **Crates:** Check `rust/samples/*/` directories

---

## 4. Known Bugs

### 4a. MF-1: CRC Trailer Corruption
- Silent data corruption bug caught by SoT code review, NOT by tests
- **Severity:** High — data integrity issue
- **Status:** Identified, fix pending investigation

### 4b. `log.` Prefix Coupling
- `SyncFileDevice` and `LogRecoveryEngine` have tight coupling via filename prefix convention
- Design concern, not a correctness bug
- **Files:** `src/device/`, `src/store/recovery.rs`

---

## 5. Infrastructure

### 5a. Push to Origin Blocked (EMU Auth)
- 38+ commits on `squad` branch, unpushed
- Unresolved Azure DevOps EMU authentication issue
- **Action:** Resolve with org admin, then `git push origin squad`

### 5b. Squad Identity Files
- `.squad/identity/wisdom.md` — empty
- `.squad/identity/now.md` — stale
- Low priority, cosmetic

---

## Decision References

- Grow deferral: `.squad/decisions.md` lines 176–187
- Test economics model: `.squad/plans/release-plan.md` (top section)
- Tiered validation architecture: `.squad/plans/release-plan.md` (Part 1)
- Mutation testing policy: `.squad/decisions.md` (cargo-mutants section)
