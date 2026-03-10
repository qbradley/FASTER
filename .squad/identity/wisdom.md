---
last_updated: 2026-03-09T23:59:00Z
---

# Team Wisdom

Reusable patterns and heuristics learned through work. NOT transcripts — each entry is a distilled, actionable insight.

## Patterns

### Rust Toolchain & Build

**Pattern:** `cargo nextest run` must NEVER use `--all-targets`. Criterion bench binaries don't implement nextest's test discovery and will run full statistical benchmarks (12+ minutes per file).
**Context:** All `harness = false` criterion benchmarks must use a custom `main()` that checks for `--bench` flag. `criterion_main!()` is banned.

**Pattern:** Loom tests require `--features loom`, not `RUSTFLAGS="--cfg loom"`. The cfg flag approach causes 56+ compile errors because it activates loom's std replacements without the loom crate being a dependency.
**Context:** Run as: `cargo nextest run --features loom -p faster-core --test loom_tests`

**Pattern:** Miri tests require `#[cfg(miri)]` gating and `cargo +nightly miri nextest run -p faster-core --test miri_tests`. The `-E 'test(miri)'` filter matches nothing because "miri" is a cfg, not a test name substring.
**Context:** Both loom and miri must be hard failures in release gates, never advisory.

**Pattern:** For crates.io publishing, path dependencies MUST include `version = "x.y.z"` alongside `path = "..."`. Omitting the version causes publish failures that are only caught at `cargo publish` time, not at build time.
**Context:** All inter-workspace crate deps in Cargo.toml.

**Pattern:** `cargo-mutants -F` flag is a substring match. `-F 'address.rs'` also matches `begin_address.rs`. Use `-F 'src/address.rs'` for precision.
**Context:** Mutation testing runs.

**Pattern:** Bitwise const ops in Rust produce unviable mutants (e.g., `<<`→`>>` in const bitmask definitions). The type system catches them at compile time. This is NOT a test gap — it's the compiler doing its job.
**Context:** Mutation testing triage — don't write tests for these.

**Pattern:** `FasterKv::builder()` returns a non-generic `FasterKvBuilder`. Doctests must use turbofish: `FasterKv::<SimpleFunctions<u64, u64>>::builder()`.
**Context:** Any doc example that constructs a FasterKv.

### Architecture & Design

**Pattern:** `Address::INVALID = 1` (not 0) because all-zeros means an empty hash bucket entry. An address of 1 means "address field is populated but refers to nothing valid."
**Context:** `LogicalAddress` in `address.rs`. This is a C++/C# FASTER invariant that must be preserved.

**Pattern:** Hash bucket = exactly 64 bytes = 1 cache line. 7 entries + overflow pointer. 8-bit tag per entry from hash MSBs reduces false-positive chain walks by ~99%. This layout is non-negotiable for performance.
**Context:** `hash/bucket.rs`, `#[repr(C, align(64))]`

**Pattern:** Two-phase intermediate CAS protocol prevents ABA in EPVS state transitions. Bit 63 set = intermediate state. Observers spin-wait when they see it. This is the foundation of checkpoint/grow mutual exclusion.
**Context:** `AtomicSystemState::try_transition()` in `state/mod.rs`

**Pattern:** RecordInfo bit layout is `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`. Version is 12 bits (max 4095). Every bit position matters — changing any breaks on-disk format compatibility.
**Context:** `record/record_info.rs`

**Pattern:** Memory ordering must be explicitly justified for every atomic operation. `seal()` = Release, `is_sealed()` = Acquire, `try_seal()` = AcqRel/Acquire CAS. No SeqCst defaults — that's lazy, not correct.
**Context:** All atomic ops across the codebase. Sam's standing rule.

**Pattern:** `SyncFileDevice` prefix MUST be `"log."` for checkpoint/recovery compatibility. `LogRecoveryEngine::validate_log_file` hardcodes `log.{n}` segment filenames. Any other prefix causes silent recovery failure.
**Context:** Known bug MF-1 area. Long-term fix: store prefix in checkpoint metadata.

**Pattern:** Grow state machine uses a separate `AtomicU8` instead of the shared `AtomicSystemState`. There is NO mutual exclusion between grow and checkpoint — both could start simultaneously. This must be fixed before grow ships (0.2.0).
**Context:** `hash/grow/state_machine.rs` vs `state/mod.rs`. See BACKLOG.md for full dependency chain.

**Pattern:** Prefetch architecture is two-level: L1 = hash bucket (issued at key hash time), L2 = record data (issued after find() returns). L1 uses `_MM_HINT_T0` (read), L2 uses `_MM_HINT_ET0` (exclusive/write) for upserts. aarch64 equivalents: `PRFM PLDL1KEEP` / `PSTL1KEEP`.
**Context:** `hash/prefetch.rs`, `store/operations.rs`

### Testing

**Pattern:** Test economics: code is cheap, runtime + investigation is expensive. Write more tests rather than spending hours triaging whether a gap matters. A 10-line test that catches one mutant is better than a 2-hour investigation proving the mutant is equivalent.
**Context:** User directive (qbradley). Applies to mutation testing, fuzz testing, and gap-filling.

**Pattern:** Small `FasterKvConfig` for pressure tests: `hash_index_size_log2: 1..10`, `buffer_size_pages: 4`, `mutable_fraction: 0.5`, `max_in_memory_pages: 3`, `grow_config.enabled: false`. This creates real memory pressure without needing large datasets.
**Context:** `tests/memory_pressure_tests.rs` — the canonical pattern for stress testing.

**Pattern:** DST scenarios use parameterized expansion: one template function takes crash point + record count, generating 108 scenarios from 14 categories × 3-6 parameters each. Campaign seed sweep tests each crash point across data permutations.
**Context:** `faster-dst/src/scenarios/expansion.rs`, `all_expanded_scenarios()` API.

**Pattern:** Mutation testing: 80% overall kill rate target, 85% for critical modules. 90%+ is diminishing returns for systems code with unsafe blocks. Timeouts in unsafe code (segfaults/hangs from UB) count as "tested" — they prove the test harness detects the mutation.
**Context:** Mutation testing campaign. Applies to future runs.

**Pattern:** Disk I/O benchmark classification: <0.1% pending = in-memory (not a real disk benchmark), 0.1-5% = mixed, ≥5% = disk-bound. The 32GB VM with default buffer pages keeps everything in memory. Use `buffer_pages ≤ 16` or `--force-disk` for real disk testing.
**Context:** `benches/disk_io_bench.rs`, benchmark methodology.

### Multi-Agent Operations

**Pattern:** The `checkin` script runs `git add -A`, which sweeps ALL untracked files including artifacts from other agents' branches. Use selective `git add <files>` + `git commit` directly when multiple agents share a working tree.
**Context:** Every multi-agent wave. This has bitten us repeatedly.

**Pattern:** When multiple agents share a filesystem and run simultaneously, they can accidentally commit on each other's branches. Fix: `git branch -f {branch} {last-good-sha}` to reset, then cherry-pick good commits.
**Context:** Wave 2 had cross-branch contamination. Wave 3 was clean because agents used separate branches.

**Pattern:** Merge consolidation across 5+ agent branches requires ordering by dependency chain. Merge leaf branches first (no deps on other branches), then branches that depend on those, etc. Running tests after each merge catches integration issues early.
**Context:** Every wave's merge phase.

**Pattern:** `cargo-release consolidate-commits = true` is essential for workspace releases — produces one version-bump commit covering all crates instead of one per crate.
**Context:** `rust/release.toml` configuration.

### Release Process

**Pattern:** Crate publish order follows the dependency graph with 30-second delays between publishes (crates.io index sync latency): `faster-core` → `faster-device` → `faster-tokio` / `faster-uring` → `faster-ffi`.
**Context:** `rust/docs/releasing.md`, `.github/workflows/rust-release.yml`

**Pattern:** Release-gate has 4 independent tiers: Fast Gate (<60s), Correctness (<5min), Deep Validation (<30min), Release Gate (<2hr). Tiers can be run independently (`--tier N`). Missing tools produce warnings, not failures — graceful degradation on any dev machine.
**Context:** `rust/scripts/release-gate`

**Pattern:** Tag format for Rust releases is `rust-v*` (e.g., `rust-v0.1.0`). Plain `v*` is reserved for the Node.js squad tooling releases. Namespace collision = accidental workflows triggering.
**Context:** `.github/workflows/rust-release.yml`

**Pattern:** Benchmarks NEVER run locally — always on a dedicated VM via sub-agent. Local results are meaningless due to background processes, thermal throttling, and inconsistent system state.
**Context:** User directive (qbradley). Enforced by convention.
