//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::SpinMutex;

static UTS_NODENAME: SpinMutex<[u8; UTSNAME_FIELD]> = SpinMutex::new(default_nodename());

const PERSONALITY_QUERY: u32 = u32::MAX;
const PER_MASK: u32 = 0x00ff;
const PER_HPUX: u32 = 0x0010;
const UNAME26: u32 = 0x0002_0000;
const ADDR_NO_RANDOMIZE: u32 = 0x0004_0000;
const FDPIC_FUNCPTRS: u32 = 0x0008_0000;
const MMAP_PAGE_ZERO: u32 = 0x0010_0000;
const ADDR_COMPAT_LAYOUT: u32 = 0x0020_0000;
const READ_IMPLIES_EXEC: u32 = 0x0040_0000;
const ADDR_LIMIT_32BIT: u32 = 0x0080_0000;
const SHORT_INODE: u32 = 0x0100_0000;
const WHOLE_SECONDS: u32 = 0x0200_0000;
const STICKY_TIMEOUTS: u32 = 0x0400_0000;
const ADDR_LIMIT_3GB: u32 = 0x0800_0000;
const PERSONALITY_KNOWN_FLAGS: u32 = UNAME26
    | ADDR_NO_RANDOMIZE
    | FDPIC_FUNCPTRS
    | MMAP_PAGE_ZERO
    | ADDR_COMPAT_LAYOUT
    | READ_IMPLIES_EXEC
    | ADDR_LIMIT_32BIT
    | SHORT_INODE
    | WHOLE_SECONDS
    | STICKY_TIMEOUTS
    | ADDR_LIMIT_3GB;

#[cfg(test)]
pub(crate) fn reset_uts_nodename_for_test() {
    *UTS_NODENAME.lock() = default_nodename();
}

const fn default_nodename() -> [u8; UTSNAME_FIELD] {
    let mut out = [0u8; UTSNAME_FIELD];
    out[0] = b't';
    out[1] = b'x';
    out[2] = b'k';
    out[3] = b'e';
    out[4] = b'r';
    out[5] = b'n';
    out[6] = b'e';
    out[7] = b'l';
    out
}

/// `personality(persona)` — Linux generic ABI `__NR_personality = 92`.
///
/// `0xffffffff` is the read-only query sentinel; other recognised values
/// replace the per-process personality and return the previous one. The
/// behavioural side effects of compatibility flags are intentionally small
/// for now, but recording the value is enough for libc and LTP readback
/// probes such as `personality01` and `personality02`.
pub(super) fn sys_personality<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let persona = args[0] as u32;
    if persona == PERSONALITY_QUERY {
        return SyscallResult::Return(ctx.process.personality() as i64);
    }

    if persona & !(PER_MASK | PERSONALITY_KNOWN_FLAGS) != 0 || (persona & PER_MASK) > PER_HPUX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let old = ctx.process.swap_personality(persona);
    SyscallResult::Return(old as i64)
}

/// `getrandom(buf, buflen, flags)` — Linux RV64 generic ABI
/// `__NR_getrandom = 278`.
///
/// Slice 7 v1: fills `buflen` bytes at `buf` from the kernel CSPRNG.
/// Supported `flags` (`GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE`)
/// are accepted but ignored; unsupported bits return `-EINVAL`.
///
/// User-VA writeback flows through `bootstrap_copy_to_user`
/// (canonical `aspace.copy_to_user` lane with kernel-pointer fallback
/// for test scaffolding). Null `buf` with non-zero `buflen` returns
/// `-EFAULT`; `buflen == 0` is a successful no-op (`Return(0)`).
pub(super) fn sys_getrandom<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let buf_len = args[1] as usize;
    let flags = args[2] as u32;
    let supported_flags = GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE;

    if flags & !supported_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if buf_len == 0 {
        return SyscallResult::Return(0);
    }
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Fill from the kernel CSPRNG, then copy out through the
    // canonical user-VA lane.
    let mut tmp = alloc::vec![0u8; buf_len];
    tx_services::random::fill_bytes(&mut tmp);
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &tmp) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(buf_len as i64)
}

pub(super) fn sys_sethostname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    set_uts_name_from_user(ctx, args[0], args[1] as usize, UtsNameKind::Host)
}

pub(super) fn sys_setdomainname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    set_uts_name_from_user(ctx, args[0], args[1] as usize, UtsNameKind::Domain)
}

/// `uname(buf)` — Linux generic ABI `__NR_uname = 160`.
///
/// Writes a static utsname (`sysname` / `nodename` / `release` /
/// `version` / `machine` / `domainname`) to `buf`. Each field is a
/// `[u8; 65]` NUL-padded string. Slice 7 pins:
///
/// - `sysname = "Linux"` so musl's runtime "is this Linux?" probe
///   succeeds.
/// - `release = "6.1.0-txkernel"` so the version-triple parser at the
///   front of the string sees a Linux 2.6.16+ kernel (musl's
///   kernel-feature gating reads only the leading digits).
/// - `machine` follows the selected platform ABI (`riscv64` or
///   `loongarch64`) so musl's architecture probes see the right target.
///
/// SAFETY: kernel-buffer exemption (mirrors `sys_getresuid`).
pub(super) fn sys_uname<'a, P: AuxvIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let Some(nsproxy) = ctx.process.nsproxy_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let hostname = nsproxy.uts_ns.hostname();
    let domainname = nsproxy.uts_ns.domainname();
    let utsname = build_utsname_for_machine(P::arch_auxv_facts().platform, &hostname, &domainname);
    if let Err(errno) = bootstrap_write_user::<UtsnameLayout>(&ctx.aspace, buf_uaddr, utsname) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SysinfoLayout {
    pub(super) uptime: i64,
    pub(super) loads: [u64; 3],
    pub(super) totalram: u64,
    pub(super) freeram: u64,
    pub(super) sharedram: u64,
    pub(super) bufferram: u64,
    pub(super) totalswap: u64,
    pub(super) freeswap: u64,
    pub(super) procs: u16,
    pub(super) pad: u16,
    pub(super) totalhigh: u64,
    pub(super) freehigh: u64,
    pub(super) mem_unit: u32,
    pub(super) _f: [u8; 0],
}

const _: () = assert!(core::mem::size_of::<SysinfoLayout>() == 112);
const SYSINFO_TOTAL_RAM_BYTES: u64 = 256 * 1024 * 1024;
const SYSINFO_FREE_RAM_BYTES: u64 = 128 * 1024 * 1024;

pub(super) fn sys_sysinfo<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let info_uaddr = args[0];
    if info_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let ns = <P as TimeIf>::read_ns();
    let uptime = ns.saturating_add(999_999_999) / 1_000_000_000;
    let procs = core::cmp::max(ctx.process.live_thread_count(), 1).min(u16::MAX as usize) as u16;
    let info = SysinfoLayout {
        uptime: uptime as i64,
        loads: [0; 3],
        totalram: SYSINFO_TOTAL_RAM_BYTES,
        freeram: SYSINFO_FREE_RAM_BYTES,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs,
        pad: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
        _f: [],
    };
    if let Err(errno) = bootstrap_write_user::<SysinfoLayout>(&ctx.aspace, info_uaddr, info) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_prctl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    match args[0] {
        PR_GET_DUMPABLE => match ctx.process.dumpable() {
            Some(value) => SyscallResult::Return(value as i64),
            None => SyscallResult::Error(ESRCH_VALUE),
        },
        PR_SET_DUMPABLE => {
            let value = args[1];
            if value > 1 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            if ctx.process.set_dumpable(value as u8) {
                SyscallResult::Return(0)
            } else {
                SyscallResult::Error(ESRCH_VALUE)
            }
        }
        PR_GET_TIMING => SyscallResult::Return(PR_TIMING_STATISTICAL as i64),
        PR_SET_TIMING => {
            if args[1] == PR_TIMING_STATISTICAL {
                SyscallResult::Return(0)
            } else {
                SyscallResult::Error(EINVAL_VALUE)
            }
        }
        PR_SET_NAME => set_prctl_name(ctx, args[1]),
        PR_GET_NAME => {
            let comm = ctx.process.comm();
            match bootstrap_copy_to_user(&ctx.aspace, args[1], &comm) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        PR_SET_NO_NEW_PRIVS | PR_GET_NO_NEW_PRIVS => SyscallResult::Error(ENOSYS_VALUE),
        _ => SyscallResult::Error(EINVAL_VALUE),
    }
}

fn set_prctl_name<'a>(ctx: &SyscallCtx<'a>, name_uaddr: u64) -> SyscallResult {
    let mut comm = [0u8; 16];
    for (idx, slot) in comm.iter_mut().take(15).enumerate() {
        let Some(addr) = name_uaddr.checked_add(idx as u64) else {
            return SyscallResult::Error(EFAULT_VALUE);
        };
        match bootstrap_read_user::<u8>(&ctx.aspace, addr) {
            Ok(0) => break,
            Ok(byte) => *slot = byte,
            Err(errno) => return SyscallResult::error_from(errno),
        }
    }
    if ctx.process.set_comm(comm) {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(ESRCH_VALUE)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RiscvHwprobePair {
    key: i64,
    value: u64,
}

const _: () = assert!(core::mem::size_of::<RiscvHwprobePair>() == 16);
const RISCV_HWPROBE_MAX_KEY: i64 = RISCV_HWPROBE_KEY_VENDOR_EXT_SIFIVE_0;

pub(super) fn sys_riscv_hwprobe<P: TimeIf + SmpIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let pairs_uaddr = args[0];
    let pair_count = args[1] as usize;
    let cpusetsize = args[2] as usize;
    let cpus_uaddr = args[3];
    let flags = args[4];

    match flags {
        0 => riscv_hwprobe_get_values::<P>(ctx, pairs_uaddr, pair_count, cpusetsize, cpus_uaddr),
        RISCV_HWPROBE_WHICH_CPUS => {
            riscv_hwprobe_which_cpus::<P>(ctx, pairs_uaddr, pair_count, cpusetsize, cpus_uaddr)
        }
        _ => SyscallResult::Error(EINVAL_VALUE),
    }
}

fn riscv_hwprobe_get_values<P: TimeIf + SmpIf>(
    ctx: &SyscallCtx<'_>,
    pairs_uaddr: u64,
    pair_count: usize,
    cpusetsize: usize,
    cpus_uaddr: u64,
) -> SyscallResult {
    let online = P::online_cpus().bits();
    let mask = match read_hwprobe_cpuset(ctx, cpusetsize, cpus_uaddr, online, false) {
        Ok(mask) => mask,
        Err(result) => return result,
    };
    if mask == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    for idx in 0..pair_count {
        let pair_addr = match hwprobe_pair_addr(pairs_uaddr, idx) {
            Some(addr) => addr,
            None => return SyscallResult::Error(EFAULT_VALUE),
        };
        let mut pair = match bootstrap_read_user::<RiscvHwprobePair>(&ctx.aspace, pair_addr) {
            Ok(pair) => pair,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        match riscv_hwprobe_value::<P>(pair.key) {
            Some(value) => pair.value = value,
            None => {
                pair.key = -1;
                pair.value = 0;
            }
        }
        if let Err(errno) = bootstrap_write_user::<RiscvHwprobePair>(&ctx.aspace, pair_addr, pair) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

fn riscv_hwprobe_which_cpus<P: TimeIf + SmpIf>(
    ctx: &SyscallCtx<'_>,
    pairs_uaddr: u64,
    pair_count: usize,
    cpusetsize: usize,
    cpus_uaddr: u64,
) -> SyscallResult {
    if cpusetsize == 0 || cpus_uaddr == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let online = P::online_cpus().bits();
    let mut candidate_mask = match read_hwprobe_cpuset(ctx, cpusetsize, cpus_uaddr, online, true) {
        Ok(mask) => mask,
        Err(result) => return result,
    };

    for idx in 0..pair_count {
        let pair_addr = match hwprobe_pair_addr(pairs_uaddr, idx) {
            Some(addr) => addr,
            None => return SyscallResult::Error(EFAULT_VALUE),
        };
        let mut pair = match bootstrap_read_user::<RiscvHwprobePair>(&ctx.aspace, pair_addr) {
            Ok(pair) => pair,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        if !riscv_hwprobe_key_is_valid(pair.key) {
            pair.key = -1;
            pair.value = 0;
            if let Err(errno) =
                bootstrap_write_user::<RiscvHwprobePair>(&ctx.aspace, pair_addr, pair)
            {
                return SyscallResult::error_from(errno);
            }
            candidate_mask = 0;
            break;
        }

        let Some(value) = riscv_hwprobe_value::<P>(pair.key) else {
            candidate_mask = 0;
            break;
        };
        if !riscv_hwprobe_pair_matches(pair.key, value, pair.value) {
            candidate_mask = 0;
        }
    }

    match write_hwprobe_cpuset(ctx, cpusetsize, cpus_uaddr, candidate_mask & online) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

fn hwprobe_pair_addr(base: u64, idx: usize) -> Option<u64> {
    base.checked_add((idx * core::mem::size_of::<RiscvHwprobePair>()) as u64)
}

fn read_hwprobe_cpuset(
    ctx: &SyscallCtx<'_>,
    cpusetsize: usize,
    cpus_uaddr: u64,
    online: u64,
    empty_means_online: bool,
) -> Result<u64, SyscallResult> {
    if cpusetsize == 0 && cpus_uaddr == 0 {
        return Ok(online);
    }
    if cpusetsize != 0 && cpus_uaddr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }

    let mut bytes = [0u8; 8];
    let n = core::cmp::min(cpusetsize, bytes.len());
    if n != 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes[..n], cpus_uaddr) {
            return Err(SyscallResult::error_from(errno));
        }
    }
    let requested = u64::from_ne_bytes(bytes);
    if requested == 0 && empty_means_online {
        Ok(online)
    } else {
        Ok(requested & online)
    }
}

fn write_hwprobe_cpuset(
    ctx: &SyscallCtx<'_>,
    cpusetsize: usize,
    cpus_uaddr: u64,
    mask: u64,
) -> Result<(), tx_subsystems::execution::Errno> {
    let bytes = mask.to_ne_bytes();
    let n = core::cmp::min(cpusetsize, bytes.len());
    bootstrap_copy_to_user(&ctx.aspace, cpus_uaddr, &bytes[..n])
}

fn riscv_hwprobe_key_is_valid(key: i64) -> bool {
    (0..=RISCV_HWPROBE_MAX_KEY).contains(&key)
}

fn riscv_hwprobe_key_is_bitmask(key: i64) -> bool {
    matches!(
        key,
        RISCV_HWPROBE_KEY_BASE_BEHAVIOR
            | RISCV_HWPROBE_KEY_IMA_EXT_0
            | RISCV_HWPROBE_KEY_CPUPERF_0
            | RISCV_HWPROBE_KEY_VENDOR_EXT_THEAD_0
            | RISCV_HWPROBE_KEY_VENDOR_EXT_SIFIVE_0
    )
}

fn riscv_hwprobe_pair_matches(key: i64, actual: u64, requested: u64) -> bool {
    if riscv_hwprobe_key_is_bitmask(key) {
        (actual & requested) == requested
    } else {
        actual == requested
    }
}

fn riscv_hwprobe_value<P: TimeIf>(key: i64) -> Option<u64> {
    match key {
        RISCV_HWPROBE_KEY_MVENDORID | RISCV_HWPROBE_KEY_MARCHID | RISCV_HWPROBE_KEY_MIMPID => {
            Some(0)
        }
        RISCV_HWPROBE_KEY_BASE_BEHAVIOR => Some(RISCV_HWPROBE_BASE_BEHAVIOR_IMA),
        RISCV_HWPROBE_KEY_IMA_EXT_0 => Some(0),
        RISCV_HWPROBE_KEY_CPUPERF_0 => Some(RISCV_HWPROBE_MISALIGNED_SLOW),
        RISCV_HWPROBE_KEY_ZICBOZ_BLOCK_SIZE | RISCV_HWPROBE_KEY_ZICBOM_BLOCK_SIZE => Some(0),
        RISCV_HWPROBE_KEY_HIGHEST_VIRT_ADDRESS => {
            Some(tx_subsystems::vm::FULL_USER_V1_TOP as u64 - 1)
        }
        RISCV_HWPROBE_KEY_TIME_CSR_FREQ => Some(P::frequency_hz()),
        RISCV_HWPROBE_KEY_MISALIGNED_SCALAR_PERF => Some(RISCV_HWPROBE_MISALIGNED_SCALAR_SLOW),
        RISCV_HWPROBE_KEY_MISALIGNED_VECTOR_PERF => Some(RISCV_HWPROBE_MISALIGNED_VECTOR_UNKNOWN),
        RISCV_HWPROBE_KEY_VENDOR_EXT_THEAD_0 | RISCV_HWPROBE_KEY_VENDOR_EXT_SIFIVE_0 => Some(0),
        _ => None,
    }
}

pub(super) fn sys_riscv_flush_icache<P: CacheIf>(args: [u64; 6]) -> SyscallResult {
    let flags = args[2];
    if flags & !SYS_RISCV_FLUSH_ICACHE_ALL != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if flags & SYS_RISCV_FLUSH_ICACHE_LOCAL != 0 {
        P::fence_i_local();
    } else {
        P::fence_i_all();
    }
    SyscallResult::Return(0)
}

#[derive(Clone, Copy)]
enum UtsNameKind {
    Host,
    Domain,
}

fn set_uts_name_from_user<'a>(
    ctx: &SyscallCtx<'a>,
    name_uaddr: u64,
    len: usize,
    kind: UtsNameKind,
) -> SyscallResult {
    if len > tx_subsystems::process::nsproxy::UTS_NAME_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let Some(nsproxy) = ctx.process.nsproxy_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let owner_user_ns = nsproxy.uts_ns.owner_user_namespace();
    if !tx_subsystems::process::nsproxy::has_capability_in_subject_user_namespace(
        ctx.cred(),
        &nsproxy.user_ns,
        &owner_user_ns,
        Capability::SYS_ADMIN,
    ) {
        return SyscallResult::Error(EPERM_VALUE);
    }

    let mut name = alloc::vec![0u8; len];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut name, name_uaddr) {
        return SyscallResult::error_from(errno);
    }

    let result = match kind {
        UtsNameKind::Host => nsproxy.uts_ns.set_hostname(&name),
        UtsNameKind::Domain => nsproxy.uts_ns.set_domainname(&name),
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

/// `prlimit64(pid, resource, new_rlim, old_rlim)` — Linux RV64
/// generic ABI `__NR_prlimit64 = 261`.
///
/// Slice 7 v1: read-only static rlimit table for the calling process.
/// `pid == 0` or `pid == self.pid` is the only supported target;
/// cross-pid queries return `-EPERM`. `new_rlim` is silently ignored
/// — limits are not actually enforced by any in-tree subsystem yet
/// (`TODO(phase-rlimit-enforcement)`). The static table is generous
/// (`RLIMIT_NOFILE = (1024, 4096)`, `RLIMIT_STACK = 8 MiB`, the rest
/// `RLIM_INFINITY`).
///
/// Unknown resource ids return `-EINVAL`. Null `old_rlim` is OK (the
/// arm just reports back via the return value) — Linux only requires
/// the writeback when `old_rlim` is non-null.
pub(super) fn sys_prlimit64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as u32;
    let resource = args[1] as u32;
    let new_uaddr = args[2];
    let old_uaddr = args[3];

    if pid != 0 && pid != ctx.process.pid.0 {
        // TODO(phase-pid-resolver): cross-pid prlimit64 once a global
        // pid → Cap<ProcessIdentity> table is wired.
        return SyscallResult::Error(EPERM_VALUE);
    }

    if (resource == RLIMIT_NOFILE || resource == RLIMIT_MEMLOCK) && new_uaddr != 0 {
        let new_limit = match bootstrap_read_user::<RlimitLayout>(&ctx.aspace, new_uaddr) {
            Ok(limit) => limit,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        if new_limit.rlim_cur > new_limit.rlim_max
            || (resource == RLIMIT_NOFILE && new_limit.rlim_max > u32::MAX as u64)
        {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if resource == RLIMIT_NOFILE {
            ctx.process
                .set_rlimit_nofile(new_limit.rlim_cur as u32, new_limit.rlim_max as u32);
        } else {
            ctx.process
                .set_rlimit_memlock(new_limit.rlim_cur, new_limit.rlim_max);
        }
    }

    let limit = match resource {
        RLIMIT_NOFILE => {
            let (cur, max) = ctx.process.rlimit_nofile();
            RlimitLayout {
                rlim_cur: cur as u64,
                rlim_max: max as u64,
            }
        }
        RLIMIT_STACK => RlimitLayout {
            rlim_cur: 8 * 1024 * 1024,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_CORE => RlimitLayout {
            rlim_cur: 0,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_MEMLOCK => {
            let (cur, max) = ctx.process.rlimit_memlock();
            RlimitLayout {
                rlim_cur: cur,
                rlim_max: max,
            }
        }
        RLIMIT_CPU | RLIMIT_FSIZE | RLIMIT_DATA | RLIMIT_RSS | RLIMIT_NPROC | RLIMIT_AS
        | RLIMIT_LOCKS | RLIMIT_SIGPENDING | RLIMIT_MSGQUEUE | RLIMIT_NICE | RLIMIT_RTPRIO
        | RLIMIT_RTTIME => RlimitLayout {
            rlim_cur: RLIM_INFINITY,
            rlim_max: RLIM_INFINITY,
        },
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    if old_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<RlimitLayout>(&ctx.aspace, old_uaddr, limit) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

/// `getrlimit(resource, rlim)` — old generic ABI facade over the same
/// rlimit table used by `prlimit64(pid=0, ..., old_rlim)`.
pub(super) fn sys_getrlimit<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let resource = args[0] as u32;
    let old_uaddr = args[1];

    if old_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    sys_prlimit64([0, resource as u64, 0, old_uaddr, 0, 0], ctx)
}

/// `setrlimit(resource, rlim)` — old generic ABI facade over the same
/// rlimit table used by `prlimit64(pid=0, ..., new_rlim, NULL)`.
pub(super) fn sys_setrlimit<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let resource = args[0] as u32;
    let new_uaddr = args[1];

    if new_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    sys_prlimit64([0, resource as u64, new_uaddr, 0, 0, 0], ctx)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct UtsnameLayout {
    pub(super) sysname: [u8; UTSNAME_FIELD],
    pub(super) nodename: [u8; UTSNAME_FIELD],
    pub(super) release: [u8; UTSNAME_FIELD],
    pub(super) version: [u8; UTSNAME_FIELD],
    pub(super) machine: [u8; UTSNAME_FIELD],
    pub(super) domainname: [u8; UTSNAME_FIELD],
}

pub(super) fn build_utsname_for_machine(
    machine: &str,
    hostname: &[u8],
    domainname: &[u8],
) -> UtsnameLayout {
    fn pad_bytes(bytes: &[u8]) -> [u8; UTSNAME_FIELD] {
        let mut out = [0u8; UTSNAME_FIELD];
        // Reserve the trailing NUL byte. `min(len, 64)` clamps the
        // copy so `out[64] = 0` always.
        let n = core::cmp::min(bytes.len(), UTSNAME_FIELD - 1);
        let (head, _) = out.split_at_mut(n);
        head.copy_from_slice(&bytes[..n]);
        out
    }
    fn pad(s: &str) -> [u8; UTSNAME_FIELD] {
        pad_bytes(s.as_bytes())
    }
    UtsnameLayout {
        sysname: pad("Linux"),
        nodename: pad_bytes(hostname),
        // Linux 6.1.0 is the LTS line musl 1.2.x runtime probes treat
        // as fully featured.
        release: pad("6.1.0-txkernel"),
        version: pad("#1 SMP txkernel"),
        machine: pad(machine),
        domainname: pad_bytes(domainname),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RlimitLayout {
    rlim_cur: u64,
    rlim_max: u64,
}

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    pub(super) use super::{RlimitLayout, SysinfoLayout, UtsnameLayout};
    use crate::linux_syscall::{KernelToUserLayout, KernelUserField, KernelUserLayout};

    impl KernelToUserLayout for UtsnameLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "UtsnameLayout",
            musl_header: "sys/utsname.h",
            musl_type: "struct utsname",
            size: size_of::<UtsnameLayout>(),
            align: align_of::<UtsnameLayout>(),
            fields: &[
                KernelUserField {
                    rust: "sysname",
                    musl: "sysname",
                    offset: offset_of!(UtsnameLayout, sysname),
                },
                KernelUserField {
                    rust: "nodename",
                    musl: "nodename",
                    offset: offset_of!(UtsnameLayout, nodename),
                },
                KernelUserField {
                    rust: "release",
                    musl: "release",
                    offset: offset_of!(UtsnameLayout, release),
                },
                KernelUserField {
                    rust: "version",
                    musl: "version",
                    offset: offset_of!(UtsnameLayout, version),
                },
                KernelUserField {
                    rust: "machine",
                    musl: "machine",
                    offset: offset_of!(UtsnameLayout, machine),
                },
                KernelUserField {
                    rust: "domainname",
                    musl: "domainname",
                    offset: offset_of!(UtsnameLayout, domainname),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const UTSNAME_LAYOUT: KernelUserLayout =
        <UtsnameLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for RlimitLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "RlimitLayout",
            musl_header: "sys/resource.h",
            musl_type: "struct rlimit",
            size: size_of::<RlimitLayout>(),
            align: align_of::<RlimitLayout>(),
            fields: &[
                KernelUserField {
                    rust: "rlim_cur",
                    musl: "rlim_cur",
                    offset: offset_of!(RlimitLayout, rlim_cur),
                },
                KernelUserField {
                    rust: "rlim_max",
                    musl: "rlim_max",
                    offset: offset_of!(RlimitLayout, rlim_max),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const RLIMIT_LAYOUT: KernelUserLayout =
        <RlimitLayout as KernelToUserLayout>::LAYOUT;
}
