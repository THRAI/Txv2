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
    /// Day-1: stores semid values. The authoritative `Cap<SemArrayIdentity>`
    /// registry lives in `crate::ipc::sysv_sem`. When `AllocIndex` lands,
    /// this table becomes `IndexTable<SysvKey, Cap<SemArrayIdentity>>`.
    pub sysv_sem: SpinMutex<BTreeMap<SysvKey, u32>>,
    /// SysV shared memory segments, keyed by `key_t`.
    pub sysv_shm: SpinMutex<BTreeMap<SysvKey, u32>>,
    /// SysV message queues, keyed by `key_t`.
    pub sysv_msg: SpinMutex<BTreeMap<SysvKey, u32>>,
    /// POSIX message queues, keyed by path name.
    pub posix_mq: SpinMutex<BTreeMap<PosixMqName, u32>>,
    /// Per-namespace tunable limits.
    pub limits: SpinMutex<IpcLimits>,
    /// Monotonic counter for `IPC_PRIVATE` id generation (shared across
    /// all three SysV kinds — Linux uses separate idr per kind but a
    /// shared `ipc_ids` allocator). Day-1 single atomic counter.
    next_private_id: AtomicU32,
}

impl IpcNamespace {
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
        Self {
            sysv_sem: SpinMutex::new(BTreeMap::new()),
            sysv_shm: SpinMutex::new(BTreeMap::new()),
            sysv_msg: SpinMutex::new(BTreeMap::new()),
            posix_mq: SpinMutex::new(BTreeMap::new()),
            limits: SpinMutex::new(IpcLimits::default()),
            next_private_id: AtomicU32::new(0),
        }
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

// Mount namespace (`mnt_ns`) is deferred. The `MountNamespace` type
// already exists in `crate::mount`, but it is not yet wired into the
// process model at bootstrap time (requires a root `MountIdentity` +
// `MountPayload` + `FsOps`). When the mount namespace is bootstrapped
// before `bootstrap_init_process`, add it to NsProxy and thread
// through `sign_init_nsproxy`.
//
// pub use crate::mount::MountNamespace;

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
    // TODO(txdoc:NAMESPACE-VIEW-CORE-PLACEMENT-1): add mnt_ns when
    // mount namespace bootstrap is wired to the process model.
    // pub mnt_ns: Cap<MountNamespace>,
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
/// `mnt_ns` is deferred — MountNamespace requires a root
/// `MountIdentity` + `MountPayload` + `FsOps` that aren't available
/// at bootstrap time. When the mount namespace is bootstrapped before
/// the init process, thread it through this function.
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
        // mnt_ns: deferred — TODO(txdoc:NAMESPACE-VIEW-CORE-PLACEMENT-1)
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
/// When `CLONE_NEWIPC` / `CLONE_NEWPID` / etc. land, the clone
/// path will call `sign_init_nsproxy` for the subset of namespaces
/// being unshared.
pub fn clone_nsproxy(nsproxy: &Cap<NsProxy>) -> Cap<NsProxy> {
    nsproxy.clone()
}
