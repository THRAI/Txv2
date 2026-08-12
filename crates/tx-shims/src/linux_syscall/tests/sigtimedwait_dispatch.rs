//! Dispatch tests for `sys_rt_sigtimedwait` (NR=137).
//!
//! libctest's `runtest.c` blocks `SIGCHLD`, forks a child that runs
//! the test body, then calls `sigtimedwait(&{SIGCHLD}, NULL, &10s)`
//! to wait for the child to exit. If the kernel's bit-encoding,
//! pending-snapshot, or set-mask logic is wrong, the parent gets
//! `-EAGAIN` after 10 s and the libctest harness prints `[timed
//! out]` for every test in the suite.
//!
//! These tests run the syscall against a thread whose `pending()`
//! queue is pre-loaded by direct method call — i.e. the unit-test
//! equivalent of `post_sigchld_to_parent` → process-directed kill →
//! catchable-signal posting's final write. If `sys_rt_sigtimedwait` observes
//! and clears that bit correctly here, the kernel-side wait path
//! is correct, and any libctest hang must lie in the child-exit /
//! signal-post path instead of the wait path.

#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::SIGSETSIZE_BYTES;
use tx_subsystems::signal::Signum;

const NR_RT_SIGTIMEDWAIT: u64 = 137;
const NR_RT_SIGSUSPEND: u64 = 133;
const E_AGAIN: i32 = 11;
const E_INTR: i32 = 4;
const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Bit position of `SIGCHLD` (17) in the 64-bit kernel sigset_t:
/// signal `n` → bit `n-1`. SIGCHLD lives at bit 16.
const SIGCHLD_BIT: u64 = 1u64 << (17 - 1);

#[test]
fn sigsuspend_pending_signal_defers_mask_restore_to_ast() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread.clone());
    let payload = thread.payload_cap().expect("thread payload alive");

    payload.pending().post(Signum::SIGTERM);
    assert!(payload.pending().is_pending(Signum::SIGTERM));
    assert_eq!(payload.signal_mask().raw_bits(), 0);

    let suspend_mask = SIGCHLD_BIT;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_RT_SIGSUSPEND,
            [
                &suspend_mask as *const u64 as u64,
                SIGSETSIZE_BYTES,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_INTR));
    assert_eq!(
        payload.signal_mask().raw_bits(),
        SIGCHLD_BIT,
        "the temporary suspend mask must remain active until AST builds the handler frame"
    );
    assert_eq!(
        payload
            .take_sigsuspend_restore_mask()
            .map(|mask| mask.raw_bits()),
        Some(0),
        "AST must receive the caller's pre-suspend mask for rt_sigreturn"
    );
    assert!(
        payload.pending().is_pending(Signum::SIGTERM),
        "sigsuspend observes delivery readiness but leaves actual consumption to AST delivery"
    );
}

/// Happy path: SIGCHLD is pending on the calling thread when
/// `rt_sigtimedwait(&{SIGCHLD}, NULL, &10s)` runs. The call must
/// return `Return(17)` and clear the pending bit. This is the
/// exact shape libctest's `runtest.c` exercises post-child-exit.
#[test]
fn sigtimedwait_returns_pending_signum_and_clears_bit() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread.clone());

    // Pre-load SIGCHLD on the thread's pending queue — same
    // place catchable-signal posting writes to.
    let payload = thread.payload_cap().expect("thread payload alive");
    payload.pending().post(Signum::SIGCHLD);
    assert!(payload.pending().is_pending(Signum::SIGCHLD));

    // Build sigset_t (one u64) and timespec (10 s) on the stack.
    let set: u64 = SIGCHLD_BIT;
    let set_uaddr = &set as *const u64 as u64;
    let timeout = TestTimespec {
        tv_sec: 10,
        tv_nsec: 0,
    };
    let timeout_uaddr = &timeout as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_RT_SIGTIMEDWAIT,
        [set_uaddr, 0, timeout_uaddr, SIGSETSIZE_BYTES, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(
        result,
        SyscallResult::Return(17),
        "sigtimedwait should return SIGCHLD=17 when it's the only pending bit in the set"
    );
    assert!(
        !payload.pending().is_pending(Signum::SIGCHLD),
        "sigtimedwait must clear the consumed bit"
    );
}

/// Sigsetsize != 8 → -EINVAL. The kernel's RV64 ABI uses an
/// 8-byte sigset_t; musl's `sigtimedwait` wrapper passes
/// `_NSIG/8 = 8`. Anything else is an ABI mismatch.
#[test]
fn sigtimedwait_wrong_sigsetsize_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let set: u64 = SIGCHLD_BIT;
    let set_uaddr = &set as *const u64 as u64;
    let timeout = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let timeout_uaddr = &timeout as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_RT_SIGTIMEDWAIT,
        [set_uaddr, 0, timeout_uaddr, 16, 0, 0], // glibc-style size
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// Null `set` pointer → -EFAULT. Linux returns EFAULT when the
/// kernel can't read the mask.
#[test]
fn sigtimedwait_null_set_returns_neg_efault() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_RT_SIGTIMEDWAIT, [0, 0, 0, SIGSETSIZE_BYTES, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// Pending signal NOT in the set is ignored — the call should
/// keep polling (and eventually return -EAGAIN). This guards
/// against a confused-mask bug where the kernel returns a
/// not-asked-for signum.
///
/// We use a tiny timeout so the test bounds quickly: the poll
/// loop sleeps in 5 ms chunks and the `block_on` helper
/// spin-polls, so the test should resolve in well under the
/// 1024-iter cap.
#[test]
fn sigtimedwait_ignores_signals_outside_set_returns_neg_eagain() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread.clone());

    let payload = thread.payload_cap().expect("thread payload alive");
    // Post SIGTERM (15). The set below only asks for SIGCHLD.
    payload.pending().post(Signum::SIGTERM);

    let set: u64 = SIGCHLD_BIT;
    let set_uaddr = &set as *const u64 as u64;
    // 0-second timeout: the loop polls once, checks the deadline,
    // and returns -EAGAIN. We don't want to spin a real sleep
    // here.
    let timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let timeout_uaddr = &timeout as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_RT_SIGTIMEDWAIT,
        [set_uaddr, 0, timeout_uaddr, SIGSETSIZE_BYTES, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_AGAIN));

    // The unrelated SIGTERM bit is still pending — sigtimedwait
    // must not have consumed it.
    assert!(payload.pending().is_pending(Signum::SIGTERM));
}

/// A finite `SIGCHLD` wait must still honor its timeout when no child
/// exits. The libctest `runtest` wrapper relies on this to kill a stuck
/// child test instead of waiting forever on the process exit wait-source.
#[test]
fn sigtimedwait_sigchld_finite_timeout_expires_without_child_exit() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let set: u64 = SIGCHLD_BIT;
    let set_uaddr = &set as *const u64 as u64;
    let timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 1,
    };
    let timeout_uaddr = &timeout as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_RT_SIGTIMEDWAIT,
        [set_uaddr, 0, timeout_uaddr, SIGSETSIZE_BYTES, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_AGAIN));
}

/// End-to-end libctest shape: fork a child, zombify it via
/// explicit no-context group exit (which routes through
/// `post_sigchld_to_parent` → process-directed parent `SIGCHLD` →
/// catchable-signal post → `payload.pending().post(SIGCHLD)`
/// on the parent thread), then dispatch `sigtimedwait`. The
/// signum must be observed and cleared.
///
/// This is the unit-test equivalent of the libctest `runtest.c`
/// pattern — minus the actual fork+execve (the test calls
/// group exit synchronously to fast-forward the child to
/// the post-exit state).
#[test]
fn sigtimedwait_observes_sigchld_posted_by_child_exit() {
    use tx_hal::UserTrapContext;
    use tx_subsystems::process::{step_exit_group_with_posts, ExitStatus};
    use tx_subsystems::reactor_submit::{self, SubmitChildThreadStatus};

    fn noop(_p: Cap<ProcessIdentity>, _t: Cap<ThreadIdentity>) -> SubmitChildThreadStatus {
        SubmitChildThreadStatus::QueuedFallback
    }
    let _setup = setup();
    reactor_submit::install_submit_child_thread(noop);

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    // Seed the parent's saved trap context so `sys_clone` has
    // something to clone from. Mirrors `fork_clone_wait4_wave3`'s
    // `seed_parent_trap_context`.
    let regs = [0usize; 32];
    let parent_ctx = UserTrapContext {
        regs,
        pc: 0x4000_2000,
        status: 0x123,
        fp: tx_hal::UserFpContext::empty(),
    };
    thread
        .payload_cap()
        .expect("alive thread payload")
        .store_saved_user_context(Some(parent_ctx));

    let ctx = make_ctx(proc_cap.clone(), thread.clone());

    // Fork once.
    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    assert_eq!(proc_cap.children().len(), 1);
    let child = proc_cap.children()[0].clone();
    assert!(!child.is_zombie());

    // Zombify the child. This drives the real kernel-side post
    // path: group exit -> `post_sigchld_to_parent` ->
    // process-directed parent `SIGCHLD` → catchable-signal post on
    // the parent's chosen thread.
    step_exit_group_with_posts(
        &child,
        ExitStatus::Exited(0),
        |weak, event| {
            let Some(mailbox) = weak.upgrade() else {
                return;
            };
            let _ = mailbox.post(event);
        },
        |mailbox, event| mailbox.post(event),
    );
    assert!(child.is_zombie());

    // The parent's thread payload's pending queue should now
    // carry SIGCHLD.
    let payload = thread.payload_cap().expect("parent payload alive");
    assert!(
        payload.pending().is_pending(Signum::SIGCHLD),
        "post_sigchld_to_parent must land SIGCHLD on the parent thread's pending queue"
    );

    // Now run `sigtimedwait` and verify it observes + clears the
    // bit.
    let set: u64 = SIGCHLD_BIT;
    let set_uaddr = &set as *const u64 as u64;
    let timeout = TestTimespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let timeout_uaddr = &timeout as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_RT_SIGTIMEDWAIT,
        [set_uaddr, 0, timeout_uaddr, SIGSETSIZE_BYTES, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(
        result,
        SyscallResult::Return(17),
        "end-to-end fork → exit_group → sigtimedwait must return SIGCHLD"
    );
    assert!(
        !payload.pending().is_pending(Signum::SIGCHLD),
        "the consumed bit must be cleared"
    );
}
