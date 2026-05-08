// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFL, F_SETFL, NR_FCNTL, NR_GETRANDOM, NR_KILL, NR_PRLIMIT64,
    NR_RT_SIGRETURN, NR_TGKILL, NR_TKILL, NR_UNAME, O_RDWR, RLIMIT_AS, RLIMIT_NOFILE,
    RLIM_INFINITY,
};

const E_BADF: i32 = 9;
const E_NOSYS: i32 = 38;
const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_PERM: i32 = 1;
const E_SRCH: i32 = 3;

// -----------------------------------------------------------------
// F_DUPFD / F_DUPFD_CLOEXEC / F_GETFL / F_SETFL.
// -----------------------------------------------------------------

/// `fcntl(fd, F_DUPFD, min)` returns the lowest unused fd ≥ min,
/// referring to the same OpenFile and with the cloexec bit cleared.
#[test]
fn dispatch_fcntl_f_dupfd_returns_new_fd_at_or_above_min() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    // Pre-mark fd 5 cloexec so we can assert F_DUPFD clears the
    // bit on the new fd (POSIX: F_DUPFD never inherits cloexec).
    proc_cap.set_fd_cloexec(3, true);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_DUPFD as u64, 10, 0, 0, 0]),
        &ctx,
    ));
    let new_fd = match r {
        SyscallResult::Return(v) => v as u32,
        other => panic!("F_DUPFD expected Return(_), got {other:?}"),
    };
    assert!(
        new_fd >= 10,
        "F_DUPFD must return an fd >= the requested min (got {new_fd})"
    );
    assert!(
        proc_cap.fd(new_fd).is_some(),
        "F_DUPFD must install the duplicated OpenFile at the returned fd"
    );
    assert!(
        !proc_cap.fd_cloexec(new_fd),
        "F_DUPFD must clear the cloexec bit on the new fd (POSIX)"
    );
    assert!(
        proc_cap.fd_cloexec(3),
        "F_DUPFD must not perturb the source fd's cloexec bit"
    );
}

/// `fcntl(fd, F_DUPFD_CLOEXEC, min)` returns the new fd with the
/// per-fd cloexec bit set.
#[test]
fn dispatch_fcntl_f_dupfd_cloexec_sets_cloexec_on_new_fd() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_DUPFD_CLOEXEC as u64, 7, 0, 0, 0]),
        &ctx,
    ));
    let new_fd = match r {
        SyscallResult::Return(v) => v as u32,
        other => panic!("F_DUPFD_CLOEXEC expected Return(_), got {other:?}"),
    };
    assert!(new_fd >= 7);
    assert!(
        proc_cap.fd_cloexec(new_fd),
        "F_DUPFD_CLOEXEC must set the cloexec bit on the new fd"
    );
}

/// `fcntl(fd, F_GETFL, _)` composes the access-mode bits from
/// the per-OpenFile flags. The console is opened RW so the result
/// is `O_RDWR`.
#[test]
fn dispatch_fcntl_f_getfl_returns_open_flag_bits() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_GETFL as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    // `open_console_for_init` opens read=true, write=true,
    // append=false, nonblocking=false → bits = O_RDWR (2).
    assert_eq!(r, SyscallResult::Return(O_RDWR as i64));
}

/// `fcntl(fd, F_SETFL, _)` returns `-ENOSYS` for now (carryover —
/// `OpenFileFlags` is a plain Copy-struct field on OpenFile, no
/// interior mutability yet; future slice owns this).
#[test]
fn dispatch_fcntl_f_setfl_returns_neg_enosys() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_SETFL as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_NOSYS));
}

// -----------------------------------------------------------------
// getpgrp.
// -----------------------------------------------------------------

// Note: `dispatch_getpgrp_returns_caller_pgid` already lives at
// line ~1910 in this file (replaced the previous -ENOSYS test —
// Slice 7 made `getpgrp` a real arm).

// -----------------------------------------------------------------
// kill / tkill / tgkill.
// -----------------------------------------------------------------

/// `kill(self_pid, SIGTERM)` returns 0 — the post is delivered to
/// the calling process's leader thread. The process becomes a
/// zombie via the (separately-tested) signal-driven exit path, but
/// the test only asserts the return value (which is what userspace
/// sees).
#[test]
fn dispatch_kill_self_with_sigterm_succeeds() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    // SIGTERM = 15 (catchable; routes through post_signal, no
    // zombification side-effect on the calling thread).
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `kill(unknown_pid, sig)` returns `-ESRCH`.
#[test]
fn dispatch_kill_unknown_pid_returns_neg_esrch() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // pid 9999 is well above the bootstrap pid counter.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [9999, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_SRCH));
}

/// `kill(pid, 0)` is the existence probe: returns 0 for live
/// targets without delivering anything.
#[test]
fn dispatch_kill_signal_zero_against_live_returns_zero() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    // The process must remain live — `sig == 0` is probe-only.
    assert!(!proc_cap.is_zombie());
}

/// `kill(pid, sig)` with an out-of-range signum returns `-EINVAL`.
#[test]
fn dispatch_kill_invalid_signum_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    // 65 is just past `Signum::MAX = 64`.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 65, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_INVAL));
}

/// `tkill(tid, sig)` aliases to `kill(pid_as_tid, sig)` — the
/// shape returns 0 for self-targeting + valid signum just like
/// the kill arm.
#[test]
fn dispatch_tkill_aliases_to_kill() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [pid, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `tgkill(tgid, tid, sig)` shifts `sig` from args[2] to the
/// kill-arg slot and treats `tgid` as a pid.
#[test]
fn dispatch_tgkill_aliases_to_kill() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TGKILL, [pid, /* tid ignored */ 0, 15, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

// -----------------------------------------------------------------
// getrandom.
// -----------------------------------------------------------------

/// `getrandom(buf, len, 0)` fills the buffer with non-zero bytes
/// and returns `len`.
#[test]
fn dispatch_getrandom_fills_buffer() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 32];
    let buf_uaddr = buf.as_mut_ptr() as u64;
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETRANDOM, [buf_uaddr, 32, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(32));
    // The xorshift default impl produces non-zero output for any
    // non-zero counter seed; assert at least one byte is non-zero.
    assert!(
        buf.iter().any(|b| *b != 0),
        "getrandom must fill the buffer with non-zero entropy"
    );
}

/// `getrandom(buf, 0, _)` returns 0 without writing anything.
#[test]
fn dispatch_getrandom_zero_length_returns_zero() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // Null user-pointer is accepted because buflen == 0 short-
    // circuits before any deref.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETRANDOM, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `getrandom(NULL, len, _)` with non-zero len returns `-EFAULT`.
#[test]
fn dispatch_getrandom_null_buffer_returns_neg_efault() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETRANDOM, [0, 16, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_FAULT));
}

// -----------------------------------------------------------------
// uname.
// -----------------------------------------------------------------

/// `uname(buf)` writes the static utsname. We assert the leading
/// `sysname` field is `"Linux\0"` (musl's runtime probe just reads
/// this and the leading version digits in `release`).
#[test]
fn dispatch_uname_writes_utsname_to_user() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // 6 fields × 65 bytes = 390 bytes.
    let mut buf = [0u8; 6 * 65];
    let buf_uaddr = buf.as_mut_ptr() as u64;
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_UNAME, [buf_uaddr, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    // sysname starts at offset 0; expect "Linux" then NUL pad.
    assert_eq!(&buf[0..5], b"Linux");
    assert_eq!(buf[5], 0, "sysname must be NUL-terminated after \"Linux\"");
    // release starts at offset 130 (2 × 65); expect "6.1.0" prefix.
    assert_eq!(&buf[130..135], b"6.1.0");
    // machine starts at offset 260 (4 × 65); expect "riscv64".
    assert_eq!(&buf[260..267], b"riscv64");
}

/// `uname(NULL)` returns `-EFAULT`.
#[test]
fn dispatch_uname_null_buffer_returns_neg_efault() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_UNAME, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_FAULT));
}

// -----------------------------------------------------------------
// prlimit64.
// -----------------------------------------------------------------

/// `prlimit64(0, RLIMIT_NOFILE, NULL, &old)` returns 0 and writes
/// the static `(1024, 4096)` pair to `old`.
#[test]
fn dispatch_prlimit64_rlimit_nofile_returns_default() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // Two consecutive u64s: rlim_cur (1024) then rlim_max (4096).
    let mut buf = [0u64; 2];
    let buf_uaddr = buf.as_mut_ptr() as u64;
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PRLIMIT64, [0, RLIMIT_NOFILE as u64, 0, buf_uaddr, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(buf[0], 1024, "RLIMIT_NOFILE rlim_cur must default to 1024");
    assert_eq!(buf[1], 4096, "RLIMIT_NOFILE rlim_max must default to 4096");
}

/// `prlimit64(0, RLIMIT_AS, NULL, &old)` returns the
/// `RLIM_INFINITY` pair (no address-space limit enforced).
#[test]
fn dispatch_prlimit64_rlimit_as_returns_infinity() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u64; 2];
    let buf_uaddr = buf.as_mut_ptr() as u64;
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PRLIMIT64, [0, RLIMIT_AS as u64, 0, buf_uaddr, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(buf[0], RLIM_INFINITY);
    assert_eq!(buf[1], RLIM_INFINITY);
}

/// `prlimit64(0, /* unknown */, ...)` returns `-EINVAL`.
#[test]
fn dispatch_prlimit64_invalid_resource_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PRLIMIT64, [0, 999, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_INVAL));
}

/// `prlimit64(other_pid, ...)` returns `-EPERM` (cross-pid not
/// supported on day-1).
#[test]
fn dispatch_prlimit64_cross_pid_returns_neg_eperm() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let other_pid = proc_cap.pid.0 + 100;
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_PRLIMIT64,
            [other_pid as u64, RLIMIT_NOFILE as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_PERM));
}

// -----------------------------------------------------------------
// rt_sigreturn (carryover marker).
// -----------------------------------------------------------------

/// `rt_sigreturn` returns `-ENOSYS` for now. The
/// `SignalFrameIf::restore_signal_frame` surface needs a
/// `TrapFrameMut` the dispatcher doesn't yet expose; carryover
/// is documented at the syscall arm itself
/// (`TODO(phase-signal-frame)`).
#[test]
fn dispatch_rt_sigreturn_returns_neg_enosys() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RT_SIGRETURN, [0; 6]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_NOSYS));
}

// E_BADF is reserved for the F_DUPFD-against-closed-fd shape;
// not currently asserted because the existing
// `dispatch_fcntl_closed_fd_returns_neg_ebadf` already covers the
// shared EBADF gate that all fcntl cmds (including F_DUPFD) hit
// before reaching the cmd switch.
#[cfg_attr(test, allow(dead_code))]
const _UNUSED_E_BADF: i32 = E_BADF;
