# Skill: Architecture Review Checklist

## When to Use

Before greenlighting any subsystem implementation. Required for all new modules, major API changes, or cross-cutting concerns.

## Pattern

### Pre-Implementation Questions

**1. Interface Boundaries**
- [ ] Does this module have a clear public API surface?
- [ ] Are internal implementation details hidden?
- [ ] Can this be tested in isolation?

**2. Cross-Language Consistency**
- [ ] Does the behavior match C++ FASTER reference implementation?
- [ ] Does the behavior match C# FASTER reference implementation?
- [ ] If diverging, is the rationale documented?

**3. Concurrency & Safety**
- [ ] Are all public types Send+Sync where safe?
- [ ] Is `unsafe` confined to FFI, atomics, or allocator?
- [ ] Are `#[deny(unsafe_code)]` boundaries respected?

**4. API Design**
- [ ] Is the API idiomatic Rust?
- [ ] Are errors non-exhaustive for forward compatibility?
- [ ] Does it follow single-writer session model (no interior mutability on hot path)?

**5. Dependencies**
- [ ] No async runtime dependency (use `std::thread` + channels)?
- [ ] Feature flags for optional deps (e.g., `tokio`)?
- [ ] All path dependencies include `version = "x.y.z"` for crates.io compat?

**6. Testing Strategy**
- [ ] Which tier(s) of the 4-tier validation model apply?
- [ ] Are property tests needed for this API?
- [ ] Are concurrency tests (Miri/Loom) needed?

### Decision Documentation

If any answer is "yes, but..." or requires a trade-off:
1. Document in `.squad/decisions/inbox/gandalf-{brief-slug}.md`
2. Include: decision, rationale, alternatives considered, team impact

## Confidence: high

## Learned From

- "Architecture-first approach validated: 12 binding decisions on Day 1 were never revisited."
- "No implementation without a clear structural plan"
- Foundation hardening (Wave 0) — module boundaries established before any implementation
