// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-19).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::process::execution::step_clone_thread;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::ThreadIdentity;
use tx_hal::UserTrapContext;

/// Build a parent `UserTrapContext` with distinguishable values
/// in every GPR slot so child-side preservation is observable.
fn synthetic_parent_ctx() -> UserTrapContext {
    let mut regs = [0usize; 32];
    for (i, slot) in regs.iter_mut().enumerate() {
        *slot = 0x1000 + i;
    }
    UserTrapContext {
        regs,
        pc: 0x4000_1000,
        status: 0xdeadc0de,
        fp: tx_hal::UserFpContext::empty(),
    }
}

#[test]
fn clone_thread_creates_sibling_in_same_process() {
    let _g = setup();
    let parent = bootstrap();

    // Before: exactly one leader thread.
    assert_eq!(parent.live_thread_count(), 1);
    let before_snapshot = {
        let pg = parent.payload.lock();
        let p = pg.as_ref().expect("parent live");
        p.threads.snapshot()
    };
    assert_eq!(before_snapshot.len(), 1);

    let parent_ctx = synthetic_parent_ctx();
    let stack: usize = 0x7000_0000;
    let tls: usize = 0x6000_0000;
    let ctid: u64 = 0x8000_0000;

    let child =
        step_clone_thread(&parent, &parent_ctx, stack, tls, ctid).expect("step_clone_thread");

    // After: two threads.
    assert_eq!(parent.live_thread_count(), 2);
    let after_snapshot = {
        let pg = parent.payload.lock();
        let p = pg.as_ref().expect("parent live");
        p.threads.snapshot()
    };
    assert_eq!(after_snapshot.len(), 2);

    // Child tid differs from the leader.
    assert_ne!(child.tid, before_snapshot[0].tid);

    // Child belongs to the same process.
    let guard = crate::process::adapter::step_engine::guard();
    let owner = child.owner_proc.upgrade(&guard).expect("owner live");
    assert_eq!(owner.pid, parent.pid);
    drop(guard);

    // Child has a saved_user_context with a0=0 (clone return value)
    // and correct sp/tp from our arguments.
    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");
    assert_eq!(saved.regs[10], 0, "child a0 must be 0 (clone return)");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(saved.regs[2], stack, "child sp must be the requested stack");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(saved.regs[4], tls, "child tp must be the requested tls");

    // Child has the clear_child_tid stored.
    let ctid_stored = *child
        .payload_cap()
        .expect("fresh child has payload")
        .clear_child_tid
        .lock();
    assert_eq!(ctid_stored, Some(ctid));
}

#[test]
fn clone_thread_zero_ctid_is_no_op() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    let ctid_stored = *child
        .payload_cap()
        .expect("fresh child has payload")
        .clear_child_tid
        .lock();
    assert_eq!(ctid_stored, None);
}

#[test]
fn clone_thread_increments_thread_count() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    assert_eq!(parent.live_thread_count(), 1);
    let _t2 = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("t2");
    assert_eq!(parent.live_thread_count(), 2);
    let _t3 = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("t3");
    assert_eq!(parent.live_thread_count(), 3);
}

#[test]
fn clone_thread_children_appear_in_thread_snapshot() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let t2 = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("t2");
    let t3 = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("t3");

    let snapshot = {
        let pg = parent.payload.lock();
        let p = pg.as_ref().expect("parent live");
        p.threads.snapshot()
    };
    assert_eq!(snapshot.len(), 3);

    let tids: alloc::vec::Vec<_> = snapshot.iter().map(|t| t.tid).collect();
    assert!(tids.contains(&t2.tid));
    assert!(tids.contains(&t3.tid));
}

#[test]
fn clone_thread_preserves_parent_pc_and_status() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");

    assert_eq!(saved.pc, parent_ctx.pc);
    assert_eq!(saved.status, parent_ctx.status);
}

#[test]
fn clone_thread_zero_stack_inherits_parent_sp() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(
        saved.regs[2], parent_ctx.regs[2],
        "sp inherits parent when stack=0"
    );
}

#[test]
fn clone_thread_zero_tls_inherits_parent_tp() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(
        saved.regs[4], parent_ctx.regs[4],
        "tp inherits parent when tls=0"
    );
}

#[test]
fn ordinary_thread_exit_decrements_live_thread_count() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();
    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    assert_eq!(parent.live_thread_count(), 2);

    step_thread_exit(child, 0);

    assert_eq!(
        parent.live_thread_count(),
        1,
        "thread_count must track the roster after non-last thread exit"
    );
}

#[test]
fn ordinary_thread_exit_unregisters_only_thread_namespace_role() {
    use crate::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};

    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();
    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");
    let child_tid = child.tid.0 as u64;
    let parent_pid = parent.pid.0 as u64;

    assert!(matches!(
        resolve_pid_number_as(child_tid, PidNameKind::Thread),
        Some(PidName::Thread(_))
    ));

    step_thread_exit(child, 0);

    assert!(
        resolve_pid_number_as(child_tid, PidNameKind::Thread).is_none(),
        "non-leader thread exit must remove only the TID binding"
    );
    assert!(matches!(
        resolve_pid_number_as(parent_pid, PidNameKind::Process),
        Some(PidName::Process(_)),
    ));
    assert!(matches!(
        resolve_pid_number_as(parent_pid, PidNameKind::Thread),
        Some(PidName::Thread(_)),
    ));
}

#[test]
fn repeated_clone_thread_and_exit_keeps_process_roster_consistent() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();
    let leader = first_thread(&parent);

    let mut children = alloc::vec::Vec::new();
    for i in 0..128usize {
        let stack = 0x7000_0000 + i * 0x4000;
        let tls = 0x6000_0000 + i * 0x40;
        let ctid = 0x8000_0000 + i as u64 * 4;
        let child =
            step_clone_thread(&parent, &parent_ctx, stack, tls, ctid).expect("step_clone_thread");
        assert!(
            parent.thread_by_tid(child.tid.0).is_some(),
            "fresh clone must be addressable by tid"
        );
        children.push(child);
    }

    assert_eq!(parent.live_thread_count(), 129);
    assert!(
        parent.thread_by_tid(leader.tid.0).is_some(),
        "leader must stay in the process roster while siblings run"
    );

    for child in children {
        let tid = child.tid.0;
        step_thread_exit(child, 0);
        assert!(
            parent.thread_by_tid(tid).is_none(),
            "exited sibling must leave the process roster"
        );
    }

    assert_eq!(parent.live_thread_count(), 1);
    assert!(
        parent.thread_by_tid(leader.tid.0).is_some(),
        "leader must be the sole remaining live thread"
    );
}

#[test]
fn exec_group_collapse_keeps_initiator_and_clears_episode() {
    let _g = setup();
    let parent = bootstrap();
    let leader = first_thread(&parent);
    let parent_ctx = synthetic_parent_ctx();
    let sibling = step_clone_thread(&parent, &parent_ctx, 0, 0, 0).expect("step_clone_thread");

    let collapsed = parent
        .collapse_threads_for_exec(&leader)
        .expect("live process");

    assert_eq!(collapsed, 1);
    assert_eq!(parent.live_thread_count(), 1);
    assert!(
        parent.thread_by_tid(leader.tid.0).is_some(),
        "exec initiator remains the sole live thread"
    );
    assert!(
        parent.thread_by_tid(sibling.tid.0).is_none(),
        "exec collapse removes sibling thread from live roster"
    );
    assert!(
        sibling.is_zombie(),
        "exec collapse zombifies sibling identities before AS replacement"
    );
}
