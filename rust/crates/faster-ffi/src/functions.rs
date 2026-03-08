//! Byte-slice Functions implementation for the FFI layer.
//!
//! The FFI boundary operates on opaque byte buffers (`*const u8` + length),
//! so we need a [`Functions`] implementation where both key and value are
//! `Vec<u8>`. [`SimpleFunctions`] requires `V: Copy`, which `Vec<u8>` does
//! not satisfy, so we provide [`ByteSliceFunctions`] as a full-value-replacement
//! analogue for variable-length byte data.

use faster_core::status::OperationStatus;
use faster_core::store::{Functions, ReadInfo, RmwInfo, RmwInPlaceResult, UpsertInfo};

/// [`Functions`] implementation for `Vec<u8>` keys and values.
///
/// Semantics mirror [`SimpleFunctions`](faster_core::SimpleFunctions):
///
/// - **Upsert** replaces the stored value with the input.
/// - **Read** clones the stored value into `Option<Vec<u8>>` output.
/// - **RMW** replaces the stored value with the input (full-value replacement).
/// - **Delete** is a no-op callback (tombstone handling is in the store).
///
/// # Associated Types
///
/// | Type | Concrete |
/// |---------|-----------------|
/// | Key | `Vec<u8>` |
/// | Value | `Vec<u8>` |
/// | Input | `Vec<u8>` |
/// | Output | `Option<Vec<u8>>` |
/// | Context | `()` |
#[derive(Debug, Clone, Default)]
pub struct ByteSliceFunctions;

impl Functions for ByteSliceFunctions {
    type Key = Vec<u8>;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(
        &self,
        _key: &Self::Key,
        value: &Self::Value,
        _input: &Self::Input,
        output: &mut Self::Output,
        _info: &ReadInfo,
    ) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        _old_value: Option<&Self::Value>,
        _output: &mut Self::Output,
        _info: &UpsertInfo,
    ) {
        value.clear();
        value.extend_from_slice(input);
    }

    fn rmw_initial(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        value.clear();
        value.extend_from_slice(input);
    }

    fn rmw_in_place(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        if input.len() <= value.len() {
            // Fits in existing allocation — update in place.
            value.clear();
            value.extend_from_slice(input);
            RmwInPlaceResult::InPlaceOk
        } else {
            // New value is larger — need a new record.
            RmwInPlaceResult::NeedsNewRecord
        }
    }

    fn rmw_copy_update(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        _old_value: &Self::Value,
        new_value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        new_value.clear();
        new_value.extend_from_slice(input);
    }

    fn rmw_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: OperationStatus,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::{FasterKv, FasterKvConfig, NullDevice};

    fn make_store() -> FasterKv<ByteSliceFunctions> {
        FasterKv::new(
            FasterKvConfig::default(),
            ByteSliceFunctions,
            NullDevice::new(),
        )
    }

    #[test]
    fn upsert_then_read() {
        let store = make_store();
        let mut session = store.new_session();

        let key = b"hello".to_vec();
        let val = b"world".to_vec();

        let _status = store.upsert(&mut session, &key, &val, ());

        let mut output: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &key, &Vec::new(), &mut output, ());
        assert!(status.is_success());
        assert_eq!(output, Some(b"world".to_vec()));

        store.dispose_session(session);
    }

    #[test]
    fn delete_removes_key() {
        let store = make_store();
        let mut session = store.new_session();

        let key = b"key".to_vec();
        let val = b"val".to_vec();

        let _status = store.upsert(&mut session, &key, &val, ());
        let _status = store.delete(&mut session, &key, ());

        let mut output: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &key, &Vec::new(), &mut output, ());
        assert!(status.is_not_found());
        assert!(output.is_none());

        store.dispose_session(session);
    }

    #[test]
    fn rmw_creates_and_replaces() {
        let store = make_store();
        let mut session = store.new_session();

        let key = b"counter".to_vec();

        // RMW on non-existent key creates it.
        let mut output: Option<Vec<u8>> = None;
        let status = store.rmw(&mut session, &key, &b"first".to_vec(), &mut output, ());
        assert!(status.is_success() || status.is_pending());

        // RMW again replaces the value.
        let mut output2: Option<Vec<u8>> = None;
        let status2 = store.rmw(&mut session, &key, &b"second".to_vec(), &mut output2, ());
        assert!(status2.is_success() || status2.is_pending());

        // Read should return latest value.
        let mut read_out: Option<Vec<u8>> = None;
        let _status = store.read(&mut session, &key, &Vec::new(), &mut read_out, ());
        assert_eq!(read_out, Some(b"second".to_vec()));

        store.dispose_session(session);
    }
}
