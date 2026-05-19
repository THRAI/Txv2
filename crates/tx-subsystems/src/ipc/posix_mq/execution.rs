//! POSIX message queue step operations.
//!
//! Phase IPC-4. Thin wrappers over SysV `MsgQueuePayload`.
//! `mq_notify` is a one-shot signal attachment per `SIGNAL_ATTACHMENTS_v1.md`.
//! Timed variants (mq_timedsend/mq_timedreceive) are deferred.

use alloc::vec::Vec;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::posix_mq::structure;
use crate::ipc::sysv_msg::execution as msg;
use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::Cap;
use crate::process::nsproxy::PosixMqName;

// ---------------------------------------------------------------------------
// step_mq_open
// ---------------------------------------------------------------------------

/// `mq_open(name, oflag, mode, attr)` — open or create a POSIX mq.
///
/// Returns an fd-shaped mq descriptor on success.
pub fn step_mq_open(
    name: &[u8],
    oflag: i32,
    mode: u16,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<u32, Errno> {
    let mq_name = PosixMqName::new(name);
    let create = (oflag & 0o100) != 0; // O_CREAT
    let exclusive = (oflag & 0o200) != 0; // O_EXCL

    if let Some(identity) = structure::lookup_mq_by_name(&mq_name) {
        if exclusive && create {
            return Err(Errno::EEXIST);
        }
        let msqid = identity.msqid;
        structure::alloc_mqfd(msqid, oflag).map_err(|_| Errno::ENOMEM)
    } else if create {
        // Create underlying SysV msg queue first.
        let msqid = msg::step_msgget(
            crate::ipc::sysv_shm::execution::IPC_PRIVATE,
            0o666,
            cred,
            nsproxy,
        )?;
        structure::register_mq(mq_name, cred.clone(), IpcPerm::new(mode), msqid)
            .map_err(|_| Errno::ENOMEM)?;
        structure::alloc_mqfd(msqid, oflag).map_err(|_| Errno::ENOMEM)
    } else {
        Err(Errno::ENOENT)
    }
}

// ---------------------------------------------------------------------------
// step_mq_send / step_mq_receive
// ---------------------------------------------------------------------------

/// `mq_send(mqfd, msg_ptr, msg_len, msg_prio)` — send a message.
pub fn step_mq_send(mqfd: u32, msg: Vec<u8>, prio: u32, cred: &Cap<Cred>) -> Result<(), Errno> {
    let instance = structure::lookup_instance(mqfd).ok_or(Errno::EBADF)?;
    let msqid = instance.msqid;
    msg::step_msgsnd(msqid, prio as i64, msg, 0, cred)?;
    Ok(())
}

/// `mq_receive(mqfd, msg_ptr, msg_len, msg_prio)` — receive a message.
pub fn step_mq_receive(
    mqfd: u32,
    max_len: usize,
    cred: &Cap<Cred>,
) -> Result<(Vec<u8>, u32), Errno> {
    let instance = structure::lookup_instance(mqfd).ok_or(Errno::EBADF)?;
    let msqid = instance.msqid;
    let (mtype, mtext) = msg::step_msgrcv(msqid, max_len, 0, 0, cred)?;
    Ok((mtext, mtype as u32))
}

// ---------------------------------------------------------------------------
// step_mq_notify
// ---------------------------------------------------------------------------

/// `mq_notify(mqfd, sevp)` — register for notification.
///
/// One-shot: at most one notification is sent. If another process
/// has already registered, returns `EBUSY`. `None` sevp cancels.
/// Only `SIGEV_SIGNAL` is supported; `SIGEV_THREAD` is glibc concern.
pub fn step_mq_notify(mqfd: u32, signum: Option<crate::signal::Signum>) -> Result<(), Errno> {
    let instance = structure::lookup_instance(mqfd).ok_or(Errno::EBADF)?;
    let mut slot = instance.notify_signal.lock();
    if signum.is_some() && slot.is_some() {
        return Err(Errno::EBUSY);
    }
    *slot = signum;
    Ok(())
}

// ---------------------------------------------------------------------------
// step_mq_unlink / step_mq_close
// ---------------------------------------------------------------------------

/// `mq_unlink(name)` — remove a POSIX mq by name.
pub fn step_mq_unlink(name: &[u8]) -> Result<(), Errno> {
    let mq_name = PosixMqName::new(name);
    if structure::unlink_mq(&mq_name) {
        Ok(())
    } else {
        Err(Errno::ENOENT)
    }
}

/// `close(mqfd)` — release an mq descriptor.
pub fn step_mq_close(mqfd: u32) {
    structure::close_instance(mqfd);
}

// ---------------------------------------------------------------------------
// mq_notify fire helper
// ---------------------------------------------------------------------------

/// Fire mq_notify for a queue if registered. Called from the SysV
/// msgsnd path when a message arrives on an empty queue that has a
/// notification registered.
pub fn try_fire_mq_notify(_msqid: u32) {
    // Walk all instances and find any that reference this msqid with
    // a pending notification. Day-1: iterate the global table.
    // TODO: optimize with a reverse index msqid → instance list.
    // TODO: walk instance table and fire signal for matching msqid.
    // The signal firing lives in the syscall arm since we need
    // process context to deliver.
}
