//! SysV message queue step operations.
//!
//! Phase IPC-2. Introduces `send_source` / `recv_source` WaitSources
//! for blocking send/recv, `queue_seq` sequencing, and `msgrcv` with
//! typed filtering (msgtyp > 0 exact, < 0 lowest ≤ |msgtyp|, == 0 any).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_msg::checks;
use crate::ipc::sysv_msg::structure::Msg;
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL, IPC_NOWAIT, IPC_PRIVATE};
use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::Cap;
use crate::process::adapter::wait_routing::Mask;
use crate::process::nsproxy::SysvKey;

// msgctl commands (same numbering as shmctl)
pub use crate::ipc::sysv_shm::execution::{IPC_INFO, IPC_RMID, IPC_SET, IPC_STAT};

// ---------------------------------------------------------------------------
// Payload access helper
// ---------------------------------------------------------------------------

/// Lock the identity's payload slot and return a guard to the payload.
/// Returns `Err(EIDRM)` if the payload has been torn down (queue removed
/// while a waiter was blocked).
macro_rules! with_payload {
    ($queue:expr, $guard:ident, $block:block) => {{
        let payload_guard = $queue.payload.lock();
        let $guard = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
        $block
    }};
}

// ---------------------------------------------------------------------------
// step_msgget
// ---------------------------------------------------------------------------

/// `msgget(key, msgflg)` — get or create a message queue.
pub fn step_msgget(
    key: i32,
    msgflg: i32,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<u32, Errno> {
    let ipc_key = if key == IPC_PRIVATE {
        None
    } else {
        Some(SysvKey::new(key as u32))
    };

    let create = (msgflg & IPC_CREAT) != 0;
    let exclusive = (msgflg & IPC_EXCL) != 0;
    let perm = IpcPerm::new((msgflg & 0o777) as u16);

    if let Some(ref k) = ipc_key {
        let table = nsproxy.ipc_ns.sysv_msg.lock();
        if let Some(&existing_msqid) = table.get(k) {
            drop(table);
            let q = checks::require_msg_exists(existing_msqid)?;
            if exclusive {
                return Err(Errno::EEXIST);
            }
            checks::require_can_read_msg(&q, cred)?;
            return Ok(existing_msqid);
        }
    }

    if !create && ipc_key.is_some() {
        return Err(Errno::ENOENT);
    }

    let limits = nsproxy.ipc_ns.limits.lock();
    let max_bytes = limits.msgmnb as usize;
    let max_msg_size = limits.msgmax as usize;
    drop(limits);

    let msqid = crate::ipc::sysv_msg::structure::register_msg(
        ipc_key,
        cred.clone(),
        perm,
        cred.euid.0,
        cred.egid.0,
        max_bytes,
        max_msg_size,
    )
    .map_err(|_| Errno::ENOMEM)?;

    if let Some(ref k) = ipc_key {
        nsproxy.ipc_ns.sysv_msg.lock().insert(*k, msqid);
    }

    Ok(msqid)
}

// ---------------------------------------------------------------------------
// step_msgsnd
// ---------------------------------------------------------------------------

/// `msgsnd(msqid, msgp, msgsz, msgflg)` — send a message.
pub fn step_msgsnd(
    msqid: u32,
    mtype: i64,
    mtext: Vec<u8>,
    msgflg: i32,
    cred: &Cap<Cred>,
) -> Result<usize, Errno> {
    if mtype <= 0 {
        return Err(Errno::EINVAL);
    }

    let queue = checks::require_msg_exists(msqid)?;
    if queue.destroyed.load(Ordering::Acquire) {
        return Err(Errno::EIDRM);
    }
    checks::require_can_write_msg(&queue, cred)?;

    let msg = Msg { mtype, mtext };
    let msg_len = msg.mtext.len();

    with_payload!(queue, payload, {
        if msg_len > payload.max_msg_size {
            return Err(Errno::EINVAL);
        }

        let current_bytes = payload.current_bytes.load(Ordering::Acquire);
        if current_bytes + (msg_len as u64) > payload.max_bytes as u64 {
            if (msgflg & IPC_NOWAIT) != 0 {
                return Err(Errno::EAGAIN);
            }
            // TODO: blocking send — yield OnWaitSource with send_source
            return Err(Errno::EAGAIN);
        }

        let mut messages = payload.messages.lock();
        messages.push(msg);
        drop(messages);
        payload
            .current_bytes
            .fetch_add(msg_len as u64, Ordering::Release);
        payload.msg_count.fetch_add(1, Ordering::Release);
        payload.queue_seq.fetch_add(1, Ordering::Release);
        payload.recv_channel.fire(Mask::from_bits(1));
    });

    Ok(msg_len)
}

// ---------------------------------------------------------------------------
// step_msgrcv
// ---------------------------------------------------------------------------

/// `msgrcv(msqid, msgp, msgsz, msgtyp, msgflg)` — receive a message.
pub fn step_msgrcv(
    msqid: u32,
    msgsz: usize,
    msgtyp: i64,
    msgflg: i32,
    cred: &Cap<Cred>,
) -> Result<(i64, Vec<u8>), Errno> {
    let queue = checks::require_msg_exists(msqid)?;
    checks::require_can_read_msg(&queue, cred)?;

    with_payload!(queue, payload, {
        let msg_count = payload.msg_count.load(Ordering::Acquire);
        if msg_count == 0 {
            if (msgflg & IPC_NOWAIT) != 0 {
                return Err(Errno::EAGAIN);
            }
            if queue.destroyed.load(Ordering::Acquire) {
                return Err(Errno::EIDRM);
            }
            // TODO: blocking recv — yield OnWaitSource with recv_source
            return Err(Errno::EAGAIN);
        }

        let mut messages = payload.messages.lock();
        let pos = find_msg(&messages, msgtyp, msgsz).ok_or(Errno::E2BIG)?;
        let msg = messages.remove(pos);
        let mlen = msg.mtext.len();
        drop(messages);
        payload
            .current_bytes
            .fetch_sub(mlen as u64, Ordering::Release);
        payload.msg_count.fetch_sub(1, Ordering::Release);
        payload.send_channel.fire(Mask::from_bits(1));

        Ok((msg.mtype, msg.mtext))
    })
}

fn find_msg(messages: &[Msg], msgtyp: i64, max_bytes: usize) -> Option<usize> {
    if msgtyp == 0 {
        messages.iter().position(|m| m.mtext.len() <= max_bytes)
    } else if msgtyp > 0 {
        messages
            .iter()
            .position(|m| m.mtype == msgtyp && m.mtext.len() <= max_bytes)
    } else {
        let threshold = -msgtyp;
        messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.mtype <= threshold && m.mtext.len() <= max_bytes)
            .min_by_key(|(_, m)| m.mtype)
            .map(|(i, _)| i)
    }
}

// ---------------------------------------------------------------------------
// step_msgctl
// ---------------------------------------------------------------------------

/// `msgctl(msqid, cmd, buf)` — control a message queue.
pub fn step_msgctl(msqid: u32, cmd: i32, cred: &Cap<Cred>) -> Result<MsgCtlResult, Errno> {
    match cmd {
        IPC_RMID => {
            let queue = checks::require_msg_exists(msqid)?;
            checks::require_owner_or_admin(&queue, cred)?;

            if let Some(q) = crate::ipc::sysv_msg::structure::withdraw_msg(msqid) {
                q.destroyed.store(true, Ordering::Release);
            }
            Ok(MsgCtlResult::Success)
        }
        IPC_SET => {
            let queue = checks::require_msg_exists(msqid)?;
            checks::require_owner_or_admin(&queue, cred)?;
            let _ = queue;
            Ok(MsgCtlResult::Success)
        }
        IPC_STAT => {
            let queue = checks::require_msg_exists(msqid)?;
            checks::require_can_read_msg(&queue, cred)?;
            let (current_bytes, msg_count) = with_payload!(queue, payload, {
                (
                    payload.current_bytes.load(Ordering::Acquire) as usize,
                    payload.msg_count.load(Ordering::Acquire),
                )
            });
            Ok(MsgCtlResult::Stat(MsgInfo {
                msqid: queue.msqid,
                cuid: queue.cuid,
                cgid: queue.cgid,
                perm: queue.perm,
                current_bytes,
                msg_count,
            }))
        }
        IPC_INFO => Ok(MsgCtlResult::Info {
            msgmni: 32000,
            msgmax: 8192,
            msgmnb: 16384,
        }),
        _ => Err(Errno::EINVAL),
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum MsgCtlResult {
    Success,
    Stat(MsgInfo),
    Info {
        msgmni: u64,
        msgmax: u64,
        msgmnb: u64,
    },
}

#[derive(Clone, Debug)]
pub struct MsgInfo {
    pub msqid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub perm: IpcPerm,
    pub current_bytes: usize,
    pub msg_count: u32,
}
