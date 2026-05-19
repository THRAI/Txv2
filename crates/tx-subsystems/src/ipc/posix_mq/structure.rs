//! POSIX message queue — identity, instance, and name registry.
//!
//! `PosixMqIdentity`: name → SysV MsgQueue mapping.
//! `PosixMqInstance`: fd-shaped open-instance with notify state.
//!
//! Day-1 single-namespace: a global `MQ_NAME_TABLE` maps name → msqid.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::{Cap, SpinMutex, Zone, ZoneAllocated, ZoneError};
use crate::process::nsproxy::PosixMqName;

// ---------------------------------------------------------------------------
// PosixMqIdentity / PosixMqInstance
// ---------------------------------------------------------------------------

/// POSIX message queue identity — the named queue.
pub struct PosixMqIdentity {
    pub name: PosixMqName,
    pub cred: Cap<crate::cred::Cred>,
    pub perm: IpcPerm,
    pub msqid: u32,
}

/// POSIX message queue open-instance — an fd-shaped wrapper.
pub struct PosixMqInstance {
    pub msqid: u32,
    pub flags: i32,
    pub notify_signal: SpinMutex<Option<crate::signal::Signum>>,
}

// ---------------------------------------------------------------------------
// Global name registry
// ---------------------------------------------------------------------------

static MQ_NAME_TABLE: SpinMutex<BTreeMap<PosixMqName, Cap<PosixMqIdentity>>> =
    SpinMutex::new(BTreeMap::new());

static MQ_INSTANCE_TABLE: SpinMutex<BTreeMap<u32, Cap<PosixMqInstance>>> =
    SpinMutex::new(BTreeMap::new());

static NEXT_MQFD: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Zone registration
// ---------------------------------------------------------------------------

static MQ_IDENTITY_ZONE: Zone<PosixMqIdentity> = Zone::const_new();
static MQ_INSTANCE_ZONE: Zone<PosixMqInstance> = Zone::const_new();

unsafe impl ZoneAllocated for PosixMqIdentity {
    fn zone() -> &'static Zone<Self> {
        &MQ_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for PosixMqInstance {
    fn zone() -> &'static Zone<Self> {
        &MQ_INSTANCE_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<PosixMqIdentity>()?;
    register_zone_for::<PosixMqInstance>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public registry accessors
// ---------------------------------------------------------------------------

pub(crate) fn lookup_mq_by_name(name: &PosixMqName) -> Option<Cap<PosixMqIdentity>> {
    MQ_NAME_TABLE.lock().get(name).cloned()
}

pub(crate) fn register_mq(
    name: PosixMqName,
    cred: Cap<crate::cred::Cred>,
    perm: IpcPerm,
    msqid: u32,
) -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let identity = sign(PosixMqIdentity {
        name: name.clone(),
        cred,
        perm,
        msqid,
    })?;
    MQ_NAME_TABLE.lock().insert(name, identity);
    Ok(())
}

pub(crate) fn unlink_mq(name: &PosixMqName) -> bool {
    MQ_NAME_TABLE.lock().remove(name).is_some()
}

pub(crate) fn alloc_mqfd(msqid: u32, flags: i32) -> Result<u32, ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let fd = NEXT_MQFD.fetch_add(1, Ordering::Relaxed);
    let instance = sign(PosixMqInstance {
        msqid,
        flags,
        notify_signal: SpinMutex::new(None),
    })?;
    MQ_INSTANCE_TABLE.lock().insert(fd, instance);
    Ok(fd)
}

pub(crate) fn lookup_instance(mqfd: u32) -> Option<Cap<PosixMqInstance>> {
    MQ_INSTANCE_TABLE.lock().get(&mqfd).cloned()
}

pub(crate) fn close_instance(mqfd: u32) {
    MQ_INSTANCE_TABLE.lock().remove(&mqfd);
}
