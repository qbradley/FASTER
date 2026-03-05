# Work Routing

How to decide who handles what for the FASTER Rust implementation.

## Routing Table

| Work Type | Route To | Examples |
|-----------|----------|----------|
| Architecture, system design, scope decisions | Thrawn | Overall Rust FASTER design, subsystem decomposition, API surface decisions |
| C++ reference code analysis, C++ porting | Grievous | Analyze cc/ directory, understand C++ FASTER internals, porting guidance |
| C# reference code analysis, C# porting | Dooku | Analyze cs/ directory, understand C# FASTER internals, porting guidance |
| Rust implementation, idioms, ownership | Mando | Core Rust code, trait design, lifetime management, safe abstractions |
| Low-level systems, memory, threading, atomics | Chirrut | Memory-mapped I/O, lock-free data structures, OS primitives, cache lines |
| Reverse engineering FASTER behavior | Cassian | Architecture extraction, protocol analysis, behavioral specification |
| Disk I/O, persistence, durability, storage | Tarkin | WAL design, log-structured storage, checkpoint/recovery, hybrid log |
| Security audit, threat modeling, unsafe review | Maul | Unsafe code audit, memory safety, FFI boundary hardening |
| Performance profiling, benchmarking, optimization | Ahsoka | Benchmarks, flame graphs, cache-friendliness, SIMD, throughput |
| Testing, correctness, edge cases | Rex | Unit tests, integration tests, property-based tests, CI setup |
| Deterministic simulation testing, fault injection | Jyn | Simulation harness, deterministic scheduling, failure scenario coverage |
| Async integration, Tokio, runtime compat, C FFI | Kenobi | Runtime-agnostic async, Tokio integration, C header generation |
| Documentation, samples, developer experience | Leia | Docs, examples, migration guides, adoption strategy, DX audit |
| Code review (any domain) | Thrawn + domain expert | Architecture review + domain-specific correctness |
| Session logging | Scribe | Automatic — never needs routing |

## Review Gates

| Artifact Type | Reviewer |
|---------------|----------|
| Architecture / design decisions | Thrawn |
| Rust implementation code | Thrawn (architecture) + Mando (Rust idioms) |
| Unsafe code blocks | Maul (security) + Chirrut (correctness) |
| Performance-critical paths | Ahsoka |
| Test suites | Rex |
| Simulation test harness | Jyn |
| C FFI boundary | Kenobi + Maul (security) |
| Storage/persistence layer | Tarkin + Thrawn |

## Issue Routing

| Label | Action | Who |
|-------|--------|-----|
| `squad` | Triage: analyze issue, assign `squad:{member}` label | Thrawn |
| `squad:{name}` | Pick up issue and complete the work | Named member |

## Rules

1. **Eager by default** — spawn all agents who could usefully start work, including anticipatory downstream work.
2. **Scribe always runs** after substantial work, always as `mode: "background"`. Never blocks.
3. **Quick facts → coordinator answers directly.** Don't spawn an agent for "what branch are we on?"
4. **When two agents could handle it**, pick the one whose domain is the primary concern.
5. **"Team, ..." → fan-out.** Spawn all relevant agents in parallel as `mode: "background"`.
6. **Anticipate downstream work.** If a feature is being built, spawn the tester to write test cases from requirements simultaneously.
7. **Cross-reference work** — when implementing from C++/C# reference, spawn Grievous or Dooku alongside the implementer to verify behavioral equivalence.
8. **Unsafe code** always gets dual review: Maul (security) + Chirrut (systems correctness).
