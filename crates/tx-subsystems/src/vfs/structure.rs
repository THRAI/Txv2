//! VFS structure: SSoT data types and zone-managed live nodes.
//!
//! This module hosts the value vocabulary the rest of the VFS surface
//! consumes — names, ids, metadata, directory cursors — plus the three
//! zone-allocated entities (`DEntry`, `RNode`, `OpenFile`) and their
//! constructors/accessors. Per `SUBSYSTEM_ANATOMY_v2_1` §structure,
//! mutation step bodies live in `execution.rs`; only constructors and
//! pure observation helpers belong here.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU64, Ordering};

use crate::vfs::adapter::step_engine::{
    self, Cap, PayloadCap, SpinMutex, Weak, Zone, ZoneAllocated, ZoneError,
};
use crate::vfs::adapter::wait_routing::{
    MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource,
};

use crate::aio::AioContext;
use crate::cred::{CapabilitySet, Cred, CredSnapshot};
use crate::device::{BlockDeviceRegistration, CharDeviceBinding};
use crate::epoll::Epoll;
use crate::eventfd::EventFd;
use crate::execution::Errno;
use crate::io_uring::IoUring;
use crate::ipc::posix_mq::structure::PosixMqInstance;
use crate::mount::{FsObjectDestroyToken, FsObjectPin, MountApiFile, MountIdentity, MountPayload};
use crate::net::{NetNamespacePayload, SocketIdentity};
use crate::page_backed::PageContainer;
use crate::process::{ProcessGroup, ProcessIdentity};
use crate::signalfd::SignalFd;
use crate::timerfd::TimerFd;
use crate::tty::execution::IoctlSideEffect;
use crate::tty::structure::TtyIdentity;
use crate::tty::structure::{Termios, Winsize};
use crate::userfaultfd::UserfaultFd;
use crate::vfs::notification;
pub use crate::vfs::notification::{VFS_READABLE, VFS_WRITABLE};

pub const VFS_NAME_MAX: usize = 255;

/// Per-RNode wait-source interest mask: bytes are available to read.
///
/// VFS owns per-inode read/write wait-source bits. The bit lives on VFS rather
/// than on a backing because the wake-publication shape is VFS-uniform: every
/// inode has read/write semantics with the same blocking-IO contract.
// === zone statics =====================================================

static DENTRY_ZONE: Zone<DEntry> = Zone::const_new();
static RNODE_ZONE: Zone<RNode> = Zone::const_new();
static OPEN_FILE_ZONE: Zone<OpenFile> = Zone::const_new();
static FSNOTIFY_INSTANCE_ZONE: Zone<FsNotifyInstance> = Zone::const_new();
static FLOCK_TABLE: SpinMutex<BTreeMap<FsObjectId, FlockRecord>> = SpinMutex::new(BTreeMap::new());

#[derive(Default)]
struct FlockRecord {
    exclusive_owner: Option<u64>,
    shared_owners: BTreeSet<u64>,
}

unsafe impl ZoneAllocated for DEntry {
    fn zone() -> &'static Zone<Self> {
        &DENTRY_ZONE
    }
}

unsafe impl ZoneAllocated for RNode {
    fn zone() -> &'static Zone<Self> {
        &RNODE_ZONE
    }
}

unsafe impl ZoneAllocated for OpenFile {
    fn zone() -> &'static Zone<Self> {
        &OPEN_FILE_ZONE
    }
}

unsafe impl ZoneAllocated for FsNotifyInstance {
    fn zone() -> &'static Zone<Self> {
        &FSNOTIFY_INSTANCE_ZONE
    }
}

// === namespace identifiers =============================================

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FsObjectId(u64);

impl FsObjectId {
    pub const ROOT: Self = Self(1);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Construct a persistent-filesystem object identity from its allocation
    /// slot and incarnation generation.
    ///
    /// The low 32 bits remain the on-disk inode number for backends such as
    /// ext4.  The high 32 bits distinguish successive occupants of the same
    /// inode bitmap slot, preventing a reclaimed inode from inheriting an old
    /// RNode, page cache, orphan record, or advisory lock.
    pub const fn from_inode_generation(inode: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | inode as u64)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn inode_number(self) -> u32 {
        self.0 as u32
    }

    pub const fn inode_generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Walker-side projection of a process's credential. Carries only the
/// fields VFS permission checks need: the **effective** uid/gid and
/// the effective capability set (`CAP_DAC_OVERRIDE` short-circuit).
///
/// `Credential::default()` is no longer "root"-equivalent: it produces
/// `{ uid: 0, gid: 0, effective_caps: CapabilitySet::EMPTY }`. The
/// uid happens to be 0 because that is the `Default::default()` for
/// `u32`, but with no capabilities the walker treats this as an
/// unprivileged caller (Wave 3, when the DAC predicate lands; today
/// the walker still allows everything, but the field is in place so
/// Wave 3 has a stable seam). Production paths that *do* want a
/// root-equivalent walker cred must call [`Credential::root`].
///
/// See `txdoc:VFS-CHECKS-PERMISSIONS-1` for the walker-side
/// permission contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Credential {
    pub uid: u32,
    pub gid: u32,
    /// Effective capability set. The walker consults
    /// `CAP_DAC_OVERRIDE` here (Wave 3) without re-locking the
    /// per-process `Cred`.
    pub effective_caps: CapabilitySet,
}

impl Credential {
    /// Root walker credential: uid 0, gid 0, all capabilities. Use in
    /// bootstrap paths where the caller is root by construction
    /// (e.g. `init`'s pre-userspace bring-up). Distinct from
    /// `Credential::default()`, which post-Wave-1 is no longer
    /// root-equivalent.
    pub const fn root() -> Self {
        Self {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        }
    }
}

/// Project a full `Cred` onto the walker-side `Credential`. Per the
/// POSIX path-resolution rule, DAC checks consult the **effective**
/// uid/gid (not the real uid/gid); see `man 2 path_resolution` and
/// `man 2 chmod`. The `permitted_caps` set is intentionally dropped:
/// it has no walker-side use today, and the bridge keeps the walker
/// type lean.
impl From<&Cred> for Credential {
    fn from(cred: &Cred) -> Self {
        Self {
            uid: cred.euid.raw(),
            gid: cred.egid.raw(),
            effective_caps: cred.effective_caps,
        }
    }
}

/// Project a syscall-entry [`CredSnapshot`] onto the walker-side
/// `Credential` directly, skipping the intermediate by-value `Cred`
/// copy. Same projection rule as `From<&Cred>` (DAC uses **euid** /
/// **egid**, drops `permitted_caps`), but call sites that hold a
/// `&CredSnapshot` from `SyscallCtx::cred_snapshot()` can land here in
/// one step.
impl From<&CredSnapshot> for Credential {
    fn from(snapshot: &CredSnapshot) -> Self {
        Self::from(snapshot.as_cred())
    }
}

// === inode metadata + POSIX mode constants ============================

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

// POSIX S_IFMT mode-bit constants. Mode carries the file kind in its upper
// nibble; `InodeMeta::kind()` derives `InodeKind` from these bits.
pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFIFO: u16 = 0o010000;
pub const S_IFSOCK: u16 = 0o140000;

// POSIX special-mode bits: setuid, setgid, sticky. Live above the
// standard `rwxrwxrwx` triplets but below `S_IFMT`. Used by the DAC +
// setuid slice's chmod/chown bookkeeping (e.g. `chown_inode` silently
// clears `S_ISUID`/`S_ISGID` for non-privileged callers).
pub const S_ISUID: u16 = 0o4000;
pub const S_ISGID: u16 = 0o2000;
pub const S_ISVTX: u16 = 0o1000;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i32,
}

impl Timespec {
    pub const EPOCH: Self = Self { sec: 0, nsec: 0 };

    pub const fn new(sec: i64, nsec: i32) -> Self {
        Self { sec, nsec }
    }
}

// Per `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md` §pub-types and
// `bringup_fs_specs_v_1` §load_inode_meta. `mode` carries S_IFMT bits;
// `kind()` derives `InodeKind` from them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeMeta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub nlinks: u32,
    pub blocks: u64,
    pub flags: u32,
}

impl InodeMeta {
    /// Construct a fresh inode meta. `kind` is asserted into the mode's
    /// S_IFMT bits if not already present; the mode is otherwise preserved.
    pub const fn new(kind: InodeKind, mode: u16) -> Self {
        let mode = if mode & S_IFMT == 0 {
            mode | kind_to_ifmt(kind)
        } else {
            mode
        };
        Self {
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            atime: Timespec::EPOCH,
            mtime: Timespec::EPOCH,
            ctime: Timespec::EPOCH,
            nlinks: 1,
            blocks: 0,
            flags: 0,
        }
    }

    pub const fn kind(&self) -> InodeKind {
        match self.mode & S_IFMT {
            S_IFDIR => InodeKind::Directory,
            S_IFLNK => InodeKind::Symlink,
            S_IFCHR => InodeKind::CharDevice,
            S_IFBLK => InodeKind::BlockDevice,
            S_IFIFO => InodeKind::Fifo,
            S_IFSOCK => InodeKind::Socket,
            _ => InodeKind::Regular,
        }
    }
}

const fn kind_to_ifmt(kind: InodeKind) -> u16 {
    match kind {
        InodeKind::Regular => S_IFREG,
        InodeKind::Directory => S_IFDIR,
        InodeKind::Symlink => S_IFLNK,
        InodeKind::CharDevice => S_IFCHR,
        InodeKind::BlockDevice => S_IFBLK,
        InodeKind::Fifo => S_IFIFO,
        InodeKind::Socket => S_IFSOCK,
    }
}

// === directory iteration cursor =======================================

// Opaque directory iteration cursor per
// `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md` §pub-types: filesystem
// implementations define the internal byte layout. Helpers below cover the
// common case of a u64-shaped cursor stored in the leading 8 bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirCursor(pub [u8; 16]);

impl DirCursor {
    pub const START: Self = Self([0; 16]);

    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn from_u64(value: u64) -> Self {
        let v = value.to_le_bytes();
        Self([
            v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], 0, 0, 0, 0, 0, 0, 0, 0,
        ])
    }

    pub const fn as_u64(self) -> u64 {
        u64::from_le_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5], self.0[6], self.0[7],
        ])
    }

    pub const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

// === name types =======================================================

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct InlineName {
    len: u8,
    bytes: [u8; VFS_NAME_MAX],
}

impl InlineName {
    /// Empty-name sentinel for root-of-filesystem `DEntry`. The root
    /// dentry has no namable parent component; path-render code
    /// recognises `is_empty() == true` as "this is the root marker"
    /// and emits a leading `/` instead of a name component. The
    /// public `new` constructor rejects empty bytes — only `ROOT`
    /// produces an empty `InlineName`.
    pub const ROOT: Self = Self {
        len: 0,
        bytes: [0; VFS_NAME_MAX],
    };

    pub fn new(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > VFS_NAME_MAX || bytes.contains(&b'/') {
            return Err(Errno::ENAMETOOLONG);
        }

        let mut name = Self {
            len: bytes.len() as u8,
            bytes: [0; VFS_NAME_MAX],
        };
        name.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(name)
    }

    pub const fn len(self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl core::fmt::Debug for InlineName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match core::str::from_utf8(self.as_bytes()) {
            Ok(text) => write!(f, "InlineName({text:?})"),
            Err(_) => write!(f, "InlineName({:?})", self.as_bytes()),
        }
    }
}

// Lexicographic ordering on the active-prefix slice. A derive would
// compare `len` first and then the full inline buffer, which would
// (a) order shorter names before all longer ones regardless of bytes,
// and (b) include trailing zero padding. Hand-written `cmp` over
// `as_bytes()` is the intended `BTreeMap<InlineName, _>` key shape.
impl Ord for InlineName {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl PartialOrd for InlineName {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsName<'a>(&'a [u8]);

impl<'a> VfsName<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > VFS_NAME_MAX || bytes.contains(&b'/') {
            return Err(Errno::ENAMETOOLONG);
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub fs_object_id: FsObjectId,
    pub kind: InodeKind,
    pub name: InlineName,
}

impl DirEntry {
    pub fn new(fs_object_id: FsObjectId, kind: InodeKind, name: &[u8]) -> Result<Self, Errno> {
        Ok(Self {
            fs_object_id,
            kind,
            name: InlineName::new(name)?,
        })
    }
}

// === open-file flags + RNode backing ==================================

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenFileFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    /// `O_CLOEXEC` (Linux generic ABI bit `0o2000000` = `0x80000`):
    /// the resulting fd is marked close-on-exec so the next exec
    /// silently closes it. The walker itself does not look at this
    /// bit — it threads through to the syscall arm (`sys_open` once
    /// it lands; today only the per-process bitmap is exposed via
    /// `fcntl(F_SETFD)`), which is responsible for setting the
    /// matching bit in `ProcessPayload.fd_cloexec` after the fd
    /// table install completes. The flag stays on `OpenFileFlags`
    /// itself so a future `dup3(F_DUPFD_CLOEXEC)` / `pipe2` can
    /// observe it without re-decoding the open flags.
    pub cloexec: bool,
    /// `O_NONBLOCK` (Linux generic ABI bit `0o4000`): I/O against
    /// this fd never blocks — paths that would `Blocked(token)` for a
    /// blocking fd surface `Errno::EAGAIN` instead. fd-ops Wave 3
    /// honours this for `pipe2(2)` reader/writer ends; other backings
    /// (page-backed regular files, TTY) ignore it today and
    /// re-honour it once the per-backing nonblock plumbing lands.
    pub nonblocking: bool,
    /// `O_DIRECT` on pipes means packet mode. Other backings preserve
    /// this bit for `F_GETFL` / `F_SETFL` round-trips until direct-I/O
    /// semantics land in their owner.
    pub packet: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenFileIoctl<'a> {
    Tcgets,
    Tcsets { termios: Termios },
    Tiocgpgrp,
    Tiocspgrp { new_pgrp: &'a Cap<ProcessGroup> },
    Tiocgwinsz,
    Tiocswinsz { winsize: Winsize },
    Tiocsctty,
    Tiocnotty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenFileIoctlResult {
    None,
    Termios(Termios),
    Pgrp(u32),
    Winsize(Winsize),
    SideEffect(IoctlSideEffect),
}

#[derive(Clone, Debug)]
pub enum RNodeBacking {
    PageBacked {
        pc: Cap<PageContainer>,
    },
    Directory,
    /// Symlink target stored as raw bytes. Targets may contain `/`
    /// (multi-component) or be absolute (leading `/`), so the bytes
    /// cannot fit through `InlineName::new`'s slash-rejecting
    /// constructor; the walker substitutes the bytes directly into
    /// the remaining component stream per
    /// `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`.
    Symlink {
        target: Box<[u8]>,
    },
    StructBacked {
        payload: StructPayload,
    },
    Projected {
        schema: ProjectionSchemaId,
        key: ProjectionKey,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionSchemaId {
    Procfs,
    Sysfs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionKey {
    pub object_id: FsObjectId,
}

impl ProjectionKey {
    pub const fn from_fs_object_id(object_id: FsObjectId) -> Self {
        Self { object_id }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsNotifyKind {
    Inotify,
    Fanotify,
}

#[derive(Debug)]
pub struct FsNotifyInstance {
    kind: FsNotifyKind,
}

impl FsNotifyInstance {
    pub const fn new(kind: FsNotifyKind) -> Self {
        Self { kind }
    }

    pub fn new_cap(kind: FsNotifyKind) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new(kind))
    }

    pub const fn kind(&self) -> FsNotifyKind {
        self.kind
    }
}

#[derive(Clone, Debug)]
pub enum StructPayload {
    Tty(Cap<TtyIdentity>),
    CharDevice(&'static CharDeviceBinding),
    BlockDevice(&'static BlockDeviceRegistration),
    FsNotify {
        instance: Cap<FsNotifyInstance>,
    },
    /// Anonymous pipe — `pipe2(2)`. `side` distinguishes the
    /// reader-end RNode from the writer-end RNode; both share a
    /// single `Cap<PipePayload>`. fd-ops Wave 3.
    Pipe {
        payload: Cap<crate::pipe::PipePayload>,
        side: crate::pipe::PipeSide,
    },
    /// Socket-backed open file used by the network syscall facade.
    Socket {
        identity: Cap<SocketIdentity>,
    },
    NetNamespace {
        payload: PayloadCap<NetNamespacePayload>,
    },
    /// Mount-namespace-backed open file for `/proc/<pid>/ns/mnt`, consumed by
    /// `setns(2)`. Restored alongside the net subsystem re-home.
    MountNamespace {
        payload: Cap<crate::mount::MountNamespace>,
    },
}

// === live-node entities ===============================================

pub struct RNode {
    fs_object_id: FsObjectId,
    meta: InodeMeta,
    backing: RNodeBacking,
    containing_mount: Option<Weak<MountPayload>>,
    /// Shared persistent-object lifetime.  All RNodes and file
    /// PageContainers for the same `(mount, FsObjectId)` hold the same pin.
    object_pin: Option<FsObjectPin>,
    /// Optional per-inode readiness endpoints. Most RNodes in the current
    /// tree never expose VFS-level blocking readiness (pipes, tty, sockets,
    /// timerfd, eventfd, etc. own their wait sources on the backing object),
    /// so allocating two global wait-source slots for every path lookup
    /// turns long LTP runs into unbounded registry pressure. The endpoints
    /// are still available to future callers, but only once requested.
    wait_points: SpinMutex<Option<notification::RNodeWaitPoints>>,
}

impl RNode {
    pub fn new(fs_object_id: FsObjectId, meta: InodeMeta, backing: RNodeBacking) -> Self {
        Self {
            fs_object_id,
            meta,
            backing,
            containing_mount: None,
            object_pin: None,
            wait_points: SpinMutex::new(None),
        }
    }

    pub fn new_cap(
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        backing: RNodeBacking,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new(fs_object_id, meta, backing))
    }

    /// Like `new_cap` but stamps `containing_mount` immediately so
    /// `containing_mount_weak()` is non-None on the returned cap.
    /// Used by `materialise_child_rnode_v3` to forward the current
    /// mount context to descendant directory rnodes, enabling
    /// `fs_ops_for_rnode` (and therefore `getdents64`) to work on
    /// any sub-directory, not just the mount root.
    pub fn new_cap_in_mount(
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        backing: RNodeBacking,
        mount: &Cap<MountPayload>,
    ) -> Result<Cap<Self>, ZoneError> {
        let reservation = step_engine::reserve_for::<Self>()?;
        Ok(step_engine::sign_for(
            reservation,
            Self::new(fs_object_id, meta, backing).with_containing_mount(mount),
        ))
    }

    pub const fn fs_object_id(&self) -> FsObjectId {
        self.fs_object_id
    }

    pub const fn meta(&self) -> InodeMeta {
        self.meta
    }

    pub const fn backing(&self) -> &RNodeBacking {
        &self.backing
    }

    pub fn flock_try_acquire(
        &self,
        owner: u64,
        lock_type: u32,
        blocking: bool,
    ) -> Result<(), Errno> {
        if owner == 0 {
            return Err(Errno::EINVAL);
        }

        loop {
            {
                let mut locks = FLOCK_TABLE.lock();
                let record = locks.entry(self.fs_object_id).or_default();
                let can_lock = match lock_type {
                    1 => record.exclusive_owner.is_none() || record.exclusive_owner == Some(owner),
                    2 => {
                        (record.exclusive_owner.is_none()
                            && (record.shared_owners.is_empty()
                                || (record.shared_owners.len() == 1
                                    && record.shared_owners.contains(&owner))))
                            || record.exclusive_owner == Some(owner)
                    }
                    _ => return Err(Errno::EINVAL),
                };

                if can_lock {
                    match lock_type {
                        1 => {
                            record.exclusive_owner = None;
                            record.shared_owners.insert(owner);
                        }
                        2 => {
                            record.shared_owners.remove(&owner);
                            record.exclusive_owner = Some(owner);
                        }
                        _ => unreachable!(),
                    }
                    return Ok(());
                }

                if !blocking {
                    return Err(Errno::EAGAIN);
                }
            }
            core::hint::spin_loop();
        }
    }

    pub fn flock_release_owner(&self, owner: u64) {
        if owner == 0 {
            return;
        }

        let mut locks = FLOCK_TABLE.lock();
        let Some(record) = locks.get_mut(&self.fs_object_id) else {
            return;
        };

        if record.exclusive_owner == Some(owner) {
            record.exclusive_owner = None;
        }
        record.shared_owners.remove(&owner);

        if record.exclusive_owner.is_none() && record.shared_owners.is_empty() {
            locks.remove(&self.fs_object_id);
        }
    }

    pub fn with_containing_mount(mut self, mount: &Cap<MountPayload>) -> Self {
        if self.object_pin.is_none() {
            self.object_pin = Some(FsObjectPin::acquire(mount, self.fs_object_id));
        }
        self.containing_mount = Some(mount.downgrade());
        self
    }

    pub fn fs_object_destroy_token(&self) -> Option<FsObjectDestroyToken> {
        self.object_pin.as_ref().map(FsObjectPin::destroy_token)
    }

    /// Snapshot the containing-mount `Weak<MountPayload>`. Used by the
    /// VFS walker (`crate::vfs::walker::step_walk`) to resolve the
    /// in-scope `FsOps` for a dentry's RNode, and by the page cache
    /// when materialising file-backed RNodes.
    ///
    /// Returns `None` when the RNode was constructed without
    /// `with_containing_mount` (the trio's bootstrap rootfs root
    /// rnode falls into this category today; a follow-up wires the
    /// hint at mount-publication time).
    pub fn containing_mount_weak(&self) -> Option<Weak<MountPayload>> {
        self.containing_mount
    }

    fn with_wait_points<R>(&self, f: impl FnOnce(&notification::RNodeWaitPoints) -> R) -> R {
        let mut wait_points = self.wait_points.lock();
        if wait_points.is_none() {
            *wait_points = Some(notification::new_rnode_wait_points());
        }
        f(wait_points.as_ref().expect("wait points were initialized"))
    }

    fn read_wait_snapshot(&self) -> Option<Arc<WaitSource>> {
        let wait_points = self.wait_points.lock();
        wait_points
            .as_ref()
            .map(|points| points.read_source.clone())
    }

    fn write_wait_snapshot(&self) -> Option<Arc<WaitSource>> {
        let wait_points = self.wait_points.lock();
        wait_points
            .as_ref()
            .map(|points| points.write_source.clone())
    }

    /// Registry carrier id for [`Self::read_wait_source`].
    pub fn read_wait_source_id(&self) -> u64 {
        self.with_wait_points(|points| points.read_source_id)
    }

    /// Per-inode `WaitSource` for the mailbox-based wake path.
    fn read_wait_source(&self) -> Arc<WaitSource> {
        self.with_wait_points(|points| points.read_source.clone())
    }

    /// Read endpoint exposed to wait drivers.
    pub fn read_endpoint(&self) -> Arc<WaitSource> {
        self.read_wait_source()
    }

    /// Companion to [`Self::read_wait_source_id`] for the writable
    /// direction.
    pub fn write_wait_source_id(&self) -> u64 {
        self.with_wait_points(|points| points.write_source_id)
    }

    /// Companion to [`Self::read_wait_source`] for the writable
    /// direction.
    fn write_wait_source(&self) -> Arc<WaitSource> {
        self.with_wait_points(|points| points.write_source.clone())
    }

    /// Write endpoint exposed to wait drivers.
    pub fn write_endpoint(&self) -> Arc<WaitSource> {
        self.write_wait_source()
    }

    /// Fire the per-inode read wake path through a caller-provided mailbox
    /// post route.
    ///
    /// Fire the per-inode read wait source through a caller-provided mailbox
    /// post route.
    pub fn fire_read_wait_with_post<F>(&self, mask: u64, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        let Some(source) = self.read_wait_snapshot() else {
            return;
        };
        notification::notify_readable_with_post(&source, mask, post)
    }

    /// Companion to [`Self::fire_read_wait_with_post`] for the writable
    /// direction.
    pub fn fire_write_wait_with_post<F>(&self, mask: u64, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        let Some(source) = self.write_wait_snapshot() else {
            return;
        };
        notification::notify_writable_with_post(&source, mask, post)
    }
}

/// PR-3D-5: release the per-inode wait-source registry slots when the
/// inode retires. EBR semantics on `Cap<RNode>` defer the drop until
/// concurrent readers' guards complete, so the registry release
/// happens after every observer has unparked. Symmetric with
/// `Drop for PipePayload` and `Drop for TtyIdentity` (the latter
/// releases via the identity-side carrier on payload teardown — for
/// RNode the carrier lives on the identity itself, so `Drop for
/// RNode` is the natural site). Without this drop, the
/// large-N inode-create-destroy stress path would leak two registry
/// rows per inode.
impl Drop for RNode {
    fn drop(&mut self) {
        let Some(wait_points) = self.wait_points.lock().take() else {
            return;
        };
        notification::release_rnode_wait_points(
            wait_points.read_source_id,
            wait_points.write_source_id,
        );
    }
}

// PR-3D-5: `WaitSource` is not `Debug`, so the previous `#[derive(Debug)]`
// on `RNode` cannot survive after the wait-source fields land. The manual
// impl below preserves the previously-derived shape (one entry per kept
// field) and elides the wake-publication internals (which would be noisy and
// carry no value for debug
// output — observers care about the inode identity / metadata, not
// the registered subscriber list).
impl core::fmt::Debug for RNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RNode")
            .field("fs_object_id", &self.fs_object_id)
            .field("meta", &self.meta)
            .field("backing", &self.backing)
            .field("containing_mount", &self.containing_mount)
            .field("object_pin", &self.object_pin)
            .field("wait_points_allocated", &self.wait_points.lock().is_some())
            .finish()
    }
}

#[derive(Debug)]
pub struct DEntry {
    name: InlineName,
    parent: Option<Cap<DEntry>>,
    rnode: Cap<RNode>,
    mounted: Option<Weak<MountIdentity>>,
    children: SpinMutex<BTreeMap<InlineName, Weak<DEntry>>>,
    // Keep this field after `rnode`: Rust drops fields in declaration order,
    // so observers are marked only after the EBR callback has released the
    // DEntry-owned RNode Cap.
    retire_notifier: DEntryRetireNotifier,
}

#[derive(Clone, Debug)]
pub struct DEntryRetireToken {
    retired: Arc<AtomicBool>,
}

impl DEntryRetireToken {
    pub(crate) fn retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct DEntryRetireNotifier {
    observers: SpinMutex<Vec<Arc<AtomicBool>>>,
}

impl DEntryRetireNotifier {
    const fn new() -> Self {
        Self {
            observers: SpinMutex::new(Vec::new()),
        }
    }

    fn token(&self) -> DEntryRetireToken {
        let retired = Arc::new(AtomicBool::new(false));
        self.observers.lock().push(retired.clone());
        DEntryRetireToken { retired }
    }
}

impl Drop for DEntryRetireNotifier {
    fn drop(&mut self) {
        for observer in self.observers.lock().drain(..) {
            observer.store(true, Ordering::Release);
        }
    }
}

impl DEntry {
    pub fn new(name: InlineName, rnode: Cap<RNode>) -> Self {
        Self {
            name,
            parent: None,
            rnode,
            mounted: None,
            children: SpinMutex::new(BTreeMap::new()),
            retire_notifier: DEntryRetireNotifier::new(),
        }
    }

    pub fn new_cap(name: InlineName, rnode: Cap<RNode>) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new(name, rnode))
    }

    pub const fn name(&self) -> InlineName {
        self.name
    }

    pub fn rnode(&self) -> &Cap<RNode> {
        &self.rnode
    }

    pub fn retire_token(&self) -> DEntryRetireToken {
        self.retire_notifier.token()
    }

    pub fn set_parent_hint(&mut self, parent: &Cap<DEntry>) {
        self.parent = Some(parent.clone());
    }

    pub fn set_mounted_hint(&mut self, mount: &Cap<MountIdentity>) {
        self.mounted = Some(mount.downgrade());
    }

    /// Return the parent-hint `Cap<DEntry>` if installed. The parent is held
    /// strongly so live cwd/path dentries keep their ancestor chain renderable;
    /// the child cache is weak, so parent/child cache cycles do not retain
    /// removed directory subtrees.
    pub fn parent_hint(&self) -> Option<Cap<DEntry>> {
        self.parent.clone()
    }

    /// Snapshot the `mounted` weak hint. Used by the VFS walker
    /// (`crate::vfs::walker::step_walk`) for mount-point boundary
    /// crossing per
    /// `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1` and
    /// `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`. Returns `None`
    /// for dentries that have not been published as a mount point.
    pub fn mounted_hint(&self) -> Option<Weak<MountIdentity>> {
        self.mounted
    }

    pub fn cached_child(&self, name: InlineName) -> Option<Cap<DEntry>> {
        let mut children = self.children.lock();
        let child = children.get(&name).copied()?;
        let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
        match child.upgrade(&guard) {
            Some(cap) => Some(cap),
            None => {
                children.remove(&name);
                None
            }
        }
    }

    pub fn cache_child(&self, child: Cap<DEntry>) -> Cap<DEntry> {
        let mut children = self.children.lock();
        if let Some(existing) = children.get(&child.name()).copied() {
            let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
            if let Some(canonical) = existing.upgrade(&guard) {
                return canonical;
            }
        }
        children.insert(child.name(), child.downgrade());
        child
    }

    pub fn remove_cached_child(&self, name: InlineName) {
        self.children.lock().remove(&name);
    }

    pub fn remove_cached_child_by_name(&self, name: &[u8]) {
        if let Ok(name) = InlineName::new(name) {
            self.remove_cached_child(name);
        }
    }

    pub fn clear_cached_children(&self) {
        self.children.lock().clear();
    }
}

/// Render an absolute path string for a `DEntry` by walking its
/// `parent_hint` chain up to the root. Each component contributes
/// its `name()` bytes; the chain terminates at a dentry whose
/// `parent_hint` is `None` (the root marker).
///
/// Conventions:
/// - The root `DEntry` carries `InlineName::ROOT` (empty); the
///   render emits a single `/` for it.
/// - Non-root components are joined by `/`. Output starts with `/`.
/// - Output bytes are not validated as UTF-8 — POSIX paths are
///   byte sequences with `/` and `\0` reserved.
pub fn render_dentry_path(dentry: &Cap<DEntry>) -> Option<alloc::vec::Vec<u8>> {
    render_dentry_path_in_namespace(dentry, None)
}

/// Namespace-aware variant of [`render_dentry_path`]: when the parent-hint
/// chain tops out at a mounted filesystem's root dentry, hop to that mount's
/// mountpoint dentry and keep walking toward the namespace root.
///
/// Without the hop, a cwd inside a mount renders with the mountpoint
/// component silently dropped — `cd /musl/root/t1` then `getcwd()` returned
/// `/root/t1`, and every caller that round-trips the result through an
/// absolute-path walk (git stores `getcwd()` output and re-opens
/// `<cwd>/.git`) landed in a directory that does not exist. `None` keeps the
/// plain chain rendering for callers with no namespace in scope.
pub fn render_dentry_path_in_namespace(
    dentry: &Cap<DEntry>,
    namespace: Option<&Cap<crate::mount::MountNamespace>>,
) -> Option<alloc::vec::Vec<u8>> {
    let mut components: alloc::vec::Vec<InlineName> = alloc::vec::Vec::new();
    let mut cursor: Cap<DEntry> = dentry.clone();
    // Mount hops are bounded so a mis-registered mountpoint chain (a cycle)
    // degrades to a truncated path instead of a hang.
    let mut hops = 0usize;
    loop {
        components.push(cursor.name());
        if let Some(parent) = cursor.parent_hint() {
            cursor = parent;
            continue;
        }
        // Chain top. If it is the root of a mounted fs, continue from the
        // dentry the mount is attached to (e.g. `/musl`).
        let Some(ns) = namespace else { break };
        let Some(mount) = ns.mount_containing_dentry(&cursor) else {
            break;
        };
        let Some(mountpoint) = mount.mountpoint() else {
            // The namespace root mount: rendering is complete.
            break;
        };
        if mountpoint.key() == cursor.key() || hops >= 8 {
            break;
        }
        hops += 1;
        cursor = mountpoint;
    }

    // components collected leaf → root; reverse for root → leaf rendering.
    components.reverse();

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    out.push(b'/');
    let mut first_named = true;
    for name in &components {
        if name.is_empty() {
            // Root marker — leading slash already pushed.
            continue;
        }
        if !first_named {
            out.push(b'/');
        }
        out.extend_from_slice(name.as_bytes());
        first_named = false;
    }
    Some(out)
}

/// Tag for the non-VFS `OpenFile` shapes. PR-10 phase 0 (D7 §3.7)
/// introduces the first non-RNode variant — `Ufd` — so a
/// `userfaultfd(2)` fd can live in the same `BTreeMap<u32, Cap<OpenFile>>`
/// fd table as every other open file. Phase 0 keeps the existing
/// RNode-backed shape as the default; later passes (e.g. pipe
/// migration, `memfd_create`) may move pipes / anon fds into their
/// own variants too.
///
/// Construction APIs:
/// - `OpenFile::new(rnode, flags)` / `OpenFile::new_cap` — VFS-backed,
///   always builds `OpenFileBacking::Rnode { rnode }`. Every existing
///   call site keeps working unchanged.
/// - `OpenFile::new_userfaultfd(ufd, flags)` /
///   `OpenFile::new_userfaultfd_cap` — userfaultfd-backed.
///
/// The legacy `pub fn rnode(&self) -> &Cap<RNode>` accessor still
/// resolves the inner cap for the `Rnode` shape and **panics** for
/// `Ufd`. Callers that may handle either kind discriminate via
/// [`OpenFile::backing`] / [`OpenFile::ufd`] first.
#[derive(Debug)]
pub enum OpenFileBacking {
    /// VFS-rooted open file. Every existing in-tree path uses this
    /// variant: regular files, directories, TTYs, char/block devices,
    /// pipes, symlinks (the symlink target is just bytes on the
    /// RNode).
    Rnode { rnode: Cap<RNode> },
    /// `userfaultfd(2)` open file (PR-10 phase 0). The cap is the
    /// substrate-side endpoint identity later phases use to install
    /// page-fault delegation requests; see
    /// `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`.
    /// Drop semantics: when the last `Cap<OpenFile>` for a ufd is
    /// released and EBR retires this slot, the inner `Cap<UserfaultFd>`
    /// drops too and (in later phases) `Drop for UserfaultFd` will
    /// drive the `DelegateRegistry` delegate endpoint-death transition.
    Ufd { ufd: Cap<UserfaultFd> },
    /// `io_setup(2)` open file (PR-11 phase 1). The cap is the
    /// substrate-side `AioContext` identity later phases use to route
    /// iocb submissions and completion events; see
    /// `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §4.1
    /// for the fd-shape decision (we normalize `aio_context_t` to a
    /// real fd, diverging from Linux's pointer-shaped opaque value).
    /// Drop semantics: when the last `Cap<OpenFile>` for an AIO
    /// context is released and EBR retires this slot, the inner
    /// `Cap<AioContext>` drops too — in later phases the
    /// `Drop for AioContext` impl will fire the worker's
    /// `exit_source` so the `with_on_behalf_of` borrow body observes
    /// `Killed` and the worker task aborts.
    AioContext { ctx: Cap<AioContext> },
    /// `signalfd(2)` open file (D9-D). The cap is the substrate-side
    /// per-fd signal subscription identity; signal posts to the owning
    /// process route through the per-process subscription registry in
    /// [`crate::signalfd`] and fan out to every matching cap *after*
    /// the existing thread-eligibility post.
    /// Drop semantics: when the last `Cap<OpenFile>` for a signalfd is
    /// released and EBR retires this slot, the inner `Cap<SignalFd>`
    /// drops too — `Drop for SignalFd` removes the subscription entry
    /// from the per-process registry so future
    /// process-directed kill calls no longer route to it.
    SignalFd { sfd: Cap<SignalFd> },
    /// `io_uring_setup(2)` open file (future PR-12 phase 0 — second
    /// `OnBehalfOf<P>` canary). The cap is the substrate-side
    /// `IoUring` identity later phases use to route SQE submissions
    /// and completion events; see
    /// `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §8.1 (SQPOLL design).
    /// Drop semantics: when the last `Cap<OpenFile>` for a uring fd
    /// is released and EBR retires this slot, the inner
    /// `Cap<IoUring>` drops too — `Drop for IoUring` trips the SQPOLL
    /// kthread's `worker_abort` signal so the `with_on_behalf_of`
    /// borrow body observes the cooperative-cancel reason and the
    /// kthread aborts.
    IoUring { ring: Cap<IoUring> },
    /// `epoll_create1(2)` open file (Phase B.1). The cap is the
    /// substrate-side [`Epoll`] identity used to route readiness
    /// notifications via `YieldShape::OnEdge`.
    Epoll { ep: Cap<Epoll> },
    /// `eventfd(2)` open file. The cap is the substrate-side
    /// [`EventFd`] identity carrying a 64-bit counter.  Read drains
    /// the counter; write adds to it.  Drop semantics: when the last
    /// `Cap<OpenFile>` for an eventfd is released and EBR retires
    /// this slot, the inner `Cap<EventFd>` drops too — no side
    /// effects (eventfd has no external registrations to clean up).
    Eventfd { efd: Cap<EventFd> },
    /// `timerfd_create(2)` open file. The cap is the substrate-side
    /// [`TimerFd`] identity carrying a deadline and expiration
    /// counter.  Read returns the number of expirations since the
    /// last read.  Drop semantics: same as eventfd — the inner
    /// `Cap<TimerFd>` drops via EBR with no external cleanup needed.
    Timerfd { tfd: Cap<TimerFd> },
    /// `mq_open(2)` open file. musl defines `mqd_t` as `int`, so the
    /// descriptor is a normal fd-table entry whose backing carries the
    /// POSIX mq identity and per-open flags.
    PosixMq { mq: Cap<PosixMqInstance> },
    /// `pidfd_open(2)` open file. The fd keeps a capability reference
    /// to the target process identity; exit/readiness lives on the
    /// process side, so this adapter does not duplicate signal truth.
    Pidfd { process: Cap<ProcessIdentity> },
    /// Minimal typed kernel-object fds for syscall surfaces whose full
    /// subsystem semantics are intentionally staged.
    KernelObject { object: KernelObjectFile },
    /// Linux new mount API fd (`fsopen`, `fspick`, `fsmount`, `open_tree`).
    MountApi { file: Cap<MountApiFile> },
    /// Minimal anonymous `socketpair(AF_UNIX, SOCK_STREAM)` open file.
    /// Each endpoint is backed by two pipe payloads: `rx` receives
    /// bytes from the peer, and `tx` carries bytes written by this fd
    /// to the peer. It is a non-path fd kind, so syscall handlers must
    /// dispatch via [`OpenFile::socketpair_endpoint`] before calling
    /// [`OpenFile::rnode`].
    SocketPair {
        rx: Cap<crate::pipe::PipePayload>,
        tx: Cap<crate::pipe::PipePayload>,
    },
}

/// Metadata carried by phase-0 kernel-object fds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelObjectFile {
    PerfEvent(PerfEventFile),
    BpfMap(BpfMapFile),
}

/// `perf_event_open(2)` phase-0 fd metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfEventFile {
    pub attr_type: u32,
    pub config: u64,
    pub pid: i32,
    pub cpu: i32,
    pub group_fd: i32,
    pub flags: u64,
}

/// `bpf(BPF_MAP_CREATE)` phase-0 fd metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BpfMapFile {
    pub map_type: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
}

/// Per-fd file-position carrier.
///
/// fd-ops Wave 4 made `offset` interior-mutable (`AtomicU64`) so
/// `OpenFile::step_lseek` and the page-backed `step_read` /
/// `step_write` lanes can mutate the position through `&self`. This
/// matches Linux's "shared file description across `dup`/`fork` →
/// shared offset" semantic without bolting an extra lock onto every
/// fd-table read.
///
/// **PR-10 phase 0:** the backing field replaces the historical
/// `rnode: Cap<RNode>` direct field, gated through the
/// [`OpenFileBacking`] enum. The VFS-shaped accessor
/// [`Self::rnode`] still returns `&Cap<RNode>` for backwards
/// compatibility with existing call sites; it panics if the file
/// happens to be a `userfaultfd(2)` (a state no VFS-aware caller can
/// reach today).
#[derive(Debug)]
pub struct OpenFile {
    pub(crate) backing: OpenFileBacking,
    offset: AtomicU64,
    /// Per-fd readdir cursor. Slice 6 of the shell-prompt roadmap
    /// added this so `getdents64(2)` can resume across calls without
    /// rewinding the directory each time.
    ///
    /// Stored as an `AtomicU64` round-tripped through
    /// [`DirCursor::from_u64`] / [`DirCursor::as_u64`] — every in-tree
    /// FsOps backend (tmpfs, devfs) emits a u64-shaped cursor, so the
    /// 16-byte `DirCursor` pads with zeros above the lower u64 word.
    /// `Cap` clone (`dup` / `fork`) shares the cell, matching Linux's
    /// "shared file description across `dup` / `fork`" semantic for
    /// directory streams.
    readdir_cursor: AtomicU64,
    pub(crate) flags: OpenFileFlags,
    /// Runtime `O_NONBLOCK` override set via `fcntl(F_SETFL)`.
    nonblocking_override: AtomicI8,
    /// Runtime pipe packet-mode override set via `fcntl(F_SETFL)`.
    packet_override: AtomicI8,
    /// Best-effort DEntry hint set by `step_open`. `None` for
    /// non-VFS shapes (ufd, aio, etc.). Used by `fchdir`.
    opendir_dentry: Option<Cap<DEntry>>,
}

impl OpenFile {
    pub fn new(rnode: Cap<RNode>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Rnode { rnode },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    pub fn new_cap(rnode: Cap<RNode>, flags: OpenFileFlags) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new(rnode, flags))
    }

    /// Like [`Self::new_cap`] but also records the DEntry that
    /// `step_open` resolved so `fchdir` can recover it later.
    pub fn new_cap_with_dentry(
        rnode: Cap<RNode>,
        flags: OpenFileFlags,
        dentry: Cap<DEntry>,
    ) -> Result<Cap<Self>, ZoneError> {
        let mut file = Self::new(rnode, flags);
        file.opendir_dentry = Some(dentry);
        step_engine::sign(file)
    }

    /// Construct a userfaultfd-backed `OpenFile` (PR-10 phase 0). The
    /// resulting value carries `OpenFileBacking::Ufd { ufd }` and no
    /// `Cap<RNode>` — userfaultfd is a non-VFS fd kind (see D7 §3.7).
    /// Existing VFS-only paths (`step_read` / `step_write` /
    /// `step_lseek` / etc.) must not be called against this shape;
    /// callers branch via [`Self::backing`] / [`Self::ufd`].
    pub fn new_userfaultfd(ufd: Cap<UserfaultFd>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Ufd { ufd },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh userfaultfd-backed `OpenFile`. The
    /// counterpart to [`Self::new_cap`] for the ufd shape (PR-10
    /// phase 0).
    pub fn new_userfaultfd_cap(
        ufd: Cap<UserfaultFd>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_userfaultfd(ufd, flags))
    }

    /// Construct an AIO-context-backed `OpenFile` (PR-11 phase 1). The
    /// resulting value carries `OpenFileBacking::AioContext { ctx }`
    /// and no `Cap<RNode>` — AIO contexts are a non-VFS fd kind
    #[allow(clippy::too_many_arguments)]
    /// (joining ufd in the OpenFileBacking enum, see D8 §4.1).
    /// Existing VFS-only paths (`step_read` / `step_write` /
    /// `step_lseek` / etc.) must not be called against this shape;
    /// callers branch via [`Self::backing`] / [`Self::aio_context`].
    pub fn new_aio_context(ctx: Cap<AioContext>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::AioContext { ctx },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh AIO-context-backed `OpenFile`. The
    /// counterpart to [`Self::new_cap`] for the AIO shape (PR-11
    /// phase 1).
    pub fn new_aio_context_cap(
        ctx: Cap<AioContext>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_aio_context(ctx, flags))
    }

    /// Construct a signalfd-backed `OpenFile` (D9-D). The resulting
    /// value carries `OpenFileBacking::SignalFd { sfd }` and no
    /// `Cap<RNode>` — signalfds are a non-VFS fd kind (joining ufd
    /// and AIO in the OpenFileBacking enum, see D9 §6).
    pub fn new_signalfd(sfd: Cap<SignalFd>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::SignalFd { sfd },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh signalfd-backed `OpenFile` (D9-D).
    pub fn new_signalfd_cap(
        sfd: Cap<SignalFd>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_signalfd(sfd, flags))
    }

    /// Construct an epoll-backed `OpenFile` (Phase B.1).
    /// The resulting value carries `OpenFileBacking::Epoll { ep }`
    /// and no `Cap<RNode>` — epoll instances are a non-VFS fd kind.
    pub fn new_epoll(ep: Cap<Epoll>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Epoll { ep },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh epoll-backed `OpenFile` (Phase B.1).
    pub fn new_epoll_cap(ep: Cap<Epoll>, flags: OpenFileFlags) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_epoll(ep, flags))
    }

    /// Construct an eventfd-backed `OpenFile`.
    /// The resulting value carries `OpenFileBacking::Eventfd { efd }`
    /// and no `Cap<RNode>`.
    pub fn new_eventfd(efd: Cap<EventFd>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Eventfd { efd },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh eventfd-backed `OpenFile`.
    pub fn new_eventfd_cap(
        efd: Cap<EventFd>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_eventfd(efd, flags))
    }

    /// Construct a timerfd-backed `OpenFile`.
    /// The resulting value carries `OpenFileBacking::Timerfd { tfd }`
    /// and no `Cap<RNode>`.
    pub fn new_timerfd(tfd: Cap<TimerFd>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Timerfd { tfd },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh timerfd-backed `OpenFile`.
    pub fn new_timerfd_cap(
        tfd: Cap<TimerFd>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_timerfd(tfd, flags))
    }

    /// Construct a POSIX message-queue-backed `OpenFile`.
    pub fn new_posix_mq(mq: Cap<PosixMqInstance>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::PosixMq { mq },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh POSIX message-queue-backed `OpenFile`.
    pub fn new_posix_mq_cap(
        mq: Cap<PosixMqInstance>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_posix_mq(mq, flags))
    }

    /// Construct a pidfd-backed `OpenFile`.
    pub fn new_pidfd(process: Cap<ProcessIdentity>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Pidfd { process },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh pidfd-backed `OpenFile`.
    pub fn new_pidfd_cap(
        process: Cap<ProcessIdentity>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_pidfd(process, flags))
    }

    /// Construct a phase-0 kernel-object-backed `OpenFile`.
    pub fn new_kernel_object(object: KernelObjectFile, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::KernelObject { object },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh phase-0 kernel-object-backed `OpenFile`.
    pub fn new_kernel_object_cap(
        object: KernelObjectFile,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_kernel_object(object, flags))
    }

    /// Construct a new-mount-API-backed `OpenFile`.
    pub fn new_mount_api(file: Cap<MountApiFile>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::MountApi { file },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh new-mount-API-backed `OpenFile`.
    pub fn new_mount_api_cap(
        file: Cap<MountApiFile>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_mount_api(file, flags))
    }

    /// Construct one endpoint of an anonymous stream socketpair.
    pub fn new_socketpair_endpoint(
        rx: Cap<crate::pipe::PipePayload>,
        tx: Cap<crate::pipe::PipePayload>,
        flags: OpenFileFlags,
    ) -> Self {
        Self {
            backing: OpenFileBacking::SocketPair { rx, tx },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign one anonymous stream socketpair endpoint.
    pub fn new_socketpair_endpoint_cap(
        rx: Cap<crate::pipe::PipePayload>,
        tx: Cap<crate::pipe::PipePayload>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_socketpair_endpoint(rx, tx, flags))
    }

    /// Construct an io_uring-backed `OpenFile` (future PR-12 phase 0 —
    /// second `OnBehalfOf<P>` canary). The resulting value carries
    /// `OpenFileBacking::IoUring { ring }` and no `Cap<RNode>` —
    /// io_uring rings are a non-VFS fd kind (joining ufd, AIO, and
    /// signalfd in the OpenFileBacking enum). Callers branch via
    /// [`Self::backing`] / [`Self::io_uring`].
    pub fn new_io_uring(ring: Cap<IoUring>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::IoUring { ring },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicI8::new(-1),
            packet_override: AtomicI8::new(-1),
            flags,
            opendir_dentry: None,
        }
    }

    /// Zone-sign a fresh io_uring-backed `OpenFile`.
    pub fn new_io_uring_cap(
        ring: Cap<IoUring>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_io_uring(ring, flags))
    }

    /// Snapshot the backing shape. Callers that may handle either an
    /// RNode-backed or a ufd-backed OpenFile discriminate via this
    /// accessor; the legacy [`Self::rnode`] accessor stays valid for
    /// the dominant VFS path.
    pub fn backing(&self) -> &OpenFileBacking {
        &self.backing
    }

    /// Return the anonymous-pipe endpoint carried by this file, if any.
    ///
    /// Used by process fd-table accounting so `close` / `dup` / `fork`
    /// update pipe reader/writer counts synchronously. That keeps pipe
    /// EOF/EPIPE visible at fd-close time instead of waiting for EBR to
    /// eventually retire the shared `OpenFile`.
    pub fn pipe_endpoint(&self) -> Option<(Cap<crate::pipe::PipePayload>, crate::pipe::PipeSide)> {
        let OpenFileBacking::Rnode { rnode } = &self.backing else {
            return None;
        };
        let RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { payload, side },
        } = rnode.backing()
        else {
            return None;
        };
        Some((payload.clone(), *side))
    }

    /// Return the anonymous stream socketpair endpoint, if this fd is
    /// one. The first payload is the readable side for this fd; the
    /// second is the writable side toward the peer.
    pub fn socketpair_endpoint(
        &self,
    ) -> Option<(Cap<crate::pipe::PipePayload>, Cap<crate::pipe::PipePayload>)> {
        match &self.backing {
            OpenFileBacking::SocketPair { rx, tx } => Some((rx.clone(), tx.clone())),
            _ => None,
        }
    }

    /// Adjust the fd-table-visible endpoint count without cloning any `Cap`.
    /// Exec's post-PoNR CLOEXEC commit uses the decrement form, so this method
    /// must remain allocation-free and infallible.
    pub(crate) fn adjust_process_fd_reference(&self, increment: bool) {
        self.adjust_process_fd_reference_with_post(increment, &mut |mailbox, event| {
            mailbox.post(event)
        });
    }

    /// Adjust fd-table-visible endpoint counts while routing any last-close
    /// wake through the caller. This is required for exit-time pipe EOF: the
    /// reader may be parked on a different scheduler hart from the exiting
    /// writer.
    pub(crate) fn adjust_process_fd_reference_with_post<F>(&self, increment: bool, post: &mut F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        // final-smp tracks descriptor ownership explicitly for network
        // sockets. Cap clones are lifetime pins and cannot be used to decide
        // when close(2) must publish FIN/EOF.
        if let Some(socket) = self.socket_identity() {
            if increment {
                socket.incr_fd_ref();
            } else {
                socket.decr_fd_ref();
            }
        }
        match &self.backing {
            OpenFileBacking::Rnode { rnode } => {
                let RNodeBacking::StructBacked {
                    payload: StructPayload::Pipe { payload, side },
                } = rnode.backing()
                else {
                    return;
                };
                match (*side, increment) {
                    (crate::pipe::PipeSide::Reader, true) => payload.incr_reader(),
                    (crate::pipe::PipeSide::Writer, true) => payload.incr_writer(),
                    (crate::pipe::PipeSide::Reader, false) => {
                        payload.decr_reader_with_post(&mut *post)
                    }
                    (crate::pipe::PipeSide::Writer, false) => {
                        payload.decr_writer_with_post(&mut *post)
                    }
                }
            }
            OpenFileBacking::SocketPair { rx, tx } => {
                if increment {
                    rx.incr_reader();
                    tx.incr_writer();
                } else {
                    rx.decr_reader_with_post(&mut *post);
                    tx.decr_writer_with_post(&mut *post);
                }
            }
            _ => {}
        }
    }

    /// P3-S2/S5 (D13): byte and lifecycle operations supplied by a rich
    /// file kind. Readiness remains fd-neutral through `query_fd_ready`.
    pub fn file_ops(&self) -> Option<&dyn crate::device::FileOps> {
        self.socket_identity()
            .map(|identity| identity as &dyn crate::device::FileOps)
    }

    /// Socket identity backing this open file, if it is a network socket.
    /// Restored alongside the net subsystem re-home; used by
    /// `/proc/net/{tcp,udp,...}` enumeration to walk a process's open sockets.
    pub fn socket_identity(&self) -> Option<&Cap<SocketIdentity>> {
        match &self.backing {
            OpenFileBacking::Rnode { rnode } => match rnode.backing() {
                RNodeBacking::StructBacked {
                    payload: StructPayload::Socket { identity },
                } => Some(identity),
                _ => None,
            },
            _ => None,
        }
    }

    /// VFS-shaped accessor — returns the inner `Cap<RNode>` for an
    /// `OpenFileBacking::Rnode` shape.
    ///
    /// # Panics
    ///
    /// Panics for `OpenFileBacking::Ufd`. Userfaultfd fds are
    /// non-VFS; callers reachable from a VFS path cannot encounter
    /// this state today (no in-tree code installs a ufd on an
    /// otherwise-VFS dispatch path). New code that may handle either
    /// shape should branch on [`Self::backing`] first.
    pub fn rnode(&self) -> &Cap<RNode> {
        match &self.backing {
            OpenFileBacking::Rnode { rnode } => rnode,
            OpenFileBacking::Ufd { .. } => panic!(
                "OpenFile::rnode() called on a userfaultfd-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::ufd() first",
            ),
            OpenFileBacking::AioContext { .. } => panic!(
                "OpenFile::rnode() called on an AIO-context-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::aio_context() first",
            ),
            OpenFileBacking::SignalFd { .. } => panic!(
                "OpenFile::rnode() called on a signalfd-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::signalfd() first",
            ),
            OpenFileBacking::Epoll { .. } => panic!(
                "OpenFile::rnode() called on an epoll-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::epoll() first",
            ),
            OpenFileBacking::IoUring { .. } => panic!(
                "OpenFile::rnode() called on an io_uring-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::io_uring() first",
            ),
            OpenFileBacking::Eventfd { .. } => panic!(
                "OpenFile::rnode() called on an eventfd-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::eventfd() first",
            ),
            OpenFileBacking::Timerfd { .. } => panic!(
                "OpenFile::rnode() called on a timerfd-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::timerfd() first",
            ),
            OpenFileBacking::PosixMq { .. } => panic!(
                "OpenFile::rnode() called on a POSIX-mq-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::posix_mq() first",
            ),
            OpenFileBacking::Pidfd { .. } => panic!(
                "OpenFile::rnode() called on a pidfd-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::pidfd_process() first",
            ),
            OpenFileBacking::KernelObject { .. } => panic!(
                "OpenFile::rnode() called on a kernel-object-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::kernel_object() first",
            ),
            OpenFileBacking::MountApi { .. } => panic!(
                "OpenFile::rnode() called on a mount-api-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::mount_api_file() first",
            ),
            OpenFileBacking::SocketPair { .. } => panic!(
                "OpenFile::rnode() called on a socketpair-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::socketpair_endpoint() first",
            ),
        }
    }

    /// Return the DEntry hint set by step_open.  Used by fchdir.
    pub fn opendir_dentry(&self) -> Option<Cap<DEntry>> {
        self.opendir_dentry.clone()
    }

    /// Acquire an advisory file lock (POSIX flock).
    ///
    /// Returns `Ok(())` on success, `Err(EWOULDBLOCK)` if
    /// non-blocking and another holder has the lock.
    /// `lock_type`: `LOCK_SH` (1) or `LOCK_EX` (2).
    /// `blocking`: `false` = `LOCK_NB`.
    pub fn flock_acquire(
        &self,
        lock_type: u32,
        blocking: bool,
    ) -> Result<(), crate::execution::Errno> {
        let owner = self as *const Self as u64;
        self.rnode().flock_try_acquire(owner, lock_type, blocking)
    }

    /// Release a held advisory lock.
    pub fn flock_release(&self) {
        let owner = self as *const Self as u64;
        if let OpenFileBacking::Rnode { rnode } = &self.backing {
            rnode.flock_release_owner(owner);
        }
    }

    /// `Some(&Cap<UserfaultFd>)` iff this `OpenFile` is the
    /// userfaultfd-backed shape (PR-10 phase 0). Returns `None` for
    /// every VFS-backed `OpenFile`. The future `sys_close` /
    /// `ioctl(UFFDIO_*)` paths branch on this — phase 0 only
    /// exposes it for the fd-table scaffold tests to verify the
    /// install/retrieve round-trip preserves the inner cap identity.
    pub fn ufd(&self) -> Option<&Cap<UserfaultFd>> {
        match &self.backing {
            OpenFileBacking::Ufd { ufd } => Some(ufd),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<AioContext>)` iff this `OpenFile` is the
    /// AIO-context-backed shape (PR-11 phase 1). Returns `None` for
    /// every non-AIO `OpenFile`. The future `sys_io_submit` /
    /// `sys_io_getevents` / `sys_io_destroy` paths branch on this —
    /// phase 1 only exposes it for the fd-table scaffold tests to
    /// verify the install/retrieve round-trip preserves the inner cap
    /// identity.
    pub fn aio_context(&self) -> Option<&Cap<AioContext>> {
        match &self.backing {
            OpenFileBacking::AioContext { ctx } => Some(ctx),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<SignalFd>)` iff this `OpenFile` is the signalfd-backed
    /// shape (D9-D). Returns `None` for every non-signalfd `OpenFile`.
    /// The `sys_read(2)` arm branches on this to dispatch into
    /// `signalfd::signalfd_read`; future paths
    /// (`sys_signalfd4(fd, ...)` mask-update) consult this accessor as
    /// well.
    pub fn signalfd(&self) -> Option<&Cap<SignalFd>> {
        match &self.backing {
            OpenFileBacking::SignalFd { sfd } => Some(sfd),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<Epoll>)` iff this `OpenFile` is the epoll-backed
    /// shape. Returns `None` for every non-epoll `OpenFile`.
    pub fn epoll(&self) -> Option<&Cap<Epoll>> {
        match &self.backing {
            OpenFileBacking::Epoll { ep } => Some(ep),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<EventFd>)` iff this `OpenFile` is the eventfd-backed
    /// shape.  Returns `None` for every non-eventfd `OpenFile`.
    pub fn eventfd(&self) -> Option<&Cap<EventFd>> {
        match &self.backing {
            OpenFileBacking::Eventfd { efd } => Some(efd),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<TimerFd>)` iff this `OpenFile` is the timerfd-backed
    /// shape.  Returns `None` for every non-timerfd `OpenFile`.
    pub fn timerfd(&self) -> Option<&Cap<TimerFd>> {
        match &self.backing {
            OpenFileBacking::Timerfd { tfd } => Some(tfd),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<IoUring>)` iff this `OpenFile` is the io_uring-backed
    /// shape (future PR-12 phase 0 — second `OnBehalfOf<P>` canary).
    /// Returns `None` for every non-uring `OpenFile`. The future
    /// `sys_io_uring_enter` / `sys_io_uring_destroy` paths branch on
    /// this — scaffold phase only exposes it for the fd-table tests to
    /// verify the install/retrieve round-trip preserves the inner cap
    /// identity.
    pub fn io_uring(&self) -> Option<&Cap<IoUring>> {
        match &self.backing {
            OpenFileBacking::IoUring { ring } => Some(ring),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<PosixMqInstance>)` iff this `OpenFile` is a POSIX
    /// message-queue descriptor.
    pub fn posix_mq(&self) -> Option<&Cap<PosixMqInstance>> {
        match &self.backing {
            OpenFileBacking::PosixMq { mq } => Some(mq),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<ProcessIdentity>)` iff this `OpenFile` is a pidfd.
    pub fn pidfd_process(&self) -> Option<&Cap<ProcessIdentity>> {
        match &self.backing {
            OpenFileBacking::Pidfd { process } => Some(process),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&KernelObjectFile)` iff this `OpenFile` is a phase-0
    /// kernel-object fd.
    pub fn kernel_object(&self) -> Option<&KernelObjectFile> {
        match &self.backing {
            OpenFileBacking::KernelObject { object } => Some(object),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::MountApi { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// `Some(&Cap<MountApiFile>)` iff this `OpenFile` is a Linux new
    /// mount API fd.
    pub fn mount_api_file(&self) -> Option<&Cap<MountApiFile>> {
        match &self.backing {
            OpenFileBacking::MountApi { file } => Some(file),
            OpenFileBacking::Rnode { .. }
            | OpenFileBacking::Ufd { .. }
            | OpenFileBacking::AioContext { .. }
            | OpenFileBacking::SignalFd { .. }
            | OpenFileBacking::Epoll { .. }
            | OpenFileBacking::Eventfd { .. }
            | OpenFileBacking::Timerfd { .. }
            | OpenFileBacking::IoUring { .. }
            | OpenFileBacking::PosixMq { .. }
            | OpenFileBacking::Pidfd { .. }
            | OpenFileBacking::KernelObject { .. }
            | OpenFileBacking::SocketPair { .. } => None,
        }
    }

    /// Load the current per-fd offset.
    ///
    /// `Acquire` paired with the `Release` store in `set_offset` /
    /// `advance_offset` so a thread that sees a fresh offset value
    /// also sees any page-cache state writes the previous I/O step
    /// committed before bumping the offset.
    pub fn offset(&self) -> u64 {
        self.offset.load(Ordering::Acquire)
    }

    /// Replace the offset with `offset`. Used by `lseek(2)` and by
    /// the page-backed I/O lanes' `step_range` finaliser when the
    /// step ran to completion or stopped early on a partial blocked /
    /// errored result.
    pub fn set_offset(&self, offset: u64) {
        self.offset.store(offset, Ordering::Release);
    }

    /// Bump the offset by `delta`, returning the **new** value.
    ///
    /// Wraps `AtomicU64::fetch_add` (which yields the *old* value);
    /// the page-backed step bodies never need the old value, only
    /// the post-bump cursor.
    pub fn advance_offset(&self, delta: u64) -> u64 {
        // fetch_add returns old; the new value is `old + delta`.
        self.offset.fetch_add(delta, Ordering::AcqRel) + delta
    }

    pub fn flags(&self) -> OpenFileFlags {
        let mut f = self.flags;
        match self.nonblocking_override.load(Ordering::Acquire) {
            0 => f.nonblocking = false,
            1 => f.nonblocking = true,
            _ => {}
        }
        match self.packet_override.load(Ordering::Acquire) {
            0 => f.packet = false,
            1 => f.packet = true,
            _ => {}
        }
        f
    }

    pub fn set_nonblocking(&self, val: bool) {
        self.nonblocking_override
            .store(if val { 1 } else { 0 }, Ordering::Release);
    }

    pub fn set_packet_mode(&self, val: bool) {
        self.packet_override
            .store(if val { 1 } else { 0 }, Ordering::Release);
    }

    /// Snapshot the per-fd readdir cursor.
    ///
    /// Slice 6: `getdents64(2)` consumes this at the start of each
    /// call and writes the post-batch value back via
    /// [`Self::set_readdir_cursor`] so the next call resumes where the
    /// previous one left off. `Acquire` paired with the `Release`
    /// store in `set_readdir_cursor` matches the offset/lseek
    /// discipline.
    pub fn readdir_cursor(&self) -> DirCursor {
        DirCursor::from_u64(self.readdir_cursor.load(Ordering::Acquire))
    }

    /// Replace the readdir cursor with `cursor`.
    ///
    /// Cap clone (`dup` / `fork`) shares the cell, so concurrent
    /// `getdents64` against the same OpenFile via different fds
    /// observes the shared "file description" cursor — matches
    /// Linux's per-file-description directory stream semantic.
    pub fn set_readdir_cursor(&self, cursor: DirCursor) {
        self.readdir_cursor
            .store(cursor.as_u64(), Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OpenFileIoctlCaller<'a> {
    process: &'a Cap<ProcessIdentity>,
}

impl<'a> OpenFileIoctlCaller<'a> {
    pub const fn from_process(process: &'a Cap<ProcessIdentity>) -> Self {
        Self { process }
    }

    pub const fn process(self) -> &'a Cap<ProcessIdentity> {
        self.process
    }
}
