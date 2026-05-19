//! SysV message queue — identity, payload, and message types.
//!
//! `MsgQueueIdentity`: key, msqid, cred, perm.
//! `MsgQueuePayload`: message ring, send/recv WaitSources, queue_seq.
//!
//! Day-1 single-namespace: a global `MSG_TABLE` maps msqid → Cap.
//! Follows the same pattern as `sysv_shm::structure`.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::{
    Cap, PayloadCap, SpinMutex, Zone, ZoneAllocated, ZoneError,
};
use crate::process::adapter::wait_routing::Channel;
use crate::process::nsproxy::SysvKey;

// ---------------------------------------------------------------------------
// MsgQueueIdentity / MsgQueuePayload
// ---------------------------------------------------------------------------

/// System V message queue identity.
///
/// Carries a `payload` slot (matching `ProcessIdentity.payload` shape)
/// so step functions can access the live message ring without a
/// separate global payload table.
pub struct MsgQueueIdentity {
    pub key: Option<SysvKey>,
    pub msqid: u32,
    pub cred: Cap<crate::cred::Cred>,
    pub perm: IpcPerm,
    /// Creator uid/gid — used by msgctl IPC_STAT.
    pub cuid: u32,
    pub cgid: u32,
    /// Marked for deletion (IPC_RMID has been called).
    pub destroyed: AtomicBool,
    /// Live payload — the message ring, WaitSources, sequence counter.
    pub payload: SpinMutex<Option<PayloadCap<MsgQueuePayload>>>,
}

/// A single message in the queue.
#[derive(Clone, Debug)]
pub struct Msg {
    /// Message type — must be > 0 per POSIX.
    pub mtype: i64,
    /// Message payload bytes.
    pub mtext: Vec<u8>,
}

/// System V message queue payload — the live message ring.
///
/// Phase IPC-2: WaitSources for blocking send/recv, per-type
/// sequencing for `msgrcv` with `msgtyp < 0`.
pub struct MsgQueuePayload {
    /// Messages currently in the queue (FIFO order).
    pub messages: SpinMutex<Vec<Msg>>,
    /// Total bytes currently in the queue (sum of mtext lengths).
    pub current_bytes: AtomicU64,
    /// Max bytes allowed (from IpcLimits.msgmnb).
    pub max_bytes: usize,
    /// Per-message max size (from IpcLimits.msgmax).
    pub max_msg_size: usize,
    /// Monotonic sequence counter — bumped by msgsnd, checked by
    /// msgrcv with sequenced predicates.
    pub queue_seq: AtomicU64,
    /// Number of messages currently in the queue. Atomic so readers
    /// can check emptiness without locking the message vec.
    pub msg_count: AtomicU32,
    /// Wake channel for blocked senders (fired when a receiver drains
    /// bytes, making space available).
    pub send_channel: Channel,
    /// Wake channel for blocked receivers (fired when a sender pushes
    /// a message).
    pub recv_channel: Channel,
    /// Registered wait-source ids for poll/select integration.
    pub send_source_id: u64,
    pub recv_source_id: u64,
}

// ---------------------------------------------------------------------------
// Global msg registry — day-1 single-namespace
// ---------------------------------------------------------------------------

static MSG_TABLE: SpinMutex<BTreeMap<u32, Cap<MsgQueueIdentity>>> = SpinMutex::new(BTreeMap::new());

static NEXT_MSGID: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Zone registration
// ---------------------------------------------------------------------------

static MSG_IDENTITY_ZONE: Zone<MsgQueueIdentity> = Zone::const_new();
static MSG_PAYLOAD_ZONE: Zone<MsgQueuePayload> = Zone::const_new();

unsafe impl ZoneAllocated for MsgQueueIdentity {
    fn zone() -> &'static Zone<Self> {
        &MSG_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for MsgQueuePayload {
    type Policy = crate::process::adapter::step_engine::PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &MSG_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<MsgQueueIdentity>()?;
    register_zone_for::<MsgQueuePayload>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public registry accessors
// ---------------------------------------------------------------------------

pub(crate) fn lookup_msg(msqid: u32) -> Option<Cap<MsgQueueIdentity>> {
    MSG_TABLE.lock().get(&msqid).cloned()
}

pub(crate) fn register_msg(
    key: Option<SysvKey>,
    cred: Cap<crate::cred::Cred>,
    perm: IpcPerm,
    cuid: u32,
    cgid: u32,
    max_bytes: usize,
    max_msg_size: usize,
) -> Result<u32, ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let msqid = NEXT_MSGID.fetch_add(1, Ordering::Relaxed);

    let send_channel = Channel::new();
    let recv_channel = Channel::new();
    let send_source_id = crate::wait_source::register_wait_channel(send_channel.clone());
    let recv_source_id = crate::wait_source::register_wait_channel(recv_channel.clone());

    let identity = sign(MsgQueueIdentity {
        key,
        msqid,
        cred,
        perm,
        cuid,
        cgid,
        destroyed: AtomicBool::new(false),
        payload: SpinMutex::new(None),
    })?;
    let payload = sign(MsgQueuePayload {
        messages: SpinMutex::new(Vec::new()),
        current_bytes: AtomicU64::new(0),
        max_bytes,
        max_msg_size,
        queue_seq: AtomicU64::new(0),
        msg_count: AtomicU32::new(0),
        send_channel,
        recv_channel,
        send_source_id,
        recv_source_id,
    })?;
    *identity.payload.lock() = Some(PayloadCap::from_cap(payload));
    MSG_TABLE.lock().insert(msqid, identity);
    Ok(msqid)
}

pub(crate) fn withdraw_msg(msqid: u32) -> Option<Cap<MsgQueueIdentity>> {
    MSG_TABLE.lock().remove(&msqid)
}

#[expect(
    dead_code,
    reason = "txdoc:IPC-V1-MSG-1 — consumed by procfs projection when wired"
)]
pub(crate) fn all_msg_queues() -> Vec<Cap<MsgQueueIdentity>> {
    MSG_TABLE.lock().values().cloned().collect()
}
