//! Fuzz target: sequences of KvStore operations.
//!
//! Generates random sequences of Read/Upsert/RMW/Delete with random keys
//! and values, looking for panics, assertion failures, or deadlocks.
//! Uses `InMemoryDevice` so no disk I/O is needed.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::InMemoryDevice;

/// A single store operation parsed from fuzzer input.
#[derive(Arbitrary, Debug)]
enum Op {
    Read { key: u64 },
    Upsert { key: u64, value: u64 },
    Rmw { key: u64, input: u64 },
    Delete { key: u64 },
}

/// Top-level fuzzer input: a sequence of operations.
#[derive(Arbitrary, Debug)]
struct StoreInput {
    /// Hash table size exponent (clamped to [4, 14] to keep memory small).
    hash_log2: u8,
    /// Sequence of operations to execute.
    ops: Vec<Op>,
}

fuzz_target!(|input: StoreInput| {
    // Limit operation count to avoid timeouts.
    if input.ops.is_empty() || input.ops.len() > 2048 {
        return;
    }

    let hash_log2 = (input.hash_log2 % 11 + 4) as usize; // 4..14
    let config = FasterKvConfig {
        hash_index_size_log2: hash_log2,
        buffer_size_pages: 4,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());

    let mut session = store.new_session();

    for op in &input.ops {
        match op {
            Op::Read { key } => {
                let mut output: Option<u64> = None;
                let status = store.read(&mut session, key, &0u64, &mut output, ());
                match status {
                    OperationStatus::Ok => {
                        assert!(output.is_some(), "Ok status but no output");
                    }
                    OperationStatus::NotFound => {
                        // Expected for keys not yet inserted.
                    }
                    OperationStatus::Pending => {
                        // Acceptable — operation deferred to I/O.
                    }
                    other => {
                        // Any other status is unexpected for Read.
                        panic!("unexpected Read status: {other:?}");
                    }
                }
            }
            Op::Upsert { key, value } => {
                let status = store.upsert(&mut session, key, value, ());
                assert!(
                    matches!(
                        status,
                        OperationStatus::Created
                            | OperationStatus::InPlaceUpdated
                            | OperationStatus::CopyUpdated
                            | OperationStatus::Pending
                    ),
                    "unexpected Upsert status: {status:?}",
                );
            }
            Op::Rmw { key, input: inp } => {
                let mut output: Option<u64> = None;
                let status = store.rmw(&mut session, key, inp, &mut output, ());
                assert!(
                    matches!(
                        status,
                        OperationStatus::Created
                            | OperationStatus::InPlaceUpdated
                            | OperationStatus::CopyUpdated
                            | OperationStatus::Pending
                    ),
                    "unexpected RMW status: {status:?}",
                );
            }
            Op::Delete { key } => {
                let status = store.delete(&mut session, key, ());
                assert!(
                    matches!(
                        status,
                        OperationStatus::Deleted
                            | OperationStatus::NotFound
                            | OperationStatus::Pending
                    ),
                    "unexpected Delete status: {status:?}",
                );
            }
        }
    }

    store.dispose_session(session);
});
