//! Content renderers for procfs pseudo-files.
//!
//! Each function returns a `String` that `step_read` copies into the
//! user buffer.  The renderers read live subsystem state (process,
//! mount, etc.) at read time.

use alloc::string::String;
use tx_subsystems::process::{self, Pid, ProcessIdentity};
use crate::procfs::{
    pid_from_stat_id, PROCFS_MEMINFO_ID, PROCFS_MOUNTS_ID, PROCFS_SELF_ID, PROCFS_ROOT_ID,
};

use tx_subsystems::vfs::FsObjectId;

/// Render the content for `fs_object_id`.
pub fn render(fs_object_id: FsObjectId) -> String {
    if let Some(pid) = pid_from_stat_id(fs_object_id) {
        return render_stat(pid);
    }
    match fs_object_id {
        PROCFS_MOUNTS_ID => render_mounts(),
        PROCFS_MEMINFO_ID => render_meminfo(),
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

/// `/proc/mounts` — stub.
fn render_mounts() -> String {
    "rootfs / rootfs rw 0 0\n".to_string()
}

/// `/proc/meminfo` — stub.
fn render_meminfo() -> String {
    "MemTotal: 0 kB\nMemFree: 0 kB\n".to_string()
}
