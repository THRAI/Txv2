//! Shared step-result and error spelling for interface-only subsystem seams.
//!
//! The concrete reactor carriers are still owned by the reactor and bus work.
//! This module gives VFS, Mount, PageBacked, and filesystem backends one public
//! spelling to compile against until those carriers are connected.

pub use tx_substrate::epoch::Guard;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    EACCES,
    /// Resource temporarily unavailable. Surfaced by `O_NONBLOCK` I/O
    /// paths (e.g. fd-ops Wave 3 `pipe::step_read` / `step_write` with
    /// `nonblocking = true` and no progress yet).
    EAGAIN,
    /// Bad file descriptor. Today only surfaced by fd-ops Wave 3
    /// pipe dispatch when a wrong-side `step_read` / `step_write`
    /// reaches the dispatcher despite the OpenFileFlags read/write
    /// guard (defence in depth — the flag check at the top of
    /// `OpenFile::step_read/step_write` returns `EINVAL` first for
    /// the common case). Linux semantic: `read(2)` on a writer-end
    /// fd is `-EBADF`, not `-EPIPE`.
    EBADF,
    EBUSY,
    EDQUOT,
    EEXIST,
    EFAULT,
    EINVAL,
    EIO,
    EISDIR,
    ELOOP,
    ENAMETOOLONG,
    ENODEV,
    ENOEXEC,
    ENOMEM,
    ENOENT,
    ENOSYS,
    ENOTDIR,
    ENOTEMPTY,
    /// Inappropriate ioctl for device. Surfaced by Slice 5 of the
    /// shell-prompt roadmap (`ioctl(2)` arm) when the target fd is not
    /// a TTY (terminal-shape ioctl on a pipe / regular file / dir / etc.)
    /// or the request code is not one of the eight TTY ioctls v1
    /// implements. Linux value: 25.
    ENOTTY,
    EPERM,
    /// Broken pipe: write to a pipe with all readers closed. The
    /// caller is responsible for delivering SIGPIPE before returning
    /// `-EPIPE` to userspace (fd-ops Wave 3, Q2 DECIDED 2026-05-07).
    EPIPE,
    /// Numerical result out of range. Surfaced by Slice 6's
    /// `getcwd(2)` arm when the user buffer is smaller than the
    /// rendered path (NUL terminator inclusive). Linux value: 34.
    ERANGE,
    EROFS,
    /// Illegal seek. Surfaced by `lseek(2)` when called against a
    /// non-seekable file (pipe / TTY / chardev / socket). fd-ops
    /// Wave 4. Linux value: 29.
    ESPIPE,
    ESRCH,
    ESTALE,
}

/// Bridge `execution::Errno` into `step_v3::Errno`. The two enums
/// mirror each other byte-for-byte (see `tx_substrate::step_v3::Errno`),
/// so this is a 1:1 same-name mapping. The match is exhaustive with no
/// wildcard: adding a new variant on either side fails to compile here
/// until the other is extended, which keeps the two catalogs in
/// lock-step.
impl From<Errno> for tx_substrate::step_v3::Errno {
    fn from(value: Errno) -> Self {
        match value {
            Errno::EACCES => Self::EACCES,
            Errno::EAGAIN => Self::EAGAIN,
            Errno::EBADF => Self::EBADF,
            Errno::EBUSY => Self::EBUSY,
            Errno::EDQUOT => Self::EDQUOT,
            Errno::EEXIST => Self::EEXIST,
            Errno::EFAULT => Self::EFAULT,
            Errno::EINVAL => Self::EINVAL,
            Errno::EIO => Self::EIO,
            Errno::EISDIR => Self::EISDIR,
            Errno::ELOOP => Self::ELOOP,
            Errno::ENAMETOOLONG => Self::ENAMETOOLONG,
            Errno::ENODEV => Self::ENODEV,
            Errno::ENOEXEC => Self::ENOEXEC,
            Errno::ENOMEM => Self::ENOMEM,
            Errno::ENOENT => Self::ENOENT,
            Errno::ENOSYS => Self::ENOSYS,
            Errno::ENOTDIR => Self::ENOTDIR,
            Errno::ENOTEMPTY => Self::ENOTEMPTY,
            Errno::ENOTTY => Self::ENOTTY,
            Errno::EPERM => Self::EPERM,
            Errno::EPIPE => Self::EPIPE,
            Errno::ERANGE => Self::ERANGE,
            Errno::EROFS => Self::EROFS,
            Errno::ESPIPE => Self::ESPIPE,
            Errno::ESRCH => Self::ESRCH,
            Errno::ESTALE => Self::ESTALE,
        }
    }
}

/// Reverse bridge — `step_v3::Errno → execution::Errno`. Inverse of
/// the `From<Errno> for step_v3::Errno` impl above so tx-shims call
/// sites can route the step_v3 outcome's errno back through the
/// existing `errno_to_i32` translation table without each site
/// reproducing the variant-by-variant mapping. Exhaustive no-wildcard
/// match: a future `step_v3::Errno`-only addition fails to compile
/// until the `execution::Errno` mirror is grown.
impl From<tx_substrate::step_v3::Errno> for Errno {
    fn from(value: tx_substrate::step_v3::Errno) -> Self {
        use tx_substrate::step_v3::Errno as V3;
        match value {
            V3::EACCES => Errno::EACCES,
            V3::EAGAIN => Errno::EAGAIN,
            V3::EBADF => Errno::EBADF,
            V3::EBUSY => Errno::EBUSY,
            V3::EDQUOT => Errno::EDQUOT,
            V3::EEXIST => Errno::EEXIST,
            V3::EFAULT => Errno::EFAULT,
            V3::EINVAL => Errno::EINVAL,
            V3::EIO => Errno::EIO,
            V3::EISDIR => Errno::EISDIR,
            V3::ELOOP => Errno::ELOOP,
            V3::ENAMETOOLONG => Errno::ENAMETOOLONG,
            V3::ENODEV => Errno::ENODEV,
            V3::ENOEXEC => Errno::ENOEXEC,
            V3::ENOMEM => Errno::ENOMEM,
            V3::ENOENT => Errno::ENOENT,
            V3::ENOSYS => Errno::ENOSYS,
            V3::ENOTDIR => Errno::ENOTDIR,
            V3::ENOTEMPTY => Errno::ENOTEMPTY,
            V3::ENOTTY => Errno::ENOTTY,
            V3::EPERM => Errno::EPERM,
            V3::EPIPE => Errno::EPIPE,
            V3::ERANGE => Errno::ERANGE,
            V3::EROFS => Errno::EROFS,
            V3::ESPIPE => Errno::ESPIPE,
            V3::ESRCH => Errno::ESRCH,
            V3::ESTALE => Errno::ESTALE,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_token_preserves_carrier_and_interest_spelling() {
        let token = WaitToken::new(4, 0b101);

        assert_eq!(token.carrier(), 4);
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
        use tx_substrate::step_v3::Errno as V3;
        let table: [(Errno, V3); 27] = [
            (Errno::EACCES, V3::EACCES),
            (Errno::EAGAIN, V3::EAGAIN),
            (Errno::EBADF, V3::EBADF),
            (Errno::EBUSY, V3::EBUSY),
            (Errno::EDQUOT, V3::EDQUOT),
            (Errno::EEXIST, V3::EEXIST),
            (Errno::EFAULT, V3::EFAULT),
            (Errno::EINVAL, V3::EINVAL),
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
        ];
        assert_eq!(table.len(), 27);
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
