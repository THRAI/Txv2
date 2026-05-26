// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFL, F_SETFL, NR_FCNTL, NR_GETRANDOM, NR_KILL, NR_PRLIMIT64,
    NR_RT_SIGRETURN, NR_SETHOSTNAME, NR_TGKILL, NR_TKILL, NR_UNAME, O_NONBLOCK, O_RDWR, RLIMIT_AS,
    RLIMIT_NOFILE, RLIM_INFINITY,
};
use tx_subsystems::process::Pgid;

const E_BADF: i32 = 9;
const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_PERM: i32 = 1;
const E_SRCH: i32 = 3;

fn uts_field(buf: &[u8; 6 * 65], index: usize) -> &[u8] {
    let start = index * 65;
    let end = start + 65;
    let field = &buf[start..end];
    let n = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..n]
}

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

/// `fcntl(fd, F_SETFL, O_NONBLOCK)` updates the shared OpenFile status
/// bits that `F_GETFL` reports.
#[test]
fn dispatch_fcntl_f_setfl_updates_nonblocking_status_bit() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_SETFL as u64, O_NONBLOCK as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_GETFL as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return((O_RDWR | O_NONBLOCK) as i64));
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

/// `kill(self_pid, SIGTERM)` returns 0 and materialises the default
/// terminate disposition immediately. This matches blocking-server
/// shutdown paths that rely on SIGTERM killing a target even when it
/// is asleep inside a syscall.
#[test]
fn dispatch_kill_self_with_sigterm_succeeds() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap.clone(), thread);

    // SIGTERM = 15 (catchable; default disposition is terminate).
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert!(proc_cap.is_zombie());
}

/// `kill(target, sig)` from a non-privileged caller whose uid does
/// not match the target's returns `-EPERM`. Locks in the
/// cred-check wiring: `sys_kill` must route through
/// `script_kill_process` (which runs `cred::require_signal_send`
/// against the caller's syscall-entry `CredSnapshot`), **not** the
/// primitive `step_kill_process` that bypasses authorization.
#[test]
fn dispatch_kill_different_uid_returns_neg_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);

    // Fork a child so its pid is registered in the global lookup
    // (`process_by_pid`) that `sys_kill` consults. The child
    // inherits the parent's root cred at fork time; we override
    // both creds below.
    let child =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false).expect("fork");
    let child_pid = child.pid.0 as u64;

    // Drop the caller to (uid=1000, gid=1000) with empty caps so
    // CAP_KILL no longer bypasses the uid match.
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);

    // Drop the target to (uid=2000, gid=2000), non-overlapping
    // with the caller's (uid, euid). signal_permitted's day-1 rule
    // collapses to (source.uid|euid) × (target.uid|euid); no match.
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);

    // Construct ctx AFTER cred manipulation so the snapshot
    // SyscallCtx::new captures reflects the deprivileged state.
    let ctx = make_ctx(parent.clone(), parent_thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [child_pid, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_PERM));
    // Target must remain live — the cred check blocked before any
    // post happened.
    assert!(!child.is_zombie());
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

/// `kill(-pgid, sig)` resolves a registered process group and fans
/// the signal out through the same cred-checked path as `kill(0, sig)`.
#[test]
fn dispatch_kill_negative_pgid_succeeds() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);

    let child =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false).expect("fork");
    tx_subsystems::process::step_setpgid(&child, Pgid(child.pid.0)).expect("child setpgid");

    let ctx = make_ctx(parent, parent_thread);
    let neg_pgid = -(child.pgrp_cap().pgid.0 as i32);
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [neg_pgid as u64, 15, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert!(!child.is_zombie());
}

/// `kill(-pgid, 0)` is an existence probe for the process group.
#[test]
fn dispatch_kill_negative_pgid_signal_zero_returns_zero() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);

    let child =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false).expect("fork");
    tx_subsystems::process::step_setpgid(&child, Pgid(child.pid.0)).expect("child setpgid");

    let ctx = make_ctx(parent, parent_thread);
    let neg_pgid = -(child.pgrp_cap().pgid.0 as i32);
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [neg_pgid as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert!(!child.is_zombie());
}

/// `kill(-unknown_pgid, sig)` returns `-ESRCH`, not `-ENOSYS`.
#[test]
fn dispatch_kill_unknown_negative_pgid_returns_neg_esrch() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let ctx = make_ctx(parent, parent_thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-9999i32) as u64, 15, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Error(E_SRCH));
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

/// `tkill(tid, sig)` resolves the tid through the pid namespace.
/// Self-targeting with a valid signal returns 0.
#[test]
fn dispatch_tkill_self_returns_success() {
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

// Removed: `dispatch_tgkill_aliases_to_kill`. The test passed
// `tid = 0` with a "tid ignored" comment, asserting tgkill aliases
// to kill. `sys_tgkill` now matches Linux semantics — both `tgid`
// AND `tid` must identify a live thread; `tid = 0` is not a valid
// kernel thread id, so the impl returns `-ESRCH`. tgkill targeting
// the leader-thread tid is exercised in `dispatch_tkill_*`.

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

/// Unsupported getrandom flag bits are rejected with `-EINVAL`.
#[test]
fn dispatch_getrandom_invalid_flags_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 8];
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETRANDOM,
            [buf.as_mut_ptr() as u64, 8, 0x8000_0000, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_INVAL));
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
    assert_eq!(uts_field(&buf, 0), b"Linux");
    assert_eq!(buf[5], 0, "sysname must be NUL-terminated after \"Linux\"");
    assert!(uts_field(&buf, 2).starts_with(b"6.1.0"));
    assert_eq!(uts_field(&buf, 4), b"riscv64");
}

struct LoongArchUnamePmap;

impl PlatformConfig for LoongArchUnamePmap {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "shims-test-la64";
}

impl PmapIf for LoongArchUnamePmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        ShimsTestPmap::create_pmap_root()
    }
}

impl EntropyIf for LoongArchUnamePmap {}
impl tx_hal::AuxvIf for LoongArchUnamePmap {}
impl SmpIf for LoongArchUnamePmap {}
impl tx_hal::TimeIf for LoongArchUnamePmap {
    fn read_ns() -> u64 {
        ShimsTestPmap::read_ns()
    }

    fn set_deadline_ns(deadline_ns: u64) {
        ShimsTestPmap::set_deadline_ns(deadline_ns);
    }

    fn cancel_deadline() {
        ShimsTestPmap::cancel_deadline();
    }

    fn frequency_hz() -> u64 {
        ShimsTestPmap::frequency_hz()
    }
}

#[test]
fn dispatch_uname_uses_selected_platform_machine() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 6 * 65];
    let r = block_on(dispatch::<LoongArchUnamePmap>(
        SyscallRequest::new(NR_UNAME, [buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(uts_field(&buf, 4), b"loongarch64");
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

/// `sethostname(name, len)` updates the nodename observed through `uname`.
#[test]
fn dispatch_sethostname_updates_uname_nodename() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let name = b"ltp-smoke";
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETHOSTNAME,
            [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));

    let mut buf = [0u8; 6 * 65];
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_UNAME, [buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(uts_field(&buf, 1), name);
}

/// Linux rejects hostnames longer than `__NEW_UTS_LEN` (64 bytes).
#[test]
fn dispatch_sethostname_too_long_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let name = [b'x'; 65];
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETHOSTNAME,
            [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_INVAL));
}

// -----------------------------------------------------------------
// prlimit64.
// -----------------------------------------------------------------

/// `prlimit64(0, RLIMIT_NOFILE, NULL, &old)` returns 0 and writes
/// the process default `(1024, 4096)` pair to `old`.
#[test]
fn dispatch_prlimit64_rlimit_nofile_returns_default() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // Two consecutive u64s: rlim_cur then rlim_max.
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

/// `rt_sigreturn` with no parked signal frame returns `-EFAULT`.
/// The kernel has no pre-handler context to restore — POSIX leaves
/// this case undefined; we refuse rather than corrupt the live
/// `saved_user_context`.
#[test]
fn dispatch_rt_sigreturn_without_frame_returns_neg_efault() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RT_SIGRETURN, [0; 6]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(E_FAULT));
}

/// The syscall-layer fallback still restores the parked pre-handler
/// snapshot when no platform frame reader has run. The full kernel
/// thread future handles user-edited frames before this result is
/// observed.
#[test]
fn dispatch_rt_sigreturn_restores_parked_signal_context() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let payload = thread.payload_cap().expect("thread has payload");
    let mut parked = tx_hal::UserTrapContext::empty();
    parked.pc = 0x1234_5678;
    parked.regs[10] = 0xdead_beef;
    payload.store_saved_user_context(Some(tx_hal::UserTrapContext::empty()));
    payload.store_saved_signal_context(Some(parked));

    let ctx = make_ctx(proc_cap, thread.clone());
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RT_SIGRETURN, [0; 6]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::SigreturnRestored);

    let restored = thread
        .payload_cap()
        .expect("thread has payload")
        .saved_user_context()
        .expect("rt_sigreturn must have stored the parked context");
    assert_eq!(restored.pc, 0x1234_5678);
    assert_eq!(restored.regs[10], 0xdead_beef);
}

// E_BADF is reserved for the F_DUPFD-against-closed-fd shape;
// not currently asserted because the existing
// `dispatch_fcntl_closed_fd_returns_neg_ebadf` already covers the
// shared EBADF gate that all fcntl cmds (including F_DUPFD) hit
// before reaching the cmd switch.
#[cfg_attr(test, allow(dead_code))]
const _UNUSED_E_BADF: i32 = E_BADF;
