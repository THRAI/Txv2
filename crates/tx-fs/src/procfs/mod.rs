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
use tx_subsystems::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::{
    render_dentry_path, Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta,
    ProjectionKey, ProjectionSchemaId, RNode, RNodeBacking, StructPayload, S_IFDIR, S_IFLNK,
    S_IFREG,
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
pub const PROCFS_SYS_ID: FsObjectId = FsObjectId::new(0x7072_6F0A);
pub const PROCFS_SYS_KERNEL_ID: FsObjectId = FsObjectId::new(0x7072_6F0B);
pub const PROCFS_SYS_KERNEL_TAINTED_ID: FsObjectId = FsObjectId::new(0x7072_6F0C);
const PROCFS_PID_BASE: u64 = 0x7072_0000;
const PROCFS_PID_OBJECT_STRIDE: u64 = 0x100;
const PROCFS_PID_OBJECT_BASE: u64 = PROCFS_PID_BASE + 0x10000;
const PROCFS_FD_OBJECT_STRIDE: u64 = 0x10000;
const PROCFS_FD_OBJECT_BASE: u64 = PROCFS_PID_BASE + 0x0100_0000;
const PROCFS_FDINFO_OBJECT_BASE: u64 = PROCFS_PID_BASE + 0x0200_0000;
const PROCFS_TASK_OBJECT_BASE: u64 = 0x7073_0000_0000;
const PROCFS_TASK_OBJECT_TAG_MASK: u64 = 0xff;
const PROCFS_NS_OBJECT_BASE: u64 = 0x7074_0000_0000;
const PROCFS_NS_OBJECT_STRIDE: u64 = 0x100;
const PROCFS_TASK_TAG_TID_DIR: u64 = 0;
const PROCFS_TASK_TAG_STAT: u64 = 1;
const PROCFS_TAG_STAT: u64 = 0;
const PROCFS_TAG_CMDLINE: u64 = 1;
const PROCFS_TAG_MEM: u64 = 2;
const PROCFS_TAG_MAPS: u64 = 3;
const PROCFS_TAG_EXE: u64 = 4;
const PROCFS_TAG_FD_DIR: u64 = 5;
const PROCFS_TAG_TASK_DIR: u64 = 6;
const PROCFS_TAG_FDINFO_DIR: u64 = 7;
const PROCFS_NS_TAG_NET: u64 = 1;
const fn pid_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64)
}
const fn pid_object_id(pid: Pid, tag: u64) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_OBJECT_BASE + pid.0 as u64 * PROCFS_PID_OBJECT_STRIDE + tag)
}
const fn pid_stat_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_STAT)
}
const fn pid_cmdline_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_CMDLINE)
}
const fn pid_mem_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_MEM)
}
const fn pid_maps_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_MAPS)
}
const fn pid_exe_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_EXE)
}
const fn pid_fd_dir_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_FD_DIR)
}
const fn pid_task_dir_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_TASK_DIR)
}
const fn pid_fdinfo_dir_id(pid: Pid) -> FsObjectId {
    pid_object_id(pid, PROCFS_TAG_FDINFO_DIR)
}
const fn pid_fdinfo_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_FDINFO_OBJECT_BASE + pid.0 as u64 * PROCFS_FD_OBJECT_STRIDE + fd as u64)
}
const fn task_tid_dir_id(pid: Pid, tid: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_TASK_OBJECT_BASE + ((pid.0 as u64) << 32) + ((tid as u64) << 8))
}
const fn task_tid_stat_id(pid: Pid, tid: u32) -> FsObjectId {
    FsObjectId::new(
        PROCFS_TASK_OBJECT_BASE
            + ((pid.0 as u64) << 32)
            + ((tid as u64) << 8)
            + PROCFS_TASK_TAG_STAT,
    )
}
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
const fn pid_fd_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_FD_OBJECT_BASE + pid.0 as u64 * PROCFS_FD_OBJECT_STRIDE + fd as u64)
}
const fn pid_ns_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_NS_OBJECT_BASE + pid.0 as u64 * PROCFS_NS_OBJECT_STRIDE)
}
const fn pid_netns_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(
        PROCFS_NS_OBJECT_BASE + pid.0 as u64 * PROCFS_NS_OBJECT_STRIDE + PROCFS_NS_TAG_NET,
    )
}
fn pid_from_object_id(id: FsObjectId, tag: u64) -> Option<Pid> {
    let r = id.as_u64();
    if !(PROCFS_PID_OBJECT_BASE..PROCFS_FD_OBJECT_BASE).contains(&r) {
        return None;
    }
    let offset = r - PROCFS_PID_OBJECT_BASE;
    if offset % PROCFS_PID_OBJECT_STRIDE == tag {
        Some(Pid((offset / PROCFS_PID_OBJECT_STRIDE) as u32))
    } else {
        None
    }
}
pub fn pid_from_mem_id(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_MEM)
}
pub fn pid_from_maps_id(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_MAPS)
}
pub fn pid_from_exe_id(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_EXE)
}
fn pid_from_fd_dir(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_FD_DIR)
}
fn pid_from_fd_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    if !(PROCFS_FD_OBJECT_BASE..PROCFS_FDINFO_OBJECT_BASE).contains(&r) {
        return None;
    }
    let offset = r - PROCFS_FD_OBJECT_BASE;
    let pid = Pid((offset / PROCFS_FD_OBJECT_STRIDE) as u32);
    let fd = (offset % PROCFS_FD_OBJECT_STRIDE) as u32;
    Some((pid, fd))
}
fn pid_from_ns_object_id(id: FsObjectId, tag: u64) -> Option<Pid> {
    let r = id.as_u64();
    if !(PROCFS_NS_OBJECT_BASE..PROCFS_NS_OBJECT_BASE + 0x0100_0000).contains(&r) {
        return None;
    }
    let offset = r - PROCFS_NS_OBJECT_BASE;
    if offset % PROCFS_NS_OBJECT_STRIDE == tag {
        Some(Pid((offset / PROCFS_NS_OBJECT_STRIDE) as u32))
    } else {
        None
    }
}
fn pid_from_ns_dir(id: FsObjectId) -> Option<Pid> {
    pid_from_ns_object_id(id, 0)
}
fn pid_from_netns_id(id: FsObjectId) -> Option<Pid> {
    pid_from_ns_object_id(id, PROCFS_NS_TAG_NET)
}
fn pid_from_fdinfo_dir(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_FDINFO_DIR)
}
pub fn pid_from_fdinfo_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    if !(PROCFS_FDINFO_OBJECT_BASE..PROCFS_TASK_OBJECT_BASE).contains(&r) {
        return None;
    }
    let offset = r - PROCFS_FDINFO_OBJECT_BASE;
    let pid = Pid((offset / PROCFS_FD_OBJECT_STRIDE) as u32);
    let fd = (offset % PROCFS_FD_OBJECT_STRIDE) as u32;
    Some((pid, fd))
}

fn pid_from_dir(id: FsObjectId) -> Option<Pid> {
    match id {
        PROCFS_ROOT_ID
        | PROCFS_SELF_ID
        | PROCFS_MOUNTS_ID
        | PROCFS_CPUINFO_ID
        | PROCFS_UPTIME_ID
        | PROCFS_MEMINFO_ID
        | PROCFS_SYS_ID
        | PROCFS_SYS_KERNEL_ID
        | PROCFS_SYS_KERNEL_TAINTED_ID
        | PROCFS_CONFIG_ID
        | PROCFS_SYS_FS_ID
        | PROCFS_SYS_FS_PIPE_MAX_SIZE_ID
        | PROCFS_SYS_FS_LEASE_BREAK_TIME_ID
        | PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID
        | PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID
        | PROCFS_SYSVIPC_ID
        | PROCFS_SYSVIPC_MSG_ID
        | PROCFS_SYSVIPC_SEM_ID
        | PROCFS_SYSVIPC_SHM_ID => return None,
        _ => {}
    }
    let r = id.as_u64();
    if r > PROCFS_PID_BASE && r < PROCFS_PID_OBJECT_BASE {
        Some(Pid((r - PROCFS_PID_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_stat_id(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_STAT)
}
pub fn pid_from_cmdline_id(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_CMDLINE)
}
fn pid_from_task_dir(id: FsObjectId) -> Option<Pid> {
    pid_from_object_id(id, PROCFS_TAG_TASK_DIR)
}
fn task_from_object_id(id: FsObjectId, tag: u64) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    if !(PROCFS_TASK_OBJECT_BASE..PROCFS_NS_OBJECT_BASE).contains(&r) {
        return None;
    }
    let offset = r - PROCFS_TASK_OBJECT_BASE;
    if offset & PROCFS_TASK_OBJECT_TAG_MASK != tag {
        return None;
    }
    let pid = (offset >> 32) as u32;
    let tid = ((offset >> 8) & 0x00ff_ffff) as u32;
    Some((Pid(pid), tid))
}
fn task_from_tid_dir(id: FsObjectId) -> Option<(Pid, u32)> {
    task_from_object_id(id, PROCFS_TASK_TAG_TID_DIR)
}
pub fn task_from_stat_id(id: FsObjectId) -> Option<(Pid, u32)> {
    task_from_object_id(id, PROCFS_TASK_TAG_STAT)
}

fn procfs_number_exists(n: u32) -> bool {
    if process::process_by_pid(Pid(n)).is_some() {
        return true;
    }
    matches!(
        resolve_pid_number_as(n as u64, PidNameKind::Thread),
        Some(PidName::Thread(thread)) if thread.upgrade_owner_proc().is_some()
    )
}

fn process_for_procfs_number(n: u32) -> Option<Cap<process::ProcessIdentity>> {
    if let Some(proc) = process::process_by_pid(Pid(n)) {
        return Some(proc);
    }
    match resolve_pid_number_as(n as u64, PidNameKind::Thread) {
        Some(PidName::Thread(thread)) => thread.upgrade_owner_proc(),
        _ => None,
    }
}

fn dir_entry(id: FsObjectId, kind: InodeKind, name: &[u8]) -> DirEntry {
    DirEntry::new(id, kind, name).expect("procfs dir entry name")
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

pub const PROCFS_DIR_MODE: u16 = S_IFDIR | 0o555;
pub const PROCFS_FILE_MODE: u16 = S_IFREG | 0o444;
pub const PROCFS_FILE_RW_MODE: u16 = S_IFREG | 0o644;
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
            if name == b"config" {
                return StepOutcome::done(PROCFS_CONFIG_ID);
            }
            if name == b"sys" {
                return StepOutcome::done(PROCFS_SYS_ID);
            }
            if name == b"sysvipc" {
                return StepOutcome::done(PROCFS_SYSVIPC_ID);
            }
            if name == b"sys" {
                return StepOutcome::done(PROCFS_SYS_ID);
            }
            if let Ok(n) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() {
                if n > 0 && procfs_number_exists(n) {
                    return StepOutcome::done(pid_dir_id(Pid(n)));
                }
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if parent == PROCFS_SYS_ID {
            if name == b"kernel" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_ID);
            }
            if name == b"fs" {
                return StepOutcome::done(PROCFS_SYS_FS_ID);
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if parent == PROCFS_SYS_FS_ID {
            if name == b"pipe-max-size" {
                return StepOutcome::done(PROCFS_SYS_FS_PIPE_MAX_SIZE_ID);
            }
            if name == b"lease-break-time" {
                return StepOutcome::done(PROCFS_SYS_FS_LEASE_BREAK_TIME_ID);
            }
            if name == b"protected_hardlinks" {
                return StepOutcome::done(PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID);
            }
            if name == b"protected_symlinks" {
                return StepOutcome::done(PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID);
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if parent == PROCFS_SYS_KERNEL_ID {
            if name == b"tainted" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_TAINTED_ID);
            }
            return StepOutcome::err(Errno::ENOENT);
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
            return StepOutcome::err(Errno::ENOENT);
        }
        if parent == PROCFS_NET_ID {
            return match name {
                b"route" => StepOutcome::done(PROCFS_NET_ROUTE_ID),
                b"arp" => StepOutcome::done(PROCFS_NET_ARP_ID),
                b"dev" => StepOutcome::done(PROCFS_NET_DEV_ID),
                b"tx_nf_rules" => StepOutcome::done(PROCFS_NET_TX_NF_RULES_ID),
                b"nf_conntrack" => StepOutcome::done(PROCFS_NET_NF_CONNTRACK_ID),
                b"tcp" => StepOutcome::done(PROCFS_NET_TCP_ID),
                b"udp" => StepOutcome::done(PROCFS_NET_UDP_ID),
                b"raw" => StepOutcome::done(PROCFS_NET_RAW_ID),
                b"snmp" => StepOutcome::done(PROCFS_NET_SNMP_ID),
                b"netlink" => StepOutcome::done(PROCFS_NET_NETLINK_ID),
                b"if_inet6" => StepOutcome::done(PROCFS_NET_IF_INET6_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == PROCFS_SYS_ID {
            return match name {
                b"net" => StepOutcome::done(PROCFS_SYS_NET_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == PROCFS_SYS_NET_ID {
            return match name {
                b"ipv4" => StepOutcome::done(PROCFS_SYS_NET_IPV4_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == PROCFS_SYS_NET_IPV4_ID {
            return match name {
                b"ip_forward" => StepOutcome::done(PROCFS_SYS_NET_IPV4_IP_FORWARD_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == PROCFS_SYS_ID {
            if name == b"kernel" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_KERNEL_ID {
            if name == b"tainted" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_TAINTED_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if let Some(pid) = pid_from_dir(parent) {
            if name == b"stat" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_stat_id(pid));
            }
            if name == b"cmdline" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_cmdline_id(pid));
            }
            if name == b"mem" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_mem_id(pid));
            }
            if name == b"maps" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_maps_id(pid));
            }
            if name == b"exe" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_exe_id(pid));
            }
            if name == b"fd" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_fd_dir_id(pid));
            }
            if name == b"task" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_task_dir_id(pid));
            }
            if name == b"fdinfo" && process_for_procfs_number(pid.0).is_some() {
                return StepOutcome::done(pid_fdinfo_dir_id(pid));
            }
            if name == b"ns" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_ns_dir_id(pid));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if let Some(pid) = pid_from_fdinfo_dir(parent) {
            let Ok(fd) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() else {
                return StepOutcome::err(Errno::ENOENT);
            };
            if process::process_by_pid(pid)
                .and_then(|proc| proc.fd(fd))
                .is_some()
            {
                return StepOutcome::done(pid_fdinfo_id(pid, fd));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if let Some(pid) = pid_from_task_dir(parent) {
            if let Ok(n) = core::str::from_utf8(name).unwrap_or("").parse::<u32>() {
                if process::process_by_pid(pid)
                    .and_then(|proc| proc.thread_by_tid(n))
                    .is_some()
                {
                    return StepOutcome::done(task_tid_dir_id(pid, n));
                }
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if let Some((pid, tid)) = task_from_tid_dir(parent) {
            if name == b"stat"
                && process::process_by_pid(pid)
                    .and_then(|proc| proc.thread_by_tid(tid))
                    .is_some()
            {
                return StepOutcome::done(task_tid_stat_id(pid, tid));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if let Some(pid) = pid_from_ns_dir(parent) {
            if name == b"net" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_netns_id(pid));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        StepOutcome::err(Errno::ENOENT)
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
            PROCFS_SYS_ID
            | PROCFS_SYS_KERNEL_ID
            | PROCFS_SYS_FS_ID
            | PROCFS_SYSVIPC_ID
            | PROCFS_NET_ID
            | PROCFS_SYS_NET_ID
            | PROCFS_SYS_NET_IPV4_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SELF_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Symlink, PROCFS_SYMLINK_MODE))
            }
            PROCFS_MOUNTS_ID
            | PROCFS_CPUINFO_ID
            | PROCFS_UPTIME_ID
            | PROCFS_MEMINFO_ID
            | PROCFS_CONFIG_ID
            | PROCFS_SYS_KERNEL_TAINTED_ID
            | PROCFS_NET_ROUTE_ID
            | PROCFS_NET_ARP_ID
            | PROCFS_NET_DEV_ID
            | PROCFS_NET_NF_CONNTRACK_ID
            | PROCFS_NET_TCP_ID
            | PROCFS_NET_UDP_ID
            | PROCFS_NET_RAW_ID
            | PROCFS_NET_SNMP_ID
            | PROCFS_NET_NETLINK_ID
            | PROCFS_NET_IF_INET6_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_SYSVIPC_ID | PROCFS_SYS_ID | PROCFS_SYS_KERNEL_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SYSVIPC_MSG_ID
            | PROCFS_SYSVIPC_SEM_ID
            | PROCFS_SYSVIPC_SHM_ID
            | PROCFS_SYS_KERNEL_TAINTED_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_SYS_NET_IPV4_IP_FORWARD_ID | PROCFS_NET_TX_NF_RULES_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_RW_MODE))
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
            id if pid_from_task_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if task_from_tid_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if task_from_stat_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_ns_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_netns_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            _ => StepOutcome::err(Errno::ENOENT),
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

        if id == PROCFS_NET_ID {
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                (b"route", PROCFS_NET_ROUTE_ID, InodeKind::Regular),
                (b"arp", PROCFS_NET_ARP_ID, InodeKind::Regular),
                (b"dev", PROCFS_NET_DEV_ID, InodeKind::Regular),
                (
                    b"tx_nf_rules",
                    PROCFS_NET_TX_NF_RULES_ID,
                    InodeKind::Regular,
                ),
                (
                    b"nf_conntrack",
                    PROCFS_NET_NF_CONNTRACK_ID,
                    InodeKind::Regular,
                ),
                (b"tcp", PROCFS_NET_TCP_ID, InodeKind::Regular),
                (b"udp", PROCFS_NET_UDP_ID, InodeKind::Regular),
                (b"raw", PROCFS_NET_RAW_ID, InodeKind::Regular),
                (b"snmp", PROCFS_NET_SNMP_ID, InodeKind::Regular),
                (b"netlink", PROCFS_NET_NETLINK_ID, InodeKind::Regular),
                (b"if_inet6", PROCFS_NET_IF_INET6_ID, InodeKind::Regular),
            ];
            if idx < files.len() {
                let (name, oid, kind) = files[idx];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor::from_u64((idx + 1) as u64),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_ID {
            let files: &[(&[u8], FsObjectId, InodeKind)] =
                &[(b"net", PROCFS_SYS_NET_ID, InodeKind::Directory)];
            if idx < files.len() {
                let (name, oid, kind) = files[idx];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor::from_u64((idx + 1) as u64),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_NET_ID {
            let files: &[(&[u8], FsObjectId, InodeKind)] =
                &[(b"ipv4", PROCFS_SYS_NET_IPV4_ID, InodeKind::Directory)];
            if idx < files.len() {
                let (name, oid, kind) = files[idx];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor::from_u64((idx + 1) as u64),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_NET_IPV4_ID {
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[(
                b"ip_forward",
                PROCFS_SYS_NET_IPV4_IP_FORWARD_ID,
                InodeKind::Regular,
            )];
            if idx < files.len() {
                let (name, oid, kind) = files[idx];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor::from_u64((idx + 1) as u64),
                )));
            }
            return StepOutcome::done(None);
        }

        if let Some(pid) = pid_from_dir(id) {
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
                (b"task", pid_task_dir_id(pid), InodeKind::Directory),
                (b"fdinfo", pid_fdinfo_dir_id(pid), InodeKind::Directory),
                (b"ns", pid_ns_dir_id(pid), InodeKind::Directory),
            ];
            let fi = idx.saturating_sub(2);
            if fi < files.len() {
                let (name, oid, kind) = files[fi];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor([2, (fi + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if let Some(pid) = pid_from_task_dir(id) {
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            if let Some(proc) = process::process_by_pid(pid) {
                let mut threads = proc.threads_snapshot().unwrap_or_default();
                threads.sort_by_key(|thread| thread.tid.0);
                let ti = idx.saturating_sub(2);
                if let Some(thread) = threads.get(ti) {
                    let tid = thread.tid.0;
                    let s = alloc::format!("{}", tid);
                    return StepOutcome::done(Some((
                        dir_entry(
                            task_tid_dir_id(pid, tid),
                            InodeKind::Directory,
                            s.as_bytes(),
                        ),
                        DirCursor([2, (ti + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
            }
            return StepOutcome::done(None);
        }

        if let Some((pid, tid)) = task_from_tid_dir(id) {
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            if idx == 2
                && process::process_by_pid(pid)
                    .and_then(|proc| proc.thread_by_tid(tid))
                    .is_some()
            {
                return StepOutcome::done(Some((
                    dir_entry(task_tid_stat_id(pid, tid), InodeKind::Regular, b"stat"),
                    DirCursor([2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_ID {
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                (b"kernel", PROCFS_SYS_KERNEL_ID, InodeKind::Directory),
                (b"fs", PROCFS_SYS_FS_ID, InodeKind::Directory),
            ];
            let fi = idx.saturating_sub(2);
            if fi < files.len() {
                let (name, oid, kind) = files[fi];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor([2, (fi + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_FS_ID {
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                (
                    b"pipe-max-size",
                    PROCFS_SYS_FS_PIPE_MAX_SIZE_ID,
                    InodeKind::Regular,
                ),
                (
                    b"lease-break-time",
                    PROCFS_SYS_FS_LEASE_BREAK_TIME_ID,
                    InodeKind::Regular,
                ),
                (
                    b"protected_hardlinks",
                    PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID,
                    InodeKind::Regular,
                ),
                (
                    b"protected_symlinks",
                    PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID,
                    InodeKind::Regular,
                ),
            ];
            let fi = idx.saturating_sub(2);
            if fi < files.len() {
                let (name, oid, kind) = files[fi];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor([2, (fi + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if id == PROCFS_SYS_KERNEL_ID {
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            if idx == 2 {
                return StepOutcome::done(Some((
                    dir_entry(PROCFS_SYS_KERNEL_TAINTED_ID, InodeKind::Regular, b"tainted"),
                    DirCursor([2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

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
                    DirCursor([2, (fi + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
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
                    DirCursor([2, (fi + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                )));
            }
            return StepOutcome::done(None);
        }

        if let Some(pid) = pid_from_ns_dir(id) {
            let files: &[(&[u8], FsObjectId, InodeKind)] =
                &[(b"net", pid_netns_id(pid), InodeKind::Regular)];
            if idx < files.len() {
                let (name, oid, kind) = files[idx];
                return StepOutcome::done(Some((
                    dir_entry(oid, kind, name),
                    DirCursor::from_u64((idx + 1) as u64),
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
            if id == PROCFS_SYS_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                if idx.saturating_sub(2) == 0 {
                    return StepOutcome::done(Some((
                        dir_entry(PROCFS_SYS_KERNEL_ID, InodeKind::Directory, b"kernel"),
                        DirCursor([2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
                return StepOutcome::done(None);
            }
            if id == PROCFS_SYS_KERNEL_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                if idx.saturating_sub(2) == 0 {
                    return StepOutcome::done(Some((
                        dir_entry(PROCFS_SYS_KERNEL_TAINTED_ID, InodeKind::Regular, b"tainted"),
                        DirCursor([2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
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
            (b"config", PROCFS_CONFIG_ID, InodeKind::Regular),
            (b"sys", PROCFS_SYS_ID, InodeKind::Directory),
            (b"sysvipc", PROCFS_SYSVIPC_ID, InodeKind::Directory),
            (b"sys", PROCFS_SYS_ID, InodeKind::Directory),
        ];
        let si = idx.saturating_sub(2);
        if state_byte == 2 && si < statics.len() {
            let (name, oid, kind) = statics[si];
            return StepOutcome::done(Some((
                dir_entry(oid, kind, name),
                DirCursor([2, (si + 3) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
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
                return StepOutcome::err(Errno::ENOENT);
            };
            let Some(_open_file) = proc.fd(fd_num) else {
                return StepOutcome::err(Errno::ENOENT);
            };
            // v1: render as "fd:N" since we don't have reverse-path from OpenFile.
            let target = alloc::format!("anon_inode:[{}]", fd_num);
            StepOutcome::done(target.into_bytes().into_boxed_slice())
        } else if let Some(pid) = pid_from_exe_id(id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT);
            };
            let Some(exe_dentry) = proc.exe_file() else {
                return StepOutcome::err(Errno::ENOENT);
            };
            match render_dentry_path(&exe_dentry) {
                Some(path) => StepOutcome::done(path.into_boxed_slice()),
                None => StepOutcome::err(Errno::ENOENT),
            }
        } else {
            StepOutcome::err(Errno::ENOENT)
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
        StepOutcome::err(Errno::EROFS)
    }
    fn mkdir(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: u16,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn unlink(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn rmdir(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn symlink(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: &[u8],
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn rename(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &[u8],
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn link(
        &self,
        _: FsObjectId,
        _: &[u8],
        _: FsObjectId,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
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
        if let Some(pid) = pid_from_netns_id(id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT);
            };
            let Some(payload) = proc.net_namespace() else {
                return StepOutcome::err(Errno::ESRCH);
            };
            return match RNode::new_cap_in_mount(
                id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::NetNamespace { payload },
                },
                mount,
            ) {
                Ok(cap) => StepOutcome::done(cap),
                Err(_) => StepOutcome::err(Errno::ENOMEM),
            };
        }

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
            Err(_) => StepOutcome::err(Errno::ENOMEM),
        }
    }

    fn step_read_projected(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        buf: &mut [u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        self.step_read_projected_with_netns(fs_object_id, offset, buf, None, guard)
    }

    fn step_read_projected_with_netns(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        buf: &mut [u8],
        caller_netns: Option<&tx_subsystems::net::NetNamespacePayload>,
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
            let src = UserPtr::<u8>::new(offset as usize);
            match aspace.copy_from_user(buf, src, guard) {
                StepOutcome::Done(n) => StepOutcome::done(n as u64),
                StepOutcome::Err(e) => StepOutcome::err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    StepOutcome::err(Errno::EIO)
                }
            }
        } else {
            // Other projected files: render content via read::render.
            let content: Vec<u8> = read::render_with_netns(fs_object_id, caller_netns).into_bytes();
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

    fn step_write_projected(
        &self,
        fs_object_id: FsObjectId,
        _offset: u64,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        self.step_write_projected_with_netns(fs_object_id, _offset, bytes, None, _guard)
    }

    fn step_write_projected_with_netns(
        &self,
        fs_object_id: FsObjectId,
        _offset: u64,
        bytes: &[u8],
        caller_netns: Option<&tx_subsystems::net::NetNamespacePayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        let default_netns;
        let netns = match caller_netns {
            Some(netns) => netns,
            None => {
                default_netns = tx_subsystems::net::initial_net_namespace_payload();
                &default_netns
            }
        };

        if fs_object_id == PROCFS_NET_TX_NF_RULES_ID {
            return match tx_subsystems::net::apply_netfilter_control_command(netns, bytes) {
                Ok(()) => StepOutcome::done(bytes.len() as u64),
                Err(errno) => StepOutcome::err(errno),
            };
        }

        if matches!(
            fs_object_id,
            PROCFS_SYS_FS_PIPE_MAX_SIZE_ID
                | PROCFS_SYS_FS_LEASE_BREAK_TIME_ID
                | PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID
                | PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID
        ) {
            return StepOutcome::done(bytes.len() as u64);
        }

        if fs_object_id != PROCFS_SYS_NET_IPV4_IP_FORWARD_ID {
            return StepOutcome::err(Errno::EROFS);
        }
        let trimmed = trim_ascii_space(bytes);
        let enabled = match trimmed {
            b"0" => false,
            b"1" => true,
            _ => return StepOutcome::err(Errno::EINVAL),
        };
        netns.set_ipv4_forwarding_for_test_or_bootstrap(enabled);
        StepOutcome::done(bytes.len() as u64)
    }
    fn step_chmod(
        &self,
        _: FsObjectId,
        _: u16,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
    fn step_chown(
        &self,
        _: FsObjectId,
        _: Option<u32>,
        _: Option<u32>,
        _: &Credential,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
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
        let next_idx = if idx == 0 { 1 } else { 2 };
        let next_state = if idx == 0 { state_byte } else { 2 };
        let next = DirCursor([
            next_state, next_idx, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        StepOutcome::done(Some((entry, next)))
    } else {
        StepOutcome::done(None)
    }
}

impl FsPageBacking for Procfs {
    fn fetch_page(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<Frame, NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }
    fn flush_page(
        &self,
        _: FsObjectId,
        _: u64,
        _: &Frame,
        _: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }
    fn truncate(&self, _: FsObjectId, _: u64, _: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }
    fn fsync_file(&self, _: FsObjectId, _: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_subsystems::device::DevT;
    use tx_subsystems::net::{
        create_veth_pair_for_test_or_bootstrap, EthernetAddress, Ipv4Address, NetAdminAuthority,
        VethEndpointConfig, VethPairConfig, VETH_DEFAULT_MTU,
    };
    use tx_subsystems::zones;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        tx_subsystems::cross_crate_test_support::reset_init_process();
        tx_subsystems::cross_crate_test_support::reset_pid_counter();
        tx_subsystems::cross_crate_test_support::reset_tid_counter();
        let _ = zones::register_all();
        tx_subsystems::net::reset_initial_net_namespace_for_test();
        tx_subsystems::net::reset_netfilter_for_test();
        tx_test_support::drain_to_quiescence();
        guard
    }

    fn read_one(fs: &Procfs, id: FsObjectId, cursor: DirCursor) -> (DirEntry, DirCursor) {
        let guard = adapter::step_engine::guard();
        match fs.readdir(id, cursor, &guard) {
            StepOutcome::Done(Some(entry)) => entry,
            other => panic!("procfs readdir returned {other:?}"),
        }
    }

    #[test]
    fn root_readdir_cursor_advances_through_static_entries() {
        tx_test_support::init_host();
        let fs = Procfs::new();
        let (dot, cursor) = read_one(&fs, PROCFS_ROOT_ID, DirCursor::START);
        assert_eq!(dot.name.as_bytes(), b".");
        let (dotdot, cursor) = read_one(&fs, PROCFS_ROOT_ID, cursor);
        assert_eq!(dotdot.name.as_bytes(), b"..");
        let (self_entry, cursor) = read_one(&fs, PROCFS_ROOT_ID, cursor);
        assert_eq!(self_entry.name.as_bytes(), b"self");
        let (mounts_entry, cursor) = read_one(&fs, PROCFS_ROOT_ID, cursor);
        assert_eq!(mounts_entry.name.as_bytes(), b"mounts");
        let (cpuinfo_entry, _) = read_one(&fs, PROCFS_ROOT_ID, cursor);
        assert_eq!(cpuinfo_entry.name.as_bytes(), b"cpuinfo");
    }

    #[test]
    fn procfs_meminfo_renders_ltp_parseable_linux_fields() {
        let meminfo = read::render(PROCFS_MEMINFO_ID);

        assert!(meminfo.contains("MemTotal:"), "{meminfo}");
        assert!(meminfo.contains("MemFree:"), "{meminfo}");
        assert!(meminfo.contains("MemAvailable:"), "{meminfo}");
        assert!(meminfo.contains("Cached:"), "{meminfo}");
        assert!(meminfo.contains("SwapTotal:"), "{meminfo}");
        for key in ["MemTotal:", "MemFree:", "MemAvailable:"] {
            let line = meminfo
                .lines()
                .find(|line| line.starts_with(key))
                .expect("field present");
            let value = line[key.len()..]
                .split_ascii_whitespace()
                .next()
                .expect("numeric value");
            assert!(value.parse::<u64>().expect("numeric kB value") > 0);
        }
    }

    #[test]
    fn procfs_sys_kernel_tainted_exists_and_renders_zero() {
        let _setup = setup();
        let fs = Procfs::new();
        let sys_id = lookup(&fs, PROCFS_ROOT_ID, b"sys");
        let kernel_id = lookup(&fs, sys_id, b"kernel");
        let tainted_id = lookup(&fs, kernel_id, b"tainted");
        let guard = adapter::step_engine::guard();

        assert!(matches!(
            fs.load_inode_meta(tainted_id, &guard),
            StepOutcome::Done(meta) if meta.kind() == InodeKind::Regular
        ));
        assert_eq!(read::render(tainted_id), "0\n");
    }

    #[test]
    fn procfs_fdinfo_renders_posix_mq_attributes() {
        let _setup = setup();
        let guard = adapter::step_engine::guard();
        let procfs = Procfs::new();
        let auth = NetAdminAuthority::for_test_or_bootstrap();
        let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "proc-route0",
                devt: DevT::new(98, 1),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 1]),
            },
            right: VethEndpointConfig {
                name: "proc-peer0",
                devt: DevT::new(98, 2),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 2]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let netns = tx_subsystems::net::initial_net_namespace_payload();
        netns
            .attach_device_for_test_or_bootstrap(pair.left, None)
            .expect("attach proc-route0");
        let ifindex = netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.name == "proc-route0")
            .expect("proc-route0 link")
            .ifindex;
        netns
            .set_device_ipv4_addr_by_ifindex(
                auth,
                ifindex,
                Some(Ipv4Address::new([172, 17, 0, 1])),
                Some(16),
            )
            .expect("set proc-route0 addr");

        let mut out = [0u8; 512];
        let read = match procfs.step_read_projected(PROCFS_NET_ROUTE_ID, 0, &mut out, &guard) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("route read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("route text utf8");

        assert!(text.contains("Iface"));
        assert!(text.contains("proc-route0"));
        assert!(text.contains("000011AC"));
        assert!(text.contains("0000FFFF"));
    }

    #[test]
    fn procfs_net_dev_renders_loopback_and_namespace_links() {
        let _lock = setup();
        let guard = tx_substrate::epoch::guard();
        let procfs = Procfs::new();
        let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "procdev0",
                devt: DevT::new(98, 11),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 1, 1]),
            },
            right: VethEndpointConfig {
                name: "procpeer0",
                devt: DevT::new(98, 12),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 1, 2]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let netns = tx_subsystems::net::initial_net_namespace_payload();
        netns
            .attach_device_for_test_or_bootstrap(pair.left, None)
            .expect("attach procdev0");

        let mut out = [0u8; 512];
        let read = match procfs.step_read_projected(PROCFS_NET_DEV_ID, 0, &mut out, &guard) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("dev read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("dev text utf8");

        assert!(text.contains("Inter-|"));
        assert!(text.contains("lo"));
        assert!(text.contains("procdev0"));
    }

    #[test]
    fn procfs_ip_forward_read_write_toggles_initial_namespace_forwarding() {
        let _setup = setup();
        let guard = adapter::step_engine::guard();
        let procfs = Procfs::new();
        let netns = tx_subsystems::net::initial_net_namespace_payload();
        assert!(!netns.ipv4_forwarding_enabled());

        assert_eq!(
            procfs.step_write_projected(PROCFS_SYS_NET_IPV4_IP_FORWARD_ID, 0, b"1\n", &guard,),
            StepOutcome::Done(2)
        );
        assert!(netns.ipv4_forwarding_enabled());

        let mut out = [0u8; 8];
        let read = match procfs.step_read_projected(
            PROCFS_SYS_NET_IPV4_IP_FORWARD_ID,
            0,
            &mut out,
            &guard,
        ) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("ip_forward read failed: {other:?}"),
        };
        assert_eq!(&out[..read], b"1\n");

        assert_eq!(
            procfs.step_write_projected(PROCFS_SYS_NET_IPV4_IP_FORWARD_ID, 0, b"0\n", &guard,),
            StepOutcome::Done(2)
        );
        assert!(!netns.ipv4_forwarding_enabled());
    }

    #[test]
    fn procfs_netfilter_rules_file_adds_lists_and_deletes_masquerade_rule() {
        let _setup = setup();
        let guard = adapter::step_engine::guard();
        let procfs = Procfs::new();
        let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "proc-nf0",
                devt: DevT::new(99, 1),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 1, 1]),
            },
            right: VethEndpointConfig {
                name: "proc-nf-peer0",
                devt: DevT::new(99, 2),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 1, 2]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let netns = tx_subsystems::net::initial_net_namespace_payload();
        netns
            .attach_device_for_test_or_bootstrap(pair.left, None)
            .expect("attach proc-nf0");

        let command = b"masquerade 172.17.0.0/16 proc-nf0\n";
        assert_eq!(
            procfs.step_write_projected(PROCFS_NET_TX_NF_RULES_ID, 0, command, &guard),
            StepOutcome::Done(command.len() as u64)
        );

        let mut out = [0u8; 512];
        let read = match procfs.step_read_projected(PROCFS_NET_TX_NF_RULES_ID, 0, &mut out, &guard)
        {
            StepOutcome::Done(read) => read as usize,
            other => panic!("netfilter rules read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("netfilter rules text utf8");
        assert!(text.contains("nat"));
        assert!(text.contains("MASQUERADE"));
        assert!(text.contains("172.17.0.0/16"));
        assert!(text.contains("proc-nf0"));

        let dnat = b"dnat tcp 10.0.2.15 8080 172.17.0.2 80\n";
        assert_eq!(
            procfs.step_write_projected(PROCFS_NET_TX_NF_RULES_ID, 0, dnat, &guard),
            StepOutcome::Done(dnat.len() as u64)
        );
        let read = match procfs.step_read_projected(PROCFS_NET_TX_NF_RULES_ID, 0, &mut out, &guard)
        {
            StepOutcome::Done(read) => read as usize,
            other => panic!("netfilter dnat rules read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("netfilter dnat text utf8");
        assert!(text.contains("DNAT"));
        assert!(text.contains("tcp"));
        assert!(text.contains("10.0.2.15/32"));
        assert!(text.contains("8080"));
        assert!(text.contains("172.17.0.2:80"));

        assert_eq!(
            procfs.step_write_projected(PROCFS_NET_TX_NF_RULES_ID, 0, b"delete 0\n", &guard),
            StepOutcome::Done(9)
        );
        assert_eq!(tx_subsystems::net::netfilter_rules_snapshot().len(), 1);
        assert_eq!(
            procfs.step_write_projected(PROCFS_NET_TX_NF_RULES_ID, 0, b"flush\n", &guard),
            StepOutcome::Done(6)
        );
        assert!(tx_subsystems::net::netfilter_rules_snapshot().is_empty());
    }

    #[test]
    fn procfs_net_files_use_supplied_caller_network_namespace() {
        let _lock = setup();
        let guard = tx_substrate::epoch::guard();
        let procfs = Procfs::new();
        let auth = NetAdminAuthority::for_test_or_bootstrap();

        let initial_pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "proc-initial0",
                devt: DevT::new(100, 1),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 2, 1]),
            },
            right: VethEndpointConfig {
                name: "proc-initial-peer0",
                devt: DevT::new(100, 2),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 2, 2]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let initial = tx_subsystems::net::initial_net_namespace_payload();
        initial
            .attach_device_for_test_or_bootstrap(initial_pair.left, None)
            .expect("attach initial iface");
        let initial_ifindex = initial
            .link_snapshot()
            .into_iter()
            .find(|link| link.name == "proc-initial0")
            .expect("initial link")
            .ifindex;
        initial
            .set_device_ipv4_addr_by_ifindex(
                auth,
                initial_ifindex,
                Some(Ipv4Address::new([10, 99, 0, 1])),
                Some(24),
            )
            .expect("set initial addr");

        let isolated = tx_subsystems::net::create_isolated_net_namespace_for_test("procfs-netns")
            .expect("isolated netns")
            .payload_cap()
            .expect("isolated payload");
        let isolated_pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "proc-iso0",
                devt: DevT::new(100, 3),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 2, 3]),
            },
            right: VethEndpointConfig {
                name: "proc-iso-peer0",
                devt: DevT::new(100, 4),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 2, 4]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        isolated
            .attach_device_for_test_or_bootstrap(isolated_pair.left, None)
            .expect("attach isolated iface");
        let isolated_ifindex = isolated
            .link_snapshot()
            .into_iter()
            .find(|link| link.name == "proc-iso0")
            .expect("isolated link")
            .ifindex;
        isolated
            .set_device_ipv4_addr_by_ifindex(
                auth,
                isolated_ifindex,
                Some(Ipv4Address::new([172, 31, 0, 2])),
                Some(16),
            )
            .expect("set isolated addr");

        let mut out = [0u8; 768];
        let read = match procfs.step_read_projected_with_netns(
            PROCFS_NET_ROUTE_ID,
            0,
            &mut out,
            Some(&isolated),
            &guard,
        ) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("isolated route read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("route text utf8");
        assert!(text.contains("proc-iso0"));
        assert!(!text.contains("proc-initial0"));

        assert_eq!(
            procfs.step_write_projected_with_netns(
                PROCFS_SYS_NET_IPV4_IP_FORWARD_ID,
                0,
                b"1\n",
                Some(&isolated),
                &guard,
            ),
            StepOutcome::Done(2)
        );
        assert!(isolated.ipv4_forwarding_enabled());
        assert!(!initial.ipv4_forwarding_enabled());

        let command = b"masquerade 172.31.0.0/16 proc-iso0\n";
        assert_eq!(
            procfs.step_write_projected_with_netns(
                PROCFS_NET_TX_NF_RULES_ID,
                0,
                command,
                Some(&isolated),
                &guard,
            ),
            StepOutcome::Done(command.len() as u64)
        );
        assert!(tx_subsystems::net::netfilter_rules_snapshot().is_empty());
        assert_eq!(
            tx_subsystems::net::netfilter_rules_snapshot_for_namespace(&isolated).len(),
            1
        );

        let read = match procfs.step_read_projected_with_netns(
            PROCFS_NET_TX_NF_RULES_ID,
            0,
            &mut out,
            Some(&isolated),
            &guard,
        ) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("isolated netfilter read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("netfilter text utf8");
        assert!(text.contains("proc-iso0"));

        let read = match procfs.step_read_projected(PROCFS_NET_TX_NF_RULES_ID, 0, &mut out, &guard)
        {
            StepOutcome::Done(read) => read as usize,
            other => panic!("initial netfilter read failed: {other:?}"),
        };
        let text = core::str::from_utf8(&out[..read]).expect("initial netfilter text utf8");
        assert!(!text.contains("proc-iso0"));
    }
}
