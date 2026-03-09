### 2026-03-09T17:20Z: User directive — local merge workflow
**By:** qbradley (via Copilot)
**What:** Don't worry about pushing to origin. Merge agent branches locally to `squad` branch for now.
**Why:** User request — push is still blocked by EMU auth, work continues locally.

### 2026-03-09T17:20Z: User directive — hash_layout_bench is broken
**By:** qbradley (via Copilot)
**What:** hash_layout_bench is broken — takes an hour in debug, many hours in release. User has disabled it by commenting out benches() call. A sub-agent should fix it as a background task so slow work doesn't hold up other agents.
**Why:** User request — bench needs to be fixed to run in reasonable time.
