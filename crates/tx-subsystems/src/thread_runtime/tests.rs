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

use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_fork, ExitStatus};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::adapter::reactor_entry::{SyscallRequest, UserspaceTrapInfo};
use crate::thread_runtime::adapter::step_engine::{guard, Cap, PayloadCap, StepOutcome};
use crate::thread_runtime::execution::prepare_userspace_entry_payload;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::{
    bind_thread_task, current_thread_task, drain_pending_syscall_return,
    reset_tid_counter_for_test, set_current_thread_payload, thread_by_tid, thread_payload_by_tid,
    thread_task_by_tid, ThreadIdentity,
};
use crate::vm::adapter::step_engine::page_allocator;
use crate::vm::{
    AddressSpace, MapPlacement, Prot, TestPmap, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use crate::zones;
use tx_hal::UserPtr;
use tx_hal::UserTrapContext;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for thread-runtime tests: {error:?}"),
    }
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

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let threads = payload.threads.snapshot();
    threads[0].clone()
}

fn process_aspace(proc_cap: &Cap<ProcessIdentity>) -> Cap<AddressSpace> {
    proc_cap
        .payload
        .lock()
        .as_ref()
        .expect("alive process")
        .aspace_cap()
}

fn map_user_page(aspace: &AddressSpace, page_start: usize) {
    let range =
        UserRange::new_aligned(UserVirtAddr(page_start), USER_PAGE_SIZE).expect("aligned range");
    let req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    aspace.try_mmap(req).expect("mmap anon page for test");
}

fn write_user_u64(aspace: &AddressSpace, addr: u64, value: u64) {
    let eguard = guard();
    let copied = aspace.copy_to_user(
        UserPtr::<u8>::new(addr as usize),
        &value.to_ne_bytes(),
        &eguard,
    );
    drop(eguard);
    assert_eq!(copied, StepOutcome::Done(core::mem::size_of::<u64>()));
}

fn write_user_u32(aspace: &AddressSpace, addr: u64, value: u32) {
    let eguard = guard();
    let copied = aspace.copy_to_user(
        UserPtr::<u8>::new(addr as usize),
        &value.to_ne_bytes(),
        &eguard,
    );
    drop(eguard);
    assert_eq!(copied, StepOutcome::Done(core::mem::size_of::<u32>()));
}

fn read_user_u32(aspace: &AddressSpace, addr: u64) -> u32 {
    let eguard = guard();
    let value = match aspace.read_user(UserPtr::<u32>::new(addr as usize), &eguard) {
        StepOutcome::Done(value) => value,
        other => panic!("read_user_u32 failed: {other:?}"),
    };
    drop(eguard);
    value
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
fn thread_task_lookup_tracks_payload_binding_and_zombie_clear() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let tid = leader.tid;
    let payload = leader.payload_cap().expect("leader payload");
    let reactor = tx_reactor::Reactor::new();
    let task = reactor.submit_task(async {});

    assert_eq!(thread_by_tid(tid).map(|thread| thread.tid), Some(tid));
    assert!(thread_payload_by_tid(tid).is_some());
    assert_eq!(thread_task_by_tid(tid), None);

    assert_eq!(bind_thread_task(&leader, task), None);
    assert_eq!(payload.task(), Some(task));
    assert_eq!(thread_task_by_tid(tid), Some(task));

    let _prev = set_current_thread_payload(0, payload.clone());
    assert_eq!(current_thread_task(0), Some(task));
    let _ = crate::thread_runtime::clear_current_thread_payload(0);

    step_thread_exit(leader.clone(), 0);
    assert!(leader.is_zombie());
    assert!(thread_payload_by_tid(tid).is_none());
    assert_eq!(thread_task_by_tid(tid), None);
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
fn last_thread_exit_marks_robust_list_and_pending_owner_died() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);
    let aspace = process_aspace(&proc_cap);
    let page = 0x5300_0000usize;
    let head = page as u64;
    let entry = head + 0x20;
    let pending = head + 0x40;
    let futex_offset = 8u64;
    let futex = entry + futex_offset;
    let pending_futex = pending + futex_offset;
    const FUTEX_WAITERS: u32 = 0x8000_0000;
    const FUTEX_OWNER_DIED: u32 = 0x4000_0000;

    map_user_page(&aspace, page);
    write_user_u64(&aspace, head, entry);
    write_user_u64(&aspace, head + 8, futex_offset);
    write_user_u64(&aspace, head + 16, pending);
    write_user_u64(&aspace, entry, head);
    write_user_u64(&aspace, pending, 0);
    write_user_u32(&aspace, futex, FUTEX_WAITERS | leader.tid.0);
    write_user_u32(&aspace, pending_futex, leader.tid.0);
    {
        let payload_guard = leader.payload.lock();
        let payload = payload_guard.as_ref().expect("live thread payload");
        *payload.robust_list_head.lock() = Some(head);
        *payload.robust_list_len.lock() = 24;
    }

    step_thread_exit(leader, 99);

    assert_eq!(
        read_user_u32(&aspace, futex),
        FUTEX_WAITERS | FUTEX_OWNER_DIED,
        "list entry should preserve FUTEX_WAITERS and set FUTEX_OWNER_DIED",
    );
    assert_eq!(
        read_user_u32(&aspace, pending_futex),
        FUTEX_OWNER_DIED,
        "list_op_pending should be processed in addition to the list",
    );
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
    crate::process::step_exit_group(&proc_cap, ExitStatus::Exited(0));
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
    crate::process::step_exit_group(&proc_cap, ExitStatus::Exited(0));
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
