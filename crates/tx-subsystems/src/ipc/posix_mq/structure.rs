//! POSIX message queue — identity, instance, and name registry.
//!
//! `PosixMqIdentity`: name → SysV MsgQueue mapping.
//! `PosixMqInstance`: fd-shaped open-instance with notify state.
//!
//! Day-1 single-namespace: a global `MQ_NAME_TABLE` maps name → msqid.

use alloc::collections::BTreeMap;

use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::{
    sign, Cap, SpinMutex, Weak, Zone, ZoneAllocated, ZoneError,
};
use crate::process::nsproxy::PosixMqName;
use crate::process::structure::ProcessIdentity;

// ---------------------------------------------------------------------------
// PosixMqIdentity / PosixMqInstance
// ---------------------------------------------------------------------------

/// POSIX message queue identity — the named queue.
pub struct PosixMqIdentity {
    pub name: PosixMqName,
    pub cred: Cap<crate::cred::Cred>,
    pub perm: IpcPerm,
    pub msqid: u32,
    pub maxmsg: i64,
    pub msgsize: i64,
    pub notify: SpinMutex<Option<MqNotification>>,
}

/// POSIX message queue open-instance — an fd-shaped wrapper.
pub struct PosixMqInstance {
    pub identity: Cap<PosixMqIdentity>,
    pub flags: SpinMutex<i64>,
}

#[derive(Clone, Copy, Debug)]
pub enum MqNotification {
    None,
    Signal {
        signum: crate::signal::Signum,
        owner: Weak<ProcessIdentity>,
    },
}

impl PosixMqInstance {
    pub fn msqid(&self) -> u32 {
        self.identity.msqid
    }

    pub fn maxmsg(&self) -> i64 {
        self.identity.maxmsg
    }

    pub fn msgsize(&self) -> i64 {
        self.identity.msgsize
    }

    pub fn flags(&self) -> i64 {
        *self.flags.lock()
    }

    pub fn set_flags(&self, flags: i64) {
        *self.flags.lock() = flags;
    }
}

// ---------------------------------------------------------------------------
// Global name registry
// ---------------------------------------------------------------------------

static MQ_NAME_TABLE: SpinMutex<BTreeMap<PosixMqName, Cap<PosixMqIdentity>>> =
    SpinMutex::new(BTreeMap::new());

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
    maxmsg: i64,
    msgsize: i64,
) -> Result<Cap<PosixMqIdentity>, ZoneError> {
    let identity = sign(PosixMqIdentity {
        name: name.clone(),
        cred,
        perm,
        msqid,
        maxmsg,
        msgsize,
        notify: SpinMutex::new(None),
    })?;
    MQ_NAME_TABLE.lock().insert(name, identity.clone());
    Ok(identity)
}

pub(crate) fn unlink_mq(name: &PosixMqName) -> bool {
    MQ_NAME_TABLE.lock().remove(name).is_some()
}

pub(crate) fn open_instance(
    identity: Cap<PosixMqIdentity>,
    flags: i64,
) -> Result<Cap<PosixMqInstance>, ZoneError> {
    sign(PosixMqInstance {
        identity,
        flags: SpinMutex::new(flags),
    })
}
