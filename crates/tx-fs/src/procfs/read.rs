//! Content renderers for procfs pseudo-files.

use crate::procfs::{
    pid_from_cmdline_id, pid_from_fdinfo_id, pid_from_maps_id, pid_from_stat_id, task_from_stat_id,
    PROCFS_CONFIG_ID, PROCFS_CPUINFO_ID, PROCFS_MEMINFO_ID, PROCFS_MOUNTS_ID, PROCFS_NET_ARP_ID,
    PROCFS_NET_DEV_ID, PROCFS_NET_NF_CONNTRACK_ID, PROCFS_NET_ROUTE_ID, PROCFS_NET_TX_NF_RULES_ID,
    PROCFS_SYSVIPC_MSG_ID, PROCFS_SYSVIPC_SEM_ID, PROCFS_SYSVIPC_SHM_ID,
    PROCFS_SYS_FS_LEASE_BREAK_TIME_ID, PROCFS_SYS_FS_PIPE_MAX_SIZE_ID,
    PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID, PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID,
    PROCFS_SYS_KERNEL_TAINTED_ID, PROCFS_SYS_NET_IPV4_IP_FORWARD_ID, PROCFS_UPTIME_ID,
};
use alloc::format;
use alloc::string::String;
use tx_subsystems::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::FsObjectId;

pub fn render(fs_object_id: FsObjectId) -> String {
    if let Some(pid) = pid_from_stat_id(fs_object_id) {
        return render_stat(pid);
    }
    if let Some((pid, tid)) = task_from_stat_id(fs_object_id) {
        return render_thread_stat(pid, tid);
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
        PROCFS_CONFIG_ID => render_config(),
        PROCFS_SYS_KERNEL_TAINTED_ID => String::from("0\n"),
        PROCFS_SYS_FS_PIPE_MAX_SIZE_ID => String::from("4096\n"),
        PROCFS_SYS_FS_LEASE_BREAK_TIME_ID => String::from("45\n"),
        PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID | PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID => {
            String::from("1\n")
        }
        PROCFS_SYSVIPC_MSG_ID => render_sysvipc_msg(),
        PROCFS_SYSVIPC_SEM_ID => render_sysvipc_sem(),
        PROCFS_SYSVIPC_SHM_ID => render_sysvipc_shm(),
        PROCFS_NET_ROUTE_ID => render_net_route(),
        PROCFS_NET_ARP_ID => render_net_arp(),
        PROCFS_NET_DEV_ID => render_net_dev(),
        PROCFS_NET_TX_NF_RULES_ID => render_netfilter_rules(),
        PROCFS_NET_NF_CONNTRACK_ID => render_nf_conntrack(),
        PROCFS_SYS_NET_IPV4_IP_FORWARD_ID => render_ip_forward(),
        _ => String::new(),
    }
}

fn render_stat(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return match resolve_pid_number_as(pid.0 as u64, PidNameKind::Thread) {
            Some(PidName::Thread(thread)) => match thread.upgrade_owner_proc() {
                Some(owner) => render_thread_stat(owner.pid, pid.0),
                None => render_stat_line(pid.0, "?", 'Z', 0, 0, 0),
            },
            _ => render_stat_line(pid.0, "?", 'Z', 0, 0, 0),
        };
    };
    let ppid = proc.parent_pid();
    let pgrp = proc.pgrp_cap().pgid;
    let session = proc.pgrp_cap().session_cap().sid;

    let buf = proc.comm();
    let comm =
        core::str::from_utf8(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(16)]).unwrap_or("?");

    let state = proc.state_char() as char;

    render_stat_line(pid.0, comm, state, ppid.0, pgrp.0, session.0)
}

fn render_thread_stat(pid: Pid, tid: u32) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return render_stat_line(tid, "?", 'Z', 0, 0, 0);
    };
    let Some(thread) = proc.thread_by_tid(tid) else {
        return render_stat_line(tid, "?", 'Z', 0, 0, 0);
    };
    let ppid = proc.parent_pid();
    let pgrp = proc.pgrp_cap().pgid;
    let session = proc.pgrp_cap().session_cap().sid;

    let buf = proc.comm();
    let comm =
        core::str::from_utf8(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(16)]).unwrap_or("?");

    render_stat_line(
        tid,
        comm,
        thread.proc_state_char() as char,
        ppid.0,
        pgrp.0,
        session.0,
    )
}

fn render_stat_line(
    pid: u32,
    comm: &str,
    state: char,
    ppid: u32,
    pgrp: u32,
    session: u32,
) -> String {
    alloc::format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid, comm, state, ppid, pgrp, session
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
    String::from(
        "MemTotal:        1048576 kB\n\
         MemFree:          524288 kB\n\
         MemAvailable:     524288 kB\n\
         Buffers:               0 kB\n\
         Cached:                0 kB\n\
         SwapCached:            0 kB\n\
         Active:                0 kB\n\
         Inactive:              0 kB\n\
         SwapTotal:             0 kB\n\
         SwapFree:              0 kB\n",
    )
}

fn render_config() -> String {
    String::from(
        "CONFIG_EVENTFD=y\n\
         CONFIG_TIME_NS=y\n\
         CONFIG_HIGH_RES_TIMERS=y\n",
    )
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

fn render_net_route() -> String {
    tx_subsystems::net::proc_net_route_snapshot_text(
        &tx_subsystems::net::initial_net_namespace_payload(),
    )
}

fn render_net_arp() -> String {
    tx_subsystems::net::proc_net_arp_snapshot_zero_text(
        &tx_subsystems::net::initial_net_namespace_payload().ether_ifaces_snapshot(),
    )
}

fn render_net_dev() -> String {
    tx_subsystems::net::proc_net_dev_snapshot_text(
        &tx_subsystems::net::initial_net_namespace_payload().ether_ifaces_snapshot(),
    )
}

fn render_netfilter_rules() -> String {
    tx_subsystems::net::proc_net_netfilter_rules_text()
}

fn render_nf_conntrack() -> String {
    tx_subsystems::net::proc_net_nf_conntrack_text()
}

fn render_ip_forward() -> String {
    let enabled = tx_subsystems::net::initial_net_namespace_payload().ipv4_forwarding_enabled();
    if enabled {
        String::from("1\n")
    } else {
        String::from("0\n")
    }
}
