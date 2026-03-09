# Releasing FASTER Rust Crates

Current release: 0.1.0

This document describes the release process for the FASTER Rust workspace.

## Overview

The workspace publishes five crates to [crates.io](https://crates.io):

| Crate | Description |
|-------|-------------|
| `faster-core` | Core KV engine — no async runtime dependency |
| `faster-device` | Device trait and I/O implementations |
| `faster-tokio` | Tokio async adapter |
| `faster-uring` | io_uring async I/O (Linux-only) |
| `faster-ffi` | C FFI bindings |

Non-publishable crates (`faster-bench`, `faster-dst`, samples) are excluded
from releases via `publish = false` and `release = false`.

## Version Strategy

- **Pre-1.0 (`0.x.y`):** Minor bumps may contain breaking changes per semver
  convention. Patch bumps are backwards-compatible fixes.
- **Post-1.0 (`x.y.z`):** Full semver — major = breaking, minor = features,
  patch = fixes. The `cargo-semver-checks` CI job will enforce this.
- **All crates share the same version number** (workspace-level release).

## Prerequisites

Install the release tooling:

```bash
cargo install cargo-release
cargo install cargo-semver-checks
```

## Step-by-Step Release Process

### 1. Verify the Baseline

Ensure all tests pass and CI is green on the `squad` (or `main`) branch:

```bash
cd rust
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo test --workspace --release
```

### 2. Check for Semver Violations

Run semver checks against the last published version:

```bash
cargo semver-checks -p faster-core
cargo semver-checks -p faster-device
cargo semver-checks -p faster-tokio
cargo semver-checks -p faster-uring
cargo semver-checks -p faster-ffi
```

> **Pre-1.0 note:** Semver violations are informational — minor bumps may break
> APIs. After 1.0, these become hard failures.

### 3. Dry Run

Preview what cargo-release will do without actually publishing:

```bash
# From the rust/ directory:
cargo release <NEW_VERSION> --dry-run

# Examples:
cargo release 0.2.0 --dry-run      # Minor bump
cargo release 0.1.1 --dry-run      # Patch bump
cargo release patch --dry-run       # Bump patch automatically
cargo release minor --dry-run       # Bump minor automatically
```

This will show:
- Which crates will be published (and which are skipped)
- The git tag that will be created
- The commit message
- Version replacements in docs

### 4. Execute the Release

When satisfied with the dry run:

```bash
cargo release <NEW_VERSION> --execute
```

This will:
1. Bump version in all publishable `Cargo.toml` files
2. Update version strings in README and docs
3. Run pre-release hooks (clippy)
4. Create a git commit: `release: <NEW_VERSION>`
5. Create a git tag: `rust-v<NEW_VERSION>`
6. Push the commit and tag to origin

### 5. CI Takes Over

Pushing the `rust-v*` tag triggers `.github/workflows/rust-release.yml`:

1. **Validate** — format + clippy checks
2. **Test** — full nextest suite (debug + release) and doc tests
3. **Semver Checks** — informational API compatibility scan
4. **Publish** — crates published to crates.io in dependency order:
   `faster-core → faster-device → faster-tokio → faster-uring → faster-ffi`
5. **GitHub Release** — release created with changelog and install instructions

### 6. Verify the Release

After CI completes:

```bash
# Check crates.io (may take a few minutes to index):
cargo search faster-core
cargo search faster-device

# Verify install works:
cargo add faster-core@<VERSION>
```

## Manual Dry-Run via GitHub Actions

You can trigger a dry-run release from the GitHub Actions UI:

1. Go to **Actions → Rust Release → Run workflow**
2. Select `dry_run: true`
3. Review the output — no crates are actually published

## Publish Order

Crates must be published in dependency order to satisfy crates.io resolution:

```
faster-core          (no internal deps)
    ├── faster-device
    ├── faster-tokio
    ├── faster-uring
    └── faster-ffi   (also depends on faster-device)
```

The CI workflow and `release.toml` both encode this order.

## Rollback

### If a crate publish partially fails

crates.io publishes are **permanent** — you cannot un-publish a version. Instead:

1. **Fix the issue** in the source.
2. **Bump to the next patch version** (e.g., `0.1.0` → `0.1.1`).
3. **Yank the broken version** (hides it from new installs but doesn't remove it):
   ```bash
   cargo yank --version 0.1.0 faster-core
   ```
4. **Re-release** with the fix.

### If the tag was created but publish didn't happen

```bash
# Delete the remote tag:
git push origin :refs/tags/rust-v0.1.0

# Delete the local tag:
git tag -d rust-v0.1.0

# Revert the version bump commit if needed:
git revert HEAD

# Fix, re-tag, re-push.
```

### If only some crates were published

The remaining crates can be published manually:

```bash
cd rust
CARGO_REGISTRY_TOKEN=<token> cargo publish -p <crate-name>
```

## Configuration Files

| File | Purpose |
|------|---------|
| `rust/release.toml` | cargo-release configuration (publish order, tags, hooks) |
| `.github/workflows/rust-release.yml` | Release CI workflow |
| `.github/workflows/rust-ci.yml` | PR CI (includes semver-checks job) |

## Troubleshooting

**"crate `faster-core` not found" during publish of dependent crate**
→ crates.io index hasn't synced yet. The CI workflow includes a 30-second
delay between publishes. If manual, wait 60 seconds and retry.

**Semver check fails but version is pre-1.0**
→ Expected for breaking changes. The CI job is `continue-on-error: true`
and won't block the release. After 1.0, breaking changes require a major bump.

**cargo-release refuses to run on this branch**
→ Check `allow-branch` in `rust/release.toml`. Only `squad`, `main`, and
select feature branches are permitted.
