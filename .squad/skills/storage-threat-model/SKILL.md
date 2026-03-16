# Skill: Storage Engine Threat Model

## When to Use
When reviewing or designing any component of a persistent storage system (hash map, log, index, recovery, compaction, checkpoint).

## Domain-Specific Threats for FASTER

### Threat Category 1: Untrusted Disk Data
**Assumption:** Disk contents may be maliciously crafted or corrupted.

**Attack Surface:**
- Checkpoint files (index, metadata JSON)
- Log segment files
- Page trailers (CRC, write size)
- Serialized records (key length, value length, tombstone flags)

**Vulnerabilities to Check:**
- [ ] Integer overflow in length fields (u32 read size → allocation)
- [ ] CRC bypass (format version check, magic bytes)
- [ ] Malformed JSON (infinite recursion, resource exhaustion)
- [ ] Buffer overruns from unchecked slice construction
- [ ] Invalid enum discriminants from deserialization

**Mitigations:**
- Validate ALL lengths before allocation
- CRC verification BEFORE deserialization
- Magic byte + version checks at file header
- Fuzz targets for every deserialization path

### Threat Category 2: FFI Boundary Violations
**Assumption:** C callers may pass invalid pointers, violate thread affinity, or race with dispose.

**Attack Surface:**
- Null pointers in input arguments
- Dangling session pointers (dispose during pending I/O)
- Thread-affinity violations (session used from wrong thread)
- Invalid enum values from C enums
- Integer truncation (u64 address → u32 in FFI)

**Vulnerabilities to Check:**
- [ ] Missing null checks on FFI entry
- [ ] Panic unwinding across FFI (instant UB)
- [ ] Use-after-free if session disposed with pending callbacks
- [ ] Data race from Send+Sync impl without enforcement
- [ ] Integer truncation on address/size parameters

**Mitigations:**
- `catch_unwind` on ALL `extern "C"` functions
- Null checks BEFORE dereference
- Debug-mode thread ID validation for thread-affine types
- Ensure `dispose` drains pending I/O before free

### Threat Category 3: Concurrent Use-After-Free
**Assumption:** Readers may race with epoch-based reclamation.

**Attack Surface:**
- Allocator free list (ABA problem with 16-bit tag)
- Epoch-protected pointers (use after epoch advance)
- Page eviction during read-from-disk operation
- Compaction address swaps racing with readers

**Vulnerabilities to Check:**
- [ ] ABA counter too narrow (16-bit wraps in ~65K frees)
- [ ] Missing epoch protection on pointer dereference
- [ ] Premature deallocation (freed before epoch drain)
- [ ] CAS loop without ABA mitigation

**Mitigations:**
- Epoch protection for ALL cross-thread pointer access
- 16-bit ABA tag documented as "sufficient with epoch, fragile without"
- Miri tests for allocator lifecycle
- Loom tests for concurrent data structures

### Threat Category 4: Resource Exhaustion
**Assumption:** Adversary controls input size and can amplify resource usage.

**Attack Surface:**
- Unbounded log growth (no compaction trigger)
- Hash table overflow chain (degenerate hash collision)
- Fuzz input size (OOM from large deserialize)
- Pending I/O queue (unbounded async operations)

**Vulnerabilities to Check:**
- [ ] No limit on overflow chain length
- [ ] Compaction trigger only on size, not time
- [ ] Fuzz targets without input size caps
- [ ] Async operations queued without backpressure

**Mitigations:**
- Cap overflow chain traversal in hash table
- Fuzz targets: `if input.len() > LIMIT { return; }`
- Monitor pending I/O count (warn if >threshold)
- Document expected compaction cadence

### Threat Category 5: Information Disclosure
**Assumption:** Deleted/overwritten data may leak to new operations.

**Attack Surface:**
- Uninitialized memory in allocations
- Tombstone records not zeroed
- Index entries pointing to freed pages
- Debug logs containing key/value data

**Vulnerabilities to Check:**
- [ ] Allocator returns unzeroed memory
- [ ] Tombstone flag respected in all read paths
- [ ] Compaction doesn't copy tombstones
- [ ] Error messages don't leak keys/values

**Mitigations:**
- Document whether allocator zeroes (or caller must)
- Tombstone checks in LogScanIterator and compaction
- Miri tests verify no uninitialized reads
- Sanitize error messages (no user data in logs)

## Prioritization by Attack Feasibility

### P0: File-based attacks (checkpoint/recovery)
**Feasibility:** HIGH — attacker with filesystem access can plant malicious files.
**Fuzz targets required:** checkpoint_recovery, log_recovery, page_trailer

### P1: FFI attacks (C caller bugs)
**Feasibility:** MEDIUM — requires control of C code, but C code is often less safe.
**Fuzz targets required:** FFI boundary operations

### P2: Concurrent UAF (epoch bugs)
**Feasibility:** LOW — requires precise timing and epoch configuration.
**Testing required:** Loom shuttle, Miri on allocator

### P3: Resource exhaustion
**Feasibility:** MEDIUM — depends on whether attacker controls key distribution.
**Mitigations:** Production monitoring, compaction tuning

### P4: Information disclosure
**Feasibility:** LOW — requires read access after delete, same process.
**Mitigations:** Documentation, code review

## Checklist for New Storage Features
- [ ] Threat model updated for new attack surface
- [ ] Fuzz target for any new deserialization
- [ ] Miri test for any new unsafe code
- [ ] FFI null checks + catch_unwind for new C functions
- [ ] Resource limits documented (max size, max chain, max pending)
- [ ] Integration test simulating adversarial input

## Confidence: high

## Learned From
- 2026-03-06: Production security audit — identified FFI panic safety as Critical
- 2026-03-11: Fuzz target expansion — discovered need for file-based fuzzing
- **Domain insight:** Storage engines have unique threat model vs. web services. The attacker often has filesystem access (backup restore, volume mount) or controls key distribution (hash flooding).
