//! PR-9 phase 5 (D5 Path A): zone-allocated `Cred` integration tests.
//!
//! Pin the new mutation semantics: every cred-mutator
//! (`step_setuid` / `step_setgid` / ...) reserves a fresh `Cap<Cred>`
//! and atomically swaps the per-payload `AtomicSlot<Cap<Cred>>`. The
//! previous cap drops at the mutator's stack frame exit and the slab
//! entry is EBR-retired when concurrent readers' guards complete.
//!
//! These tests live outside `crates/tx-subsystems/src/cred/tests.rs`
//! because they pin the **cap-shape** semantics specifically — the
//! lock-vs-slot atomicity story and the pre-mutation cap holding
//! pre-mutation cred across a publication boundary. The in-module
//! tests already cover the privilege rules and the cred values.
//!
//! Invariants pinned (bundled into one `#[test]` because the
//! `reset_*_for_test` helpers are `pub(crate)` and not visible from
//! integration-test binaries; a bootstrap-once-then-fork structure
//! gives us full coverage without resets):
//!
//! 1. **fresh-cred-cap-on-bootstrap**. `bootstrap_init_process` mints
//!    a fresh `Cap<Cred>` for the new process.
//! 2. **mutator-publishes-fresh-cap**. After `step_setuid` returns
//!    `CredChange::Replaced`, the slot's cap key differs from the
//!    pre-mutation cap key (a new slab entry is in place).
//! 3. **pre-mutation-cap-still-derefs-to-pre-cred**. Holding a
//!    `Cap<Cred>` clone across a `step_setuid` call must keep
//!    yielding the pre-mutation value — the SUBJ-3-style publication
//!    boundary preserves prior readers' view.
//! 4. **fork-child-gets-independent-cap**. `step_fork` mints a fresh
//!    `Cap<Cred>` for the child; parent and child cap keys must
//!    differ even when their `Cred` values match (D5 constraint #3:
//!    don't share the cap).
//! 5. **cred-cap-accessor-shape**. `ProcessIdentity::cred_cap()`
//!    returns `Some(cap)` for live processes and `None` for zombies.

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_substrate::epoch;
use tx_substrate::testing::init_host_for_test_once;
use tx_substrate::zone::Cap;

use tx_subsystems::cred::{step_setuid, CredChange, Uid};
use tx_subsystems::process::{bootstrap_init_process, step_exit_group, step_fork, ExitStatus};
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

/// Local stub pmap (this integration test cannot see `vm::TestPmap`,
/// which is `pub(crate)` and `cfg(test)`-gated). Mirrors the shape
/// of the tx-substrate integration-test pmaps but only implements
/// what `AddressSpace::new_cap_for_platform` / `step_fork` exercise.
struct StubPmap;

static NEXT_ROOT_ID: AtomicUsize = AtomicUsize::new(1);

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let id = NEXT_ROOT_ID.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * 4096)),
            Asid(id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }

    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }

    fn protect_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Ok(Some(PmapInvalidation::new(virt, kind.size())))
    }

    fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {}

    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

/// Single integration test that bootstraps init once and exercises
/// every cred-cap invariant from there.
#[test]
fn cred_zone_allocation_invariants_round_trip() {
    init_host_for_test_once();
    let _ = zones::register_all();
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);

    let init = bootstrap_init_process(fresh_aspace()).expect("bootstrap");

    // (1) Fresh cred cap on bootstrap: deref to `Cred::root()`.
    let bootstrap_cap = init.cred_cap().expect("alive process has cred cap");
    assert!(bootstrap_cap.uid.is_root());
    assert!(bootstrap_cap.euid.is_root());

    // (4) Fork-child gets independent cap.
    let parent_key_pre_fork = bootstrap_cap.key();
    let child = step_fork::<StubPmap>(&init).expect("fork");
    let child_cap = child.cred_cap().expect("alive");
    let child_key = child_cap.key();
    assert_ne!(parent_key_pre_fork, child_key, "fork must mint a fresh cap");
    // Values match (fork copies the parent's cred snapshot).
    assert_eq!(*bootstrap_cap, *child_cap);

    // (2 + 3) Mutator publishes a fresh cap; the prior cap still
    // derefs to the pre-mutation cred (publication boundary).
    let pre_cap = init.cred_cap().expect("alive");
    let pre_key = pre_cap.key();
    let pre_value = *pre_cap;
    assert!(pre_value.uid.is_root());

    match step_setuid(&init, Uid(7777)) {
        CredChange::Replaced { new, .. } => {
            assert_eq!(new.uid, Uid(7777));
        }
        other => panic!("expected Replaced, got {other:?}"),
    }

    let post_cap = init.cred_cap().expect("alive");
    let post_key = post_cap.key();
    assert_ne!(pre_key, post_key, "slot should hold a fresh cap");
    assert_eq!(post_cap.uid, Uid(7777));

    // Critically: the cap we cloned *before* the mutation still
    // derefs to the pre-mutation cred. EBR keeps the slab entry live
    // as long as `pre_cap` is held.
    let still_pre = *pre_cap;
    assert_eq!(still_pre, pre_value);
    assert!(still_pre.uid.is_root());

    // (5) `cred_cap()` returns `None` for zombies. Zombify the child
    // via `step_exit_group`; the identity stays observable (parent
    // retains it for `waitpid`) but the payload — and therefore the
    // cred-slot — is dropped.
    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie(), "step_exit_group zombifies the child");
    assert!(
        child.cred_cap().is_none(),
        "zombies have no cred cap (payload dropped)"
    );

    // Final drop housekeeping — let EBR retire the caps we held.
    drop(bootstrap_cap);
    drop(pre_cap);
    drop(post_cap);
    drop(child_cap);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
}
