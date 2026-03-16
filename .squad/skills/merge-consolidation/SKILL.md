# Skill: Multi-Branch Merge Consolidation

## When to Use

When consolidating 3+ feature branches into a shared integration branch (e.g., `squad`, `main`). Common after parallel agent work on related subsystems.

## Pattern

### Pre-Merge Preparation

1. **Validate each branch independently:**
   ```bash
   git checkout branch-name
   cargo fmt --check && cargo clippy -- -D warnings && cargo nextest run
   ```

2. **Identify dependency chains:**
   - Merge foundational branches before dependent ones
   - Example: `core-types` before `operations` before `benchmarks`

3. **Check for shared file conflicts:**
   ```bash
   # Common conflict zones:
   # - src/*/mod.rs (re-exports)
   # - Cargo.toml (dependencies)
   # - status.rs / error.rs (enum variants)
   ```

### Merge Sequence

1. **One branch at a time:**
   ```bash
   git checkout integration-branch
   git merge --no-ff feature-branch-1
   # Resolve conflicts
   cargo nextest run  # Verify immediately
   git merge --no-ff feature-branch-2
   # Resolve, verify, repeat
   ```

2. **Conflict resolution principles:**
   - **Re-exports:** Keep all additions, alphabetize
   - **Enum variants:** Keep all, ensure uniqueness
   - **Dependencies:** Keep all, de-duplicate versions
   - **Tests:** Keep all, rename if name collision
   - **When uncertain:** Check both branches' intent, preserve both features

3. **Post-merge validation:**
   ```bash
   cargo fmt
   cargo clippy -- -D warnings
   cargo nextest run  # Full suite
   git log --oneline --graph -20  # Verify merge structure
   ```

### Surgical Staging (When Needed)

If other branches touched the working tree (e.g., via checkin script):
```bash
git add <specific-files-only>
git commit -m "..."
# Avoid: git add -A (sweeps in untracked files)
```

## Confidence: high

## Learned From

- Iteration 6 merge (6 branches, 14 conflicts across 8 files)
- Session 2 retrospective (avg 45min merge time, 2 conflict cycles)
- "The checkin script runs git add -A, so untracked files get swept in"
