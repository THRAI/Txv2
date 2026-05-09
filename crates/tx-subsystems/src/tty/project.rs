//! TTY-facing devfs/devpts materialization helpers.
//!
//! These helpers are intentionally below a full filesystem implementation.
//! They let devfs/devpts or tests materialize the RNode/OpenFile shape that
//! PAGE_BACKED/DEVICE/TTY specify without needing path-walk or fd tables yet.

use alloc::vec::Vec;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::execution;
use crate::tty::structure::registry;
use crate::tty::structure::TtyIdentity;
use crate::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNode, RNodeBacking, StructPayload,
};

const DEVFS_TTY_OBJECT_BASE: u64 = 0x7474_7900;
pub const DEVPTS_ROOT_OBJECT_ID: FsObjectId = FsObjectId::new(0x7074_7300);
pub const DEVPTS_PTMX_OBJECT_ID: FsObjectId = FsObjectId::new(0x7074_7301);
const DEVPTS_SLAVE_OBJECT_BASE: u64 = 0x7074_7400;

pub struct DevptsInstance;

impl FsOps for DevptsInstance {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        if parent != DEVPTS_ROOT_OBJECT_ID {
            return StepOutcome::Err(Errno::ENOENT);
        }

        if name == b"ptmx" {
            return StepOutcome::Done(DEVPTS_PTMX_OBJECT_ID);
        }

        let Some(index) = parse_u32_decimal(name) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        if registry::contains_pty_slave(index) {
            return StepOutcome::Done(devpts_object_id_for_index(index));
        }

        StepOutcome::Err(Errno::ENOENT)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        if fs_object_id == DEVPTS_ROOT_OBJECT_ID {
            return StepOutcome::Done(InodeMeta::new(InodeKind::Directory, 0o040755));
        }
        if fs_object_id == DEVPTS_PTMX_OBJECT_ID {
            return StepOutcome::Done(InodeMeta::new(InodeKind::CharDevice, 0o020666));
        }
        if let Some(index) = pty_index_from_devpts_object_id(fs_object_id) {
            if registry::contains_pty_slave(index) {
                return StepOutcome::Done(InodeMeta::new(InodeKind::CharDevice, 0o020620));
            }
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
        if fs_object_id != DEVPTS_ROOT_OBJECT_ID {
            return StepOutcome::Err(Errno::ENOTDIR);
        }

        let entries = match devpts_dir_entries() {
            Ok(entries) => entries,
            Err(err) => return StepOutcome::Err(err),
        };
        let index = cursor.as_u64() as usize;
        let Some(entry) = entries.get(index).copied() else {
            return StepOutcome::Done(None);
        };
        StepOutcome::Done(Some((entry, DirCursor::from_u64(cursor.as_u64() + 1))))
    }

    fn destroy_inode(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        if fs_object_id == DEVPTS_ROOT_OBJECT_ID
            || fs_object_id == DEVPTS_PTMX_OBJECT_ID
            || pty_index_from_devpts_object_id(fs_object_id)
                .is_some_and(registry::contains_pty_slave)
        {
            return StepOutcome::Done(());
        }

        StepOutcome::Err(Errno::ENOENT)
    }
}

/// Resolve a devfs TTY entry such as `ttyS0` or `console`.
pub fn devfs_tty_by_name(name: &[u8], _guard: &Guard<'_>) -> StepOutcome<Cap<TtyIdentity>> {
    if name == b"ptmx" {
        return StepOutcome::Err(Errno::EINVAL);
    }

    if let Some(index) = parse_tty_s_index(name) {
        if let Some(tty) = registry::hardware_tty(index) {
            return StepOutcome::Done(tty);
        }
    }

    if let Some(alias) = registry::devfs_alias(name) {
        return StepOutcome::Done(alias);
    }

    StepOutcome::Err(Errno::ENOENT)
}

/// Snapshot of one devfs alias entry, surfaced to consumers that want to
/// enumerate `/dev` without poking the registry's internal storage type.
///
/// Returned by [`devfs_alias_entries`]; each entry is a clone of the live
/// `Cap<TtyIdentity>` and the `name` bytes the alias was registered under.
#[derive(Clone)]
pub struct DevfsAliasEntry {
    pub name: Vec<u8>,
    pub tty: Cap<TtyIdentity>,
}

/// Snapshot the currently-registered devfs alias entries (per
/// `txdoc:TTY-LOOKUP-1` / `txdoc:TTY-RNODE-MATERIALIZATION-1`). Used by
/// `tx-fs::devfs::FsOps::readdir` to enumerate `/dev` without coupling to
/// the registry's slot storage.
pub fn devfs_alias_entries() -> Vec<DevfsAliasEntry> {
    let mut out = Vec::new();
    for entry in registry::devfs_alias_snapshot() {
        let mut name = Vec::new();
        name.extend_from_slice(entry.name.as_bytes());
        out.push(DevfsAliasEntry {
            name,
            tty: entry.tty,
        });
    }
    out
}

/// Thin wrapper around the devfs alias and hardware-TTY tables, exposing a
/// single `Option<Cap<TtyIdentity>>` indirection for `tx-fs::devfs` (and any
/// other consumer that wants alias-shape lookup without depending on the
/// registry's internal storage type or the `StepOutcome` ladder).
///
/// Resolution order matches `devfs_tty_by_name` (per
/// `txdoc:TTY-THE-HARDWARE-CONSOLE-PATH-1`):
///
/// 1. `ttyS<N>` parses as a hardware index and resolves to the registered
///    hardware TTY when present.
/// 2. Otherwise, the devfs alias table is consulted (this is where
///    `register_console_alias("console", ...)` publishes).
///
/// `ptmx` is intentionally not exposed here — it materialises through the
/// devpts projection schema, not as a static alias.
pub fn resolve_devfs_alias(name: &[u8]) -> Option<Cap<TtyIdentity>> {
    if name == b"ptmx" {
        return None;
    }

    if let Some(index) = parse_tty_s_index(name) {
        if let Some(tty) = registry::hardware_tty(index) {
            return Some(tty);
        }
    }

    registry::devfs_alias(name)
}

/// Materialize a devfs RNode for `ttyS<N>` or `console`.
pub fn devfs_rnode_by_name(name: &[u8], guard: &Guard<'_>) -> StepOutcome<Cap<RNode>> {
    let tty = match devfs_tty_by_name(name, guard) {
        StepOutcome::Done(tty) => tty,
        StepOutcome::Err(err) => return StepOutcome::Err(err),
        _ => return StepOutcome::Err(Errno::EIO),
    };
    let index = tty.index;
    rnode_for_tty(tty, FsObjectId::new(DEVFS_TTY_OBJECT_BASE + index as u64))
}

/// Open `/dev/ptmx`-shaped pty master. Full path-walk/fd-table layers can wrap
/// this and install `master_file` into the caller's fd table.
pub fn open_ptmx(guard: &Guard<'_>) -> StepOutcome<execution::OpenPtyOutcome> {
    execution::step_openpty(guard)
}

/// Open a devfs hardware/alias TTY entry such as `/dev/ttyS0` or `/dev/console`.
pub fn open_devfs_tty_by_name(name: &[u8], guard: &Guard<'_>) -> StepOutcome<Cap<OpenFile>> {
    let tty = match devfs_tty_by_name(name, guard) {
        StepOutcome::Done(tty) => tty,
        StepOutcome::Err(err) => return StepOutcome::Err(err),
        _ => return StepOutcome::Err(Errno::EIO),
    };
    open_file_for_tty(tty, guard)
}

/// Resolve a devpts numeric slave entry.
pub fn devpts_slave_by_index(index: u32, _guard: &Guard<'_>) -> StepOutcome<Cap<TtyIdentity>> {
    match registry::pty_slave(index) {
        Some(slave) => StepOutcome::Done(slave),
        None => StepOutcome::Err(Errno::ENOENT),
    }
}

/// Materialize `/dev/pts/<N>` as `StructBacked::Tty(slave)`.
pub fn devpts_rnode_by_index(index: u32, guard: &Guard<'_>) -> StepOutcome<Cap<RNode>> {
    let slave = match devpts_slave_by_index(index, guard) {
        StepOutcome::Done(slave) => slave,
        StepOutcome::Err(err) => return StepOutcome::Err(err),
        _ => return StepOutcome::Err(Errno::EIO),
    };
    rnode_for_tty(slave, devpts_object_id_for_index(index))
}

/// Build an OpenFile over a fresh StructBacked TTY RNode.
pub fn open_file_for_tty(tty: Cap<TtyIdentity>, _guard: &Guard<'_>) -> StepOutcome<Cap<OpenFile>> {
    let object_id = FsObjectId::new(DEVFS_TTY_OBJECT_BASE + tty.raw() as u64);
    let rnode = match rnode_for_tty(tty, object_id) {
        StepOutcome::Done(rnode) => rnode,
        StepOutcome::Err(err) => return StepOutcome::Err(err),
        _ => return StepOutcome::Err(Errno::EIO),
    };

    match OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    ) {
        Ok(file) => StepOutcome::Done(file),
        Err(_) => StepOutcome::Err(Errno::EIO),
    }
}

fn rnode_for_tty(tty: Cap<TtyIdentity>, object_id: FsObjectId) -> StepOutcome<Cap<RNode>> {
    match RNode::new_cap(
        object_id,
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        },
    ) {
        Ok(rnode) => StepOutcome::Done(rnode),
        Err(_) => StepOutcome::Err(Errno::EIO),
    }
}

fn devpts_dir_entries() -> Result<Vec<DirEntry>, Errno> {
    let mut entries = Vec::with_capacity(registry::MAX_PTY_SLAVES + 1);
    entries.push(DirEntry::new(
        DEVPTS_PTMX_OBJECT_ID,
        InodeKind::CharDevice,
        b"ptmx",
    )?);

    let (indices, len) = registry::list_pty_slave_indices();
    for index in indices.into_iter().take(len) {
        let name = PtyIndexName::new(index);
        entries.push(DirEntry::new(
            devpts_object_id_for_index(index),
            InodeKind::CharDevice,
            name.as_bytes(),
        )?);
    }

    Ok(entries)
}

fn devpts_object_id_for_index(index: u32) -> FsObjectId {
    FsObjectId::new(DEVPTS_SLAVE_OBJECT_BASE + index as u64)
}

fn pty_index_from_devpts_object_id(fs_object_id: FsObjectId) -> Option<u32> {
    let raw = fs_object_id.as_u64();
    if raw < DEVPTS_SLAVE_OBJECT_BASE {
        return None;
    }
    u32::try_from(raw - DEVPTS_SLAVE_OBJECT_BASE).ok()
}

fn parse_tty_s_index(name: &[u8]) -> Option<u32> {
    let digits = name.strip_prefix(b"ttyS")?;
    parse_u32_decimal(digits)
}

fn parse_u32_decimal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }

    let mut value = 0u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

struct PtyIndexName {
    buf: [u8; 10],
    len: usize,
}

impl PtyIndexName {
    fn new(value: u32) -> Self {
        let mut out = Self {
            buf: [0; 10],
            len: 0,
        };
        out.push_u32(value);
        out
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn push_u32(&mut self, value: u32) {
        let mut digits = [0u8; 10];
        let mut n = value;
        let mut len = 0;
        loop {
            digits[len] = b'0' + (n % 10) as u8;
            len += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for digit in digits[..len].iter().rev() {
            if self.len < self.buf.len() {
                self.buf[self.len] = *digit;
                self.len += 1;
            }
        }
    }
}

// === Wave 9b: parallel v3 trait impls ==================================
//
// `impl FsOpsV3 for DevptsInstance` and `impl FsPageBackingV3 for
// DevptsInstance` mirror the v4 surface above. Devpts is a PTY-side
// projection: every method is purely synchronous (no v4 `Advanced` /
// `Blocked` returns), so the v3 mapping is mechanical. There is no v4
// `FsPageBacking for DevptsInstance` impl — devpts has no page cache —
// so the v3 page-backing impl returns `ENOSYS` for every method whose
// v3 trait does not provide a default. Per the wave-8 design doc
// (`docs/progress/decisions/2026-05-09-fsops-v3-design.md`).
//
// Fully-qualified `tx_substrate::step_v3::*` references at the impl
// sites avoid clashing with `crate::execution::StepOutcome` already in
// scope, per the wave-4/6/7/9a trait-impl convention.

use crate::page_backed::{Frame, FsPageBackingV3, PageContainer};
use crate::vfs::FsOpsV3;

impl FsOpsV3 for DevptsInstance {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::lookup(self, parent, name, guard) {
            StepOutcome::Done(id) | StepOutcome::Advanced(id) => {
                tx_substrate::step_v3::StepOutcome::done(id)
            }
            StepOutcome::AdvancedThenBlocked(id, _) => tx_substrate::step_v3::StepOutcome::done(id),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::load_inode_meta(self, fs_object_id, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => {
                tx_substrate::step_v3::StepOutcome::done(m)
            }
            StepOutcome::AdvancedThenBlocked(m, _) => tx_substrate::step_v3::StepOutcome::done(m),
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

    // `read_link`, `materialise_rnode`, `step_chmod`, `step_chown`:
    // devpts does not override these on v4 (defaults return `ENOSYS`).
    // The matching v3 trait defaults also return `ENOSYS`, so leave
    // them unimplemented here.
}

impl FsPageBackingV3 for DevptsInstance {
    // Devpts has no page cache. Every page-backing method returns
    // `ENOSYS`. The v3 trait defaults `fallocate` to `Done(())` and
    // `supports_reflink` to `false`; both match the desired behaviour
    // for a projection-only filesystem, so leave them unimplemented.

    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn fsync(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}
