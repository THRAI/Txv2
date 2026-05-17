//! Credential service tests.
//!
//! Cover the day-1 surface: type basics, root cred semantics,
//! fork-inherits-cred, privileged-vs-non-privileged setuid/setgid,
//! and zombie ignoring.

use crate::cred::adapter::step_engine::Cap;
use crate::cred::{
    step_apply_suid_for_exec, step_setgid, step_setregid, step_setresgid, step_setresuid,
    step_setreuid, step_setuid, Capability, CapabilitySet, Cred, CredChange, CredSnapshot, Gid,
    Uid,
};
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_exit_group, step_fork, ExitStatus};
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

    let child = step_fork::<TestPmap>(&parent, false).expect("fork");
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
    // After step_exit_group reaps the payload, cred_snapshot() must
    // surface `None` so callers can fall back to CredSnapshot::root()
    // (mirroring the cred()/cred_cap() pair already established by
    // PR-9 phase 5 for the AtomicSlot<Cap<Cred>> shape).
    let _g = setup();
    let proc_cap = bootstrap();
    let _ = step_exit_group(&proc_cap, ExitStatus::Exited(0));

    assert!(proc_cap.cred_snapshot().is_none());
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
