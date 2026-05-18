// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

// Tests for [`crate::process::seed_child_leader_context`].

use super::*;
use crate::process::seed_child_leader_context;
use tx_hal::UserTrapContext;

/// Build a parent `UserTrapContext` with distinguishable values
/// in every GPR slot so child-side preservation is observable.
fn synthetic_parent_ctx() -> UserTrapContext {
    let mut regs = [0usize; 32];
    for (i, slot) in regs.iter_mut().enumerate() {
        // 0x1000 + index to keep low 12 bits distinct from any
        // arch sentinel.
        *slot = 0x1000 + i;
    }
    UserTrapContext {
        regs,
        pc: 0x4000_1000,
        status: 0xdeadc0de,
        fp: tx_hal::UserFpContext::empty(),
    }
}

fn child_leader(child: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    child
        .nth_thread(0)
        .expect("fresh child has a leader thread")
}

#[test]
fn seed_child_leader_context_zeroes_a0() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");
    let leader = child_leader(&child);

    let parent_ctx = synthetic_parent_ctx();
    seed_child_leader_context(&leader, &parent_ctx, 0, 0);

    let saved = leader
        .payload_cap()
        .expect("fresh child leader has payload")
        .saved_user_context()
        .expect("seed installs Some");
    assert_eq!(
        saved.regs[10], 0,
        "RV64 a0 (regs[10]) must be 0 in the child — Linux fork-clone ABI: \
         child's syscall return value is 0",
    );
    // With tls=0 (no CLONE_SETTLS), tp inherits the parent's value.
    assert_eq!(
        saved.regs[4], parent_ctx.regs[4],
        "RV64 tp (regs[4]) must match parent when tls=0",
    );
}

#[test]
fn seed_child_leader_context_inherits_pc() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");
    let leader = child_leader(&child);

    let parent_ctx = synthetic_parent_ctx();
    seed_child_leader_context(&leader, &parent_ctx, 0, 0);

    let saved = leader
        .payload_cap()
        .expect("fresh child leader has payload")
        .saved_user_context()
        .expect("seed installs Some");
    assert_eq!(
        saved.pc, parent_ctx.pc,
        "the trap shell (`tx-kernel::trap_handoff::hand_off_syscall`) \
         already advances PC past the trapping ecall before storing \
         `saved_user_context`; the child therefore inherits the same \
         post-ecall PC as the parent and must NOT double-advance.",
    );
}

#[test]
fn seed_child_leader_context_preserves_other_gprs_and_sp() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");
    let leader = child_leader(&child);

    let parent_ctx = synthetic_parent_ctx();
    seed_child_leader_context(&leader, &parent_ctx, 0, 0);

    let saved = leader
        .payload_cap()
        .expect("fresh child leader has payload")
        .saved_user_context()
        .expect("seed installs Some");

    // Every GPR except a0 (regs[10]) is preserved verbatim.
    for i in 0..32 {
        if i == 10 {
            continue;
        }
        assert_eq!(
            saved.regs[i], parent_ctx.regs[i],
            "regs[{i}] must match parent (only regs[10] / a0 is rewritten)",
        );
    }
    // sp = regs[2] specifically — Linux's bare-clone-with-stack=NULL
    // convention means the child shares the parent's sp and walks
    // it via demand-faulted private-anon recipes.
    assert_eq!(
        saved.regs[2], parent_ctx.regs[2],
        "sp (regs[2]) must equal the parent's sp under the bare-clone convention",
    );
    // status word is also preserved verbatim — the child re-enters
    // userspace under the same supervisor-status snapshot.
    assert_eq!(saved.status, parent_ctx.status);
}

#[test]
fn seed_child_leader_context_uses_nonzero_child_stack() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");
    let leader = child_leader(&child);

    let parent_ctx = synthetic_parent_ctx();
    let child_stack = 0x7000_1230;
    seed_child_leader_context(&leader, &parent_ctx, child_stack, 0);

    let saved = leader
        .payload_cap()
        .expect("fresh child leader has payload")
        .saved_user_context()
        .expect("seed installs Some");
    assert_eq!(saved.regs[2], child_stack);
}
