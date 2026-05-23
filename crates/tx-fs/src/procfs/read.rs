//! Content renderers for procfs pseudo-files.

use crate::procfs::{
    pid_from_cmdline_id, pid_from_fdinfo_id, pid_from_maps_id, pid_from_stat_id, PROCFS_CPUINFO_ID,
    PROCFS_MEMINFO_ID, PROCFS_MOUNTS_ID, PROCFS_SYSVIPC_MSG_ID, PROCFS_SYSVIPC_SEM_ID,
    PROCFS_SYSVIPC_SHM_ID, PROCFS_UPTIME_ID,
};
use alloc::format;
use alloc::string::String;
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::FsObjectId;

pub fn render(fs_object_id: FsObjectId) -> String {
    if let Some(pid) = pid_from_stat_id(fs_object_id) {
        return render_stat(pid);
    }
    if let Some(pid) = pid_from_cmdline_id(fs_object_id) {
        return render_cmdline(pid);
    }
    if let Some(pid) = pid_from_maps_id(fs_object_id) {
        return render_maps(pid);
    }
    if let Some((pid, fd)) = pid_from_fdinfo_id(fs_object_id) {
        return render_fdinfo(pid, fd);
    }
    match fs_object_id {
        PROCFS_MOUNTS_ID => render_mounts(),
        PROCFS_CPUINFO_ID => render_cpuinfo(),
        PROCFS_UPTIME_ID => render_uptime(),
        PROCFS_MEMINFO_ID => render_meminfo(),
        PROCFS_SYSVIPC_MSG_ID => render_sysvipc_msg(),
        PROCFS_SYSVIPC_SEM_ID => render_sysvipc_sem(),
        PROCFS_SYSVIPC_SHM_ID => render_sysvipc_shm(),
        _ => String::new(),
    }
}

fn render_stat(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let ppid = proc.parent_pid();
    let pgrp = proc.pgrp_cap().pgid;
    let session = proc.pgrp_cap().session_cap().sid;

    let buf = proc.comm();
    let comm =
        core::str::from_utf8(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(16)]).unwrap_or("?");

    let state = proc.state_char() as char;

    alloc::format!(
        "{} ({}) {} {} {} {}\n",
        pid.0,
        comm,
        state,
        ppid.0,
        pgrp.0,
        session.0
    )
}

fn render_cmdline(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    match proc.cmdline() {
        Some(cmdline) => String::from_utf8_lossy(&cmdline).replace('\0', " "),
        None => String::new(),
    }
}

fn render_mounts() -> String {
    String::from("rootfs / rootfs rw 0 0\n")
}

fn render_cpuinfo() -> String {
    String::from("processor\t: 0\nhart\t\t: 0\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n")
}

fn render_uptime() -> String {
    String::from("0.00 0.00\n")
}

fn render_maps(pid: Pid) -> String {
    use tx_subsystems::vm::VmBacking;

    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let Some(aspace) = proc.aspace_cap() else {
        return String::new();
    };

    let mut entries = aspace.recipes_snapshot();
    // Sort by start address for canonical output.
    entries.sort_by_key(|e| e.range.start());

    let mut out = String::new();
    for entry in entries {
        let start = entry.range.start().as_usize();
        let end = entry.range.end().as_usize();

        // Permissions: r/w/x/p based on prot and flags.
        let r = if entry.prot.read { 'r' } else { '-' };
        let w = if entry.prot.write { 'w' } else { '-' };
        let x = if entry.prot.execute { 'x' } else { '-' };
        let p = if entry.flags.shared { 's' } else { 'p' };

        // Offset and backing description.
        let (offset, backing_desc) = match &entry.backing {
            VmBacking::None => (0u64, "[none]"),
            VmBacking::PrivateAnon => (0u64, "[anon]"),
            VmBacking::Page { pc, offset: off } => {
                use tx_subsystems::page_backed::PageContainerKind;
                let desc = match pc.kind() {
                    PageContainerKind::Anon { .. } => "[anon]",
                    PageContainerKind::File { .. } => "[file]",
                    PageContainerKind::Device { .. } => "[device]",
                };
                (*off, desc)
            }
        };

        // Locked flag.
        let locked = if entry.flags.locked { " l" } else { "" };

        use alloc::format;
        out.push_str(&format!(
            "{:x}-{:x} {}{}{}{} {:08x} 00:00 0 {}{}\n",
            start, end, r, w, x, p, offset, backing_desc, locked,
        ));
    }

    out
}

fn render_fdinfo(pid: Pid, fd: u32) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let Some(file) = proc.fd(fd) else {
        return String::new();
    };
    let mut out = format!(
        "pos:\t{}\nflags:\t{:o}\n",
        file.offset(),
        fdinfo_flags(&file)
    );
    if let Some(mq) = file.posix_mq() {
        if let Ok(attr) = tx_subsystems::ipc::posix_mq::execution::step_mq_getattr(mq) {
            out.push_str(&format!(
                "mnt_id:\t0\nino:\t{}\nmq_flags:\t{}\nmq_maxmsg:\t{}\nmq_msgsize:\t{}\nmq_curmsgs:\t{}\n",
                mq.msqid(),
                attr.flags,
                attr.maxmsg,
                attr.msgsize,
                attr.curmsgs
            ));
        }
    }
    out
}

fn fdinfo_flags(file: &tx_subsystems::vfs::OpenFile) -> u32 {
    let flags = file.flags();
    let mut out = 0;
    if flags.write && !flags.read {
        out |= 0o1;
    } else if flags.read && flags.write {
        out |= 0o2;
    }
    if flags.append {
        out |= 0o2000;
    }
    if flags.nonblocking {
        out |= 0o4000;
    }
    if flags.cloexec {
        out |= 0o2000000;
    }
    out
}

pub fn render_meminfo() -> String {
    String::from("MemTotal: 0 kB\nMemFree: 0 kB\n")
}

fn render_sysvipc_msg() -> String {
    let mut out = String::from("key msqid perms cbytes qnum qbytes uid gid cuid cgid\n");
    for row in tx_subsystems::ipc::sysv_msg::projection::project_sysvipc_msg() {
        out.push_str(&format!(
            "{} {} {:o} {} {} {} {} {} {} {}\n",
            row.key,
            row.msqid,
            row.mode,
            row.current_bytes,
            row.msg_count,
            row.qbytes,
            row.uid,
            row.gid,
            row.cuid,
            row.cgid
        ));
    }
    out
}

fn render_sysvipc_sem() -> String {
    let mut out = String::from("key semid perms nsems uid gid cuid cgid\n");
    for row in tx_subsystems::ipc::sysv_sem::projection::project_sysvipc_sem() {
        out.push_str(&format!(
            "{} {} {:o} {} {} {} {} {}\n",
            row.key, row.semid, row.mode, row.nsems, row.uid, row.gid, row.cuid, row.cgid
        ));
    }
    out
}

fn render_sysvipc_shm() -> String {
    let mut out = String::from("key shmid perms bytes nattch uid gid cuid cgid\n");
    for row in tx_subsystems::ipc::sysv_shm::projection::project_sysvipc_shm() {
        out.push_str(&format!(
            "{} {} {:o} {} {} {} {} {} {}\n",
            row.key,
            row.shmid,
            row.mode,
            row.size,
            row.attach_count,
            row.uid,
            row.gid,
            row.cuid,
            row.cgid
        ));
    }
    out
}
