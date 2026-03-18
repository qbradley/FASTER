//! Packed checkpoint/version state in a single atomic word.
//!
//! [`SystemState`] packs `(phase: u8, version: u56)` into a single `u64`,
//! and [`AtomicSystemState`] wraps it in an `AtomicU64` for lock-free CAS.
//!
//! # Bit Layout
//!
//! ```text
//! Bit 63                                                    Bit 0
//! ┌────────────────┬────────────────────────────────────────────────┐
//! │  Phase (8 bits)│              Version (56 bits)                 │
//! │  bits 56–63    │              bits 0–55                         │
//! └────────────────┴────────────────────────────────────────────────┘
//! ```

use core::sync::atomic::{AtomicU64, Ordering};

use super::Phase;

/// Packed `(phase, version)` state in a single 64-bit word.
///
/// - **Bits 56–63:** phase byte (bit 63 = intermediate marker via 0x80).
/// - **Bits 0–55:** monotonic version counter (56-bit).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SystemState(u64);

impl SystemState {
    const PHASE_SHIFT: u32 = 56;
    #[allow(dead_code)]
    const PHASE_MASK: u64 = 0xFF00_0000_0000_0000;
    /// Mask isolating the version field (bits 0–55).
    pub(crate) const VERSION_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;
    const INTERMEDIATE_BIT: u8 = 0x80;

    /// The initial system state: `Rest` phase, version 0.
    pub const INITIAL: Self = Self::new(Phase::Rest, 0);

    /// Create a new `SystemState` from a phase and version.
    /// The version is masked to 56 bits.
    #[inline]
    pub const fn new(phase: Phase, version: u64) -> Self {
        Self(((phase as u8 as u64) << Self::PHASE_SHIFT) | (version & Self::VERSION_MASK))
    }

    /// Extract the phase, stripping the intermediate bit.
    #[inline]
    pub const fn phase(self) -> Phase {
        let raw = (self.0 >> Self::PHASE_SHIFT) as u8 & !Self::INTERMEDIATE_BIT;
        match Phase::from_u8(raw) {
            Some(p) => p,
            None => panic!("corrupted phase discriminant in SystemState"),
        }
    }

    /// Extract the raw phase byte including the intermediate bit.
    #[inline]
    pub const fn phase_raw(self) -> u8 {
        (self.0 >> Self::PHASE_SHIFT) as u8
    }

    /// Extract the 56-bit version counter.
    #[inline]
    pub const fn version(self) -> u64 {
        self.0 & Self::VERSION_MASK
    }

    /// Returns `true` if the intermediate (mid-transition) bit is set.
    #[inline]
    pub const fn is_intermediate(self) -> bool {
        (self.phase_raw() & Self::INTERMEDIATE_BIT) != 0
    }

    /// Create the intermediate form (sets bit 7 of the phase byte).
    #[inline]
    pub const fn make_intermediate(self) -> Self {
        Self(self.0 | ((Self::INTERMEDIATE_BIT as u64) << Self::PHASE_SHIFT))
    }

    /// The raw `u64` word.
    #[inline]
    pub const fn word(self) -> u64 {
        self.0
    }

    /// Construct from a raw `u64` word.
    #[inline]
    pub const fn from_word(word: u64) -> Self {
        Self(word)
    }
}

impl core::fmt::Debug for SystemState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SystemState")
            .field("phase", &self.phase())
            .field("version", &self.version())
            .field("intermediate", &self.is_intermediate())
            .finish()
    }
}

impl core::fmt::Display for SystemState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_intermediate() {
            write!(f, "{}*(v{})", self.phase(), self.version())
        } else {
            write!(f, "{}(v{})", self.phase(), self.version())
        }
    }
}

/// Thread-safe atomic container for [`SystemState`].
///
/// All state reads and transitions go through this type. Replaces both
/// `CheckpointStateMachine::phase` (`AtomicU8`) and `HashIndex::version`
/// (`AtomicU32`) with a single `AtomicU64`.
pub struct AtomicSystemState {
    word: AtomicU64,
}

impl AtomicSystemState {
    /// Create a new atomic state from the given initial value.
    #[inline]
    pub fn new(initial: SystemState) -> Self {
        Self {
            word: AtomicU64::new(initial.word()),
        }
    }

    /// Load the current state.
    ///
    /// Use `Acquire` for reads informing subsequent data accesses.
    #[inline]
    pub fn load(&self, ordering: Ordering) -> SystemState {
        SystemState::from_word(self.word.load(ordering))
    }

    /// Attempt an atomic state transition via CAS.
    ///
    /// Returns `Ok(expected)` on success, `Err(actual)` on failure.
    /// Typical: `success = AcqRel`, `failure = Acquire`.
    #[inline]
    pub fn compare_exchange(
        &self,
        expected: SystemState,
        new: SystemState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<SystemState, SystemState> {
        self.word
            .compare_exchange(expected.word(), new.word(), success, failure)
            .map(SystemState::from_word)
            .map_err(SystemState::from_word)
    }

    /// Two-phase transition using the intermediate-state protocol.
    ///
    /// 1. CAS `expected → intermediate` — claim exclusive transition.
    /// 2. Execute `before_hooks`.
    /// 3. CAS `intermediate → next` — publish new state.
    ///
    /// Returns `true` if this thread won.
    pub fn try_transition(
        &self,
        expected: SystemState,
        next: SystemState,
        before_hooks: impl FnOnce(),
    ) -> bool {
        let intermediate = expected.make_intermediate();

        // Step 1: Claim via CAS to intermediate.
        // AcqRel: publishes intent, acquires current state.
        if self
            .compare_exchange(expected, intermediate, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        // Step 2: Execute hooks (safe — we hold exclusive transition).
        before_hooks();

        // Step 3: Publish new state. Guaranteed to succeed.
        // AcqRel: ensures hook side-effects visible before new state.
        let result = self.compare_exchange(intermediate, next, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_ok(), "intermediate → next CAS must succeed");

        true
    }

    /// Spin-wait until the intermediate bit clears.
    ///
    /// The intermediate window is very short (hook execution time).
    #[inline]
    pub fn wait_non_intermediate(&self) -> SystemState {
        loop {
            let state = self.load(Ordering::Acquire);
            if !state.is_intermediate() {
                return state;
            }
            core::hint::spin_loop();
        }
    }
}

impl core::fmt::Debug for AtomicSystemState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = self.load(Ordering::Relaxed);
        f.debug_struct("AtomicSystemState")
            .field("state", &state)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let s = SystemState::INITIAL;
        assert_eq!(s.phase(), Phase::Rest);
        assert_eq!(s.version(), 0);
        assert!(!s.is_intermediate());
    }

    #[test]
    fn new_packs_correctly() {
        let s = SystemState::new(Phase::InProgress, 42);
        assert_eq!(s.phase(), Phase::InProgress);
        assert_eq!(s.version(), 42);
    }

    #[test]
    fn version_truncated_to_56_bits() {
        let s = SystemState::new(Phase::Rest, u64::MAX);
        assert_eq!(s.version(), SystemState::VERSION_MASK);
    }

    #[test]
    fn make_intermediate_sets_bit() {
        let s = SystemState::new(Phase::Prepare, 7);
        let i = s.make_intermediate();
        assert!(i.is_intermediate());
        assert_eq!(i.phase(), Phase::Prepare);
        assert_eq!(i.version(), 7);
    }

    #[test]
    fn word_round_trip() {
        let s = SystemState::new(Phase::WaitFlush, 0xDEAD_BEEF_CAFE);
        assert_eq!(SystemState::from_word(s.word()), s);
    }

    #[test]
    fn display_normal() {
        assert_eq!(
            SystemState::new(Phase::InProgress, 5).to_string(),
            "InProgress(v5)"
        );
    }

    #[test]
    fn display_intermediate() {
        let s = SystemState::new(Phase::Prepare, 3).make_intermediate();
        assert_eq!(s.to_string(), "Prepare*(v3)");
    }

    #[test]
    fn atomic_load() {
        let a = AtomicSystemState::new(SystemState::INITIAL);
        assert_eq!(a.load(Ordering::Acquire).phase(), Phase::Rest);
    }

    #[test]
    fn compare_exchange_success() {
        let a = AtomicSystemState::new(SystemState::INITIAL);
        let new = SystemState::new(Phase::Prepare, 0);
        assert!(
            a.compare_exchange(
                SystemState::INITIAL,
                new,
                Ordering::AcqRel,
                Ordering::Acquire
            )
            .is_ok()
        );
        assert_eq!(a.load(Ordering::Acquire), new);
    }

    #[test]
    fn compare_exchange_failure() {
        let a = AtomicSystemState::new(SystemState::new(Phase::Prepare, 0));
        let result = a.compare_exchange(
            SystemState::INITIAL,
            SystemState::new(Phase::InProgress, 1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(result.is_err());
    }

    #[test]
    fn try_transition_success() {
        let a = AtomicSystemState::new(SystemState::INITIAL);
        let mut called = false;
        assert!(a.try_transition(
            SystemState::INITIAL,
            SystemState::new(Phase::Prepare, 0),
            || called = true
        ));
        assert!(called);
        assert_eq!(a.load(Ordering::Acquire).phase(), Phase::Prepare);
    }

    #[test]
    fn try_transition_failure_no_hooks() {
        let a = AtomicSystemState::new(SystemState::new(Phase::Prepare, 0));
        let mut called = false;
        assert!(!a.try_transition(
            SystemState::INITIAL,
            SystemState::new(Phase::InProgress, 1),
            || called = true
        ));
        assert!(!called);
    }

    #[test]
    fn concurrent_cas_one_winner() {
        use std::sync::{Arc, Barrier};
        let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&state);
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    s.try_transition(
                        SystemState::INITIAL,
                        SystemState::new(Phase::Prepare, 0),
                        || {},
                    )
                })
            })
            .collect();
        let wins: usize = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&w| w)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn full_checkpoint_cycle() {
        let s = AtomicSystemState::new(SystemState::INITIAL);
        let t = |from: Phase, to: Phase, fv: u64, tv: u64| {
            assert!(s.try_transition(SystemState::new(from, fv), SystemState::new(to, tv), || {}));
        };
        t(Phase::Rest, Phase::Prepare, 0, 0);
        t(Phase::Prepare, Phase::InProgress, 0, 1);
        t(Phase::InProgress, Phase::WaitFlush, 1, 1);
        t(Phase::WaitFlush, Phase::WaitCompletion, 1, 1);
        t(Phase::WaitCompletion, Phase::PersistenceCallback, 1, 1);
        t(Phase::PersistenceCallback, Phase::Rest, 1, 1);
        let final_s = s.load(Ordering::Acquire);
        assert_eq!(final_s.phase(), Phase::Rest);
        assert_eq!(final_s.version(), 1);
    }

    #[test]
    fn grow_checkpoint_mutual_exclusion() {
        use std::sync::{Arc, Barrier};
        let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
        let barrier = Arc::new(Barrier::new(2));
        let s1 = Arc::clone(&state);
        let b1 = Arc::clone(&barrier);
        let ha = std::thread::spawn(move || {
            b1.wait();
            s1.try_transition(
                SystemState::INITIAL,
                SystemState::new(Phase::Prepare, 0),
                || {},
            )
        });
        let s2 = Arc::clone(&state);
        let b2 = Arc::clone(&barrier);
        let hb = std::thread::spawn(move || {
            b2.wait();
            s2.try_transition(
                SystemState::INITIAL,
                SystemState::new(Phase::PrepareGrow, 0),
                || {},
            )
        });
        let (a, b) = (ha.join().unwrap(), hb.join().unwrap());
        assert_ne!(a, b, "exactly one should win");
    }

    #[test]
    fn version_monotonic() {
        let s = AtomicSystemState::new(SystemState::INITIAL);
        let mut last = 0u64;
        for cycle in 0u64..5 {
            let t = |from: Phase, to: Phase, fv: u64, tv: u64| {
                assert!(s.try_transition(
                    SystemState::new(from, fv),
                    SystemState::new(to, tv),
                    || {}
                ));
            };
            t(Phase::Rest, Phase::Prepare, cycle, cycle);
            t(Phase::Prepare, Phase::InProgress, cycle, cycle + 1);
            t(Phase::InProgress, Phase::WaitFlush, cycle + 1, cycle + 1);
            t(
                Phase::WaitFlush,
                Phase::WaitCompletion,
                cycle + 1,
                cycle + 1,
            );
            t(
                Phase::WaitCompletion,
                Phase::PersistenceCallback,
                cycle + 1,
                cycle + 1,
            );
            t(
                Phase::PersistenceCallback,
                Phase::Rest,
                cycle + 1,
                cycle + 1,
            );
            let v = s.load(Ordering::Acquire).version();
            assert!(v >= last);
            last = v;
        }
        assert_eq!(last, 5);
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn phase_strategy() -> impl Strategy<Value = Phase> {
            prop_oneof![
                Just(Phase::Rest),
                Just(Phase::Prepare),
                Just(Phase::InProgress),
                Just(Phase::WaitFlush),
                Just(Phase::WaitCompletion),
                Just(Phase::PersistenceCallback),
                Just(Phase::PrepareGrow),
                Just(Phase::InProgressGrow),
                Just(Phase::WaitCompletionGrow),
            ]
        }

        proptest! {
            #[test]
            fn roundtrip(phase in phase_strategy(), version in 0u64..=SystemState::VERSION_MASK) {
                let state = SystemState::new(phase, version);
                prop_assert_eq!(state.phase(), phase);
                prop_assert_eq!(state.version(), version);
            }

            #[test]
            fn intermediate_preserves(phase in phase_strategy(), version in 0u64..=SystemState::VERSION_MASK) {
                let inter = SystemState::new(phase, version).make_intermediate();
                prop_assert!(inter.is_intermediate());
                prop_assert_eq!(inter.phase(), phase);
                prop_assert_eq!(inter.version(), version);
            }

            #[test]
            fn word_identity(phase in phase_strategy(), version in 0u64..=SystemState::VERSION_MASK) {
                let s = SystemState::new(phase, version);
                prop_assert_eq!(SystemState::from_word(s.word()), s);
            }
        }
    }

    // ── Regression tests for swallowed-error audit ──────────────────

    /// Regression test for BUG-6: the intermediate→next CAS in
    /// try_transition must be enforced with assert! (not debug_assert!).
    ///
    /// We verify the positive path: after a successful try_transition,
    /// the state must be the `next` state (not stuck in intermediate).
    /// The assert! inside try_transition guarantees this in release builds.
    #[test]
    fn test_regression_intermediate_cas_assert_not_debug_assert() {
        let state = AtomicSystemState::new(SystemState::INITIAL);
        let next = SystemState::new(Phase::Prepare, 0);

        let won = state.try_transition(SystemState::INITIAL, next, || {});
        assert!(won, "should win the transition");

        let current = state.load(Ordering::Acquire);
        // The state must be `next`, NOT intermediate. The assert! in
        // try_transition guarantees this — if it were debug_assert!,
        // the state could be stuck in intermediate in release builds.
        assert!(
            !current.is_intermediate(),
            "state must not be stuck in intermediate after try_transition"
        );
        assert_eq!(current, next, "state must be the target `next` state");
    }

    /// Regression test for BUG-6: concurrent transitions must not leave
    /// intermediate state visible after try_transition returns.
    #[test]
    fn test_regression_intermediate_never_visible_after_transition() {
        use std::sync::{Arc, Barrier};

        let state = Arc::new(AtomicSystemState::new(SystemState::INITIAL));
        let barrier = Arc::new(Barrier::new(4));

        // First, do a transition to Prepare.
        assert!(state.try_transition(
            SystemState::INITIAL,
            SystemState::new(Phase::Prepare, 0),
            || {}
        ));

        // Now race 4 threads trying to advance Prepare→InProgress.
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let s = Arc::clone(&state);
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    s.try_transition(
                        SystemState::new(Phase::Prepare, 0),
                        SystemState::new(Phase::InProgress, 1),
                        || {},
                    )
                })
            })
            .collect();

        let wins: usize = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&w| w)
            .count();

        assert_eq!(wins, 1, "exactly one thread must win");

        let final_state = state.load(Ordering::Acquire);
        assert!(
            !final_state.is_intermediate(),
            "state must never be intermediate after all transitions complete"
        );
    }
}
