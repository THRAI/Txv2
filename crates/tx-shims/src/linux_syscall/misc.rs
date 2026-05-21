//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

/// `getrandom(buf, buflen, flags)` — Linux RV64 generic ABI
/// `__NR_getrandom = 278`.
///
/// Slice 7 v1: fills `buflen` bytes at `buf` from
/// `<P as EntropyIf>::fill_random`. The `flags` arg is recognised
/// (`GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE`) but ignored — the
/// in-tree default impl is deterministic + non-blocking.
///
/// User-VA writeback flows through `bootstrap_copy_to_user`
/// (canonical `aspace.copy_to_user` lane with kernel-pointer fallback
/// for test scaffolding). Null `buf` with non-zero `buflen` returns
/// `-EFAULT`; `buflen == 0` is a successful no-op (`Return(0)`).
pub(super) fn sys_getrandom<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let buf_len = args[1] as usize;
    let _flags = args[2] as u32; // GRND_* recognised but ignored.

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
    let _new_uaddr = args[2]; // ignored — limits not enforced today.
    let old_uaddr = args[3];

    if pid != 0 && pid != ctx.process.pid.0 {
        // TODO(phase-pid-resolver): cross-pid prlimit64 once a global
        // pid → Cap<ProcessIdentity> table is wired.
        return SyscallResult::Error(EPERM_VALUE);
    }

    let limit = match resource {
        RLIMIT_NOFILE => RlimitLayout {
            rlim_cur: 1024,
            rlim_max: 4096,
        },
        RLIMIT_STACK => RlimitLayout {
            rlim_cur: 8 * 1024 * 1024,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_CORE => RlimitLayout {
            rlim_cur: 0,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_CPU | RLIMIT_FSIZE | RLIMIT_DATA | RLIMIT_RSS | RLIMIT_NPROC | RLIMIT_MEMLOCK
        | RLIMIT_AS | RLIMIT_LOCKS | RLIMIT_SIGPENDING | RLIMIT_MSGQUEUE | RLIMIT_NICE
        | RLIMIT_RTPRIO | RLIMIT_RTTIME => RlimitLayout {
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
    UtsnameLayout {
        sysname: pad("Linux"),
        nodename: pad("txkernel"),
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
