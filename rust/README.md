# FASTER — Rust Implementation

[![Rust CI](https://github.com/microsoft/FASTER/actions/workflows/rust-ci.yml/badge.svg)](https://github.com/microsoft/FASTER/actions/workflows/rust-ci.yml)

A high-performance, concurrent, durable key-value store implemented in Rust.
This is a ground-up Rust implementation of [Microsoft FASTER](https://github.com/microsoft/FASTER),
designed for mission-critical cloud services at planetary scale.

## Architecture

See the full architecture specification:
[`.squad/agents/thrawn/rust-faster-architecture.md`](../.squad/agents/thrawn/rust-faster-architecture.md)

### Crate Structure

| Crate | Description |
|-------|-------------|
| `faster-core` | Core KV engine — zero async runtime dependencies |
| `faster-device` | Device trait and built-in I/O implementations |
| `faster-dst` | Deterministic simulation testing framework |
| `faster-ffi` | C FFI bindings for embedding in other languages |
| `faster-tokio` | Tokio async adapter (callback → Future bridge) |
| `faster-bench` | Benchmarks and YCSB workload generator |

## Building

```bash
# Check everything compiles
cargo check --workspace

# Run all tests
cargo test --workspace

# Run clippy lints
cargo clippy --workspace -- -D warnings

# Check formatting
cargo fmt --check

# Run benchmarks
cargo bench --package faster-bench
```

> **Before committing**, run the full quality gate (nextest + doctests + clippy + fmt).
> See [CONTRIBUTING.md](CONTRIBUTING.md) for the exact commands and conventions.

## Design Principles

- **No async in core.** The core engine uses callbacks and synchronous primitives.
  Async adapters (Tokio, compio, monoio) provide `Future`/`async fn` wrappers.
- **Minimal dependencies.** `faster-core` depends only on `crossbeam-utils` and `cfg-if`.
- **Safety first.** `#![deny(unsafe_op_in_unsafe_fn)]` enforced everywhere.
  Every `unsafe` block is documented with its soundness invariants.
- **Thread-affine sessions.** Sessions are `!Send` — the compiler enforces
  FASTER's mono-threaded session contract.

## Deterministic Simulation Testing

The `faster-dst` crate provides FoundationDB-style deterministic simulation
testing. A single seed controls all scheduling, fault injection, and crash
timing — same seed, same execution, perfect reproducibility.

Key capabilities:
- **Deterministic scheduler** — cooperative, single-threaded, PRNG-driven task selection.
- **Crash-point injection** — 18 sites across checkpoint, compaction, and recovery
  state machines. Zero-cost when the `simulation` feature is disabled.
- **Page CRC-32C checksums** — torn write detection during recovery.
- **Campaign engine** — sweeps thousands of (scenario, seed) pairs in parallel.
- **5 standard scenario templates** — crud_stress, checkpoint_crash, compaction_crash,
  recovery_stress, torn_write.

```bash
# Run all DST tests
cargo nextest run -p faster-dst

# Run campaign tests only
cargo nextest run -p faster-dst --test campaign_tests
```

See [`faster-dst/README.md`](crates/faster-dst/README.md) for quick-start examples.

## License

MIT — see [LICENSE](../LICENSE) in the repository root.
