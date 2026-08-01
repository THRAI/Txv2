//! Shared execution vocabulary for subsystem seams.
//!
//! Txv3 owns the canonical step outcome and errno catalog. This module keeps
//! the historically convenient `tx_subsystems::execution::*` import path while
//! re-exporting/aliasing the v3 shapes directly.

pub use crate::adapter::step_engine::{Guard, V3Errno as Errno};

/// Compatibility spelling for one-shot/control steps whose progress type is
/// `NoProgress`. Byte-moving steps should use `step::ByteProgress`
/// explicitly at their boundary.
pub type StepOutcome<T> =
    crate::adapter::step_engine::StepOutcome<T, crate::adapter::step_engine::NoProgress>;

pub type KernelResult<T> = Result<T, Errno>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitToken {
    source_id: u64,
    interest: u64,
    observed_generation: u64,
}

impl WaitToken {
    const NO_OBSERVED_GENERATION: u64 = u64::MAX;

    pub const fn new(source_id: u64, interest: u64) -> Self {
        Self {
            source_id,
            interest,
            observed_generation: Self::NO_OBSERVED_GENERATION,
        }
    }

    pub(crate) const fn with_observed_generation(
        source_id: u64,
        interest: u64,
        observed_generation: u64,
    ) -> Self {
        Self {
            source_id,
            interest,
            observed_generation,
        }
    }

    pub const fn source_id(self) -> u64 {
        self.source_id
    }

    pub const fn interest(self) -> u64 {
        self.interest
    }

    pub(crate) const fn observed_generation(self) -> Option<u64> {
        if self.observed_generation == Self::NO_OBSERVED_GENERATION {
            None
        } else {
            Some(self.observed_generation)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_token_preserves_source_id_and_interest_spelling() {
        let token = WaitToken::new(4, 0b101);

        assert_eq!(token.source_id(), 4);
        assert_eq!(token.interest(), 0b101);
        let same = WaitToken::new(4, 0b101);
        assert_eq!(token, same);
    }

    #[test]
    fn execution_errno_is_step_errno() {
        let errno = Errno::EADDRINUSE;
        let same: crate::adapter::step_engine::V3Errno = errno;

        assert_eq!(same, crate::adapter::step_engine::V3Errno::EADDRINUSE);
    }
}
