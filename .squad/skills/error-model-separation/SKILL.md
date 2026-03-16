# Skill: Error Model Separation — Status vs Error

## When to Use

When adding new operations or error conditions to FASTER APIs.

## Pattern

FASTER uses two distinct types for operation outcomes:

### 1. `OperationStatus` — Control Flow Signals
```rust
pub enum OperationStatus {
    Ok,
    Pending,        // I/O required, retry later
    NotFound,       // Key doesn't exist (not an error)
    InPlaceUpdated, // RMW succeeded in-place
    Copied,         // Record was copied to tail
    // ... etc
}
```
**When to use:** Operational outcomes that callers handle as normal control flow.

### 2. `FasterError` — True Errors
```rust
pub enum FasterError {
    IoError(std::io::Error),
    ChecksumMismatch { expected: u32, actual: u32 },
    CorruptedData(String),
    InvalidArgument(String),
    // ... etc
}
```
**When to use:** Exceptional conditions (I/O failure, data corruption) that indicate something went wrong.

## Decision Tree

```
Is this outcome part of normal operation?
├─ YES → Use OperationStatus
│  Examples:
│  - Key not found during read
│  - I/O pending, caller should retry
│  - Record was copied vs updated in-place
│
└─ NO → Use FasterError
   Examples:
   - Disk I/O failed
   - Checksum verification failed
   - Invalid configuration parameter
```

## Implementation Details

### Manual trait impls (no thiserror)
```rust
impl std::fmt::Display for FasterError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::IoError(e) => write!(f, "I/O error: {}", e),
            // ~40 lines total for small enums
        }
    }
}

impl std::error::Error for FasterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IoError(e) => Some(e),
            _ => None,
        }
    }
}
```

### Why no thiserror?
For small enums (~10 variants), manual impls are ~40 lines and avoid the dependency. Clear ownership of error messages.

## Examples

### Correct Usage
```rust
// Control flow — use OperationStatus
pub fn read(&self, key: &K) -> OperationStatus {
    if !self.hash_index.contains(key) {
        return OperationStatus::NotFound;  // ✅ Expected outcome
    }
    // ...
}

// True error — use FasterError
pub fn open_checkpoint(path: &Path) -> Result<Checkpoint, FasterError> {
    let file = std::fs::File::open(path)
        .map_err(FasterError::IoError)?;  // ✅ Exceptional condition
    // ...
}
```

### Incorrect Usage
```rust
// ❌ Don't use Error for expected outcomes
pub fn read(&self, key: &K) -> Result<Value, FasterError> {
    if !self.hash_index.contains(key) {
        return Err(FasterError::NotFound);  // ❌ Makes caller think it's exceptional
    }
    // ...
}
```

## Variant Set

`OperationStatus` merges semantics from:
- C++ `Status` enum
- C# `Status` enum  
- Architecture-specific states like `OkKind::Deleted`

Result: One flat enum with all possible operational outcomes.

## Confidence

High

## Learned From

- History.md lines 46-47: "Status vs Error separation" learning
- Session log: "Operational outcomes (Ok, Pending, NotFound) are control-flow signals, not errors"
- Codebase analysis: Manual Display/Error/From impls are ~40 lines
