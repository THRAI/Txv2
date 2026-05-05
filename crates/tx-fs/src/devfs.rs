//! devfs — read-only static device namespace.
//!
//! devfs is a tier-2 projection-shaped filesystem in the same family as
//! procfs and devpts: it has no on-disk state and no per-mount allocator.
//! Every entry resolves through a registry rather than an in-memory
//! BTreeMap, so all mutating operations reject with `EROFS` (per
//! `txdoc:TTY-LOOKUP-1` / `txdoc:TTY-RNODE-MATERIALIZATION-1`, lookups
//! are cheap reads against the live registry; the filesystem owns no
//! durable state to mutate).
//!
//! The key design lever: `RNodeBacking::StructBacked { payload:
//! StructPayload::Tty(Cap<TtyIdentity>) }` already routes through
//! `OpenFile::step_read` / `OpenFile::step_write` to
//! `tty::execution::step_read` / `step_write`. Once devfs's `lookup`
//! returns the right RNode, no further dispatch wiring is needed (per
//! the trio plan §"Part 3 — devfs FsOps surface" and the "RNode
//! materialisation" note that follows).
//!
//! Phase 3a deliverable. Mount wiring lives in Phase 3b
//! (`crates/tx-kernel/src/init.rs`); this module is the standalone
//! backend and the `open_console_for_init` bootstrap helper.
//!
//! Active-doc anchors:
//! - `txdoc:TTY-THE-HARDWARE-CONSOLE-PATH-1` (`docs/design/06_devices/TTY.md` §7)
//! - `txdoc:TTY-LOOKUP-1`, `txdoc:TTY-RNODE-MATERIALIZATION-1`
//!   (`docs/design/06_devices/TTY.md` §6.2 / §6.3) — devpts/devfs
//!   share the registry-projection lookup shape.
//! - `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`
//!   (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §mount-boundary)
//!   — devfs is a separate `FsOps` instance, never aliasing the parent
//!   namespace's backend.
//! - `txdoc:MOUNT-MOUNTPAYLOAD-1`
//!   (`docs/design/05_filesystem/MOUNT_v1.md` §MountPayload) — devfs
//!   has to satisfy the `Arc<dyn FsOps>` + `Arc<dyn FsPageBacking>`
//!   shape for Phase 3b's `MountIdentity::new_cap`.

use alloc::sync::Arc;

use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Errno, Guard, StepOutcome};
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::tty;
use tx_subsystems::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNode, RNodeBacking, StructPayload, S_IFCHR, S_IFDIR,
};

/// Stable `FsObjectId` for the devfs root directory.
///
/// Picks a non-overlapping namespace from devpts (`0x7074_7300`) and
/// tmpfs (which uses `2..` starting from the reserved `FsObjectId::ROOT
/// = 1` sentinel, per the trio plan §"tmpfs root materialisation").
pub const DEVFS_ROOT_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7600);

/// Base id-space for devfs character-device entries. Each registered
/// alias gets `DEVFS_ENTRY_OBJECT_BASE + slot_index_within_snapshot`,
/// so the value is stable for one snapshot but does not pretend to be
/// stable across re-registrations. devfs has no inode persistence; the
/// `FsObjectId` here is observation-only.
const DEVFS_ENTRY_OBJECT_BASE: u64 = 0x6465_7601;

/// Mode for any character-device alias resolved by devfs (per the
/// Phase 3a plan §"devfs FsOps surface": `S_IFCHR | 0o620`).
pub const DEVFS_CHAR_MODE: u16 = S_IFCHR | 0o620;

/// Mode for the devfs root directory (`S_IFDIR | 0o755`).
pub const DEVFS_ROOT_MODE: u16 = S_IFDIR | 0o755;

/// Static read-only devfs backend.
///
/// Holds no state of its own — every observation resolves against the
/// TTY registry through `tty::project`. A single instance is enough for
/// the whole kernel, but the type stays unit-shaped so Phase 3b can
/// build an `Arc<dyn FsOps>` (and an `Arc<dyn FsPageBacking>`) without
/// needing a constructor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Devfs;

impl Devfs {
    pub const fn new() -> Self {
        Self
    }

    /// Convenience factory matching the `MountOutput` shape Phase 3b
    /// will pass into `MountIdentity::new_cap`. Phase 3a does not
    /// itself call this — it exists so the type fans out cleanly when
    /// devfs is mounted later.
    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self)
    }

    pub fn fs_page_backing_arc() -> Arc<dyn FsPageBacking> {
        Arc::new(Self)
    }
}

impl FsOps for Devfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        if parent != DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::Err(Errno::ENOENT);
        }
        if tty::project::resolve_devfs_alias(name).is_some() {
            // Identify the entry by its position in the live alias
            // snapshot. Stable for one snapshot, opaque to the caller —
            // devfs makes no inode-persistence promise.
            let entries = tty::project::devfs_alias_entries();
            for (idx, entry) in entries.iter().enumerate() {
                if entry.name == name {
                    return StepOutcome::Done(FsObjectId::new(
                        DEVFS_ENTRY_OBJECT_BASE + idx as u64,
                    ));
                }
            }
        }
        StepOutcome::Err(Errno::ENOENT)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        if fs_object_id == DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::Done(InodeMeta::new(InodeKind::Directory, DEVFS_ROOT_MODE));
        }
        if entry_index_from_object_id(fs_object_id)
            .and_then(|idx| tty::project::devfs_alias_entries().into_iter().nth(idx))
            .is_some()
        {
            return StepOutcome::Done(InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE));
        }
        StepOutcome::Err(Errno::ENOENT)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        if fs_object_id != DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::Err(Errno::ENOTDIR);
        }
        let entries = tty::project::devfs_alias_entries();
        let index = cursor.as_u64() as usize;
        let Some(entry) = entries.get(index) else {
            return StepOutcome::Done(None);
        };
        let dir_entry = match DirEntry::new(
            FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + index as u64),
            InodeKind::CharDevice,
            &entry.name,
        ) {
            Ok(de) => de,
            Err(err) => return StepOutcome::Err(err),
        };
        StepOutcome::Done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))))
    }

    fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        // Registry entries are owned by the TTY subsystem; devfs has no
        // inode storage to release. Successful no-op so the VFS layer
        // can drop its `RNode` without seeing a backend error.
        StepOutcome::Done(())
    }
}

impl FsPageBacking for Devfs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame> {
        // Char-device I/O does not flow through the page cache; routing
        // happens via `OpenFile::step_read` / `step_write` against the
        // RNode's `StructBacked { Tty }` backing instead.
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }
}

fn entry_index_from_object_id(id: FsObjectId) -> Option<usize> {
    let raw = id.as_u64();
    if raw < DEVFS_ENTRY_OBJECT_BASE {
        return None;
    }
    usize::try_from(raw - DEVFS_ENTRY_OBJECT_BASE).ok()
}

/// Materialise an `RNode` for the named devfs alias.
///
/// Construction follows `txdoc:TTY-RNODE-MATERIALIZATION-1`: char-device
/// entries are `StructBacked { payload: StructPayload::Tty(...) }`, which
/// already routes `step_read` / `step_write` through the TTY subsystem
/// (see `crates/tx-subsystems/src/vfs/execution.rs` `OpenFile::step_read`
/// / `step_write` dispatch on `RNodeBacking::StructBacked`).
///
/// Returns `Errno::ENOENT` if the registry has no such alias,
/// `Errno::EIO` if RNode allocation fails.
pub fn resolve_console_rnode(name: &[u8]) -> StepOutcome<Cap<RNode>> {
    let Some(tty) = tty::project::resolve_devfs_alias(name) else {
        return StepOutcome::Err(Errno::ENOENT);
    };

    let entries = tty::project::devfs_alias_entries();
    let object_id = entries
        .iter()
        .position(|entry| entry.name == name)
        .map(|idx| FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + idx as u64))
        .unwrap_or(DEVFS_ROOT_OBJECT_ID);

    match RNode::new_cap(
        object_id,
        InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        },
    ) {
        Ok(rnode) => StepOutcome::Done(rnode),
        Err(_) => StepOutcome::Err(Errno::EIO),
    }
}

/// One-shot helper: open `/dev/console` as a read+write `OpenFile`
/// without going through the (not-yet-implemented) VFS walker.
///
/// Used by Phase 3b's `init.rs` to preopen fds 0/1/2, and by Phase 2a's
/// syscall-dispatch tests that need a console-shaped `OpenFile` before
/// `step_open` exists.
///
/// **Bootstrap exemption.** The walker-based open path is deferred — see
/// `crates/tx-subsystems/src/vfs/execution.rs` (no `step_open` yet) and
/// `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1` for the eventual contract. This
/// helper constructs the `OpenFile` directly via
/// `OpenFile::new_cap(rnode, OpenFileFlags { read, write, .. })`. When
/// the walker lands, this function should be retired in favour of a
/// real `step_open("/dev/console", O_RDWR)` call.
///
/// # Panics
///
/// Panics if the TTY registry has no `console` alias registered, or if
/// the underlying RNode/OpenFile zone reservations fail. Both
/// conditions imply the kernel cannot make further bootstrap progress
/// — there is no useful caller-recoverable error at this point.
// TODO(phase-vfs-walker): replace with `step_open("/dev/console", O_RDWR)`
// once `crates/tx-subsystems/src/vfs/execution.rs` grows `step_open` and
// init has a real cwd/root binding (Phase 3b).
pub fn open_console_for_init() -> Cap<OpenFile> {
    let tty = tty::project::resolve_devfs_alias(b"console").expect(
        "open_console_for_init: /dev/console alias not registered before bootstrap fd preopen \
         (Phase 3b registers `console` via tty::execution::register_console_alias)",
    );

    let rnode = RNode::new_cap(
        FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE),
        InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        },
    )
    .expect("open_console_for_init: RNode reservation failed");

    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
        },
    )
    .expect("open_console_for_init: OpenFile reservation failed")
}

#[cfg(test)]
mod tests;
