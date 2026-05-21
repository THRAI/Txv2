//! SysV semaphore permission checks.

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_sem::structure::SemArrayIdentity;
use crate::process::adapter::step_engine::Cap;

pub fn require_can_read_sem(array: &SemArrayIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    if cred.euid.0 == array.uid() {
        if array.perm().owner_read() {
            return Ok(());
        }
    } else if cred.egid.0 == array.gid() {
        if array.perm().group_read() {
            return Ok(());
        }
    } else if array.perm().other_read() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

pub fn require_can_write_sem(array: &SemArrayIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    if cred.euid.0 == array.uid() {
        if array.perm().owner_write() {
            return Ok(());
        }
    } else if cred.egid.0 == array.gid() {
        if array.perm().group_write() {
            return Ok(());
        }
    } else if array.perm().other_write() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

pub fn require_owner_or_admin(array: &SemArrayIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 || cred.euid.0 == array.uid() || cred.euid.0 == array.cuid {
        return Ok(());
    }
    Err(Errno::EPERM)
}

pub fn require_sem_exists(semid: u32) -> Result<Cap<SemArrayIdentity>, Errno> {
    crate::ipc::sysv_sem::structure::lookup_sem(semid).ok_or(Errno::EINVAL)
}
