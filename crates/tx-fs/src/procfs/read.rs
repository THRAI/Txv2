//! Content renderers for procfs pseudo-files.
//!
//! Each function returns a `String` that `step_read` copies into the
//! user buffer.  The renderers read live subsystem state (process,
//! mount, etc.) at read time.

use alloc::string::String;
use tx_subsystems::process::{self, Pid, ProcessIdentity};
use crate::procfs::{
    pid_from_cmdline_id, pid_from_stat_id, PROCFS_CPUINFO_ID, PROCFS_MOUNTS_ID,
    PROCFS_ROOT_ID, PROCFS_SELF_ID, PROCFS_UPTIME_ID,
};

use tx_subsystems::vfs::FsObjectId;

/// Render the content for `fs_object_id`.
pub fn render(fs_object_id: FsObjectId) -> String {
    if let Some(pid) = pid_from_stat_id(fs_object_id) {
        return render_stat(pid);
    }
    if let Some(pid) = pid_from_cmdline_id(fs_object_id) {
        return render_cmdline(pid);
    }
    match fs_object_id {
        PROCFS_MOUNTS_ID => render_mounts(),
        PROCFS_CPUINFO_ID => render_cpuinfo(),
        PROCFS_UPTIME_ID => render_uptime(),
        _ => String::new(),
    }
}

/// `/proc/<pid>/stat` — basic process status line.
///
/// Format (subset of Linux):
/// `<pid> (<comm>) <state> <ppid> <pgrp> <session>`
fn render_stat(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let ppid = proc.parent_pid();
    let pgrp = proc.pgrp_cap().pgid;
    let session = proc.pgrp_cap().session_cap().sid;

    let comm = match proc.payload.lock().as_ref() {
        Some(payload) => {
            let buf = payload.comm();
            let len = buf.iter().position(|&b| b == 0).unwrap_or(16);
            core::str::from_utf8(&buf[..len]).unwrap_or("?").to_string()
        }
        None => "?".to_string(),
    };

    let state = proc.state_char() as char;

    alloc::format!(
        "{} ({}) {} {} {} {}\n",
        pid.0, comm, state, ppid.0, pgrp.0, session.0
    )
}

/// `/proc/<pid>/cmdline` — NUL-separated argv.
fn render_cmdline(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let payload = proc.payload.lock();
    let Some(payload) = payload.as_ref() else {
        return String::new();
    };
    match payload.cmdline() {
        Some(cmdline) => String::from_utf8_lossy(&cmdline).replace('\0', " "),
        None => String::new(),
    }
}

/// `/proc/mounts` — stub.
fn render_mounts() -> String {
    "rootfs / rootfs rw 0 0\n".to_string()
}

/// `/proc/cpuinfo` — stub (bringup: single cpu).
fn render_cpuinfo() -> String {
    "processor\t: 0\nhart\t\t: 0\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n".to_string()
}

/// `/proc/uptime` — stub.
fn render_uptime() -> String {
    "0.00 0.00\n".to_string()
}

/// `/proc/meminfo` — stub.
fn render_meminfo() -> String {
    "MemTotal: 0 kB\nMemFree: 0 kB\n".to_string()
}
