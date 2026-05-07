//! Topology tests for the process subsystem.
//!
//! These exercise the entity graph (Process / Thread / ProcessGroup /
//! Session) and the lifecycle steps (`bootstrap_init_process`,
//! `step_fork`, `step_exit_group`, `step_setpgid`, `step_setsid`) without
//! signal state, credentials, rlimits, or fd-table coupling. The
//! identity/payload split is the primary subject under test: zombies
//! retain identity but drop payload.

use crate::process::execution::{
    init_process, reset_init_process_for_test, step_exit_group_with_signal, BootstrapError,
};
use crate::process::structure::{
    reset_pid_counter_for_test, ExitStatus, Pgid, Pid, ProcessIdentity,
};
use crate::process::{
    bootstrap_init_process, step_chdir, step_exit_group, step_fork, step_getcwd, step_setpgid,
    step_setsid, step_waitpid_nohang, ChdirOutcome, ForkError, SetpgidError, WaitError, WaitTarget,
};
use crate::signal::Signum;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::{reset_tid_counter_for_test, ThreadIdentity};
use crate::vfs::{DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;
use tx_substrate::testing::init_host_for_test_once;
use tx_substrate::zone::Cap;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
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
    let threads = payload.threads.lock();
    threads[0].clone()
}

#[test]
fn bootstrap_init_creates_pid_1_with_session_and_pgrp() {
    let _g = setup();
    let init = bootstrap();

    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.parent_pid(), Pid::RESERVED);
    assert!(init.parent_cap().is_none());
    assert!(!init.is_zombie());
    assert_eq!(init.live_thread_count(), 1);

    let pgrp = init.pgrp_cap();
    assert_eq!(pgrp.pgid, Pgid(Pid::INIT.0));
    let session = pgrp.session_cap();
    assert_eq!(session.sid.0, Pid::INIT.0);
    assert!(!session.has_controlling_tty());
}

#[test]
fn fork_creates_child_with_leader_thread_and_inherits_pgrp() {
    let _g = setup();
    let parent = bootstrap();

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    assert_ne!(child.pid, parent.pid);
    assert_eq!(child.parent_pid(), parent.pid);
    assert_eq!(child.parent_cap().expect("parent retained").pid, parent.pid);
    assert!(!child.is_zombie());
    assert_eq!(child.live_thread_count(), 1);

    // Child inherits parent's pgrp.
    assert_eq!(child.pgrp_cap().pgid, parent.pgrp_cap().pgid);
}

#[test]
fn fork_clones_address_space_into_distinct_cap() {
    let _g = setup();
    let parent = bootstrap();
    let parent_aspace = parent.aspace_cap().expect("parent live");

    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_aspace = child.aspace_cap().expect("child live");

    // The Cap keys must differ — child has its own address space slot.
    assert_ne!(parent_aspace.key(), child_aspace.key());
}

#[test]
fn fork_registers_child_in_parent_pgrp_member_list() {
    let _g = setup();
    let parent = bootstrap();
    let pgrp = parent.pgrp_cap();
    let before = pgrp.member_slot_count();

    let _child = step_fork::<TestPmap>(&parent).expect("fork");

    assert_eq!(pgrp.member_slot_count(), before + 1);
}

#[test]
fn fork_on_zombie_parent_returns_parent_zombie() {
    let _g = setup();
    let parent = bootstrap();
    step_exit_group(&parent, ExitStatus::Exited(0));
    assert!(parent.is_zombie());

    let result = step_fork::<TestPmap>(&parent);
    assert!(matches!(result, Err(ForkError::ParentZombie)));
}

#[test]
fn last_thread_exit_zombifies_process_keeps_identity() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 7);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(7)));
    assert_eq!(proc_cap.pid, Pid::INIT);
}

#[test]
fn exit_group_zombifies_process_at_once_and_records_status() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group(&proc_cap, ExitStatus::Exited(42));

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(42)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn setpgid_to_target_pid_creates_new_group_in_same_session() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    let original_session = child.pgrp_cap().session_cap();
    let original_session_id = original_session.sid;

    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");

    let new_pgrp = child.pgrp_cap();
    assert_eq!(new_pgrp.pgid, Pgid(child.pid.0));
    assert_eq!(new_pgrp.session_cap().sid, original_session_id);
    assert_ne!(new_pgrp.pgid, parent.pgrp_cap().pgid);
}

#[test]
fn setpgid_with_existing_group_id_is_unimplemented() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // child.pgid != child.pid, and we don't yet support joining an
    // existing group by id (would require session-walk).
    let result = step_setpgid(&child, parent.pgrp_cap().pgid);
    assert!(matches!(result, Err(SetpgidError::Unimplemented)));
}

#[test]
fn setsid_creates_fresh_session_and_pgrp_at_target_pid() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let parent_session_id = parent.pgrp_cap().session_cap().sid;

    let new_sid = step_setsid(&child).expect("setsid");

    assert_eq!(new_sid.0, child.pid.0);
    let pgrp = child.pgrp_cap();
    assert_eq!(pgrp.pgid.0, child.pid.0);
    let session = pgrp.session_cap();
    assert_eq!(session.sid, new_sid);
    assert_ne!(session.sid, parent_session_id);
    assert!(!session.has_controlling_tty());
}

#[test]
fn pid_pgid_sid_share_value_space_but_are_distinct_types() {
    let _g = setup();
    let init = bootstrap();

    // After bootstrap: pid=1, pgid=1, sid=1 — same numeric values, but
    // the types prevent accidental swapping in code.
    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.pgrp_cap().pgid, Pgid(1));
    assert_eq!(init.pgrp_cap().session_cap().sid.0, 1);
}

#[test]
fn pgrp_member_weak_observation_returns_live_process_until_identity_drops() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let pgrp = parent.pgrp_cap();

    // The pgrp has two members: parent and child.
    assert_eq!(pgrp.member_slot_count(), 2);

    let guard = tx_substrate::epoch::guard();
    let live: usize = pgrp
        .members
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(live, 2);

    // To drop the child identity we must release every strong
    // retainer: the test's `child` Cap *and* the parent.children
    // retainer (per §8.5 the parent's children list pins zombies
    // until reap). Reap path:
    //   1. zombify via step_exit_group
    //   2. drop the test's `child` Cap
    //   3. waitpid reap → parent.children removes its Cap
    //   4. epoch drain → identity reclaims
    // After step 4, pgrp.members's Weak is stale.
    let child_pid = child.pid;
    step_exit_group(&child, ExitStatus::Exited(0));
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap");
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);

    let guard = tx_substrate::epoch::guard();
    let live_after: usize = pgrp
        .members
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(
        live_after, 1,
        "after reap + drain, only parent remains live"
    );
}

#[test]
fn step_exit_group_with_signal_records_signum_and_status_encoding() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group_with_signal(&proc_cap, Signum::SIGTERM);

    assert!(proc_cap.is_zombie());
    assert_eq!(
        proc_cap.exit_status(),
        Some(ExitStatus::Signaled(Signum::SIGTERM))
    );
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));
    // POSIX <sys/wait.h> signaled-exit encoding: signum in low 7 bits.
    // (Migrated from the day-1 shell-convention `128 + sig` shape by
    // Wave 1 of the fork/clone/wait4 slice — Open Q #3 DECIDED.)
    assert_eq!(
        proc_cap.exit_status().unwrap().wait_status_word(),
        Signum::SIGTERM.raw() as i32 & 0x7f
    );
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn step_exit_group_with_signal_overrides_terminating_signal_on_double_call() {
    let _g = setup();
    let proc_cap = bootstrap();

    // First call sets terminating_signal=SIGTERM and zombifies.
    step_exit_group_with_signal(&proc_cap, Signum::SIGTERM);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));

    // Second call on the same identity should be a no-op for the
    // payload (already None) but still updates the recorded signal —
    // demonstrates idempotent slot semantics. Defensive coverage of
    // the double-zombify path.
    step_exit_group_with_signal(&proc_cap, Signum::SIGKILL);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGKILL));
}

// ----- step_waitpid_nohang (PROCESS_v1 §7.4) -----

#[test]
fn waitpid_with_no_children_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_with_live_child_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent).expect("fork");

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(
        result,
        Err(WaitError::NoneReady),
        "child exists but is alive — WNOHANG yields NoneReady, not NoChildren"
    );
}

#[test]
fn waitpid_any_reaps_zombie_child_and_returns_status() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_pid = child.pid;
    drop(child);
    let payload_was_dropped = |proc_cap: &Cap<ProcessIdentity>| proc_cap.payload.lock().is_none();

    // Get the child back via parent.children() so we can exit it.
    let live = parent.children();
    assert_eq!(live.len(), 1);
    let child = live.into_iter().next().unwrap();
    step_exit_group(&child, ExitStatus::Exited(42));
    assert!(payload_was_dropped(&child));

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Ok((child_pid, ExitStatus::Exited(42))));
}

#[test]
fn waitpid_specific_pid_reaps_only_that_child() {
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent).expect("c1");
    let c2 = step_fork::<TestPmap>(&parent).expect("c2");

    step_exit_group(&c1, ExitStatus::Exited(1));
    step_exit_group(&c2, ExitStatus::Exited(2));

    // Reap c2 specifically.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(c2.pid));
    assert_eq!(result, Ok((c2.pid, ExitStatus::Exited(2))));

    // c1 must still be reapable.
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Ok((c1.pid, ExitStatus::Exited(1))));
}

#[test]
fn waitpid_specific_pid_with_no_match_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent).expect("fork");

    // Selector targeting a pid we never forked.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(Pid(9999)));
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_specific_pid_with_live_match_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Match the live child by pid — selector matches but child isn't
    // a zombie yet, so WNOHANG gives NoneReady.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(child.pid));
    assert_eq!(result, Err(WaitError::NoneReady));
}

#[test]
fn waitpid_reap_withdraws_from_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_pid = child.pid;

    assert_eq!(parent.child_count(), 1);

    step_exit_group(&child, ExitStatus::Exited(0));
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap succeeds");

    // Parent's children list no longer carries the reaped child's
    // Weak. Slot count drops to 0.
    assert_eq!(parent.child_count(), 0);
    assert_eq!(parent.children().len(), 0);
}

#[test]
fn waitpid_reap_withdraws_from_pgrp_members_list() {
    let _g = setup();
    let parent = bootstrap();
    let pgrp = parent.pgrp_cap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    assert_eq!(pgrp.member_slot_count(), 2); // parent + child

    step_exit_group(&child, ExitStatus::Exited(0));

    // Per PROCESS_v1 §8.5: zombies stay in pgrp.members until reap.
    // The Weak still upgrades because child Cap is still held by the
    // test. (member_slot_count is the raw slot count incl. stale.)
    assert_eq!(pgrp.member_slot_count(), 2);

    let child_pid = child.pid;
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap");

    // Reap withdraws from pgrp.members.
    assert_eq!(pgrp.member_slot_count(), 1, "only parent remains in pgrp");
}

#[test]
fn waitpid_reap_returns_signaled_status() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_pid = child.pid;

    // Child exits via signal — recorded as ExitStatus::Signaled.
    step_exit_group_with_signal(&child, crate::signal::Signum::SIGTERM);
    drop(child);

    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid));
    assert_eq!(
        result,
        Ok((
            child_pid,
            ExitStatus::Signaled(crate::signal::Signum::SIGTERM)
        ))
    );
}

#[test]
fn waitpid_after_reaping_all_children_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    step_exit_group(&child, ExitStatus::Exited(0));
    let child_pid = child.pid;
    drop(child);

    let _ = step_waitpid_nohang(&parent, WaitTarget::Any).expect("first reap");

    // Children list now empty; second wait returns NoChildren.
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Err(WaitError::NoChildren));
    let _ = child_pid; // silence unused-var (kept for narrative)
}

// ----- VFS cwd integration (chdir + getcwd) -----

/// Mint a fresh `RNode` with a small anon page container backing.
/// The RNode's content doesn't matter for cwd-render tests; we just
/// need *some* `Cap<RNode>` to attach to a `DEntry`.
fn fresh_rnode(fs_object_id: u64) -> Cap<RNode> {
    use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Persistent,
        },
        1,
    )
    .expect("page container");
    RNode::new_cap(
        FsObjectId::new(fs_object_id),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode")
}

/// Build a root `DEntry` (parent=None, name=ROOT). Synthetic — no
/// real filesystem behind it.
fn fresh_root_dentry() -> Cap<DEntry> {
    DEntry::new_cap(InlineName::ROOT, fresh_rnode(1)).expect("root dentry")
}

/// Build a child `DEntry` named `name`, with `parent` as its parent
/// hint. Mutates the new dentry to install the parent_hint via the
/// payload's `with_*` mutator pattern — but DEntry doesn't expose
/// such a mutator publicly post-allocation. We instead construct an
/// unallocated DEntry with `set_parent_hint`, then sign into the zone.
fn fresh_dentry_under(parent: &Cap<DEntry>, name: &[u8], fs_id: u64) -> Cap<DEntry> {
    let inline = InlineName::new(name).expect("name");
    let mut raw = DEntry::new(inline, fresh_rnode(fs_id));
    raw.set_parent_hint(parent);
    let res = tx_substrate::zone::reserve_for::<DEntry>().expect("dentry slot");
    tx_substrate::zone::sign_for(res, raw)
}

#[test]
fn getcwd_on_process_with_no_cwd_returns_none() {
    let _g = setup();
    let init = bootstrap();
    // bootstrap_init_process leaves cwd unset on day-1.
    assert_eq!(step_getcwd(&init), None);
}

#[test]
fn chdir_then_getcwd_renders_root_path() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();

    let outcome = step_chdir(&init, root.clone());
    assert!(matches!(outcome, ChdirOutcome::Replaced { prev: None }));

    let path = step_getcwd(&init).expect("path renders");
    assert_eq!(path.as_slice(), b"/");
}

#[test]
fn chdir_then_getcwd_renders_nested_path() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 100);
    let bin = fresh_dentry_under(&usr, b"bin", 101);

    step_chdir(&init, bin);
    let path = step_getcwd(&init).expect("nested path");
    assert_eq!(path.as_slice(), b"/usr/bin");
}

#[test]
fn chdir_returns_previous_cwd_in_replaced() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 200);

    step_chdir(&init, root);
    let outcome = step_chdir(&init, usr);
    match outcome {
        ChdirOutcome::Replaced { prev: Some(prev) } => {
            assert!(prev.name().is_empty(), "previous cwd was the root marker");
        }
        other => panic!("expected Replaced{{Some}}, got {other:?}"),
    }
}

#[test]
fn chdir_on_zombie_returns_zombie_ignored() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();

    step_exit_group(&init, ExitStatus::Exited(0));
    assert!(init.is_zombie());

    let outcome = step_chdir(&init, root);
    assert!(matches!(outcome, ChdirOutcome::ZombieIgnored));
}

#[test]
fn fork_inherits_parent_cwd() {
    let _g = setup();
    let parent = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 300);
    step_chdir(&parent, usr.clone());

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Child's cwd renders to the same path as parent's.
    let parent_path = step_getcwd(&parent).expect("parent path");
    let child_path = step_getcwd(&child).expect("child path");
    assert_eq!(parent_path, child_path);
    assert_eq!(child_path.as_slice(), b"/usr");
}

#[test]
fn parent_chdir_after_fork_does_not_affect_child() {
    let _g = setup();
    let parent = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 400);
    let var = fresh_dentry_under(&root, b"var", 401);

    step_chdir(&parent, usr);
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Parent moves to /var; child's cwd should still be /usr (it
    // got its own Cap<DEntry> at fork time pointing at /usr).
    step_chdir(&parent, var);

    assert_eq!(step_getcwd(&parent).unwrap().as_slice(), b"/var");
    assert_eq!(step_getcwd(&child).unwrap().as_slice(), b"/usr");
}

// ----- waitpid pgrp selectors (PROCESS_v1 §7.4) -----

#[test]
fn waitpid_caller_pgrp_reaps_zombie_in_callers_pgroup() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    // Child inherits parent's pgrp at fork. Both share parent.pgrp.

    step_exit_group(&child, ExitStatus::Exited(5));
    let child_pid = child.pid;
    drop(child);

    let result = step_waitpid_nohang(&parent, WaitTarget::CallerPgrp);
    assert_eq!(result, Ok((child_pid, ExitStatus::Exited(5))));
}

#[test]
fn waitpid_caller_pgrp_skips_child_in_other_pgroup() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Move child out of parent's pgrp into its own.
    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");
    assert_ne!(child.pgrp_cap().pgid, parent.pgrp_cap().pgid);

    step_exit_group(&child, ExitStatus::Exited(0));

    // CallerPgrp matches only children in caller's pgrp; child is no
    // longer there, so NoChildren — no matching children at all.
    let result = step_waitpid_nohang(&parent, WaitTarget::CallerPgrp);
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_pgrp_selector_matches_specific_pgid() {
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent).expect("c1");
    let c2 = step_fork::<TestPmap>(&parent).expect("c2");

    // Move c2 to its own pgrp.
    step_setpgid(&c2, Pgid(c2.pid.0)).expect("setpgid");
    let c2_pgid = c2.pgrp_cap().pgid;

    step_exit_group(&c1, ExitStatus::Exited(1));
    step_exit_group(&c2, ExitStatus::Exited(2));
    let c2_pid = c2.pid;
    drop(c1);
    drop(c2);

    // Pgrp(c2_pgid) matches only c2.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(c2_pgid));
    assert_eq!(result, Ok((c2_pid, ExitStatus::Exited(2))));

    // c1 is still reapable via Any (different pgrp).
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert!(result.is_ok());
}

#[test]
fn waitpid_pgrp_selector_with_no_matching_pgid_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent).expect("fork");

    // Pgid that no child belongs to.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(Pgid(9999)));
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_pgrp_selector_with_live_match_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_pgid = child.pgrp_cap().pgid;

    // Live child in target pgrp — selector matches but no zombie.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(child_pgid));
    assert_eq!(result, Err(WaitError::NoneReady));
}

// ----- SIGCHLD producer (PROCESS_v1 §7.3.3 phase 5) -----

fn leader_has_sigchld_pending(proc_cap: &Cap<ProcessIdentity>) -> bool {
    let payload = proc_cap.payload.lock();
    let leader = payload.as_ref().expect("alive").threads.lock()[0].clone();
    drop(payload);
    let leader_payload = leader.payload.lock();
    leader_payload
        .as_ref()
        .expect("alive")
        .pending()
        .is_pending(Signum::SIGCHLD)
}

#[test]
fn child_exit_via_step_exit_group_posts_sigchld_to_parent() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    assert!(!leader_has_sigchld_pending(&parent));

    step_exit_group(&child, ExitStatus::Exited(0));

    assert!(
        leader_has_sigchld_pending(&parent),
        "parent's leader thread should have SIGCHLD pending after child zombifies"
    );
}

#[test]
fn child_exit_via_last_thread_cascade_posts_sigchld_to_parent() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_leader = first_thread(&child);

    step_thread_exit(child_leader, 7);
    assert!(child.is_zombie());

    assert!(
        leader_has_sigchld_pending(&parent),
        "parent should see SIGCHLD via last-thread cascade too"
    );
}

#[test]
fn bootstrap_init_exit_does_not_panic_with_no_parent() {
    // Init has no parent. SIGCHLD post must short-circuit cleanly.
    let _g = setup();
    let init = bootstrap();
    step_exit_group(&init, ExitStatus::Exited(0));
    // Just exercising the code path; assertion is "didn't panic".
    assert!(init.is_zombie());
}

#[test]
fn orphaned_child_exit_does_not_post_sigchld() {
    // Parent exits first (severs child); child later exits. With no
    // parent slot to upgrade, the SIGCHLD producer skips silently.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    step_exit_group(&parent, ExitStatus::Exited(0));
    assert!(child.parent_cap().is_none(), "severed by parent's exit");

    // Now child exits. No parent to receive SIGCHLD; should not
    // panic or error.
    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());
}

#[test]
fn zombie_parent_does_not_receive_sigchld() {
    // If the parent has somehow zombified before the child does
    // (without severing — pathological in spec terms but a defensive
    // shape worth covering), the SIGCHLD producer's underlying
    // step_kill_process returns NoLiveThread and we discard.
    //
    // Note: real flows always sever children at parent exit, so this
    // test simulates the pathological window by manually clearing
    // children before exiting parent (preventing sever from running
    // on the child) — the child keeps a stale parent Weak that
    // upgrades to a zombie identity.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Manually clear parent.children so sever doesn't run on the child.
    parent.children.lock().clear();
    step_exit_group(&parent, ExitStatus::Exited(0));
    assert!(parent.is_zombie());
    // child still has parent slot pointing at the (now zombie) parent.
    assert!(child.parent_cap().is_some());

    // Child exit invokes post_sigchld_to_parent which calls
    // step_kill_process on a zombie target: NoLiveThread, discarded.
    // No panic.
    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());
}

// ----- INIT_PROCESS slot + reparent-to-init (PROCESS_v1 §8.1) -----

#[test]
fn init_process_handle_returns_none_before_bootstrap() {
    let _g = setup();
    // setup() resets INIT_PROCESS; nothing has bootstrapped yet.
    assert!(init_process().is_none());
}

#[test]
fn init_process_handle_returns_some_after_bootstrap() {
    let _g = setup();
    let init = bootstrap();

    let handle = init_process().expect("init_process registered");
    assert_eq!(handle.pid, init.pid);
    // Same identity by Cap key.
    assert_eq!(handle.key(), init.key());
}

#[test]
fn bootstrap_init_process_errors_if_already_bootstrapped() {
    let _g = setup();
    let _first = bootstrap();

    // Second call without reset.
    let result = bootstrap_init_process(fresh_aspace());
    assert!(matches!(result, Err(BootstrapError::AlreadyBootstrapped)));
}

#[test]
fn non_init_parent_exit_reparents_children_to_init() {
    // init → middle → leaf chain. middle exits. Per §8.1, leaf
    // should reparent to init.
    let _g = setup();
    let init = bootstrap();
    let middle = step_fork::<TestPmap>(&init).expect("fork middle");
    let leaf = step_fork::<TestPmap>(&middle).expect("fork leaf");

    assert_eq!(leaf.parent_pid(), middle.pid);
    let init_children_before = init.child_count();

    step_exit_group(&middle, ExitStatus::Exited(0));

    // leaf's parent now points at init.
    assert_eq!(leaf.parent_pid(), init.pid);
    assert_eq!(leaf.parent_cap().expect("init retained").key(), init.key());
    // init's children list grew by one (received leaf).
    assert_eq!(init.child_count(), init_children_before + 1);
    // middle's children list is now empty.
    assert_eq!(middle.child_count(), 0);
}

#[test]
fn init_exit_severs_children_without_reparent_target() {
    // When init itself exits, sever_children's "init is the exiting
    // process" branch fires — children get parent=None rather than
    // being reparented to themselves.
    let _g = setup();
    let init = bootstrap();
    let child = step_fork::<TestPmap>(&init).expect("fork");

    step_exit_group(&init, ExitStatus::Exited(0));

    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

// ----- children container (PROCESS_v1 §2.1 + §8.1) -----

#[test]
fn bootstrap_init_has_no_children() {
    let _g = setup();
    let init = bootstrap();

    assert_eq!(init.child_count(), 0);
    assert!(init.children().is_empty());
}

#[test]
fn fork_pushes_child_into_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();
    assert_eq!(parent.child_count(), 0);

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    assert_eq!(parent.child_count(), 1);
    let live = parent.children();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].pid, child.pid);
}

#[test]
fn multiple_forks_accumulate_in_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();

    let c1 = step_fork::<TestPmap>(&parent).expect("fork 1");
    let c2 = step_fork::<TestPmap>(&parent).expect("fork 2");
    let c3 = step_fork::<TestPmap>(&parent).expect("fork 3");

    assert_eq!(parent.child_count(), 3);
    let live_pids: alloc::collections::BTreeSet<_> =
        parent.children().iter().map(|c| c.pid).collect();
    assert_eq!(live_pids.len(), 3);
    assert!(live_pids.contains(&c1.pid));
    assert!(live_pids.contains(&c2.pid));
    assert!(live_pids.contains(&c3.pid));
}

#[test]
fn dropping_test_child_cap_leaves_parent_children_list_intact() {
    // Per §8.5 + §2.1: parent.children retains children with strong
    // Caps until reap. Dropping the test's external Cap on a child
    // does NOT reclaim the child — parent.children still holds a
    // strong ref. Children leave the list only via waitpid reap or
    // when the parent itself reclaims.
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent).expect("fork 1");
    let _c2 = step_fork::<TestPmap>(&parent).expect("fork 2");

    assert_eq!(parent.child_count(), 2);

    // Drop one external Cap. Parent's children list retains both.
    drop(c1);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);

    assert_eq!(
        parent.child_count(),
        2,
        "parent retains child via Cap; drop of external retainer is not enough"
    );
    assert_eq!(parent.children().len(), 2);
}

#[test]
fn parent_exit_via_step_exit_group_severs_children_parent_slot() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    assert_eq!(child.parent_pid(), parent.pid);

    step_exit_group(&parent, ExitStatus::Exited(0));

    // Per PROCESS_v1 §8.1 (day-1 stub): child's parent slot is
    // severed. Future reparent-to-init lands when init is globally
    // addressable.
    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

#[test]
fn parent_exit_via_last_thread_cascade_severs_children_parent_slot() {
    // Last-thread cascade exits via step_thread_exit →
    // step_process_exit, not via step_exit_group. Verify both paths
    // sever children.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let parent_leader = first_thread(&parent);

    step_thread_exit(parent_leader, 7);

    assert!(parent.is_zombie());
    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

#[test]
fn child_severance_does_not_affect_grandchildren() {
    // Sever is shallow: parent's exit only severs its direct
    // children. Grandchildren keep their parent (the now-orphaned
    // child).
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let grandchild = step_fork::<TestPmap>(&child).expect("fork-of-child");

    assert_eq!(grandchild.parent_pid(), child.pid);

    step_exit_group(&parent, ExitStatus::Exited(0));

    // child's parent severed (None).
    assert!(child.parent_cap().is_none());
    // grandchild's parent unchanged — still child.
    assert_eq!(grandchild.parent_pid(), child.pid);
}

#[test]
fn step_exit_group_does_not_set_terminating_signal() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group(&proc_cap, ExitStatus::Exited(7));

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(7)));
    assert_eq!(proc_cap.terminating_signal(), None);
}

#[test]
fn process_payload_aspace_atomic_replace_returns_previous_cap() {
    // Per `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`,
    // exec swaps `ProcessPayload.aspace` atomically and the previous
    // `Cap<AddressSpace>` is returned for EBR-deferred drop. Verify
    // the slot semantics: after `replace_aspace(new)`, `aspace_cap`
    // returns the new `Cap`, and the previous `Cap` is the value we
    // started with.
    let _g = setup();
    let proc_cap = bootstrap();
    let initial = proc_cap.aspace_cap().expect("alive aspace");
    let initial_key = initial.key();

    let replacement = fresh_aspace();
    let replacement_key = replacement.key();

    let prev = proc_cap
        .replace_aspace(replacement)
        .expect("replace returns previous");

    assert_eq!(prev.key(), initial_key, "replace returns the original");
    let post = proc_cap.aspace_cap().expect("still alive");
    assert_eq!(
        post.key(),
        replacement_key,
        "live aspace is the replacement"
    );
    assert_ne!(post.key(), initial_key);
}

// ----- Wave 2 ELF loader plan: per-fd CLOEXEC bitmap + exec phase-7 -----
//
// Process-side tests for the Wave 2 deliverables:
// - `ProcessPayload.fd_cloexec` storage (default 0; set/clear round trip).
// - `step_fork` clones parent's CLOEXEC bits (Linux semantics).
// - `step_close_cloexec_fds` (P1) closes only marked fds and clears the
//   bitmap.
// - `step_install_brk_for_exec` (P3) overwrites both `brk_base` and
//   `current_brk`.

/// Helper: synthesise an `OpenFile` `Cap` over a regular-file RNode so
/// fd-table tests can install slots without standing up a TTY/devfs.
fn fresh_open_file() -> Cap<crate::vfs::OpenFile> {
    use crate::vfs::OpenFileFlags;
    let rnode = fresh_rnode(7777);
    crate::vfs::OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
    .expect("open file cap")
}

#[test]
fn process_payload_fd_cloexec_default_zero() {
    let _g = setup();
    let proc_cap = bootstrap();
    // Every fd defaults to "not CLOEXEC" — bootstrap_init_process
    // initialises the CLOEXEC set empty per the Wave 2 plan (init's
    // stdio is not close-on-exec by Linux convention).
    //
    // fd-ops Wave 1: assert via the snapshot accessor that the set
    // really is empty; the previous AtomicU32 word check is gone.
    assert!(
        proc_cap.fd_cloexec_snapshot().is_empty(),
        "bootstrap CLOEXEC set must start empty"
    );
    for fd in 0u32..16 {
        assert!(!proc_cap.fd_cloexec(fd), "fd {fd} should default to false");
    }
    // fd-ops Wave 1: any `u32` is a valid fd key (the BTreeSet has no
    // upper bound). Pre-Wave-1 the AtomicU32 capped at fd 31; the
    // sparse set lifts that.
    assert!(!proc_cap.fd_cloexec(31));
    assert!(!proc_cap.fd_cloexec(32));
    assert!(!proc_cap.fd_cloexec(64));
}

#[test]
fn process_payload_set_fd_cloexec_round_trip() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd_cloexec(3, true);
    assert!(proc_cap.fd_cloexec(3));
    assert!(!proc_cap.fd_cloexec(2));
    assert!(!proc_cap.fd_cloexec(4));

    // Toggling another bit must not perturb the first.
    proc_cap.set_fd_cloexec(5, true);
    assert!(proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(5));

    // Clearing fd 3 leaves fd 5 alone.
    proc_cap.set_fd_cloexec(3, false);
    assert!(!proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(5));

    // fd-ops Wave 1: large fd values (> 31) are now legal — the
    // sparse `BTreeSet<u32>` has no upper bound. Pre-Wave-1 the
    // AtomicU32 silently dropped these.
    proc_cap.set_fd_cloexec(100, true);
    assert!(proc_cap.fd_cloexec(100));
    assert!(proc_cap.fd_cloexec(5));
    proc_cap.set_fd_cloexec(100, false);
    assert!(!proc_cap.fd_cloexec(100));
}

// ---------------------------------------------------------------------------
// fd-ops Wave 1 (2026-05-07): sparse `BTreeMap`/`BTreeSet` fd table.
// ---------------------------------------------------------------------------

/// fd-ops Wave 1: any `u32` fd is a valid key in the sparse
/// `BTreeMap<u32, Cap<OpenFile>>` table. Pre-Wave-1 the table was a
/// fixed `[Option<Cap<OpenFile>>; 8]` array and the install at fd 100
/// would have been silently dropped.
#[test]
fn process_payload_fds_btreemap_supports_sparse_fd_above_31() {
    let _g = setup();
    let proc_cap = bootstrap();

    let file = fresh_open_file();
    let prev = proc_cap.set_fd(100, Some(file));
    assert!(prev.is_none(), "fd 100 was not previously occupied");

    assert!(proc_cap.fd(100).is_some(), "fd 100 must be observable");
    // No other fds occupy.
    for fd in [0u32, 1, 2, 3, 7, 31, 32, 99, 101, 200] {
        assert!(
            proc_cap.fd(fd).is_none(),
            "fd {fd} must be empty (only fd 100 was set)"
        );
    }

    let removed = proc_cap.set_fd(100, None);
    assert!(removed.is_some(), "removing fd 100 returns the prior file");
    assert!(
        proc_cap.fd(100).is_none(),
        "fd 100 must be empty post-remove"
    );
}

/// fd-ops Wave 1: `allocate_fd` returns the lowest unused fd ≥ 0.
/// Walks the BTreeMap's sorted keys looking for the first gap.
#[test]
fn process_payload_allocate_fd_returns_lowest_unused() {
    let _g = setup();
    let proc_cap = bootstrap();

    // Empty table: lowest unused fd is 0.
    assert_eq!(proc_cap.allocate_fd(), 0);

    // Install fd 0 and fd 2 — leaving fd 1 as the gap.
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    assert_eq!(proc_cap.allocate_fd(), 1, "fd 1 is the lowest gap");

    // Plug the gap; lowest unused fd shifts to 3.
    proc_cap.set_fd(1, Some(fresh_open_file()));
    assert_eq!(proc_cap.allocate_fd(), 3);

    // `next_fd_above` is the same scan with a non-zero floor.
    assert_eq!(proc_cap.next_fd_above(2), 3);
    assert_eq!(proc_cap.next_fd_above(10), 10);
}

/// fd-ops Wave 1: `install_fd` returns the previous occupant so the
/// caller can EBR-defer-drop the displaced `Cap<OpenFile>`. Matches
/// the `dup2`/`dup3` shape (Wave 4).
#[test]
fn process_payload_install_fd_returns_previous_occupant() {
    let _g = setup();
    let proc_cap = bootstrap();

    let first = fresh_open_file();
    let second = fresh_open_file();

    // First install: slot was empty.
    let prev1 = proc_cap.install_fd(5, first);
    assert!(
        prev1.is_none(),
        "first install at fd 5 has no prior occupant"
    );

    // Second install at the same fd: the first occupant returns.
    let prev2 = proc_cap.install_fd(5, second);
    assert!(
        prev2.is_some(),
        "second install at fd 5 must return the first occupant"
    );

    assert!(proc_cap.fd(5).is_some(), "fd 5 must remain installed");
}

/// fd-ops Wave 1: `step_fork`'s fd-table clone walks the parent's
/// `BTreeMap` entries (sparse fds included), not a 0..8 array index
/// loop. The child sees every parent fd, including fds > 31.
#[test]
fn process_payload_step_fork_clones_sparse_fd_table() {
    let _g = setup();
    let parent = bootstrap();

    // Parent's fd table: fds 0, 1, 2 (the canonical stdio shape) plus
    // fd 100 (the sparse case the BTreeMap migration unlocks).
    parent.set_fd(0, Some(fresh_open_file()));
    parent.set_fd(1, Some(fresh_open_file()));
    parent.set_fd(2, Some(fresh_open_file()));
    parent.set_fd(100, Some(fresh_open_file()));

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Child inherits the entire sparse map.
    assert!(child.fd(0).is_some(), "child inherits fd 0");
    assert!(child.fd(1).is_some(), "child inherits fd 1");
    assert!(child.fd(2).is_some(), "child inherits fd 2");
    assert!(child.fd(100).is_some(), "child inherits sparse fd 100");
    assert!(child.fd(3).is_none(), "fd 3 was never set; child sees None");

    // Mutating the child must not bleed back into the parent.
    child.set_fd(100, None);
    assert!(child.fd(100).is_none());
    assert!(
        parent.fd(100).is_some(),
        "parent's fd 100 survives the child's close"
    );
}

/// fd-ops Wave 1: the CLOEXEC `BTreeSet<u32>` accepts arbitrary `u32`
/// keys — pre-Wave-1 the `AtomicU32` silently dropped fds ≥ 32.
#[test]
fn process_payload_fd_cloexec_btreeset_supports_sparse_fds_above_31() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd_cloexec(100, true);
    assert!(proc_cap.fd_cloexec(100));
    assert!(!proc_cap.fd_cloexec(99));
    assert!(!proc_cap.fd_cloexec(101));

    // Snapshot reflects only the high fd.
    let snap = proc_cap.fd_cloexec_snapshot();
    assert_eq!(snap.len(), 1);
    assert!(snap.contains(&100));

    proc_cap.set_fd_cloexec(100, false);
    assert!(!proc_cap.fd_cloexec(100));
    assert!(proc_cap.fd_cloexec_snapshot().is_empty());
}

#[test]
fn step_fork_clones_fd_cloexec_bits() {
    let _g = setup();
    let parent = bootstrap();
    parent.set_fd_cloexec(1, true);
    parent.set_fd_cloexec(4, true);

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // Child inherits parent's snapshot at fork time.
    assert!(child.fd_cloexec(1));
    assert!(child.fd_cloexec(4));
    assert!(!child.fd_cloexec(0));
    assert!(!child.fd_cloexec(2));

    // Mutating the child must not bleed back into the parent.
    child.set_fd_cloexec(2, true);
    assert!(child.fd_cloexec(2));
    assert!(!parent.fd_cloexec(2));

    // Mutating the parent post-fork must not bleed into the child.
    parent.set_fd_cloexec(0, true);
    assert!(parent.fd_cloexec(0));
    assert!(!child.fd_cloexec(0));
}

#[test]
fn step_close_cloexec_fds_closes_marked_fds_clears_others() {
    use crate::process::execution::step_close_cloexec_fds;
    let _g = setup();
    let proc_cap = bootstrap();

    // Install three open files at fds 0, 1, 2; mark only fd 1 as
    // CLOEXEC.
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(1, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    proc_cap.set_fd_cloexec(1, true);

    step_close_cloexec_fds(&proc_cap);

    // Only fd 1 should be closed; the others remain.
    assert!(proc_cap.fd(0).is_some(), "fd 0 was not marked; survives");
    assert!(
        proc_cap.fd(1).is_none(),
        "fd 1 was marked CLOEXEC; should be closed"
    );
    assert!(proc_cap.fd(2).is_some(), "fd 2 was not marked; survives");
}

#[test]
fn step_close_cloexec_fds_clears_bitmap_after() {
    use crate::process::execution::step_close_cloexec_fds;
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd(2, Some(fresh_open_file()));
    proc_cap.set_fd_cloexec(2, true);
    assert!(proc_cap.fd_cloexec(2));

    step_close_cloexec_fds(&proc_cap);

    // The sweep clears the set wholesale: future fcntl(F_SETFD) calls
    // start from a clean state.
    assert!(
        !proc_cap.fd_cloexec(2),
        "post-sweep, the CLOEXEC bit must be cleared"
    );
    assert!(
        proc_cap.fd_cloexec_snapshot().is_empty(),
        "post-sweep, the CLOEXEC set must be empty"
    );
}

#[test]
fn step_install_brk_for_exec_resets_both_brk_base_and_current() {
    use crate::process::execution::{step_install_brk_for_exec, BOOTSTRAP_BRK_BASE};
    let _g = setup();
    let proc_cap = bootstrap();

    // Bootstrap state: both brk_base and current_brk seeded to the
    // same bootstrap value (per the existing
    // `bootstrap_init_process` contract).
    assert_eq!(proc_cap.brk_base(), BOOTSTRAP_BRK_BASE);
    assert_eq!(proc_cap.current_brk(), BOOTSTRAP_BRK_BASE);

    // Simulate a userspace brk(2) advance so current_brk diverges
    // from brk_base — this is the "running process" state exec
    // takes over.
    proc_cap.set_current_brk(BOOTSTRAP_BRK_BASE + 0x1000);
    assert_eq!(proc_cap.current_brk(), BOOTSTRAP_BRK_BASE + 0x1000);
    assert_eq!(proc_cap.brk_base(), BOOTSTRAP_BRK_BASE);

    // Install fresh exec-image brk: both fields rewritten to the
    // same new value (per `txdoc:EXEC-12-4-INSTALL-BRK`).
    let new_brk: u64 = 0xb000_0000;
    step_install_brk_for_exec(&proc_cap, new_brk);

    assert_eq!(proc_cap.brk_base(), new_brk);
    assert_eq!(proc_cap.current_brk(), new_brk);
}

// ----- Wave 1 fork/clone/wait4 slice (2026-05-06) -----
//
// Tests for the kernel-side prerequisites Wave 2's `sys_clone` and
// `sys_wait4` syscall arms will consume:
//   - `seed_child_leader_context` (Part 1A): the syscall driver
//     helper that stamps `regs[10] = 0` (RV64 a0) and `pc + 4`
//     onto the child leader thread's saved trap context.
//   - `ProcessPayload.exit_port` (Part 1B): the per-process wait
//     channel that fires on child zombification, so a parent
//     parked on `sys_wait4` wakes when any child exits.
//   - POSIX `wait_status_word` migration (Open Q #3 DECIDED):
//     `(code & 0xff) << 8` for explicit exits and `sig & 0x7f` for
//     signal exits.

mod seed_child_leader_context {
    //! Tests for [`crate::process::seed_child_leader_context`].

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
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let leader = child_leader(&child);

        let parent_ctx = synthetic_parent_ctx();
        seed_child_leader_context(&leader, &parent_ctx);

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
    }

    #[test]
    fn seed_child_leader_context_skips_ecall() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let leader = child_leader(&child);

        let parent_ctx = synthetic_parent_ctx();
        seed_child_leader_context(&leader, &parent_ctx);

        let saved = leader
            .payload_cap()
            .expect("fresh child leader has payload")
            .saved_user_context()
            .expect("seed installs Some");
        assert_eq!(
            saved.pc,
            parent_ctx.pc + 4,
            "RV64 ecall is 4 bytes; child resumes after the trapping ecall, not at it",
        );
    }

    #[test]
    fn seed_child_leader_context_preserves_other_gprs_and_sp() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let leader = child_leader(&child);

        let parent_ctx = synthetic_parent_ctx();
        seed_child_leader_context(&leader, &parent_ctx);

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
}

mod exit_port {
    //! Tests for the per-process `exit_port` wait carrier.

    use super::*;
    use crate::process::structure::EXIT_PORT_CHILD_ZOMBIFIED;

    #[test]
    fn process_payload_exit_port_default_constructed_and_registered() {
        let _g = setup();
        let init = bootstrap();

        let carrier_id = init
            .exit_port_carrier_id()
            .expect("live init has an exit_port carrier id");
        // Must be a non-zero, registered id.
        assert!(carrier_id != 0);
        assert!(crate::wait_carrier::lookup_wait_channel(carrier_id).is_some());

        // Exit_port wait token is built from the carrier id + the
        // child-zombified bit.
        let token = init
            .exit_port_wait_token()
            .expect("live init has an exit_port wait token");
        assert_eq!(token.carrier(), carrier_id);
        assert_eq!(token.interest(), EXIT_PORT_CHILD_ZOMBIFIED);
    }

    #[test]
    fn post_sigchld_to_parent_fires_exit_port() {
        use tx_reactor::wait::Mask;
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");

        // Park a wait future on the parent's exit_port BEFORE the
        // child zombifies so the fire site has an awaiter to release.
        let token = parent
            .exit_port_wait_token()
            .expect("live parent has token");
        let channel = crate::wait_carrier::lookup_wait_channel(token.carrier())
            .expect("registered carrier resolves");
        let mut wait = channel.wait(Mask::from_bits(token.interest()));

        // Drive a single poll to register the awaiter.
        use core::future::Future;
        use core::pin::Pin;
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        let raw = RawWaker::new(core::ptr::null(), &VTABLE);
        // SAFETY: vtable functions are no-ops.
        let waker = unsafe { Waker::from_raw(raw) };
        let mut cx = Context::from_waker(&waker);
        let pre = Pin::new(&mut wait).poll(&mut cx);
        assert!(matches!(pre, Poll::Pending), "no fires yet → Pending");

        // Zombify the child via step_exit_group; this calls
        // post_sigchld_to_parent which fires the parent's exit_port.
        step_exit_group(&child, ExitStatus::Exited(0));

        // The awaiter should now resolve on next poll.
        let post = Pin::new(&mut wait).poll(&mut cx);
        assert!(
            matches!(post, Poll::Ready(_)),
            "exit_port fire on zombification should release the parked awaiter",
        );
    }

    #[test]
    fn step_fork_clones_init_with_fresh_exit_port() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");

        let parent_id = parent.exit_port_carrier_id().expect("parent live");
        let child_id = child.exit_port_carrier_id().expect("child live");
        assert_ne!(
            parent_id, child_id,
            "each ProcessPayload gets its own Channel + carrier id",
        );

        // Both carriers must resolve through the registry.
        assert!(crate::wait_carrier::lookup_wait_channel(parent_id).is_some());
        assert!(crate::wait_carrier::lookup_wait_channel(child_id).is_some());
    }

    #[test]
    fn zombie_process_exit_port_carrier_id_is_none() {
        let _g = setup();
        let init = bootstrap();
        step_exit_group(&init, ExitStatus::Exited(0));
        assert!(init.is_zombie());

        // After zombification the payload is gone; the carrier id is
        // unobservable through the identity accessor (the underlying
        // registry slot may still hold the channel — see
        // Cross-cutting Risk #1 in the slice plan; that's a Wave 2+
        // cleanup item).
        assert!(init.exit_port_carrier_id().is_none());
        assert!(init.exit_port_wait_token().is_none());
        // fire_exit_port on a zombie is a no-op (returns 0).
        assert_eq!(
            init.fire_exit_port(tx_reactor::wait::Mask::from_bits(EXIT_PORT_CHILD_ZOMBIFIED)),
            0,
        );
    }
}

mod posix_wait_status_word {
    //! POSIX `<sys/wait.h>` migration tests for
    //! [`crate::process::ExitStatus::wait_status_word`].

    use super::*;

    #[test]
    fn exit_status_wait_status_word_exited_zero_encodes_zero() {
        // `(0 & 0xff) << 8 == 0` — `WIFEXITED(0) == 1`,
        // `WEXITSTATUS(0) == 0`.
        let s = ExitStatus::Exited(0).wait_status_word();
        assert_eq!(s, 0);
        // WIFEXITED predicate.
        assert_eq!(s & 0x7f, 0);
        // WEXITSTATUS extractor.
        assert_eq!((s >> 8) & 0xff, 0);
    }

    #[test]
    fn exit_status_wait_status_word_exited_42_encodes_0x2a00() {
        // `(42 & 0xff) << 8 == 0x2a00`. WIFEXITED == 1,
        // WEXITSTATUS == 42.
        let s = ExitStatus::Exited(42).wait_status_word();
        assert_eq!(s, 0x2a00);
        assert_eq!(s & 0x7f, 0, "WIFEXITED predicate");
        assert_eq!((s >> 8) & 0xff, 42, "WEXITSTATUS extractor");
    }

    #[test]
    fn exit_status_wait_status_word_terminated_by_sigkill_encodes_0x09() {
        // SIGKILL = 9; signaled-exit encoding = sig & 0x7f.
        let s = ExitStatus::Signaled(Signum::SIGKILL).wait_status_word();
        assert_eq!(s, 9);
        // WIFSIGNALED predicate: low 7 bits non-zero and != 0x7f.
        let low = s & 0x7f;
        assert!(low > 0 && low < 0x7f, "WIFSIGNALED predicate");
        // WTERMSIG extractor.
        assert_eq!(s & 0x7f, 9, "WTERMSIG extractor");
    }
}
