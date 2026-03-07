# Work Routing

How to decide who handles what for the FASTER Rust implementation.

## Routing Table

| Work Type | Route To | Examples |
|-----------|----------|----------|
| Architecture, system design, scope decisions | Gandalf | Overall Rust FASTER design, subsystem decomposition, API surface decisions |
| C++ reference code analysis, C++ porting | Saruman | Analyze cc/ directory, understand C++ FASTER internals, porting guidance |
| C# reference code analysis, C# porting | Faramir | Analyze cs/ directory, understand C# FASTER internals, porting guidance |
| Rust implementation, idioms, ownership | Aragorn | Core Rust code, trait design, lifetime management, safe abstractions |
| Low-level systems, memory, threading, atomics | Sam | Memory-mapped I/O, lock-free data structures, OS primitives, cache lines |
| Reverse engineering FASTER behavior | Frodo | Architecture extraction, protocol analysis, behavioral specification |
| Disk I/O, persistence, durability, storage | Gimli | WAL design, log-structured storage, checkpoint/recovery, hybrid log |
| Security audit, threat modeling, unsafe review | Galadriel | Unsafe code audit, memory safety, FFI boundary hardening |
| Performance profiling, benchmarking, optimization | Legolas | Benchmarks, flame graphs, cache-friendliness, SIMD, throughput |
| Testing, correctness, edge cases | Boromir | Unit tests, integration tests, property-based tests, CI setup |
| Deterministic simulation testing, fault injection | Éowyn | Simulation harness, deterministic scheduling, failure scenario coverage |
| Async integration, Tokio, runtime compat, C FFI | Elrond | Runtime-agnostic async, Tokio integration, C header generation |
| Documentation, samples, developer experience | Arwen | Docs, examples, migration guides, adoption strategy, DX audit |
| Code review (any domain) | Gandalf + domain expert | Architecture review + domain-specific correctness |
| Session logging | Scribe | Automatic — never needs routing |

## Review Gates

| Artifact Type | Reviewer |
|---------------|----------|
| Architecture / design decisions | Gandalf |
| Rust implementation code | Gandalf (architecture) + Aragorn (Rust idioms) |
| Unsafe code blocks | Galadriel (security) + Sam (correctness) |
| Performance-critical paths | Legolas |
| Test suites | Boromir |
| Simulation test harness | Éowyn |
| C FFI boundary | Elrond + Galadriel (security) |
| Storage/persistence layer | Gimli + Gandalf |

## Issue Routing

| Label | Action | Who |
|-------|--------|-----|
| `squad` | Triage: analyze issue, assign `squad:{member}` label | Gandalf |
| `squad:{name}` | Pick up issue and complete the work | Named member |

## Rules

1. **Eager by default** — spawn all agents who could usefully start work, including anticipatory downstream work.
2. **Scribe always runs** after substantial work, always as `mode: "background"`. Never blocks.
3. **Quick facts → coordinator answers directly.** Don't spawn an agent for "what branch are we on?"
4. **When two agents could handle it**, pick the one whose domain is the primary concern.
5. **"Team, ..." → fan-out.** Spawn all relevant agents in parallel as `mode: "background"`.
6. **Anticipate downstream work.** If a feature is being built, spawn the tester to write test cases from requirements simultaneously.
7. **Cross-reference work** — when implementing from C++/C# reference, spawn Saruman or Faramir alongside the implementer to verify behavioral equivalence.
8. **Unsafe code** always gets dual review: Galadriel (security) + Sam (systems correctness).
