// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use tx_hal::UserTrapContext;
use tx_subsystems::process::{Pgid, Pid};
use tx_subsystems::reactor_submit;

// -----------------------------------------------------------------------
// Reactor-submission test capture.
//
// The `submit_child_thread` seam is a process-static `AtomicPtr`-backed
// function pointer. Tests install a no-op capture, run `sys_clone`,
// then verify the seam was hit. We can't capture the exact (process,
// thread) caps through a plain `fn` pointer (no captured state), so
// we count invocations and snapshot the most-recent submission via
// a process-static pair of AtomicUsize key holders.
// -----------------------------------------------------------------------

static SUBMIT_CHILD_THREAD_CALLS: AtomicUsize = AtomicUsize::new(0);
static SUBMIT_CHILD_PROC_KEY: AtomicUsize = AtomicUsize::new(0);
static SUBMIT_CHILD_THREAD_KEY: AtomicUsize = AtomicUsize::new(0);

fn capturing_submit(child_process: Cap<ProcessIdentity>, child_thread: Cap<ThreadIdentity>) {
    SUBMIT_CHILD_THREAD_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    SUBMIT_CHILD_PROC_KEY.store(child_process.key().raw() as usize, AtomicOrdering::SeqCst);
    SUBMIT_CHILD_THREAD_KEY.store(child_thread.key().raw() as usize, AtomicOrdering::SeqCst);
}

fn install_capturing_seam_and_reset() {
    SUBMIT_CHILD_THREAD_CALLS.store(0, AtomicOrdering::SeqCst);
    SUBMIT_CHILD_PROC_KEY.store(0, AtomicOrdering::SeqCst);
    SUBMIT_CHILD_THREAD_KEY.store(0, AtomicOrdering::SeqCst);
    reactor_submit::install_submit_child_thread(capturing_submit);
}

/// Synthesise a parent `UserTrapContext` and stamp it onto the
/// calling thread's payload so `sys_clone` finds something to copy.
/// Returns the stamped context for assertion.
fn seed_parent_trap_context(thread: &Cap<ThreadIdentity>) -> UserTrapContext {
    let mut regs = [0usize; 32];
    for (i, slot) in regs.iter_mut().enumerate() {
        *slot = 0x2000 + i;
    }
    // a0 (regs[10]) and a7 (regs[17]) carry the syscall number /
    // first arg before trap entry — they're whatever the parent
    // passed; the seed must zero a0 in the child.
    regs[10] = 0xdead_beef;
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
    parent_ctx
}

// ----------------- Part 2: NR_CLONE ------------------------------

/// `clone(SIGCHLD, 0, 0, 0, 0)` returns the child's pid (a positive
/// number, distinct from the caller's pid).
#[test]
fn dispatch_clone_bare_sigchld_returns_child_pid() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _parent_ctx = seed_parent_trap_context(&thread);

    let parent_pid = proc_cap.pid;
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    let child_pid = match result {
        SyscallResult::Return(v) => v,
        other => panic!("expected Return(child_pid), got {other:?}"),
    };
    assert!(child_pid > 0, "child pid must be positive, got {child_pid}");
    assert_ne!(
        child_pid as u32, parent_pid.0,
        "child must have a different pid from the parent"
    );
    // step_fork registers the child in the parent's children list.
    assert_eq!(
        proc_cap.children().len(),
        1,
        "step_fork must add the child to parent.children"
    );
}

/// flags = `SIGCHLD | CLONE_VM` (0x100) → child pid.  CLONE_VM
/// is accepted (child shares parent's AddressSpace).
#[test]
fn dispatch_clone_with_clone_vm_flag_returns_child_pid() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap, thread);

    const CLONE_VM: u64 = 0x100;
    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD | CLONE_VM, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    // CLONE_VM is now accepted — child shares parent's aspace.
    match result {
        SyscallResult::Return(pid) => assert!(pid > 0),
        other => panic!("expected Return(pid), got {other:?}"),
    }
}

/// flags = `SIGCHLD | CLONE_NEWIPC` creates a child process in a
/// fresh IPC namespace while the rest of the namespace bundle remains
/// shared.
#[test]
fn dispatch_clone_with_clone_newipc_publishes_fresh_ipc_namespace() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let parent_nsproxy = proc_cap.nsproxy_cap().expect("parent nsproxy");
    parent_nsproxy.ipc_ns.limits.lock().mq_maxmsg = 19;
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    const CLONE_NEWIPC: u64 = 0x08000000;
    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD | CLONE_NEWIPC, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let child_pid = match result {
        SyscallResult::Return(pid) => pid,
        other => panic!("expected Return(pid), got {other:?}"),
    };

    let child = tx_subsystems::process::process_by_pid(tx_subsystems::process::structure::Pid(
        child_pid as u32,
    ))
    .expect("child process is registered after clone");
    let child_nsproxy = child.nsproxy_cap().expect("child nsproxy");
    assert_ne!(parent_nsproxy.key().raw(), child_nsproxy.key().raw());
    assert_ne!(
        parent_nsproxy.ipc_ns.key().raw(),
        child_nsproxy.ipc_ns.key().raw()
    );
    assert_eq!(
        parent_nsproxy.pid_ns.key().raw(),
        child_nsproxy.pid_ns.key().raw()
    );
    assert_eq!(child_nsproxy.ipc_ns.limits.lock().mq_maxmsg, 19);
}

/// flags = bare SIGCHLD, stack = `0x4000_0000` → success; the child's
/// sp register is seeded with the supplied stack. Linux semantic:
/// non-zero `newsp` means the libc `__clone` wrapper has staged the
/// child stack (typically with `fn`/`arg` pushed) and the child must
/// enter userspace with `sp = newsp`.
#[test]
fn dispatch_clone_with_nonzero_stack_seeds_child_sp() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap, thread);

    const NEWSP: u64 = 0x4000_0000;
    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, NEWSP, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let child_pid = match result {
        SyscallResult::Return(pid) => {
            assert!(pid > 0);
            pid
        }
        other => panic!("expected Return(pid), got {other:?}"),
    };
    let child = tx_subsystems::process::process_by_pid(tx_subsystems::process::structure::Pid(
        child_pid as u32,
    ))
    .expect("child process is registered after clone");
    let child_leader = child.nth_thread(0).expect("child has leader thread");
    let saved = child_leader
        .payload_cap()
        .expect("child leader has payload")
        .saved_user_context()
        .expect("seed_child_leader_context stored a saved_user_context");
    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(saved.regs[2] as u64, NEWSP, "RV64 sp must equal newsp");
    #[cfg(target_arch = "loongarch64")]
    assert_eq!(saved.regs[3] as u64, NEWSP, "LA64 sp must equal newsp");
}

/// flags = 0 (no SIGCHLD, no CLONE_*) → -EINVAL. Bare-SIGCHLD only.
#[test]
fn dispatch_clone_with_zero_flags_returns_neg_einval() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_CLONE, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(22));
}

/// After a successful `clone(SIGCHLD, 0)`, the child's leader thread
/// has `saved_user_context.regs[10] == 0` and `pc == parent.pc + 4`,
/// matching the RV64 fork-clone ABI shape.
#[test]
fn dispatch_clone_seeds_child_a0_to_zero_and_pc_after_ecall() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let parent_ctx = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(v) if v > 0));

    // Inspect the child's leader thread saved context.
    let children = proc_cap.children();
    assert_eq!(children.len(), 1);
    let child = children[0].clone();
    let child_leader = child.nth_thread(0).expect("child has leader thread");
    let saved = child_leader
        .payload_cap()
        .expect("alive child leader payload")
        .saved_user_context()
        .expect("seed_child_leader_context installed Some");
    assert_eq!(
        saved.regs[10], 0,
        "RV64 a0 (regs[10]) must be 0 in the child — Linux fork-clone ABI"
    );
    assert_eq!(
        saved.pc, parent_ctx.pc,
        "child inherits parent's saved-context PC verbatim; the +4 ecall-skip \
         is applied by the trap shell at user-mode entry, not at clone time \
         (see seed_child_leader_context comment)"
    );
    // Other GPRs preserved.
    for i in 0..32 {
        if i == 10 {
            continue;
        }
        assert_eq!(
            saved.regs[i], parent_ctx.regs[i],
            "regs[{i}] must match parent (only a0 is rewritten)"
        );
    }
    assert_eq!(saved.status, parent_ctx.status, "status preserved");
}

/// `sys_clone` calls through `reactor_submit::submit_child_thread`
/// with the fresh (process, thread) caps. The test installs a
/// capturing seam and verifies the call counter ticks AND the
/// captured cap keys match what `step_fork`'s output would have
/// been (the child cap exposed via `parent.children()` and its
/// leader thread).
#[test]
fn dispatch_clone_submits_child_via_reactor_seam() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(v) if v > 0));

    // Seam was hit exactly once.
    assert_eq!(SUBMIT_CHILD_THREAD_CALLS.load(AtomicOrdering::SeqCst), 1);

    // Captured (process_key, thread_key) match the child + child's
    // leader thread visible from parent.children().
    let children = proc_cap.children();
    assert_eq!(children.len(), 1);
    let child = children[0].clone();
    let child_leader = child.nth_thread(0).expect("child has leader");
    assert_eq!(
        SUBMIT_CHILD_PROC_KEY.load(AtomicOrdering::SeqCst),
        child.key().raw() as usize,
        "captured process cap must be the freshly forked child"
    );
    assert_eq!(
        SUBMIT_CHILD_THREAD_KEY.load(AtomicOrdering::SeqCst),
        child_leader.key().raw() as usize,
        "captured thread cap must be the child's leader thread"
    );
}

// ----------------- Part 4: introspection arms --------------------

/// `getppid()` from init returns `0` (`Pid::RESERVED`) — init has
/// no parent. Real Linux returns init's pid for orphans; the
/// trio's `parent_pid` accessor returns `Pid::RESERVED` (0) in this
/// pre-init bootstrap edge.
#[test]
fn dispatch_getppid_returns_init_parent_pid_zero() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETPPID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// After a successful `clone(SIGCHLD, 0)`, the child's `getppid()`
/// returns the parent's pid.
#[test]
fn dispatch_getppid_returns_real_parent_pid_after_clone() {
    let _setup = setup();
    install_capturing_seam_and_reset();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let parent_pid = proc_cap.pid;
    let ctx_parent = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx_parent));

    let children = proc_cap.children();
    let child = children[0].clone();
    let child_leader = child.nth_thread(0).expect("child has leader");

    let ctx_child = make_ctx(child, child_leader);
    let req = SyscallRequest::new(NR_GETPPID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
    assert_eq!(result, SyscallResult::Return(parent_pid.0 as i64));
}

/// `setpgid(0, 0)` on self creates a fresh process group rooted at
/// the caller's pid. Returns 0; the caller's pgid changes to its pid.
#[test]
fn dispatch_setpgid_self_zero_returns_zero() {
    let _setup = setup();
    // Bootstrap init starts in pgid == pid == 1 already, so we
    // need a non-init process to observe a state change. Fork once
    // first; the child inherits init's pgrp (pgid == 1), then
    // setpgid(0, 0) on the child creates a fresh pgrp at child's pid.
    install_capturing_seam_and_reset();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let children = proc_cap.children();
    let child = children[0].clone();
    let child_leader = child.nth_thread(0).expect("child has leader");
    // Before: child inherits parent's pgid (== 1).
    assert_eq!(child.pgrp_cap().pgid, Pgid(Pid::INIT.0));

    let ctx_child = make_ctx(child.clone(), child_leader);
    // setpgid(0, 0) means "self, use self.pid".
    let req = SyscallRequest::new(NR_SETPGID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        child.pgrp_cap().pgid,
        Pgid(child.pid.0),
        "setpgid(0,0) must create a fresh pgrp rooted at child's pid"
    );
}

/// Cross-process `setpgid(target, 0)` returns `-EPERM` — day-1 only
/// supports self-pid.
#[test]
fn dispatch_setpgid_cross_process_returns_neg_eperm() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // pid = 999 (non-self). EPERM.
    let req = SyscallRequest::new(NR_SETPGID, [999, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(1));
}

/// `getpgid(0)` returns the caller's pgid.
#[test]
fn dispatch_getpgid_self_returns_own_pgid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let expected_pgid = proc_cap.pgrp_cap().pgid.0 as i64;
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETPGID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(expected_pgid));
}

/// `getpgrp()` returns the caller's process-group id.
///
/// Slice 7 of the shell-prompt roadmap (2026-05-07) replaced the
/// previous `-ENOSYS` stub with the real implementation
/// (`process.pgrp_cap().pgid.0`). musl uses `getpgid(0)` directly
/// and never issues this number, but glibc emulates `getpgrp()`
/// as `getpgid(0)` and shipping the real arm removes a startup
/// `-ENOSYS` from any glibc-built binary that lands later.
#[test]
fn dispatch_getpgrp_returns_caller_pgid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let expected_pgid = proc_cap.pgrp_cap().pgid.0 as i64;
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETPGRP, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(expected_pgid));
}

/// `getsid(0)` returns the caller's session id.
#[test]
fn dispatch_getsid_self_returns_own_sid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let expected_sid = proc_cap.pgrp_cap().session_cap().sid.0 as i64;
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETSID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(expected_sid));
}

/// `setsid()` creates a fresh session rooted at the caller's pid.
/// Returns the new sid (== caller's pid).
#[test]
fn dispatch_setsid_returns_new_sid() {
    let _setup = setup();
    // Bootstrap init starts as its own session leader (sid == pid == 1)
    // so we need to fork first to observe a real session change.
    install_capturing_seam_and_reset();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let _ = seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let children = proc_cap.children();
    let child = children[0].clone();
    let child_pid = child.pid.0;
    let child_leader = child.nth_thread(0).expect("child leader");
    let ctx_child = make_ctx(child.clone(), child_leader);

    let req = SyscallRequest::new(NR_SETSID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
    assert_eq!(
        result,
        SyscallResult::Return(child_pid as i64),
        "setsid returns the new session id, == caller's pid"
    );
    assert_eq!(
        child.pgrp_cap().session_cap().sid.0,
        child_pid,
        "child must now lead its own session"
    );
}

// ----------------- Part 5: musl-startup stubs --------------------

/// `set_tid_address(_)` returns the calling thread's tid.
#[test]
fn dispatch_set_tid_address_returns_thread_tid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let expected_tid = thread.tid.0 as i64;
    let ctx = make_ctx(proc_cap, thread);

    // Pass an arbitrary non-zero pointer to verify it's ignored.
    let req = SyscallRequest::new(NR_SET_TID_ADDRESS, [0xdead_beef, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(expected_tid));
}

/// `set_robust_list(_, _)` returns 0 unconditionally (stub).
#[test]
fn dispatch_set_robust_list_returns_zero() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_SET_ROBUST_LIST, [0xdead_beef, 24, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `get_robust_list(0, headp, lenp)` round-trips the current thread's
/// stored robust-list head and length.
#[test]
fn dispatch_get_robust_list_round_trips_current_thread_state() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread.clone());

    let robust_head = [0x1111u64, 0x2222, 0x3333];
    let robust_head_ptr = &robust_head as *const u64 as u64;
    let req = SyscallRequest::new(NR_SET_ROBUST_LIST, [robust_head_ptr, 24, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    let mut out_head: u64 = 0;
    let mut out_len: u64 = 0;
    let out_head_ptr = &mut out_head as *mut u64 as u64;
    let out_len_ptr = &mut out_len as *mut u64 as u64;
    let req = SyscallRequest::new(NR_GET_ROBUST_LIST, [0, out_head_ptr, out_len_ptr, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        out_head, robust_head_ptr,
        "getter should return stored head pointer"
    );
    assert_eq!(
        out_len, 24,
        "getter should return stored robust-list length"
    );
}
