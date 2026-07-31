// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-19).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::process::execution::step_clone_thread;
use crate::signal::SignalMask;
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

    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, stack, tls, ctid)
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

    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
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
    let _t2 = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0).expect("t2");
    assert_eq!(parent.live_thread_count(), 2);
    let _t3 = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0).expect("t3");
    assert_eq!(parent.live_thread_count(), 3);
}

#[test]
fn clone_thread_children_appear_in_thread_snapshot() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();

    let t2 = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0).expect("t2");
    let t3 = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0).expect("t3");

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

    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
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

    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
        .expect("step_clone_thread");

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

    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
        .expect("step_clone_thread");

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
fn clone_thread_inherits_parent_signal_mask() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();
    let inherited_mask = SignalMask::new((1u64 << (2 - 1)) | (1u64 << (17 - 1)));

    let child = step_clone_thread(&parent, &parent_ctx, inherited_mask, 0, 0, 0)
        .expect("step_clone_thread");

    let child_mask = child
        .payload_cap()
        .expect("fresh child has payload")
        .signal_mask();
    assert_eq!(
        child_mask.raw_bits(),
        inherited_mask.raw_bits(),
        "clone thread must inherit the caller's blocked-signal mask"
    );
}

#[test]
fn ordinary_thread_exit_decrements_live_thread_count() {
    let _g = setup();
    let parent = bootstrap();
    let parent_ctx = synthetic_parent_ctx();
    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
        .expect("step_clone_thread");

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
    let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
        .expect("step_clone_thread");
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
        let child = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, stack, tls, ctid)
            .expect("step_clone_thread");
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
    let sibling = step_clone_thread(&parent, &parent_ctx, SignalMask::EMPTY, 0, 0, 0)
        .expect("step_clone_thread");

    let mut prep =
        crate::process::ProcessExecPrep::begin(&parent, &leader).expect("reserve exec lifecycle");
    let collapsed = prep.collapse_threads(&leader).expect("live process");

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
    assert_eq!(
        crate::process::ProcessExecPrep::begin(&parent, &leader).err(),
        Some(crate::process::ExecPrepError::Again),
        "collapse keeps the exec lifecycle episode reserved through phase 7"
    );
    drop(prep);
    assert!(crate::process::ProcessExecPrep::begin(&parent, &leader).is_ok());
}

#[test]
fn thread_exit_reservation_makes_concurrent_exec_retry_before_zombify() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("step_clone_thread");
    let (checked, worker_pause) = bounded_test_pause();

    std::thread::scope(|scope| {
        let exiting_leader = leader.clone();
        let exit = scope.spawn(move || {
            crate::thread_runtime::execution::step_thread_exit_after_lane_check_for_test(
                exiting_leader,
                23,
                move || worker_pause.pause(),
            )
        });

        checked.wait_until_paused();
        let begin_error = match crate::process::ProcessExecPrep::begin(&process, &leader) {
            Ok(prep) => {
                drop(prep);
                None
            }
            Err(error) => Some(error),
        };
        checked.release();
        assert_eq!(
            exit.join().expect("thread-exit worker"),
            crate::thread_runtime::ThreadExitOutcome::Completed,
        );
        assert_eq!(begin_error, Some(crate::process::ExecPrepError::Again));
    });

    assert!(leader.is_zombie());
    assert!(!process.is_zombie());
    assert!(process.thread_by_tid(sibling.tid.0).is_some());
}

#[test]
fn stale_exec_binding_cannot_collapse_or_replace_aspace() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let mut prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");
    let detached = process
        .payload
        .lock()
        .take()
        .expect("force stale binding for negative test");
    let old_aspace_key = detached.aspace_cap().key();

    assert_eq!(
        prep.collapse_threads(&leader),
        Err(crate::process::ExecPrepError::Zombie),
    );
    assert_eq!(
        prep.replace_aspace(fresh_aspace()).err(),
        Some(crate::process::ExecPrepError::Zombie),
    );
    assert_eq!(detached.aspace_cap().key(), old_aspace_key);
}

#[test]
fn clone_thread_racing_exec_reservation_retries_without_attach() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let (clone_ready, worker_pause) = bounded_test_pause();
    let candidate_tid = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    std::thread::scope(|scope| {
        let clone_process = process.clone();
        let worker_candidate_tid = candidate_tid.clone();
        let clone = scope.spawn(move || {
            crate::process::execution::step_clone_thread_before_attach_for_test(
                &clone_process,
                &synthetic_parent_ctx(),
                SignalMask::EMPTY,
                0,
                0,
                0,
                move |tid| {
                    worker_candidate_tid.store(tid.0 as u64, std::sync::atomic::Ordering::Release);
                    worker_pause.pause();
                },
            )
        });

        clone_ready.wait_until_paused();
        let prep = crate::process::ProcessExecPrep::begin(&process, &leader)
            .expect("exec wins the lifecycle lane before clone attach");
        let candidate_tid = candidate_tid.load(std::sync::atomic::Ordering::Acquire);
        assert_ne!(
            candidate_tid, 0,
            "clone worker must publish its candidate to the test"
        );
        assert!(
            crate::process::numbers::resolve_pid_number_as(
                candidate_tid,
                crate::process::numbers::PidNameKind::Thread,
            )
            .is_none(),
            "a clone rejected by lifecycle admission must never publish a ghost TID role"
        );
        clone_ready.release();
        assert!(matches!(
            clone.join().expect("clone worker"),
            Err(crate::process::ForkError::Busy)
        ));
        assert_eq!(process.live_thread_count(), 1);
        assert_eq!(process.threads_snapshot().expect("live process").len(), 1);
        drop(prep);
    });
}

#[test]
fn clone_thread_during_exec_collapse_retries_without_attach() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed sibling");
    let (collapsing, worker_pause) = bounded_test_pause();

    std::thread::scope(|scope| {
        let exec_leader = leader.clone();
        let mut prep = crate::process::ProcessExecPrep::begin(&process, &leader)
            .expect("reserve exec lifecycle");
        let collapse = scope.spawn(move || {
            prep.collapse_threads_after_lane_transition_for_test(&exec_leader, move || {
                worker_pause.pause()
            })
        });

        collapsing.wait_until_paused();
        assert!(matches!(
            step_clone_thread(
                &process,
                &synthetic_parent_ctx(),
                SignalMask::EMPTY,
                0,
                0,
                0,
            ),
            Err(crate::process::ForkError::Busy)
        ));
        collapsing.release();
        assert_eq!(collapse.join().expect("collapse worker"), Ok(1));
    });

    assert_eq!(process.live_thread_count(), 1);
    assert!(process.thread_by_tid(leader.tid.0).is_some());
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
}

#[test]
fn exec_collapse_tolerates_sibling_exit_after_snapshot() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed sibling");
    let callback_sibling = sibling.clone();
    let mut prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");

    let collapsed = prep.collapse_threads_after_lane_transition_for_test(&leader, move || {
        assert_eq!(
            step_thread_exit(callback_sibling, 23),
            crate::thread_runtime::ThreadExitOutcome::Completed,
            "sibling completes its own exit after the exec snapshot"
        );
    });

    assert_eq!(collapsed, Ok(1));
    assert_eq!(process.live_thread_count(), 1);
    assert_eq!(
        process.threads_snapshot().expect("live process"),
        alloc::vec![leader.clone()]
    );
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
}

#[test]
fn exec_collapse_claims_each_sibling_tid_once() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed sibling");
    let callback_sibling = sibling.clone();
    let callback_process = process.clone();
    let mut prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");

    let collapsed = prep.collapse_threads_after_lane_transition_for_test(&leader, move || {
        let (claimed, worker_pause) = bounded_test_pause();

        std::thread::scope(|scope| {
            let first_sibling = callback_sibling.clone();
            let first = scope.spawn(move || {
                crate::thread_runtime::execution::step_thread_exit_after_lane_check_for_test(
                    first_sibling,
                    29,
                    move || worker_pause.pause(),
                )
            });

            claimed.wait_until_paused();
            let second = step_thread_exit(callback_sibling.clone(), 31);

            claimed.release();
            assert_eq!(
                first.join().expect("first sibling exit"),
                crate::thread_runtime::ThreadExitOutcome::Completed
            );
            assert_eq!(
                second,
                crate::thread_runtime::ThreadExitOutcome::Retry,
                "a second exit cannot claim the same tid in one exec episode"
            );
            assert_eq!(callback_process.live_thread_count(), 1);
            assert!(callback_process
                .thread_by_tid(callback_sibling.tid.0)
                .is_none());
        });
    });

    assert_eq!(collapsed, Ok(1));
    assert_eq!(process.live_thread_count(), 1);
    assert_eq!(
        process.threads_snapshot().expect("live process"),
        alloc::vec![leader]
    );
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
}

#[test]
fn exec_collapse_hands_off_abort_until_claimed_exit_finishes() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed sibling");
    let other_sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed second sibling");
    let callback_sibling = sibling.clone();
    let (claimed, worker_pause) = bounded_test_pause();
    let worker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let callback_worker = worker.clone();
    let callback_claimed = &claimed;
    let mut prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");

    let collapsed = prep.collapse_threads_after_lane_transition_for_test(&leader, move || {
        let handle = std::thread::spawn(move || {
            crate::thread_runtime::execution::step_thread_exit_after_lane_check_for_test(
                callback_sibling,
                37,
                move || worker_pause.pause(),
            )
        });
        callback_claimed.wait_until_paused();
        *callback_worker.lock().expect("worker slot") = Some(handle);
    });
    let count_at_handoff = process.live_thread_count();
    let other_completed =
        other_sibling.is_zombie() && process.thread_by_tid(other_sibling.tid.0).is_none();

    claimed.release();
    let worker_result = worker
        .lock()
        .expect("worker slot")
        .take()
        .expect("exit worker")
        .join();

    assert_eq!(collapsed, Err(crate::process::ExecPrepError::Again));
    assert_eq!(
        count_at_handoff, 2,
        "exec continues collapsing unclaimed siblings before abort handoff"
    );
    assert!(
        other_completed,
        "unclaimed sibling completes before handoff"
    );
    assert_eq!(
        worker_result.expect("claimed sibling exit"),
        crate::thread_runtime::ThreadExitOutcome::Completed
    );
    assert_eq!(process.live_thread_count(), 1);
    assert_eq!(
        process.threads_snapshot().expect("live process"),
        alloc::vec![leader.clone()]
    );
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
    assert!(
        crate::process::ProcessExecPrep::begin(&process, &leader).is_ok(),
        "last aborting exit completion releases the lifecycle lane"
    );
}

#[test]
fn exec_collapse_hands_off_abort_for_zombie_with_unfinished_claim() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("seed sibling");
    let callback_sibling = sibling.clone();
    let (zombified, worker_pause) = bounded_test_pause();
    let worker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let callback_worker = worker.clone();
    let callback_zombified = &zombified;
    let mut prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");

    let collapsed = prep.collapse_threads_after_lane_transition_for_test(&leader, move || {
        let handle = std::thread::spawn(move || {
            crate::thread_runtime::execution::step_thread_exit_after_zombify_for_test(
                callback_sibling,
                41,
                move || worker_pause.pause(),
            )
        });
        callback_zombified.wait_until_paused();
        *callback_worker.lock().expect("worker slot") = Some(handle);
    });

    zombified.release();
    let worker_result = worker
        .lock()
        .expect("worker slot")
        .take()
        .expect("exit worker")
        .join();

    assert_eq!(collapsed, Err(crate::process::ExecPrepError::Again));
    assert_eq!(
        worker_result.expect("claimed sibling exit"),
        crate::thread_runtime::ThreadExitOutcome::Completed
    );
    assert_eq!(process.live_thread_count(), 1);
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
    assert!(
        crate::process::ProcessExecPrep::begin(&process, &leader).is_ok(),
        "zombie claim completion releases the aborting lifecycle lane"
    );
}

#[test]
fn clone_thread_commit_is_rejected_after_group_exit_install() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let sibling =
        crate::process::execution::spawn_sibling_thread_for_test(&process).expect("sibling");

    let participants =
        crate::process::execution::initiate_group_exit(&process, ExitStatus::Exited(31));
    assert_eq!(participants.len(), 2);

    let result = step_clone_thread(
        &process,
        &synthetic_parent_ctx(),
        SignalMask::EMPTY,
        0,
        0,
        0,
    );
    assert!(
        matches!(
            result,
            Err(crate::process::adapter::step_engine::ZoneError::InvalidState)
        ),
        "CLONE_THREAD must not publish after GroupExit fixed its participant set"
    );
    assert_eq!(process.live_thread_count(), 2);

    crate::thread_runtime::step_thread_exit_with_status(leader, ExitStatus::Exited(31));
    crate::thread_runtime::step_thread_exit_with_status(sibling, ExitStatus::Exited(31));
    assert!(process.is_zombie());
}
