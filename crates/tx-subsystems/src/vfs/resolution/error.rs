//! WalkCause → Errno classification per `txdoc:VFS-CHECKS-ERROR-CLASSIFY-1` (§7).
//!
//! Converts a resolved [`WalkCause`] into the caller-visible [`Errno`].
//! The mapping is total: every variant maps to exactly one POSIX error
//! code, with pass-through for backend-rejected errors.

use crate::execution::Errno;

use super::state::{NonTerminalDenial, WalkCause};

/// Map a walk-failure cause to the appropriate POSIX errno.
///
/// Per `txdoc:VFS-CHECKS-ERROR-CLASSIFY-1` — the function is pure
/// (no IO, no allocation) and total.
pub fn classify(cause: &WalkCause) -> Errno {
    match cause {
        WalkCause::TraverseDenied => Errno::EACCES,
        WalkCause::ComponentNotFound => Errno::ENOENT,
        WalkCause::NotADirectory => Errno::ENOTDIR,
        WalkCause::SymlinkLimit => Errno::ELOOP,
        WalkCause::MountPointGap => Errno::EIO,
        WalkCause::FsOpsRejected(e) => *e,
        WalkCause::Permission(denial) => match denial {
            NonTerminalDenial::SearchDenied => Errno::EACCES,
        },
        WalkCause::TerminalOpenFailed(e) => *e,
    }
}
