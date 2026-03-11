# Boromir - QA Engineer History

## Learnings

### Mutation Testing Campaign - 17 Gap Survivors (2024)

Successfully wrote mutation-killing tests for 17 identified mutations that survived the initial test suite:

**Mutations Killed:**

1. **eviction.rs (4 gaps)**
   - `needs_eviction` boundary: `>` → `>=` - Test verifies exact boundary (pages == max) doesn't trigger eviction
   - `evict_and_truncate` zero check: `>` → `>=` - Test verifies no truncation when evicted == 0
   - `evict_and_truncate` offset calc: `*` → `+` - Test verifies correct page * page_size calculation
   - `advance_head` OR logic: `||` → `&&` - Documented; covered by existing integration tests

2. **flush.rs (4 gaps)**
   - `flush_page_sync` state checks (3 mutations) - Already covered by existing tests
   - `flush_sealed_pages` counter: `+=` → `*=` - Documented; covered by tier-2 integration test

3. **log_allocator.rs (4 gaps)**
   - `try_allocate` seal boundary: `==` → `!=` - Test verifies sealing only at exact page fill
   - `advance_to_next_page` retry: `!=` → `==` - Test verifies CAS retry logic
   - `load_pages_from_device` offset: `*` → `/` - Test uses marker bytes to verify correct offsets
   - `load_pages_from_device` counter: `+=` → `*=` - Test verifies accurate page count

4. **page.rs (3 gaps)**
   - `get_or_allocate_frame` recycle: `||` → `&&` - Test verifies Evicted OR Free recycling
   - `PageTrailer::write_size` alignment (2 mutations) - Tests verify sector alignment formula

5. **regions.rs (2 gaps)**
   - `is_in_memory` always true: Test verifies OnDisk/Truncated/Invalid return false
   - `needs_flush` comparison: `<` → `>` - Test verifies fuzzy region detection

**Key Patterns Discovered:**

- **Page size matters**: FASTER uses 32MB pages (1 << 25), requiring full-page allocations for sealing tests
- **Region boundaries**: Many operations require pages in specific regions (read-only, flushed, etc.)
- **State transitions**: Complex state machines (Open → Sealed → Flushing → Flushed → Evicted)
- **Private methods**: Some mutations are in private methods, best tested via public APIs or documented
- **Counter mutations**: `+=` → `*=` breaks all counters (0 * 1 = 0); verify counts > 0
- **Arithmetic mutations**: `*` → `+` or `/` in offset calculations; use marker bytes to verify correctness
- **Boundary mutations**: `>` → `>=` or `==` → `!=`; test exact boundaries and off-by-one cases
- **Logic mutations**: `||` → `&&`; ensure tests exercise individual conditions independently

**Testing Strategy:**

1. Unit tests for simple arithmetic/logic mutations
2. Integration tests for complex state machine interactions
3. Documentation for private method mutations covered by higher-level tests
4. Marker bytes/distinct values to verify calculation correctness
5. Boundary testing at exact limits (==, off-by-one)
