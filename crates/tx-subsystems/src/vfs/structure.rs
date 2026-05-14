//! VFS structure: SSoT data types and zone-managed live nodes.
//!
//! This module hosts the value vocabulary the rest of the VFS surface
//! consumes — names, ids, metadata, directory cursors — plus the three
//! zone-allocated entities (`DEntry`, `RNode`, `OpenFile`) and their
//! constructors/accessors. Per `SUBSYSTEM_ANATOMY_v2_1` §structure,
//! mutation step bodies live in `execution.rs`; only constructors and
//! pure observation helpers belong here.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::vfs::adapter::step_engine::{self, Cap, Weak, Zone, ZoneAllocated, ZoneError};
use crate::vfs::adapter::wait_routing::{self, Channel, WaitSource};

use crate::aio::AioContext;
use crate::cred::{CapabilitySet, Cred};
use crate::device::{BlockDeviceRegistration, CharDeviceBinding};
use crate::execution::Errno;
use crate::io_uring::IoUring;
use crate::mount::{MountIdentity, MountPayload};
use crate::page_backed::PageContainer;
use crate::process::{ProcessGroup, ProcessIdentity};
use crate::signalfd::SignalFd;
use crate::tty::execution::IoctlSideEffect;
use crate::tty::structure::TtyIdentity;
use crate::tty::structure::{Termios, Winsize};
use crate::userfaultfd::UserfaultFd;
use crate::wait_source;

pub const VFS_NAME_MAX: usize = 255;

/// Per-RNode wait-source interest mask: bytes are available to read.
///
/// PR-3D-5: VFS lands the per-inode read/write wait-source primitive
/// in the same shape pipe/tty/exit_source use: one `Arc<WaitSource>`
/// per direction per inode, sharing the legacy `wait_source` registry
/// `u64` namespace with a paired reactor `Channel`. The bit lives on
/// VFS rather than on a backing because the wake-publication shape is
/// VFS-uniform (every inode has read/write semantics with the same
/// blocking-IO contract); per-backing helpers (`pipe`, `tty`, future
/// `socket`) layer their own bit allocations on top of this shape if
/// they need them.
pub const VFS_READABLE: u64 = 0x1;

/// Per-RNode wait-source interest mask: space is available to write.
/// See [`VFS_READABLE`] for the namespace + lifecycle convention.
pub const VFS_WRITABLE: u64 = 0x2;

// === zone statics =====================================================

static DENTRY_ZONE: Zone<DEntry> = Zone::const_new();
static RNODE_ZONE: Zone<RNode> = Zone::const_new();
static OPEN_FILE_ZONE: Zone<OpenFile> = Zone::const_new();

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

// === namespace identifiers =============================================

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FsObjectId(u64);

impl FsObjectId {
    pub const ROOT: Self = Self(1);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
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
// setuid slice's chmod/chown bookkeeping (e.g. `step_chown` silently
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
    Projected,
}

#[derive(Clone, Debug)]
pub enum StructPayload {
    Tty(Cap<TtyIdentity>),
    CharDevice(&'static CharDeviceBinding),
    BlockDevice(&'static BlockDeviceRegistration),
    /// Anonymous pipe — `pipe2(2)`. `side` distinguishes the
    /// reader-end RNode from the writer-end RNode; both share a
    /// single `Cap<PipePayload>`. fd-ops Wave 3.
    Pipe {
        payload: Cap<crate::pipe::PipePayload>,
        side: crate::pipe::PipeSide,
    },
}

// === live-node entities ===============================================

pub struct RNode {
    fs_object_id: FsObjectId,
    meta: InodeMeta,
    backing: RNodeBacking,
    containing_mount: Option<Weak<MountPayload>>,
    /// Legacy reactor wait channel paired with [`Self::read_wait_source`].
    /// Fires on every per-inode "newly readable" transition. PR-3D-5
    /// (D2/D4 coexistence): callers landing v3 blocking-IO step bodies
    /// against a regular-file / future-socket RNode park on the matching
    /// `WaitToken` via [`crate::wait_source::wait_on_token`] resolved
    /// through [`Self::read_wait_source_id`]; the parallel
    /// `WaitSource` path is fired on the same transition so v3 callers
    /// holding a `TaskMailbox` see the same wake.
    ///
    /// Today no in-tree fire site exists for non-pipe/non-tty backings
    /// (pipe + tty manage their own wait channels on the backing
    /// payload); this slot is the durable wake-publication endpoint for
    /// future page-backed-blocking, socket, and `inotify` wires per
    /// the per-inode unbounded-count flag W-M raised. The Drop impl
    /// releases the registry slot when the inode retires (EBR), so the
    /// large-N inode-create-destroy stress path does not leak registry
    /// rows.
    read_wait_channel: Channel,
    /// Legacy registry carrier id for [`Self::read_wait_channel`].
    /// Shares the `u64` namespace with [`Self::read_wait_source`]'s
    /// `WaitSourceId` so a v3 caller's `YieldShape::OnWaitSource
    /// { source: WaitSourceId(id), .. }` resolves to this same slot.
    read_wait_source_id: u64,
    /// PR-3D-5 (D2/D4 coexistence). Per-inode `WaitSource` for the new
    /// mailbox-based wake path, fired in parallel with
    /// [`Self::read_wait_channel`] on every "newly readable" transition.
    /// Shape mirrors `Cap<RNode>`'s EBR semantics — the source is held
    /// by `Arc<WaitSource>` on the RNode, so subscribers cloning the
    /// strong ref retain it across the wait window independent of inode
    /// retirement; once the inode drops, the registry slot is released
    /// (see `Drop for RNode`) and no further notifies arrive.
    read_wait_source: Arc<WaitSource>,
    /// Companion to [`Self::read_wait_channel`] for the writable
    /// direction. Fires on every "newly writable" transition (space
    /// available in a future socket / page-backed-blocking ring,
    /// reader-closed-EPIPE on a future socket reset path, etc.).
    write_wait_channel: Channel,
    /// Companion to [`Self::read_wait_source_id`] for the writable
    /// direction.
    write_wait_source_id: u64,
    /// Companion to [`Self::read_wait_source`] for the writable
    /// direction.
    write_wait_source: Arc<WaitSource>,
}

impl RNode {
    pub fn new(fs_object_id: FsObjectId, meta: InodeMeta, backing: RNodeBacking) -> Self {
        let read_wait_channel = Channel::new();
        let read_wait_source_id = wait_source::register_wait_channel(read_wait_channel.clone());
        let read_wait_source = wait_routing::new_wait_source(read_wait_source_id);
        let write_wait_channel = Channel::new();
        let write_wait_source_id = wait_source::register_wait_channel(write_wait_channel.clone());
        let write_wait_source = wait_routing::new_wait_source(write_wait_source_id);
        Self {
            fs_object_id,
            meta,
            backing,
            containing_mount: None,
            read_wait_channel,
            read_wait_source_id,
            read_wait_source,
            write_wait_channel,
            write_wait_source_id,
            write_wait_source,
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

    pub fn with_containing_mount(mut self, mount: &Cap<MountPayload>) -> Self {
        self.containing_mount = Some(mount.downgrade());
        self
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

    /// Legacy reactor `Channel` paired with [`Self::read_wait_source`].
    /// Production callers fire this and the new `WaitSource` in tandem
    /// from the per-inode "newly readable" transition site (see module
    /// docs); v3 callers awaiting via
    /// [`crate::wait_source::wait_on_token`] resolve the carrier id from
    /// [`Self::read_wait_source_id`].
    pub fn read_wait_channel(&self) -> &Channel {
        &self.read_wait_channel
    }

    /// Legacy registry carrier id for [`Self::read_wait_channel`].
    /// Shares the same `u64` with [`Self::read_wait_source`]'s
    /// `WaitSourceId` so a v3 caller's `YieldShape::OnWaitSource
    /// { source: WaitSourceId(id), .. }` resolves to this same slot.
    pub fn read_wait_source_id(&self) -> u64 {
        self.read_wait_source_id
    }

    /// PR-3D-5: per-inode `WaitSource` for the new mailbox-based wake
    /// path. Returned as `&Arc<WaitSource>` so callers can clone the
    /// strong ref and hold the source alive across the wait window via
    /// `WaitSource::prepare(..).install_if(..)` independent of the
    /// inode's EBR retirement. `WaitSource::id()` matches
    /// [`Self::read_wait_source_id`].
    pub fn read_wait_source(&self) -> &Arc<WaitSource> {
        &self.read_wait_source
    }

    /// Companion to [`Self::read_wait_channel`] for the writable
    /// direction.
    pub fn write_wait_channel(&self) -> &Channel {
        &self.write_wait_channel
    }

    /// Companion to [`Self::read_wait_source_id`] for the writable
    /// direction.
    pub fn write_wait_source_id(&self) -> u64 {
        self.write_wait_source_id
    }

    /// Companion to [`Self::read_wait_source`] for the writable
    /// direction.
    pub fn write_wait_source(&self) -> &Arc<WaitSource> {
        &self.write_wait_source
    }

    /// Fire the per-inode read wake path on both the legacy `Channel`
    /// and the new `WaitSource` (PR-3D-5 D2/D4 coexistence). Pass the
    /// interest bits the transition signals — today that is
    /// [`VFS_READABLE`] for the single-bit "bytes available" semantic;
    /// future per-backing wires (e.g. socket urgent-data, future
    /// `inotify`) may carry additional bits on the same source.
    ///
    /// Returns the number of legacy `Channel` awaiters released by
    /// [`Channel::fire`]. The `WaitSource::notify` count is intentionally
    /// not surfaced — production callers don't branch on it, and the
    /// dual-fire happens unconditionally under the same call (so
    /// either both paths fire or neither does, matching the
    /// exit_source / tty templates).
    pub fn fire_read_wait(&self, mask: u64) -> usize {
        let released = wait_routing::fire_legacy_channel(&self.read_wait_channel, mask);
        wait_routing::notify_v3_source(&self.read_wait_source, mask);
        released
    }

    /// Companion to [`Self::fire_read_wait`] for the writable direction.
    pub fn fire_write_wait(&self, mask: u64) -> usize {
        let released = wait_routing::fire_legacy_channel(&self.write_wait_channel, mask);
        wait_routing::notify_v3_source(&self.write_wait_source, mask);
        released
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
        wait_source::release_wait_channel(self.read_wait_source_id);
        wait_source::release_wait_channel(self.write_wait_source_id);
    }
}

// PR-3D-5: `Channel` and `WaitSource` are not `Debug`, so the previous
// `#[derive(Debug)]` on `RNode` cannot survive after the wait-source
// fields land. The manual impl below preserves the previously-derived
// shape (one entry per kept field) and elides the wake-publication
// internals (which would be noisy and carry no value for debug
// output — observers care about the inode identity / metadata, not
// the registered subscriber list).
impl core::fmt::Debug for RNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RNode")
            .field("fs_object_id", &self.fs_object_id)
            .field("meta", &self.meta)
            .field("backing", &self.backing)
            .field("containing_mount", &self.containing_mount)
            .field("read_wait_source_id", &self.read_wait_source_id)
            .field("write_wait_source_id", &self.write_wait_source_id)
            .finish()
    }
}

#[derive(Debug)]
pub struct DEntry {
    name: InlineName,
    parent: Option<Cap<DEntry>>,
    rnode: Cap<RNode>,
    mounted: Option<Weak<MountIdentity>>,
}

impl DEntry {
    pub fn new(name: InlineName, rnode: Cap<RNode>) -> Self {
        Self {
            name,
            parent: None,
            rnode,
            mounted: None,
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

    pub fn set_parent_hint(&mut self, parent: &Cap<DEntry>) {
        self.parent = Some(parent.clone());
    }

    pub fn set_mounted_hint(&mut self, mount: &Cap<MountIdentity>) {
        self.mounted = Some(mount.downgrade());
    }

    /// Return the parent-hint `Cap<DEntry>` if installed. The parent is held
    /// by strong reference so the chain remains valid after any `chdir`.
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
    let mut components: alloc::vec::Vec<InlineName> = alloc::vec::Vec::new();
    components.push(dentry.name());

    let mut current = dentry.parent_hint();
    while let Some(parent_cap) = current {
        components.push(parent_cap.name());
        current = parent_cap.parent_hint();
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
    /// drive `DelegateRegistry::mark_endpoint_died`.
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
    /// `step_kill_process` calls no longer route to it.
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
    nonblocking_override: AtomicBool,
}

impl OpenFile {
    pub fn new(rnode: Cap<RNode>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::Rnode { rnode },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicBool::new(false),
            flags,
        }
    }

    pub fn new_cap(rnode: Cap<RNode>, flags: OpenFileFlags) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new(rnode, flags))
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
            nonblocking_override: AtomicBool::new(false),
            flags,
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
    /// (joining ufd in the OpenFileBacking enum, see D8 §4.1).
    /// Existing VFS-only paths (`step_read` / `step_write` /
    /// `step_lseek` / etc.) must not be called against this shape;
    /// callers branch via [`Self::backing`] / [`Self::aio_context`].
    pub fn new_aio_context(ctx: Cap<AioContext>, flags: OpenFileFlags) -> Self {
        Self {
            backing: OpenFileBacking::AioContext { ctx },
            offset: AtomicU64::new(0),
            readdir_cursor: AtomicU64::new(0),
            nonblocking_override: AtomicBool::new(false),
            flags,
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
            nonblocking_override: AtomicBool::new(false),
            flags,
        }
    }

    /// Zone-sign a fresh signalfd-backed `OpenFile` (D9-D).
    pub fn new_signalfd_cap(
        sfd: Cap<SignalFd>,
        flags: OpenFileFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        step_engine::sign(Self::new_signalfd(sfd, flags))
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
            nonblocking_override: AtomicBool::new(false),
            flags,
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
            OpenFileBacking::IoUring { .. } => panic!(
                "OpenFile::rnode() called on an io_uring-backed OpenFile; \
                 dispatch via OpenFile::backing() / OpenFile::io_uring() first",
            ),
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
            | OpenFileBacking::IoUring { .. } => None,
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
            | OpenFileBacking::IoUring { .. } => None,
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
            | OpenFileBacking::IoUring { .. } => None,
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
            | OpenFileBacking::SignalFd { .. } => None,
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
        if self.nonblocking_override.load(Ordering::Acquire) {
            f.nonblocking = true;
        }
        f
    }

    pub fn set_nonblocking(&self, val: bool) {
        self.nonblocking_override.store(val, Ordering::Release);
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

/// Pipe-side lifecycle hook (shell-prompt roadmap Slice 1).
///
/// `Cap<OpenFile>` is refcounted via the zone-substrate machinery; the
/// inner `OpenFile` value drops exactly once, when the last `Cap`
/// referencing it is released and EBR fires the slot reclamation
/// callback. That single-shot guarantee is what makes a per-side
/// reader/writer count against `PipePayload` correct without an
/// explicit hook on every `sys_close` / `sys_dup3`-replace / fork-CLOEXEC
/// / exit-cleanup path: each fd-slot drop releases one `Cap`, and only
/// the *last* such drop reaches this destructor.
///
/// The behaviour is keyed on `RNodeBacking::StructBacked { payload:
/// StructPayload::Pipe { side, .. } }`; non-pipe backings have no
/// per-OpenFile lifecycle (page-backed inodes own their own page
/// containers; tty/chardev RNodes outlive any OpenFile referencing
/// them).
///
/// On the *last-reader-close* transition `decr_reader` fires the
/// writer-side wait channel so any blocked writer surfaces SIGPIPE/
/// EPIPE. On the *last-writer-close* transition `decr_writer` fires
/// the reader-side wait channel so any blocked reader surfaces EOF
/// (`Done(0)`). Both transitions are owned by `pipe::PipePayload`'s
/// `decr_*` helpers.
impl Drop for OpenFile {
    fn drop(&mut self) {
        // PR-10 phase 0: only the RNode-backed shape carries the
        // pipe lifecycle hook. Userfaultfd-backed OpenFiles have no
        // per-side ref count to decrement — their inner
        // `Cap<UserfaultFd>` drops via the normal `OpenFileBacking::Ufd`
        // field drop and EBR reclamation of the ufd zone slot follows.
        if let OpenFileBacking::Rnode { rnode } = &self.backing {
            if let RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, side },
            } = rnode.backing()
            {
                match side {
                    crate::pipe::PipeSide::Reader => payload.decr_reader(),
                    crate::pipe::PipeSide::Writer => payload.decr_writer(),
                }
            }
        }
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
