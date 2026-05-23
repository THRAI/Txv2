//! POSIX message queue step operations.
//!
//! Phase IPC-4. Thin wrappers over SysV `MsgQueuePayload`.
//! `mq_notify` is a one-shot signal attachment per `SIGNAL_ATTACHMENTS_v1.md`.
//! Timed variants use the syscall-layer timeout pointer only for ABI
//! validation; the day-1 queue operations are non-blocking over the
//! SysV message queue substrate.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::posix_mq::structure::{self, MqNotification};
use crate::ipc::sysv_msg::structure::Msg;
use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::Cap;
use crate::process::adapter::wait_routing::Mask;
use crate::process::nsproxy::PosixMqName;
use crate::signal::SignalTarget;

/// POSIX mq creation attributes copied from musl's LP64 `struct mq_attr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqCreateAttr {
    pub maxmsg: i64,
    pub msgsize: i64,
}

/// Kernel snapshot of the musl-visible mq attributes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqAttr {
    pub flags: i64,
    pub maxmsg: i64,
    pub msgsize: i64,
    pub curmsgs: i64,
}

/// Poll/readiness snapshot for fd-shaped POSIX mq descriptors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqPollInfo {
    pub readable: bool,
    pub writable: bool,
    pub read_source_id: u64,
    pub write_source_id: u64,
}

pub const MQ_O_CREAT: i32 = 0o100;
pub const MQ_O_EXCL: i32 = 0o200;
pub const MQ_O_NONBLOCK: i32 = 0o4000;
pub const MQ_PRIO_MAX: u32 = 32768;

// ---------------------------------------------------------------------------
// step_mq_open
// ---------------------------------------------------------------------------

/// `mq_open(name, oflag, mode, attr)` — open or create a POSIX mq.
///
/// Returns an fd-shaped queue instance on success. The syscall layer
/// wraps it in an `OpenFile` and installs the real userspace fd.
pub fn step_mq_open(
    name: &[u8],
    oflag: i32,
    mode: u16,
    attr: Option<MqCreateAttr>,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<Cap<structure::PosixMqInstance>, Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let mq_name = PosixMqName::new(name);
    let create = (oflag & MQ_O_CREAT) != 0;
    let exclusive = (oflag & MQ_O_EXCL) != 0;
    let instance_flags = (oflag & MQ_O_NONBLOCK) as i64;

    let existing_identity = {
        let table = nsproxy.ipc_ns.posix_mq.lock();
        table.get(&mq_name).cloned()
    };
    if let Some(identity) = existing_identity {
        if exclusive && create {
            return Err(Errno::EEXIST);
        }
        structure::open_instance(identity, instance_flags).map_err(|_| Errno::ENOMEM)
    } else if create {
        let limits = nsproxy.ipc_ns.limits.lock();
        let defaults = MqCreateAttr {
            maxmsg: limits.mq_maxmsg as i64,
            msgsize: limits.mq_msgsize_max.min(limits.msgmax) as i64,
        };
        let attr = attr.unwrap_or(defaults);
        let max_allowed_msgsize = limits.mq_msgsize_max.min(limits.msgmax) as i64;
        let max_allowed_maxmsg = limits.mq_maxmsg as i64;
        drop(limits);

        if attr.maxmsg <= 0
            || attr.msgsize <= 0
            || attr.maxmsg > max_allowed_maxmsg
            || attr.msgsize > max_allowed_msgsize
        {
            return Err(Errno::EINVAL);
        }
        let max_bytes = (attr.maxmsg as usize)
            .checked_mul(attr.msgsize as usize)
            .ok_or(Errno::EINVAL)?;

        let msg_queue = crate::ipc::sysv_msg::structure::register_msg(
            None,
            cred.clone(),
            IpcPerm::new(mode),
            cred.euid.0,
            cred.egid.0,
            max_bytes,
            attr.msgsize as usize,
        )
        .map_err(|_| Errno::ENOMEM)?;
        let (_mqid, identity) = structure::register_mq(
            mq_name,
            cred.clone(),
            IpcPerm::new(mode),
            msg_queue.msqid,
            attr.maxmsg,
            attr.msgsize,
        )
        .map_err(|_| Errno::ENOMEM)?;
        nsproxy
            .ipc_ns
            .posix_mq
            .lock()
            .insert(identity.name.clone(), identity.clone());
        structure::open_instance(identity, instance_flags).map_err(|_| Errno::ENOMEM)
    } else {
        Err(Errno::ENOENT)
    }
}

// ---------------------------------------------------------------------------
// step_mq_send / step_mq_receive
// ---------------------------------------------------------------------------

/// `mq_send(mqfd, msg_ptr, msg_len, msg_prio)` — send a message.
pub fn step_mq_send(
    instance: &structure::PosixMqInstance,
    msg: &[u8],
    prio: u32,
    cred: &Cap<Cred>,
) -> Result<(), Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if prio >= MQ_PRIO_MAX {
        return Err(Errno::EINVAL);
    }
    let mtype = (prio as i64).checked_add(1).ok_or(Errno::EINVAL)?;
    let flags = if (instance.flags() & MQ_O_NONBLOCK as i64) != 0 {
        crate::ipc::sysv_shm::execution::IPC_NOWAIT
    } else {
        0
    };
    {
        let queue =
            crate::ipc::sysv_msg::structure::lookup_msg(instance.msqid()).ok_or(Errno::EINVAL)?;
        let payload_guard = queue.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
        if payload.msg_count.load(Ordering::Acquire) as i64 >= instance.maxmsg() {
            return Err(Errno::EAGAIN);
        }
    }
    let queue = crate::ipc::sysv_msg::checks::require_msg_exists(instance.msqid())?;
    if queue.destroyed.load(Ordering::Acquire) {
        return Err(Errno::EIDRM);
    }
    crate::ipc::sysv_msg::checks::require_can_write_msg(&queue, cred)?;

    let notification = {
        let payload_guard = queue.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
        if msg.len() > payload.max_msg_size {
            return Err(Errno::EINVAL);
        }
        let current_bytes = payload.current_bytes.load(Ordering::Acquire);
        if current_bytes + (msg.len() as u64) > payload.max_bytes as u64 {
            let _ = flags;
            return Err(Errno::EAGAIN);
        }

        let was_empty = payload.msg_count.load(Ordering::Acquire) == 0;
        {
            let mut messages = payload.messages.lock();
            messages.push(Msg {
                mtype,
                mtext: msg.to_vec(),
            });
        }
        payload
            .current_bytes
            .fetch_add(msg.len() as u64, Ordering::Release);
        payload.msg_count.fetch_add(1, Ordering::Release);
        payload.queue_seq.fetch_add(1, Ordering::Release);
        let woken_receivers = payload.recv_channel.fire(Mask::from_bits(1));

        if was_empty && woken_receivers == 0 {
            instance.identity.notify.lock().take()
        } else {
            None
        }
    };

    if let Some(notification) = notification {
        match notification {
            MqNotification::Signal { signum, owner } => {
                let guard = crate::process::adapter::step_engine::guard();
                if let Some(owner) = owner.upgrade(&guard) {
                    drop(guard);
                    let _ =
                        crate::signal::deliver_posix_signal(SignalTarget::Process(owner), signum);
                }
            }
            MqNotification::None => {}
        }
    }
    Ok(())
}

/// `mq_receive(mqfd, msg_ptr, msg_len, msg_prio)` — receive a message.
pub fn step_mq_receive(
    instance: &structure::PosixMqInstance,
    max_len: usize,
    cred: &Cap<Cred>,
) -> Result<(Vec<u8>, u32), Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let flags = if (instance.flags() & MQ_O_NONBLOCK as i64) != 0 {
        crate::ipc::sysv_shm::execution::IPC_NOWAIT
    } else {
        0
    };
    let queue =
        crate::ipc::sysv_msg::structure::lookup_msg(instance.msqid()).ok_or(Errno::EINVAL)?;
    crate::ipc::sysv_msg::checks::require_can_read_msg(&queue, cred)?;
    let payload_guard = queue.payload.lock();
    let payload = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
    if payload.msg_count.load(Ordering::Acquire) == 0 {
        let _ = flags;
        return Err(Errno::EAGAIN);
    }

    let mut messages = payload.messages.lock();
    let mut best_pos = None;
    let mut best_type = i64::MIN;
    for (index, message) in messages.iter().enumerate() {
        if message.mtype > best_type {
            best_type = message.mtype;
            best_pos = Some(index);
        }
    }
    let pos = best_pos.ok_or(Errno::EAGAIN)?;
    if messages[pos].mtext.len() > max_len {
        return Err(Errno::E2BIG);
    }
    let msg = messages.remove(pos);
    let msg_len = msg.mtext.len();
    drop(messages);
    payload
        .current_bytes
        .fetch_sub(msg_len as u64, Ordering::Release);
    payload.msg_count.fetch_sub(1, Ordering::Release);
    payload.send_channel.fire(Mask::from_bits(1));
    Ok((msg.mtext, msg.mtype.saturating_sub(1) as u32))
}

pub fn step_mq_getattr(instance: &structure::PosixMqInstance) -> Result<MqAttr, Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let queue =
        crate::ipc::sysv_msg::structure::lookup_msg(instance.msqid()).ok_or(Errno::EINVAL)?;
    let payload_guard = queue.payload.lock();
    let payload = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
    Ok(MqAttr {
        flags: instance.flags(),
        maxmsg: instance.maxmsg(),
        msgsize: instance.msgsize(),
        curmsgs: payload.msg_count.load(Ordering::Acquire) as i64,
    })
}

pub fn step_mq_poll_info(instance: &structure::PosixMqInstance) -> Result<MqPollInfo, Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let queue =
        crate::ipc::sysv_msg::structure::lookup_msg(instance.msqid()).ok_or(Errno::EINVAL)?;
    let payload_guard = queue.payload.lock();
    let payload = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
    let msg_count = payload.msg_count.load(Ordering::Acquire) as i64;
    let current_bytes = payload.current_bytes.load(Ordering::Acquire);
    Ok(MqPollInfo {
        readable: msg_count > 0,
        writable: msg_count < instance.maxmsg() && current_bytes < payload.max_bytes as u64,
        read_source_id: payload.recv_source_id,
        write_source_id: payload.send_source_id,
    })
}

/// Set only the mutable `mq_flags` field. Linux ignores the other
/// members of `newattr` for `mq_setattr(3)`.
pub fn step_mq_setattr(
    instance: &structure::PosixMqInstance,
    new_flags: i64,
) -> Result<MqAttr, Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if new_flags & !(MQ_O_NONBLOCK as i64) != 0 {
        return Err(Errno::EINVAL);
    }
    let old = step_mq_getattr(instance)?;
    instance.set_flags(new_flags);
    Ok(old)
}

// ---------------------------------------------------------------------------
// step_mq_notify
// ---------------------------------------------------------------------------

/// `mq_notify(mqfd, sevp)` — register for notification.
///
/// One-shot: at most one notification is sent. If another process
/// has already registered, returns `EBUSY`. `None` sevp cancels.
/// `SIGEV_SIGNAL` and `SIGEV_THREAD_ID` route through the POSIX
/// signal mailbox path; `SIGEV_THREAD` callback delivery is deferred.
pub fn step_mq_notify(
    instance: &structure::PosixMqInstance,
    notification: Option<MqNotification>,
) -> Result<(), Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let mut slot = instance.identity.notify.lock();
    if notification.is_some() && slot.is_some() {
        return Err(Errno::EBUSY);
    }
    *slot = notification;
    Ok(())
}

// ---------------------------------------------------------------------------
// step_mq_unlink / step_mq_close
// ---------------------------------------------------------------------------

/// `mq_unlink(name)` — remove a POSIX mq by name.
pub fn step_mq_unlink(
    name: &[u8],
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<(), Errno> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let mq_name = PosixMqName::new(name);
    if nsproxy.ipc_ns.posix_mq.lock().remove(&mq_name).is_some() {
        Ok(())
    } else {
        Err(Errno::ENOENT)
    }
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
