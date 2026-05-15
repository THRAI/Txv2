//! procfs — minimal projected filesystem for `/proc`.
//! Bringup scope: `/proc`, `/proc/<pid>`, `/proc/self`, `/proc/mounts`.

use alloc::sync::Arc;
use alloc::vec::Vec;

pub mod adapter;

use adapter::step_engine::{self, Cap, NoProgress, StepOutcome};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, RNode, RNodeBacking,
    S_IFDIR, S_IFREG, S_IFLNK,
};

pub const PROCFS_ROOT_ID: FsObjectId = FsObjectId::new(0x7072_6F00);
pub const PROCFS_SELF_ID: FsObjectId = FsObjectId::new(0x7072_6F01);
pub const PROCFS_MOUNTS_ID: FsObjectId = FsObjectId::new(0x7072_6F02);
pub const PROCFS_CPUINFO_ID: FsObjectId = FsObjectId::new(0x7072_6F03);
pub const PROCFS_UPTIME_ID: FsObjectId = FsObjectId::new(0x7072_6F04);
const PROCFS_PID_BASE: u64 = 0x7072_0000;
const PROCFS_STAT_OFFSET: u64 = 0x10000;
const PROCFS_MEM_OFFSET: u64 = 0x10002;
const fn pid_dir_id(pid: Pid) -> FsObjectId { FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64) }
const fn pid_stat_id(pid: Pid) -> FsObjectId { FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_STAT_OFFSET) }
const fn pid_cmdline_id(pid: Pid) -> FsObjectId { FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_STAT_OFFSET + 1) }
const fn pid_mem_id(pid: Pid) -> FsObjectId { FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_MEM_OFFSET) }
pub fn pid_from_mem_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_MEM_OFFSET;
    if r >= base && r < base + 0x10000 { Some(Pid((r - base) as u32)) } else { None }
}

fn pid_from_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r > PROCFS_PID_BASE && r < PROCFS_PID_BASE + PROCFS_STAT_OFFSET { Some(Pid((r - PROCFS_PID_BASE) as u32)) } else { None }
}
pub fn pid_from_stat_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_STAT_OFFSET;
    if r >= base && r < base + 0x10000 { Some(Pid((r - base) as u32)) } else { None }
}
pub fn pid_from_cmdline_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_STAT_OFFSET + 1;
    if r >= base && r < base + 0x10000 { Some(Pid((r - base) as u32)) } else { None }
}

fn dir_entry(id: FsObjectId, kind: InodeKind, name: &[u8]) -> DirEntry {
    DirEntry::new(id, kind, name).expect("procfs dir entry name")
}

pub const PROCFS_DIR_MODE: u16 = S_IFDIR | 0o555;
pub const PROCFS_FILE_MODE: u16 = S_IFREG | 0o444;
pub const PROCFS_SYMLINK_MODE: u16 = S_IFLNK | 0o777;

#[derive(Clone, Copy, Debug, Default)]
pub struct Procfs;

impl Procfs {
    pub const fn new() -> Self { Self }
    pub fn fs_ops_arc() -> Arc<dyn FsOps> { Arc::new(Self::new()) }
}

impl FsOps for Procfs {
    fn lookup(&self, parent: FsObjectId, name: &[u8], _guard: &Guard<'_>) -> StepOutcome<FsObjectId, NoProgress> {
        if parent == PROCFS_ROOT_ID {
            if name == b"self" { return StepOutcome::done(PROCFS_SELF_ID); }
            if name == b"mounts" { return StepOutcome::done(PROCFS_MOUNTS_ID); }
            if name == b"cpuinfo" { return StepOutcome::done(PROCFS_CPUINFO_ID); }
            if name == b"uptime" { return StepOutcome::done(PROCFS_UPTIME_ID); }
            if let Ok(n) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() {
                if n > 0 && process::process_by_pid(Pid(n)).is_some() {
                    return StepOutcome::done(pid_dir_id(Pid(n)));
                }
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if let Some(pid) = pid_from_dir(parent) {
            if name == b"stat" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_stat_id(pid));
            }
            if name == b"cmdline" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_cmdline_id(pid));
            }
            if name == b"mem" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_mem_id(pid));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        StepOutcome::err(Errno::ENOENT.into())
    }

    fn load_inode_meta(&self, id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<InodeMeta, NoProgress> {
        match id {
            PROCFS_ROOT_ID => StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE)),
            PROCFS_SELF_ID => StepOutcome::done(InodeMeta::new(InodeKind::Symlink, PROCFS_SYMLINK_MODE)),
            PROCFS_MOUNTS_ID | PROCFS_CPUINFO_ID | PROCFS_UPTIME_ID =>
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE)),
            id if pid_from_dir(id).is_some() => StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE)),
            id if pid_from_stat_id(id).is_some() => StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE)),
            id if pid_from_cmdline_id(id).is_some() => StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE)),
            id if pid_from_mem_id(id).is_some() => StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE | 0o600)),
            _ => StepOutcome::err(Errno::ENOENT.into()),
        }
    }

    fn readdir(
        &self,
        id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        let raw = cursor.0;
        let state_byte = raw[0];
        let idx = raw[1] as usize;

        if let Some(pid) = pid_from_dir(id) {
            // PID directory: dots + stat + cmdline
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                (b"stat", pid_stat_id(pid), InodeKind::Regular),
                (b"cmdline", pid_cmdline_id(pid), InodeKind::Regular),
                (b"mem", pid_mem_id(pid), InodeKind::Regular),
            ];
            let fi = idx.saturating_sub(2);
            if fi < files.len() {
                let (name, oid, kind) = files[fi];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor([2, (fi + 1) as u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if id != PROCFS_ROOT_ID {
            return finish_dots(state_byte, idx, id);
        }

        if state_byte < 2 {
            return finish_dots(state_byte, idx, id);
        }

        let statics: &[(&[u8], FsObjectId, InodeKind)] = &[
            (b"self", PROCFS_SELF_ID, InodeKind::Symlink),
            (b"mounts", PROCFS_MOUNTS_ID, InodeKind::Regular),
            (b"cpuinfo", PROCFS_CPUINFO_ID, InodeKind::Regular),
            (b"uptime", PROCFS_UPTIME_ID, InodeKind::Regular),
        ];
        let si = idx.saturating_sub(2);
        if state_byte == 2 && si < statics.len() {
            let (name, oid, kind) = statics[si];
            return StepOutcome::done(Some((
                dir_entry(oid, kind, name),
                DirCursor([2, (si + 1) as u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0]),
            )));
        }

        let pids: Vec<(Pid, bool)> = process::all_pids();
        let pi = if state_byte == 2 { 0 } else { idx };
        if pi < pids.len() {
            let (pid, alive) = pids[pi];
            if alive {
                let s = alloc::format!("{}", pid.0);
                return StepOutcome::done(Some((
                    dir_entry(pid_dir_id(pid), InodeKind::Directory, s.as_bytes()),
                    DirCursor([3, (pi + 1) as u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0]),
                )));
            }
        }

        StepOutcome::done(None)
    }

    fn read_link(&self, id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        if id == PROCFS_SELF_ID {
            StepOutcome::done(alloc::boxed::Box::from(&b"1"[..]))
        } else {
            StepOutcome::err(Errno::ENOENT.into())
        }
    }

    fn create_inode(&self, _: FsObjectId, _: &[u8], _: u16, _: &Credential, _: &Guard<'_>) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn mkdir(&self, _: FsObjectId, _: &[u8], _: u16, _: &Credential, _: &Guard<'_>) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn unlink(&self, _: FsObjectId, _: &[u8], _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn rmdir(&self, _: FsObjectId, _: &[u8], _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn symlink(&self, _: FsObjectId, _: &[u8], _: &[u8], _: &Credential, _: &Guard<'_>) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn rename(&self, _: FsObjectId, _: &[u8], _: FsObjectId, _: &[u8], _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn link(&self, _: FsObjectId, _: &[u8], _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn destroy_inode(&self, _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::done(()) }
    fn serialize_inode_meta(&self, _: FsObjectId, _: &InodeMeta, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::done(()) }
    fn materialise_rnode(
        &self,
        id: FsObjectId,
        meta: InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        let rnode = RNode::new(id, meta, RNodeBacking::Projected);
        match Err("zone unavailable") /* TODO: step_engine::sign removed */ {
            Ok(cap) => StepOutcome::done(cap),
            Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
        }
    }

    fn step_read_projected(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        buf: &mut [u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        // `/proc/<pid>/mem` — read from target process's address space.
        // `offset` is the virtual address to read from.
        if let Some(pid) = pid_from_mem_id(fs_object_id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ESRCH);
            };
            let Some(aspace) = proc.aspace_cap() else {
                return StepOutcome::err(Errno::ESRCH);
            };
            let src = tx_hal::UserPtr::<u8>::from(offset as usize);
            match aspace.copy_from_user(buf, src, guard) {
                StepOutcome::Done(n) => StepOutcome::done(n as u64),
                StepOutcome::Err(e) => StepOutcome::err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    StepOutcome::err(Errno::EIO)
                }
            }
        } else {
            // Other projected files: return empty for now.
            let content: Vec<u8> = Vec::new();
            let bytes = content.as_slice();
            let off = offset as usize;
            if off >= bytes.len() {
                return StepOutcome::done(0);
            }
            let available = &bytes[off..];
            let len = available.len().min(buf.len());
            buf[..len].copy_from_slice(&available[..len]);
            StepOutcome::done(len as u64)
        }
    }
    fn step_chmod(&self, _: FsObjectId, _: u16, _: &Credential, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
    fn step_chown(&self, _: FsObjectId, _: Option<u32>, _: Option<u32>, _: &Credential, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::EROFS.into()) }
}

fn finish_dots(state_byte: u8, idx: usize, id: FsObjectId) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
    if (idx as u8) < 2 {
        let name: &[u8] = if idx == 0 { b"." } else { b".." };
        let entry = dir_entry(id, InodeKind::Directory, name);
        let next = DirCursor([state_byte, (idx + 1) as u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0]);
        StepOutcome::done(Some((entry, next)))
    } else {
        StepOutcome::done(None)
    }
}

impl FsPageBacking for Procfs {
    fn fetch_page(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<Frame, NoProgress> { StepOutcome::err(Errno::ENOSYS.into()) }
    fn flush_page(&self, _: FsObjectId, _: u64, _: &Frame, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::ENOSYS.into()) }
    fn truncate(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::ENOSYS.into()) }
    fn fsync(&self, _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> { StepOutcome::err(Errno::ENOSYS.into()) }
}