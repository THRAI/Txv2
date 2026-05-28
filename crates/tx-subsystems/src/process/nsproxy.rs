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
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::cred::{Capability, Cred};
use crate::execution::Errno;
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

/// A single uid/gid mapping row in a Linux user namespace map file.
///
/// Phase A only needs a real namespace identity so `unshare(CLONE_NEWUSER)`
/// can publish a distinct cap. The bounded map storage lands now because the
/// next procfs phase will write `/proc/self/{uid_map,gid_map}` into these rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserIdMapEntry {
    pub inside: u32,
    pub outside: u32,
    pub length: u32,
}

/// Linux `/proc/<pid>/setgroups` policy for a user namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetgroupsPolicy {
    Allow,
    Deny,
}

/// Minimal user namespace model.
///
/// This is deliberately smaller than Linux's full credential namespace. It is
/// enough to give each `CLONE_NEWUSER` caller a real namespace identity and a
/// home for the uid/gid maps LTP writes through procfs in the next phase.
pub struct UserNamespace {
    pub parent: Option<Cap<UserNamespace>>,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub uid_map: SpinMutex<Vec<UserIdMapEntry>>,
    pub gid_map: SpinMutex<Vec<UserIdMapEntry>>,
    uid_map_written: SpinMutex<bool>,
    gid_map_written: SpinMutex<bool>,
    pub setgroups: SpinMutex<SetgroupsPolicy>,
}

impl UserNamespace {
    pub fn init() -> Self {
        Self {
            parent: None,
            owner_uid: 0,
            owner_gid: 0,
            uid_map: SpinMutex::new(vec![UserIdMapEntry {
                inside: 0,
                outside: 0,
                length: u32::MAX,
            }]),
            gid_map: SpinMutex::new(vec![UserIdMapEntry {
                inside: 0,
                outside: 0,
                length: u32::MAX,
            }]),
            uid_map_written: SpinMutex::new(true),
            gid_map_written: SpinMutex::new(true),
            setgroups: SpinMutex::new(SetgroupsPolicy::Allow),
        }
    }

    pub fn child(parent: Cap<UserNamespace>, owner_uid: u32, owner_gid: u32) -> Self {
        let inherited_setgroups = *parent.setgroups.lock();
        Self {
            parent: Some(parent),
            owner_uid,
            owner_gid,
            uid_map: SpinMutex::new(Vec::new()),
            gid_map: SpinMutex::new(Vec::new()),
            uid_map_written: SpinMutex::new(false),
            gid_map_written: SpinMutex::new(false),
            setgroups: SpinMutex::new(inherited_setgroups),
        }
    }

    pub fn maps_uid(&self, uid: u32) -> bool {
        self.uid_map.lock().iter().any(|entry| entry.contains(uid))
    }

    pub fn maps_gid(&self, gid: u32) -> bool {
        self.gid_map.lock().iter().any(|entry| entry.contains(gid))
    }

    pub fn uid_map_snapshot(&self) -> Vec<UserIdMapEntry> {
        self.uid_map.lock().clone()
    }

    pub fn gid_map_snapshot(&self) -> Vec<UserIdMapEntry> {
        self.gid_map.lock().clone()
    }

    pub fn setgroups_policy(&self) -> SetgroupsPolicy {
        *self.setgroups.lock()
    }

    pub fn uid_map_written(&self) -> bool {
        *self.uid_map_written.lock()
    }

    pub fn gid_map_written(&self) -> bool {
        *self.gid_map_written.lock()
    }
}

impl UserIdMapEntry {
    pub fn contains(self, id: u32) -> bool {
        let end = self.inside.saturating_add(self.length);
        id >= self.inside && id < end
    }
}

/// Linux-style namespace-relative capability check.
///
/// The initial namespace uses the credential's ordinary effective-capability
/// test. A process that is a member of a child user namespace has full
/// capabilities in that namespace only; a parent-namespace process whose euid
/// is the recorded namespace owner also has capabilities in that child
/// namespace, matching Linux's owner rule for procfs map setup.
pub fn has_capability_in_user_namespace(
    cred: Cred,
    subject_user_ns: &Cap<UserNamespace>,
    target_user_ns: &Cap<UserNamespace>,
    cap: Capability,
) -> bool {
    if subject_user_ns.key() == target_user_ns.key() {
        if target_user_ns.parent.is_none() {
            return cred.is_privileged_for(cap);
        }
        return true;
    }

    if let Some(parent) = target_user_ns.parent.as_ref() {
        if parent.key() == subject_user_ns.key() && cred.euid.raw() == target_user_ns.owner_uid {
            return true;
        }
    }

    false
}

const USERNS_MAP_MAX_BYTES: usize = 4096;
const USERNS_MAP_MAX_ENTRIES: usize = 340;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserNsMapKind {
    Uid,
    Gid,
}

pub fn write_user_namespace_setgroups(
    target_user_ns: &Cap<UserNamespace>,
    offset: u64,
    bytes: &[u8],
) -> Result<(), Errno> {
    if offset != 0 {
        return Err(Errno::EINVAL);
    }
    if bytes.len() >= USERNS_MAP_MAX_BYTES {
        return Err(Errno::EINVAL);
    }

    let trimmed = trim_ascii_space(bytes);
    match trimmed {
        b"deny" => {
            if target_user_ns.gid_map_written() {
                return Err(Errno::EPERM);
            }
            *target_user_ns.setgroups.lock() = SetgroupsPolicy::Deny;
            Ok(())
        }
        b"allow" => {
            if target_user_ns.gid_map_written()
                || target_user_ns.setgroups_policy() == SetgroupsPolicy::Deny
            {
                return Err(Errno::EPERM);
            }
            *target_user_ns.setgroups.lock() = SetgroupsPolicy::Allow;
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

pub fn write_user_namespace_id_map(
    target_user_ns: &Cap<UserNamespace>,
    writer_cred: Cred,
    writer_user_ns: &Cap<UserNamespace>,
    kind: UserNsMapKind,
    offset: u64,
    bytes: &[u8],
) -> Result<(), Errno> {
    if offset != 0 {
        return Err(Errno::EINVAL);
    }

    let entries = parse_userns_id_map(bytes)?;
    let cap = match kind {
        UserNsMapKind::Uid => Capability::SETUID,
        UserNsMapKind::Gid => Capability::SETGID,
    };
    if !has_capability_in_user_namespace(writer_cred, writer_user_ns, target_user_ns, cap) {
        return Err(Errno::EPERM);
    }

    validate_parent_map_authority(target_user_ns, writer_cred, writer_user_ns, kind, &entries)?;

    match kind {
        UserNsMapKind::Uid => {
            let mut written = target_user_ns.uid_map_written.lock();
            if *written {
                return Err(Errno::EPERM);
            }
            *target_user_ns.uid_map.lock() = entries;
            *written = true;
        }
        UserNsMapKind::Gid => {
            let mut written = target_user_ns.gid_map_written.lock();
            if *written {
                return Err(Errno::EPERM);
            }
            *target_user_ns.gid_map.lock() = entries;
            *written = true;
        }
    }

    Ok(())
}

fn validate_parent_map_authority(
    target_user_ns: &Cap<UserNamespace>,
    writer_cred: Cred,
    writer_user_ns: &Cap<UserNamespace>,
    kind: UserNsMapKind,
    entries: &[UserIdMapEntry],
) -> Result<(), Errno> {
    let Some(parent) = target_user_ns.parent.as_ref() else {
        return Err(Errno::EPERM);
    };

    let parent_maps_all = entries
        .iter()
        .all(|entry| id_range_mapped(parent, kind, entry.outside, entry.length));
    if !parent_maps_all {
        return Err(Errno::EPERM);
    }

    let cap = match kind {
        UserNsMapKind::Uid => Capability::SETUID,
        UserNsMapKind::Gid => Capability::SETGID,
    };
    if has_capability_in_user_namespace(writer_cred, writer_user_ns, parent, cap) {
        return Ok(());
    }

    let owner_id = match kind {
        UserNsMapKind::Uid => target_user_ns.owner_uid,
        UserNsMapKind::Gid => target_user_ns.owner_gid,
    };
    let unprivileged_single_owner_map =
        matches!(entries, [entry] if entry.length == 1 && entry.outside == owner_id);
    if !unprivileged_single_owner_map {
        return Err(Errno::EPERM);
    }

    if kind == UserNsMapKind::Gid && target_user_ns.setgroups_policy() != SetgroupsPolicy::Deny {
        return Err(Errno::EPERM);
    }

    Ok(())
}

fn id_range_mapped(
    user_ns: &Cap<UserNamespace>,
    kind: UserNsMapKind,
    outside_start: u32,
    length: u32,
) -> bool {
    let Some(last) = outside_start.checked_add(length - 1) else {
        return false;
    };
    let maps_id = |id| match kind {
        UserNsMapKind::Uid => user_ns.maps_uid(id),
        UserNsMapKind::Gid => user_ns.maps_gid(id),
    };
    maps_id(outside_start) && maps_id(last)
}

fn parse_userns_id_map(bytes: &[u8]) -> Result<Vec<UserIdMapEntry>, Errno> {
    if bytes.is_empty() || bytes.len() >= USERNS_MAP_MAX_BYTES {
        return Err(Errno::EINVAL);
    }
    let text = core::str::from_utf8(bytes).map_err(|_| Errno::EINVAL)?;
    let text = text.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r');
    if text.is_empty() {
        return Err(Errno::EINVAL);
    }

    let mut entries: Vec<UserIdMapEntry> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let inside = parse_map_u32(fields.next())?;
        let outside = parse_map_u32(fields.next())?;
        let length = parse_map_u32(fields.next())?;
        if fields.next().is_some() || length == 0 {
            return Err(Errno::EINVAL);
        }
        let entry = UserIdMapEntry {
            inside,
            outside,
            length,
        };
        validate_map_entry_range(entry)?;
        for prev in &entries {
            if ranges_overlap(prev.inside, prev.length, entry.inside, entry.length)
                || ranges_overlap(prev.outside, prev.length, entry.outside, entry.length)
            {
                return Err(Errno::EINVAL);
            }
        }
        entries.push(entry);
        if entries.len() > USERNS_MAP_MAX_ENTRIES {
            return Err(Errno::EINVAL);
        }
    }

    if entries.is_empty() {
        Err(Errno::EINVAL)
    } else {
        Ok(entries)
    }
}

fn parse_map_u32(field: Option<&str>) -> Result<u32, Errno> {
    let Some(field) = field else {
        return Err(Errno::EINVAL);
    };
    field.parse::<u32>().map_err(|_| Errno::EINVAL)
}

fn validate_map_entry_range(entry: UserIdMapEntry) -> Result<(), Errno> {
    if entry.inside.checked_add(entry.length - 1).is_none()
        || entry.outside.checked_add(entry.length - 1).is_none()
    {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn ranges_overlap(a_start: u32, a_len: u32, b_start: u32, b_len: u32) -> bool {
    let a_end = a_start as u64 + a_len as u64;
    let b_end = b_start as u64 + b_len as u64;
    (a_start as u64) < b_end && (b_start as u64) < a_end
}

fn trim_ascii_space(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

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
    pub user_ns: Cap<UserNamespace>,
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
static USER_NAMESPACE_ZONE: Zone<UserNamespace> = Zone::const_new();
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
unsafe impl ZoneAllocated for UserNamespace {
    fn zone() -> &'static Zone<Self> {
        &USER_NAMESPACE_ZONE
    }
}

stub_zone!(CgroupNamespaceStub, CGROUP_NS_STUB_ZONE);
stub_zone!(UtsNamespaceStub, UTS_NS_STUB_ZONE);
stub_zone!(NetNamespaceStub, NET_NS_STUB_ZONE);
stub_zone!(TimeNamespaceStub, TIME_NS_STUB_ZONE);

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<NsProxy>()?;
    register_zone_for::<IpcNamespace>()?;
    register_zone_for::<PidNamespaceStub>()?;
    register_zone_for::<UserNamespace>()?;
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
    let user_ns = sign(UserNamespace::init())?;
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

/// Return a replacement namespace bundle with a fresh child user namespace.
pub fn clone_nsproxy_with_user_namespace(
    nsproxy: &Cap<NsProxy>,
    owner_uid: u32,
    owner_gid: u32,
) -> Result<Cap<NsProxy>, ZoneError> {
    let user_ns = sign(UserNamespace::child(
        nsproxy.user_ns.clone(),
        owner_uid,
        owner_gid,
    ))?;

    sign(NsProxy {
        pid_ns: nsproxy.pid_ns.clone(),
        pid_for_children: nsproxy.pid_for_children.clone(),
        mnt_ns: nsproxy.mnt_ns.clone(),
        user_ns,
        cgroup_ns: nsproxy.cgroup_ns.clone(),
        uts_ns: nsproxy.uts_ns.clone(),
        ipc_ns: nsproxy.ipc_ns.clone(),
        net_ns: nsproxy.net_ns.clone(),
        time_ns: nsproxy.time_ns.clone(),
    })
}
