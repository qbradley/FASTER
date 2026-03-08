//! Cross-platform software prefetch hints.
//!
//! Provides a thin wrapper around architecture-specific prefetch intrinsics
//! so that callers can issue *read* or *write* prefetch hints without
//! `unsafe` at the call site and without caring about the target ISA.
//!
//! # Supported targets
//!
//! | Target          | Read Intrinsic                       | Write Intrinsic                      |
//! |-----------------|--------------------------------------|--------------------------------------|
//! | `x86_64`        | `_mm_prefetch(..., _MM_HINT_T0)` (L1)| `_mm_prefetch(..., _MM_HINT_ET0)`   |
//! | `aarch64`       | `PRFM PLDL1KEEP` via inline asm     | `PRFM PSTL1KEEP` via inline asm     |
//! | everything else | no-op (safe, just loses the hint)    | no-op                                |

/// Issues a *read* prefetch hint for the cache line containing `*ptr`.
///
/// The hint asks the hardware to begin fetching data into the **L1** cache.
/// It is purely advisory – the program is correct regardless of whether the
/// hardware honours it.
///
/// # Safety model
///
/// The function is intentionally **safe** to call.  Prefetch instructions on
/// both x86-64 and AArch64 are specified as NOP-like hints that never fault,
/// even if the address is invalid, unmapped, or null.
#[inline(always)]
pub fn prefetch_read<T>(ptr: *const T) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `_mm_prefetch` is a non-faulting hint on x86-64.
        // `_MM_HINT_T0` requests prefetch into all cache levels (L1/L2/L3).
        unsafe {
            core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_T0);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `PRFM PLDL1KEEP` is a non-faulting hint on AArch64.
        unsafe {
            core::arch::asm!("prfm pldl1keep, [{addr}]", addr = in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    // On all other architectures the hint is silently dropped.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}

/// Issues a *write* prefetch hint for the cache line containing `*ptr`.
///
/// On x86-64 this uses `_MM_HINT_ET0` which brings the line into L1 in
/// **exclusive** (Modified/Exclusive) MESI state, avoiding a later
/// read-for-ownership stall on the first write.
///
/// Use this for record addresses that will be mutated (upsert in-place,
/// RMW in-place, delete tombstone writes). For read-only accesses, prefer
/// [`prefetch_read`] to avoid unnecessary MESI invalidation traffic on
/// other cores' caches.
///
/// # Safety model
///
/// Same as [`prefetch_read`] — non-faulting hint, safe to call with any
/// pointer value including null.
#[inline(always)]
pub fn prefetch_write<T>(ptr: *mut T) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `_mm_prefetch` with `_MM_HINT_ET0` is a non-faulting hint.
        // ET0 = Exclusive access to T0 cache level — writes without RFO stall.
        unsafe {
            core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_ET0);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `PRFM PSTL1KEEP` is the store-prefetch hint on AArch64.
        unsafe {
            core::arch::asm!("prfm pstl1keep, [{addr}]", addr = in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_stack_variable() {
        let value: u64 = 42;
        // Must not panic or fault.
        prefetch_read(&value as *const u64);
    }

    #[test]
    fn prefetch_heap_allocation() {
        let boxed = Box::new([0u8; 128]);
        prefetch_read(boxed.as_ptr());
    }

    #[test]
    fn prefetch_null_is_safe() {
        // Prefetch of null must be a silent no-op on all targets.
        prefetch_read(core::ptr::null::<u8>());
    }

    #[test]
    fn prefetch_aligned_cache_line() {
        // 64-byte aligned, cache-line-sized buffer.
        #[repr(align(64))]
        struct CacheLine {
            _data: [u8; 64],
        }
        let cl = CacheLine { _data: [0xAB; 64] };
        prefetch_read(&cl as *const CacheLine);
    }

    #[test]
    fn prefetch_write_stack_variable() {
        let mut value: u64 = 42;
        prefetch_write(&mut value as *mut u64);
    }

    #[test]
    fn prefetch_write_heap_allocation() {
        let mut boxed = Box::new([0u8; 128]);
        prefetch_write(boxed.as_mut_ptr());
    }

    #[test]
    fn prefetch_write_null_is_safe() {
        prefetch_write(core::ptr::null_mut::<u8>());
    }
}
