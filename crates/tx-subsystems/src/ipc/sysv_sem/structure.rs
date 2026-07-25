//! SysV semaphore — identity, payload, and SEM_UNDO types.
//!
//! `SemArrayIdentity`: key, semid, cred, nsems, perm.
//! `SemArrayPayload`: per-sem values, changed_seq, wake source.
//!
//! Day-1 single-namespace: a global `SEM_TABLE` maps semid → Cap.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::{
    Cap, PayloadCap, SpinMutex, Zone, ZoneAllocated, ZoneError,
};
use crate::process::adapter::wait_routing::{self, WaitSource};
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
    pub mode: AtomicU16,
    pub uid: AtomicU32,
    pub gid: AtomicU32,
    pub cuid: u32,
    pub cgid: u32,
    pub destroyed: AtomicBool,
    pub payload: SpinMutex<Option<PayloadCap<SemArrayPayload>>>,
}

impl SemArrayIdentity {
    pub fn perm(&self) -> IpcPerm {
        IpcPerm::new(self.mode.load(Ordering::Relaxed))
    }

    pub fn uid(&self) -> u32 {
        self.uid.load(Ordering::Relaxed)
    }

    pub fn gid(&self) -> u32 {
        self.gid.load(Ordering::Relaxed)
    }

    pub fn key_raw(&self) -> i32 {
        self.key.map(|key| key.0 as i32).unwrap_or(0)
    }
}

/// A single semaphore value within an array.
#[derive(Clone, Copy, Debug, Default)]
pub struct SemValue {
    pub val: i16,
    pub last_pid: u32,
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
    pub const SEM_UNDO: i16 = 0x1000;
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
    pub changed_source_id: u64,
    pub changed_source: alloc::sync::Arc<WaitSource>,
}

impl SemArrayPayload {
    /// Endpoint fired when any semaphore value in the array changes.
    pub fn changed_endpoint(&self) -> &alloc::sync::Arc<WaitSource> {
        &self.changed_source
    }
}

impl Drop for SemArrayPayload {
    fn drop(&mut self) {
        crate::wait_source::release_wait_source(self.changed_source_id);
        wait_routing::unregister_source(self.changed_source_id);
    }
}

// ---------------------------------------------------------------------------
// Global sem registry
// ---------------------------------------------------------------------------

static SEM_TABLE: SpinMutex<BTreeMap<u32, Cap<SemArrayIdentity>>> = SpinMutex::new(BTreeMap::new());
static REMOVED_SEM_IDS: SpinMutex<Vec<u32>> = SpinMutex::new(Vec::new());
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

pub(crate) fn was_sem_removed(semid: u32) -> bool {
    REMOVED_SEM_IDS.lock().contains(&semid)
}

pub(crate) fn register_sem(
    key: Option<SysvKey>,
    cred: Cap<crate::cred::Cred>,
    nsems: u16,
    perm: IpcPerm,
    cuid: u32,
    cgid: u32,
) -> Result<Cap<SemArrayIdentity>, ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let semid = NEXT_SEMID.fetch_add(1, Ordering::Relaxed);

    let (changed_source_id, changed_source) =
        crate::ipc::sysv_sem::notification::new_changed_source();

    let identity = sign(SemArrayIdentity {
        key,
        semid,
        cred,
        nsems,
        mode: AtomicU16::new(perm.mode),
        uid: AtomicU32::new(cuid),
        gid: AtomicU32::new(cgid),
        cuid,
        cgid,
        destroyed: AtomicBool::new(false),
        payload: SpinMutex::new(None),
    })?;
    let payload = sign(SemArrayPayload {
        values: SpinMutex::new(alloc::vec![SemValue::default(); nsems as usize]),
        changed_seq: AtomicU64::new(0),
        changed_source_id,
        changed_source,
    })?;
    *identity.payload.lock() = Some(PayloadCap::from_cap(payload));
    SEM_TABLE.lock().insert(semid, identity.clone());
    Ok(identity)
}

pub(crate) fn withdraw_sem(semid: u32) -> Option<Cap<SemArrayIdentity>> {
    let removed = SEM_TABLE.lock().remove(&semid);
    if removed.is_some() {
        REMOVED_SEM_IDS.lock().push(semid);
    }
    removed
}

/// Iterate all live semaphore arrays (for /proc/sysvipc/sem projection).
pub(crate) fn all_sem_arrays() -> Vec<Cap<SemArrayIdentity>> {
    SEM_TABLE.lock().values().cloned().collect()
}
