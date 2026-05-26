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
const UNAME26: u32 = 0x0020_000;
const ADDR_NO_RANDOMIZE: u32 = 0x0040_000;
const FDPIC_FUNCPTRS: u32 = 0x0080_000;
const MMAP_PAGE_ZERO: u32 = 0x0100_000;
const ADDR_COMPAT_LAYOUT: u32 = 0x0200_000;
const READ_IMPLIES_EXEC: u32 = 0x0400_000;
const ADDR_LIMIT_32BIT: u32 = 0x0800_000;
const SHORT_INODE: u32 = 0x1000_000;
const WHOLE_SECONDS: u32 = 0x2000_000;
const STICKY_TIMEOUTS: u32 = 0x4000_000;
const ADDR_LIMIT_3GB: u32 = 0x8000_000;
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

/// `sethostname(name, len)` — Linux generic ABI `__NR_sethostname = 161`.
///
/// txKernel has a single global UTS nodename for now. This is enough for
/// libc/LTP `gethostname()` probes, which update the hostname and then
/// read it back through `uname().nodename`.
pub(super) fn sys_sethostname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let name_uaddr = args[0];
    let len = args[1] as usize;

    if len > UTSNAME_FIELD - 1 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if len != 0 && name_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let mut next = [0u8; UTSNAME_FIELD];
    if len != 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut next[..len], name_uaddr) {
            return SyscallResult::error_from(errno);
        }
    }
    *UTS_NODENAME.lock() = next;
    SyscallResult::Return(0)
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
    let utsname = build_utsname_for_machine(P::arch_auxv_facts().platform);
    if let Err(errno) = bootstrap_write_user::<UtsnameLayout>(&ctx.aspace, buf_uaddr, utsname) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
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

pub(super) fn build_utsname_for_machine(machine: &str) -> UtsnameLayout {
    fn pad(s: &str) -> [u8; UTSNAME_FIELD] {
        let mut out = [0u8; UTSNAME_FIELD];
        let bytes = s.as_bytes();
        // Reserve the trailing NUL byte. `min(len, 64)` clamps the
        // copy so `out[64] = 0` always.
        let n = core::cmp::min(bytes.len(), UTSNAME_FIELD - 1);
        let (head, _) = out.split_at_mut(n);
        head.copy_from_slice(&bytes[..n]);
        out
    }
    let nodename = *UTS_NODENAME.lock();
    UtsnameLayout {
        sysname: pad("Linux"),
        nodename,
        // Linux 6.1.0 is the LTS line musl 1.2.x runtime probes treat
        // as fully featured.
        release: pad("6.1.0-txkernel"),
        version: pad("#1 SMP txkernel"),
        machine: pad(machine),
        domainname: pad("(none)"),
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

    pub(super) use super::{RlimitLayout, UtsnameLayout};
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
