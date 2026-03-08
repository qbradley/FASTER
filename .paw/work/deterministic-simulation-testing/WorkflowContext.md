# WorkflowContext

Work Title: Deterministic Simulation Testing
Work ID: deterministic-simulation-testing
Base Branch: squad
Target Branch: feature/deterministic-simulation-testing
Workflow Mode: full
Review Strategy: local
Review Policy: planning-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: society-of-thought
Final Review Interactive: smart
Final Review Models: none
Final Review Specialists: all
Final Review Interaction Mode: debate
Final Review Specialist Models: none
Final Review Perspectives: auto
Final Review Perspective Cap: 2
Plan Generation Mode: single-model
Plan Generation Models: none
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.4, gemini-3-pro-preview, claude-opus-4.6
Planning Review Specialists: all
Planning Review Interaction Mode: parallel
Planning Review Specialist Models: none
Planning Review Perspectives: auto
Planning Review Perspective Cap: 2
Custom Workflow Instructions: none
Initial Prompt: Build a FoundationDB-style deterministic simulation testing framework for FASTER Rust. The framework should provide a controlled scheduler that abstracts all nondeterminism (thread scheduling, I/O completion timing), injects faults systematically, and makes every failure perfectly reproducible from a seed. Key focus areas: checkpoint state machine, compaction, hybrid log flush/eviction, recovery. The framework should be its own crate (faster-sim-test) and integrate with the existing FASTER architecture without requiring changes to production code paths (use trait abstractions and cfg flags where needed). Must align with test economics model: lots of code is fine (free), but tests must run fast and produce zero false positives.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: none
