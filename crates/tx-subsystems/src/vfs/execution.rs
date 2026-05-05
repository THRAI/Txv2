//! VFS execution: backend trait and mutating step bodies.
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1` §execution: this module owns the `FsOps`
//! trait — the boundary that filesystem backends implement — plus the
//! mount-time output value `MountOutput` and the `OpenFile` step methods
//! that dispatch read/write through the RNode backing.

use alloc::sync::Arc;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::page_backed::FsPageBacking;
use crate::tty;

use super::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InodeMeta, OpenFile, RNodeBacking, StructPayload,
};

/// Filesystem backend trait. The boundary tx-ext4, tmpfs, devfs, etc.
/// implement to provide namespace + page-backing operations.
pub trait FsOps: Send + Sync + 'static {
    fn lookup(&self, parent: FsObjectId, name: &[u8], guard: &Guard<'_>)
        -> StepOutcome<FsObjectId>;

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta>;

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>>;

    fn destroy_inode(&self, fs_object_id: FsObjectId, guard: &Guard<'_>) -> StepOutcome<()>;

    /// Read a symlink's target bytes.
    ///
    /// The walker calls this when materialising an `RNode` for an
    /// inode whose `load_inode_meta(...).kind() == InodeKind::Symlink`.
    /// Returned bytes are substituted into the remaining path
    /// component stream per `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`.
    ///
    /// Default returns `Errno::ENOSYS` so backends that do not yet
    /// support symlinks (devfs, devpts, projection-only filesystems)
    /// inherit the right error without forcing a method-by-method
    /// flood. tmpfs and ext4 override.
    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>> {
        let _ = (fs_object_id, guard);
        StepOutcome::Err(Errno::ENOSYS)
    }

    /// Backend hook for materialising an `RNode` for a non-directory,
    /// non-symlink inode.
    ///
    /// The walker handles `Directory` (always
    /// `RNodeBacking::Directory`) and `Symlink` (via [`read_link`])
    /// inline; everything else (regular files, char/block devices,
    /// fifos, sockets) needs backend-specific materialisation
    /// because the right backing depends on the filesystem:
    ///
    /// - tmpfs `Regular` → `RNodeBacking::PageBacked { pc }` over
    ///   the inode's `Cap<PageContainer>`.
    /// - devfs `CharDevice` → `RNodeBacking::StructBacked { Tty }`
    ///   resolved through the TTY registry.
    /// - ext4 `Regular` → `RNodeBacking::PageBacked { pc }` over a
    ///   per-inode `Cap<PageContainer>` keyed by `(mount, fs_object_id)`.
    ///
    /// Default returns `Errno::ENOSYS` so backends that don't grow
    /// the hook fall through cleanly.
    ///
    /// `meta` is the freshly-loaded inode meta. The walker passes
    /// it so the backend can decide based on `meta.kind()`.
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<tx_substrate::zone::Cap<crate::vfs::structure::RNode>> {
        let _ = (fs_object_id, meta, guard);
        StepOutcome::Err(Errno::ENOSYS)
    }
}

/// Filesystem driver output produced at mount time and consumed by Mount
/// to build the mount payload. Per `TX_EXT4_PLAN_v1_2.md` §pub-types and
/// `bringup_fs_specs_v_1` §root-output.
pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

// === OpenFile read/write step dispatch ================================
//
// Phase D interface slice: full fd tables, UserBuf copying, page-backed
// file I/O, and projection schemas are still later work. TTY and raw
// char-device struct payloads already route through their owning
// subsystems.

impl OpenFile {
    /// Dispatch a read against this file's RNode backing.
    pub fn step_read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.read {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_read(tty, out, guard),
                StructPayload::CharDevice(binding) => binding.ops.read(out, guard),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }

    /// Dispatch a write against this file's RNode backing.
    pub fn step_write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.write {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_write(tty, bytes, guard),
                StructPayload::CharDevice(binding) => binding.ops.write(bytes, guard),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }
}
