//! Minimal typed fd providers for heavyweight kernel-object syscalls.
//!
//! This phase is intentionally narrow: it validates the creation forms used by
//! LTP's generic fd-provider probes and installs a real, typed non-socket fd.
//! Full perf counter sampling and BPF map operations remain separate
//! subsystem charters.

use super::*;
use tx_subsystems::vfs::structure::{BpfMapFile, KernelObjectFile, PerfEventFile};

pub(super) const PERF_TYPE_SOFTWARE: u32 = 1;
pub(super) const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;

const PERF_ATTR_COPY_BYTES: usize = 48;
const PERF_ATTR_DISABLED: u64 = 1 << 0;
const PERF_ATTR_EXCLUDE_KERNEL: u64 = 1 << 5;
const PERF_ATTR_EXCLUDE_HV: u64 = 1 << 6;
const PERF_ATTR_SUPPORTED_FLAGS: u64 =
    PERF_ATTR_DISABLED | PERF_ATTR_EXCLUDE_KERNEL | PERF_ATTR_EXCLUDE_HV;

pub(super) const BPF_MAP_CREATE: u32 = 0;
pub(super) const BPF_MAP_TYPE_ARRAY: u32 = 2;
const BPF_MAP_CREATE_COPY_BYTES: usize = 20;

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut out = [0; 4];
    out.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(out)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut out = [0; 8];
    out.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(out)
}

fn install_kernel_object_fd(ctx: &SyscallCtx<'_>, object: KernelObjectFile) -> SyscallResult {
    let open_file = match OpenFile::new_kernel_object_cap(
        object,
        OpenFileFlags {
            read: true,
            ..OpenFileFlags::default()
        },
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let fd = match next_stdio_fd_below_nofile(&ctx.process) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.install_fd(fd, open_file);
    SyscallResult::Return(fd as i64)
}

/// `perf_event_open(attr, pid, cpu, group_fd, flags)`.
///
/// Phase 0 accepts the software CPU-clock event shape used by LTP's
/// fd-provider probe. It records the event metadata but does not implement
/// counter reads, mmap rings, ioctl control, or sampling.
pub(super) fn sys_perf_event_open(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let attr_uaddr = args[0];
    if attr_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let mut bytes = [0u8; PERF_ATTR_COPY_BYTES];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, attr_uaddr) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let attr_type = read_u32(&bytes, 0);
    let attr_size = read_u32(&bytes, 4) as usize;
    let config = read_u64(&bytes, 8);
    let attr_flags = read_u64(&bytes, 40);
    let pid = args[1] as i64 as i32;
    let cpu = args[2] as i64 as i32;
    let group_fd = args[3] as i64 as i32;
    let flags = args[4];

    if attr_size < PERF_ATTR_COPY_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if attr_type != PERF_TYPE_SOFTWARE || config != PERF_COUNT_SW_CPU_CLOCK {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    if pid != 0 || cpu != -1 || group_fd != -1 || flags != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    if attr_flags & !PERF_ATTR_SUPPORTED_FLAGS != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }

    install_kernel_object_fd(
        ctx,
        KernelObjectFile::PerfEvent(PerfEventFile {
            attr_type,
            config,
            pid,
            cpu,
            group_fd,
            flags,
        }),
    )
}

/// `bpf(cmd, attr, size)`.
///
/// Phase 0 supports `BPF_MAP_CREATE` for array maps. The returned fd records
/// map metadata; map update/lookup/delete and program operations remain future
/// BPF subsystem work.
pub(super) fn sys_bpf(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cmd = args[0] as u32;
    let attr_uaddr = args[1];
    let attr_size = args[2] as usize;

    if cmd != BPF_MAP_CREATE {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    if attr_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if attr_size < BPF_MAP_CREATE_COPY_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let mut bytes = [0u8; BPF_MAP_CREATE_COPY_BYTES];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, attr_uaddr) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let map_type = read_u32(&bytes, 0);
    let key_size = read_u32(&bytes, 4);
    let value_size = read_u32(&bytes, 8);
    let max_entries = read_u32(&bytes, 12);
    let map_flags = read_u32(&bytes, 16);

    if map_type != BPF_MAP_TYPE_ARRAY {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    if key_size == 0 || value_size == 0 || max_entries == 0 || map_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    install_kernel_object_fd(
        ctx,
        KernelObjectFile::BpfMap(BpfMapFile {
            map_type,
            key_size,
            value_size,
            max_entries,
            map_flags,
        }),
    )
}
