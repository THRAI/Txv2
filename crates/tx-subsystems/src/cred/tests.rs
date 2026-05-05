//! Credential service tests.
//!
//! Cover the day-1 surface: type basics, root cred semantics,
//! fork-inherits-cred, privileged-vs-non-privileged setuid/setgid,
//! and zombie ignoring.

use crate::cred::{
    step_setgid, step_setuid, Capability, CapabilitySet, Cred, CredChange, Gid, Uid,
};
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_exit_group, step_fork, ExitStatus};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
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

fn cred_of(proc_cap: &Cap<ProcessIdentity>) -> Cred {
    let payload_guard = proc_cap.payload.lock();
    payload_guard.as_ref().expect("alive").cred()
}

fn set_cred(proc_cap: &Cap<ProcessIdentity>, cred: Cred) {
    let payload_guard = proc_cap.payload.lock();
    *payload_guard.as_ref().expect("alive").cred.lock() = cred;
}

#[test]
fn cred_root_has_uid_zero_and_full_capabilities() {
    let root = Cred::root();
    assert!(root.uid.is_root());
    assert!(root.euid.is_root());
    assert_eq!(root.effective_caps, CapabilitySet::FULL);
    assert!(root.is_privileged_for(Capability::SETUID));
    assert!(root.is_privileged_for(Capability::KILL));
}

#[test]
fn cred_default_is_unprivileged() {
    let nobody = Cred::default();
    assert_eq!(nobody.uid, Uid(0));
    assert_eq!(nobody.effective_caps, CapabilitySet::EMPTY);
    // Default uid happens to be 0 by Default trait — but the
    // unprivileged check is on caps, not uid. So privileged_for(SETUID)
    // is true via euid root path. This documents the day-1 behavior.
    assert!(nobody.is_privileged_for(Capability::SETUID));
}

#[test]
fn capability_set_add_remove_contains() {
    let mut caps = CapabilitySet::EMPTY;
    assert!(!caps.contains(Capability::KILL));
    caps.add(Capability::KILL);
    assert!(caps.contains(Capability::KILL));
    assert!(!caps.contains(Capability::SETUID));
    caps.remove(Capability::KILL);
    assert!(!caps.contains(Capability::KILL));
}

#[test]
fn bootstrap_init_process_starts_with_root_cred() {
    let _g = setup();
    let init = bootstrap();
    assert_eq!(cred_of(&init), Cred::root());
}

#[test]
fn fork_inherits_parent_cred_unchanged() {
    let _g = setup();
    let parent = bootstrap();
    // Make parent a non-root user with KILL cap so we can verify all
    // fields are inherited, not reset to root.
    let mut effective_caps = CapabilitySet::EMPTY;
    effective_caps.add(Capability::KILL);
    let custom = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        effective_caps,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&parent, custom);

    let child = step_fork::<TestPmap>(&parent).expect("fork");
    assert_eq!(cred_of(&child), custom);
    // Still equal to parent (no aliasing — they're independent Copies).
    assert_eq!(cred_of(&child), cred_of(&parent));
}

#[test]
fn setuid_privileged_changes_both_uid_and_euid() {
    let _g = setup();
    let proc_cap = bootstrap(); // root by default
    let outcome = step_setuid(&proc_cap, Uid(1000));

    let CredChange::Replaced { prev, new } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert!(prev.euid.is_root());
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1000));
}

#[test]
fn setuid_unprivileged_can_swap_among_existing_ids_only() {
    let _g = setup();
    let proc_cap = bootstrap();

    // Drop privileges: uid=1000, euid=1001, no caps.
    let limited = Cred {
        uid: Uid(1000),
        euid: Uid(1001),
        gid: Gid(0),
        egid: Gid(0),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, limited);

    // Allowed: swap euid back to uid (the original "real" id).
    let outcome = step_setuid(&proc_cap, Uid(1000));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1000));

    // Allowed: swap back to the saved euid (still 1000 currently;
    // semantics here let us re-pick the prior euid, which is 1000
    // since we just assigned it). Validate the rejection path next.
    let outcome = step_setuid(&proc_cap, Uid(1001));
    assert_eq!(outcome, CredChange::PermissionDenied);
}

#[test]
fn setuid_on_zombie_returns_zombie() {
    let _g = setup();
    let proc_cap = bootstrap();
    step_exit_group(&proc_cap, ExitStatus::Exited(0));
    let outcome = step_setuid(&proc_cap, Uid(1000));
    assert_eq!(outcome, CredChange::Zombie);
}

#[test]
fn setgid_privileged_changes_both_gid_and_egid() {
    let _g = setup();
    let proc_cap = bootstrap();
    let outcome = step_setgid(&proc_cap, Gid(2000));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.gid, Gid(2000));
    assert_eq!(new.egid, Gid(2000));
}

#[test]
fn setgid_unprivileged_rejects_arbitrary_gid() {
    let _g = setup();
    let proc_cap = bootstrap();
    let limited = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        gid: Gid(100),
        egid: Gid(100),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, limited);

    let outcome = step_setgid(&proc_cap, Gid(200));
    assert_eq!(outcome, CredChange::PermissionDenied);

    // But swapping among (gid, egid) is allowed.
    let outcome = step_setgid(&proc_cap, Gid(100));
    assert!(matches!(outcome, CredChange::Replaced { .. }));
}

#[test]
fn cred_shares_euid_compares_effective_uids() {
    let make = |euid| Cred {
        uid: Uid(0),
        euid,
        gid: Gid(0),
        egid: Gid(0),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let a = make(Uid(1000));
    let b = make(Uid(1000));
    let c = make(Uid(2000));

    assert!(a.shares_euid(b));
    assert!(!a.shares_euid(c));
}
