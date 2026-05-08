// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![allow(unused_imports)]
use super::*;

use tx_subsystems::cred::{step_setresuid, Uid};
use tx_subsystems::cross_crate_test_support::clear_caps_for_test;

use crate::linux_syscall::{
    NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETRESGID, NR_GETRESUID, NR_GETUID, NR_SETGID,
    NR_SETREGID, NR_SETRESUID, NR_SETREUID, NR_SETUID,
};

/// `(u32) -1` — Linux's "leave unchanged" sentinel for the
/// `setre{u,g}id` / `setres{u,g}id` family. Userspace passes this
/// as the unsigned `uid_t` cast of `-1`; the kernel arm decodes it
/// to `Option::None` before calling the cred helper.
const NEG_ONE_U32: u64 = u32::MAX as u64;

/// Drop privileges from the bootstrap-init root cred to a
/// concrete non-root uid. Uses `step_setresuid` from the
/// privileged starting state, which sets `uid`, `euid`, and
/// `suid` all to `target` in one atomic step. Then clears
/// `effective_caps` / `permitted_caps` via the cross-crate
/// test-support helper so `Cred::is_privileged_for(...)` no
/// longer short-circuits (root.effective_caps == FULL by
/// construction; the shipping `step_set*` mutators preserve
/// caps, so a uid drop alone is not enough to land in an
/// unprivileged state).
fn drop_privs_to(proc_cap: &Cap<ProcessIdentity>, uid: u32) {
    let target = Uid(uid);
    let outcome = step_setresuid(proc_cap, Some(target), Some(target), Some(target));
    assert!(
        matches!(outcome, tx_subsystems::cred::CredChange::Replaced { .. }),
        "drop_privs_to({uid}) must succeed from root: got {outcome:?}"
    );
    clear_caps_for_test(proc_cap);
}

// -----------------------------------------------------------------
// Read-side arms.
// -----------------------------------------------------------------

/// `getuid()` returns the bootstrap-init real uid (0 for root).
#[test]
fn dispatch_getuid_returns_cred_uid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETUID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `geteuid()` returns the bootstrap-init effective uid (0).
#[test]
fn dispatch_geteuid_returns_cred_euid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETEUID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `getgid()` returns the bootstrap-init real gid (0).
#[test]
fn dispatch_getgid_returns_cred_gid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETGID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `getegid()` returns the bootstrap-init effective gid (0).
#[test]
fn dispatch_getegid_returns_cred_egid() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETEGID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// After dropping privs to uid 1000, `getuid` reads the new
/// real uid through `ctx.cred()` — pins that the
/// `SyscallCtx::cred()` accessor reads from `ProcessPayload.cred`
/// (not a stale snapshot stamped at construction).
#[test]
fn dispatch_getuid_reflects_post_setresuid_cred() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETUID, [0; 6]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(1000));
}

// -----------------------------------------------------------------
// Single-arg setters.
// -----------------------------------------------------------------

/// Privileged `setuid(0)` (root → root) succeeds with
/// `Return(0)`. Sanity-check on the privileged path.
#[test]
fn dispatch_setuid_root_to_zero_succeeds() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_SETUID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.uid.raw(), 0);
    assert_eq!(cred.euid.raw(), 0);
    assert_eq!(cred.suid.raw(), 0);
}

/// Non-privileged `setuid(target)` where `target` already matches
/// one of `(uid, euid, suid)` succeeds (`euid` swap). After
/// dropping privs to 1000, `setuid(1000)` is a no-op-shaped
/// success.
#[test]
fn dispatch_setuid_to_existing_id_succeeds() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_SETUID, [1000, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.euid.raw(), 1000);
}

/// Non-privileged `setuid(unrelated)` returns `-EPERM` and leaves
/// the cred unchanged.
#[test]
fn dispatch_setuid_unprivileged_outside_returns_neg_eperm() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // 2000 is not in {uid=1000, euid=1000, suid=1000}.
    let req = SyscallRequest::new(NR_SETUID, [2000, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(1), "expected -EPERM");
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.uid.raw(), 1000);
    assert_eq!(cred.euid.raw(), 1000);
    assert_eq!(cred.suid.raw(), 1000);
}

/// Non-privileged `setgid(target)` where `target` matches one of
/// `(gid, egid, sgid)` succeeds.
#[test]
fn dispatch_setgid_to_existing_id_succeeds() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // After drop_privs_to(1000), gid family is still 0 (only the
    // uid family was changed). `setgid(0)` is a no-op-shaped
    // success because 0 is in {gid, egid, sgid}.
    let req = SyscallRequest::new(NR_SETGID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.egid.raw(), 0);
}

// -----------------------------------------------------------------
// Two-arg setters.
// -----------------------------------------------------------------

/// Privileged `setresuid(1000, 1000, 1000)` writes all three uid
/// fields to 1000.
#[test]
fn dispatch_setresuid_privileged_writes_all_three() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_SETRESUID, [1000, 1000, 1000, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.uid.raw(), 1000);
    assert_eq!(cred.euid.raw(), 1000);
    assert_eq!(cred.suid.raw(), 1000);
}

/// `setresuid(-1, -1, -1)` decodes all three sentinels and
/// leaves the cred unchanged. `Return(0)` because the privilege
/// rule trivially passes (no requested fields to validate).
#[test]
fn dispatch_setresuid_with_neg1_leaves_unchanged() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(
        NR_SETRESUID,
        [NEG_ONE_U32, NEG_ONE_U32, NEG_ONE_U32, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.uid.raw(), 1000);
    assert_eq!(cred.euid.raw(), 1000);
    assert_eq!(cred.suid.raw(), 1000);
}

/// Non-privileged `setresuid(other, -1, -1)` where `other` is not
/// in `{uid, euid, suid}` returns `-EPERM` and leaves the cred
/// unchanged.
#[test]
fn dispatch_setresuid_unprivileged_outside_returns_neg_eperm() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // 2000 is unrelated to the current {1000, 1000, 1000}.
    let req = SyscallRequest::new(NR_SETRESUID, [2000, NEG_ONE_U32, NEG_ONE_U32, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(1), "expected -EPERM");
    let cred = proc_cap.cred().expect("alive process has cred");
    assert_eq!(cred.uid.raw(), 1000);
}

/// Non-privileged `setreuid(-1, current_uid)` exercises the
/// Linux saved-set quirk: changing `euid` away from the pre-call
/// `uid` bumps `suid`. We arrange a state where the quirk fires
/// — start at `(uid, euid, suid) = (0, 0, 0)`, drop euid only.
/// Wait — the privileged path always sets all three. We instead
/// arrange a non-trivial starting state via two calls: privileged
/// `setresuid(1001, 1000, 1000)` to get `(1001, 1000, 1000)`,
/// then non-privileged `setreuid(-1, 1001)` (1001 is in the set
/// because uid==1001). The quirk says "if euid_after != prev.uid,
/// suid := euid_after". Pre-call: `(1001, 1000, 1000)`.
/// Post-call: `(1001, 1001, 1001)`.
#[test]
fn dispatch_setreuid_updates_suid_on_euid_change() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    // Privileged call: split uid from euid/suid using
    // step_setresuid directly (NR_SETRESUID would also work).
    let outcome = step_setresuid(&proc_cap, Some(Uid(1001)), Some(Uid(1000)), Some(Uid(1000)));
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));

    let ctx = make_ctx(proc_cap.clone(), thread);
    // Non-privileged (euid is 1000, not 0) `setreuid(-1, 1001)`.
    // 1001 is in the existing set as `uid`. The quirk fires
    // because `euid_after == 1001 != prev.uid == 1001`? No —
    // wait. Pre: prev.uid = 1001. New euid = 1001. The quirk
    // condition is `ruid.is_some() || new.euid != prev.uid`. With
    // ruid = None and new.euid (1001) == prev.uid (1001), the
    // quirk does NOT fire. We need euid != prev.uid. So instead
    // set up `(1000, 1001, 1001)` and call setreuid(-1, 1000).
    let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1001)));
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));

    // Now: prev = (1000, 1001, 1001). Non-privileged setreuid(-1, 1000):
    // - 1000 is in {1000, 1001, 1001}, so the rule passes.
    // - new.euid = 1000, prev.uid = 1000 → quirk does NOT fire.
    // Need a case that DOES fire: setreuid(-1, 1001) with prev
    // (1000, 1001, 1001) — new.euid=1001, prev.uid=1000 → quirk
    // fires, suid := 1001 (already 1001). To observe a change,
    // start with suid != target euid: (1000, 1000, 1000) start
    // and call setreuid(-1, 1000) — no change; can't trigger.
    // Privileged setresuid(1000, 1001, 1000) → (1000, 1001, 1000)
    // then non-privileged setreuid(-1, 1000) → euid_after=1000,
    // prev.uid=1000, quirk doesn't fire; setreuid(-1, 1001)
    // → euid_after=1001, prev.uid=1000, quirk fires, suid=1001.
    let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1000)));
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    // Pre-syscall snapshot for clarity.
    let pre = proc_cap.cred().expect("alive process has cred");
    assert_eq!(pre.uid.raw(), 1000);
    assert_eq!(pre.euid.raw(), 1001);
    assert_eq!(pre.suid.raw(), 1000);

    // setreuid(-1, 1001) — 1001 is in {1000, 1001, 1000}, allowed.
    // Quirk fires: new.euid=1001 != prev.uid=1000 → suid := 1001.
    let req = SyscallRequest::new(NR_SETREUID, [NEG_ONE_U32, 1001, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let post = proc_cap.cred().expect("alive process has cred");
    assert_eq!(post.uid.raw(), 1000);
    assert_eq!(post.euid.raw(), 1001);
    assert_eq!(
        post.suid.raw(),
        1001,
        "setreuid quirk: euid change must bump suid to euid_after"
    );
}

/// Gid analog of the setreuid-bumps-suid test. Starts at
/// `(gid, egid, sgid) = (1000, 1001, 1000)` and calls
/// `setregid(-1, 1001)` (non-privileged). The quirk fires:
/// `new.egid=1001 != prev.gid=1000 → sgid := 1001`.
#[test]
fn dispatch_setregid_updates_sgid_on_egid_change() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    // Privileged setresgid to seed the asymmetric state.
    let outcome = tx_subsystems::cred::step_setresgid(
        &proc_cap,
        Some(tx_subsystems::cred::Gid(1000)),
        Some(tx_subsystems::cred::Gid(1001)),
        Some(tx_subsystems::cred::Gid(1000)),
    );
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    // Drop uid privs so the gid mutators run as non-privileged.
    // (CAP_SETGID is granted by full caps; root euid==0 also
    // privileges; we need both gone. drop_privs_to clears euid.)
    drop_privs_to(&proc_cap, 1000);

    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_SETREGID, [NEG_ONE_U32, 1001, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let post = proc_cap.cred().expect("alive process has cred");
    assert_eq!(post.gid.raw(), 1000);
    assert_eq!(post.egid.raw(), 1001);
    assert_eq!(
        post.sgid.raw(),
        1001,
        "setregid quirk: egid change must bump sgid to egid_after"
    );
}

// -----------------------------------------------------------------
// getresuid / getresgid round-trips.
// -----------------------------------------------------------------

/// `getresuid(&r, &e, &s)` writes `(uid.raw(), euid.raw(),
/// suid.raw())` to the three user pointers. Wave 2 bootstrap
/// exemption: the three uaddrs are kernel-side `*mut u32`. NULL
/// pointers skip the corresponding write.
#[test]
fn dispatch_getresuid_writes_three_u32s() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // Asymmetric state so the three reads are distinguishable.
    let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1002)));
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    let ctx = make_ctx(proc_cap, thread);

    let mut ruid: u32 = 0xDEAD_BEEF;
    let mut euid: u32 = 0xDEAD_BEEF;
    let mut suid: u32 = 0xDEAD_BEEF;
    let req = SyscallRequest::new(
        NR_GETRESUID,
        [
            &mut ruid as *mut u32 as u64,
            &mut euid as *mut u32 as u64,
            &mut suid as *mut u32 as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(ruid, 1000);
    assert_eq!(euid, 1001);
    assert_eq!(suid, 1002);
}

/// Gid analog of the getresuid round-trip.
#[test]
fn dispatch_getresgid_writes_three_u32s() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let outcome = tx_subsystems::cred::step_setresgid(
        &proc_cap,
        Some(tx_subsystems::cred::Gid(2000)),
        Some(tx_subsystems::cred::Gid(2001)),
        Some(tx_subsystems::cred::Gid(2002)),
    );
    assert!(matches!(
        outcome,
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    let ctx = make_ctx(proc_cap, thread);

    let mut rgid: u32 = 0xDEAD_BEEF;
    let mut egid: u32 = 0xDEAD_BEEF;
    let mut sgid: u32 = 0xDEAD_BEEF;
    let req = SyscallRequest::new(
        NR_GETRESGID,
        [
            &mut rgid as *mut u32 as u64,
            &mut egid as *mut u32 as u64,
            &mut sgid as *mut u32 as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(rgid, 2000);
    assert_eq!(egid, 2001);
    assert_eq!(sgid, 2002);
}

/// `getresuid(NULL, NULL, NULL)` returns `0` and writes nothing.
/// Confirms the NULL-skip arm doesn't fault when given zero
/// pointers (kernel-side bootstrap exemption applies).
#[test]
fn dispatch_getresuid_null_pointers_skip_writes() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETRESUID, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}
