# Skill: Rust Workspace Release Pipeline

## Pattern

Multi-crate Rust workspaces need coordinated publishing to crates.io in dependency
order. This skill captures the infrastructure pattern.

## Structure

```
rust/release.toml                      # cargo-release config
.github/workflows/rust-release.yml     # CI pipeline on tag push
.github/workflows/rust-ci.yml          # PR gate (includes semver-checks)
rust/docs/releasing.md                 # Human process guide
```

## Key Mechanics

1. **Dependency-ordered publish:** Root crates first, dependents after. Each
   publish waits 30s for crates.io index sync before the next.

2. **Version constraints on path deps:** Every `path = "..."` dependency in a
   publishable crate MUST also have `version = "x.y.z"`. Without this, `cargo
   publish` rejects the crate.

3. **Tag-triggered release:** Push `rust-v0.2.0` tag → CI runs full test suite →
   publishes in order → creates GitHub Release.

4. **Semver-checks as PR gate:** `cargo-semver-checks` runs on PRs with
   `continue-on-error: true` pre-1.0. After 1.0, remove that flag.

5. **Dry-run via workflow_dispatch:** Manual trigger from GitHub Actions UI
   validates everything without publishing.

## When to Use

- Setting up a new Rust workspace for crates.io publishing
- Adding a new crate to an existing workspace release
- Migrating from manual to automated releases

## Checklist for Adding a New Crate

1. Add `[packages.<name>]` section to `release.toml`
2. Add publish step in `rust-release.yml` (respect dependency order)
3. Add semver-checks step in both `rust-ci.yml` and `rust-release.yml`
4. Ensure `Cargo.toml` has: description, license, repository, keywords, categories
5. Ensure all path deps have version constraints
6. Remove `publish = false` if the crate should be on crates.io
