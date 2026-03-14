// src/log.rs

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use dashmap::DashMap;

/// Represents a record in the HyperLog.
pub struct Record {
    value: Vec<u8>,
}

/// A high-performance, lock-free log structure.
pub struct Log {
    /// Atomic counter for the next write position.
    next_pos: AtomicUsize,
    /// In-memory index mapping keys to their positions in the log.
    index: Arc<Index>,
    /// Storage for log records.
    records: Arc<DashMap<usize, Arc<Record>>>,
}

impl Log {
    /// Initializes a new Log instance.
    pub fn new() -> Self {
        Log {
            next_pos: AtomicUsize::new(0),
            index: Arc::new(Index::new()),
            records: Arc::new(DashMap::new()),
        }
    }

    /// Adds a record to the log in a thread-safe manner.
    pub fn add_record(&self, key: usize, value: Vec<u8>) {
        let pos = self.next_pos.fetch_add(1, Ordering::SeqCst);
        let record = Arc::new(Record { value });
        self.records.insert(pos, record.clone());
        self.index.insert(key, pos);
    }

    /// Reads a record from the log efficiently using the in-memory index.
    pub fn read_record(&self, key: usize) -> Option<Vec<u8>> {
        if let Some(pos) = self.index.get(&key) {
            self.records.get(&pos).map(|entry| entry.value().clone().value)
        } else {
            None
        }
    }
}

/// A simple concurrent in-memory index.
struct Index {
    map: DashMap<usize, usize>,
}

impl Index {
    /// Initializes a new Index instance.
    fn new() -> Self {
        Index {
            map: DashMap::new(),
        }
    }

    /// Inserts a key-position pair into the index.
    fn insert(&self, key: usize, pos: usize) {
        self.map.insert(key, pos);
    }

    /// Retrieves the position for a given key.
    fn get(&self, key: &usize) -> Option<usize> {
        self.map.get(key).map(|entry| *entry.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_add_and_read_record() {
        let log = Log::new();
        log.add_record(1, vec![10, 20, 30]);
        let value = log.read_record(1);
        assert_eq!(value, Some(vec![10, 20, 30]));
    }

    #[test]
    fn test_concurrent_additions() {
        let log = Arc::new(Log::new());
        let mut handles = vec![];

        for i in 0..10 {
            let log_clone = Arc::clone(&log);
            handles.push(thread::spawn(move || {
                log_clone.add_record(i, vec![i as u8; 3]);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 0..10 {
            let value = log.read_record(i);
            assert_eq!(value, Some(vec![i as u8; 3]));
        }
    }

    #[test]
    fn test_read_nonexistent_record() {
        let log = Log::new();
        let value = log.read_record(999);
        assert_eq!(value, None);
    }
}
