//! procfs — minimal projected filesystem for `/proc`.
//! Bringup scope: `/proc`, `/proc/<pid>`, `/proc/self`, `/proc/mounts`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use tx_hal::UserPtr;

pub mod adapter;
mod read;

use adapter::step_engine::{Cap, NoProgress, StepOutcome};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::{
    render_dentry_path, Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta,
    ProjectionKey, ProjectionSchemaId, RNode, RNodeBacking, S_IFDIR, S_IFLNK, S_IFREG,
};

pub const PROCFS_ROOT_ID: FsObjectId = FsObjectId::new(0x7072_6F00);
pub const PROCFS_SELF_ID: FsObjectId = FsObjectId::new(0x7072_6F01);
pub const PROCFS_MOUNTS_ID: FsObjectId = FsObjectId::new(0x7072_6F02);
pub const PROCFS_CPUINFO_ID: FsObjectId = FsObjectId::new(0x7072_6F03);
pub const PROCFS_UPTIME_ID: FsObjectId = FsObjectId::new(0x7072_6F04);
pub const PROCFS_MEMINFO_ID: FsObjectId = FsObjectId::new(0x7072_6F05);
pub const PROCFS_SYSVIPC_ID: FsObjectId = FsObjectId::new(0x7072_6F06);
pub const PROCFS_SYSVIPC_MSG_ID: FsObjectId = FsObjectId::new(0x7072_6F07);
pub const PROCFS_SYSVIPC_SEM_ID: FsObjectId = FsObjectId::new(0x7072_6F08);
pub const PROCFS_SYSVIPC_SHM_ID: FsObjectId = FsObjectId::new(0x7072_6F09);
const PROCFS_PID_BASE: u64 = 0x7072_0000;
const PROCFS_STAT_OFFSET: u64 = 0x10000;
const PROCFS_MEM_OFFSET: u64 = 0x10002;
const PROCFS_MAPS_OFFSET: u64 = 0x10003;
const PROCFS_EXE_OFFSET: u64 = 0x10004;
const PROCFS_FD_OFFSET: u64 = 0x20000;
const PROCFS_FDINFO_OFFSET: u64 = 0x30000;
const fn pid_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64)
}
const fn pid_stat_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_STAT_OFFSET)
}
const fn pid_cmdline_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_STAT_OFFSET + 1)
}
const fn pid_mem_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_MEM_OFFSET)
}
const fn pid_maps_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_MAPS_OFFSET)
}
const fn pid_exe_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64 + PROCFS_EXE_OFFSET)
}
const fn pid_fd_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + PROCFS_FD_OFFSET + (pid.0 as u64 * 0x10000))
}
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
const fn pid_fd_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + PROCFS_FD_OFFSET + (pid.0 as u64 * 0x10000) + 1 + fd as u64)
}
const fn pid_fdinfo_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + PROCFS_FDINFO_OFFSET + (pid.0 as u64 * 0x10000))
}
const fn pid_fdinfo_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(
        PROCFS_PID_BASE + PROCFS_FDINFO_OFFSET + (pid.0 as u64 * 0x10000) + 1 + fd as u64,
    )
}
pub fn pid_from_mem_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_MEM_OFFSET;
    if r >= base && r < base + 0x10000 {
        Some(Pid((r - base) as u32))
    } else {
        None
    }
}
pub fn pid_from_maps_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_MAPS_OFFSET;
    if r >= base && r < base + 0x10000 {
        Some(Pid((r - base) as u32))
    } else {
        None
    }
}
pub fn pid_from_exe_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_EXE_OFFSET;
    if r >= base && r < base + 0x10000 {
        Some(Pid((r - base) as u32))
    } else {
        None
    }
}
fn pid_from_fd_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_FD_OFFSET;
    if r >= base && r < base + (0x10000 * 0x10000) && (r - base).is_multiple_of(0x10000) {
        Some(Pid(((r - base) / 0x10000) as u32))
    } else {
        None
    }
}
fn pid_from_fd_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_FD_OFFSET + 1;
    if r >= base {
        let offset = r - base;
        let pid = Pid((offset / 0x10000) as u32);
        let fd = (offset % 0x10000) as u32;
        Some((pid, fd))
    } else {
        None
    }
}
fn pid_from_fdinfo_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_FDINFO_OFFSET;
    if r >= base && r < base + (0x10000 * 0x10000) && (r - base).is_multiple_of(0x10000) {
        Some(Pid(((r - base) / 0x10000) as u32))
    } else {
        None
    }
}
pub fn pid_from_fdinfo_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_FDINFO_OFFSET + 1;
    if r >= base {
        let offset = r - base;
        let pid = Pid((offset / 0x10000) as u32);
        let fd = (offset % 0x10000) as u32;
        Some((pid, fd))
    } else {
        None
    }
}

fn pid_from_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r > PROCFS_PID_BASE && r < PROCFS_PID_BASE + PROCFS_STAT_OFFSET {
        Some(Pid((r - PROCFS_PID_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_stat_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_STAT_OFFSET;
    if r >= base && r < base + 0x10000 {
        Some(Pid((r - base) as u32))
    } else {
        None
    }
}
pub fn pid_from_cmdline_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    let base = PROCFS_PID_BASE + PROCFS_STAT_OFFSET + 1;
    if r >= base && r < base + 0x10000 {
        Some(Pid((r - base) as u32))
    } else {
        None
    }
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
    pub const fn new() -> Self {
        Self
    }
    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self::new())
    }
}

impl FsOps for Procfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if parent == PROCFS_ROOT_ID {
            if name == b"self" {
                return StepOutcome::done(PROCFS_SELF_ID);
            }
            if name == b"mounts" {
                return StepOutcome::done(PROCFS_MOUNTS_ID);
            }
            if name == b"cpuinfo" {
                return StepOutcome::done(PROCFS_CPUINFO_ID);
            }
            if name == b"uptime" {
                return StepOutcome::done(PROCFS_UPTIME_ID);
            }
            if name == b"meminfo" {
                return StepOutcome::done(PROCFS_MEMINFO_ID);
            }
            if name == b"sysvipc" {
                return StepOutcome::done(PROCFS_SYSVIPC_ID);
            }
            if let Ok(n) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() {
                if n > 0 && process::process_by_pid(Pid(n)).is_some() {
                    return StepOutcome::done(pid_dir_id(Pid(n)));
                }
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYSVIPC_ID {
            if name == b"msg" {
                return StepOutcome::done(PROCFS_SYSVIPC_MSG_ID);
            }
            if name == b"sem" {
                return StepOutcome::done(PROCFS_SYSVIPC_SEM_ID);
            }
            if name == b"shm" {
                return StepOutcome::done(PROCFS_SYSVIPC_SHM_ID);
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
            if name == b"maps" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_maps_id(pid));
            }
            if name == b"exe" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_exe_id(pid));
            }
            if name == b"fd" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_fd_dir_id(pid));
            }
            if name == b"fdinfo" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_fdinfo_dir_id(pid));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if let Some(pid) = pid_from_fdinfo_dir(parent) {
            let Ok(fd) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            if process::process_by_pid(pid)
                .and_then(|proc| proc.fd(fd))
                .is_some()
            {
                return StepOutcome::done(pid_fdinfo_id(pid, fd));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        StepOutcome::err(Errno::ENOENT.into())
    }

    fn load_inode_meta(
        &self,
        id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        match id {
            PROCFS_ROOT_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SELF_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Symlink, PROCFS_SYMLINK_MODE))
            }
            PROCFS_MOUNTS_ID | PROCFS_CPUINFO_ID | PROCFS_UPTIME_ID | PROCFS_MEMINFO_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_SYSVIPC_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SYSVIPC_MSG_ID | PROCFS_SYSVIPC_SEM_ID | PROCFS_SYSVIPC_SHM_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_stat_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_cmdline_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_mem_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE | 0o600))
            }
            id if pid_from_maps_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_exe_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Symlink, PROCFS_SYMLINK_MODE))
            }
            id if pid_from_fd_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_fd_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Symlink, PROCFS_SYMLINK_MODE))
            }
            id if pid_from_fdinfo_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_fdinfo_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
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
                (b"maps", pid_maps_id(pid), InodeKind::Regular),
                (b"exe", pid_exe_id(pid), InodeKind::Symlink),
                (b"fd", pid_fd_dir_id(pid), InodeKind::Directory),
                (b"fdinfo", pid_fdinfo_dir_id(pid), InodeKind::Directory),
            ];
            let fi = idx.saturating_sub(2);
            if fi < files.len() {
                let (name, oid, kind) = files[fi];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor([2, (fi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if id != PROCFS_ROOT_ID {
            if let Some(pid) = pid_from_fdinfo_dir(id) {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let Some(proc) = process::process_by_pid(pid) else {
                    return StepOutcome::done(None);
                };
                let fds: Vec<_> = proc
                    .payload_slot()
                    .lock()
                    .as_ref()
                    .map(|payload| payload.open_fds().into_keys().collect())
                    .unwrap_or_default();
                let fi = idx.saturating_sub(2);
                if fi < fds.len() {
                    let fd = fds[fi];
                    let name = alloc::format!("{}", fd);
                    return StepOutcome::done(Some((
                        dir_entry(pid_fdinfo_id(pid, fd), InodeKind::Regular, name.as_bytes()),
                        DirCursor([2, (fi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
                return StepOutcome::done(None);
            }
            if id == PROCFS_SYSVIPC_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (b"msg", PROCFS_SYSVIPC_MSG_ID, InodeKind::Regular),
                    (b"sem", PROCFS_SYSVIPC_SEM_ID, InodeKind::Regular),
                    (b"shm", PROCFS_SYSVIPC_SHM_ID, InodeKind::Regular),
                ];
                let fi = idx.saturating_sub(2);
                if fi < files.len() {
                    let (name, oid, kind) = files[fi];
                    return StepOutcome::done(Some((
                        dir_entry(oid, kind, name),
                        DirCursor([2, (fi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
                return StepOutcome::done(None);
            }
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
            (b"meminfo", PROCFS_MEMINFO_ID, InodeKind::Regular),
            (b"sysvipc", PROCFS_SYSVIPC_ID, InodeKind::Directory),
        ];
        let si = idx.saturating_sub(2);
        if state_byte == 2 && si < statics.len() {
            let (name, oid, kind) = statics[si];
            return StepOutcome::done(Some((
                dir_entry(oid, kind, name),
                DirCursor([2, (si + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
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
                    DirCursor([3, (pi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
        }

        StepOutcome::done(None)
    }

    fn read_link(
        &self,
        id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        if id == PROCFS_SELF_ID {
            // v1: /proc/self always points to pid 1.
            // Full implementation requires caller pid context.
            StepOutcome::done(alloc::boxed::Box::from(&b"1"[..]))
        } else if let Some((pid, fd_num)) = pid_from_fd_id(id) {
            // /proc/<pid>/fd/N — symlink target is the path of the open file.
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            let Some(_open_file) = proc.fd(fd_num) else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            // v1: render as "fd:N" since we don't have reverse-path from OpenFile.
            let target = alloc::format!("anon_inode:[{}]", fd_num);
            StepOutcome::done(target.into_bytes().into_boxed_slice())
        } else if let Some(pid) = pid_from_exe_id(id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            let Some(exe_dentry) = proc.exe_file() else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            match render_dentry_path(&exe_dentry) {
                Some(path) => StepOutcome::done(path.into_boxed_slice()),
                None => StepOutcome::err(Errno::ENOENT.into()),
            }
        } else {
            StepOutcome::err(Errno::ENOENT.into())
        }
    }

    fn create_inode(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: u16,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn mkdir(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: u16,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn unlink(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn rmdir(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn symlink(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: &[u8],
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn rename(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &[u8],
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn link(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn destroy_inode(&self, _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }
    fn serialize_inode_meta(
        &self,
        _: FsObjectId,
        _: &InodeMeta,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }
    fn materialise_rnode(
        &self,
        id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        match RNode::new_cap_in_mount(
            id,
            meta,
            RNodeBacking::Projected {
                schema: ProjectionSchemaId::Procfs,
                key: ProjectionKey::from_fs_object_id(id),
            },
            mount,
        ) {
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
                return StepOutcome::err(Errno::ESRCH.into());
            };
            let Some(aspace) = proc.aspace_cap() else {
                return StepOutcome::err(Errno::ESRCH.into());
            };
            let src = UserPtr::<u8>::new(offset as usize);
            match aspace.copy_from_user(buf, src, guard) {
                StepOutcome::Done(n) => StepOutcome::done(n as u64),
                StepOutcome::Err(e) => StepOutcome::err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    StepOutcome::err(Errno::EIO.into())
                }
            }
        } else {
            // Other projected files: render content via read::render.
            let content: Vec<u8> = read::render(fs_object_id).into_bytes();
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
    fn step_chmod(
        &self,
        _: FsObjectId,
        _: u16,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
    fn step_chown(
        &self,
        _: FsObjectId,
        _: Option<u32>,
        _: Option<u32>,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
}

fn finish_dots(
    state_byte: u8,
    idx: usize,
    id: FsObjectId,
) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
    if (idx as u8) < 2 {
        let name: &[u8] = if idx == 0 { b"." } else { b".." };
        let entry = dir_entry(id, InodeKind::Directory, name);
        let next = DirCursor([
            state_byte,
            (idx + 1) as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        StepOutcome::done(Some((entry, next)))
    } else {
        StepOutcome::done(None)
    }
}

impl FsPageBacking for Procfs {
    fn fetch_page(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<Frame, NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }
    fn flush_page(
        &self,
        _: FsObjectId,
        _: u64,
        _: &Frame,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }
    fn truncate(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }
    fn fsync_file(&self, _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{LazyLock, Mutex};
    use tx_hal::{
        Arch, Asid, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapReservation, PmapReserveKind,
        PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
    };
    use tx_subsystems::cred::{sign_cred, Cred};
    use tx_subsystems::ipc::{sysv_msg, sysv_sem, sysv_shm};
    use tx_subsystems::process::nsproxy::sign_init_nsproxy;
    use tx_subsystems::vm::USER_PAGE_SIZE;
    use tx_subsystems::zones;

    struct ProcfsTestPmap;

    impl PlatformConfig for ProcfsTestPmap {
        const ARCH: Arch = Arch::Riscv64;
        const BOARD: &'static str = "procfs-test";
    }

    #[derive(Default)]
    struct ProcfsTestPmapState {
        next_root: usize,
        mappings: BTreeMap<(usize, usize), PhysAddr>,
    }

    static PROCFS_TEST_PMAP_STATE: LazyLock<Mutex<ProcfsTestPmapState>> =
        LazyLock::new(|| Mutex::new(ProcfsTestPmapState::default()));

    fn root_key(root: &PmapRoot) -> usize {
        root.phys().0
    }

    impl PmapIf for ProcfsTestPmap {
        fn create_pmap_root() -> Result<PmapRoot, PmapError> {
            let mut state = PROCFS_TEST_PMAP_STATE.lock().expect("procfs pmap lock");
            let root_id = state.next_root.max(1);
            state.next_root = root_id + 1;
            Ok(PmapRoot::new(
                PtNode::boot_pool(PhysAddr(root_id * USER_PAGE_SIZE)),
                Asid(root_id as u16),
            ))
        }

        fn destroy_pmap_root(root: PmapRoot) {
            let mut state = PROCFS_TEST_PMAP_STATE.lock().expect("procfs pmap lock");
            let key = root.phys().0;
            state.mappings.retain(|(r, _), _| *r != key);
        }

        fn reserve_mapping(
            root: &PmapRoot,
            virt: VirtAddr,
            phys: PhysAddr,
            kind: PmapReserveKind,
        ) -> Result<Option<PmapReservation>, PmapError> {
            let state = PROCFS_TEST_PMAP_STATE.lock().expect("procfs pmap lock");
            if state.mappings.contains_key(&(root_key(root), virt.0)) {
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new(virt, phys, kind)))
        }

        fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

        fn commit_mapping(
            root: &PmapRoot,
            reservation: PmapReservation,
            _permissions: tx_hal::PmapPermissions,
        ) {
            let mut state = PROCFS_TEST_PMAP_STATE.lock().expect("procfs pmap lock");
            state
                .mappings
                .insert((root_key(root), reservation.virt().0), reservation.phys());
        }

        fn unmap_mapping(
            root: &PmapRoot,
            virt: VirtAddr,
            kind: PmapReserveKind,
        ) -> Result<Option<PmapUnmapResult>, PmapError> {
            let mut state = PROCFS_TEST_PMAP_STATE.lock().expect("procfs pmap lock");
            let Some(phys) = state.mappings.remove(&(root_key(root), virt.0)) else {
                return Ok(None);
            };
            Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
        }
    }

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        tx_subsystems::cross_crate_test_support::reset_init_process();
        tx_subsystems::cross_crate_test_support::reset_pid_counter();
        tx_subsystems::cross_crate_test_support::reset_tid_counter();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
    }

    fn root_cred() -> adapter::step_engine::Cap<Cred> {
        sign_cred(Cred::root()).expect("root cred cap")
    }

    fn lookup(fs: &Procfs, parent: FsObjectId, name: &[u8]) -> FsObjectId {
        let guard = adapter::step_engine::guard();
        match fs.lookup(parent, name, &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup({:?}) failed: {other:?}", name),
        }
    }

    #[test]
    fn procfs_sysvipc_files_render_live_sysv_ipc_rows() {
        let _setup = setup();
        let fs = Procfs::new();
        let cred = root_cred();
        let ns = sign_init_nsproxy().expect("nsproxy");

        let msqid = sysv_msg::execution::step_msgget(
            0x4d534750,
            sysv_shm::execution::IPC_CREAT | 0o640,
            &cred,
            &ns,
        )
        .expect("msgget");
        sysv_msg::execution::step_msgsnd(msqid, 5, b"hello".to_vec(), 0, &cred).expect("msgsnd");
        let semid = sysv_sem::execution::step_semget(
            0x53454d50,
            2,
            sysv_shm::execution::IPC_CREAT | 0o660,
            &cred,
            &ns,
        )
        .expect("semget");
        sysv_sem::execution::step_semctl(
            semid,
            0,
            sysv_sem::execution::SETVAL,
            sysv_sem::execution::SemCtlArg::Val(3),
            &cred,
            None,
        )
        .expect("SETVAL");
        let shmid = sysv_shm::execution::step_shmget(
            0x53484d50,
            4096,
            sysv_shm::execution::IPC_CREAT | 0o600,
            &cred,
            &ns,
        )
        .expect("shmget");

        let sysvipc_id = lookup(&fs, PROCFS_ROOT_ID, b"sysvipc");
        let msg_id = lookup(&fs, sysvipc_id, b"msg");
        let sem_id = lookup(&fs, sysvipc_id, b"sem");
        let shm_id = lookup(&fs, sysvipc_id, b"shm");

        let msg = read::render(msg_id);
        assert!(msg.contains("key"));
        assert!(msg.contains(&alloc::format!("{} {}", 0x4d534750, msqid)));
        assert!(msg.contains("5"), "{msg}");

        let sem = read::render(sem_id);
        assert!(sem.contains("nsems"));
        assert!(sem.contains(&alloc::format!("{} {}", 0x53454d50, semid)));
        assert!(sem.contains("2"), "{sem}");

        let shm = read::render(shm_id);
        assert!(shm.contains("bytes"));
        assert!(shm.contains(&alloc::format!("{} {}", 0x53484d50, shmid)));
        assert!(shm.contains("4096"), "{shm}");
    }

    #[test]
    fn procfs_fdinfo_renders_posix_mq_attributes() {
        let _setup = setup();
        let fs = Procfs::new();
        let cred = root_cred();
        let ns = sign_init_nsproxy().expect("nsproxy");
        let proc = tx_subsystems::process::bootstrap_init_process(
            tx_subsystems::vm::AddressSpace::new_cap_for_platform::<ProcfsTestPmap>()
                .expect("aspace"),
        )
        .expect("bootstrap init");
        let mq = tx_subsystems::ipc::posix_mq::execution::step_mq_open(
            b"tx-proc-mq",
            tx_subsystems::ipc::posix_mq::execution::MQ_O_CREAT,
            0o600,
            Some(tx_subsystems::ipc::posix_mq::execution::MqCreateAttr {
                maxmsg: 4,
                msgsize: 32,
            }),
            &cred,
            &ns,
        )
        .expect("mq_open");
        tx_subsystems::ipc::posix_mq::execution::step_mq_send(&mq, b"msg", 7, &cred)
            .expect("mq_send");
        let open_file = tx_subsystems::vfs::OpenFile::new_posix_mq_cap(
            mq,
            tx_subsystems::vfs::OpenFileFlags {
                read: true,
                write: true,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("mq open file");
        proc.install_fd(7, open_file);

        let proc_id = lookup(&fs, PROCFS_ROOT_ID, b"1");
        let fdinfo_dir = lookup(&fs, proc_id, b"fdinfo");
        let fdinfo_id = lookup(&fs, fdinfo_dir, b"7");
        let fdinfo = read::render(fdinfo_id);

        assert!(fdinfo.contains("mq_maxmsg:\t4"), "{fdinfo}");
        assert!(fdinfo.contains("mq_msgsize:\t32"), "{fdinfo}");
        assert!(fdinfo.contains("mq_curmsgs:\t1"), "{fdinfo}");
    }
}
