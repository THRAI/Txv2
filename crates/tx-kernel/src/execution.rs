//! Shared step-result and error spelling for interface-only subsystem seams.
//!
//! The concrete reactor carriers are still owned by the reactor and bus work.
//! This module gives VFS, Mount, PageBacked, and filesystem backends one public
//! spelling to compile against until those carriers are connected.

pub use tx_substrate::epoch::Guard;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    EBUSY,
    EDQUOT,
    EFAULT,
    EINVAL,
    EIO,
    EISDIR,
    ENAMETOOLONG,
    ENODEV,
    ENOMEM,
    ENOENT,
    ENOSYS,
    ENOTDIR,
    EROFS,
    ESTALE,
}

pub type KernelResult<T> = Result<T, Errno>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitToken {
    carrier: u64,
    interest: u64,
}

impl WaitToken {
    pub const fn new(carrier: u64, interest: u64) -> Self {
        Self { carrier, interest }
    }

    pub const fn carrier(self) -> u64 {
        self.carrier
    }

    pub const fn interest(self) -> u64 {
        self.interest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome<T> {
    Done(T),
    Advanced(T),
    Blocked(WaitToken),
    AdvancedThenBlocked(T, WaitToken),
    Err(Errno),
}

impl<T> StepOutcome<T> {
    pub const fn done(value: T) -> Self {
        Self::Done(value)
    }

    pub const fn err(errno: Errno) -> Self {
        Self::Err(errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_token_preserves_carrier_and_interest_spelling() {
        let token = WaitToken::new(4, 0b101);

        assert_eq!(token.carrier(), 4);
        assert_eq!(token.interest(), 0b101);
        assert_eq!(
            StepOutcome::<()>::Blocked(token),
            StepOutcome::Blocked(token)
        );
    }
}
