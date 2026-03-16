//! Checkpoint metadata persistence.
//!
//! [`CheckpointMetadataStore`] manages the on-disk layout of checkpoint
//! metadata files. Each completed checkpoint is stored under
//! `{base_dir}/checkpoints/{token}/` with JSON files for index recovery
//! info, log recovery info, per-session state, and a master checkpoint
//! descriptor.
//!
//! All writes use the *write-to-temp-then-rename* pattern to guarantee
//! crash safety: data is written to a `.tmp` sibling, fsynced, renamed
//! to the final path, and the parent directory is fsynced.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::sync::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::manager::CheckpointError;
use super::metadata::{
    CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo, SessionRecoveryInfo,
};

// ---------------------------------------------------------------------------
// CheckpointMetadata
// ---------------------------------------------------------------------------

/// Complete metadata bundle for a single checkpoint.
///
/// This is the in-memory representation of everything stored on disk for one
/// checkpoint. [`CheckpointMetadataStore::read_checkpoint_metadata`] assembles
/// it from the individual JSON files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMetadata {
    /// Token that uniquely identifies this checkpoint.
    pub token: CheckpointToken,
    /// Strategy used for the log portion of the checkpoint.
    pub checkpoint_type: CheckpointType,
    /// Hash-index recovery metadata.
    pub index_info: IndexRecoveryInfo,
    /// Hybrid-log recovery metadata.
    pub log_info: LogRecoveryInfo,
    /// Per-session recovery state.
    pub session_infos: Vec<SessionRecoveryInfo>,
    /// ISO 8601 timestamp of when the checkpoint was created.
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Master descriptor (on-disk only)
// ---------------------------------------------------------------------------

/// On-disk master file (`checkpoint.json`) for a single checkpoint directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointDescriptor {
    token: CheckpointToken,
    checkpoint_type: CheckpointType,
    status: String,
    created_at: String,
}

// ---------------------------------------------------------------------------
// CheckpointMetadataStore
// ---------------------------------------------------------------------------

/// Manages the on-disk persistence of checkpoint metadata files.
///
/// # Directory Layout
///
/// ```text
/// {base_dir}/checkpoints/{token}/
///   ├── checkpoint.json          # master descriptor
///   ├── index.recovery.json      # IndexRecoveryInfo
///   ├── log.recovery.json        # LogRecoveryInfo
///   └── sessions/
///       ├── 0.recovery.json      # SessionRecoveryInfo for session 0
///       └── 1.recovery.json      # SessionRecoveryInfo for session 1
/// ```
///
/// All writes are atomic (write-to-temp + fsync + rename + dir-fsync).
pub struct CheckpointMetadataStore {
    base_dir: PathBuf,
}

impl CheckpointMetadataStore {
    /// Creates a new store rooted at `base_dir`.
    ///
    /// The directory tree is created lazily on the first write.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Persists all metadata for a checkpoint to disk.
    ///
    /// Creates the checkpoint directory and writes the index recovery info,
    /// log recovery info, per-session recovery info, and master descriptor.
    /// Each file is written atomically.
    pub fn write_checkpoint_metadata(
        &self,
        token: &CheckpointToken,
        index_info: &IndexRecoveryInfo,
        log_info: &LogRecoveryInfo,
        session_infos: &[SessionRecoveryInfo],
    ) -> Result<(), CheckpointError> {
        let ckpt_dir = self.checkpoint_dir(token);
        let sessions_dir = ckpt_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;

        // Remove stale session files from a previous write of the same token.
        for entry in fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                fs::remove_file(entry.path())?;
            }
        }

        // Index recovery info
        let index_json = serde_json::to_string_pretty(index_info)
            .map_err(|e| CheckpointError::InvalidState(format!("serialize index info: {e}")))?;
        atomic_write(&ckpt_dir.join("index.recovery.json"), index_json.as_bytes())?;

        // Log recovery info
        let log_json = serde_json::to_string_pretty(log_info)
            .map_err(|e| CheckpointError::InvalidState(format!("serialize log info: {e}")))?;
        atomic_write(&ckpt_dir.join("log.recovery.json"), log_json.as_bytes())?;

        // Per-session recovery info
        for session in session_infos {
            let filename = format!("{}.recovery.json", session.session_id);
            let session_json = serde_json::to_string_pretty(session).map_err(|e| {
                CheckpointError::InvalidState(format!("serialize session info: {e}"))
            })?;
            atomic_write(&sessions_dir.join(filename), session_json.as_bytes())?;
        }

        // Master descriptor (written last so readers can use its presence as a
        // "checkpoint complete" marker).
        let now = now_iso8601();
        let descriptor = CheckpointDescriptor {
            token: *token,
            checkpoint_type: log_info.checkpoint_type,
            status: "completed".to_string(),
            created_at: now,
        };
        let desc_json = serde_json::to_string_pretty(&descriptor)
            .map_err(|e| CheckpointError::InvalidState(format!("serialize descriptor: {e}")))?;
        atomic_write(&ckpt_dir.join("checkpoint.json"), desc_json.as_bytes())?;

        // Fsync the checkpoint directory itself so the new entries are durable.
        fsync_dir(&ckpt_dir)?;
        fsync_dir(&sessions_dir)?;

        Ok(())
    }

    /// Reads all metadata for a previously persisted checkpoint.
    ///
    /// Returns [`CheckpointError::NotFound`] if the checkpoint directory or
    /// master descriptor does not exist, and [`CheckpointError::InvalidState`]
    /// if any file contains invalid JSON.
    pub fn read_checkpoint_metadata(
        &self,
        token: &CheckpointToken,
    ) -> Result<CheckpointMetadata, CheckpointError> {
        let ckpt_dir = self.checkpoint_dir(token);
        if !ckpt_dir.exists() {
            return Err(CheckpointError::NotFound);
        }

        // Master descriptor
        let desc: CheckpointDescriptor = read_json(&ckpt_dir.join("checkpoint.json"))?;

        // Index recovery info
        let index_info: IndexRecoveryInfo = read_json(&ckpt_dir.join("index.recovery.json"))?;

        // Log recovery info
        let log_info: LogRecoveryInfo = read_json(&ckpt_dir.join("log.recovery.json"))?;

        // Session recovery infos
        let sessions_dir = ckpt_dir.join("sessions");
        let mut session_infos = Vec::new();
        if sessions_dir.exists() {
            for entry in fs::read_dir(&sessions_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    let info: SessionRecoveryInfo = read_json(&path)?;
                    session_infos.push(info);
                }
            }
            // Sort by session_id for deterministic ordering.
            session_infos.sort_by_key(|s| s.session_id);
        }

        Ok(CheckpointMetadata {
            token: desc.token,
            checkpoint_type: desc.checkpoint_type,
            index_info,
            log_info,
            session_infos,
            created_at: desc.created_at,
        })
    }

    /// Lists all checkpoint tokens that have a completed descriptor on disk.
    ///
    /// Returns an empty vector when no checkpoints exist.
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointToken>, CheckpointError> {
        let checkpoints_dir = self.checkpoints_root();
        if !checkpoints_dir.exists() {
            return Ok(Vec::new());
        }

        let mut tokens = Vec::new();
        for entry in fs::read_dir(&checkpoints_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let descriptor_path = entry.path().join("checkpoint.json");
            if descriptor_path.exists() {
                match read_json::<CheckpointDescriptor>(&descriptor_path) {
                    Ok(desc) => tokens.push(desc.token),
                    // Skip directories with corrupt/unreadable descriptors.
                    Err(_) => continue,
                }
            }
        }

        // Sort by token value for deterministic ordering.
        tokens.sort_by_key(|t| t.as_u128());
        Ok(tokens)
    }

    /// Returns the token of the most recently created checkpoint, or `None`
    /// if no checkpoints exist.
    ///
    /// "Latest" is determined by the `created_at` timestamp in the master
    /// descriptor file.
    pub fn latest_checkpoint(&self) -> Result<Option<CheckpointToken>, CheckpointError> {
        let checkpoints_dir = self.checkpoints_root();
        if !checkpoints_dir.exists() {
            return Ok(None);
        }

        let mut latest: Option<(String, CheckpointToken)> = None;
        for entry in fs::read_dir(&checkpoints_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let descriptor_path = entry.path().join("checkpoint.json");
            if descriptor_path.exists() {
                if let Ok(desc) = read_json::<CheckpointDescriptor>(&descriptor_path) {
                    let dominated = match &latest {
                        Some((ts, _)) => desc.created_at > *ts,
                        None => true,
                    };
                    if dominated {
                        latest = Some((desc.created_at.clone(), desc.token));
                    }
                }
            }
        }

        Ok(latest.map(|(_, tok)| tok))
    }

    /// Deletes all files for the given checkpoint.
    ///
    /// Returns [`CheckpointError::NotFound`] if the checkpoint directory does
    /// not exist.
    pub fn delete_checkpoint(&self, token: &CheckpointToken) -> Result<(), CheckpointError> {
        let ckpt_dir = self.checkpoint_dir(token);
        if !ckpt_dir.exists() {
            return Err(CheckpointError::NotFound);
        }
        fs::remove_dir_all(&ckpt_dir)?;

        // Fsync the parent so the directory removal is durable.
        let parent = ckpt_dir
            .parent()
            .ok_or_else(|| CheckpointError::InvalidState("no parent directory".into()))?;
        fsync_dir(parent)?;
        Ok(())
    }

    // -- internal helpers ---------------------------------------------------

    /// Root directory for all checkpoints: `{base_dir}/checkpoints`.
    fn checkpoints_root(&self) -> PathBuf {
        self.base_dir.join("checkpoints")
    }

    /// Directory for one checkpoint: `{base_dir}/checkpoints/{token}`.
    fn checkpoint_dir(&self, token: &CheckpointToken) -> PathBuf {
        self.checkpoints_root().join(token.to_string())
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Atomically writes `data` to `path` using the write-tmp-fsync-rename
/// pattern.
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), CheckpointError> {
    let tmp_path = path.with_extension("json.tmp");

    // 1. Write to temp file.
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(data)?;

    // 2. Fsync the file so all data is durable before rename.
    file.sync_all()?;
    drop(file);

    // 3. Atomic rename.
    fs::rename(&tmp_path, path)?;

    // 4. Fsync the parent directory so the new directory entry is durable.
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Fsync a directory to make metadata changes (renames, new entries) durable.
///
/// On Unix, this opens the directory and calls `fsync()`.
///
/// On Windows, `fs::File::open()` on a directory requires
/// `FILE_FLAG_BACKUP_SEMANTICS` which the standard library does not set,
/// causing `PermissionDenied` (OS error 5). NTFS journals metadata
/// operations so the atomic rename in [`atomic_write`] already provides
/// sufficient durability guarantees. This is the same approach used by
/// RocksDB and SQLite on Windows.
fn fsync_dir(dir: &Path) -> Result<(), CheckpointError> {
    #[cfg(unix)]
    {
        let d = fs::File::open(dir)?;
        d.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Reads and deserialises a JSON file, mapping errors to
/// [`CheckpointError`].
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, CheckpointError> {
    let data = fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CheckpointError::NotFound
        } else {
            CheckpointError::IoError(e)
        }
    })?;
    serde_json::from_str(&data).map_err(|e| {
        CheckpointError::InvalidState(format!("invalid JSON in {}: {e}", path.display()))
    })
}

/// Returns the current time as an ISO 8601 string (UTC-ish, no TZ offset).
///
/// Uses a minimal implementation that avoids pulling in `chrono` or `time`.
fn now_iso8601() -> String {
    // std::time::SystemTime → duration since UNIX_EPOCH → manual formatting.
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();

    // Cheap UTC breakdown (no leap-second handling needed for timestamps).
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days → year/month/day via a civil-calendar algorithm.
    let (y, m, d) = civil_from_days(days as i64);

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Converts a day count since 1970-01-01 to (year, month, day).
///
/// Algorithm from Howard Hinnant's `chrono`-compatible date library.
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era [0, 399]
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use std::fs;

    // -- helpers ------------------------------------------------------------

    fn sample_token(val: u128) -> CheckpointToken {
        CheckpointToken::new(val)
    }

    fn sample_index_info() -> IndexRecoveryInfo {
        IndexRecoveryInfo {
            format_version: 2,
            version: 1,
            table_size: 1 << 16,
            num_ht_bytes: (1 << 16) * 64,
            num_ofb_bytes: 4096,
            num_buckets: (1 << 16) + 64,
            start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
            final_logical_address: LogicalAddress::new(Page(10), Offset(512)),
        }
    }

    fn sample_log_info() -> LogRecoveryInfo {
        LogRecoveryInfo {
            format_version: 2,
            version: 1,
            checkpoint_type: CheckpointType::Snapshot,
            begin_address: LogicalAddress::new(Page(0), Offset(64)),
            flushed_until_address: LogicalAddress::new(Page(5), Offset(0)),
            final_address: LogicalAddress::new(Page(10), Offset(512)),
            head_address: LogicalAddress::new(Page(2), Offset(0)),
            snapshot_start_address: LogicalAddress::new(Page(5), Offset(0)),
            snapshot_final_address: LogicalAddress::new(Page(10), Offset(512)),
            use_snapshot_file: true,
            object_log_segment_count: 3,
        }
    }

    fn sample_sessions() -> Vec<SessionRecoveryInfo> {
        vec![
            SessionRecoveryInfo {
                session_id: 0,
                serial_number: 100,
                excluded_serial_numbers: vec![50, 75],
            },
            SessionRecoveryInfo {
                session_id: 1,
                serial_number: 200,
                excluded_serial_numbers: vec![],
            },
        ]
    }

    // -- round-trip ----------------------------------------------------------

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(1);
        let idx = sample_index_info();
        let log = sample_log_info();
        let sessions = sample_sessions();

        store
            .write_checkpoint_metadata(&token, &idx, &log, &sessions)
            .unwrap();

        let meta = store.read_checkpoint_metadata(&token).unwrap();
        assert_eq!(meta.token, token);
        assert_eq!(meta.checkpoint_type, CheckpointType::Snapshot);
        assert_eq!(meta.index_info, idx);
        assert_eq!(meta.log_info, log);
        assert_eq!(meta.session_infos, sessions);
        assert!(!meta.created_at.is_empty());
    }

    #[test]
    fn read_nonexistent_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let err = store
            .read_checkpoint_metadata(&sample_token(999))
            .unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    // -- list ---------------------------------------------------------------

    #[test]
    fn list_checkpoints_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let list = store.list_checkpoints().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_checkpoints_single() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(42);
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();
        let list = store.list_checkpoints().unwrap();
        assert_eq!(list, vec![token]);
    }

    #[test]
    fn list_checkpoints_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let tokens: Vec<_> = (1..=3).map(sample_token).collect();
        for t in &tokens {
            store
                .write_checkpoint_metadata(
                    t,
                    &sample_index_info(),
                    &sample_log_info(),
                    &sample_sessions(),
                )
                .unwrap();
        }
        let list = store.list_checkpoints().unwrap();
        assert_eq!(list, tokens);
    }

    // -- latest -------------------------------------------------------------

    #[test]
    fn latest_checkpoint_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        assert_eq!(store.latest_checkpoint().unwrap(), None);
    }

    #[test]
    fn latest_checkpoint_returns_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());

        // Write two checkpoints; the second is written later so its timestamp
        // is >= the first.
        let t1 = sample_token(1);
        let t2 = sample_token(2);
        store
            .write_checkpoint_metadata(
                &t1,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();
        // Nudge time forward by patching the descriptor.
        store
            .write_checkpoint_metadata(
                &t2,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();

        // Manually overwrite t2's descriptor with a future timestamp to
        // guarantee it sorts later regardless of wall-clock granularity.
        let t2_desc_path = dir
            .path()
            .join("checkpoints")
            .join(t2.to_string())
            .join("checkpoint.json");
        let mut desc: CheckpointDescriptor =
            serde_json::from_str(&fs::read_to_string(&t2_desc_path).unwrap()).unwrap();
        desc.created_at = "2099-12-31T23:59:59Z".to_string();
        atomic_write(
            &t2_desc_path,
            serde_json::to_string_pretty(&desc).unwrap().as_bytes(),
        )
        .unwrap();

        let latest = store.latest_checkpoint().unwrap();
        assert_eq!(latest, Some(t2));
    }

    // -- delete -------------------------------------------------------------

    #[test]
    fn delete_checkpoint_removes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(7);
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();
        assert!(store.read_checkpoint_metadata(&token).is_ok());
        store.delete_checkpoint(&token).unwrap();
        assert!(matches!(
            store.read_checkpoint_metadata(&token),
            Err(CheckpointError::NotFound)
        ));
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let err = store.delete_checkpoint(&sample_token(999)).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    // -- corrupt file detection ---------------------------------------------

    #[test]
    fn corrupt_descriptor_detected() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(10);
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();

        // Corrupt the master descriptor.
        let desc_path = dir
            .path()
            .join("checkpoints")
            .join(token.to_string())
            .join("checkpoint.json");
        fs::write(&desc_path, b"NOT VALID JSON").unwrap();

        let err = store.read_checkpoint_metadata(&token).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    #[test]
    fn corrupt_index_recovery_detected() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(11);
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();

        // Corrupt the index recovery file.
        let idx_path = dir
            .path()
            .join("checkpoints")
            .join(token.to_string())
            .join("index.recovery.json");
        fs::write(&idx_path, b"{\"bogus\": true}").unwrap();

        let err = store.read_checkpoint_metadata(&token).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    // -- atomic write: temp file cleanup ------------------------------------

    #[test]
    fn temp_files_cleaned_up_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(20);
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();

        // Walk the entire checkpoint dir — no .tmp files should remain.
        let ckpt_dir = dir.path().join("checkpoints").join(token.to_string());
        let tmp_files: Vec<_> = walkdir(&ckpt_dir)
            .into_iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        assert!(tmp_files.is_empty(), "unexpected temp files: {tmp_files:?}");
    }

    // -- concurrent writes to different checkpoints -------------------------

    #[test]
    fn concurrent_writes_to_different_checkpoints() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        let handles: Vec<_> = (100..110)
            .map(|i| {
                let base = base.clone();
                thread::spawn(move || {
                    let store = CheckpointMetadataStore::new(base);
                    let token = sample_token(i);
                    store
                        .write_checkpoint_metadata(
                            &token,
                            &sample_index_info(),
                            &sample_log_info(),
                            &sample_sessions(),
                        )
                        .unwrap();
                    token
                })
            })
            .collect();

        let tokens: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let store = CheckpointMetadataStore::new(base);
        let list = store.list_checkpoints().unwrap();
        for t in &tokens {
            assert!(list.contains(t), "missing token {t}");
        }
        assert_eq!(list.len(), 10);
    }

    // -- session recovery persistence ---------------------------------------

    #[test]
    fn empty_sessions_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(30);
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &sample_log_info(), &[])
            .unwrap();
        let meta = store.read_checkpoint_metadata(&token).unwrap();
        assert!(meta.session_infos.is_empty());
    }

    #[test]
    fn many_sessions_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(31);
        let sessions: Vec<_> = (0..20)
            .map(|i| SessionRecoveryInfo {
                session_id: i,
                serial_number: u64::from(i) * 1000,
                excluded_serial_numbers: vec![u64::from(i) * 100],
            })
            .collect();
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &sample_log_info(), &sessions)
            .unwrap();
        let meta = store.read_checkpoint_metadata(&token).unwrap();
        assert_eq!(meta.session_infos, sessions);
    }

    // -- fold-over checkpoint type round-trip --------------------------------

    #[test]
    fn fold_over_type_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(40);
        let log = LogRecoveryInfo {
            checkpoint_type: CheckpointType::FoldOver,
            ..sample_log_info()
        };
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &log, &sample_sessions())
            .unwrap();
        let meta = store.read_checkpoint_metadata(&token).unwrap();
        assert_eq!(meta.checkpoint_type, CheckpointType::FoldOver);
        assert_eq!(meta.log_info.checkpoint_type, CheckpointType::FoldOver);
    }

    // -- overwrite existing checkpoint --------------------------------------

    #[test]
    fn overwrite_existing_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let token = sample_token(50);

        // First write.
        store
            .write_checkpoint_metadata(
                &token,
                &sample_index_info(),
                &sample_log_info(),
                &sample_sessions(),
            )
            .unwrap();

        // Overwrite with different data.
        let new_idx = IndexRecoveryInfo {
            version: 99,
            ..sample_index_info()
        };
        store
            .write_checkpoint_metadata(&token, &new_idx, &sample_log_info(), &[])
            .unwrap();

        let meta = store.read_checkpoint_metadata(&token).unwrap();
        assert_eq!(meta.index_info.version, 99);
        assert!(meta.session_infos.is_empty());
    }

    // -- CheckpointMetadata serde round-trip --------------------------------

    #[test]
    fn checkpoint_metadata_serde_round_trip() {
        let meta = CheckpointMetadata {
            token: sample_token(60),
            checkpoint_type: CheckpointType::Snapshot,
            index_info: sample_index_info(),
            log_info: sample_log_info(),
            session_infos: sample_sessions(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: CheckpointMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }

    // -- list ignores directories without checkpoint.json -------------------

    #[test]
    fn list_ignores_incomplete_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());

        // Write a real checkpoint.
        let token = sample_token(70);
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &sample_log_info(), &[])
            .unwrap();

        // Create a directory without a descriptor (simulates incomplete write).
        fs::create_dir_all(dir.path().join("checkpoints").join("fake-checkpoint-dir")).unwrap();

        let list = store.list_checkpoints().unwrap();
        assert_eq!(list, vec![token]);
    }

    // -- helper: recursive walk ---------------------------------------------

    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    result.extend(walkdir(&path));
                } else {
                    result.push(path);
                }
            }
        }
        result
    }
}
