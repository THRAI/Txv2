//! Small helpers and constants shared across syscall modules.
//!
//! Items here are `pub(super)` so every sub-module under
//! `linux_syscall` can reach them through the parent re-export.

use super::numbers::{DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK};
use crate::adapter::step_engine::Cap;
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vfs::structure::InodeKind;

/// Read 8 little-endian bytes from a slice as a `u64`. Used by
/// `sys_rt_sigaction`'s `struct sigaction` decode.
pub(super) fn read_u64_le(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() >= 8);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

/// Resolve fd `idx` against the process payload's fd table. Returns
/// `None` if the process is a zombie or the slot is empty.
///
/// Per fd-ops Wave 1 the table is a sparse `BTreeMap<u32, Cap<OpenFile>>`;
/// any `u32` fd value is a valid key.
pub(super) fn resolve_fd(process: &Cap<ProcessIdentity>, idx: u32) -> Option<Cap<OpenFile>> {
    process.fd(idx)
}

/// field alone" signal without any further sign-extension dance.
pub(super) const UID_LEAVE_UNCHANGED: u32 = u32::MAX;

/// to compute the trailing name-and-pad offset.
pub(super) const LINUX_DIRENT64_HEADER_BYTES: usize = 19;

/// Default `st_blksize` reported by Slice 6's stat arms. Linux's
/// page-backed filesystems all report 4096; txKernel has no
/// per-FS blocksize hint to override this with today.
pub(super) const STAT_BLKSIZE: i32 = 4096;

/// Round `x` up to the nearest multiple of 8. Used by `getdents64` to
/// pad records to the 8-byte boundary the ABI requires.
pub(super) const fn align_up_8(x: usize) -> usize {
    (x + 7) & !7
}

/// Project an `InodeKind` onto the `linux_dirent64` `d_type` byte. The
/// match exhausts every variant of the enum (verified from
/// `vfs::structure::InodeKind`).
pub(super) const fn inode_kind_to_dt(kind: InodeKind) -> u8 {
    match kind {
        InodeKind::Regular => DT_REG,
        InodeKind::Directory => DT_DIR,
        InodeKind::Symlink => DT_LNK,
        InodeKind::CharDevice => DT_CHR,
        InodeKind::BlockDevice => DT_BLK,
        InodeKind::Fifo => DT_FIFO,
        InodeKind::Socket => DT_SOCK,
    }
}

// ---------------------------------------------------------------------
// Layout structs for Slice 7 syscall arms.
// ---------------------------------------------------------------------

/// Linux uapi `struct utsname` field width (`__NEW_UTS_LEN + 1 = 65`).
pub(super) const UTSNAME_FIELD: usize = 65;
