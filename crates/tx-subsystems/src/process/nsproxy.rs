//! Namespace proxy bundle — the per-process namespace view controller.
//!
//! Per `NAMESPACE_VIEW_v1.md` §1: NsProxy is an immutable bundle of
//! namespace references placed on `ProcessPayload`. Clone, unshare,
//! and setns publish a new bundle (or reuse an existing compatible
//! one). NsProxy does not own the resources exposed through its
//! namespaces — it is a lens, not a registry.
//!
//! **Day-1 single-namespace.** All fields point at the init namespace
//! cap. Per-namespace isolation (CLONE_NEWPID, CLONE_NEWIPC, ...)
//! arrives with the PidName / AllocIndex implementation. The
//! `IpcNamespace` tables are populated on-demand: the shm/sem/msg
//! registries are empty until the first `*get` call creates an entry.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::ipc::posix_mq::structure::PosixMqIdentity;
use crate::ipc::sysv_msg::structure::MsgQueueIdentity;
use crate::ipc::sysv_sem::structure::SemArrayIdentity;
use crate::ipc::sysv_shm::structure::ShmSegmentIdentity;
use crate::mount::MountNamespace;
use crate::process::adapter::step_engine::{sign, Cap, SpinMutex, Zone, ZoneAllocated, ZoneError};

// ---------------------------------------------------------------------------
// SysV IPC key types
// ---------------------------------------------------------------------------

/// System V IPC key — the `key_t` parameter to `*get` calls.
/// `IPC_PRIVATE` is represented as `None`; positive keys are `Some(key)`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SysvKey(pub u32);

impl SysvKey {
    pub const fn new(key: u32) -> Self {
        Self(key)
    }
}

/// POSIX message queue name — a null-terminated path like `/myqueue`.
/// Interned as a byte vector; comparisons are byte-for-byte per POSIX
/// semantics (`/`-prefixed, no trailing `/`, no `//`).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PosixMqName(pub alloc::vec::Vec<u8>);

impl PosixMqName {
    pub fn new(name: &[u8]) -> Self {
        Self(name.to_vec())
    }
}

// ---------------------------------------------------------------------------
// IPC limits — per-namespace tunables
// ---------------------------------------------------------------------------

/// Per-IpcNamespace resource limits. All fields default to Linux's
/// canonical `/proc/sys/kernel/sem` / `shmmax` / ... values, scoped
/// to the namespace so `CLONE_NEWIPC` inherits a copy.
#[derive(Clone, Copy, Debug)]
pub struct IpcLimits {
    /// Max number of semaphore arrays (SEMMNI). Linux default: 32000.
    pub semmni: u32,
    /// Max total semaphores system-wide (SEMMNS). Linux default: 1024000000.
    pub semmns: u32,
    /// Max semaphores per array (SEMMSL). Linux default: 32000.
    pub semmsl: u32,
    /// Max operations per semop call (SEMOPM). Linux default: 500.
    pub semopm: u32,
    /// Max number of shared memory segments (SHMMNI). Linux default: 4096.
    pub shmmni: u32,
    /// Max shared memory segment size in bytes (SHMMAX). Linux default: u64::MAX.
    pub shmmax: u64,
    /// Min shared memory segment size in bytes (SHMMIN). Linux default: 1.
    pub shmmin: u64,
    /// Max number of message queues (MSGMNI). Linux default: 32000.
    pub msgmni: u32,
    /// Max message size in bytes (MSGMAX). Linux default: 8192.
    pub msgmax: u32,
    /// Max total bytes in a message queue (MSGMNB). Linux default: 16384.
    pub msgmnb: u32,
    /// Max number of POSIX message queues. Linux default: see
    /// `/proc/sys/fs/mqueue/queues_max` (default 256).
    pub mq_queues_max: u32,
    /// Max message size for POSIX mq (mq_msgsize_max). Linux default: 8192.
    pub mq_msgsize_max: u32,
    /// Max messages per POSIX mq (mq_maxmsg). Linux default: 10.
    pub mq_maxmsg: u32,
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            semmni: 32000,
            semmns: 1024000000,
            semmsl: 32000,
            semopm: 500,
            shmmni: 4096,
            shmmax: u64::MAX,
            shmmin: 1,
            msgmni: 32000,
            msgmax: 8192,
            msgmnb: 16384,
            mq_queues_max: 256,
            mq_msgsize_max: 8192,
            mq_maxmsg: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// IpcNamespace
// ---------------------------------------------------------------------------

/// IPC namespace — the SysV and POSIX IPC object registry.
///
/// Per `08_SYSV_IPC_v1.md` §2: `IpcNamespace` holds key→identity index
/// tables for SysV semaphores, shared memory, and message queues, plus
/// a name→identity table for POSIX message queues. POSIX shared memory
/// (`shm_open`) resolves through the mount namespace's `/dev/shm` tmpfs.
///
/// Tables are `SpinMutex<BTreeMap>` — the same shape as the existing
/// `MountNamespace.mounts` table. When `IndexTable` / `AllocIndex`
/// substrate lands (per `NAMESPACE_VIEW_v1.md` §3), these maps are
/// replaced with namespace-number `IndexTable<SysvKey, Cap<...>>`.
pub struct IpcNamespace {
    /// SysV semaphore arrays, keyed by `key_t` (or IPC_PRIVATE id).
    pub sysv_sem: SpinMutex<BTreeMap<SysvKey, Cap<SemArrayIdentity>>>,
    /// SysV shared memory segments, keyed by `key_t`.
    pub sysv_shm: SpinMutex<BTreeMap<SysvKey, Cap<ShmSegmentIdentity>>>,
    /// SysV message queues, keyed by `key_t`.
    pub sysv_msg: SpinMutex<BTreeMap<SysvKey, Cap<MsgQueueIdentity>>>,
    /// POSIX message queues, keyed by path name.
    pub posix_mq: SpinMutex<BTreeMap<PosixMqName, Cap<PosixMqIdentity>>>,
    /// Per-namespace tunable limits.
    pub limits: SpinMutex<IpcLimits>,
    /// Monotonic counter for `IPC_PRIVATE` id generation (shared across
    /// all three SysV kinds — Linux uses separate idr per kind but a
    /// shared `ipc_ids` allocator). Day-1 single atomic counter.
    next_private_id: AtomicU32,
}

impl IpcNamespace {
    pub fn with_limits(limits: IpcLimits) -> Self {
        Self {
            sysv_sem: SpinMutex::new(BTreeMap::new()),
            sysv_shm: SpinMutex::new(BTreeMap::new()),
            sysv_msg: SpinMutex::new(BTreeMap::new()),
            posix_mq: SpinMutex::new(BTreeMap::new()),
            limits: SpinMutex::new(limits),
            next_private_id: AtomicU32::new(0),
        }
    }

    /// Allocate a fresh `IPC_PRIVATE` id. Returns a `SysvKey` with
    /// bit 31 set (negative `key_t` in Linux userspace), matching
    /// the convention that `IPC_PRIVATE`-allocated keys don't collide
    /// with user-supplied positive keys.
    pub fn alloc_private_key(&self) -> SysvKey {
        let id = self.next_private_id.fetch_add(1, Ordering::Relaxed);
        SysvKey(id | 0x8000_0000)
    }
}

impl Default for IpcNamespace {
    fn default() -> Self {
        Self::with_limits(IpcLimits::default())
    }
}

// ---------------------------------------------------------------------------
// Placeholder namespace types
//
// Day-1 single-namespace stubs. Each will become a full namespace type
// when its corresponding subsystem lands. For now they are zero-sized
// zone-allocated tokens so NsProxy can carry a Cap to each.
// ---------------------------------------------------------------------------

/// Pid namespace stub. Real impl arrives with `PidName` / `AllocIndex`
/// per `NAMESPACE_VIEW_v1.md` §3.
pub struct PidNamespaceStub;

/// User namespace stub. Real impl arrives with the user-ns / capability
/// lens per `NAMESPACE_VIEW_v1.md` §7.
pub struct UserNamespaceStub;

/// Cgroup namespace stub. Real impl arrives with cgroup-v2 subsystem.
pub struct CgroupNamespaceStub;

/// UTS namespace stub. Real impl arrives with `sethostname` / `uname`.
pub struct UtsNamespaceStub;

/// Network namespace stub. Real impl arrives with socket subsystem.
pub struct NetNamespaceStub;

/// Time namespace stub. Real impl arrives with `clock_settime` per-ns offsets.
pub struct TimeNamespaceStub;

// ---------------------------------------------------------------------------
// NsProxy
// ---------------------------------------------------------------------------

/// Immutable namespace-proxy bundle. Placed on `ProcessPayload.nsproxy`
/// per `NAMESPACE_VIEW_v1.md` §1.
///
/// Clone / unshare / setns operations publish a new `NsProxy` (or reuse
/// an existing compatible one). The bundle does not own the resources
/// exposed through its namespaces — invariant NSPROXY-1.
pub struct NsProxy {
    pub pid_ns: Cap<PidNamespaceStub>,
    pub pid_for_children: Cap<PidNamespaceStub>,
    /// Mount namespace view. `None` only during early bootstrap before the
    /// root mount exists; boot and tests publish a replacement NsProxy once
    /// they have a concrete `MountNamespace` cap.
    pub mnt_ns: Option<Cap<MountNamespace>>,
    pub user_ns: Cap<UserNamespaceStub>,
    pub cgroup_ns: Cap<CgroupNamespaceStub>,
    pub uts_ns: Cap<UtsNamespaceStub>,
    pub ipc_ns: Cap<IpcNamespace>,
    pub net_ns: Cap<NetNamespaceStub>,
    pub time_ns: Cap<TimeNamespaceStub>,
}

// ---------------------------------------------------------------------------
// Zone registration
// ---------------------------------------------------------------------------

static NSPROXY_ZONE: Zone<NsProxy> = Zone::const_new();
static IPC_NAMESPACE_ZONE: Zone<IpcNamespace> = Zone::const_new();

static PID_NS_STUB_ZONE: Zone<PidNamespaceStub> = Zone::const_new();
static USER_NS_STUB_ZONE: Zone<UserNamespaceStub> = Zone::const_new();
static CGROUP_NS_STUB_ZONE: Zone<CgroupNamespaceStub> = Zone::const_new();
static UTS_NS_STUB_ZONE: Zone<UtsNamespaceStub> = Zone::const_new();
static NET_NS_STUB_ZONE: Zone<NetNamespaceStub> = Zone::const_new();
static TIME_NS_STUB_ZONE: Zone<TimeNamespaceStub> = Zone::const_new();

unsafe impl ZoneAllocated for NsProxy {
    fn zone() -> &'static Zone<Self> {
        &NSPROXY_ZONE
    }
}

unsafe impl ZoneAllocated for IpcNamespace {
    fn zone() -> &'static Zone<Self> {
        &IPC_NAMESPACE_ZONE
    }
}

macro_rules! stub_zone {
    ($t:ty, $z:ident) => {
        unsafe impl ZoneAllocated for $t {
            fn zone() -> &'static Zone<Self> {
                &$z
            }
        }
    };
}

stub_zone!(PidNamespaceStub, PID_NS_STUB_ZONE);
stub_zone!(UserNamespaceStub, USER_NS_STUB_ZONE);
stub_zone!(CgroupNamespaceStub, CGROUP_NS_STUB_ZONE);
stub_zone!(UtsNamespaceStub, UTS_NS_STUB_ZONE);
stub_zone!(NetNamespaceStub, NET_NS_STUB_ZONE);
stub_zone!(TimeNamespaceStub, TIME_NS_STUB_ZONE);

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<NsProxy>()?;
    register_zone_for::<IpcNamespace>()?;
    register_zone_for::<PidNamespaceStub>()?;
    register_zone_for::<UserNamespaceStub>()?;
    register_zone_for::<CgroupNamespaceStub>()?;
    register_zone_for::<UtsNamespaceStub>()?;
    register_zone_for::<NetNamespaceStub>()?;
    register_zone_for::<TimeNamespaceStub>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bootstrap — create the init NsProxy
// ---------------------------------------------------------------------------

/// Create the root (init) `NsProxy` for `bootstrap_init_process`.
/// All namespace caps point at freshly allocated stub singletons.
///
/// `mnt_ns` starts as `None` because MountNamespace requires a root
/// `MountIdentity` + `MountPayload` + `FsOps` that aren't available
/// at process-bootstrap time in the current boot order. Boot publishes
/// a replacement bundle with a concrete mount namespace after rootfs
/// mount creation.
///
/// Panics on zone allocation failure — bootstrap is infallible in
/// practice; if zone registration failed, the kernel cannot start.
pub fn sign_init_nsproxy() -> Result<Cap<NsProxy>, ZoneError> {
    let pid_ns = sign(PidNamespaceStub)?;
    let pid_for_children = sign(PidNamespaceStub)?;
    let user_ns = sign(UserNamespaceStub)?;
    let cgroup_ns = sign(CgroupNamespaceStub)?;
    let uts_ns = sign(UtsNamespaceStub)?;
    let ipc_ns = sign(IpcNamespace::default())?;
    let net_ns = sign(NetNamespaceStub)?;
    let time_ns = sign(TimeNamespaceStub)?;

    sign(NsProxy {
        pid_ns,
        pid_for_children,
        mnt_ns: None,
        user_ns,
        cgroup_ns,
        uts_ns,
        ipc_ns,
        net_ns,
        time_ns,
    })
}

/// Clone the init `NsProxy` for `step_fork`. Since the bundle is
/// immutable, cloning just clones the `Cap` (refcount bump).
pub fn clone_nsproxy(nsproxy: &Cap<NsProxy>) -> Cap<NsProxy> {
    nsproxy.clone()
}

/// Build the namespace bundle published by fork/clone.
///
/// Default fork inherits the parent's immutable bundle. `CLONE_NEWIPC`
/// publishes a replacement bundle with all non-IPC namespace caps shared
/// and a fresh IPC namespace whose limits are copied from the parent.
pub fn clone_nsproxy_for_fork(
    nsproxy: &Cap<NsProxy>,
    clone_newipc: bool,
) -> Result<Cap<NsProxy>, ZoneError> {
    if !clone_newipc {
        return Ok(clone_nsproxy(nsproxy));
    }

    let ipc_limits = *nsproxy.ipc_ns.limits.lock();
    let ipc_ns = sign(IpcNamespace::with_limits(ipc_limits))?;
    sign(NsProxy {
        pid_ns: nsproxy.pid_ns.clone(),
        pid_for_children: nsproxy.pid_for_children.clone(),
        mnt_ns: nsproxy.mnt_ns.clone(),
        user_ns: nsproxy.user_ns.clone(),
        cgroup_ns: nsproxy.cgroup_ns.clone(),
        uts_ns: nsproxy.uts_ns.clone(),
        ipc_ns,
        net_ns: nsproxy.net_ns.clone(),
        time_ns: nsproxy.time_ns.clone(),
    })
}

/// Return a replacement namespace bundle with `mnt_ns` installed.
///
/// NsProxy is immutable after publication, so mount namespace setup follows
/// the same rule as unshare/setns: build a new bundle, then atomically publish
/// it on the process payload.
pub fn clone_nsproxy_with_mount_namespace(
    nsproxy: &Cap<NsProxy>,
    mnt_ns: Cap<MountNamespace>,
) -> Result<Cap<NsProxy>, ZoneError> {
    sign(NsProxy {
        pid_ns: nsproxy.pid_ns.clone(),
        pid_for_children: nsproxy.pid_for_children.clone(),
        mnt_ns: Some(mnt_ns),
        user_ns: nsproxy.user_ns.clone(),
        cgroup_ns: nsproxy.cgroup_ns.clone(),
        uts_ns: nsproxy.uts_ns.clone(),
        ipc_ns: nsproxy.ipc_ns.clone(),
        net_ns: nsproxy.net_ns.clone(),
        time_ns: nsproxy.time_ns.clone(),
    })
}
