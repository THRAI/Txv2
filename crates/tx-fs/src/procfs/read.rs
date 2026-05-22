//! Content renderers for procfs pseudo-files.

use crate::procfs::{
    pid_from_cmdline_id, pid_from_maps_id, pid_from_stat_id, task_from_stat_id, PROCFS_CONFIG_ID,
    PROCFS_CPUINFO_ID, PROCFS_MEMINFO_ID, PROCFS_MOUNTS_ID, PROCFS_SYS_KERNEL_TAINTED_ID,
    PROCFS_UPTIME_ID,
};
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
    match fs_object_id {
        PROCFS_MOUNTS_ID => render_mounts(),
        PROCFS_CPUINFO_ID => render_cpuinfo(),
        PROCFS_UPTIME_ID => render_uptime(),
        PROCFS_MEMINFO_ID => render_meminfo(),
        PROCFS_CONFIG_ID => render_config(),
        PROCFS_SYS_KERNEL_TAINTED_ID => String::from("0\n"),
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
