//! SysV message queue permission checks.
//!
//! Mirrors the shm check pattern — owner/group/other, root bypass.

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_msg::structure::MsgQueueIdentity;
use crate::process::adapter::step_engine::Cap;

pub fn require_can_read_msg(queue: &MsgQueueIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    let uid = queue.uid();
    let gid = queue.gid();
    let perm = queue.perm();
    if cred.euid.0 == uid || cred.euid.0 == queue.cuid {
        if perm.owner_read() {
            return Ok(());
        }
    } else if cred.egid.0 == gid || cred.egid.0 == queue.cgid {
        if perm.group_read() {
            return Ok(());
        }
    } else if perm.other_read() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

pub fn require_can_write_msg(queue: &MsgQueueIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 {
        return Ok(());
    }
    let uid = queue.uid();
    let gid = queue.gid();
    let perm = queue.perm();
    if cred.euid.0 == uid || cred.euid.0 == queue.cuid {
        if perm.owner_write() {
            return Ok(());
        }
    } else if cred.egid.0 == gid || cred.egid.0 == queue.cgid {
        if perm.group_write() {
            return Ok(());
        }
    } else if perm.other_write() {
        return Ok(());
    }
    Err(Errno::EACCES)
}

pub fn require_owner_or_admin(queue: &MsgQueueIdentity, cred: &Cred) -> Result<(), Errno> {
    if cred.euid.0 == 0 || cred.euid.0 == queue.uid() || cred.euid.0 == queue.cuid {
        return Ok(());
    }
    Err(Errno::EPERM)
}

pub fn require_msg_exists(msqid: u32) -> Result<Cap<MsgQueueIdentity>, Errno> {
    crate::ipc::sysv_msg::structure::lookup_msg(msqid).ok_or_else(|| {
        if crate::ipc::sysv_msg::structure::was_msg_removed(msqid) {
            Errno::EIDRM
        } else {
            Errno::EINVAL
        }
    })
}
