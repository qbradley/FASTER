# FASTER-Rust Compaction Scanner Analysis - Complete Documentation

This directory contains three comprehensive documents analyzing the FASTER-Rust codebase to facilitate building a compaction scanner.

## Documents Provided

### 1. **QUICK_REFERENCE.md** (8.2 KB, 277 lines)
**START HERE** — Fast lookup guide with code snippets.

**Contents:**
- All 10 core types with type definitions and line numbers
- Key methods for each type in table format
- Implementation tips with runnable code examples
- Version chain walking example
- Key constants lookup table
- Test file references

**Use when:** You need to quickly find a type name, method signature, or see how to use something.

---

### 2. **COMPACTION_SCANNER_ANALYSIS.md** (27 KB, 636 lines)
**DETAILED REFERENCE** — Complete technical breakdown with full context.

**Sections:**
1. **Address Types** (address.rs)
   - LogicalAddress internals
   - Page/Offset newtypes
   - Special sentinel values
   - All methods with line numbers

2. **Record Format** (record/*.rs)
   - RecordInfo bit layout (8 bytes, 5 fields)
   - RecordLayout computation
   - Serialization/deserialization functions
   - Record size calculation

3. **Hybrid Log** (hybrid_log/*.rs)
   - HybridLogAllocator with all fields
   - Address region invariants (begin ≤ head ≤ ro ≤ sro ≤ tail)
   - RecordAccessor and MutableRecordAccessor
   - VersionChainIterator for walking version chains
   - ScanOptions/ScanRecord/LogScanIterator
   - PageState lifecycle
   - Page size constants

4. **Hash Index** (hash/*.rs)
   - HashIndex concurrent lookup structure
   - HashTable latch-free implementation
   - HashBucket with 7 inline entries
   - HashBucketEntry packed layout (48-bit addr, 14-bit tag, tentative flag)
   - Two-phase insert protocol

5. **Store Operations** (store/*.rs)
   - FasterKv main store struct
   - FasterKvConfig with all parameters
   - FasterSession (thread-affine, !Send)
   - CRUD operation signatures (read, upsert, delete, rmw)
   - Functions trait and SimpleFunctions
   - Session lifecycle

6. **Existing Tests** (tests/)
   - Integration test examples
   - Store usage patterns

7. **Implementation Considerations**
   - Forward log iteration strategy
   - Using LogScanIterator vs manual iteration
   - Version chain traversal
   - Raw memory access safety

**Use when:** You need comprehensive understanding of a subsystem, detailed field names, or exact bit layouts.

---

### 3. **FILE_REFERENCE.txt** (14 KB, 303 lines)
**LOCATE-ANYTHING GUIDE** — File-by-file index with exact line numbers.

**Organization:**
- Grouped by functional area (ADDRESS, RECORD FORMAT, HYBRID LOG, etc.)
- Every public type, method, constant listed
- Exact line number for each item
- File size reference
- Quick lookup table at end for all constants

**Example format:**
```
FILE: address.rs (26.5 KB)
  Line 50-72:   Constants (ADDRESS_BITS, OFFSET_BITS, MAX_OFFSET, etc.)
  Line 226:     LogicalAddress::new() constructor
  Line 269:     LogicalAddress::page()
```

**Use when:** You need to jump directly to code, know the exact line number, or navigate from one file to another.

---

## Quick Navigation

### Finding Information
1. **"Where is LogicalAddress?"** → FILE_REFERENCE.txt § ADDRESS TYPES
2. **"What's the RecordInfo bit layout?"** → COMPACTION_SCANNER_ANALYSIS.md § RECORD FORMAT
3. **"How do I iterate a log?"** → QUICK_REFERENCE.md § Compaction Scanner Implementation Tips
4. **"What's in the hash bucket?"** → COMPACTION_SCANNER_ANALYSIS.md § HASH INDEX § Hash Bucket Structure
5. **"How do I create a session?"** → QUICK_REFERENCE.md § 9. Store (line 369 example)

### By Task
**Building a scanner:**
1. Read QUICK_REFERENCE.md sections 1-6 (types overview)
2. Review QUICK_REFERENCE.md "Implementation Tips" code examples
3. Reference FILE_REFERENCE.txt when you need exact line numbers
4. Use COMPACTION_SCANNER_ANALYSIS.md for deep dives

**Understanding a specific type:**
1. Check QUICK_REFERENCE.md type table
2. Get line numbers from FILE_REFERENCE.txt
3. Read full details in COMPACTION_SCANNER_ANALYSIS.md

**Looking up a constant:**
1. Check "Key Constants" table in QUICK_REFERENCE.md
2. Or search COMPACTION_SCANNER_ANALYSIS.md for constant name
3. Or check quick lookup at end of FILE_REFERENCE.txt

---

## Key Facts You Should Know

### Architecture
- **FASTER** is a hybrid log-structured key-value store
- **Hybrid log**: In-memory + on-disk regions (address spaces)
- **Address space**: 48 bits (23-bit page + 25-bit offset) = 256 TB total
- **Page size**: 32 MB (2^25 bytes)
- **Records**: 8-byte aligned, versioned, chainable

### Data Structure
- **LogicalAddress**: 48-bit identifier into hybrid log
- **RecordInfo**: 8-byte header with previous-address chain, version, flags
- **HashIndex**: Latch-free concurrent hash table (7 entries/bucket + overflow)
- **Version chains**: Records linked via LogicalAddress pointers in RecordInfo

### Critical Flags
- **Tombstone** (bit 62): Record deleted (skip in compaction)
- **Invalid** (bit 61): Record superseded (skip in compaction)
- **Final** (bit 63): CPR coordination (rare, internal use)

### Threading
- **FasterKv**: Shared, Send + Sync
- **FasterSession**: Thread-local (!Send), must call begin_unsafe() for epochs
- **RecordAccessor**: Holds raw pointers, lifetime-tied to epoch guard

### Memory Safety
- All record access via `RecordAccessor::from_log()` (safe wrapper)
- Raw pointers only obtained via `HybridLogAllocator::get_physical_address()` (safe)
- Epoch protection prevents page eviction during record access

---

## File Statistics

| File | Size | Lines | Purpose |
|------|------|-------|---------|
| QUICK_REFERENCE.md | 8.2 KB | 277 | Fast lookup, code examples |
| COMPACTION_SCANNER_ANALYSIS.md | 27 KB | 636 | Detailed breakdown |
| FILE_REFERENCE.txt | 14 KB | 303 | Line-by-line index |
| **Total** | **49 KB** | **1216** | |

---

## Source Code Statistics

Core FASTER-Rust modules analyzed:

| Module | File | Size | Key Types | 
|--------|------|------|-----------|
| address | address.rs | 26.5 KB | LogicalAddress, Page, Offset, AtomicLogicalAddress |
| record | record/*.rs | 49 KB total | RecordInfo, RecordLayout, Key/Value traits |
| hybrid_log | hybrid_log/*.rs | 155 KB total | HybridLogAllocator, RecordAccessor, LogScanIterator |
| hash | hash/*.rs | 2+ MB total | HashIndex, HashTable, HashBucket, HashBucketEntry |
| store | store/*.rs | 330 KB total | FasterKv, FasterSession, Functions trait |
| tests | tests/*.rs | Examples | Integration tests, usage patterns |

---

## Implementation Checklist for Compaction Scanner

- [ ] Understand address space layout (begin ≤ head ≤ ro ≤ sro ≤ tail)
- [ ] Know record header format (8 bytes, 5 packed fields)
- [ ] Implement forward log iteration (handle page boundaries)
- [ ] Filter records (skip tombstones, invalid)
- [ ] Follow version chains (traverse backwards via LogicalAddress)
- [ ] Use RecordAccessor for safe access (not raw pointers)
- [ ] Check epoch protection during iteration
- [ ] Handle in-memory vs on-disk records
- [ ] Know when records can be compacted
- [ ] Test with SimpleFunctions<u64, u64>

---

## Questions Answered

### "What are the key types I need?"
LogicalAddress, Page, Offset, RecordInfo, RecordLayout, HybridLogAllocator, RecordAccessor, HashIndex, FasterKv, FasterSession

### "How big is a record on disk?"
8 bytes (header) + key + padding + value + padding, rounded to 8-byte alignment

### "How do I find all versions of a key?"
Follow the `previous_address` chain in RecordInfo until you hit INVALID sentinel

### "What fields are in RecordInfo?"
previous_address (48-bit), checkpoint_version (13-bit), invalid, tombstone, final

### "How many buckets in the hash index?"
2^log2_size, default: 2^20 = 1,048,576 buckets

### "What's the page size?"
2^25 = 33,554,432 bytes (32 MB)

### "Can I scan in parallel?"
FasterSession is !Send, one per thread. Use epoch protection. LogScanIterator is thread-safe (&self).

### "Where are the tests?"
tests/integration_basics.rs, tests/e2e_tests.rs, tests/common/mod.rs

---

## References & Conventions

**Notation:**
- `|` = inclusive range (e.g., head ≤ addr ≤ tail)
- `|)` = inclusive-exclusive range (e.g., begin ≤ addr < head)
- File:Line = path/to/file.rs:123 for exact location

**Terminology:**
- **Hybrid log**: In-memory + on-disk hybrid structure
- **Logical address**: 48-bit identifier (page + offset)
- **Physical address**: Actual memory pointer (runtime-derived)
- **Version chain**: Linked list of record versions via previous_address
- **Tombstone**: Marked-for-deletion record (acts as "not found")
- **Invalid**: Superseded record (there's a newer version at tail)
- **Epoch**: Synchronization point for safe memory reclamation

---

## Document Maintenance

**Last Updated:** March 6, 2025
**FASTER-Rust Version:** Latest main branch
**Scope:** rust/crates/faster-core/src/
**Focus:** Compaction scanner requirements

---

**For questions, refer to:**
1. QUICK_REFERENCE.md for quick answers
2. COMPACTION_SCANNER_ANALYSIS.md for deep dives
3. FILE_REFERENCE.txt for exact locations
4. Source code at /home/azureuser/FASTER/rust/crates/faster-core/src/

