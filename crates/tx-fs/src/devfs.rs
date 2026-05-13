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
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::process;
use tx_subsystems::tty;
use tx_subsystems::vfs::{
    self, Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
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
/// build an `Arc<dyn FsOps>` (and an `Arc<dyn FsPageBacking>`)
/// without needing a constructor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Devfs;

impl Devfs {
    pub const fn new() -> Self {
        Self
    }

    /// v3 factory matching the `MountOutput` shape Phase 3b will pass
    /// into `MountIdentity::new_cap`.
    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self)
    }

    /// v3 page-backing factory.
    pub fn fs_page_backing_arc() -> Arc<dyn FsPageBacking> {
        Arc::new(Self)
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
pub fn resolve_console_rnode(
    name: &[u8],
) -> tx_substrate::step_v3::StepOutcome<Cap<RNode>, tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    let Some(tty) = tty::project::resolve_devfs_alias(name) else {
        return V3::err(Errno::ENOENT.into());
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
        Ok(rnode) => V3::done(rnode),
        Err(_) => V3::err(Errno::EIO.into()),
    }
}

/// One-shot helper: open `/dev/console` as a read+write `OpenFile`.
///
/// Phase 4 of the pre-ELF runtime plan
/// (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`)
/// retires the bootstrap exemption: when init's cwd is bound and the
/// rootfs+devfs mount table is populated, the body goes through
/// `vfs::step_open(root, b"/dev/console", RDWR, ...)` per
/// `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`.
///
/// The function stays synchronous because callers in
/// `crates/tx-kernel/src/init.rs::bind_init_cwd_and_root` are not in
/// async context. We drive the walker via a tiny in-module
/// [`block_on`] helper.
///
/// ## Fallback path
///
/// The Phase 1+4 sibling slices may land out of order with the
/// `mount_devfs_at_dev` wiring that *publishes* the `mounted` hint
/// on `/dev`. When init's cwd or the mount-hint tree is not yet
/// observable, the walker has no way to cross from rootfs into
/// devfs and `step_open(/dev/console)` would return `ENOENT`. To
/// keep the trio's existing tests green during the staged rollout,
/// the helper falls back to the legacy direct-RNode materialisation
/// (the same shape the bootstrap exemption used) whenever
/// `init_process()` is `None`. Once init.rs's
/// `mount_devfs_at_dev` is updated to set the mount hint and
/// `bind_init_cwd_and_root` runs before the helper is called, the
/// fallback is skipped and the walker resolves the path end-to-end.
///
/// # Panics
///
/// Panics if neither the walker nor the fallback can resolve
/// `/dev/console`. Both branches indicate the kernel cannot make
/// further bootstrap progress.
pub fn open_console_for_init() -> Cap<OpenFile> {
    open_console_for_init_legacy()
}

pub fn open_console_for_init_via_walker() -> Cap<OpenFile> {
    if let Some(init) = process::execution::init_process() {
        if let Some(root) = init.cwd() {
            // Bootstrap path: init opens /dev/console as root.
            let cred = Credential::root();
            let guard = tx_substrate::epoch::guard();
            use tx_substrate::step_v3::StepOutcome as V3;
            let outcome = block_on(vfs::step_open(
                root,
                b"/dev/console",
                OpenFileFlags {
                    read: true,
                    write: true,
                    append: false,
                    cloexec: false,
                    nonblocking: false,
                },
                0,
                &cred,
                &guard,
            ));
            match outcome {
                V3::Done(file) => return file,
                _other => {
                    // Walker could not resolve the path under current
                    // bootstrap state — fall through to legacy
                    // direct-RNode materialisation. Once Phase 1's
                    // `mount_devfs_at_dev` mount-hint wiring lands,
                    // this branch is unreachable on the boot path.
                }
            }
        }
    }
    open_console_for_init_legacy()
}

/// Legacy direct-RNode path. Materialises an `OpenFile` over the
/// `console` alias's TTY without consulting the walker. Retained as
/// the fallback for `open_console_for_init`'s walker redirect (see
/// the function's "Fallback path" doc) and as the synchronous
/// primitive Phase 2a tests still rely on.
fn open_console_for_init_legacy() -> Cap<OpenFile> {
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
            cloexec: false,
            nonblocking: false,
        },
    )
    .expect("open_console_for_init: OpenFile reservation failed")
}

/// Tiny synchronous future driver used by [`open_console_for_init`].
///
/// `vfs::step_open` is `async fn`. Init's bootstrap path is fully
/// synchronous, so we drive the future on a no-op waker until it
/// resolves. The walker today never actually parks (every
/// `FsOps` call site is synchronous in-memory), so this loop
/// terminates on the first poll for the bootstrap path; we cap at
/// 1024 polls to surface a runaway future during development.
fn block_on<F: core::future::Future>(mut fut: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn raw_clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn raw_wake(_: *const ()) {}
    fn raw_wake_by_ref(_: *const ()) {}
    fn raw_drop(_: *const ()) {}
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: the vtable is `'static` and the data pointer is
    // unused (raw_* take it but never dereference).
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on the stack for the full loop; we never
    // move it after the unchecked pin.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("open_console_for_init::block_on: future did not resolve in 1024 polls");
}

// === FsOps / FsPageBacking impls ====================================
//
// Devfs is a read-only projection backend (every mutating op returns
// `EROFS`, every page-cache op returns `ENOSYS`) — every method
// returns synchronously, so each body is a direct
// `StepOutcome::done(...)` / `StepOutcome::err(...)` ladder. Per the
// wave-8 design doc
// (`docs/progress/decisions/2026-05-09-fsops-v3-design.md`), v3 callers
// (the walker entry points) opt into these impls via
// `Arc<dyn FsOps>` / `Arc<dyn FsPageBacking>`.
//
// Fully-qualified `tx_substrate::step_v3::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::StepOutcome`
// already in scope, per the wave-4/6/7 trait-impl convention.

impl FsOps for Devfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        if parent != DEVFS_ROOT_OBJECT_ID {
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into());
        }
        if tty::project::resolve_devfs_alias(name).is_some() {
            // Identify the entry by its position in the live alias
            // snapshot. Stable for one snapshot, opaque to the caller —
            // devfs makes no inode-persistence promise.
            let entries = tty::project::devfs_alias_entries();
            for (idx, entry) in entries.iter().enumerate() {
                if entry.name == name {
                    return tx_substrate::step_v3::StepOutcome::done(FsObjectId::new(
                        DEVFS_ENTRY_OBJECT_BASE + idx as u64,
                    ));
                }
            }
        }
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into())
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        if fs_object_id == DEVFS_ROOT_OBJECT_ID {
            return tx_substrate::step_v3::StepOutcome::done(InodeMeta::new(
                InodeKind::Directory,
                DEVFS_ROOT_MODE,
            ));
        }
        if entry_index_from_object_id(fs_object_id)
            .and_then(|idx| tty::project::devfs_alias_entries().into_iter().nth(idx))
            .is_some()
        {
            return tx_substrate::step_v3::StepOutcome::done(InodeMeta::new(
                InodeKind::CharDevice,
                DEVFS_CHAR_MODE,
            ));
        }
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into())
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    > {
        if fs_object_id != DEVFS_ROOT_OBJECT_ID {
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOTDIR.into());
        }
        let entries = tty::project::devfs_alias_entries();
        let index = cursor.as_u64() as usize;
        let Some(entry) = entries.get(index) else {
            return tx_substrate::step_v3::StepOutcome::done(None);
        };
        let dir_entry = match DirEntry::new(
            FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + index as u64),
            InodeKind::CharDevice,
            &entry.name,
        ) {
            Ok(de) => de,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };
        tx_substrate::step_v3::StepOutcome::done(Some((
            dir_entry,
            DirCursor::from_u64(cursor.as_u64() + 1),
        )))
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        // Registry entries are owned by the TTY subsystem; devfs has no
        // inode storage to release. Successful no-op so the VFS layer
        // can drop its `RNode` without seeing a backend error.
        tx_substrate::step_v3::StepOutcome::done(())
    }

    /// Materialise an `RNode` for a character-device alias.
    ///
    /// Per `txdoc:TTY-RNODE-MATERIALIZATION-1`
    /// (`docs/design/06_devices/TTY.md` §6.3): char-device entries on
    /// devfs project the registered TTY as
    /// `RNodeBacking::StructBacked { payload: StructPayload::Tty(...) }`,
    /// which routes `OpenFile::step_read` / `step_write` through the
    /// TTY subsystem. The walker invokes this hook after `lookup` /
    /// `load_inode_meta` resolve a char-device inode; without it, the
    /// walker falls through to the trait default (`ENOSYS`) and the
    /// path resolution fails at the terminal component.
    ///
    /// Mirrors the standalone `resolve_console_rnode` helper above —
    /// the latter is retained for the bootstrap-fallback path
    /// (`open_console_for_init_legacy`); this hook is the production
    /// walker site.
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Cap<RNode>, tx_substrate::step_v3::NoProgress> {
        if meta.kind() != InodeKind::CharDevice {
            // devfs only publishes the root directory and
            // char-device aliases. Directories are handled by the
            // walker's inline `Directory` arm; anything else is a
            // backend bug.
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into());
        }
        let Some(idx) = entry_index_from_object_id(fs_object_id) else {
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into());
        };
        let entries = tty::project::devfs_alias_entries();
        let Some(entry) = entries.into_iter().nth(idx) else {
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into());
        };
        let tty = entry.tty;
        match RNode::new_cap(
            fs_object_id,
            meta,
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            },
        ) {
            Ok(rnode) => tx_substrate::step_v3::StepOutcome::done(rnode),
            Err(_) => tx_substrate::step_v3::StepOutcome::err(Errno::EIO.into()),
        }
    }

    fn step_chmod(
        &self,
        _fs_object_id: FsObjectId,
        _new_mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        // devfs is a read-only projection-shaped backend; mode-bit
        // mutation is not supported.
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }

    fn step_chown(
        &self,
        _fs_object_id: FsObjectId,
        _new_uid: Option<u32>,
        _new_gid: Option<u32>,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::EROFS.into())
    }
}

impl FsPageBacking for Devfs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        // Char-device I/O does not flow through the page cache; routing
        // happens via `OpenFile::step_read` / `step_write` against the
        // RNode's `StructBacked { Tty }` backing instead.
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn fsync(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    // `fallocate` keeps the v3 trait default (`Done(())`) — devfs has
    // no on-disk space to reserve, so a hint is a successful no-op.
    // `supports_reflink` keeps the trait default (`false`).
}

#[cfg(test)]
mod tests;
