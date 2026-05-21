//! SysV shared memory permission checks.
//!
//! require_can_read_shm, require_can_write_shm, require_owner_or_cap,
//! require_shm_exists.

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_shm::structure::ShmSegmentIdentity;
use crate::process::adapter::step_engine::Cap;

/// Check that the caller has read permission on the segment.
/// Linux checks: if caller is owner (euid == cuid), check owner bits;
/// if caller's gid matches cgid, check group bits; otherwise check
/// other bits. Root (euid == 0) bypasses all checks except IPC_RMID
/// (which requires ownership or CAP_SYS_ADMIN).
pub fn require_can_read_shm(segment: &ShmSegmentIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    let uid = segment.uid();
    let gid = segment.gid();
    let perm = segment.perm();
    if cred.euid.0 == uid || cred.euid.0 == segment.cuid {
        if perm.owner_read() {
            return Ok(());
        }
    } else if cred.egid.0 == gid || cred.egid.0 == segment.cgid {
        if perm.group_read() {
            return Ok(());
        }
    } else if perm.other_read() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

/// Check that the caller has write permission on the segment.
pub fn require_can_write_shm(segment: &ShmSegmentIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    let uid = segment.uid();
    let gid = segment.gid();
    let perm = segment.perm();
    if cred.euid.0 == uid || cred.euid.0 == segment.cuid {
        if perm.owner_write() {
            return Ok(());
        }
    } else if cred.egid.0 == gid || cred.egid.0 == segment.cgid {
        if perm.group_write() {
            return Ok(());
        }
    } else if perm.other_write() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

/// Check that the caller is the owner (euid == cuid) or has
/// `CAP_SYS_ADMIN` (root-euid day-1 approximation).
pub fn require_owner_or_admin(segment: &ShmSegmentIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 || cred.euid.0 == segment.uid() || cred.euid.0 == segment.cuid {
        return Ok(());
    }
    Err(Errno::EPERM)
}

/// Check that a segment with the given shmid exists and is not
/// destroyed-for-new-attaches.
pub fn require_shm_exists(shmid: u32) -> Result<Cap<ShmSegmentIdentity>, Errno> {
    crate::ipc::sysv_shm::structure::lookup_shm(shmid).ok_or(Errno::EINVAL)
}
