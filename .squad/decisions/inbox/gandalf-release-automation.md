# Decision: Rust Release Automation Infrastructure

**Author:** Gandalf (Lead / System Architect)
**Date:** 2026-03-09
**Branch:** `gandalf/release-automation`
**Status:** Implemented — ready for review

## Summary

Established the complete release pipeline for publishing FASTER Rust crates to
crates.io. Five crates are publishable; three crates and all samples are excluded.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| Tag format `rust-v*` | Avoids collision with Node.js `squad-release.yml` which uses `v*` |
| Semver-checks advisory pre-1.0 | 0.x minor bumps may break API per semver convention |
| 30s delay between crate publishes | crates.io index sync takes ~20-30s; dependent crates fail without this |
| Consolidated version commits | One commit for all workspace version bumps, cleaner git history |
| `publish = false` on faster-dst | Test-only framework, not useful to downstream consumers |
| Version constraints on path deps | Required for crates.io — `version = "0.1.0"` alongside `path = "..."` |

## Publish Order (dependency graph)

```
faster-core          (root — no internal deps)
    ├── faster-device
    ├── faster-tokio
    ├── faster-uring
    └── faster-ffi   (also depends on faster-device)
```

## Team Impact

- **All agents:** When adding new public API to publishable crates, semver-checks
  in CI will flag breaking changes on PRs.
- **Sam/Faramir:** If new crates are added to the workspace, update `release.toml`
  and the publish order in `rust-release.yml`.
- **Legolas:** Benchmark crate (`faster-bench`) is excluded from publishing —
  it's a dev-only tool.

## Files Changed

- `rust/release.toml` (new)
- `.github/workflows/rust-release.yml` (new)
- `.github/workflows/rust-ci.yml` (modified — added semver-checks job)
- `rust/docs/releasing.md` (new)
- `rust/crates/faster-{core,device,tokio,ffi,uring}/Cargo.toml` (modified)
- `rust/crates/faster-dst/Cargo.toml` (modified — added `publish = false`)
