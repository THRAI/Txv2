//! Shared step-result and error spelling for interface-only subsystem seams.
//!
//! The concrete reactor carriers are still owned by the reactor and bus work.
//! This module gives VFS, Mount, PageBacked, and filesystem backends one public
//! spelling to compile against until those carriers are connected.

pub use crate::adapter::step_engine::{Guard, V3Errno as Errno};
pub type StepOutcome<T> =
    crate::adapter::step_engine::StepOutcome<T, crate::adapter::step_engine::NoProgress>;

pub type KernelResult<T> = Result<T, Errno>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitToken {
    source_id: u64,
    interest: u64,
}

impl WaitToken {
    pub const fn new(source_id: u64, interest: u64) -> Self {
        Self {
            source_id,
            interest,
        }
    }

    pub const fn source_id(self) -> u64 {
        self.source_id
    }

    pub const fn interest(self) -> u64 {
        self.interest
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
    fn from_v4_errno_round_trip() {
        // The `From<execution::Errno> for step_v3::Errno` impl must
        // map each variant to the same-named variant. Closed catalog:
        // the table below names each `execution::Errno` variant
        // explicitly so adding a new variant later (without extending
        // `step_v3::Errno` + the From impl) fails to compile or this
        // test fails immediately.
        use crate::adapter::step_engine::V3Errno as V3;
        let table: [(Errno, V3); 30] = [
            (Errno::E2BIG, V3::E2BIG),
            (Errno::EACCES, V3::EACCES),
            (Errno::EAGAIN, V3::EAGAIN),
            (Errno::EBADF, V3::EBADF),
            (Errno::EBUSY, V3::EBUSY),
            (Errno::EDQUOT, V3::EDQUOT),
            (Errno::EEXIST, V3::EEXIST),
            (Errno::EFAULT, V3::EFAULT),
            (Errno::EINVAL, V3::EINVAL),
            (Errno::EINTR, V3::EINTR),
            (Errno::EIO, V3::EIO),
            (Errno::EISDIR, V3::EISDIR),
            (Errno::ELOOP, V3::ELOOP),
            (Errno::ENAMETOOLONG, V3::ENAMETOOLONG),
            (Errno::ENODEV, V3::ENODEV),
            (Errno::ENOEXEC, V3::ENOEXEC),
            (Errno::ENOMEM, V3::ENOMEM),
            (Errno::ENOENT, V3::ENOENT),
            (Errno::ENOSYS, V3::ENOSYS),
            (Errno::ENOTDIR, V3::ENOTDIR),
            (Errno::ENOTEMPTY, V3::ENOTEMPTY),
            (Errno::ENOTTY, V3::ENOTTY),
            (Errno::EPERM, V3::EPERM),
            (Errno::EPIPE, V3::EPIPE),
            (Errno::ERANGE, V3::ERANGE),
            (Errno::EROFS, V3::EROFS),
            (Errno::ESPIPE, V3::ESPIPE),
            (Errno::ESRCH, V3::ESRCH),
            (Errno::ESTALE, V3::ESTALE),
            (Errno::ETIMEDOUT, V3::ETIMEDOUT),
        ];
        assert_eq!(table.len(), 30);
        for (v4, expected_v3) in table {
            let mapped: V3 = v4.into();
            assert_eq!(
                mapped, expected_v3,
                "v4 {:?} should map to v3 {:?}",
                v4, expected_v3
            );
        }
    }
}
