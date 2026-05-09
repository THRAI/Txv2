//! tmpfs — in-memory filesystem backed by anonymous PageContainers.
//!
//! Phase 3b deliverable. Provides the rootfs over which devfs is
//! mounted at `/dev` and the surface every subsequent VFS workload
//! lands on before a real on-disk filesystem exists.
//!
//! Active-doc anchors:
//! - `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`
//!   (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md`) — tmpfs is a
//!   distinct `FsOps` instance, never aliasing another mount's
//!   namespace.
//! - `txdoc:MOUNT-MOUNTPAYLOAD-1`,
//!   `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`
//!   (`docs/design/05_filesystem/MOUNT_v1.md`) — `MountOutput` shape
//!   tmpfs hands to `MountIdentity::new_cap`.
//! - `txdoc:PAGE-BACKED-ANON-1`
//!   (`docs/design/03_memory-vm/PAGE_BACKED_v1.md`) — regular files
//!   use `PageContainerKind::Anon { swap_policy: Reclaimable }`,
//!   reclaim-eligible per spec.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_substrate::zone::Cap;
use tx_substrate::SpinMutex;
use tx_subsystems::cred::Capability;
use tx_subsystems::execution::{Errno, Guard, StepOutcome};
use tx_subsystems::page_backed::{
    step_truncate, AnonSwapPolicy, Frame, FsPageBacking, MaterializeAccess, PageContainer,
    PageContainerKind, PageIndex,
};
use tx_subsystems::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InlineName, InodeKind, InodeMeta,
    MountOutput, RNode, RNodeBacking, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, S_ISGID, S_ISUID,
    VFS_NAME_MAX,
};

/// Mode for the tmpfs root directory.
pub const TMPFS_ROOT_MODE: u16 = S_IFDIR | 0o755;

/// `FsObjectId::ROOT == 1` is the reserved sentinel; tmpfs hands out
/// 2 onward (root inode is 2). See plan §"tmpfs FsOps surface".
pub const TMPFS_ROOT_OBJECT_ID: FsObjectId = FsObjectId::new(2);

/// First object-id available for non-root tmpfs allocations.
const TMPFS_FIRST_FREE_OBJECT_ID: u64 = 3;

/// Maximum page count a tmpfs regular file can grow to. Phase 3b's
/// `PageContainer::new` requires a fixed `page_count` capacity at
/// allocation time (see `PageContainer::check_bounds`); tmpfs files
/// are created with this cap and `size_bytes` grows lazily through
/// `step_write` / `step_truncate`. 4 MiB ÷ 4 KiB pages is enough for
/// every Phase 3b workload (init's preopened fds plus the test fixtures);
/// raising the cap is a backward-compatible follow-up once a sparse
/// page-count growth shape lands.
// TODO(phase-vfs-tmpfs-grow): teach `PageContainer` to grow `page_count`
// on demand so tmpfs files are bounded only by global swap pressure
// rather than by this static cap.
const TMPFS_FILE_PAGE_CAP: u64 = 1024;

/// Maximum length of an inline symlink target, in bytes.
///
/// tmpfs stores symlink targets inline; the cap matches `VFS_NAME_MAX`
/// since any longer target would not fit through `lookup`'s `&[u8]`
/// component anyway. Per the plan §"FsOps::symlink".
pub const TMPFS_SYMLINK_MAX: usize = VFS_NAME_MAX;

/// Per-inode payload. Variants line up with the inode kinds tmpfs
/// supports today. Block-device, fifo, and socket variants are not
/// yet in scope; `create_inode` rejects those modes with `EINVAL`.
enum TmpfsPayload {
    /// Directory: child name → inode id.
    Directory(BTreeMap<InlineName, FsObjectId>),
    /// Regular file: anon `PageContainer` plus the visible byte size.
    /// `size` tracks the externally-visible size (POSIX `st_size`),
    /// which is what `serialize_inode_meta` and `truncate` mutate.
    RegularFile {
        container: Cap<PageContainer>,
        size: u64,
    },
    /// Symlink: target bytes stored inline. The bytes are observed
    /// through `readlink`-style paths (deferred — Phase 3b only
    /// surfaces creation), so the variant currently appears unread.
    #[cfg_attr(test, allow(dead_code))]
    Symlink(Vec<u8>),
}

/// One inode entry. Stored inside `TmpfsState::inodes`, keyed by id.
struct TmpfsInode {
    meta: InodeMeta,
    payload: TmpfsPayload,
}

struct TmpfsState {
    inodes: BTreeMap<FsObjectId, TmpfsInode>,
}

impl TmpfsState {
    fn new() -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            TMPFS_ROOT_OBJECT_ID,
            TmpfsInode {
                meta: InodeMeta::new(InodeKind::Directory, TMPFS_ROOT_MODE),
                payload: TmpfsPayload::Directory(BTreeMap::new()),
            },
        );
        Self { inodes }
    }
}

/// In-memory tmpfs backend.
///
/// Holds the inode table plus a monotonic id allocator. One instance
/// per mount; the `ROOT_MOUNT` slot in `tx-kernel`'s init keeps a
/// strong `Arc<dyn FsOps>` and `Arc<dyn FsPageBacking>` against the
/// same `Tmpfs` so both trait objects observe the same state.
pub struct Tmpfs {
    state: SpinMutex<TmpfsState>,
    next_object_id: AtomicU64,
}

impl Tmpfs {
    pub fn new() -> Self {
        Self {
            state: SpinMutex::new(TmpfsState::new()),
            next_object_id: AtomicU64::new(TMPFS_FIRST_FREE_OBJECT_ID),
        }
    }

    /// Convenience factory matching the `MountOutput` shape Phase 3b
    /// hands to `MountIdentity::new_cap`. Two factories rather than
    /// one `Arc<Self>` + two `Arc::clone` calls so the call site at
    /// `init.rs` reads identically to devfs's pattern.
    pub fn fs_ops_arc(self: Arc<Self>) -> Arc<dyn FsOps> {
        self
    }

    pub fn fs_page_backing_arc(self: Arc<Self>) -> Arc<dyn FsPageBacking> {
        self
    }

    /// Materialise the tmpfs root and return the `MountOutput` ready
    /// for `MountIdentity::new_cap`. Per the plan §"tmpfs root
    /// materialisation": `root_fs_object_id = FsObjectId::new(2)`,
    /// `root_inode_meta = <S_IFDIR | 0o755>`.
    ///
    /// Returns `(tmpfs, mount_output)` so the caller retains an
    /// `Arc<Tmpfs>` if it wants to consult the backend directly
    /// (tx-kernel uses this to `mkdir("/dev")` against the same
    /// instance the mount payload exposes).
    pub fn new_root() -> (Arc<Self>, MountOutput) {
        let tmpfs: Arc<Self> = Arc::new(Self::new());
        let fs_ops: Arc<dyn FsOps> = tmpfs.clone();
        let fs_page_backing: Arc<dyn FsPageBacking> = tmpfs.clone();
        // Wave 9c: populate the v3 sibling fields from the same
        // `Arc<Tmpfs>` via the `fs_ops_v3_arc` / `fs_page_backing_v3_arc`
        // factories landed in wave 9a. Both v3 fields are alongside the
        // v4 fields; the new walker entry points (`step_walk_v3` /
        // `step_open_v3`) consume `fs_ops_v3`.
        let fs_ops_v3 = tmpfs.clone().fs_ops_v3_arc();
        let fs_page_backing_v3 = tmpfs.clone().fs_page_backing_v3_arc();
        let output = MountOutput {
            fs_ops,
            fs_ops_v3,
            fs_page_backing,
            fs_page_backing_v3,
            root_fs_object_id: TMPFS_ROOT_OBJECT_ID,
            root_inode_meta: InodeMeta::new(InodeKind::Directory, TMPFS_ROOT_MODE),
        };
        (tmpfs, output)
    }

    fn alloc_object_id(&self) -> FsObjectId {
        let raw = self.next_object_id.fetch_add(1, Ordering::AcqRel);
        FsObjectId::new(raw)
    }
}

impl Default for Tmpfs {
    fn default() -> Self {
        Self::new()
    }
}

impl FsOps for Tmpfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let state = self.state.lock();
        let Some(parent_inode) = state.inodes.get(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        match children.get(&inline) {
            Some(id) => StepOutcome::Done(*id),
            None => StepOutcome::Err(Errno::ENOENT),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        let state = self.state.lock();
        match state.inodes.get(&fs_object_id) {
            Some(inode) => {
                let mut meta = inode.meta;
                if let TmpfsPayload::RegularFile { size, .. } = &inode.payload {
                    meta.size = *size;
                }
                StepOutcome::Done(meta)
            }
            None => StepOutcome::Err(Errno::ENOENT),
        }
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        let mut state = self.state.lock();
        let Some(inode) = state.inodes.get_mut(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        // Preserve the IFMT bits from the existing meta — the kind is
        // determined at create time and must not be mutated through
        // chmod/serialize. The spec rule
        // (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §inode-meta)
        // is that mode bits below S_IFMT are caller-mutable, but the
        // type bits are immutable.
        let existing_kind_bits = inode.meta.mode & S_IFMT;
        let new_meta = InodeMeta {
            mode: (meta.mode & !S_IFMT) | existing_kind_bits,
            ..*meta
        };
        inode.meta = new_meta;
        StepOutcome::Done(())
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        // Day-1 tmpfs only handles regular files via `create_inode`.
        // Directories arrive through `mkdir`, symlinks through
        // `symlink`. Reject anything else with `EINVAL`.
        let kind_bits = mode & S_IFMT;
        if kind_bits != 0 && kind_bits != S_IFREG {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let mode = (mode & !S_IFMT) | S_IFREG;

        let container = match PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            TMPFS_FILE_PAGE_CAP,
        ) {
            Ok(cap) => cap,
            Err(_) => return StepOutcome::Err(Errno::ENOMEM),
        };

        let new_id = self.alloc_object_id();
        let mut meta = InodeMeta::new(InodeKind::Regular, mode);
        meta.uid = cred.uid;
        meta.gid = cred.gid;
        meta.size = 0;

        let mut state = self.state.lock();
        let Some(parent_inode) = state.inodes.get_mut(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        if children.contains_key(&inline) {
            return StepOutcome::Err(Errno::EEXIST);
        }
        children.insert(inline, new_id);

        state.inodes.insert(
            new_id,
            TmpfsInode {
                meta,
                payload: TmpfsPayload::RegularFile { container, size: 0 },
            },
        );

        StepOutcome::Done((new_id, meta))
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let mut state = self.state.lock();
        let Some(parent_inode) = state.inodes.get_mut(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        let Some(found_id) = children.get(&inline).copied() else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        if found_id != target {
            return StepOutcome::Err(Errno::ENOENT);
        }
        // Reject directory targets — those go through `rmdir`.
        if let Some(target_inode) = state.inodes.get(&found_id) {
            if matches!(target_inode.payload, TmpfsPayload::Directory(_)) {
                return StepOutcome::Err(Errno::EISDIR);
            }
        }
        let parent_inode = state
            .inodes
            .get_mut(&parent)
            .expect("parent inode disappeared mid-unlink");
        if let TmpfsPayload::Directory(children) = &mut parent_inode.payload {
            children.remove(&inline);
        }
        state.inodes.remove(&found_id);
        StepOutcome::Done(())
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // TODO(phase-vfs-rename-xdir): cross-directory rename. Day-1
        // ships same-directory rename; cross-directory needs the
        // walker + dentry rebinding seam to land first.
        if old_parent != new_parent {
            return StepOutcome::Err(Errno::ENOSYS);
        }
        let old_key = match InlineName::new(old_name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let new_key = match InlineName::new(new_name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        if old_key == new_key {
            return StepOutcome::Done(());
        }

        let mut state = self.state.lock();
        let Some(parent_inode) = state.inodes.get_mut(&old_parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        let Some(target_id) = children.remove(&old_key) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        // If a file exists at the destination, replace it (POSIX
        // rename semantics for same-type-collision; cross-type
        // collision is left as a follow-up alongside cross-dir).
        let displaced = children.insert(new_key, target_id);
        if let Some(displaced_id) = displaced {
            state.inodes.remove(&displaced_id);
        }
        StepOutcome::Done(())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // Hard links are out of scope for Phase 3b; tmpfs day-1 maps
        // each child name to a single owning inode and refcounts via
        // the parent's BTreeMap. Adding `link` requires a full nlink
        // counter pass on `unlink`/`rmdir`/`destroy_inode`.
        // TODO(phase-vfs-tmpfs-link): implement when nlink semantics
        // land in the broader VFS layer.
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let mode = (mode & !S_IFMT) | S_IFDIR;
        let new_id = self.alloc_object_id();
        let mut meta = InodeMeta::new(InodeKind::Directory, mode);
        meta.uid = cred.uid;
        meta.gid = cred.gid;

        let mut state = self.state.lock();
        let Some(parent_inode) = state.inodes.get_mut(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        if children.contains_key(&inline) {
            return StepOutcome::Err(Errno::EEXIST);
        }
        children.insert(inline, new_id);

        state.inodes.insert(
            new_id,
            TmpfsInode {
                meta,
                payload: TmpfsPayload::Directory(BTreeMap::new()),
            },
        );

        StepOutcome::Done((new_id, meta))
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let mut state = self.state.lock();
        let Some(target_inode) = state.inodes.get(&target) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(target_children) = &target_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        if !target_children.is_empty() {
            return StepOutcome::Err(Errno::ENOTEMPTY);
        }

        let Some(parent_inode) = state.inodes.get_mut(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        let Some(found_id) = children.get(&inline).copied() else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        if found_id != target {
            return StepOutcome::Err(Errno::ENOENT);
        }
        children.remove(&inline);
        state.inodes.remove(&found_id);
        StepOutcome::Done(())
    }

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        if link_target.is_empty() || link_target.len() > TMPFS_SYMLINK_MAX {
            return StepOutcome::Err(Errno::ENAMETOOLONG);
        }
        // `InlineName::new` is the canonical name-validity check
        // (rejects empty, oversized, or `/` -bearing names). The
        // inline name is also the directory BTreeMap key now that
        // `InlineName: Ord` is upstream.
        let inline = match InlineName::new(name) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let new_id = self.alloc_object_id();
        let mut meta = InodeMeta::new(InodeKind::Symlink, S_IFLNK | 0o777);
        meta.uid = cred.uid;
        meta.gid = cred.gid;
        meta.size = link_target.len() as u64;

        let mut state = self.state.lock();
        let Some(parent_inode) = state.inodes.get_mut(&parent) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &mut parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        if children.contains_key(&inline) {
            return StepOutcome::Err(Errno::EEXIST);
        }
        children.insert(inline, new_id);

        let mut target = Vec::with_capacity(link_target.len());
        target.extend_from_slice(link_target);
        state.inodes.insert(
            new_id,
            TmpfsInode {
                meta,
                payload: TmpfsPayload::Symlink(target),
            },
        );

        StepOutcome::Done((new_id, meta))
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        let state = self.state.lock();
        let Some(parent_inode) = state.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let TmpfsPayload::Directory(children) = &parent_inode.payload else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        let index = cursor.as_u64() as usize;
        let Some((name, child_id)) = children.iter().nth(index) else {
            return StepOutcome::Done(None);
        };
        // Resolve child kind for the DirEntry by peeking the child
        // inode's meta. Falls back to Regular if the child is missing
        // (a state inconsistency we shouldn't observe in practice).
        let kind = state
            .inodes
            .get(child_id)
            .map(|child| child.meta.kind())
            .unwrap_or(InodeKind::Regular);
        let entry = match DirEntry::new(*child_id, kind, name.as_bytes()) {
            Ok(e) => e,
            Err(err) => return StepOutcome::Err(err),
        };
        StepOutcome::Done(Some((entry, DirCursor::from_u64(cursor.as_u64() + 1))))
    }

    fn destroy_inode(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        // The plan calls for `destroy_inode` to drop the inode entry;
        // the regular-file `Cap<PageContainer>` falls when the
        // owning `TmpfsInode` is dropped, releasing all anon pages
        // through the standard PageContainer drop path. `unlink` /
        // `rmdir` already remove the inode in the same step they
        // unhook the dentry; the VFS layer also calls `destroy_inode`
        // when the last RNode reference falls. Treat a missing entry
        // as a successful no-op so the upper layer can drop without
        // observing an error.
        let mut state = self.state.lock();
        state.inodes.remove(&fs_object_id);
        StepOutcome::Done(())
    }

    /// Read the inline symlink target bytes for a tmpfs symlink inode.
    ///
    /// Used by the VFS walker to materialise an
    /// `RNodeBacking::Symlink { target }` so the resolution loop can
    /// substitute the target into the remaining component stream
    /// (`txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`). Returns `EINVAL` when
    /// invoked against a non-symlink inode (POSIX `readlink(2)` shape).
    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>> {
        let state = self.state.lock();
        let Some(inode) = state.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        match &inode.payload {
            TmpfsPayload::Symlink(bytes) => StepOutcome::Done(bytes.clone().into_boxed_slice()),
            _ => StepOutcome::Err(Errno::EINVAL),
        }
    }

    /// Materialise a `Cap<RNode>` for a non-directory, non-symlink
    /// tmpfs inode.
    ///
    /// Pre-ELF Phase 6 (item 7) introduced the
    /// `FsOps::materialise_rnode` hook with a default `ENOSYS`
    /// implementation; devfs already overrides for `CharDevice →
    /// StructBacked { Tty }`. Phase 7 of the ELF-loader plan adds the
    /// tmpfs override so the VFS walker can resolve regular files
    /// (e.g. `/init`) to a `RNodeBacking::PageBacked { pc }` over the
    /// inode's existing `Cap<PageContainer>`. Without this override
    /// the walker emits `ENOSYS` at the terminal regular-file
    /// component, and `exec_script` cannot reach a page-backed view
    /// of the file's bytes.
    ///
    /// Backing returned per inode kind:
    /// - `Regular` → `RNodeBacking::PageBacked { pc: container.clone() }`.
    ///   The container is the same `Cap<PageContainer>` constructed
    ///   in `create_inode`; cloning the Cap shares the page-backing
    ///   between the file's RNode and its inode payload, so writes
    ///   through the FsPageBacking surface and reads through
    ///   `OpenFile::step_read` (or `read_exact_at` from
    ///   `exec_script`) observe the same pages.
    /// - `Directory` and `Symlink` are handled inline by the walker
    ///   (`materialise_child_rnode` in
    ///   `crates/tx-subsystems/src/vfs/walker.rs`); reaching this
    ///   arm with one of those kinds is a backend bug. Return
    ///   `EISDIR` / `EINVAL` respectively to mirror the Linux
    ///   `inode_operations.lookup` shape.
    /// - Block / FIFO / Socket: not supported by tmpfs today; return
    ///   `ENOSYS` so callers fall through cleanly until those kinds
    ///   acquire concrete materialisers.
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>> {
        let state = self.state.lock();
        let Some(inode) = state.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let backing = match &inode.payload {
            TmpfsPayload::RegularFile { container, .. } => RNodeBacking::PageBacked {
                pc: container.clone(),
            },
            // The walker handles Directory and Symlink inline; this
            // arm should not be reached for those kinds. Return a
            // POSIX-shaped errno rather than panicking so a backend
            // misroute surfaces as a recoverable error.
            TmpfsPayload::Directory(_) => return StepOutcome::Err(Errno::EISDIR),
            TmpfsPayload::Symlink(_) => return StepOutcome::Err(Errno::EINVAL),
        };
        drop(state);

        match RNode::new_cap(fs_object_id, meta, backing) {
            Ok(rnode) => StepOutcome::Done(rnode),
            Err(_) => StepOutcome::Err(Errno::ENOMEM),
        }
    }

    /// Update the inode's mode bits. Caller must be the inode owner
    /// or carry `CAP_FOWNER`. Preserves `S_IFMT` (file kind is set
    /// at creation and immutable through chmod). Per the DAC +
    /// setuid plan §"FsOps::step_chmod / step_chown".
    fn step_chmod(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        let mut state = self.state.lock();
        let Some(inode) = state.inodes.get_mut(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        // Permission: owner or CAP_FOWNER.
        if !cred.effective_caps.contains(Capability::FOWNER) && cred.uid != inode.meta.uid {
            return StepOutcome::Err(Errno::EPERM);
        }
        // Preserve the IFMT bits from the existing meta — kind is
        // immutable through chmod (matches `serialize_inode_meta`).
        // Mask the request to the file mode bits the slice supports
        // (S_ISUID | S_ISGID | S_ISVTX | rwxrwxrwx = 0o7777).
        let masked = new_mode & 0o7777;
        let kind_bits = inode.meta.mode & S_IFMT;
        inode.meta.mode = kind_bits | masked;
        StepOutcome::Done(())
    }

    /// Update the inode's `(uid, gid)`. `None` for either field
    /// leaves it unchanged. Privilege rule: only `CAP_FOWNER` grants
    /// arbitrary changes; non-privileged callers may chown only to
    /// their own uid/gid. Linux's silent-clear-`S_ISUID`/`S_ISGID`
    /// rule applies for non-privileged callers (matches LTP
    /// `chown03`). Per the DAC + setuid plan §"FsOps::step_chmod /
    /// step_chown".
    fn step_chown(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        let mut state = self.state.lock();
        let Some(inode) = state.inodes.get_mut(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let privileged = cred.effective_caps.contains(Capability::FOWNER);
        if !privileged {
            if let Some(u) = new_uid {
                if u != cred.uid {
                    return StepOutcome::Err(Errno::EPERM);
                }
            }
            if let Some(g) = new_gid {
                if g != cred.gid {
                    return StepOutcome::Err(Errno::EPERM);
                }
            }
        }
        if let Some(u) = new_uid {
            inode.meta.uid = u;
        }
        if let Some(g) = new_gid {
            inode.meta.gid = g;
        }
        // Linux clears S_ISUID / S_ISGID on chown by non-privileged
        // callers to prevent privilege-escalation via setuid binary
        // ownership shifts. Slice mirrors LTP `chown03`'s rule.
        if !privileged {
            inode.meta.mode &= !(S_ISUID | S_ISGID);
        }
        StepOutcome::Done(())
    }
}

impl FsPageBacking for Tmpfs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame> {
        let state = self.state.lock();
        let Some(inode) = state.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let container = match &inode.payload {
            TmpfsPayload::RegularFile { container, .. } => container.clone(),
            TmpfsPayload::Directory(_) => return StepOutcome::Err(Errno::EISDIR),
            TmpfsPayload::Symlink(_) => return StepOutcome::Err(Errno::EINVAL),
        };
        drop(state);

        let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
        if !offset.is_multiple_of(page_size) {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let page_index = PageIndex::new(offset / page_size);
        match container.materialize_page(page_index, MaterializeAccess::Read, guard) {
            StepOutcome::Done(materialized) => StepOutcome::Done(Frame::new(materialized.ppn)),
            StepOutcome::Advanced(materialized) => {
                StepOutcome::Advanced(Frame::new(materialized.ppn))
            }
            StepOutcome::AdvancedThenBlocked(materialized, token) => {
                StepOutcome::AdvancedThenBlocked(Frame::new(materialized.ppn), token)
            }
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
        }
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // tmpfs is in-memory: there is no underlying durable store to
        // sync. Returning `Done(())` short-circuits `step_fsync`'s
        // dirty-page walk to a no-op, per the plan §"FsPageBacking".
        StepOutcome::Done(())
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        // Snapshot the container under the state lock, then call
        // `step_truncate` outside it: the page-backed lifecycle path
        // takes its own internal lock, and we must not stack lock
        // domains.
        let container = {
            let state = self.state.lock();
            let Some(inode) = state.inodes.get(&fs_object_id) else {
                return StepOutcome::Err(Errno::ENOENT);
            };
            match &inode.payload {
                TmpfsPayload::RegularFile { container, .. } => container.clone(),
                TmpfsPayload::Directory(_) => return StepOutcome::Err(Errno::EISDIR),
                TmpfsPayload::Symlink(_) => return StepOutcome::Err(Errno::EINVAL),
            }
        };

        match step_truncate(&container, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
            StepOutcome::Blocked(token) => return StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked((), token) => {
                return StepOutcome::AdvancedThenBlocked((), token);
            }
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
        }

        // Update the visible size in the inode payload + meta. The
        // `meta.size` field is recomputed from `payload.size` on every
        // `load_inode_meta`, so updating the payload is sufficient,
        // but we also normalize the cached meta for any caller that
        // reads `inode.meta` directly.
        let mut state = self.state.lock();
        if let Some(inode) = state.inodes.get_mut(&fs_object_id) {
            if let TmpfsPayload::RegularFile { size, .. } = &mut inode.payload {
                *size = new_size;
            }
            inode.meta.size = new_size;
        }
        StepOutcome::Done(())
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        // In-memory; durability is trivially satisfied.
        StepOutcome::Done(())
    }
}

// === Wave 9a: parallel v3 trait impls ==================================
//
// `impl FsOpsV3 for Tmpfs` and `impl FsPageBackingV3 for Tmpfs` mirror
// the v4 bodies above one-for-one. Tmpfs is one-shot through every
// method (no v4 `Advanced(t)` is ever returned by the v4 bodies, so
// the v3 mapping has zero ambiguous Continue-vs-Done call sites).
// Per the wave-8 design doc
// (`docs/progress/decisions/2026-05-09-fsops-v3-design.md`), v3 callers
// (the new walker entry points landing in wave 9b) opt into these
// impls via `Arc<dyn FsOpsV3>` / `Arc<dyn FsPageBackingV3>` once
// `MountPayload` grows the sibling fields. Tmpfs grows `fs_ops_v3_arc`
// / `fs_page_backing_v3_arc` factories alongside the existing
// `fs_ops_arc` / `fs_page_backing_arc` so backend wiring is a one-liner.
//
// Fully-qualified `tx_substrate::step_v3::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::StepOutcome`
// already in scope, per the wave-4/6/7 trait-impl convention.

use tx_subsystems::page_backed::FsPageBackingV3;
use tx_subsystems::vfs::FsOpsV3;

impl Tmpfs {
    /// v3 sibling of [`Tmpfs::fs_ops_arc`]. Wave 9b walker entry points
    /// will populate `MountPayload::fs_ops_v3` from this constructor.
    pub fn fs_ops_v3_arc(self: Arc<Self>) -> Arc<dyn FsOpsV3> {
        self
    }

    /// v3 sibling of [`Tmpfs::fs_page_backing_arc`].
    pub fn fs_page_backing_v3_arc(self: Arc<Self>) -> Arc<dyn FsPageBackingV3> {
        self
    }
}

impl FsOpsV3 for Tmpfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::lookup(self, parent, name, guard) {
            StepOutcome::Done(id) => tx_substrate::step_v3::StepOutcome::done(id),
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
            // Tmpfs's v4 `lookup` body is purely synchronous (no
            // `Advanced` / `Blocked` returns), so these arms are
            // unreachable in practice. Map them to terminal v3
            // outcomes: `Advanced(id)` → `done(id)` (one-shot trait,
            // surface is "lookup ran to completion"); `Blocked` /
            // `AdvancedThenBlocked` map to `EAGAIN` defensively
            // since the v3 trait returns `NoProgress` and there is
            // no carrier-progress yield shape that fits a one-shot
            // identity-side query.
            StepOutcome::Advanced(id) => tx_substrate::step_v3::StepOutcome::done(id),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::AdvancedThenBlocked(id, _) => tx_substrate::step_v3::StepOutcome::done(id),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::load_inode_meta(self, fs_object_id, guard) {
            StepOutcome::Done(meta) => tx_substrate::step_v3::StepOutcome::done(meta),
            StepOutcome::Advanced(meta) => tx_substrate::step_v3::StepOutcome::done(meta),
            StepOutcome::AdvancedThenBlocked(meta, _) => {
                tx_substrate::step_v3::StepOutcome::done(meta)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::serialize_inode_meta(self, fs_object_id, meta, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::create_inode(self, parent, name, mode, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::unlink(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::rename(self, old_parent, old_name, new_parent, new_name, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::link(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::mkdir(self, parent, name, mode, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::rmdir(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::symlink(self, parent, name, link_target, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::readdir(self, fs_object_id, cursor, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::destroy_inode(self, fs_object_id, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        alloc::boxed::Box<[u8]>,
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::read_link(self, fs_object_id, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Cap<RNode>,
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::materialise_rnode(self, fs_object_id, meta, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn step_chmod(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::step_chmod(self, fs_object_id, new_mode, cred, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn step_chown(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::step_chown(self, fs_object_id, new_uid, new_gid, cred, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }
}

impl FsPageBackingV3 for Tmpfs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fetch_page(self, fs_object_id, offset, guard) {
            StepOutcome::Done(frame) | StepOutcome::Advanced(frame) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::AdvancedThenBlocked(frame, _) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::flush_page(self, fs_object_id, offset, frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::truncate(self, fs_object_id, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn fsync(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fsync(self, fs_object_id, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn fallocate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fallocate(self, fs_object_id, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn supports_reflink(&self, other: &tx_subsystems::page_backed::PageContainer) -> bool {
        <Self as FsPageBacking>::supports_reflink(self, other)
    }
}

#[cfg(test)]
mod tests;
