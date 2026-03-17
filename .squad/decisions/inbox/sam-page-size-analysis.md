# Page Size Configurability Analysis

**Author:** Sam (Systems & Storage Expert)  
**Requested by:** qbradley  
**Date:** 2026-03-16  
**Status:** Analysis complete — recommendation included

---

## Executive Summary

Page size is hard-coded at 32 MB (`OFFSET_BITS = 25`) because it is **baked into the 48-bit logical address encoding** — the fundamental on-disk and in-memory identity for every record. The C++ and Rust implementations both hard-code this as a compile-time constant. The C# implementation, by contrast, makes it a **runtime parameter** (`LogSettings.PageSizeBits`), proving it *can* be done — but the C# version doesn't persist addresses across restarts in the same bit-packed format.

For DST, the best path is a **feature flag** (`#[cfg(feature = "small-pages")]`) that sets `OFFSET_BITS = 16` (64 KB pages). This is low-risk, zero blast radius for production, and gives DST exactly what it needs: page fills, flushes, and evictions within a ~2 MB test workload.

---

## 1. Where OFFSET_BITS Is Defined

**Single source of truth:**

```
rust/crates/faster-core/src/address.rs:56
    pub const OFFSET_BITS: u32 = 25;
```

This drives all derived constants at `address.rs:58-68`:
- `PAGE_BITS = ADDRESS_BITS - OFFSET_BITS` → 23 bits (line 59)
- `MAX_OFFSET = (1 << OFFSET_BITS) - 1` → 33,554,431 (line 62)
- `MAX_PAGE = (1 << PAGE_BITS) - 1` → 8,388,607 (line 65)
- `OFFSET_MASK = (1 << OFFSET_BITS) - 1` (line 190)

**There are 30+ usage sites** across the codebase. Every one computes `page_size = 1 << OFFSET_BITS` locally.

---

## 2. Why It Must Be a Compile-Time Constant

### 2a. Address Encoding (the fundamental constraint)

`LogicalAddress` is a 48-bit packed value — `(page << OFFSET_BITS) | offset`. This is the identity for every record in:
- The hash index (entries point to `LogicalAddress`)
- RecordInfo's `prev_address` chain (48 bits, `address.rs:6`)
- Checkpoint metadata (`LogRecoveryInfo` serializes raw `LogicalAddress` values)
- Device I/O offsets (`device_offset = page * (1 << OFFSET_BITS) + offset`)

```
address.rs:232    Self((p << OFFSET_BITS) | o)      // encode
address.rs:270    Page((self.0 >> OFFSET_BITS) as u32)  // decode page
address.rs:190    const OFFSET_MASK: u64 = (1u64 << OFFSET_BITS) - 1;  // decode offset
```

These are **hot-path bit shifts** — the compiler must know the shift amount to emit optimal instructions. Making this a runtime value would:
1. Replace constant shifts with variable shifts (minor perf hit)
2. Break the `const fn` constructors (`LogicalAddress::new` is `const`)
3. Require threading a page-size parameter through every subsystem

### 2b. Compile-Time Assertions

```
allocator.rs:107-109
    const _: () = assert!(
        ITEMS_PER_PAGE_BITS <= crate::address::OFFSET_BITS,
        "ITEMS_PER_PAGE must fit in LogicalAddress offset field"
    );
```

The overflow bucket allocator (`MallocFixedPageSize`) uses `ITEMS_PER_PAGE_BITS = 20`, which must be ≤ `OFFSET_BITS`. This is a compile-time check.

### 2c. C++ Uses Identical Pattern

```
cc/src/core/address.h:36    static constexpr uint64_t kOffsetBits = 25;
cc/src/core/address.h:128   uint64_t offset_ : kOffsetBits;  // C++ bitfield
```

The C++ `Address` uses **bitfields** — the field width *must* be a compile-time constant. The Rust version packs manually but follows the exact same layout for wire compatibility.

---

## 3. What Depends on Page Size

| Subsystem | File:Line | How page_size is used |
|-----------|-----------|----------------------|
| **Address encoding** | `address.rs:232,270` | Shift/mask for page↔offset packing |
| **Log allocator** | `log_allocator.rs:77` | `page_size = 1 << OFFSET_BITS`; page-fill detection, next-page transition |
| **Page table** | `page.rs:31` | `DEFAULT_PAGE_SIZE = 1 << OFFSET_BITS`; frame allocation size |
| **Flush** | `flush.rs:487` | Page-sized I/O writes, trailer placement |
| **Record ops** | `record_ops.rs:37` | `safe_record_size()` — bytes remaining on page |
| **Store operations** | `operations.rs:130` | `safe_read_record_size()` — page boundary safety |
| **Log scan** | `scan.rs:179,569` | Page boundary detection during iteration |
| **Compaction scanner** | `scanner.rs:145` | Page boundary detection during record enumeration |
| **Begin-address advance** | `begin_address.rs:113,277` | `device_offset = page * page_size + offset` for device truncation |
| **Recovery** | `log_recovery.rs:56` | `PAGE_SIZE = 1 << OFFSET_BITS`; page-aligned reads, CRC trailer location |
| **Page trailer** | `page.rs:write_size()` | `min(aligned, page_size)` — trailer position capped at page boundary |
| **Overflow allocator** | `allocator.rs:108` | `ITEMS_PER_PAGE_BITS ≤ OFFSET_BITS` compile-time check |

---

## 4. Could It Be a Const Generic?

```rust
// Hypothetical:
pub struct FasterKv<const OFFSET_BITS: u32, D: Device> { ... }
```

**Blast radius: catastrophic.** Every type that touches an address would need the const generic:
- `LogicalAddress<OFFSET_BITS>`, `Page<OFFSET_BITS>`, `Offset<OFFSET_BITS>`
- `HybridLogAllocator<OFFSET_BITS>`, `PageTable<OFFSET_BITS>`, `PageFlusher<OFFSET_BITS>`
- `RecordInfo<OFFSET_BITS>` (prev_address is a LogicalAddress)
- All operations, sessions, recovery, compaction, scan...

This would infect **every generic boundary** in the crate. Compile times would explode. Type ergonomics would be terrible. **Not recommended.**

---

## 5. Could It Be a Feature Flag? ✅ RECOMMENDED

```rust
// address.rs — proposed:
#[cfg(feature = "small-pages")]
pub const OFFSET_BITS: u32 = 16;  // 64 KB pages

#[cfg(not(feature = "small-pages"))]
pub const OFFSET_BITS: u32 = 25;  // 32 MB pages (production default)
```

**Why this works:**
- `OFFSET_BITS` remains a compile-time constant — all bit shifts, const fns, and compile-time assertions still work
- Zero code changes outside `address.rs` — every site already reads `OFFSET_BITS`
- The `simulation` feature already exists and uses the same `cfg` pattern (`sync.rs:60-107`)
- DST tests compile with `--features simulation,small-pages` — get 64 KB pages that fill with ~64 records instead of ~500K
- Production builds never see the flag — no risk

**What OFFSET_BITS=16 gives you:**
- Page size: 64 KB (vs 32 MB)
- At 32-byte records: ~2,048 records fill a page
- A 2 MB workload fills ~32 pages → flush/eviction pipeline fully exercised
- `MAX_PAGE` becomes `(1 << 32) - 1` = 4B pages (plenty of address space)

**Implications:**
- `ITEMS_PER_PAGE_BITS = 20 > 16` → **the overflow allocator assertion would fail!** Need to also adjust: `#[cfg(feature = "small-pages")] const ITEMS_PER_PAGE_BITS: u32 = 16;`
- Tests that hardcode `OFFSET_BITS = 25` (e.g., `loom_tests.rs:1546`) need cfg-gating
- Checkpoint files from small-pages builds are **incompatible** with production builds (different address encoding)

**Proposed feature name:** `small-pages` (could also tie it to `simulation` since DST is the primary consumer)

---

## 6. Could It Be a Runtime Parameter?

**What prevents it:**

1. **`const fn` constructors** — `LogicalAddress::new()` is `const`, requiring `OFFSET_BITS` at compile time (`address.rs:226`)
2. **Compile-time assertions** — `allocator.rs:107-109` uses `const _: () = assert!(...)` 
3. **No natural place to store it** — `LogicalAddress` is a transparent `u64` wrapper. There's nowhere to carry a "my page size is X" field.
4. **Performance** — Every address decode on the hot path (`page()`, `offset()`) would become a variable shift instead of a constant shift. On modern CPUs this is ~1 cycle difference, but it's on the absolute hottest path in the system.
5. **Pervasive threading** — `page_size` would need to be passed to (or stored in) every component: allocator, scanner, compactor, recovery engine, session, etc.

**Verdict:** Technically possible but would require a major refactor (~50+ call sites) with a measurable hot-path perf cost. Not justified when a feature flag achieves the same goal with zero code change.

---

## 7. On-Disk Implications of Changing OFFSET_BITS

### Can existing data be read?

**No.** If `OFFSET_BITS` changes, every persisted `LogicalAddress` is misinterpreted:

Example: Address `(page=100, offset=5)` with `OFFSET_BITS=25`:
- Raw bits: `100 << 25 | 5` = `3,355,443,205`

Decoded with `OFFSET_BITS=16`:
- page = `3,355,443,205 >> 16` = `51,200` (wrong!)
- offset = `3,355,443,205 & 0xFFFF` = `50,181` (wrong!)

This affects:
- **Hash index entries** — point to wrong records
- **RecordInfo prev_address chains** — break version chain traversal
- **Checkpoint metadata** — `LogRecoveryInfo` stores `begin_address`, `flushed_until_address`, etc. as raw `LogicalAddress` values (`log_recovery.rs:70-80`)
- **Device I/O offsets** — `begin_address.rs:113`: `device_offset = page * page_size + offset` computes wrong byte positions

### Does the hash index address encoding break?

**Yes.** Hash bucket entries store `LogicalAddress` values. With different `OFFSET_BITS`, the page/offset split moves, corrupting lookups.

### Does recovery assume page-aligned reads?

**Yes.** Recovery reads full pages from device files and locates `PageTrailer` (CRC + valid_bytes) at sector-aligned positions within each page (`log_recovery.rs:56`, `page.rs:write_size()`). The trailer position depends on `page_size`.

### Bottom line

**Changing OFFSET_BITS between runs is a breaking change.** Data written with one value cannot be read with another. This is acceptable for DST (ephemeral data) but must never happen in production without a migration tool.

---

## 8. C++ and C# Comparison

### C++ (cc/src/core/address.h)

```cpp
static constexpr uint64_t kOffsetBits = 25;  // Hard-coded, line 36
uint64_t offset_ : kOffsetBits;              // C++ bitfield, line 128
```

**Hard-coded.** Uses C++ bitfields which require compile-time widths. No configurability mechanism exists. The C++ FASTER has always used 32 MB pages.

### C# (cs/src/core/Index/Common/LogSettings.cs)

```csharp
public int PageSizeBits = 25;  // DEFAULT, but user-configurable at construction time
```

```csharp
// cs/src/core/Index/Common/FasterKVSettings.cs:42
public long PageSize = 1 << 25;  // Also configurable
```

```csharp
// cs/src/core/Allocator/AllocatorBase.cs
LogPageSizeBits = settings.PageSizeBits;    // Stored as instance field
PageSize = 1 << LogPageSizeBits;            // Computed once
PageSizeMask = PageSize - 1;                // Computed once
```

**Runtime configurable.** The C# implementation:
- Stores `LogPageSizeBits` as an instance field on the allocator
- Computes page size once during construction
- Uses it throughout via instance references
- Tests commonly set `PageSizeBits = 10` (1 KB), `12` (4 KB), or `16` (64 KB)
- This works because C# doesn't use packed bitfield addresses — it uses `long` arithmetic

**Key insight:** The C# version proves page size *can* be configurable. The constraint in Rust/C++ is the bit-packed `LogicalAddress` format and compile-time bitfield/shift requirements, not algorithmic necessity.

---

## 9. Recommendation: Feature Flag for DST

### Immediate action (low risk, high value):

1. **Add `small-pages` feature to `Cargo.toml`:**
   ```toml
   [features]
   small-pages = []
   ```

2. **Cfg-gate `OFFSET_BITS` in `address.rs`:**
   ```rust
   #[cfg(feature = "small-pages")]
   pub const OFFSET_BITS: u32 = 16;  // 64 KB pages
   
   #[cfg(not(feature = "small-pages"))]
   pub const OFFSET_BITS: u32 = 25;  // 32 MB pages
   ```

3. **Cfg-gate `ITEMS_PER_PAGE_BITS` in `allocator.rs`:**
   ```rust
   #[cfg(feature = "small-pages")]
   pub const ITEMS_PER_PAGE_BITS: u32 = 16;
   
   #[cfg(not(feature = "small-pages"))]
   pub const ITEMS_PER_PAGE_BITS: u32 = 20;
   ```

4. **DST and simulation tests compile with:** `--features simulation,small-pages`

5. **CI matrix:** Run the standard test suite with both `--features ""` (production) and `--features small-pages` (verify nothing breaks at the smaller page size).

### Why not tie to `simulation` directly?

Keeping `small-pages` separate from `simulation` allows:
- Running simulation with production-sized pages (for perf benchmarks)
- Running standard tests with small pages (for fast memory-pressure coverage)
- Combining both for DST scenarios

### Future option (if runtime is ever needed):

If a customer needs runtime page size selection, follow the C# pattern:
1. Make `OFFSET_BITS` a field on a `PageConfig` struct
2. Thread `PageConfig` through all subsystems
3. Replace `const fn` address constructors with regular fns
4. Store `OFFSET_BITS` in checkpoint metadata for recovery validation

This is a ~2-week refactor affecting ~50+ call sites. Only pursue if there's a production use case (there isn't today).

---

## Summary

| Approach | Effort | Risk | DST Value |
|----------|--------|------|-----------|
| **Feature flag** `small-pages` | ~1 hour | Zero (production unchanged) | ✅ Full pipeline exercise |
| Const generic | ~2 weeks | High (type explosion) | ✅ Full pipeline exercise |
| Runtime parameter | ~2 weeks | Medium (perf, threading) | ✅ Full pipeline exercise |
| Do nothing | 0 | 0 | ❌ DST can't stress flush/eviction |

**Recommendation: Feature flag.** It's the right tool for this job — zero production risk, trivial implementation, and it solves the DST problem completely.
