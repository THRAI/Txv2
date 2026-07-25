//! Credential service tests.
//!
//! Cover the day-1 surface: type basics, root cred semantics,
//! fork-inherits-cred, privileged-vs-non-privileged setuid/setgid,
//! and zombie ignoring.

use crate::cred::adapter::step_engine::Cap;
use crate::cred::{
    commit_prepared_exec_cred, prepare_exec_cred, prepare_setid_for_exec, step_apply_suid_for_exec,
    step_set_capability_sets, step_setgid, step_setregid, step_setresgid, step_setresuid,
    step_setreuid, step_setuid, Capability, CapabilitySet, Cred, CredChange, CredSnapshot,
    ExecSetidPolicy, Gid, Uid,
};
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_exit_group_with_posts, step_fork, ExitStatus};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::vfs::structure::{S_ISGID, S_ISUID};
use crate::vfs::Credential;
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;

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
    step_exit_group_with_posts(
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

fn cred_of(proc_cap: &Cap<ProcessIdentity>) -> Cred {
    let payload_guard = proc_cap.payload.lock();
    payload_guard.as_ref().expect("alive").cred()
}

fn set_cred(proc_cap: &Cap<ProcessIdentity>, cred: Cred) {
    // PR-9 phase 5 (D5 Path A): `cred` now lives in
    // `AtomicSlot<Cap<Cred>>`; tests mint a fresh `Cap<Cred>` and
    // atomic-swap into the slot. The previous cap drops here.
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let new_cap = crate::cred::sign_cred(cred).expect("zone slab has capacity in tests");
    let _old = payload.replace_cred(new_cap);
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
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&parent, custom);

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
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
    // Privileged setuid bumps the saved-set to match.
    assert_eq!(new.suid, Uid(1000));
}

#[test]
fn setuid_privileged_to_nonroot_drops_capabilities() {
    let _g = setup();
    let proc_cap = bootstrap();
    let outcome = step_setuid(&proc_cap, Uid(1000));

    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1000));
    assert_eq!(new.effective_caps, CapabilitySet::EMPTY);
    assert_eq!(new.permitted_caps, CapabilitySet::EMPTY);
    assert!(!new.is_privileged_for(Capability::KILL));
}

#[test]
fn setresuid_effective_root_transition_updates_effective_caps_only() {
    let _g = setup();
    let proc_cap = bootstrap();

    let outcome = step_setresuid(&proc_cap, None, Some(Uid(1000)), None);
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert_eq!(new.uid, Uid::ROOT);
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid::ROOT);
    assert_eq!(new.effective_caps, CapabilitySet::EMPTY);
    assert_eq!(new.permitted_caps, CapabilitySet::FULL);

    let outcome = step_setresuid(&proc_cap, None, Some(Uid::ROOT), None);
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert_eq!(new.euid, Uid::ROOT);
    assert_eq!(new.effective_caps, CapabilitySet::FULL);
    assert_eq!(new.permitted_caps, CapabilitySet::FULL);
}

#[test]
fn setuid_unprivileged_can_swap_among_existing_ids_only() {
    let _g = setup();
    let proc_cap = bootstrap();

    // Drop privileges: uid=1000, euid=1001, suid=1001, no caps.
    let limited = Cred {
        uid: Uid(1000),
        euid: Uid(1001),
        suid: Uid(1001),
        gid: Gid(0),
        egid: Gid(0),
        sgid: Gid(0),
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
    // Non-privileged setuid preserves suid.
    assert_eq!(new.suid, Uid(1001));

    // Allowed: 1001 is still in {uid: 1000, euid: 1000, suid: 1001}.
    let outcome = step_setuid(&proc_cap, Uid(1001));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced, got {outcome:?}");
    };
    assert_eq!(new.euid, Uid(1001));
    assert_eq!(new.suid, Uid(1001));

    // Disallowed: 9999 is not in {uid, euid, suid}.
    let outcome = step_setuid(&proc_cap, Uid(9999));
    assert_eq!(outcome, CredChange::PermissionDenied);
}

#[test]
fn setuid_on_zombie_returns_zombie() {
    let _g = setup();
    let proc_cap = bootstrap();
    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(0));
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
    assert_eq!(new.sgid, Gid(2000));
}

#[test]
fn setgid_unprivileged_rejects_arbitrary_gid() {
    let _g = setup();
    let proc_cap = bootstrap();
    let limited = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(100),
        egid: Gid(100),
        sgid: Gid(100),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, limited);

    let outcome = step_setgid(&proc_cap, Gid(200));
    assert_eq!(outcome, CredChange::PermissionDenied);

    // But swapping among (gid, egid, sgid) is allowed.
    let outcome = step_setgid(&proc_cap, Gid(100));
    assert!(matches!(outcome, CredChange::Replaced { .. }));
}

#[test]
fn cred_shares_euid_compares_effective_uids() {
    let make = |euid| Cred {
        uid: Uid(0),
        euid,
        suid: Uid(0),
        gid: Gid(0),
        egid: Gid(0),
        sgid: Gid(0),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let a = make(Uid(1000));
    let b = make(Uid(1000));
    let c = make(Uid(2000));

    assert!(a.shares_euid(b));
    assert!(!a.shares_euid(c));
}

// ============================================================
// DAC + setuid Wave 1: saved-set helpers
// ============================================================

/// Helper: drop privileges to (uid=ruid, euid=euid, suid=suid).
fn drop_to(proc_cap: &Cap<ProcessIdentity>, ruid: u32, euid: u32, suid: u32) {
    let cred = Cred {
        uid: Uid(ruid),
        euid: Uid(euid),
        suid: Uid(suid),
        gid: Gid(ruid),
        egid: Gid(euid),
        sgid: Gid(suid),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(proc_cap, cred);
}

#[test]
fn step_setuid_privileged_writes_suid_to_new_uid() {
    let _g = setup();
    let proc_cap = bootstrap();
    let outcome = step_setuid(&proc_cap, Uid(1000));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1000));
}

#[test]
fn step_setuid_non_privileged_preserves_suid() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1001, 1002);

    // 1002 is in {1000, 1001, 1002}, so the call is permitted.
    let outcome = step_setuid(&proc_cap, Uid(1002));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1000)); // real preserved
    assert_eq!(new.euid, Uid(1002));
    assert_eq!(new.suid, Uid(1002)); // saved-set preserved (not changed)
}

#[test]
fn step_setresuid_privileged_sets_all_three() {
    let _g = setup();
    let proc_cap = bootstrap();
    let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1002)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1001));
    assert_eq!(new.suid, Uid(1002));
}

#[test]
fn step_setresuid_unprivileged_within_real_euid_suid_succeeds() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1001, 1002);

    // All three values are in {1000, 1001, 1002}; permitted.
    let outcome = step_setresuid(&proc_cap, Some(Uid(1001)), Some(Uid(1000)), Some(Uid(1002)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1001));
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1002));
}

#[test]
fn step_setresuid_unprivileged_outside_returns_eperm() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1001, 1002);
    let prev = cred_of(&proc_cap);

    // 9999 is not in {uid, euid, suid}; the entire call must fail
    // atomically (no field change).
    let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(9999)), None);
    assert_eq!(outcome, CredChange::PermissionDenied);
    assert_eq!(cred_of(&proc_cap), prev);
}

#[test]
fn step_setresuid_with_none_leaves_field_unchanged() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1001, 1002);

    // Only update euid; ruid/suid stay as-is.
    let outcome = step_setresuid(&proc_cap, None, Some(Uid(1000)), None);
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1002));
}

#[test]
fn step_setreuid_updates_suid_on_euid_change() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1000, 1000);

    // Non-privileged, change euid to a value in the existing set
    // (the real uid 1000 is itself in the set, so euid stays in
    // {uid, euid, suid}). Use suid=1002 instead so the bump is
    // visible.
    drop_to(&proc_cap, 1000, 1001, 1002);

    // Set euid to 1002 (in set). Per Linux quirk: post-call euid
    // (1002) differs from prev.uid (1000), so suid is bumped.
    let outcome = step_setreuid(&proc_cap, None, Some(Uid(1002)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.uid, Uid(1000));
    assert_eq!(new.euid, Uid(1002));
    assert_eq!(new.suid, Uid(1002)); // bumped per the quirk
}

#[test]
fn step_setreuid_does_not_bump_suid_when_euid_unchanged_and_no_ruid_change() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1000, 1002);

    // Set euid back to its current value (1000). post-call euid
    // (1000) matches prev.uid (1000), and ruid is None. Suid stays.
    let outcome = step_setreuid(&proc_cap, None, Some(Uid(1000)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.suid, Uid(1002));
}

#[test]
fn step_setresgid_privileged_sets_all_three() {
    let _g = setup();
    let proc_cap = bootstrap();
    let outcome = step_setresgid(&proc_cap, Some(Gid(2000)), Some(Gid(2001)), Some(Gid(2002)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.gid, Gid(2000));
    assert_eq!(new.egid, Gid(2001));
    assert_eq!(new.sgid, Gid(2002));
}

#[test]
fn step_setresgid_unprivileged_outside_returns_eperm() {
    let _g = setup();
    let proc_cap = bootstrap();
    // Bring caller down to non-privileged uid first so cred is
    // unprivileged for SETGID checks.
    drop_to(&proc_cap, 1000, 1000, 1000);
    // Set gid context.
    let cred = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(2000),
        egid: Gid(2001),
        sgid: Gid(2002),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, cred);
    let prev = cred_of(&proc_cap);

    let outcome = step_setresgid(&proc_cap, None, Some(Gid(9999)), None);
    assert_eq!(outcome, CredChange::PermissionDenied);
    assert_eq!(cred_of(&proc_cap), prev);
}

#[test]
fn step_setregid_updates_sgid_on_egid_change() {
    let _g = setup();
    let proc_cap = bootstrap();
    let cred = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(2000),
        egid: Gid(2001),
        sgid: Gid(2002),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, cred);

    // Set egid to 2002 (in {gid, egid, sgid}); since post-call egid
    // (2002) differs from prev.gid (2000), sgid is bumped.
    let outcome = step_setregid(&proc_cap, None, Some(Gid(2002)));
    let CredChange::Replaced { new, .. } = outcome else {
        panic!("expected Replaced");
    };
    assert_eq!(new.egid, Gid(2002));
    assert_eq!(new.sgid, Gid(2002));
}

#[test]
fn cred_to_walker_credential_drops_permitted_keeps_effective() {
    let _g = setup();
    let proc_cap = bootstrap();
    // Set non-trivial cred where uid != euid and effective_caps !=
    // permitted_caps to verify the bridge: walker reads euid/egid
    // and effective_caps; permitted_caps is dropped.
    let mut effective = CapabilitySet::EMPTY;
    effective.add(Capability::DAC_OVERRIDE);
    let mut permitted = CapabilitySet::EMPTY;
    permitted.add(Capability::DAC_OVERRIDE);
    permitted.add(Capability::SETUID); // present in permitted but not effective
    let cred = Cred {
        uid: Uid(1000),
        euid: Uid(1001),
        suid: Uid(1001),
        gid: Gid(2000),
        egid: Gid(2001),
        sgid: Gid(2001),
        effective_caps: effective,
        permitted_caps: permitted,
    };
    set_cred(&proc_cap, cred);
    let snapshot = cred_of(&proc_cap);

    let walker_cred = Credential::from(&snapshot);
    // Linux semantics: walker uses effective uid/gid for DAC checks.
    assert_eq!(walker_cred.uid, 1001);
    assert_eq!(walker_cred.gid, 2001);
    // effective_caps is preserved verbatim.
    assert_eq!(walker_cred.effective_caps, effective);
    // permitted_caps is not represented on Credential at all
    // (compile-time guarantee — if the field grew, the test wouldn't
    // compile).
}

// ============================================================
// DAC + setuid Wave 4 Part 5: step_apply_suid_for_exec
// ============================================================

#[test]
fn step_apply_suid_for_exec_no_setuid_bit_unchanged() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);
    let prev = cred_of(&proc_cap);

    // mode 0o755 has no setuid/setgid bits.
    let outcome =
        step_apply_suid_for_exec(&proc_cap, Uid(1000), Gid(1000), 0o755).expect("alive process");
    assert!(!outcome.at_secure);
    assert_eq!(outcome.previous_cred, prev);
    assert_eq!(cred_of(&proc_cap), prev);
}

#[test]
fn step_apply_suid_for_exec_setuid_bit_sets_euid_and_suid() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);

    // S_ISUID | 0o755 → file owned by uid 1000 should produce
    // post-recompute euid=1000, suid=1000; real uid stays at 1001.
    let outcome = step_apply_suid_for_exec(&proc_cap, Uid(1000), Gid(0), S_ISUID | 0o755)
        .expect("alive process");
    assert!(outcome.at_secure);
    assert_eq!(outcome.previous_cred.euid, Uid(1001));

    let new = cred_of(&proc_cap);
    assert_eq!(new.uid, Uid(1001));
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1000));
}

#[test]
fn prepare_setid_for_exec_does_not_publish_until_commit() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);
    let before_cap = proc_cap.cred_cap().expect("alive cred cap");
    let before = *before_cap;

    let prepared = prepare_setid_for_exec(&proc_cap, Uid(1000), Gid(2000), S_ISUID | 0o755)
        .expect("prepare setid cred");

    assert_eq!(prepared.credential_snapshot().uid, before.uid);
    assert_eq!(prepared.credential().euid, Uid(1000));
    assert!(prepared.at_secure());
    assert_eq!(
        proc_cap.cred_cap().expect("current cred cap").key(),
        before_cap.key()
    );
    assert_eq!(cred_of(&proc_cap), before);

    let outcome = commit_prepared_exec_cred(prepared).expect("authoritative exec binding");
    assert!(outcome.at_secure);
    assert_eq!(outcome.previous_cred, before);
    assert_ne!(
        proc_cap.cred_cap().expect("committed cred cap").key(),
        before_cap.key()
    );
    assert_eq!(cred_of(&proc_cap).euid, Uid(1000));
}

#[test]
fn exec_cred_reservation_blocks_mutators_until_abort() {
    let _g = setup();
    let proc_cap = bootstrap();
    let before = cred_of(&proc_cap);
    let prepared = prepare_setid_for_exec(&proc_cap, Uid(1000), Gid(2000), S_ISUID | 0o755)
        .expect("prepare setid cred");

    assert_eq!(step_setuid(&proc_cap, Uid(1234)), CredChange::Again);
    assert_eq!(step_setgid(&proc_cap, Gid(1234)), CredChange::Again);
    assert_eq!(
        step_setreuid(&proc_cap, Some(Uid(1234)), Some(Uid(1234))),
        CredChange::Again
    );
    assert_eq!(
        step_setregid(&proc_cap, Some(Gid(1234)), Some(Gid(1234))),
        CredChange::Again
    );
    assert_eq!(
        step_setresuid(&proc_cap, Some(Uid(1234)), Some(Uid(1234)), Some(Uid(1234)),),
        CredChange::Again
    );
    assert_eq!(
        step_setresgid(&proc_cap, Some(Gid(1234)), Some(Gid(1234)), Some(Gid(1234)),),
        CredChange::Again
    );
    assert_eq!(
        step_set_capability_sets(&proc_cap, CapabilitySet::EMPTY, CapabilitySet::EMPTY),
        CredChange::Again
    );
    assert!(matches!(
        prepare_setid_for_exec(&proc_cap, Uid(2000), Gid(2000), S_ISUID | 0o755),
        Err(crate::execution::Errno::EAGAIN)
    ));
    assert_eq!(cred_of(&proc_cap), before);

    drop(prepared);
    assert!(matches!(
        step_setuid(&proc_cap, Uid(1234)),
        CredChange::Replaced { .. }
    ));
}

#[test]
fn exec_cred_commit_releases_reservation() {
    let _g = setup();
    let proc_cap = bootstrap();
    let prepared = prepare_setid_for_exec(&proc_cap, Uid(1000), Gid(2000), S_ISUID | 0o755)
        .expect("prepare setid cred");

    let outcome = commit_prepared_exec_cred(prepared).expect("authoritative exec binding");
    assert_eq!(outcome.previous_cred, Cred::root());
    assert!(matches!(
        step_setuid(&proc_cap, Uid(1234)),
        CredChange::Replaced { .. }
    ));
}

#[test]
fn stale_process_binding_rejects_exec_cred_commit_without_mutation() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);
    let prepared = prepare_setid_for_exec(&proc_cap, Uid(2000), Gid(3000), S_ISUID | 0o755)
        .expect("prepare setid cred");
    let detached = proc_cap
        .payload
        .lock()
        .take()
        .expect("force stale authoritative binding");
    let before_cap = detached.cred_cap();
    let before = *before_cap;

    assert!(matches!(
        commit_prepared_exec_cred(prepared),
        Err(crate::process::ExecPrepError::Zombie)
    ));
    assert_eq!(detached.cred_cap().key(), before_cap.key());
    assert_eq!(detached.cred(), before);
}

#[test]
fn nosuid_exec_holds_reservation_without_preparing_replacement() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);
    let before_cap = proc_cap.cred_cap().expect("alive cred cap");
    let before = *before_cap;

    let prepared = prepare_exec_cred(
        &proc_cap,
        Uid(1000),
        Gid(2000),
        S_ISUID | 0o755,
        ExecSetidPolicy::Suppress,
    )
    .expect("reserve nosuid exec cred");

    assert!(!prepared.has_replacement());
    assert_eq!(prepared.credential_snapshot(), before);
    assert!(!prepared.at_secure());
    assert_eq!(step_setuid(&proc_cap, Uid(1001)), CredChange::Again);

    let outcome = commit_prepared_exec_cred(prepared).expect("authoritative exec binding");
    assert_eq!(outcome.previous_cred, before);
    assert_eq!(
        proc_cap.cred_cap().expect("post-commit cred cap").key(),
        before_cap.key()
    );
}

#[test]
fn exec_cred_reservation_blocks_group_exit_until_abort() {
    let _g = setup();
    let proc_cap = bootstrap();
    let prepared = prepare_setid_for_exec(&proc_cap, Uid(1000), Gid(2000), S_ISUID | 0o755)
        .expect("prepare setid cred");

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(7));

    assert!(
        !proc_cap.is_zombie(),
        "an in-flight exec reservation must prevent payload detach"
    );
    drop(prepared);
    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(7));
    assert!(proc_cap.is_zombie());
}

#[test]
fn concurrent_exec_reservation_makes_group_exit_retry_without_detach() {
    let _g = setup();
    let proc_cap = bootstrap();
    let prepared = prepare_setid_for_exec(&proc_cap, Uid(1000), Gid(2000), S_ISUID | 0o755)
        .expect("prepare setid cred");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    std::thread::scope(|scope| {
        let process = proc_cap.clone();
        let worker_barrier = barrier.clone();
        let exit = scope.spawn(move || {
            worker_barrier.wait();
            crate::process::step_exit_group_with_posts(
                &process,
                ExitStatus::Exited(9),
                |weak, event| {
                    if let Some(mailbox) = weak.upgrade() {
                        let _ = mailbox.post(event);
                    }
                },
                |mailbox, event| mailbox.post(event),
            )
        });
        barrier.wait();
        assert_eq!(
            exit.join().expect("exit worker"),
            crate::process::ProcessExitOutcome::Retry
        );
    });

    assert!(!proc_cap.is_zombie());
    assert!(proc_cap.payload.lock().is_some());
    drop(prepared);
}

#[test]
fn step_apply_suid_for_exec_setgid_with_group_x_sets_egid_and_sgid() {
    let _g = setup();
    let proc_cap = bootstrap();
    let cred = Cred {
        uid: Uid(1001),
        euid: Uid(1001),
        suid: Uid(1001),
        gid: Gid(2001),
        egid: Gid(2001),
        sgid: Gid(2001),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, cred);

    // S_ISGID | 0o2755 → has S_ISGID and group-X (0o010). File owned
    // by gid 2000.
    let outcome = step_apply_suid_for_exec(&proc_cap, Uid(0), Gid(2000), S_ISGID | 0o755)
        .expect("alive process");
    assert!(outcome.at_secure);

    let new = cred_of(&proc_cap);
    assert_eq!(new.gid, Gid(2001), "real gid preserved");
    assert_eq!(new.egid, Gid(2000));
    assert_eq!(new.sgid, Gid(2000));
}

#[test]
fn step_apply_suid_for_exec_setgid_without_group_x_unchanged() {
    let _g = setup();
    let proc_cap = bootstrap();
    let cred = Cred {
        uid: Uid(1001),
        euid: Uid(1001),
        suid: Uid(1001),
        gid: Gid(2001),
        egid: Gid(2001),
        sgid: Gid(2001),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    set_cred(&proc_cap, cred);
    let prev = cred_of(&proc_cap);

    // S_ISGID | 0o4744 → has S_ISGID but NO group-X bit (group is r--).
    // Linux's mandatory-locking semantic: cred is untouched.
    let mode = S_ISGID | 0o744;
    assert_eq!(mode & 0o010, 0, "fixture must lack group-X");

    let outcome =
        step_apply_suid_for_exec(&proc_cap, Uid(0), Gid(2000), mode).expect("alive process");
    assert!(!outcome.at_secure);
    assert_eq!(cred_of(&proc_cap), prev);
}

#[test]
fn step_apply_suid_for_exec_at_secure_true_when_euid_changes() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1001, 1001, 1001);

    // file_uid (1000) differs from caller's euid (1001) → at_secure.
    let outcome = step_apply_suid_for_exec(&proc_cap, Uid(1000), Gid(0), S_ISUID | 0o755)
        .expect("alive process");
    assert!(outcome.at_secure);
}

#[test]
fn cred_snapshot_freezes_value_against_later_mutation() {
    // cred_service_v_1 §"In flight": the snapshot is a by-value copy
    // captured once at syscall entry. A subsequent setuid on the same
    // process must publish a new cap into the AtomicSlot without
    // mutating the snapshot a prior caller already holds.
    let _g = setup();
    let proc_cap = bootstrap();

    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let snap = payload.cred_snapshot();
    drop(payload_guard);

    assert!(snap.cred().euid.is_root());
    assert!(snap.is_privileged_for(Capability::SETUID));

    // Mutate the canonical cred underneath the snapshot.
    let change = step_setuid(&proc_cap, Uid(1000));
    assert!(matches!(change, CredChange::Replaced { .. }));

    // The snapshot still reflects the pre-mutation value; the live
    // cred has moved on.
    assert!(snap.cred().euid.is_root());
    assert_eq!(cred_of(&proc_cap).euid, Uid(1000));

    // A fresh snapshot taken now sees the new value.
    let fresh = proc_cap
        .cred_snapshot()
        .expect("alive process produces a snapshot");
    assert_eq!(fresh.cred().euid, Uid(1000));
}

#[test]
fn cred_snapshot_root_constructor_matches_root_cred() {
    // CredSnapshot::root() is the defensive fallback used by SyscallCtx
    // when the target process is a zombie at construction time.
    let snap = CredSnapshot::root();
    let cred = Cred::root();
    assert_eq!(snap.cred(), cred);
    assert_eq!(snap.as_cred(), &cred);
    assert!(snap.is_privileged_for(Capability::KILL));
}

#[test]
fn cred_snapshot_returns_none_for_zombie() {
    // After group exit reaps the payload, cred_snapshot() must
    // surface `None` so callers can fall back to CredSnapshot::root()
    // (mirroring the cred()/cred_cap() pair already established by
    // PR-9 phase 5 for the AtomicSlot<Cap<Cred>> shape).
    let _g = setup();
    let proc_cap = bootstrap();
    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(0));

    assert!(proc_cap.cred_snapshot().is_none());
}

#[test]
fn require_path_search_passes_for_dac_override() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_path_search;
    use crate::vfs::structure::InodeMeta;

    let _g = setup();

    // Caller carries CAP_DAC_OVERRIDE → the directory's X bits don't
    // matter; require_path_search returns SearchAuthorized.
    let mut effective_caps = CapabilitySet::EMPTY;
    effective_caps.add(Capability::DAC_OVERRIDE);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps,
        permitted_caps: CapabilitySet::EMPTY,
    });

    // Directory with mode 0o600 (no traversal for anyone) — still passes.
    let mut meta = InodeMeta::new(crate::vfs::structure::InodeKind::Directory, 0o600);
    meta.uid = 0;
    meta.gid = 0;

    let g = guard();
    let _w = require_path_search(&snap, &meta, &g).expect("CAP_DAC_OVERRIDE bypass");
}

#[test]
fn require_path_search_denies_without_x_bit() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_path_search;
    use crate::execution::Errno;
    use crate::vfs::structure::InodeMeta;

    let _g = setup();

    // Caller without CAP_DAC_OVERRIDE; directory has no X bit for "other".
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let mut meta = InodeMeta::new(crate::vfs::structure::InodeKind::Directory, 0o644);
    meta.uid = 2000;
    meta.gid = 2000;

    let g = guard();
    let err = require_path_search(&snap, &meta, &g).err().expect("denied");
    assert_eq!(err, Errno::EACCES);
}

#[test]
fn require_open_honors_read_and_write_bits() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_open;
    use crate::execution::Errno;
    use crate::vfs::structure::{InodeMeta, OpenFileFlags};

    let _g = setup();

    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    });

    // Owner-only readable file (0o400), caller is owner.
    let mut meta = InodeMeta::new(crate::vfs::structure::InodeKind::Regular, 0o400);
    meta.uid = 1000;
    meta.gid = 1000;
    let flags_r = OpenFileFlags {
        read: true,
        write: false,
        ..Default::default()
    };
    let flags_rw = OpenFileFlags {
        read: true,
        write: true,
        ..Default::default()
    };

    let g = guard();
    let _w = require_open(&snap, &meta, flags_r, &g).expect("read-only owner open");
    let err = require_open(&snap, &meta, flags_rw, &g)
        .err()
        .expect("rw on read-only mode");
    assert_eq!(err, Errno::EACCES);
}

// ---------- require_unlink ----------

fn fresh_dir_meta(mode: u16, uid: u32, gid: u32) -> crate::vfs::structure::InodeMeta {
    let mut m =
        crate::vfs::structure::InodeMeta::new(crate::vfs::structure::InodeKind::Directory, mode);
    m.uid = uid;
    m.gid = gid;
    m
}

fn fresh_file_meta(mode: u16, uid: u32, gid: u32) -> crate::vfs::structure::InodeMeta {
    let mut m =
        crate::vfs::structure::InodeMeta::new(crate::vfs::structure::InodeKind::Regular, mode);
    m.uid = uid;
    m.gid = gid;
    m
}

fn unprivileged_snap(uid: u32, gid: u32) -> CredSnapshot {
    CredSnapshot::from_cred(Cred {
        uid: Uid(uid),
        euid: Uid(uid),
        suid: Uid(uid),
        gid: Gid(gid),
        egid: Gid(gid),
        sgid: Gid(gid),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    })
}

#[test]
fn require_unlink_passes_for_writable_parent_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;

    let _g = setup();
    // Owner of parent has rwx (0o700). Sticky not set. Owns parent
    // and child. Permitted.
    let parent = fresh_dir_meta(0o700, 1000, 1000);
    let child = fresh_file_meta(0o600, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_unlink(&snap, &parent, &child, &g).expect("owner with W+X");
}

#[test]
fn require_unlink_denies_eacces_when_parent_lacks_write_bit() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;
    use crate::execution::Errno;

    let _g = setup();
    // Parent mode 0o555 (r-xr-xr-x) — owner has no W. Sticky off.
    let parent = fresh_dir_meta(0o555, 1000, 1000);
    let child = fresh_file_meta(0o600, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_unlink(&snap, &parent, &child, &g)
        .err()
        .expect("denied");
    assert_eq!(err, Errno::EACCES);
}

#[test]
fn require_unlink_cap_dac_override_bypasses_write_bit() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;

    let _g = setup();
    // Parent denies write to everyone (0o555), but caller carries
    // CAP_DAC_OVERRIDE → unlink permitted.
    let parent = fresh_dir_meta(0o555, 0, 0);
    let child = fresh_file_meta(0o600, 0, 0);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::DAC_OVERRIDE);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let _w = require_unlink(&snap, &parent, &child, &g).expect("DAC_OVERRIDE bypass");
}

#[test]
fn require_unlink_sticky_bit_denies_eperm_for_non_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // /tmp-style: parent has sticky + world-writable. Caller has
    // write+search bits, but owns neither parent nor child →
    // EPERM (the /tmp protection rule).
    let parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let child = fresh_file_meta(0o644, 2000, 2000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_unlink(&snap, &parent, &child, &g)
        .err()
        .expect("sticky denies");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_unlink_sticky_bit_permits_child_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // Owns child but not parent. Sticky still permits — POSIX:
    // sticky requires owner-of-child OR owner-of-parent.
    let parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let child = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_unlink(&snap, &parent, &child, &g).expect("child owner");
}

#[test]
fn require_unlink_sticky_bit_not_bypassed_by_dac_override() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // CAP_DAC_OVERRIDE handles the W bit but NOT the sticky-bit
    // ownership rule — POSIX requires CAP_FOWNER (or owner match)
    // to override sticky.
    let parent = fresh_dir_meta(S_ISVTX | 0o1755, 0, 0);
    let child = fresh_file_meta(0o644, 2000, 2000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::DAC_OVERRIDE);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let err = require_unlink(&snap, &parent, &child, &g)
        .err()
        .expect("DAC_OVERRIDE doesn't bypass sticky");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_unlink_sticky_bit_permits_cap_fowner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_unlink;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // CAP_FOWNER is the POSIX bypass for sticky-bit ownership rule.
    let parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let child = fresh_file_meta(0o644, 2000, 2000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::FOWNER);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let _w = require_unlink(&snap, &parent, &child, &g).expect("CAP_FOWNER bypass");
}

// ---------- require_link ----------

#[test]
fn require_link_passes_for_writable_parent_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_link;

    let _g = setup();
    let parent = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_link(&snap, &parent, &g).expect("owner with W+X");
}

#[test]
fn require_link_denies_eacces_when_parent_lacks_write_bit() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_link;
    use crate::execution::Errno;

    let _g = setup();
    // Owner has r-xr-xr-x (0o555) — no write for anyone. Caller is owner.
    let parent = fresh_dir_meta(0o555, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_link(&snap, &parent, &g).err().expect("denied");
    assert_eq!(err, Errno::EACCES);
}

#[test]
fn require_link_cap_dac_override_bypasses() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_link;

    let _g = setup();
    // World-unwritable but caller carries CAP_DAC_OVERRIDE.
    let parent = fresh_dir_meta(0o555, 0, 0);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::DAC_OVERRIDE);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let _w = require_link(&snap, &parent, &g).expect("DAC_OVERRIDE bypass");
}

#[test]
fn require_link_sticky_bit_irrelevant() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_link;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // Sticky set + world-writable. Sticky is *only* a removal-time
    // rule (unlink / rmdir / rename source). Adding a name doesn't
    // touch any existing entry, so sticky is silent here — non-owner
    // with W+X passes.
    let parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_link(&snap, &parent, &g).expect("sticky doesn't gate creation");
}

// ---------- require_rename ----------

#[test]
fn require_rename_passes_for_owner_of_both_parents() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;

    let _g = setup();
    let old_parent = fresh_dir_meta(0o700, 1000, 1000);
    let old_child = fresh_file_meta(0o600, 1000, 1000);
    let new_parent = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_rename(&snap, &old_parent, &old_child, &new_parent, None, &g)
        .expect("owner with W+X on both");
}

#[test]
fn require_rename_denies_when_old_parent_lacks_write() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;
    use crate::execution::Errno;

    let _g = setup();
    // Old parent r-x for owner — can search but not remove → EACCES
    // from the unlink-side rule.
    let old_parent = fresh_dir_meta(0o500, 1000, 1000);
    let old_child = fresh_file_meta(0o600, 1000, 1000);
    let new_parent = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_rename(&snap, &old_parent, &old_child, &new_parent, None, &g)
        .err()
        .expect("denied at old side");
    assert_eq!(err, Errno::EACCES);
}

#[test]
fn require_rename_denies_when_new_parent_lacks_write() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;
    use crate::execution::Errno;

    let _g = setup();
    // Old side fine, new parent has no W for caller → EACCES from
    // the link-side rule.
    let old_parent = fresh_dir_meta(0o700, 1000, 1000);
    let old_child = fresh_file_meta(0o600, 1000, 1000);
    let new_parent = fresh_dir_meta(0o500, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_rename(&snap, &old_parent, &old_child, &new_parent, None, &g)
        .err()
        .expect("denied at new side");
    assert_eq!(err, Errno::EACCES);
}

#[test]
fn require_rename_sticky_on_old_parent_denies_non_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // /tmp-style: sticky-protected old parent. Caller has W+X but
    // owns neither the child nor the parent → EPERM (rename can't
    // remove a sticky-protected entry).
    let old_parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let old_child = fresh_file_meta(0o644, 2000, 2000);
    let new_parent = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_rename(&snap, &old_parent, &old_child, &new_parent, None, &g)
        .err()
        .expect("sticky on old parent");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_rename_sticky_on_new_parent_denies_displaced_non_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // Both parents OK; new path has a displaced entry under a
    // sticky parent. Caller owns the displaced child's *parent*?
    // No — uid 1000 ≠ 0 (parent owner) and ≠ 2000 (displaced
    // owner) → EPERM.
    let old_parent = fresh_dir_meta(0o700, 1000, 1000);
    let old_child = fresh_file_meta(0o600, 1000, 1000);
    let new_parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let displaced = fresh_file_meta(0o644, 2000, 2000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_rename(
        &snap,
        &old_parent,
        &old_child,
        &new_parent,
        Some(&displaced),
        &g,
    )
    .err()
    .expect("sticky on new parent + displacement");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_rename_displaced_none_skips_displaced_check() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_rename;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // Same setup as the previous test but with displaced=None
    // (the common case: rename creates, not overwrites). The
    // sticky-on-new-parent rule does *not* fire when no
    // displacement is happening; W+X on new parent is all that's
    // needed.
    let old_parent = fresh_dir_meta(0o700, 1000, 1000);
    let old_child = fresh_file_meta(0o600, 1000, 1000);
    let new_parent = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_rename(&snap, &old_parent, &old_child, &new_parent, None, &g)
        .expect("displaced=None bypasses sticky on new parent");
}

// ---------- require_chmod / require_chown ----------

#[test]
fn require_chmod_passes_for_owner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chmod;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w = require_chmod(&snap, &target, 0o755, &g).expect("owner");
}

#[test]
fn require_chmod_denies_non_owner_without_cap_fowner() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chmod;
    use crate::execution::Errno;

    let _g = setup();
    let target = fresh_file_meta(0o644, 2000, 2000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_chmod(&snap, &target, 0o755, &g)
        .err()
        .expect("denied");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_chmod_cap_fowner_bypasses_ownership() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chmod;

    let _g = setup();
    let target = fresh_file_meta(0o644, 2000, 2000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::FOWNER);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let _w = require_chmod(&snap, &target, 0o755, &g).expect("CAP_FOWNER bypass");
}

#[test]
fn require_chown_passes_for_self_uid_and_gid() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chown;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let _w =
        require_chown(&snap, &target, Some(1000), Some(1000), &g).expect("self uid/gid permitted");
}

#[test]
fn require_chown_denies_foreign_uid_for_non_privileged() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chown;
    use crate::execution::Errno;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_chown(&snap, &target, Some(2000), None, &g)
        .err()
        .expect("foreign uid");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_chown_denies_foreign_gid_for_non_privileged() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chown;
    use crate::execution::Errno;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    let g = guard();
    let err = require_chown(&snap, &target, None, Some(999), &g)
        .err()
        .expect("foreign gid");
    assert_eq!(err, Errno::EPERM);
}

#[test]
fn require_chown_cap_fowner_permits_arbitrary_uid_gid() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::require_chown;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::FOWNER);
    let snap = CredSnapshot::from_cred(Cred {
        uid: Uid(2000),
        euid: Uid(2000),
        suid: Uid(2000),
        gid: Gid(2000),
        egid: Gid(2000),
        sgid: Gid(2000),
        effective_caps: caps,
        permitted_caps: CapabilitySet::EMPTY,
    });
    let g = guard();
    let _w = require_chown(&snap, &target, Some(3000), Some(3000), &g).expect("CAP_FOWNER bypass");
}

// ---------- authorize_* combinators ----------
//
// Each combinator wraps a `require_*` predicate: takes its own
// fresh epoch guard, runs the predicate, drops the witness, returns
// Result<(), Errno>. The unit tests below pin the contract that
// the combinator agrees with the underlying predicate for both the
// permitted and denied cases, end-to-end without a caller-managed
// guard.

#[test]
fn authorize_unlink_agrees_with_require_unlink() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{authorize_unlink, require_unlink};
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    // Permitted: owner with W+X, no sticky.
    let parent_ok = fresh_dir_meta(0o700, 1000, 1000);
    let child_ok = fresh_file_meta(0o600, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    assert_eq!(authorize_unlink(&snap, &parent_ok, &child_ok), Ok(()));
    {
        let g = guard();
        assert!(require_unlink(&snap, &parent_ok, &child_ok, &g).is_ok());
    }
    // Denied: sticky on parent, non-owner caller.
    let parent_sticky = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let child_other = fresh_file_meta(0o644, 2000, 2000);
    assert_eq!(
        authorize_unlink(&snap, &parent_sticky, &child_other),
        Err(Errno::EPERM)
    );
}

#[test]
fn authorize_link_agrees_with_require_link() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{authorize_link, require_link};
    use crate::execution::Errno;

    let _g = setup();
    let parent_ok = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    assert_eq!(authorize_link(&snap, &parent_ok), Ok(()));
    {
        let g = guard();
        assert!(require_link(&snap, &parent_ok, &g).is_ok());
    }
    let parent_ro = fresh_dir_meta(0o555, 1000, 1000);
    assert_eq!(authorize_link(&snap, &parent_ro), Err(Errno::EACCES));
}

#[test]
fn authorize_rename_agrees_with_require_rename() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{authorize_rename, require_rename};
    use crate::execution::Errno;
    use crate::vfs::structure::S_ISVTX;

    let _g = setup();
    let op_meta = fresh_dir_meta(0o700, 1000, 1000);
    let oc_meta = fresh_file_meta(0o600, 1000, 1000);
    let np_meta = fresh_dir_meta(0o700, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    assert_eq!(
        authorize_rename(&snap, &op_meta, &oc_meta, &np_meta, None),
        Ok(())
    );
    {
        let g = guard();
        assert!(require_rename(&snap, &op_meta, &oc_meta, &np_meta, None, &g).is_ok());
    }
    // Sticky on old parent + non-owner of child → EPERM.
    let op_sticky = fresh_dir_meta(S_ISVTX | 0o1777, 0, 0);
    let oc_other = fresh_file_meta(0o644, 2000, 2000);
    assert_eq!(
        authorize_rename(&snap, &op_sticky, &oc_other, &np_meta, None),
        Err(Errno::EPERM)
    );
}

#[test]
fn authorize_chmod_agrees_with_require_chmod() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{authorize_chmod, require_chmod};
    use crate::execution::Errno;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap_owner = unprivileged_snap(1000, 1000);
    let snap_other = unprivileged_snap(2000, 2000);
    assert_eq!(authorize_chmod(&snap_owner, &target, 0o755), Ok(()));
    {
        let g = guard();
        assert!(require_chmod(&snap_owner, &target, 0o755, &g).is_ok());
    }
    assert_eq!(
        authorize_chmod(&snap_other, &target, 0o755),
        Err(Errno::EPERM)
    );
}

#[test]
fn authorize_chown_agrees_with_require_chown() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{authorize_chown, require_chown};
    use crate::execution::Errno;

    let _g = setup();
    let target = fresh_file_meta(0o644, 1000, 1000);
    let snap = unprivileged_snap(1000, 1000);
    // Self-uid → permitted.
    assert_eq!(authorize_chown(&snap, &target, Some(1000), None), Ok(()));
    {
        let g = guard();
        assert!(require_chown(&snap, &target, Some(1000), None, &g).is_ok());
    }
    // Foreign uid for non-privileged → EPERM.
    assert_eq!(
        authorize_chown(&snap, &target, Some(2000), None),
        Err(Errno::EPERM)
    );
}

#[test]
fn authorize_path_search_and_open_agree_with_require() {
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{
        authorize_open, authorize_path_search, require_open, require_path_search,
    };
    use crate::execution::Errno;
    use crate::vfs::structure::{InodeMeta, OpenFileFlags};

    let _g = setup();
    let dir = fresh_dir_meta(0o755, 1000, 1000);
    let snap = unprivileged_snap(2000, 2000);
    // Other gets r-x — search ok.
    assert_eq!(authorize_path_search(&snap, &dir), Ok(()));
    {
        let g = guard();
        assert!(require_path_search(&snap, &dir, &g).is_ok());
    }

    // Open: a regular file mode 0o400 owned by uid 1000. Caller is
    // uid 2000 → falls into "other" triplet → no R bit.
    let mut file = InodeMeta::new(crate::vfs::structure::InodeKind::Regular, 0o400);
    file.uid = 1000;
    file.gid = 1000;
    let flags_r = OpenFileFlags {
        read: true,
        ..Default::default()
    };
    assert_eq!(authorize_open(&snap, &file, flags_r), Err(Errno::EACCES));
    {
        let g = guard();
        assert!(matches!(
            require_open(&snap, &file, flags_r, &g),
            Err(Errno::EACCES)
        ));
    }
}

#[test]
fn walker_cred_variants_agree_with_snapshot_variants() {
    // The _with_walker_cred overloads must produce the same verdict
    // as their snapshot-shaped siblings when the walker projection
    // is derived from the same Cred. Pins the equivalence so the
    // walker mint sites and the syscall-arm gate stay in lockstep.
    use crate::cred::adapter::step_engine::guard;
    use crate::cred::checks::{
        require_open, require_open_with_walker_cred, require_path_search,
        require_path_search_with_walker_cred,
    };
    use crate::vfs::structure::{Credential as WalkerCred, InodeMeta, OpenFileFlags};

    let _g = setup();
    let cred = Cred {
        uid: Uid(2000),
        euid: Uid(2000),
        suid: Uid(2000),
        gid: Gid(2000),
        egid: Gid(2000),
        sgid: Gid(2000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let snap = CredSnapshot::from_cred(cred);
    let walker_cred = WalkerCred::from(&snap);

    let dir = fresh_dir_meta(0o755, 1000, 1000);
    let g = guard();
    assert_eq!(
        require_path_search(&snap, &dir, &g).is_ok(),
        require_path_search_with_walker_cred(&walker_cred, &dir, &g).is_ok(),
    );

    let mut file = InodeMeta::new(crate::vfs::structure::InodeKind::Regular, 0o400);
    file.uid = 1000;
    file.gid = 1000;
    let flags = OpenFileFlags {
        read: true,
        ..Default::default()
    };
    assert_eq!(
        require_open(&snap, &file, flags, &g).is_ok(),
        require_open_with_walker_cred(&walker_cred, &file, flags, &g).is_ok(),
    );
}

#[test]
fn step_apply_suid_for_exec_at_secure_false_when_no_change() {
    let _g = setup();
    let proc_cap = bootstrap();
    drop_to(&proc_cap, 1000, 1000, 1000);

    // Setuid bit set, but file_uid (1000) matches caller's euid (1000).
    // No effective-uid delta → at_secure stays false.
    let outcome = step_apply_suid_for_exec(&proc_cap, Uid(1000), Gid(0), S_ISUID | 0o755)
        .expect("alive process");
    assert!(!outcome.at_secure);

    // suid is still rewritten to track the new euid (which equals
    // prev.euid here, so it's a no-op write — pin the equality).
    let new = cred_of(&proc_cap);
    assert_eq!(new.euid, Uid(1000));
    assert_eq!(new.suid, Uid(1000));
}
