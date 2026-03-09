# Decision: SyncFileDevice Prefix Must Be "log." for Recovery Compatibility

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-09
**Status:** Observation — requires documentation or API fix

## Summary

The `LogRecoveryEngine::validate_log_file` hardcodes segment filenames as `log.{n}` (e.g., `log.0`, `log.1`). If a `SyncFileDevice` is created with a different prefix (e.g., `"hlog."`), checkpoint metadata will reference log addresses that the recovery engine cannot locate, causing `ValidationFailed` errors.

## Impact

Any code that creates a `SyncFileDevice` with a non-`"log."` prefix and later attempts recovery will fail silently at the validation step. This affects:

- **Gandalf/Faramir**: Store construction in examples or production configs must use `"log."` prefix.
- **Eowyn**: DST scenarios using SyncFileDevice need the same prefix.
- **Sam/Legolas**: Benchmark setups that use alternate prefixes won't be recovery-compatible.

## Recommendation

Either:
1. **Document the constraint**: Add a doc comment on `SyncFileDevice::new` noting that the prefix must be `"log."` for checkpoint/recovery compatibility.
2. **Or fix the coupling**: Store the device prefix in checkpoint metadata so `validate_log_file` uses the correct prefix.

Option 2 is the correct long-term fix but is a larger change.
