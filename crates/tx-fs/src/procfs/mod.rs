//! procfs — minimal projected filesystem for `/proc`.
//! Bringup scope: `/proc`, `/proc/<pid>`, `/proc/self`, `/proc/mounts`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use tx_hal::UserPtr;

pub mod adapter;
mod read;

pub use read::procfs_register_uptime_clock;

use adapter::step_engine::{Cap, NoProgress, StepOutcome};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::process::{self, Pid};
use tx_subsystems::vfs::{
    render_dentry_path, Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta,
    ProjectionKey, ProjectionSchemaId, RNode, RNodeBacking, S_IFDIR, S_IFLNK, S_IFREG,
};
use tx_subsystems::vfs::structure::StructPayload;

// Fixed procfs node IDs live in the 0x7071_6Fxx block, deliberately BELOW
// `PROCFS_PID_BASE` (0x7072_0000). They must never fall inside the per-pid dir
// window [PROCFS_PID_BASE, PROCFS_PID_BASE + PROCFS_PID_LIMIT): a shared id
// makes `/proc/<pid>` alias a fixed file node, so `/proc/<pid>/stat` returns
// ENOTDIR. That hung LTP fs_bind once pids climbed into the old 0x7072_6Fxx
// block (pid 28416+). Keep fixed nodes in 0x7071_6Fxx; never put one at 0x7072_.
pub const PROCFS_ROOT_ID: FsObjectId = FsObjectId::new(0x7071_6F00);
pub const PROCFS_SELF_ID: FsObjectId = FsObjectId::new(0x7071_6F01);
pub const PROCFS_MOUNTS_ID: FsObjectId = FsObjectId::new(0x7071_6F02);
pub const PROCFS_CPUINFO_ID: FsObjectId = FsObjectId::new(0x7071_6F03);
pub const PROCFS_UPTIME_ID: FsObjectId = FsObjectId::new(0x7071_6F04);
pub const PROCFS_MEMINFO_ID: FsObjectId = FsObjectId::new(0x7071_6F05);
pub const PROCFS_SYSVIPC_ID: FsObjectId = FsObjectId::new(0x7071_6F06);
pub const PROCFS_SYSVIPC_MSG_ID: FsObjectId = FsObjectId::new(0x7071_6F07);
pub const PROCFS_SYSVIPC_SEM_ID: FsObjectId = FsObjectId::new(0x7071_6F08);
pub const PROCFS_SYSVIPC_SHM_ID: FsObjectId = FsObjectId::new(0x7071_6F09);
pub const PROCFS_CONFIG_ID: FsObjectId = FsObjectId::new(0x7071_6F0A);

/// Minimal plain-text kernel `.config` exposed at `/proc/config` (and seeded as
/// `/boot/config-6.1.0-txkernel`) so LTP's `tst_kconfig` parser can confirm the
/// features the network suites probe. Re-homed with the net subsystem; the
/// PR#50 re-home dropped the whole config surface (this const, the
/// `/proc/config` backing, and the `/boot/config-*` seeding), which made every
/// kconfig-gated LTP case TBROK with "Cannot parse kernel .config" regardless
/// of runner path. Conservative on purpose — it advertises only what the LTP
/// witnesses check, not the kernel's real build options.
pub const KERNEL_CONFIG_TEXT: &str = "CONFIG_EVENTFD=y\n\
CONFIG_TIME_NS=y\n\
CONFIG_HIGH_RES_TIMERS=y\n\
CONFIG_NET_NS=y\n\
CONFIG_USER_NS=y\n\
CONFIG_DUMMY=y\n\
CONFIG_VETH=y\n\
CONFIG_NET_SCH_TEQL=y\n\
CONFIG_NETFILTER_XTABLES=y\n\
CONFIG_NETFILTER_XT_MATCH_STATE=y\n\
CONFIG_NETFILTER_XT_MATCH_LIMIT=y\n\
CONFIG_NETFILTER_XT_MATCH_MULTIPORT=y\n\
CONFIG_NETFILTER_XT_TARGET_LOG=y\n\
CONFIG_IP_NF_TARGET_REJECT=y\n\
CONFIG_IP_NF_IPTABLES=y\n\
CONFIG_IP_NF_FILTER=y\n\
CONFIG_IP6_NF_IPTABLES=y\n\
CONFIG_IP6_NF_FILTER=y\n\
CONFIG_NF_TABLES=y\n\
CONFIG_TLS=y\n";

// `/proc/sys/kernel/` subtree (re-homed; PR#50 dropped all of /proc/sys). LTP's
// `tst_taint` opens `/proc/sys/kernel/tainted` in setup for many cases, and a
// missing file makes them TBROK before the test body. `/proc/sys/net/*` and
// `/proc/sys/fs/*` from the pre-rebase tree remain to be re-homed if needed.
pub const PROCFS_SYS_ID: FsObjectId = FsObjectId::new(0x7071_6F0B);
pub const PROCFS_SYS_KERNEL_ID: FsObjectId = FsObjectId::new(0x7071_6F0C);
pub const PROCFS_SYS_KERNEL_TAINTED_ID: FsObjectId = FsObjectId::new(0x7071_6F0D);
pub const PROCFS_SYS_KERNEL_PID_MAX_ID: FsObjectId = FsObjectId::new(0x7071_6F0E);
// `/proc/sys/fs/*` sysctls (re-homed from main; the net re-home took feature's
// procfs which lacked these). LTP `fcntl30`/`splice04` read `pipe-max-size`,
// fcntl-lease tests read `lease-break-time`; missing files make them TBROK.
pub const PROCFS_SYS_FS_ID: FsObjectId = FsObjectId::new(0x7071_6F40);
pub const PROCFS_SYS_FS_PIPE_MAX_SIZE_ID: FsObjectId = FsObjectId::new(0x7071_6F41);
pub const PROCFS_SYS_FS_LEASE_BREAK_TIME_ID: FsObjectId = FsObjectId::new(0x7071_6F42);
pub const PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID: FsObjectId = FsObjectId::new(0x7071_6F43);
pub const PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID: FsObjectId = FsObjectId::new(0x7071_6F44);

// `/proc/net/` + `/proc/sys/net/ipv6/` subtree (re-homed; the PR#50 net re-home
// dropped all of `/proc/sys/net` and `/proc/net/if_inet6`). LTP's
// `tst_net_detect_ipv6` requires `[ -f /proc/net/if_inet6 ]` AND
// `cat /proc/sys/net/ipv6/conf/all/disable_ipv6` == 0 (plus the per-iface
// `disable_ipv6` and a writable `accept_dad` that `sysctl -qw` touches in
// setup); without them every net.ipv6 case is skipped TCONF "IPv6 disabled".
// Each `conf/<name>` entry gets a DISTINCT inode id (hashed from the name): two
// directory names must never share one inode or the VFS dcache aliases them
// (the bug that made `sysctl -w net.ipv6.conf.eth0.accept_dad=0` return EISDIR).
// Content is name-independent (disable_ipv6=0, accept_dad writable no-op).
pub const PROCFS_NET_ID: FsObjectId = FsObjectId::new(0x7071_6F0F);
pub const PROCFS_NET_IF_INET6_ID: FsObjectId = FsObjectId::new(0x7071_6F10);
pub const PROCFS_SYS_NET_ID: FsObjectId = FsObjectId::new(0x7071_6F11);
pub const PROCFS_SYS_NET_IPV6_ID: FsObjectId = FsObjectId::new(0x7071_6F12);
pub const PROCFS_SYS_NET_IPV6_CONF_ID: FsObjectId = FsObjectId::new(0x7071_6F13);
// `/proc/net/tx_neigh` — kernel ARP+NDISC neighbor table, one line per entry, read
// by the `ip neigh show` shim. LTP ipneigh01 pings a peer (auto-creating the entry
// via learn_configured_icmpv*_neighbor) then expects `ip neigh show` to list it.
// `/proc/net/tx_neigh_ctl` — write "<addr> <dev>" to delete an entry (the `ip
// neigh del` shim path).
pub const PROCFS_NET_TX_NEIGH_ID: FsObjectId = FsObjectId::new(0x7071_6F14);
pub const PROCFS_NET_TX_NEIGH_CTL_ID: FsObjectId = FsObjectId::new(0x7071_6F15);
// `/proc/net/arp` — kernel IPv4 ARP table in the classic format, read by busybox
// `arp -an` (the LTP ipneigh01 `arp` variant).
pub const PROCFS_NET_ARP_ID: FsObjectId = FsObjectId::new(0x7071_6F16);

// `/proc/sys/net/ipv4/` — the IGMP knobs LTP's mcast-lib.sh saves, sets and
// restores in setup/cleanup (`sysctl -b` reads, `sysctl -qw` writes via ROD:
// a missing node TBROKs every net_stress.multicast test before its body).
// Values are accepted and stored; the IGMP emulation currently behaves as
// IGMPv2-compatible regardless.
pub const PROCFS_SYS_NET_IPV4_ID: FsObjectId = FsObjectId::new(0x7071_6F17);
pub const PROCFS_SYS_NET_IPV4_CONF_ID: FsObjectId = FsObjectId::new(0x7071_6F18);
pub const PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID: FsObjectId =
    FsObjectId::new(0x7071_6F19);
pub const PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID: FsObjectId = FsObjectId::new(0x7071_6F1A);

static IGMP_MAX_MEMBERSHIPS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(20);
static IGMP_MAX_MSF: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(10);
// One shared knob for `all` and every per-iface dir (mcast-lib only ever
// writes 0 and restores the saved value).
static FORCE_IGMP_VERSION: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

// `conf/<name>/` entries live in their own high id region: dir = base + tag*4,
// disable_ipv6 = dir+1, accept_dad = dir+2, where tag = FNV-1a(name) (30-bit).
const PROCFS_IPV6_CONF_BASE: u64 = PROCFS_PID_BASE + 0x80_0000_0000;
const PROCFS_IPV6_CONF_SPAN: u64 = 0x1_0000_0000;
fn ipv6_conf_tag(name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h & 0x3FFF_FFFF
}
fn ipv6_conf_dir_id(name: &[u8]) -> FsObjectId {
    FsObjectId::new(PROCFS_IPV6_CONF_BASE + ipv6_conf_tag(name) * 4)
}
/// 0 = directory, 1 = disable_ipv6, 2 = accept_dad, or `None` if not a conf id.
fn ipv6_conf_kind(id: FsObjectId) -> Option<u8> {
    let r = id.as_u64();
    if r >= PROCFS_IPV6_CONF_BASE && r < PROCFS_IPV6_CONF_BASE + PROCFS_IPV6_CONF_SPAN * 4 {
        Some((r & 3) as u8)
    } else {
        None
    }
}
// `/proc/sys/net/ipv4/conf/<name>/` mirrors the ipv6 conf id scheme in its
// own region: dir = base + tag*4, force_igmp_version = dir+1.
const PROCFS_IPV4_CONF_BASE: u64 = PROCFS_PID_BASE + 0x100_0000_0000;
fn ipv4_conf_dir_id(name: &[u8]) -> FsObjectId {
    FsObjectId::new(PROCFS_IPV4_CONF_BASE + ipv6_conf_tag(name) * 4)
}
/// 0 = directory, 1 = force_igmp_version, or `None` if not an ipv4 conf id.
fn ipv4_conf_kind(id: FsObjectId) -> Option<u8> {
    let r = id.as_u64();
    if r >= PROCFS_IPV4_CONF_BASE && r < PROCFS_IPV4_CONF_BASE + PROCFS_IPV6_CONF_SPAN * 4 {
        Some((r & 3) as u8)
    } else {
        None
    }
}
const PROCFS_PID_BASE: u64 = 0x7072_0000;
// Ceiling for pid-keyed procfs ids (`/proc/<pid>` dirs and the fd/fdinfo
// stripes). 4M matches Linux's maximum pid_max. This MUST comfortably exceed
// any real pid: the kernel's pid/tid allocator is one shared monotone counter
// that never recycles, so a full OSComp suite burns ~100k+ ids in one boot.
// The old ceiling was 0x10000 — at pid 65536 `/proc/<pid>` stopped resolving
// (ENOENT), and LTP's shell watchdog spun forever polling `/proc/<pid>/stat`,
// wedging the whole ltp-glibc lane at fs_bind_move01 (2026-07-02 regression).
const PROCFS_PID_LIMIT: u64 = 0x40_0000;
// `/proc/<pid>` directory ids occupy [PROCFS_PID_BASE, PROCFS_PID_BASE + PROCFS_PID_LIMIT).
// Per-pid scalar pseudo-files (stat/cmdline/mem/maps/exe). Each file type owns a
// distinct region wide enough for any u32 pid (PROCFS_PID_FILE_SPAN), and the
// regions are spaced far apart so `id = base + pid` never aliases across types or
// pids.
//
// The earlier scheme packed these at `PROCFS_PID_BASE + pid + {0x10000, 0x10002,
// 0x10003, 0x10004}` with decoders that matched a 0x10000-wide window. Because the
// per-type offsets were only 1–4 apart while pid is added directly, the windows
// overlapped completely: `pid_stat_id(N) == pid_mem_id(N-2)`, etc. Reading
// `/proc/<pid>/stat` for any pid >= 2 therefore decoded as `/proc/<pid-2>/mem`
// (returning ESRCH when that pid was gone) — which wedged LTP's `_tst_setup_timer`
// poll on `/proc/<watchdog>/stat`. `/proc/1/stat` happened to work only because
// `pid_stat_id(1)` fell just below the mem window.
const PROCFS_PID_FILE_SPAN: u64 = 0x1_0000_0000;
const PROCFS_STAT_BASE: u64 = PROCFS_PID_BASE + 0x30_0000_0000;
const PROCFS_CMDLINE_BASE: u64 = PROCFS_PID_BASE + 0x40_0000_0000;
const PROCFS_MEM_BASE: u64 = PROCFS_PID_BASE + 0x50_0000_0000;
const PROCFS_MAPS_BASE: u64 = PROCFS_PID_BASE + 0x60_0000_0000;
const PROCFS_EXE_BASE: u64 = PROCFS_PID_BASE + 0x70_0000_0000;
// `/proc/<pid>/smaps` (re-homed from main; LTP mlock05/mlock201/mlock203 read
// it to confirm pages are `lck` after `mlock`). Distinct 4 GiB-per-pid slot.
const PROCFS_SMAPS_BASE: u64 = PROCFS_PID_BASE + 0x90_0000_0000;
// `/proc/<pid>/fd` and `/proc/<pid>/fdinfo` stripes: each pid owns a 0x10000
// slot (dir at +0, entries at +1+fd), so a block must span
// PROCFS_PID_LIMIT * 0x10000 = 0x400_0000_0000. The old scheme parked these at
// PID_BASE+0x20000/+0x30000 — which both capped pids at 65536 (the interleaved
// stripes relied on it) and sat INSIDE any widened pid-dir window. Re-homed to
// spacious blocks above every other region.
const PROCFS_FD_BASE: u64 = PROCFS_PID_BASE + 0x200_0000_0000;
const PROCFS_FDINFO_BASE: u64 = PROCFS_PID_BASE + 0x600_0000_0000;
const PROCFS_FD_STRIPE: u64 = 0x10000;
const PROCFS_FD_BLOCK_SPAN: u64 = PROCFS_PID_LIMIT * PROCFS_FD_STRIPE;
// `/proc/<pid>/{uid_map,gid_map,setgroups}` (user-namespace map writes used by
// LTP netns setup). Sits in a dedicated id region so these ids never alias the
// stat/fd inode ids. Three files per pid:
// pid*4 + {0=uid_map, 1=gid_map, 2=setgroups}.
const PROCFS_USERNS_BASE: u64 = PROCFS_PID_BASE + 0x2_0000_0000;
pub const fn pid_uid_map_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_USERNS_BASE + (pid.0 as u64) * 4)
}
pub const fn pid_gid_map_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_USERNS_BASE + (pid.0 as u64) * 4 + 1)
}
pub const fn pid_setgroups_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_USERNS_BASE + (pid.0 as u64) * 4 + 2)
}
pub fn pid_from_uid_map_id(id: FsObjectId) -> Option<Pid> {
    pid_from_userns_id(id, 0)
}
pub fn pid_from_gid_map_id(id: FsObjectId) -> Option<Pid> {
    pid_from_userns_id(id, 1)
}
pub fn pid_from_setgroups_id(id: FsObjectId) -> Option<Pid> {
    pid_from_userns_id(id, 2)
}
fn pid_from_userns_id(id: FsObjectId, which: u64) -> Option<Pid> {
    let r = id.as_u64();
    if r < PROCFS_USERNS_BASE {
        return None;
    }
    let offset = r - PROCFS_USERNS_BASE;
    if offset % 4 != which {
        return None;
    }
    let pid = offset / 4;
    if pid <= u32::MAX as u64 {
        Some(Pid(pid as u32))
    } else {
        None
    }
}

// `/proc/<pid>/status` in its own collision-free id region (above the userns
// region), pid in the low bits.
const PROCFS_STATUS_BASE: u64 = PROCFS_PID_BASE + 0x10_0000_0000;
const fn pid_status_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_STATUS_BASE + pid.0 as u64)
}
pub fn pid_from_status_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_STATUS_BASE && r - PROCFS_STATUS_BASE <= u32::MAX as u64 {
        Some(Pid((r - PROCFS_STATUS_BASE) as u32))
    } else {
        None
    }
}

// `/proc/<pid>/ns/` directory + the per-namespace nsfs nodes in their own
// collision-free id region (the highest procfs region). Layout per pid:
//   +0 = ns dir, +1 = ns/net. (ns/mnt etc. can take +2.. when added.)
// Opened by LTP's `tst_ns_exec` (`open(/proc/<pid>/ns/net)` then `setns`).
const PROCFS_NS_BASE: u64 = PROCFS_PID_BASE + 0x20_0000_0000;
const PROCFS_NS_STRIDE: u64 = 4;
const fn pid_ns_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_NS_BASE + pid.0 as u64 * PROCFS_NS_STRIDE)
}
const fn pid_netns_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_NS_BASE + pid.0 as u64 * PROCFS_NS_STRIDE + 1)
}
const fn pid_mntns_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_NS_BASE + pid.0 as u64 * PROCFS_NS_STRIDE + 2)
}
fn pid_from_ns_object_id(id: FsObjectId, tag: u64) -> Option<Pid> {
    let r = id.as_u64();
    if r < PROCFS_NS_BASE {
        return None;
    }
    let offset = r - PROCFS_NS_BASE;
    if offset % PROCFS_NS_STRIDE == tag && offset / PROCFS_NS_STRIDE <= u32::MAX as u64 {
        Some(Pid((offset / PROCFS_NS_STRIDE) as u32))
    } else {
        None
    }
}
fn pid_from_ns_dir(id: FsObjectId) -> Option<Pid> {
    pid_from_ns_object_id(id, 0)
}
fn pid_from_netns_id(id: FsObjectId) -> Option<Pid> {
    pid_from_ns_object_id(id, 1)
}
fn pid_from_mntns_id(id: FsObjectId) -> Option<Pid> {
    pid_from_ns_object_id(id, 2)
}

#[derive(Clone, Copy)]
enum UsernsWriteTarget {
    UidMap,
    GidMap,
    Setgroups,
}

/// Best-effort `/proc/self` target pid. The procfs `read_link`/FsOps seam has no
/// caller-pid context, so this heuristic returns the highest live non-init pid —
/// i.e. the most recently created process, which is the caller for the LTP
/// netns-setup pattern (`unshare` then write `/proc/self/{setgroups,uid,gid}_map`
/// before any fork). Re-homed from the pre-rebase tree; PR#50 dropped it and
/// `read_link` was left hardcoding pid 1, which routed every `/proc/self/*` write
/// to the init user namespace. A true current-pid accessor would be cleaner.
fn procfs_self_target_pid() -> Pid {
    process::all_pids()
        .into_iter()
        .filter(|(pid, alive)| *alive && pid.0 != 1)
        .map(|(pid, _)| pid)
        .max_by_key(|pid| pid.0)
        .unwrap_or(Pid(1))
}

/// Handle a write to `/proc/<pid>/{uid_map,gid_map,setgroups}` by routing to the
/// target process's user namespace. Returns `None` if `fs_object_id` is not one
/// of these files (so the caller falls through). Self-write only matters for the
/// LTP netns setup, so the target process supplies both the target user ns and
/// the writer cred/user ns.
fn write_userns_projection(
    fs_object_id: FsObjectId,
    offset: u64,
    bytes: &[u8],
) -> Option<StepOutcome<u64, NoProgress>> {
    let (pid, target) = if let Some(pid) = pid_from_uid_map_id(fs_object_id) {
        (pid, UsernsWriteTarget::UidMap)
    } else if let Some(pid) = pid_from_gid_map_id(fs_object_id) {
        (pid, UsernsWriteTarget::GidMap)
    } else {
        (pid_from_setgroups_id(fs_object_id)?, UsernsWriteTarget::Setgroups)
    };

    let Some(proc) = process::process_by_pid(pid) else {
        return Some(StepOutcome::err(Errno::ESRCH.into()));
    };
    let Some(nsproxy) = proc.nsproxy_cap() else {
        return Some(StepOutcome::err(Errno::ESRCH.into()));
    };
    let Some(writer_cred) = proc.cred() else {
        return Some(StepOutcome::err(Errno::ESRCH.into()));
    };
    let user_ns = nsproxy.user_ns.clone();

    let result = match target {
        UsernsWriteTarget::Setgroups => {
            tx_subsystems::process::nsproxy::write_user_namespace_setgroups(&user_ns, offset, bytes)
        }
        UsernsWriteTarget::UidMap => tx_subsystems::process::nsproxy::write_user_namespace_id_map(
            &user_ns,
            writer_cred,
            &user_ns,
            tx_subsystems::process::nsproxy::UserNsMapKind::Uid,
            offset,
            bytes,
        ),
        UsernsWriteTarget::GidMap => tx_subsystems::process::nsproxy::write_user_namespace_id_map(
            &user_ns,
            writer_cred,
            &user_ns,
            tx_subsystems::process::nsproxy::UserNsMapKind::Gid,
            offset,
            bytes,
        ),
    };

    Some(match result {
        Ok(()) => StepOutcome::done(bytes.len() as u64),
        Err(errno) => StepOutcome::err(errno.into()),
    })
}
const fn pid_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_PID_BASE + pid.0 as u64)
}
const fn pid_stat_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_STAT_BASE + pid.0 as u64)
}
const fn pid_cmdline_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_CMDLINE_BASE + pid.0 as u64)
}
const fn pid_mem_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_MEM_BASE + pid.0 as u64)
}
const fn pid_maps_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_MAPS_BASE + pid.0 as u64)
}
const fn pid_smaps_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_SMAPS_BASE + pid.0 as u64)
}
const fn pid_exe_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_EXE_BASE + pid.0 as u64)
}
const fn pid_fd_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_FD_BASE + (pid.0 as u64 * PROCFS_FD_STRIPE))
}
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
const fn pid_fd_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_FD_BASE + (pid.0 as u64 * PROCFS_FD_STRIPE) + 1 + fd as u64)
}
const fn pid_fdinfo_dir_id(pid: Pid) -> FsObjectId {
    FsObjectId::new(PROCFS_FDINFO_BASE + (pid.0 as u64 * PROCFS_FD_STRIPE))
}
const fn pid_fdinfo_id(pid: Pid, fd: u32) -> FsObjectId {
    FsObjectId::new(PROCFS_FDINFO_BASE + (pid.0 as u64 * PROCFS_FD_STRIPE) + 1 + fd as u64)
}
pub fn pid_from_mem_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_MEM_BASE && r < PROCFS_MEM_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_MEM_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_smaps_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_SMAPS_BASE && r < PROCFS_SMAPS_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_SMAPS_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_maps_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_MAPS_BASE && r < PROCFS_MAPS_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_MAPS_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_exe_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_EXE_BASE && r < PROCFS_EXE_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_EXE_BASE) as u32))
    } else {
        None
    }
}
fn pid_from_fd_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_FD_BASE
        && r < PROCFS_FD_BASE + PROCFS_FD_BLOCK_SPAN
        && (r - PROCFS_FD_BASE).is_multiple_of(PROCFS_FD_STRIPE)
    {
        Some(Pid(((r - PROCFS_FD_BASE) / PROCFS_FD_STRIPE) as u32))
    } else {
        None
    }
}
fn pid_from_fd_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    let base = PROCFS_FD_BASE + 1;
    if r >= base && r < PROCFS_FD_BASE + PROCFS_FD_BLOCK_SPAN {
        let offset = r - base;
        let pid = Pid((offset / PROCFS_FD_STRIPE) as u32);
        let fd = (offset % PROCFS_FD_STRIPE) as u32;
        Some((pid, fd))
    } else {
        None
    }
}
fn pid_from_fdinfo_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_FDINFO_BASE
        && r < PROCFS_FDINFO_BASE + PROCFS_FD_BLOCK_SPAN
        && (r - PROCFS_FDINFO_BASE).is_multiple_of(PROCFS_FD_STRIPE)
    {
        Some(Pid(((r - PROCFS_FDINFO_BASE) / PROCFS_FD_STRIPE) as u32))
    } else {
        None
    }
}
pub fn pid_from_fdinfo_id(id: FsObjectId) -> Option<(Pid, u32)> {
    let r = id.as_u64();
    let base = PROCFS_FDINFO_BASE + 1;
    if r >= base && r < PROCFS_FDINFO_BASE + PROCFS_FD_BLOCK_SPAN {
        let offset = r - base;
        let pid = Pid((offset / PROCFS_FD_STRIPE) as u32);
        let fd = (offset % PROCFS_FD_STRIPE) as u32;
        Some((pid, fd))
    } else {
        None
    }
}

pub fn pid_from_dir(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r > PROCFS_PID_BASE && r < PROCFS_PID_BASE + PROCFS_PID_LIMIT {
        Some(Pid((r - PROCFS_PID_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_stat_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_STAT_BASE && r < PROCFS_STAT_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_STAT_BASE) as u32))
    } else {
        None
    }
}
pub fn pid_from_cmdline_id(id: FsObjectId) -> Option<Pid> {
    let r = id.as_u64();
    if r >= PROCFS_CMDLINE_BASE && r < PROCFS_CMDLINE_BASE + PROCFS_PID_FILE_SPAN {
        Some(Pid((r - PROCFS_CMDLINE_BASE) as u32))
    } else {
        None
    }
}

fn dir_entry(id: FsObjectId, kind: InodeKind, name: &[u8]) -> DirEntry {
    DirEntry::new(id, kind, name).expect("procfs dir entry name")
}

pub const PROCFS_DIR_MODE: u16 = S_IFDIR | 0o555;
pub const PROCFS_FILE_MODE: u16 = S_IFREG | 0o444;
/// Writable projected files (`/proc/<pid>/{uid_map,gid_map,setgroups}`): need a
/// write bit so `open(O_WRONLY)` is permitted and the write routes to
/// `step_write_projected`.
pub const PROCFS_RW_FILE_MODE: u16 = S_IFREG | 0o644;
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
            if name == b"sysvipc" {
                return StepOutcome::done(PROCFS_SYSVIPC_ID);
            }
            if name == b"sys" {
                return StepOutcome::done(PROCFS_SYS_ID);
            }
            if name == b"net" {
                return StepOutcome::done(PROCFS_NET_ID);
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
        if parent == PROCFS_SYS_ID {
            if name == b"kernel" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_ID);
            }
            if name == b"net" {
                return StepOutcome::done(PROCFS_SYS_NET_ID);
            }
            if name == b"fs" {
                return StepOutcome::done(PROCFS_SYS_FS_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
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
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_NET_ID {
            if name == b"if_inet6" {
                return StepOutcome::done(PROCFS_NET_IF_INET6_ID);
            }
            if name == b"tx_neigh" {
                return StepOutcome::done(PROCFS_NET_TX_NEIGH_ID);
            }
            if name == b"tx_neigh_ctl" {
                return StepOutcome::done(PROCFS_NET_TX_NEIGH_CTL_ID);
            }
            if name == b"arp" {
                return StepOutcome::done(PROCFS_NET_ARP_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_NET_ID {
            if name == b"ipv6" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV6_ID);
            }
            if name == b"ipv4" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV4_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_NET_IPV6_ID {
            if name == b"conf" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV6_CONF_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_NET_IPV4_ID {
            if name == b"conf" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV4_CONF_ID);
            }
            if name == b"igmp_max_memberships" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID);
            }
            if name == b"igmp_max_msf" {
                return StepOutcome::done(PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_NET_IPV6_CONF_ID {
            // `all`, `default`, or any per-interface name (`lo`, `eth0`, veth…),
            // each hashed to its own distinct directory inode.
            if !name.is_empty() {
                return StepOutcome::done(ipv6_conf_dir_id(name));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_NET_IPV4_CONF_ID {
            // Reject dotted names: busybox sysctl probes dot→slash splits
            // with access(); a catch-all that resolves
            // "eth0.force_igmp_version" as a directory makes it pick the
            // wrong split and EISDIR on the write.
            if !name.is_empty() && !name.contains(&b'.') {
                return StepOutcome::done(ipv4_conf_dir_id(name));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if ipv6_conf_kind(parent) == Some(0) {
            if name == b"disable_ipv6" {
                return StepOutcome::done(FsObjectId::new(parent.as_u64() + 1));
            }
            if name == b"accept_dad" {
                return StepOutcome::done(FsObjectId::new(parent.as_u64() + 2));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if ipv4_conf_kind(parent) == Some(0) {
            if name == b"force_igmp_version" {
                return StepOutcome::done(FsObjectId::new(parent.as_u64() + 1));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent == PROCFS_SYS_KERNEL_ID {
            if name == b"tainted" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_TAINTED_ID);
            }
            if name == b"pid_max" {
                return StepOutcome::done(PROCFS_SYS_KERNEL_PID_MAX_ID);
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
            if name == b"smaps" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_smaps_id(pid));
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
            if name == b"status" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_status_id(pid));
            }
            if name == b"uid_map" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_uid_map_id(pid));
            }
            if name == b"gid_map" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_gid_map_id(pid));
            }
            if name == b"setgroups" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_setgroups_id(pid));
            }
            if name == b"ns" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_ns_dir_id(pid));
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if let Some(pid) = pid_from_ns_dir(parent) {
            if name == b"net" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_netns_id(pid));
            }
            if name == b"mnt" && process::process_by_pid(pid).is_some() {
                return StepOutcome::done(pid_mntns_id(pid));
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
            PROCFS_MOUNTS_ID
            | PROCFS_CPUINFO_ID
            | PROCFS_UPTIME_ID
            | PROCFS_MEMINFO_ID
            | PROCFS_CONFIG_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_SYSVIPC_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SYSVIPC_MSG_ID | PROCFS_SYSVIPC_SEM_ID | PROCFS_SYSVIPC_SHM_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_SYS_ID | PROCFS_SYS_KERNEL_ID | PROCFS_SYS_FS_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SYS_KERNEL_TAINTED_ID
            | PROCFS_SYS_KERNEL_PID_MAX_ID
            | PROCFS_SYS_FS_PIPE_MAX_SIZE_ID
            | PROCFS_SYS_FS_LEASE_BREAK_TIME_ID
            | PROCFS_SYS_FS_PROTECTED_HARDLINKS_ID
            | PROCFS_SYS_FS_PROTECTED_SYMLINKS_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_NET_ID
            | PROCFS_SYS_NET_ID
            | PROCFS_SYS_NET_IPV6_ID
            | PROCFS_SYS_NET_IPV6_CONF_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_NET_IF_INET6_ID | PROCFS_NET_TX_NEIGH_ID | PROCFS_NET_ARP_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            PROCFS_NET_TX_NEIGH_CTL_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_RW_FILE_MODE))
            }
            // `conf/<name>/` directory.
            id if ipv6_conf_kind(id) == Some(0) => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            // disable_ipv6 / accept_dad are writable: LTP setup does
            // `sysctl -qw net.ipv6.conf.<iface>.accept_dad=0` (and may clear
            // disable_ipv6), which `open(O_WRONLY)`s the file.
            id if matches!(ipv6_conf_kind(id), Some(1) | Some(2)) => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_RW_FILE_MODE))
            }
            PROCFS_SYS_NET_IPV4_ID | PROCFS_SYS_NET_IPV4_CONF_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID
            | PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_RW_FILE_MODE))
            }
            id if ipv4_conf_kind(id) == Some(0) => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if ipv4_conf_kind(id) == Some(1) => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_RW_FILE_MODE))
            }
            id if pid_from_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_uid_map_id(id).is_some()
                || pid_from_gid_map_id(id).is_some()
                || pid_from_setgroups_id(id).is_some() =>
            {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_RW_FILE_MODE))
            }
            id if pid_from_status_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
            }
            id if pid_from_ns_dir(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Directory, PROCFS_DIR_MODE))
            }
            id if pid_from_netns_id(id).is_some() || pid_from_mntns_id(id).is_some() => {
                StepOutcome::done(InodeMeta::new(InodeKind::Regular, PROCFS_FILE_MODE))
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
            id if pid_from_smaps_id(id).is_some() => {
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
                (b"status", pid_status_id(pid), InodeKind::Regular),
                (b"cmdline", pid_cmdline_id(pid), InodeKind::Regular),
                (b"mem", pid_mem_id(pid), InodeKind::Regular),
                (b"maps", pid_maps_id(pid), InodeKind::Regular),
                (b"smaps", pid_smaps_id(pid), InodeKind::Regular),
                (b"exe", pid_exe_id(pid), InodeKind::Symlink),
                (b"fd", pid_fd_dir_id(pid), InodeKind::Directory),
                (b"fdinfo", pid_fdinfo_dir_id(pid), InodeKind::Directory),
                (b"uid_map", pid_uid_map_id(pid), InodeKind::Regular),
                (b"gid_map", pid_gid_map_id(pid), InodeKind::Regular),
                (b"setgroups", pid_setgroups_id(pid), InodeKind::Regular),
                (b"ns", pid_ns_dir_id(pid), InodeKind::Directory),
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

        if let Some(pid) = pid_from_ns_dir(id) {
            // `/proc/<pid>/ns/` — currently exposes `net`.
            if state_byte < 2 {
                return finish_dots(state_byte, idx, id);
            }
            let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                (b"net", pid_netns_id(pid), InodeKind::Regular),
                (b"mnt", pid_mntns_id(pid), InodeKind::Regular),
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
            if id == PROCFS_SYS_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (b"kernel", PROCFS_SYS_KERNEL_ID, InodeKind::Directory),
                    (b"net", PROCFS_SYS_NET_ID, InodeKind::Directory),
                    (b"fs", PROCFS_SYS_FS_ID, InodeKind::Directory),
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
            if id == PROCFS_SYS_KERNEL_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (b"tainted", PROCFS_SYS_KERNEL_TAINTED_ID, InodeKind::Regular),
                    (b"pid_max", PROCFS_SYS_KERNEL_PID_MAX_ID, InodeKind::Regular),
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
            if id == PROCFS_SYS_FS_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (b"pipe-max-size", PROCFS_SYS_FS_PIPE_MAX_SIZE_ID, InodeKind::Regular),
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
                        DirCursor([2, (fi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
                return StepOutcome::done(None);
            }
            if id == PROCFS_NET_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (b"if_inet6", PROCFS_NET_IF_INET6_ID, InodeKind::Regular),
                    (b"tx_neigh", PROCFS_NET_TX_NEIGH_ID, InodeKind::Regular),
                    (b"tx_neigh_ctl", PROCFS_NET_TX_NEIGH_CTL_ID, InodeKind::Regular),
                    (b"arp", PROCFS_NET_ARP_ID, InodeKind::Regular),
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
            if id == PROCFS_SYS_NET_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] =
                    &[(b"ipv6", PROCFS_SYS_NET_IPV6_ID, InodeKind::Directory)];
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
            if id == PROCFS_SYS_NET_IPV6_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] =
                    &[(b"conf", PROCFS_SYS_NET_IPV6_CONF_ID, InodeKind::Directory)];
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
            if id == PROCFS_SYS_NET_IPV6_CONF_ID {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                // `all`, `default`, then one entry per interface in the root
                // netns. All share the generic entry-dir id.
                let mut names: Vec<&[u8]> = alloc::vec![b"all".as_slice(), b"default".as_slice()];
                for link in
                    tx_subsystems::net::namespace::initial_net_namespace_payload().link_snapshot()
                {
                    names.push(link.name.as_bytes());
                }
                let fi = idx.saturating_sub(2);
                if fi < names.len() {
                    return StepOutcome::done(Some((
                        dir_entry(ipv6_conf_dir_id(names[fi]), InodeKind::Directory, names[fi]),
                        DirCursor([2, (fi + 1) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                    )));
                }
                return StepOutcome::done(None);
            }
            if ipv6_conf_kind(id) == Some(0) {
                if state_byte < 2 {
                    return finish_dots(state_byte, idx, id);
                }
                let files: &[(&[u8], FsObjectId, InodeKind)] = &[
                    (
                        b"disable_ipv6",
                        FsObjectId::new(id.as_u64() + 1),
                        InodeKind::Regular,
                    ),
                    (
                        b"accept_dad",
                        FsObjectId::new(id.as_u64() + 2),
                        InodeKind::Regular,
                    ),
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
            (b"config", PROCFS_CONFIG_ID, InodeKind::Regular),
            (b"sysvipc", PROCFS_SYSVIPC_ID, InodeKind::Directory),
            (b"sys", PROCFS_SYS_ID, InodeKind::Directory),
            (b"net", PROCFS_NET_ID, InodeKind::Directory),
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
            // Resolve to the most-recently-created live process (best-effort
            // current pid; see procfs_self_target_pid). Hardcoding pid 1 routed
            // every /proc/self/* access to init — wrong for the userns map writes.
            let pid = procfs_self_target_pid();
            let target = alloc::format!("{}", pid.0);
            StepOutcome::done(target.into_bytes().into_boxed_slice())
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
        // `/proc/<pid>/ns/net` — back the node with the target process's net
        // namespace so `open()` yields a NetNamespace-carrying file that
        // `setns(2)` (and the rtnetlink fd resolvers) can resolve.
        if let Some(pid) = pid_from_netns_id(id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            let Some(payload) = proc.net_namespace() else {
                return StepOutcome::err(Errno::ESRCH.into());
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
                Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
            };
        }
        // `/proc/<pid>/ns/mnt` — back with the target's mount namespace so
        // `setns(2)` can resolve it (mount-ns isolation is deferred; this is
        // the shared mnt ns for now, but tst_ns_exec requires the node to open).
        if let Some(pid) = pid_from_mntns_id(id) {
            let Some(proc) = process::process_by_pid(pid) else {
                return StepOutcome::err(Errno::ENOENT.into());
            };
            let Some(payload) = proc.mount_namespace_cap() else {
                return StepOutcome::err(Errno::ESRCH.into());
            };
            return match RNode::new_cap_in_mount(
                id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::MountNamespace { payload },
                },
                mount,
            ) {
                Ok(cap) => StepOutcome::done(cap),
                Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
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
    fn step_write_projected(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        if let Some(outcome) = write_userns_projection(fs_object_id, offset, bytes) {
            return outcome;
        }
        // `sysctl -w net.ipv6.conf.<iface>.{accept_dad,disable_ipv6}=…` from LTP
        // tst_net setup. We have no per-iface IPv6 toggle state; accept the write
        // (report all bytes consumed) so setup proceeds. disable_ipv6 stays 0.
        if matches!(ipv6_conf_kind(fs_object_id), Some(1) | Some(2)) {
            return StepOutcome::done(bytes.len() as u64);
        }
        // IGMP knobs (mcast-lib.sh setup/cleanup). Store the integer so the
        // save/restore round-trip reads back what was written.
        if fs_object_id == PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID
            || fs_object_id == PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID
            || ipv4_conf_kind(fs_object_id) == Some(1)
        {
            let text = core::str::from_utf8(bytes).unwrap_or("").trim();
            let Ok(value) = text.parse::<u32>() else {
                return StepOutcome::err(Errno::EINVAL.into());
            };
            let target = if fs_object_id == PROCFS_SYS_NET_IPV4_IGMP_MAX_MEMBERSHIPS_ID {
                &IGMP_MAX_MEMBERSHIPS
            } else if fs_object_id == PROCFS_SYS_NET_IPV4_IGMP_MAX_MSF_ID {
                &IGMP_MAX_MSF
            } else {
                &FORCE_IGMP_VERSION
            };
            target.store(value, core::sync::atomic::Ordering::Relaxed);
            return StepOutcome::done(bytes.len() as u64);
        }
        // `ip neigh del` writes "<addr> <dev>" here to drop a neighbor entry.
        if fs_object_id == PROCFS_NET_TX_NEIGH_CTL_ID {
            let _ = tx_subsystems::net::namespace::delete_neighbor_ctl(bytes);
            return StepOutcome::done(bytes.len() as u64);
        }
        StepOutcome::err(Errno::ENOSYS.into())
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

    /// Regression for the 2026-07-02 judge hang: pid 65571 (just past the
    /// old 0x10000 ceiling) stopped resolving as a /proc/<pid> dir, and
    /// LTP's watchdog spun on /proc/<pid>/stat until the lane died. All
    /// pid-keyed encodings must round-trip well past 16 bits, and ids
    /// from different blocks must never cross-decode.
    #[test]
    fn pid_ids_round_trip_past_16_bit_pids() {
        // Pure id arithmetic, but hold the same serialization lock as the
        // other procfs tests so this doesn't perturb their scheduling.
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for pid in [1u32, 65_535, 65_571, 100_000, (PROCFS_PID_LIMIT - 1) as u32] {
            let pid = Pid(pid);
            assert_eq!(pid_from_dir(pid_dir_id(pid)), Some(pid), "dir {pid:?}");
            assert_eq!(
                pid_from_stat_id(pid_stat_id(pid)),
                Some(pid),
                "stat {pid:?}"
            );
            assert_eq!(pid_from_fd_dir(pid_fd_dir_id(pid)), Some(pid), "fd dir");
            assert_eq!(pid_from_fd_id(pid_fd_id(pid, 42)), Some((pid, 42)), "fd");
            assert_eq!(
                pid_from_fdinfo_dir(pid_fdinfo_dir_id(pid)),
                Some(pid),
                "fdinfo dir"
            );
            assert_eq!(
                pid_from_fdinfo_id(pid_fdinfo_id(pid, 7)),
                Some((pid, 7)),
                "fdinfo"
            );
        }

        // Past the ceiling: dir id must NOT decode (and the ceiling is
        // far above any pid a full suite can burn).
        let over = Pid(PROCFS_PID_LIMIT as u32);
        assert_eq!(pid_from_dir(pid_dir_id(over)), None);

        // Cross-block isolation at a high pid: no decoder claims another
        // block's ids.
        let pid = Pid(100_000);
        assert_eq!(pid_from_fd_id(pid_dir_id(pid)), None);
        assert_eq!(pid_from_dir(pid_fd_dir_id(pid)), None);
        assert_eq!(pid_from_fd_dir(pid_fdinfo_dir_id(pid)), None);
        assert_eq!(pid_from_fdinfo_dir(pid_fd_dir_id(pid)), None);
        assert_eq!(pid_from_dir(pid_uid_map_id(pid)), None);
        assert_eq!(pid_from_dir(pid_status_id(pid)), None);
        assert_eq!(pid_from_stat_id(pid_fd_dir_id(pid)), None);
    }
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
                packet: false,
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
