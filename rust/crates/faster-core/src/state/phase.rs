//! Unified phase enum for FASTER state machines.
//!
//! Replaces the separate `CheckpointPhase` and `GrowPhase` enums with a
//! single `Phase` type that is packed into [`SystemState`](super::SystemState).

use serde::{Deserialize, Serialize};

/// All phases for the FASTER state machine.
///
/// Values 0–63 are normal phases. Bit 7 (0x80) is reserved for the
/// INTERMEDIATE marker in [`SystemState`](super::SystemState).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// No checkpoint or grow in progress.
    Rest = 0,
    /// Sessions notified; checkpoint preparation underway.
    Prepare = 1,
    /// Actively taking checkpoint. Version bumped on entry.
    InProgress = 2,
    /// Waiting for log flush completion.
    WaitFlush = 3,
    /// Waiting for all sessions to acknowledge completion.
    WaitCompletion = 4,
    /// Persistence callback; transitions directly back to Rest.
    PersistenceCallback = 5,
    /// Allocate new (2×) hash table; notify sessions.
    PrepareGrow = 8,
    /// Actively splitting buckets.
    InProgressGrow = 9,
    /// Waiting for all sessions to acknowledge grow completion.
    WaitCompletionGrow = 10,
}

impl Phase {
    /// Converts a raw `u8` to a `Phase`, returning `None` for unrecognised
    /// discriminants. The intermediate bit (0x80) should be stripped first.
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Rest),
            1 => Some(Self::Prepare),
            2 => Some(Self::InProgress),
            3 => Some(Self::WaitFlush),
            4 => Some(Self::WaitCompletion),
            5 => Some(Self::PersistenceCallback),
            8 => Some(Self::PrepareGrow),
            9 => Some(Self::InProgressGrow),
            10 => Some(Self::WaitCompletionGrow),
            _ => None,
        }
    }

    /// Returns the `u8` discriminant of this phase.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Returns `true` if this is a checkpoint phase (discriminant 1–7).
    #[inline]
    pub const fn is_checkpoint(self) -> bool {
        let d = self as u8;
        d >= 1 && d <= 7
    }

    /// Returns `true` if this is a grow phase (discriminant 8–15).
    #[inline]
    pub const fn is_grow(self) -> bool {
        let d = self as u8;
        d >= 8 && d <= 15
    }

    /// Returns `true` if the state machine is quiescent (Rest).
    #[inline]
    pub const fn is_rest(self) -> bool {
        self as u8 == 0
    }
}

impl core::fmt::Display for Phase {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::Rest => "Rest",
            Self::Prepare => "Prepare",
            Self::InProgress => "InProgress",
            Self::WaitFlush => "WaitFlush",
            Self::WaitCompletion => "WaitCompletion",
            Self::PersistenceCallback => "PersistenceCallback",
            Self::PrepareGrow => "PrepareGrow",
            Self::InProgressGrow => "InProgressGrow",
            Self::WaitCompletionGrow => "WaitCompletionGrow",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_u8_round_trip() {
        let phases = [
            Phase::Rest, Phase::Prepare, Phase::InProgress,
            Phase::WaitFlush, Phase::WaitCompletion, Phase::PersistenceCallback,
            Phase::PrepareGrow, Phase::InProgressGrow, Phase::WaitCompletionGrow,
        ];
        for phase in phases {
            assert_eq!(Phase::from_u8(phase.as_u8()), Some(phase));
        }
    }

    #[test]
    fn from_u8_invalid_returns_none() {
        for v in [6, 7, 11, 15, 16, 128, 255] {
            assert_eq!(Phase::from_u8(v), None);
        }
    }

    #[test]
    fn classification() {
        assert!(Phase::Rest.is_rest());
        assert!(Phase::Prepare.is_checkpoint());
        assert!(Phase::PrepareGrow.is_grow());
        assert!(!Phase::Rest.is_checkpoint());
        assert!(!Phase::Rest.is_grow());
    }

    #[test]
    fn display_formatting() {
        assert_eq!(Phase::Rest.to_string(), "Rest");
        assert_eq!(Phase::PersistenceCallback.to_string(), "PersistenceCallback");
        assert_eq!(Phase::WaitCompletionGrow.to_string(), "WaitCompletionGrow");
    }

    #[test]
    fn serde_round_trip() {
        for phase in [Phase::Rest, Phase::Prepare, Phase::PrepareGrow] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(phase, back);
        }
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn phase_strategy() -> impl Strategy<Value = Phase> {
            prop_oneof![
                Just(Phase::Rest), Just(Phase::Prepare), Just(Phase::InProgress),
                Just(Phase::WaitFlush), Just(Phase::WaitCompletion),
                Just(Phase::PersistenceCallback), Just(Phase::PrepareGrow),
                Just(Phase::InProgressGrow), Just(Phase::WaitCompletionGrow),
            ]
        }

        proptest! {
            #[test]
            fn from_u8_identity(phase in phase_strategy()) {
                prop_assert_eq!(Phase::from_u8(phase.as_u8()), Some(phase));
            }
        }
    }
}
