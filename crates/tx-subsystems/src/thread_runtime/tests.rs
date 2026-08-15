//! Thread-runtime topology tests.
//!
//! Focus on the thread-side half of the identity/payload split and the
//! parent-bookkeeping that `step_thread_exit` performs.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Wake;
use std::vec::Vec;

use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_fork, ExitStatus};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::adapter::reactor_entry::{SyscallRequest, UserspaceTrapInfo};
use crate::thread_runtime::adapter::step_engine::{sign, Cap, PayloadCap, ZoneAllocated};
use crate::thread_runtime::execution::prepare_userspace_entry_payload;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::{
    clear_current_thread_identity, clear_current_thread_payload, clear_current_userspace_payload,
    clear_current_userspace_thread_identity, current_thread_identity, current_thread_payload,
    current_thread_payload_mask, current_userspace_payload, current_userspace_payload_mask,
    current_userspace_thread_identity, drain_pending_syscall_return, prewarm_thread_payload_slots,
    reset_tid_counter_for_test, set_current_thread_identity, set_current_thread_payload,
    set_current_userspace_payload, set_current_userspace_thread_identity, ThreadIdentity,
    ThreadPayload,
};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;
use tx_hal::UserTrapContext;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
}

fn bootstrap() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

fn finish_process_group_for_test(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    crate::process::step_exit_group_with_posts(
        process,
        status,
        |weak, event| {
            let Some(mailbox) = weak.upgrade() else {
                return;
            };
            let _ = mailbox.post(event);
        },
        |mailbox, event| mailbox.post(event),
    );
}

#[test]
fn thread_runtime_lock_service_declares_sigprocmask_phase_names() {
    let names = crate::thread_runtime::execution::THREAD_RUNTIME_LOCK_SERVICE_TRACE_NAMES;

    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns".as_slice()
    ));
    assert!(names
        .contains(&b"debug.lock_service.thread.payload.sigprocmask.payload_missing".as_slice()));
    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns".as_slice()
    ));
    assert!(names.contains(&b"debug.lock_service.thread.payload.sigprocmask.mask_noop".as_slice()));
    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns".as_slice()
    ));
}

fn source_function_body<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(name).expect("function name present");
    let open = src[start..]
        .find('{')
        .map(|idx| start + idx)
        .expect("function body opens");
    let mut depth = 0usize;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("function body closes");
}

#[test]
fn thread_exit_repairs_userspace_before_zombie_and_process_teardown() {
    let source = include_str!("execution.rs");
    let clear_child_tid = source_function_body(source, "fn step_clear_and_wake_child_tid(");
    assert!(
        !clear_child_tid.contains("spin_loop"),
        "clear_child_tid must return wait progress to the reactor"
    );
    let step_body = source_function_body(source, "fn step_exit_with_cleanup<F>");
    let cleanup = step_body
        .find("cleanup_step(aspace")
        .expect("userspace-visible thread cleanup step present");
    let commit = step_body
        .find("finish_prepared_thread_exit_with_posts(")
        .expect("thread exit commit present");
    assert!(cleanup < commit, "userspace cleanup precedes exit commit");

    let finish_body =
        source_function_body(source, "fn finish_prepared_thread_exit_with_posts<Z, F, G>");
    let zombify = finish_body
        .find("set_thread_zombie(thread")
        .expect("thread zombify present");
    let process_exit = finish_body
        .find("step_process_exit_with_posts(")
        .expect("last-thread process teardown present");
    assert!(
        zombify < process_exit,
        "thread zombify precedes last-thread process teardown"
    );
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let threads = payload.threads.snapshot();
    threads[0].clone()
}

#[test]
fn prewarm_thread_payload_slots_keeps_next_batch_off_slab_allocator() {
    let _g = setup();

    let zone = <ThreadPayload as ZoneAllocated>::zone();
    let before = zone.allocated_slots();
    let warmed = prewarm_thread_payload_slots(33);
    assert_eq!(warmed, 33);
    let after_prewarm = zone.allocated_slots();
    assert!(after_prewarm >= before);

    let mut caps = Vec::new();
    for _ in 0..33 {
        caps.push(sign(ThreadPayload::fresh()).expect("payload sign"));
    }
    assert_eq!(
        zone.allocated_slots(),
        after_prewarm,
        "prewarmed payload slots should satisfy the next pthread-sized batch"
    );
}

#[test]
fn thread_exit_sets_status_and_drops_thread_payload() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    assert!(!leader.is_zombie());
    step_thread_exit(leader.clone(), 3);

    assert!(leader.is_zombie());
    assert_eq!(leader.exit_status(), Some(3));
}

#[test]
fn thread_exit_clears_matching_current_and_userspace_slots() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let leader_payload = leader.payload_cap_for_test().expect("leader alive");
    let other_proc = step_fork::<TestPmap>(&proc_cap, false, false).expect("fork");
    let other_thread = first_thread(&other_proc);
    let other_payload = other_thread.payload_cap_for_test().expect("other alive");

    let _ = set_current_thread_identity(0, leader.clone());
    let _ = set_current_thread_payload(0, leader_payload.clone());
    let _ = set_current_userspace_thread_identity(1, leader.clone());
    let _ = set_current_userspace_payload(1, leader_payload);
    let _ = set_current_thread_identity(2, other_thread.clone());
    let _ = set_current_thread_payload(2, other_payload.clone());
    let _ = set_current_userspace_thread_identity(3, other_thread.clone());
    let _ = set_current_userspace_payload(3, other_payload);

    assert_eq!(current_thread_payload_mask() & 0b0101, 0b0101);
    assert_eq!(current_userspace_payload_mask() & 0b1010, 0b1010);

    step_thread_exit(leader, 7);

    assert!(current_thread_payload(0).is_none());
    assert!(current_thread_identity(0).is_none());
    assert!(current_userspace_payload(1).is_none());
    assert!(current_userspace_thread_identity(1).is_none());
    assert!(current_thread_payload(2).is_some());
    assert!(current_thread_identity(2).is_some());
    assert!(current_userspace_payload(3).is_some());
    assert!(current_userspace_thread_identity(3).is_some());

    let _ = clear_current_thread_payload(2);
    let _ = clear_current_thread_identity(2);
    let _ = clear_current_userspace_payload(3);
    let _ = clear_current_userspace_thread_identity(3);
}

#[test]
fn exit_group_clears_matching_current_and_userspace_slots() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let leader_payload = leader.payload_cap_for_test().expect("leader alive");

    let _ = set_current_thread_identity(0, leader.clone());
    let _ = set_current_thread_payload(0, leader_payload.clone());
    let _ = set_current_userspace_thread_identity(1, leader.clone());
    let _ = set_current_userspace_payload(1, leader_payload);

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(9));

    assert!(current_thread_payload(0).is_none());
    assert!(current_thread_identity(0).is_none());
    assert!(current_userspace_payload(1).is_none());
    assert!(current_userspace_thread_identity(1).is_none());
    assert!(proc_cap.is_zombie());
}

#[test]
fn last_thread_exit_zombifies_owner_process() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 99);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(99)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn exec_collapse_rejects_thread_exit_completion_after_remaining_reaches_zero() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("process alive");
    let generation = payload
        .reserve_exec_lifecycle(leader.tid.0)
        .expect("reserve exec lifecycle");

    assert!(payload.begin_exec_collapse(generation, 1));
    let first = payload
        .prepare_thread_exit(leader.tid.0.wrapping_add(1), ExitStatus::Exited(0))
        .expect("first distinct tid claims exit");
    let excess = payload
        .prepare_thread_exit(leader.tid.0.wrapping_add(2), ExitStatus::Exited(0))
        .expect("second distinct tid claims exit");

    assert!(payload.finish_thread_exit(first, false));
    assert!(
        !payload.finish_thread_exit(excess, false),
        "a permit cannot complete after the exec-collapse counter reaches zero"
    );
    assert!(payload.finish_exec_collapse(generation));
    assert!(payload.exec_lifecycle_matches(generation));
    assert!(payload.release_exec_lifecycle(generation));
    drop(payload_guard);
    assert_eq!(proc_cap.live_thread_count(), 1);
}

#[test]
fn exec_collapse_abort_of_suspended_exit_settles_attempt() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("process alive");
    let generation = payload
        .reserve_exec_lifecycle(leader.tid.0)
        .expect("reserve exec lifecycle");
    let sibling_tid = leader.tid.0.wrapping_add(1);

    assert!(payload.begin_exec_collapse(generation, 1));
    let permit = payload
        .prepare_thread_exit(sibling_tid, ExitStatus::Exited(0))
        .expect("sibling claims collapse participant");
    assert!(payload.abort_thread_exit(sibling_tid, permit));
    assert!(
        payload
            .prepare_thread_exit(sibling_tid, ExitStatus::Exited(0))
            .is_none(),
        "aborted participant remains tombstoned until collapse handoff"
    );
    assert_eq!(
        payload.handoff_exec_collapse_abort(generation),
        crate::process::structure::ExecCollapseHandoff::Completed
    );
    assert!(payload.release_exec_lifecycle(generation));
}

#[test]
fn weak_owner_proc_upgrades_while_process_lives() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let upgraded = leader.upgrade_owner_proc();
    assert!(
        upgraded.is_some(),
        "owner should upgrade while process is live"
    );
    assert_eq!(upgraded.unwrap().pid, proc_cap.pid);
}

#[test]
fn weak_owner_proc_survives_payload_drop() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    // Zombify by exit_group; identity persists, payload gone.
    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(0));
    assert!(proc_cap.is_zombie());

    // Weak still resolves to the (zombie) identity.
    let upgraded = leader.upgrade_owner_proc();
    assert!(
        upgraded.is_some(),
        "weak should still resolve to zombie identity"
    );
}

#[test]
fn weak_owner_proc_flips_dead_after_identity_drop() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    // Zombify so the process payload is gone but the identity is still
    // retained by `proc_cap`.
    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(0));
    assert!(leader.upgrade_owner_proc().is_some());

    // Drop the strong handles. Weak observers should no longer find
    // it after epoch drain. Releasing the test's local Cap is not
    // sufficient: bootstrap_init_process registers the init Cap in
    // the global INIT_PROCESS slot, which is the second strong
    // retainer. Tests release it explicitly via
    // reset_init_process_for_test.
    drop(proc_cap);
    reset_init_process_for_test();
    tx_test_support::drain_to_quiescence();

    assert!(
        leader.upgrade_owner_proc().is_none(),
        "weak should observe dead after identity drops"
    );
}

#[test]
fn fork_assigns_distinct_tids_to_parent_and_child_leader_threads() {
    let _g = setup();
    let parent = bootstrap();
    let parent_leader = first_thread(&parent);

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_leader = first_thread(&child);

    assert_ne!(parent_leader.tid, child_leader.tid);
    // Owner reverse-pointers go to the right places.
    assert_eq!(
        parent_leader.upgrade_owner_proc().expect("alive").pid,
        parent.pid
    );
    assert_eq!(
        child_leader.upgrade_owner_proc().expect("alive").pid,
        child.pid
    );
}

// ---------------------------------------------------------------------------
// Trap-handoff payload extension tests (Trio Phase 1)
// ---------------------------------------------------------------------------

struct CountWake {
    wakes: Arc<AtomicUsize>,
}

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(wakes: Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(CountWake { wakes }))
}

/// Round-trip the trap-shell handoff against a `ThreadPayload`'s
/// `userspace_slot`: start a request, post a `Syscall` trap via the
/// slot, drive the wait future to readiness, and observe the
/// `UserspaceTrapInfo::Syscall` outcome. Mirrors what the trap shell
/// does when it resolves the wait via `complete_interesting_trap`
/// per `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`.
#[test]
fn userspace_slot_round_trip_resolves_with_syscall() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let payload_guard = leader.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");

    let slot = payload.userspace_slot().clone();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    payload.set_active_userspace_request(Some(request));

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    // Until the trap resolves the wait, the future stays pending.
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);

    let req = SyscallRequest::new(64, [1, 2, 3, 4, 5, 6]);
    let trap = UserspaceTrapInfo::Syscall(req);
    slot.complete_interesting_trap(request, trap)
        .expect("trap-shell resolves wait");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(trap));
}

#[test]
fn thread_exit_wakes_retained_userspace_future_with_terminal_latch() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload = leader.payload_cap_for_test().expect("alive");

    let slot = payload.userspace_slot().clone();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    payload.set_active_userspace_request(Some(request));

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);

    let _ = step_thread_exit(leader, 0);

    assert!(
        payload.exit_intent(),
        "a retained task payload must observe identity teardown without relocking the identity"
    );
    assert!(wakes.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut cx),
        Poll::Ready(UserspaceTrapInfo::TimerPreempt),
        "teardown must resolve an in-flight userspace wait so run_thread can reach its latch"
    );
}

/// `pending_syscall_return` is a one-shot slot drained by the
/// userspace-entry shim. Plan B writeback discipline: the trap shell
/// never writes the return; the shim drains and writes it into the
/// fresh trap frame before `enter_userspace`.
#[test]
fn pending_syscall_return_drains_at_userspace_entry() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let payload_guard = leader.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");

    // Empty by default.
    assert!(drain_pending_syscall_return(payload).is_none());

    // Successful syscall (e.g. write returning 6 bytes).
    payload.store_pending_syscall_return(Some(Ok(6)));
    assert_eq!(drain_pending_syscall_return(payload), Some(Ok(6)));
    // Drain leaves the slot empty; second drain yields None.
    assert!(drain_pending_syscall_return(payload).is_none());

    // Errno path (e.g. -ENOSYS = -38 encoded as Err(38)).
    payload.store_pending_syscall_return(Some(Err(38)));
    assert_eq!(drain_pending_syscall_return(payload), Some(Err(38)));
    assert!(drain_pending_syscall_return(payload).is_none());
}

// ---------------------------------------------------------------------------
// Userspace-entry shim tests (Pre-ELF Phase 2 Part 1)
//
// `prepare_userspace_entry_payload` is the single Plan-B writeback site
// that drains `pending_syscall_return`, overlays the encoded value into
// the `a0`-equivalent register of the saved user context, clears
// `active_userspace_request`, and returns the merged `UserTrapContext`
// to the platform's `enter_userspace_with_context` shim.
// ---------------------------------------------------------------------------

/// RV64 register index of `a0` inside `UserTrapContext::regs`. Mirrors
/// the constant used internally by `prepare_userspace_entry_payload`.
const A0_INDEX: usize = 10;

fn install_saved_context(
    payload: &PayloadCap<crate::thread_runtime::ThreadPayload>,
) -> UserTrapContext {
    let mut ctx = UserTrapContext {
        regs: [0; 32],
        pc: 0xCAFE_F00D,
        status: 0,
        fp: tx_hal::UserFpContext::empty(),
    };
    // Plant a recognisable value in a0 so we can prove pre-existing
    // contents are overwritten only when a syscall return is drained.
    ctx.regs[A0_INDEX] = 0xDEAD;
    payload.store_saved_user_context(Some(ctx));
    ctx
}

#[test]
fn clean_trap_capture_retains_previous_fp_image() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload = leader.payload_cap_for_test().expect("alive");

    let mut previous = UserTrapContext::empty();
    previous.pc = 0x1000;
    previous.fp.flags = tx_hal::UserFpContext::FLAG_VALID;
    previous.fp.regs[3] = 0xfeed_face_cafe_beef;
    payload.store_saved_user_context(Some(previous));

    let mut clean_capture = UserTrapContext::empty();
    clean_capture.pc = 0x2000;
    clean_capture.regs[10] = 7;
    payload.store_captured_user_context(clean_capture);

    let merged = payload.saved_user_context().expect("merged capture");
    assert_eq!(merged.pc, 0x2000);
    assert_eq!(merged.regs[10], 7);
    assert!(merged.fp.is_valid());
    assert_eq!(merged.fp.regs[3], 0xfeed_face_cafe_beef);

    let mut dirty_capture = clean_capture;
    dirty_capture.pc = 0x3000;
    dirty_capture.fp.flags = tx_hal::UserFpContext::FLAG_VALID;
    dirty_capture.fp.regs[3] = 0x1234;
    payload.store_captured_user_context(dirty_capture);
    let replaced = payload.saved_user_context().expect("replaced capture");
    assert_eq!(replaced.pc, 0x3000);
    assert_eq!(replaced.fp.regs[3], 0x1234);
}

#[test]
fn prepare_userspace_entry_payload_drains_pending_return_into_a0() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload = leader.payload_cap_for_test().expect("alive");

    install_saved_context(&payload);
    payload.store_pending_syscall_return(Some(Ok(42)));

    let ctx = prepare_userspace_entry_payload(&payload);

    assert_eq!(ctx.regs[A0_INDEX], 42, "pending Ok(42) lands in a0");
    assert_eq!(ctx.pc, 0xCAFE_F00D, "non-a0 context preserved");
    assert!(
        drain_pending_syscall_return(&payload).is_none(),
        "shim drains pending_syscall_return exactly once"
    );
    assert!(
        payload.active_userspace_request().is_none(),
        "shim clears active_userspace_request",
    );
}

#[test]
fn prepare_userspace_entry_payload_negative_errno_encodes_as_minus_errno() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload = leader.payload_cap_for_test().expect("alive");

    install_saved_context(&payload);
    // EINVAL = 22 → a0 = (-22 as i64) as u64
    payload.store_pending_syscall_return(Some(Err(22)));

    let ctx = prepare_userspace_entry_payload(&payload);

    let expected = (-22i64) as u64 as usize;
    assert_eq!(ctx.regs[A0_INDEX], expected, "Err(22) encodes as -22");
    assert!(drain_pending_syscall_return(&payload).is_none());
}

#[test]
fn prepare_userspace_entry_payload_no_pending_preserves_saved_a0() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let payload = leader.payload_cap_for_test().expect("alive");

    install_saved_context(&payload);
    // Leave pending_syscall_return empty.
    assert!(drain_pending_syscall_return(&payload).is_none());

    let ctx = prepare_userspace_entry_payload(&payload);

    assert_eq!(
        ctx.regs[A0_INDEX], 0xDEAD,
        "with no drain the saved a0 is preserved verbatim"
    );
}
