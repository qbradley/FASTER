# Decision: small-pages Feature Flag Implementation

**Author:** Sam (Systems & Storage Expert)  
**Date:** 2026-03-16  
**Status:** Implemented  
**Commit:** `3ea5ca81`  
**Branch:** `temp/revert-validation-small-pages`

---

## Summary

Implemented `#[cfg(feature = "small-pages")]` to set `OFFSET_BITS = 16` (64 KB pages) for DST testing. Production default remains `OFFSET_BITS = 25` (32 MB pages). Zero blast radius for production builds.

## What Changed

| File | Change |
|------|--------|
| `faster-core/Cargo.toml` | Added `small-pages = []` feature, updated `check-cfg` |
| `faster-core/src/address.rs` | Cfg-gated `OFFSET_BITS` (25/16), fixed `MAX_PAGE` u64 arithmetic, cfg-gated test assertions |
| `faster-core/src/allocator.rs` | Cfg-gated `ITEMS_PER_PAGE_BITS` (20/16), clippy allow on `bump_allocate` |
| `faster-core/src/hybrid_log/log_allocator.rs` | Clippy allow on two SF-7 overflow guards |
| `faster-dst/Cargo.toml` | Added `small-pages = ["faster-core/small-pages"]` feature |
| 5 test files | Fixed hardcoded offset masks, buffer sizes, address construction |

## Key Technical Decisions

1. **MAX_PAGE overflow:** With OFFSET_BITS=16, PAGE_BITS=32, and `1u32 << 32` overflows. Fixed by computing `((1u64 << PAGE_BITS) - 1) as u32`, yielding `u32::MAX`. This is correct — the full 32-bit page space is addressable within the 48-bit logical address.

2. **Clippy absurd_extreme_comparisons:** When MAX_PAGE=u32::MAX, guards like `page.0 >= MAX_PAGE` become tautological. Rather than removing these safety guards (which are meaningful at OFFSET_BITS=25), added targeted `#[allow]` attributes on the enclosing functions.

3. **ITEMS_PER_PAGE_BITS adjustment:** The overflow allocator assertion `ITEMS_PER_PAGE_BITS ≤ OFFSET_BITS` would fail (20 > 16). Reduced to 16 under small-pages. This gives 65,536 items per allocator page instead of 1M — sufficient for DST workloads.

4. **Test geometry:** 6 tests needed adjustment for 64KB pages — hardcoded offset masks, buffer sizes, and address construction assumed 32MB geometry. Fixes use `MAX_OFFSET`/`OFFSET_BITS` constants instead of literals.

## Verification Results

| Check | Result |
|-------|--------|
| `cargo build -p faster-core` | ✅ |
| `cargo build -p faster-core --features small-pages` | ✅ |
| `cargo nextest run -p faster-core` (1722 tests) | ✅ all pass |
| `cargo nextest run -p faster-core --features small-pages` (1722 tests) | ✅ all pass |
| `cargo build -p faster-dst --features small-pages` | ✅ |
| `cargo clippy -p faster-core -- -D warnings` | ✅ clean |
| `cargo clippy -p faster-core --features small-pages -- -D warnings` | ✅ clean |
| Loom (25 tests, --features loom) | ✅ all pass |

## Usage

```bash
# DST testing (both features)
cargo test -p faster-dst --features small-pages

# Small-pages only (fast memory-pressure tests)
cargo test -p faster-core --features small-pages

# Production (unchanged default)
cargo test -p faster-core
```

## What This Enables

With 64 KB pages, a ~2 MB workload fills 32 pages — exercising the full flush/eviction pipeline. At 32 MB pages, the same workload fills 0.06 of a single page. DST scenarios can now stress page advancement, sealing, flushing, eviction, and begin-address advancement at low cost.
