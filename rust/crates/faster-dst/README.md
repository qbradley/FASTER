# faster-dst — Deterministic Simulation Testing for FASTER

FoundationDB-style deterministic simulation testing framework for the FASTER
key-value store. Sweeps thousands of seed/fault/crash combinations to surface
bugs in checkpointing, compaction, and recovery that are nearly impossible to
find with conventional tests.

**Same seed → same execution → perfect reproducibility.**

## Quick Start

```rust
use faster_dst::campaign::SeedCampaign;
use faster_dst::scenario::ScenarioTemplate;
use faster_dst::workload::CrudWorkload;

let template = ScenarioTemplate::builder("basic_recovery")
    .workload(|seed| CrudWorkload::new(seed, 50))
    .check_committed_recoverable()
    .build();

let mut campaign = SeedCampaign::new();
campaign
    .add_scenario(template)
    .seed_range(0..100)
    .thread_count(4);

let report = campaign.run();
assert!(report.all_passed(), "{}", report.display_human());
```

## Standard Scenarios

Five battle-tested scenario templates covering the major failure modes:

| Template | Description | Records | Crash Points |
|----------|-------------|---------|-------------|
| `crud_stress` | High-volume write/checkpoint/recover cycle | 200 | None |
| `checkpoint_crash` | Crash at each of 6 checkpoint phases | 50 | Checkpoint |
| `compaction_crash` | Crash at each of 6 compaction phases | 100 | Compaction |
| `recovery_stress` | Crash during recovery itself | 50 | Recovery |
| `torn_write` | Partial writes + CRC validation | 50 | Checkpoint |

```rust
// Use a standard template directly:
let template = faster_dst::scenarios::checkpoint_crash::template();
```

## Writing Custom Scenarios

```rust
use faster_dst::crash::{CrashSchedule, CrashTrigger};
use faster_dst::fault::{CrashPoint, FaultConfig};
use faster_dst::invariant::{AllCommittedRecoverable, ValidPageChecksums};
use faster_dst::scenario::ScenarioTemplate;
use faster_dst::workload::{CrudWorkload, Workload};

let template = ScenarioTemplate::builder("my_scenario")
    .workload(|seed| CrudWorkload::new(seed, 100))
    .fault_config(FaultConfig::none().with_partial_writes(0.05))
    .crash_schedule(|seed| {
        let mut s = CrashSchedule::new();
        let phase = (seed % 6) as usize;
        s.add(CrashPoint::ALL_CHECKPOINT[phase], CrashTrigger::Always);
        s
    })
    .invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
    .invariant(|_wl| Box::new(ValidPageChecksums::new()))
    .build();
```

## Running Tests

```bash
# All DST tests
cargo nextest run -p faster-dst

# Campaign tests only
cargo nextest run -p faster-dst --test campaign_tests

# Include long-running (nightly) tests
cargo nextest run -p faster-dst --run-ignored
```

## See Also

- [Technical Reference](../../.paw/work/deterministic-simulation-testing/Docs.md) — full architecture, API reference, and debugging guide
- [Spec](../../.paw/work/deterministic-simulation-testing/Spec.md) — original design specification
