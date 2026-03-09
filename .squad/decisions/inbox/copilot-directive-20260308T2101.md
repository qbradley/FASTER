### 2026-03-08T21:01: User directive — Pre-release quality prioritization
**By:** qbradley (via Copilot)
**What:** Three-phase quality approach before release:
1. FIRST: Fill known test gaps (compaction, loom, miri, memory pressure)
2. THEN (parallel): Mutation testing to find remaining gaps + FoundationDB-style deterministic simulation testing (use PAW workflow for simulation, it's a big project)
3. AFTER: Reassess before implementing other work items (fault injection, release management, etc.)

Mutation testing needs a plan covering technology choice, execution strategy, and operational process (iterative, when to stop, how to improve hit rate). Simulation testing via PAW workflow or broken into phases with PAW per phase. Benchmarks never run locally — always VM via sub-agent.
**Why:** User request — strategic prioritization for pre-release quality gate
