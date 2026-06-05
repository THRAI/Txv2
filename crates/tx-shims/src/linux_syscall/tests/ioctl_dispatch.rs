// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use tx_subsystems::pipe::{step_pipe2, PipeFlags};
use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::tty::structure::termios::{ICANON, VMIN};
use tx_subsystems::tty::structure::{Termios, Winsize};
use tx_subsystems::vfs::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking, StructPayload,
};

use crate::linux_syscall::{
    NR_IOCTL, TCGETS, TCSETS, TIOCGPGRP, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP, TIOCSWINSZ,
};

const E_BADF: i32 = 9;
const E_FAULT: i32 = 14;
const E_INVAL: i32 = 22;
const E_NOTTY: i32 = 25;
const RTC_RD_TIME: u32 = 0x8024_7009;

fn ioctl_setup() -> TestSetup {
    setup()
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for ioctl tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

/// `ioctl(tty_fd, TCGETS, &out)` returns 0 and writes a Termios
/// struct into the caller's buffer. The boot console TTY is in
/// cooked mode (`Termios::default_cooked`); we observe non-zero
/// `c_lflag` (ICANON | ECHO | ...) as a signal that real termios
/// state landed in the buffer.
#[test]
fn dispatch_ioctl_tcgets_on_tty_fd_writes_termios() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let mut out = Termios::zeroed();
    let argp = &mut out as *mut Termios as u64;
    let req = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, argp, 0, 0, 0]);

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_ne!(
        out.c_lflag, 0,
        "TCGETS should write the cooked-mode termios; c_lflag has ICANON|ECHO|... set"
    );
}

/// `TCGETS` copies Linux's kernel `struct termios` prefix, not musl's
/// larger public buffer. musl passes its `struct termios *` directly
/// to the ioctl and then reads these prefix fields.
#[test]
fn dispatch_ioctl_tcgets_writes_linux_kernel_termios_layout() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let mut out = [0xa5u8; 64];
    let req = SyscallRequest::new(
        NR_IOCTL,
        [0, TCGETS as u64, out.as_mut_ptr() as u64, 0, 0, 0],
    );

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out[16], 0, "c_line is the byte at offset 16");
    assert_eq!(out[17 + VMIN], 1, "c_cc[VMIN] starts at offset 17");
    assert_ne!(
        u32::from_le_bytes(out[12..16].try_into().unwrap()) & ICANON,
        0,
        "c_lflag lives at offset 12 and carries ICANON in cooked mode",
    );
    assert_eq!(
        &out[36..],
        &[0xa5u8; 28],
        "TCGETS must only copy the 36-byte kernel termios image",
    );
}

/// `ioctl(pipe_fd, TCGETS, &out)` returns `-ENOTTY` — terminal
/// ioctls on non-TTY fds are the canonical ENOTTY case per
/// `man ioctl_tty`.
#[test]
fn dispatch_ioctl_tcgets_on_pipe_fd_returns_neg_enotty() {
    let _setup = ioctl_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let (reader_cap, _writer_cap) =
        step_pipe2(PipeFlags::default()).expect("pipe2 for ioctl-enotty test");
    proc_cap.set_fd(11, Some(reader_cap));
    let ctx = make_ctx(proc_cap, thread);

    let mut out = Termios::zeroed();
    let argp = &mut out as *mut Termios as u64;
    let req = SyscallRequest::new(NR_IOCTL, [11, TCGETS as u64, argp, 0, 0, 0]);

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTTY));
}

/// `ioctl(unknown_fd, TCGETS, &out)` returns `-EBADF` — the fd
/// resolution short-circuits before the request decode.
#[test]
fn dispatch_ioctl_tcgets_on_unknown_fd_returns_neg_ebadf() {
    let _setup = ioctl_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let mut out = Termios::zeroed();
    let argp = &mut out as *mut Termios as u64;
    let req = SyscallRequest::new(NR_IOCTL, [42, TCGETS as u64, argp, 0, 0, 0]);

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `ioctl(tty_fd, TCSETS, &new)` returns 0 and the next TCGETS
/// reads back the same termios value. We change `c_lflag` to a
/// distinct bit pattern to prove the new value lands.
#[test]
fn dispatch_ioctl_tcsets_on_tty_fd_updates_termios() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let mut new = Termios::zeroed();
    new.c_lflag = 0xdead_beef;
    let argp_in = &new as *const Termios as u64;
    let req_set = SyscallRequest::new(NR_IOCTL, [0, TCSETS as u64, argp_in, 0, 0, 0]);
    let result_set = block_on(dispatch::<ShimsTestPmap>(req_set, &ctx));
    assert_eq!(result_set, SyscallResult::Return(0));

    let mut readback = Termios::zeroed();
    let argp_out = &mut readback as *mut Termios as u64;
    let req_get = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, argp_out, 0, 0, 0]);
    let result_get = block_on(dispatch::<ShimsTestPmap>(req_get, &ctx));
    assert_eq!(result_get, SyscallResult::Return(0));
    assert_eq!(
        readback.c_lflag, 0xdead_beef,
        "TCGETS after TCSETS should observe the value installed by TCSETS"
    );
}

/// `ioctl(tty_fd, TIOCGPGRP, &out)` on a TTY without a bound
/// foreground pgrp returns `-EINVAL` — the underlying step
/// requires `tty.session_pgrp().is_some()` and surfaces EINVAL
/// when the slot is empty.
#[test]
fn dispatch_ioctl_tiocgpgrp_on_unbound_tty_returns_neg_einval() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let mut out: u32 = 0;
    let argp = &mut out as *mut u32 as u64;
    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCGPGRP as u64, argp, 0, 0, 0]);

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `ioctl(tty_fd, TIOCGWINSZ, &out)` returns 0 and writes a
/// `Winsize` struct into the caller's buffer.
#[test]
fn dispatch_ioctl_tiocgwinsz_on_tty_writes_winsize() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    // Pre-set a known winsize so the readback assertion is stable.
    let preset = Winsize::new(24, 80);
    let argp_set = &preset as *const Winsize as u64;
    let req_set = SyscallRequest::new(NR_IOCTL, [0, TIOCSWINSZ as u64, argp_set, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req_set, &ctx)),
        SyscallResult::Return(0)
    );

    let mut out = Winsize::default();
    let argp = &mut out as *mut Winsize as u64;
    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCGWINSZ as u64, argp, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out, preset);
}

#[test]
fn dispatch_ioctl_rtc_rd_time_on_rtc_char_device_writes_rtc_time() {
    let _setup = ioctl_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let rnode = RNode::new_cap(
        FsObjectId::new(0x6465_7805),
        InodeMeta::new(InodeKind::CharDevice, 0o020644),
        RNodeBacking::StructBacked {
            payload: StructPayload::CharDevice(&tx_fs::devfs::RTC_CHAR_BINDING),
        },
    )
    .expect("rtc rnode");
    let file = OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("rtc open file");
    proc_cap.set_fd(3, Some(file));
    let ctx = make_ctx(proc_cap, thread);

    let mut out = [0xa5u8; 36];
    let req = SyscallRequest::new(
        NR_IOCTL,
        [3, RTC_RD_TIME as u64, out.as_mut_ptr() as u64, 0, 0, 0],
    );

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(i32::from_le_bytes(out[12..16].try_into().unwrap()), 23);
    assert_eq!(i32::from_le_bytes(out[16..20].try_into().unwrap()), 4);
    assert_eq!(i32::from_le_bytes(out[20..24].try_into().unwrap()), 126);
}

/// `ioctl(tty_fd, TIOCSWINSZ, &new)` returns 0 and the next
/// TIOCGWINSZ reads back the same struct.
#[test]
fn dispatch_ioctl_tiocswinsz_on_tty_updates_winsize() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let new = Winsize::new(50, 120);
    let argp_in = &new as *const Winsize as u64;
    let req_set = SyscallRequest::new(NR_IOCTL, [0, TIOCSWINSZ as u64, argp_in, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req_set, &ctx)),
        SyscallResult::Return(0)
    );

    let mut readback = Winsize::default();
    let argp_out = &mut readback as *mut Winsize as u64;
    let req_get = SyscallRequest::new(NR_IOCTL, [0, TIOCGWINSZ as u64, argp_out, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req_get, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(readback, new);
}

/// `ioctl(tty_fd, TIOCSCTTY, 0)` succeeds when the caller is a
/// session leader without an existing controlling TTY. The
/// bootstrap init process satisfies both predicates: its session
/// leader's pid equals its sid, and `bootstrap_init_process`
/// constructs the session with no controlling TTY bound.
#[test]
fn dispatch_ioctl_tiocsctty_on_session_leader_succeeds() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `ioctl(tty_fd, TIOCSCTTY, 0)` returns `-EINVAL` when the same
/// caller issues a second TIOCSCTTY. `step_ioctl_tiocsctty_for_process`
/// sets `session.controlling_tty` on the first successful call, so
/// `has_controlling_tty()` is true on re-entry and `require_session_leader`
/// rejects with EINVAL before the EBUSY guard is reached.
#[test]
fn dispatch_ioctl_tiocsctty_on_already_bound_tty_returns_neg_ebusy() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
    let first = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(first, SyscallResult::Return(0));

    let req2 = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
    let second = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
    // The process-aware path sets controlling_tty on success, so
    // has_controlling_tty() is true on the second call and
    // require_session_leader returns EINVAL before the EBUSY guard.
    assert_eq!(second, SyscallResult::Error(E_INVAL));
}

/// `ioctl(tty_fd, 0xDEADBEEF, 0)` returns `-ENOTTY` — unknown
/// terminal-shape requests fall through to the default arm per
/// `man ioctl_tty`.
#[test]
fn dispatch_ioctl_unknown_request_returns_neg_enotty() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_IOCTL, [0, 0xDEAD_BEEF, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTTY));
}

/// `ioctl(tty_fd, TCGETS, NULL)` returns `-EFAULT` before any
/// step is invoked. Mirrors the time-syscall arms' null-uaddr
/// short-circuit — the user-VA sweep (Slice 9) lifts this to a
/// real EFAULT-on-invalid-VA contract via `copy_to_user`.
#[test]
fn dispatch_ioctl_null_argp_for_tcgets_returns_neg_efault() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `ioctl(tty_fd, TIOCSPGRP, &val)` against an unbound TTY
/// returns `-EINVAL` (the same path TIOCGPGRP exercises) — the
/// step rejects when no session is bound.
#[test]
fn dispatch_ioctl_tiocspgrp_on_unbound_tty_returns_neg_einval() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let new_pgrp: u32 = 7;
    let argp = &new_pgrp as *const u32 as u64;
    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSPGRP as u64, argp, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `ioctl(tty_fd, TIOCNOTTY, 0)` against a TTY whose session is
/// not bound to the caller returns `-EINVAL` (the step rejects
/// when there's no binding to detach). This exercises the
/// `TIOCNOTTY` arm dispatch wiring.
#[test]
fn dispatch_ioctl_tiocnotty_on_unbound_tty_returns_neg_einval() {
    let _setup = ioctl_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_IOCTL, [0, TIOCNOTTY as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}
