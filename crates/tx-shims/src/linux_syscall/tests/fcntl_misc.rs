// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use alloc::sync::{Arc, Weak as ArcWeak};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::linux_syscall::{
    AF_INET, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFL, F_SETFL, NR_CLOSE, NR_FCNTL, NR_GETRANDOM, NR_KILL,
    NR_PIDFD_GETFD, NR_PIDFD_OPEN, NR_PIDFD_SEND_SIGNAL, NR_PRLIMIT64, NR_READ, NR_RT_SIGRETURN,
    NR_SOCKET, NR_TGKILL, NR_TKILL, NR_UNAME, O_NONBLOCK, O_RDWR, RLIMIT_AS, RLIMIT_NOFILE,
    RLIM_INFINITY,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_subsystems::signal::{SigDisposition, Signum};

const E_BADF: i32 = 9;
const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_PERM: i32 = 1;
const E_SRCH: i32 = 3;
const E_AGAIN: i32 = 11;
const SOCK_DGRAM: u64 = 2;

static SYSCALL_SIGNAL_POST_COUNT: AtomicUsize = AtomicUsize::new(0);
static FCNTL_SOCKET_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

fn counting_mailbox_post(mailbox: ArcWeak<TaskMailbox>, event: MailboxEvent) {
    SYSCALL_SIGNAL_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    if let Some(mailbox) = mailbox.upgrade() {
        let _ = mailbox.post(event);
    }
}

fn counting_fcntl_socket_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    FCNTL_SOCKET_REF_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    mailbox.post(event)
}

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

/// `pidfd_open` installs a pidfd-backed OpenFile and preserves the
/// Linux pidfd flag surface.
#[test]
fn dispatch_pidfd_open_installs_pidfd_backing_for_self() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [pid, O_NONBLOCK as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    let fd = match r {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("expected pidfd fd, got {other:?}"),
    };
    let file = proc_cap.fd(fd).expect("pidfd installed");
    assert!(file.pidfd_process().is_some());
    assert!(file.flags().cloexec);
    assert!(file.flags().nonblocking);

    let bad = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [pid, 0x8000_0000, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(bad, SyscallResult::Error(E_INVAL));
}

/// `pidfd_send_signal(pidfd, 0, NULL, 0)` probes a valid pidfd target
/// and returns success without posting a signal.
#[test]
fn dispatch_pidfd_send_signal_zero_probes_pidfd_target() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [pid, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected pidfd fd, got {other:?}"),
    };

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_SEND_SIGNAL, [fd, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `pidfd_getfd` must account the duplicated pipe endpoint as a
/// second fd reference. Closing the target's original writer must
/// not publish EOF while the caller's duplicated writer is open.
#[test]
fn dispatch_pidfd_getfd_pipe_writer_keeps_pipe_alive_until_dup_closes() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false).expect("fork");
    let (reader, writer) =
        tx_subsystems::pipe::step_pipe2(tx_subsystems::pipe::PipeFlags::default()).expect("pipe2");
    parent.set_fd(3, Some(reader));
    child.set_fd(4, Some(writer));

    let ctx = make_ctx(parent.clone(), parent_thread);
    let pidfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [child.pid.0 as u64, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("pidfd_open child: {other:?}"),
    };
    let dupfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_GETFD, [pidfd as u64, 4, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("pidfd_getfd child writer: {other:?}"),
    };

    child.set_fd(4, None);
    let mut read_buf = [0u8; 1];
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            3,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Error(E_AGAIN),
        "the duplicated writer fd should keep the empty pipe non-EOF"
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_CLOSE, [dupfd as u64, 0, 0, 0, 0, 0]),
            &ctx
        )),
        SyscallResult::Return(0)
    );
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            3,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Return(0),
        "closing the duplicated writer should publish EOF"
    );
}

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

// Removed: `dispatch_fcntl_f_setfl_returns_neg_enosys`.
// `F_SETFL` is now wired (the OpenFile-flags interior-mutability hook
// landed in a later slice); the `-ENOSYS` expectation is stale. The
// success path is exercised by `dispatch_fcntl_f_setfl_*` tests
// elsewhere in this file when present, and at the
// `OpenFile::set_runtime_nonblocking` unit-test level.

#[test]
fn dispatch_fcntl_setfl_socket_uses_syscall_ctx_mailbox_ref_post_for_send_space() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx =
        make_ctx(proc_cap.clone(), thread).with_mailbox_ref_post(counting_fcntl_socket_ref_post);
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SOCKET, [AF_INET as u64, SOCK_DGRAM, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("socket should return fd, got {other:?}"),
    };
    let file = proc_cap.fd(fd).expect("socket fd installed");
    let identity = file.socket_identity().expect("socket backing").clone();
    identity
        .readiness
        .clear_send(tx_subsystems::net::SendWireSet::SPACE);
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _sub = identity.readiness.send_wq.subscribe(
        tx_subsystems::net::SendWireSet::SPACE.bits(),
        Arc::downgrade(&mailbox),
        generation,
    );
    FCNTL_SOCKET_REF_POST_COUNT.store(0, Ordering::SeqCst);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FCNTL,
            [fd as u64, F_SETFL as u64, O_NONBLOCK as u64, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(FCNTL_SOCKET_REF_POST_COUNT.load(Ordering::SeqCst), 1);
}

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

    // SIGTERM = 15 (catchable; routes through catchable-signal posting, no
    // zombification side-effect on the calling thread).
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 15, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `kill(target, sig)` from a non-privileged caller whose uid does
/// not match the target's returns `-EPERM`. Locks in the
/// cred-check wiring: `sys_kill` must route through
/// `script_kill_process` (which runs `cred::require_signal_send`
/// against the caller's syscall-entry `CredSnapshot`), **not** the
/// primitive process-directed kill helper that bypasses authorization.
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

#[test]
fn dispatch_signal_zero_probe_kill_self_returns_zero_without_delivery() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox_post(counting_mailbox_post);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [pid, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
    assert!(!thread
        .payload_cap()
        .expect("thread payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_unknown_pid_returns_esrch() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap, thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [9999, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_current_pgrp_returns_zero_without_delivery() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(process, thread.clone()).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!thread
        .payload_cap()
        .expect("thread payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_different_uid_returns_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [child.pid.0 as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_negative_pgrp_all_denied_returns_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    tx_subsystems::process::step_setpgid(&child, tx_subsystems::process::Pgid(child.pid.0))
        .expect("create child process group");

    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    let child_thread = child.nth_thread(0).expect("child leader");
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);
    let negative_pgid = (-(child.pid.0 as i64)) as u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [negative_pgid, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_negative_pgrp_permitted_without_delivery() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    tx_subsystems::process::step_setpgid(&child, tx_subsystems::process::Pgid(child.pid.0))
        .expect("create child process group");
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-(child.pid.0 as i64)) as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_negative_pgrp_no_target_returns_esrch() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-9999i64) as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_kill_negative_pgrp_aggregates_no_target_as_esrch() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let ctx = make_ctx(parent, parent_thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-9999i64) as u64, 15, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
}

#[test]
fn dispatch_kill_negative_pgrp_aggregates_all_denied_as_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    tx_subsystems::process::step_setpgid(&child, tx_subsystems::process::Pgid(child.pid.0))
        .expect("create child process group");
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    let ctx = make_ctx(parent, parent_thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-(child.pid.0 as i64)) as u64, 15, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child.is_zombie());
}

#[test]
fn dispatch_kill_negative_pgrp_aggregates_all_retry_as_eagain() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    tx_subsystems::process::step_setpgid(&child, tx_subsystems::process::Pgid(child.pid.0))
        .expect("create child process group");
    let exec_prep = tx_subsystems::process::ProcessExecPrep::begin(&child, &child_thread)
        .expect("reserve child exec lifecycle");
    let ctx = make_ctx(parent, parent_thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_KILL,
            [
                (-(child.pid.0 as i64)) as u64,
                Signum::SIGKILL.raw() as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_AGAIN));
    assert!(!child.is_zombie());
    drop(exec_prep);
}

#[test]
fn dispatch_kill_broadcast_aggregates_partial_success_as_success() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let retrying = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork retrying child");
    let delivered = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork delivered child");
    let retrying_thread = retrying.nth_thread(0).expect("retrying child leader");
    let exec_prep = tx_subsystems::process::ProcessExecPrep::begin(&retrying, &retrying_thread)
        .expect("reserve child exec lifecycle");
    let ctx = make_ctx(parent, parent_thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_KILL,
            [(-1i64) as u64, Signum::SIGKILL.raw() as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!retrying.is_zombie());
    assert!(delivered.is_zombie());
    drop(exec_prep);
}

#[test]
fn dispatch_signal_zero_probe_kill_broadcast_permitted_without_delivery() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-1i64) as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_broadcast_all_denied_returns_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-1i64) as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_kill_broadcast_no_target_returns_esrch() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_KILL, [(-1i64) as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
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

#[test]
fn dispatch_signal_zero_probe_tkill_different_uid_returns_eperm() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    tx_subsystems::process::numbers::register_tid(child_thread.tid, child_thread.clone());
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [child_thread.tid.0 as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child_thread
        .payload_cap()
        .expect("child payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_tkill_sigcancel_different_uid_returns_eperm_without_exit() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader");
    tx_subsystems::process::numbers::register_tid(child_thread.tid, child_thread.clone());
    clear_caps_for_test(&parent);
    set_cred_ids_for_test(&parent, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&child);
    set_cred_ids_for_test(&child, 2000, 2000, 2000, 2000, 2000, 2000);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(parent, parent_thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [child_thread.tid.0 as u64, 33, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_PERM));
    assert!(!child.is_zombie());
    assert!(!child_thread.is_zombie());
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_tkill_sigcancel_unknown_tid_returns_esrch() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [u32::MAX as u64, 33, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
}

#[test]
fn dispatch_signal_zero_probe_tkill_self_returns_zero_without_delivery() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let tid = thread.tid.0 as u64;
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(process, thread.clone()).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [tid, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!thread
        .payload_cap()
        .expect("thread payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_tkill_unknown_tid_returns_esrch() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(process, thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [u32::MAX as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_tkill_uses_syscall_ctx_mailbox_post() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let mailbox = Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::SIGTERM,
        SigDisposition::Handler(0xCAFE),
    );
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread).with_mailbox_post(counting_mailbox_post);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TKILL, [pid, Signum::SIGTERM.raw() as u64, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(
        SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "tkill should publish through SyscallCtx mailbox post"
    );
}

#[test]
fn dispatch_tgkill_uses_syscall_ctx_mailbox_post() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let mailbox = Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::SIGTERM,
        SigDisposition::Handler(0xCAFE),
    );
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let tgid = proc_cap.pid.0 as u64;
    let tid = thread.tid.0 as u64;
    let ctx = make_ctx(proc_cap, thread).with_mailbox_post(counting_mailbox_post);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TGKILL,
            [tgid, tid, Signum::SIGTERM.raw() as u64, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(
        SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "tgkill should publish through SyscallCtx mailbox post"
    );
}

#[test]
fn dispatch_signal_zero_probe_tgkill_unknown_tid_returns_esrch() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let tgid = process.pid.0 as u64;
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(process, thread).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TGKILL, [tgid, u32::MAX as u64, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_signal_zero_probe_tgkill_self_returns_zero_without_delivery() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let tgid = process.pid.0 as u64;
    let tid = thread.tid.0 as u64;
    SYSCALL_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(process, thread.clone()).with_mailbox_post(counting_mailbox_post);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TGKILL, [tgid, tid, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(!thread
        .payload_cap()
        .expect("thread payload")
        .pending()
        .is_pending(Signum::SIGTERM));
    assert_eq!(SYSCALL_SIGNAL_POST_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_tgkill_sigcancel_foreign_tgid_returns_esrch() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let foreign_tgid = process.pid.0 as u64 + 1;
    let tid = thread.tid.0 as u64;
    let ctx = make_ctx(process, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TGKILL, [foreign_tgid, tid, 33, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
}

#[test]
fn dispatch_tgkill_sigcancel_unknown_tid_returns_esrch() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let tgid = process.pid.0 as u64;
    let ctx = make_ctx(process, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TGKILL, [tgid, u32::MAX as u64, 33, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_SRCH));
}

#[test]
fn dispatch_tgkill_sigkill_returns_eagain_while_exec_owns_lifecycle() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let exec_prep = tx_subsystems::process::ProcessExecPrep::begin(&proc_cap, &thread)
        .expect("reserve exec lifecycle");
    let tgid = proc_cap.pid.0 as u64;
    let tid = thread.tid.0 as u64;
    let ctx = make_ctx(proc_cap.clone(), thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TGKILL,
            [tgid, tid, Signum::SIGKILL.raw() as u64, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_AGAIN));
    assert!(!proc_cap.is_zombie());
    drop(exec_prep);
}

#[test]
fn signal_delivery_result_maps_lifecycle_permission_and_liveness() {
    use tx_subsystems::execution::Errno;
    use tx_subsystems::signal::KillOutcome;

    assert_eq!(
        crate::linux_syscall::signal::signal_delivery_result(Ok(KillOutcome::Delivered)),
        SyscallResult::Return(0),
    );
    assert_eq!(
        crate::linux_syscall::signal::signal_delivery_result(Ok(KillOutcome::Retry)),
        SyscallResult::Error(E_AGAIN),
    );
    assert_eq!(
        crate::linux_syscall::signal::signal_delivery_result(Ok(KillOutcome::NoLiveThread)),
        SyscallResult::Error(E_SRCH),
    );
    assert_eq!(
        crate::linux_syscall::signal::signal_delivery_result(Err(Errno::EPERM)),
        SyscallResult::Error(E_PERM),
    );
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
impl tx_hal::CacheIf for LoongArchUnamePmap {}
impl tx_hal::ConsoleIf for LoongArchUnamePmap {
    fn write_bytes(_bytes: &[u8]) {}
}
impl SmpIf for LoongArchUnamePmap {}
impl tx_hal::MonotonicCounterIf for LoongArchUnamePmap {
    fn read_ns() -> u64 {
        ShimsTestPmap::read_ns()
    }

    fn frequency_hz() -> u64 {
        ShimsTestPmap::frequency_hz()
    }
}

impl tx_hal::DeadlineTimerIf for LoongArchUnamePmap {
    fn set_deadline_ns(deadline_ns: u64) {
        ShimsTestPmap::set_deadline_ns(deadline_ns);
    }

    fn cancel_deadline() {
        ShimsTestPmap::cancel_deadline();
    }
}

impl tx_hal::PersistentClockIf for LoongArchUnamePmap {}

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
// rt_sigreturn.
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
    assert_eq!(r, SyscallResult::Error(14)); // EFAULT
}

/// `rt_sigreturn` is valid only after the architecture layer has decoded the
/// userspace signal frame.  There is deliberately no kernel-side shadow
/// context fallback: such a fallback would discard handler edits to
/// `ucontext_t` (notably musl pthread cancellation).
#[test]
fn dispatch_rt_sigreturn_without_decoded_userspace_frame_returns_neg_efault() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    let ctx = make_ctx(proc_cap, thread.clone());
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RT_SIGRETURN, [0; 6]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(14)); // EFAULT
}

// E_BADF is reserved for the F_DUPFD-against-closed-fd shape;
// not currently asserted because the existing
// `dispatch_fcntl_closed_fd_returns_neg_ebadf` already covers the
// shared EBADF gate that all fcntl cmds (including F_DUPFD) hit
// before reaching the cmd switch.
#[cfg_attr(test, allow(dead_code))]
const _UNUSED_E_BADF: i32 = E_BADF;
