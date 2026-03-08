//! Cross-platform software prefetch hints.
//!
//! Provides a thin wrapper around architecture-specific prefetch intrinsics
//! so that callers can issue *read* prefetch hints without `unsafe` at the
//! call site and without caring about the target ISA.
//!
//! # Supported targets
//!
//! | Target          | Intrinsic                            |
//! |-----------------|--------------------------------------|
//! | `x86_64`        | `_mm_prefetch(..., _MM_HINT_T0)` (L1)|
//! | `aarch64`       | `PRFM PLDL1KEEP` via inline asm     |
//! | everything else | no-op (safe, just loses the hint)    |

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
}
