# Release Manager Review — Premortem Perspective

**Perspective**: premortem
**Reviewer**: Release Manager Specialist
**Diff scope**: ~3,400 lines across 21 files — variable-length record compaction for FASTER Rust engine

---

## Scenario

Six months after shipping this change, a downstream team reports that their code no longer compiles after updating `faster-core`. Their `match` on `CompactionError` is now non-exhaustive, and their test helpers that construct `CompactionPlan` and `AddressMapping` structs fail because new required fields were added. What should we have caught?

---

### Finding 1: New `ScanCorruption` variant on `CompactionError` breaks downstream `match` arms

**Severity**: should-fix
**Confidence**: HIGH
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/compaction/orchestrator.rs` (diff lines 2659–2661) adds a new variant:

```rust
+    /// Scan detected a corrupted record; compaction aborted to preserve data.
+    ScanCorruption(RecordSizeError),
```

`CompactionError` is a public enum (re-exported via `compaction::orchestrator`). It is **not** annotated with `#[non_exhaustive]`. The prior variants were `EmptyRegion`, `CopyFailed`, and `EpochDrainTimeout`. Any downstream crate using an exhaustive `match` on `CompactionError` will get a compiler error.

#### Warrant (Rule)

Under Rust's semver rules, adding a variant to a public enum without `#[non_exhaustive]` is a **breaking change** because existing exhaustive `match` arms become incomplete. Even though the crate is `publish = false` (Cargo.toml line 13), internal consumers within the monorepo or vendored copies face the same breakage. The deployment-path concern is: during a dependency update, consumers discover the break at compile time with no migration path documented.

#### Rebuttal Conditions

This is NOT a concern if: (1) `CompactionError` was already `#[non_exhaustive]` (it is not — verified in the diff); (2) the crate has zero external or internal consumers (the integration test file and store/kv.rs are consumers, but they're in-crate); (3) the team has a policy that all enum matches must use `_ =>` wildcards. Even so, best practice is to annotate the enum.

#### Suggested Verification

Add `#[non_exhaustive]` to `CompactionError` to future-proof it. Alternatively, document in the CHANGELOG that this is an intentional breaking change and bump the version accordingly. Grep the monorepo for `match.*CompactionError` to identify all downstream consumers.

---

### Finding 2: New required field `tombstone_records` on public `CompactionPlan` struct

**Severity**: should-fix
**Confidence**: HIGH
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/compaction/mod.rs` (diff lines 2631–2635) adds:

```rust
+    pub tombstone_records: Vec<LiveRecord>,
```

`CompactionPlan` is a public struct with public fields. Any downstream code constructing a `CompactionPlan` via struct literal syntax must now include `tombstone_records`. The diff itself shows this breakage in 5+ test locations (e.g., scanner.rs diff line 3164: `tombstone_records: vec![]`). This is direct evidence that external consumers face the same issue.

#### Warrant (Rule)

Adding a required public field to a public struct without a default or builder is a semver-breaking change. Downstream test code or custom compaction orchestrators that construct `CompactionPlan` directly will fail to compile. The struct has no `#[non_exhaustive]` annotation or `Default` derive that would cushion the change.

#### Rebuttal Conditions

This is NOT a concern if: (1) `CompactionPlan` is marked `#[non_exhaustive]` (it is not); (2) no external consumer constructs the struct directly (only uses it as a return value). Since the fields are all `pub`, construction is part of the API contract.

#### Suggested Verification

Either add `#[non_exhaustive]` to `CompactionPlan` (preventing external struct literal construction) or derive `Default` so existing construction sites can use `..Default::default()`. Audit whether `CompactionPlan` should be opaque (private fields + accessor methods) given that it's an internal pipeline artifact.

---

### Finding 3: New required field `record_size` on public `AddressMapping` struct

**Severity**: should-fix
**Confidence**: HIGH
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/compaction/copier.rs` (diff lines 2564–2569) adds:

```rust
+    pub record_size: usize,
```

`AddressMapping` is a public struct with public fields. The same pattern as Finding 2 — any downstream code constructing `AddressMapping` must now supply `record_size`.

#### Warrant (Rule)

Same semver rule as Finding 2. Additive public fields on non-`#[non_exhaustive]` public structs are breaking changes. The address updater's test file (diff line 2297: `updater.swing::<u64, u64>(&empty_result, &empty_plan())`) shows internal adaptation was needed. External consumers face the same.

#### Rebuttal Conditions

Same as Finding 2 — only safe if no consumer constructs `AddressMapping` directly.

#### Suggested Verification

Add `#[non_exhaustive]` to `AddressMapping` or make `record_size` derive from the copy operation (which it already does internally). Consider whether `AddressMapping` should even be public.

---

### Finding 4: Scanner `scan()` signature changed — additional type parameter and return type

**Severity**: consider
**Confidence**: MEDIUM
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/src/compaction/scanner.rs` (diff lines 2820–2828) changes:

```rust
-    pub fn scan<K: Key>(
+    pub fn scan<K: Key, V: Value>(
         &self,
         begin_address: LogicalAddress,
         until_address: LogicalAddress,
-        key_size: usize,
-        value_size: usize,
-    ) -> CompactionPlan {
+    ) -> Result<CompactionPlan, RecordSizeError> {
```

Three breaking changes in one method: (1) added generic type parameter `V`, (2) removed `key_size`/`value_size` parameters, (3) return type changed from `CompactionPlan` to `Result<CompactionPlan, RecordSizeError>`. Similarly, `AddressUpdater::swing()` changed signature (diff lines 1936–1943).

#### Warrant (Rule)

Any consumer calling `scanner.scan::<u64>(begin, until, 8, 8)` must now call `scanner.scan::<u64, u64>(begin, until)?`. The return type change forces error handling. While the primary public API (`compact()`/`maybe_compact()`) is backward compatible, the scanner and address updater are also public APIs that direct consumers may use for custom compaction strategies.

#### Rebuttal Conditions

This is NOT a concern if: (1) `CompactionScanner` and `AddressUpdater` are considered internal API that happens to be `pub` for intra-crate use only; (2) no external consumer imports `compaction::scanner::CompactionScanner` directly. The `pub(crate)` visibility on `CompactionOrchestrator::run()` suggests the internal modules were intended to be semi-public.

#### Suggested Verification

Audit the crate's public API surface using `cargo doc --document-private-items` to determine which compaction types appear in the external docs. Consider restricting `CompactionScanner` and `AddressUpdater` to `pub(crate)` if they are not intended as stable public API.

---

### Finding 5: No version bump or CHANGELOG entry for breaking changes

**Severity**: consider
**Confidence**: MEDIUM
**Category**: release-manager

#### Grounds (Evidence)

`rust/crates/faster-core/Cargo.toml` remains at `version = "0.1.0"` with `publish = false`. The diff includes no CHANGELOG or migration guide. Findings 1–4 document at least 4 breaking API changes (new enum variant, two new struct fields, method signature changes).

#### Warrant (Rule)

Even for `publish = false` crates, version bumps signal to internal consumers that API contracts changed. Pre-1.0 semver (0.x.y) treats minor bumps as breaking-change signals. Without a version bump, a downstream team pulling the latest commit has no signal that their code may break. This is the "weekly batch job" failure pattern — silent breakage discovered late.

#### Rebuttal Conditions

This is NOT a concern if: (1) the team uses branch-based dependency pinning (all consumers update together); (2) CI builds all consumers on every commit to this crate. The `publish = false` flag reduces the blast radius but doesn't eliminate internal consumer risk.

#### Suggested Verification

Bump to `0.2.0` or `0.1.1` and add a CHANGELOG entry documenting: new `CompactionError::ScanCorruption` variant, new `tombstone_records` field on `CompactionPlan`, new `record_size` field on `AddressMapping`, and relaxed trait bounds on `compact()`/`maybe_compact()`.

---

## Deployment-Path Traces

| Path | Status | Notes |
|------|--------|-------|
| Build pipeline | ✅ OK | No new files, dependencies, or build config changes |
| Rollback safety | ✅ OK | No data format changes; on-disk records unchanged. Old binary reads same log format. |
| Consumer coordination | ⚠️ Findings 1–4 | Breaking API changes without `#[non_exhaustive]` or version bump |
| CI/CD config | ✅ OK | No pipeline file changes in diff |
| Feature flag | N/A | No staged rollout needed — compile-time API change |
| Environment parity | ✅ OK | No new runtime config, env vars, or infrastructure deps |
