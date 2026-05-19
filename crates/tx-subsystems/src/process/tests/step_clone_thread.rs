// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-19).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::process::execution::step_clone_thread;
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

    let child = step_clone_thread(&parent, &parent_ctx, stack, tls, ctid)
        .expect("step_clone_thread");

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

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0)
        .expect("step_clone_thread");

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

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0)
        .expect("step_clone_thread");

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

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0)
        .expect("step_clone_thread");

    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(saved.regs[2], parent_ctx.regs[2], "sp inherits parent when stack=0");
}

#[test]
fn clone_thread_zero_tls_inherits_parent_tp() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let child = step_clone_thread(&parent, &parent_ctx, 0, 0, 0)
        .expect("step_clone_thread");

    let saved = child
        .payload_cap()
        .expect("fresh child has payload")
        .saved_user_context()
        .expect("seed installs Some");

    #[cfg(not(target_arch = "loongarch64"))]
    assert_eq!(saved.regs[4], parent_ctx.regs[4], "tp inherits parent when tls=0");
}
