//! SysV semaphore — identity, payload, and SEM_UNDO types.
//!
//! `SemArrayIdentity`: key, semid, cred, nsems, perm.
//! `SemArrayPayload`: per-sem values, undo list, changed_seq, wake channel.
//!
//! Day-1 single-namespace: a global `SEM_TABLE` maps semid → Cap.

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
// SemArrayIdentity / SemValue / SemUndo / SemArrayPayload
// ---------------------------------------------------------------------------

/// System V semaphore array identity.
pub struct SemArrayIdentity {
    pub key: Option<SysvKey>,
    pub semid: u32,
    pub cred: Cap<crate::cred::Cred>,
    pub nsems: u16,
    pub perm: IpcPerm,
    pub cuid: u32,
    pub cgid: u32,
    pub destroyed: AtomicBool,
    pub payload: SpinMutex<Option<PayloadCap<SemArrayPayload>>>,
}

/// A single semaphore value within an array.
#[derive(Clone, Copy, Debug, Default)]
pub struct SemValue {
    pub val: i16,
}

/// A single operation in a semop array.
#[derive(Clone, Copy, Debug)]
pub struct SemBuf {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

/// sem_flg bits from `struct sembuf`.
pub mod sem_flg {
    pub const IPC_NOWAIT: i16 = 0o4000;
    pub const SEM_UNDO: i16 = 0o2000;
}

/// A SEM_UNDO entry — per-process adjustment to be reversed at exit.
#[derive(Clone, Debug)]
pub struct SemUndo {
    pub semid: u32,
    pub adjustments: Vec<i16>,
}

/// System V semaphore array payload.
pub struct SemArrayPayload {
    pub values: SpinMutex<Vec<SemValue>>,
    /// Monotonic counter bumped on every semop that changes any value.
    pub changed_seq: AtomicU64,
    /// Wake channel fired when any sem value changes.
    pub changed_channel: Channel,
    pub changed_source_id: u64,
    /// Pending SEM_UNDO entries, keyed by process pid (day-1 proxy).
    /// Walked at process exit by the exit-step undo walk.
    pub undos: SpinMutex<BTreeMap<u64, SemUndo>>,
}

// ---------------------------------------------------------------------------
// Global sem registry
// ---------------------------------------------------------------------------

static SEM_TABLE: SpinMutex<BTreeMap<u32, Cap<SemArrayIdentity>>> = SpinMutex::new(BTreeMap::new());
static NEXT_SEMID: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Zone registration
// ---------------------------------------------------------------------------

static SEM_IDENTITY_ZONE: Zone<SemArrayIdentity> = Zone::const_new();
static SEM_PAYLOAD_ZONE: Zone<SemArrayPayload> = Zone::const_new();

unsafe impl ZoneAllocated for SemArrayIdentity {
    fn zone() -> &'static Zone<Self> {
        &SEM_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for SemArrayPayload {
    type Policy = crate::process::adapter::step_engine::PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &SEM_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<SemArrayIdentity>()?;
    register_zone_for::<SemArrayPayload>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public registry accessors
// ---------------------------------------------------------------------------

pub(crate) fn lookup_sem(semid: u32) -> Option<Cap<SemArrayIdentity>> {
    SEM_TABLE.lock().get(&semid).cloned()
}

pub(crate) fn register_sem(
    key: Option<SysvKey>,
    cred: Cap<crate::cred::Cred>,
    nsems: u16,
    perm: IpcPerm,
    cuid: u32,
    cgid: u32,
) -> Result<u32, ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let semid = NEXT_SEMID.fetch_add(1, Ordering::Relaxed);

    let changed_channel = Channel::new();
    let changed_source_id = crate::wait_source::register_wait_channel(changed_channel.clone());

    let identity = sign(SemArrayIdentity {
        key,
        semid,
        cred,
        nsems,
        perm,
        cuid,
        cgid,
        destroyed: AtomicBool::new(false),
        payload: SpinMutex::new(None),
    })?;
    let payload = sign(SemArrayPayload {
        values: SpinMutex::new(alloc::vec![SemValue::default(); nsems as usize]),
        changed_seq: AtomicU64::new(0),
        changed_channel,
        changed_source_id,
        undos: SpinMutex::new(BTreeMap::new()),
    })?;
    *identity.payload.lock() = Some(PayloadCap::from_cap(payload));
    SEM_TABLE.lock().insert(semid, identity);
    Ok(semid)
}

pub(crate) fn withdraw_sem(semid: u32) -> Option<Cap<SemArrayIdentity>> {
    SEM_TABLE.lock().remove(&semid)
}

pub(crate) fn all_sem_arrays() -> Vec<Cap<SemArrayIdentity>> {
    SEM_TABLE.lock().values().cloned().collect()
}
