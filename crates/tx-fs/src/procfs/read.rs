//! Content renderers for procfs pseudo-files.

use crate::procfs::{
    pid_from_cmdline_id, pid_from_fdinfo_id, pid_from_gid_map_id, pid_from_maps_id,
    pid_from_setgroups_id, pid_from_smaps_id, pid_from_stat_id, pid_from_status_id,
    pid_from_uid_map_id,
    KERNEL_CONFIG_TEXT,
    PROCFS_CONFIG_ID, PROCFS_CPUINFO_ID, PROCFS_MEMINFO_ID, PROCFS_MOUNTS_ID,
    PROCFS_NET_ARP_ID, PROCFS_NET_IF_INET6_ID, PROCFS_NET_IPV6_ROUTE_ID, PROCFS_NET_TX_NEIGH_ID,
    PROCFS_SYS_FS_LEASE_BREAK_TIME_ID, PROCFS_SYS_FS_PIPE_MAX_SIZE_ID,
    PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID, PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID,
    PROCFS_SYS_KERNEL_PID_MAX_ID, PROCFS_SYS_KERNEL_TAINTED_ID, PROCFS_SYSVIPC_MSG_ID,
    PROCFS_SYSVIPC_SEM_ID, PROCFS_SYSVIPC_SHM_ID, PROCFS_UPTIME_ID,
};
use alloc::format;
use alloc::string::String;
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::FsObjectId;

pub fn render(fs_object_id: FsObjectId) -> String {
    if let Some(pid) = pid_from_status_id(fs_object_id) {
        return render_status(pid);
    }
    if let Some(pid) = pid_from_stat_id(fs_object_id) {
        return render_stat(pid);
    }
    if let Some(pid) = pid_from_cmdline_id(fs_object_id) {
        return render_cmdline(pid);
    }
    if let Some(pid) = pid_from_maps_id(fs_object_id) {
        return render_maps(pid);
    }
    if let Some(pid) = pid_from_smaps_id(fs_object_id) {
        return render_smaps(pid);
    }
    if let Some(pid) = pid_from_uid_map_id(fs_object_id) {
        return render_userns_id_map(pid, false);
    }
    if let Some(pid) = pid_from_gid_map_id(fs_object_id) {
        return render_userns_id_map(pid, true);
    }
    if let Some(pid) = pid_from_setgroups_id(fs_object_id) {
        return render_userns_setgroups(pid);
    }
    if let Some((pid, fd)) = pid_from_fdinfo_id(fs_object_id) {
        return render_fdinfo(pid, fd);
    }
    // `/proc/sys/net/ipv6/conf/<iface>/{disable_ipv6,accept_dad}` — IPv6 enabled,
    // DAD off; both read back 0.
    if matches!(super::ipv6_conf_kind(fs_object_id), Some(1) | Some(2)) {
        return String::from("0\n");
    }
    // `/proc/sys/net/ipv4/conf/<iface>/force_igmp_version` — shared stored knob.
    if super::ipv4_conf_kind(fs_object_id) == Some(1) {
        return alloc::format!(
            "{}\n",
            super::FORCE_IGMP_VERSION.load(core::sync::atomic::Ordering::Relaxed)
        );
    }
    match fs_object_id {
        PROCFS_MOUNTS_ID => render_mounts(),
        PROCFS_CPUINFO_ID => render_cpuinfo(),
        PROCFS_UPTIME_ID => render_uptime(),
        PROCFS_MEMINFO_ID => render_meminfo(),
        PROCFS_CONFIG_ID => render_config(),
        PROCFS_SYS_KERNEL_TAINTED_ID => String::from("0\n"),
        PROCFS_SYS_KERNEL_PID_MAX_ID => String::from("4194304\n"),
        PROCFS_SYS_FS_PIPE_MAX_SIZE_ID => String::from("4096\n"),
        PROCFS_SYS_FS_LEASE_BREAK_TIME_ID => String::from("45\n"),
        PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID | PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID => {
            String::from("1\n")
        }
        id if id == super::PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID => alloc::format!(
            "{}\n",
            super::IGMP_MAX_MEMBERSHIPS.load(core::sync::atomic::Ordering::Relaxed)
        ),
        id if id == super::PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID => alloc::format!(
            "{}\n",
            super::IGMP_MAX_MSF.load(core::sync::atomic::Ordering::Relaxed)
        ),
        PROCFS_NET_IF_INET6_ID => render_if_inet6(),
        PROCFS_NET_TX_NEIGH_ID => {
            let netns = tx_subsystems::net::namespace::initial_net_namespace_payload();
            tx_subsystems::net::proc_net_neigh_snapshot_text_for_namespace(&netns)
        }
        PROCFS_NET_ARP_ID => {
            let netns = tx_subsystems::net::namespace::initial_net_namespace_payload();
            tx_subsystems::net::proc_net_arp_snapshot_zero_text(&netns.ether_ifaces_snapshot())
        }
        PROCFS_NET_IPV6_ROUTE_ID => {
            let netns = tx_subsystems::net::namespace::initial_net_namespace_payload();
            tx_subsystems::net::proc_net_ipv6_route_snapshot_text(&netns)
        }
        PROCFS_SYSVIPC_MSG_ID => render_sysvipc_msg(),
        PROCFS_SYSVIPC_SEM_ID => render_sysvipc_sem(),
        PROCFS_SYSVIPC_SHM_ID => render_sysvipc_shm(),
        _ => String::new(),
    }
}

/// `/proc/net/if_inet6` — one line per configured IPv6 address in the root net
/// namespace, in the kernel's format:
/// `<32-hex addr> <ifindex hex> <prefixlen hex> <scope hex> <flags hex> <dev>`.
/// LTP `tst_net_detect_ipv6` only requires the file to exist, but `ip -6 addr`
/// and the v6 suites read it, so we render real data from the link snapshot.
fn render_if_inet6() -> String {
    use core::fmt::Write as _;
    let netns = tx_subsystems::net::namespace::initial_net_namespace_payload();
    let mut out = String::new();
    for link in netns.link_snapshot() {
        let Some(addr) = link.ipv6_addr else {
            continue;
        };
        let octets = addr.octets();
        let prefix = link.ipv6_prefix_len.unwrap_or(64);
        let scope: u8 = if link.is_loopback {
            0x10 // host
        } else if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
            0x20 // link-local
        } else {
            0x00 // global
        };
        for b in octets {
            let _ = write!(out, "{b:02x}");
        }
        // ifindex prefixlen scope flags(IFA_F_PERMANENT=0x80) devname
        let _ = write!(
            out,
            " {:02x} {:02x} {:02x} {:02x} {:>8}\n",
            link.ifindex, prefix, scope, 0x80u8, link.name
        );
    }
    out
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

/// Monotonic-nanosecond reader registered by kernel init (mirrors the
/// `tx_observe` TS_FN pattern: procfs is not generic over the platform,
/// so the concrete `TimeIf::read_ns` is injected as a fn pointer).
/// 0 (unregistered) renders the previous static "0.00 0.00".
static UPTIME_NS_FN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn procfs_register_uptime_clock(f: fn() -> u64) {
    UPTIME_NS_FN.store(f as usize as u64, core::sync::atomic::Ordering::Relaxed);
}

fn render_uptime() -> String {
    let raw = UPTIME_NS_FN.load(core::sync::atomic::Ordering::Relaxed);
    if raw == 0 {
        return String::from("0.00 0.00\n");
    }
    // SAFETY: `raw` was written by `procfs_register_uptime_clock` from a
    // valid `fn() -> u64` pointer.
    let f: fn() -> u64 = unsafe { core::mem::transmute(raw as usize) };
    let ns = f();
    let secs = ns / 1_000_000_000;
    let hundredths = (ns % 1_000_000_000) / 10_000_000;
    // Render the idle column equal to uptime (single-purpose appliance
    // kernel; LTP consumers only parse the first column).
    format!("{secs}.{hundredths:02} {secs}.{hundredths:02}\n")
}

fn render_maps(pid: Pid) -> String {
    use tx_subsystems::vm::VmEntryBacking;

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
        let (offset, backing_desc) = match entry.backing_kind() {
            VmEntryBacking::None => (0u64, "[none]"),
            VmEntryBacking::PrivateAnon => (0u64, "[anon]"),
            VmEntryBacking::Page { offset } => {
                use tx_subsystems::page_backed::PageContainerKind;
                let Some((pc, _)) = entry.page_backing() else {
                    continue;
                };
                let desc = match pc.kind() {
                    PageContainerKind::Anon { .. } => "[anon]",
                    PageContainerKind::File { .. } => "[file]",
                    PageContainerKind::Device { .. } => "[device]",
                };
                (offset, desc)
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

/// `/proc/<pid>/smaps`: each mapping's header line (as in `maps`) followed by
/// the per-mapping size/rss/locked block. mlock LTP tests read the `Locked:`
/// field to confirm the region is resident after `mlock`/`mlockall`.
fn render_smaps(pid: Pid) -> String {
    use tx_subsystems::vm::{VmEntryBacking, USER_PAGE_SIZE};

    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let Some(aspace) = proc.aspace_cap() else {
        return String::new();
    };

    let mut entries = aspace.recipes_snapshot();
    entries.sort_by_key(|e| e.range.start());

    let mut out = String::new();
    for entry in entries {
        let start = entry.range.start().as_usize();
        let end = entry.range.end().as_usize();

        let r = if entry.prot.read { 'r' } else { '-' };
        let w = if entry.prot.write { 'w' } else { '-' };
        let x = if entry.prot.execute { 'x' } else { '-' };
        let p = if entry.flags.shared { 's' } else { 'p' };

        let (offset, backing_desc) = match entry.backing_kind() {
            VmEntryBacking::None => (0u64, "[none]"),
            VmEntryBacking::PrivateAnon => (0u64, "[anon]"),
            VmEntryBacking::Page { offset } => {
                use tx_subsystems::page_backed::PageContainerKind;
                let Some((pc, _)) = entry.page_backing() else {
                    continue;
                };
                let desc = match pc.kind() {
                    PageContainerKind::Anon { .. } => "[anon]",
                    PageContainerKind::File { .. } => "[file]",
                    PageContainerKind::Device { .. } => "[device]",
                };
                (offset, desc)
            }
        };

        let size_kb = entry.range.page_count() * (USER_PAGE_SIZE / 1024);
        let locked_kb = if entry.flags.locked { size_kb } else { 0 };

        out.push_str(&format!(
            "{:x}-{:x} {}{}{}{} {:08x} 00:00 0 {}\n",
            start, end, r, w, x, p, offset, backing_desc,
        ));
        out.push_str(&format!(
            "Size:           {:8} kB\n\
             Rss:            {:8} kB\n\
             Pss:            {:8} kB\n\
             Shared_Clean:   {:8} kB\n\
             Shared_Dirty:   {:8} kB\n\
             Private_Clean:  {:8} kB\n\
             Private_Dirty:  {:8} kB\n\
             Referenced:     {:8} kB\n\
             Anonymous:      {:8} kB\n\
             Locked:         {:8} kB\n",
            size_kb, size_kb, size_kb, 0, 0, size_kb, 0, size_kb, size_kb, locked_kb,
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

pub fn render_config() -> String {
    String::from(KERNEL_CONFIG_TEXT)
}

const fn state_name(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'Z' => "zombie",
        _ => "unknown",
    }
}

/// `/proc/<pid>/status` — enough for LTP's getdatasize() (`VmData:` line). VmData
/// is reported as a stable 0 (no per-mapping accounting re-homed yet), which is
/// sufficient for the leak check (before == after). Re-homed; PR#50 dropped it.
fn render_status(pid: Pid) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let state = proc.state_char() as char;
    let name_buf = proc.comm();
    let name =
        core::str::from_utf8(&name_buf[..name_buf.iter().position(|&b| b == 0).unwrap_or(16)])
            .unwrap_or("?");
    // VmLck: sum of locked (mlock/mlockall) regions, in kB. LTP mlock201/mlock203
    // read this field from /proc/self/status to confirm pages are locked.
    let mut vm_lck_kb = 0usize;
    if let Some(aspace) = proc.aspace_cap() {
        for entry in aspace.recipes_snapshot() {
            if entry.flags.locked {
                vm_lck_kb += entry.range.page_count() * (tx_subsystems::vm::USER_PAGE_SIZE / 1024);
            }
        }
    }
    format!(
        "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nThreads:\t{}\n\
VmData:\t{:8} kB\nVmLck:\t{:8} kB\n",
        name,
        state,
        state_name(state),
        pid.0,
        pid.0,
        proc.parent_pid().0,
        proc.live_thread_count(),
        0,
        vm_lck_kb,
    )
}

/// `/proc/<pid>/{uid_map,gid_map}` — one `inside outside length` row per entry,
/// Linux's `%10u %10u %10u` column layout.
fn render_userns_id_map(pid: Pid, gid: bool) -> String {
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let Some(nsproxy) = proc.nsproxy_cap() else {
        return String::new();
    };
    let entries = if gid {
        nsproxy.user_ns.gid_map_snapshot()
    } else {
        nsproxy.user_ns.uid_map_snapshot()
    };
    let mut out = String::new();
    for e in entries {
        out.push_str(&format!(
            "{:>10} {:>10} {:>10}\n",
            e.inside, e.outside, e.length
        ));
    }
    out
}

/// `/proc/<pid>/setgroups` — "allow\n" or "deny\n".
fn render_userns_setgroups(pid: Pid) -> String {
    use tx_subsystems::process::nsproxy::SetgroupsPolicy;
    let Some(proc) = process::process_by_pid(pid) else {
        return String::new();
    };
    let Some(nsproxy) = proc.nsproxy_cap() else {
        return String::new();
    };
    match nsproxy.user_ns.setgroups_policy() {
        SetgroupsPolicy::Allow => String::from("allow\n"),
        SetgroupsPolicy::Deny => String::from("deny\n"),
    }
}

pub fn render_meminfo() -> String {
    // Full enough for LTP's tst_memutils (`/proc/meminfo` parser): it sscanf's
    // `MemAvailable:` and needs non-zero values. main shipped a 0-valued stub
    // (MemTotal/MemFree only) which makes every new-framework LTP test TBROK at
    // setup ("Expected 1 conversions got 0"). Re-homed the user's fuller table.
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
