# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

## 2026-03-05T19:25: Item 1e — CI Skeleton Created

**What:** Created `.github/workflows/rust-ci.yml` — a multi-job GitHub Actions CI pipeline for the Rust workspace.

**Jobs:**
1. **fmt** — Formatting check using nightly rustfmt (required for `imports_granularity` and `group_imports` options in `rustfmt.toml`).
2. **ci** — Clippy + check + test (debug & release) on a 3-OS matrix (ubuntu, windows, macos). Gated behind `fmt`. Uses `Swatinem/rust-cache@v2` for caching.
3. **miri** — Weekly scheduled + on-demand Miri runs with `-Zmiri-symbolic-alignment-check -Zmiri-retag-fields`. `continue-on-error: true` — informational for now.
4. **bench** — `cargo bench --workspace --save-baseline main` on pushes to main only.
5. **deny** — `cargo deny check` using `EmbarkStudios/cargo-deny-action@v2` against the existing `rust/deny.toml`.

**Local validation:** All 5 core commands (`check`, `clippy`, `fmt --check`, `test`, `test --release`) pass clean in the current workspace.

**Also:** Added CI status badge to `rust/README.md`.

**Learnings:**
- The `rustfmt.toml` uses `imports_granularity` and `group_imports`, which are nightly-only features. Formatting CI must use nightly toolchain.
- This environment uses `msrustup` (Microsoft internal), not public `rustup`. CI uses standard `dtolnay/rust-toolchain` actions since it targets GitHub-hosted runners.
- `cargo-deny` is not installed locally but the `deny.toml` config exists and is well-structured. Using the official GH action avoids toolchain issues.
- Path filtering (`rust/**`) keeps Rust CI from running on C#/C++ changes — important since the monorepo has three language ecosystems.

---

## 2026-03-05T19:20: Wave 1 Sprint Complete — 1e Done

**Context:** All 4 Wave 1 tasks executed in parallel by full team.

**Your Deliverable (1e):**
- Task 1e: CI skeleton — 5-job GitHub Actions workflow, all validated locally

**Decision Merged:**
- Decision #14: Rust CI Architecture — GitHub Actions (not Azure Pipelines), 5 jobs, path filtering, nightly rustfmt only

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-rex.md`

**What's Next:**
- All future Rust PRs will now be gated on fmt + clippy + test (debug/release) × 3 OSes ✅
- Miri runs weekly (Sundays 06:00 UTC) + on manual dispatch for UB detection
- Benchmark baseline established for main branch

**Team Status:**
- Mando (1b/1c): Address + status/error complete, 127 tests passing
- Chirrut (1d): Hash traits complete, 64 tests passing
- All 127 tests passing in workspace. Zero regressions.



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

