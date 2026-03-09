# Decision: DST Parameterized Expansion Architecture

**Author:** Éowyn (DST Expert)
**Date:** 2026-03-09
**Branch:** `eowyn/dst-100-scenarios`
**Status:** Implemented

## Summary

Expanded DST from 5 to 108 scenario templates via parameterized expansion.
All scenarios use the existing `ScenarioTemplate` / `SeedCampaign` framework
with no breaking changes.

## Key Architecture Decisions

| Decision | Rationale |
|----------|-----------|
| Fixed crash points per template (not seed-modulated) | Each template tests one specific crash point. Campaign seed sweep tests that point across many data permutations. |
| `all_expanded_scenarios()` single entry point | One function returns all 108 templates. Easy to enumerate, count, and pass to campaigns. |
| No framework changes to campaign runner | Templates work within existing write→checkpoint→crash→recover→verify model. |
| Parameterized template constructors | New scenario files accept crash point + record count arguments. Usable both via expansion engine and directly. |

## Team Impact

- **Frodo (CI):** New `campaign_expanded_smoke` test runs 324 cases in ~30s. Good for Tier 2.
  New `campaign_expanded_full` (ignored) runs 10,800 cases for Tier 3.
- **All:** The `scenarios::expansion::all_expanded_scenarios()` API is the canonical way
  to get the full scenario set.
