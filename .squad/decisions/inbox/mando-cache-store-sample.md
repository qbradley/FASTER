# Decision: cache_store.rs Rust sample

**Author:** mando
**Date:** 2025-01-01
**Status:** implemented

## Context

qbradley requested a Rust equivalent of the C# `cs/samples/CacheStore` sample to demonstrate FASTER's disk-backed cache/KV usage pattern with pending I/O handling.

## Decisions

### Inline PRNG instead of `rand` dependency
The `rand` crate is not in faster-core's dependencies. Rather than adding a dev-dependency for a single example, used a 3-line xorshift64 PRNG. This keeps the dependency tree lean and the example self-contained.

### CLI args over stdin prompts
The C# sample uses `Console.ReadLine()` to select random vs. interactive mode. For Rust, command-line flags (`--evict`, `--interactive`) are more idiomatic and composable. Interactive mode still uses stdin for key input.

### Kept same numerical parameters as C# sample
1M keys, 2^19 progress interval, 100-op pending drain threshold — preserves the teaching value and allows direct comparison with the C# version.
