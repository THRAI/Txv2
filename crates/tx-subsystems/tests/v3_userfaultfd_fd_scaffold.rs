//! PR-10 phase 0 — `UserfaultFd` zone + `OpenFile::Ufd` fd-table
//! scaffold integration tests.
//!
//! Spec: `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//! (phase plan), `docs/Txv3/05_DELEGATE_v1.md` §8.1.
//!
//! Phase 0 is intentionally scope-limited: this file only pins the
//! fd-table identity of a userfaultfd-backed `OpenFile`. The real
//! userfaultfd semantics (`UFFDIO_API`, `UFFDIO_REGISTER`,
//! fault-path interception, `UFFDIO_COPY` / `UFFDIO_ZEROPAGE`) land
//! in P-10.1..P-10.7.
//!
//! Invariants pinned:
//!
//! 1. **zone-allocation works**. `UserfaultFd::new_cap` returns a
//!    `Cap<UserfaultFd>` resolving to a live slot with a stable
//!    `ufd_id`.
//! 2. **OpenFile::new_userfaultfd_cap respects the new backing**.
//!    The returned `Cap<OpenFile>` reports
//!    `OpenFileBacking::Ufd` (not `Rnode`) and surfaces the inner
//!    `Cap<UserfaultFd>` via `OpenFile::ufd()`.
//! 3. **fd-table install + retrieve round-trip**. A
//!    userfaultfd-backed `OpenFile` installs cleanly at a chosen fd
//!    on a real `ProcessIdentity`, and `proc.fd(fd)` returns the
//!    same `Cap<OpenFile>` (preserving the inner ufd identity).
//! 4. **close drops the ufd**. Removing the fd → dropping the last
//!    `Cap<OpenFile>` → the inner `Cap<UserfaultFd>` slot is
//!    reclaimed by EBR. Verified via `Weak::upgrade` returning
//!    `None` after the close + drain.

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_subsystems::userfaultfd::adapter::step_engine::{
    guard as ebr_guard, Cap, Errno, StepOutcome,
};

use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::userfaultfd::UserfaultFd;
use tx_subsystems::vfs::structure::{OpenFile, OpenFileBacking, OpenFileFlags};
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Local stub pmap — `vm::TestPmap` is `pub(crate)` and not visible
/// from integration-test binaries. Mirrors the cred zone test's
/// `StubPmap` (see `v3_cred_zone_allocation.rs`).
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

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

fn bootstrap_process() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

fn ufd_open_file_flags() -> OpenFileFlags {
    // userfaultfd(2) opens a read-only fd at the syscall level (the
    // agent thread `read(2)`s fault messages from it; UFFDIO_*
    // ioctls deliver the reply). For phase 0 the flags are not yet
    // consulted by any dispatch path — set conservative defaults.
    OpenFileFlags {
        read: true,
        write: false,
        append: false,
        cloexec: false,
        nonblocking: false,
        packet: false,
    }
}

/// Integration test bundle — bootstrapping init twice across tests
/// trips `BootstrapError::AlreadyBootstrapped` because the
/// `reset_*_for_test` helpers are `pub(crate)` and invisible here
/// (matching the pattern in `v3_cred_zone_allocation.rs`). We run
/// every assertion under a single `#[test]` to keep the bootstrap
/// state machine linear.
#[test]
fn userfaultfd_phase0_fd_scaffold_invariants_round_trip() {
    let _g = setup();

    // (1) zone-allocation: a fresh ufd cap resolves to a live slot
    //     with a positive monotonic id.
    let ufd_a = UserfaultFd::new_cap().expect("ufd cap a");
    let id_a = ufd_a.ufd_id();
    assert!(id_a > 0, "ufd_id must be a positive monotonic id");

    // Distinct caps mint distinct ids.
    let ufd_b = UserfaultFd::new_cap().expect("ufd cap b");
    assert_ne!(
        ufd_a.ufd_id(),
        ufd_b.ufd_id(),
        "each UserfaultFd must mint a fresh ufd_id",
    );
    drop(ufd_b);

    // (2) OpenFile::new_userfaultfd_cap surfaces the ufd backing
    //     and the inner Cap<UserfaultFd> identity.
    let file =
        OpenFile::new_userfaultfd_cap(ufd_a, ufd_open_file_flags()).expect("ufd openfile cap");
    match file.backing() {
        OpenFileBacking::Ufd { ufd } => {
            assert_eq!(ufd.ufd_id(), id_a);
        }
        OpenFileBacking::Rnode { .. }
        | OpenFileBacking::AioContext { .. }
        | OpenFileBacking::SignalFd { .. }
        | OpenFileBacking::IoUring { .. }
        | OpenFileBacking::Epoll { .. }
        | OpenFileBacking::Eventfd { .. }
        | OpenFileBacking::Timerfd { .. }
        | OpenFileBacking::PosixMq { .. }
        | OpenFileBacking::Pidfd { .. }
        | OpenFileBacking::KernelObject { .. }
        | OpenFileBacking::MountApi { .. }
        | OpenFileBacking::SocketPair { .. } => {
            panic!("expected OpenFileBacking::Ufd")
        }
    }
    let inner = file
        .ufd()
        .expect("ufd accessor must return Some for an Ufd-backed OpenFile");
    let inner_weak = inner.downgrade();
    assert_eq!(inner.ufd_id(), id_a);

    // (3) fd-table install + retrieve round-trip preserves identity.
    let proc_cap = bootstrap_process();
    let chosen_fd: u32 = 42;
    let prev = proc_cap.install_fd(chosen_fd, file);
    assert!(prev.is_none(), "fd {chosen_fd} must start empty");

    let retrieved = proc_cap.fd(chosen_fd).expect("fd 42 must be installed");
    let retrieved_inner = retrieved
        .ufd()
        .expect("retrieved openfile must report ufd backing");
    assert_eq!(
        retrieved_inner.ufd_id(),
        id_a,
        "fd-table round-trip must preserve ufd identity",
    );
    drop(retrieved);

    // (4) close: removing the fd returns the previous occupant; the
    //     `Cap<OpenFile>` we got back is the last retain. Dropping
    //     it queues the OpenFile + the inner Cap<UserfaultFd> for
    //     EBR retirement. After drain_to_quiescence(), the Weak we
    //     stamped above must fail to upgrade.
    let closed = proc_cap
        .set_fd(chosen_fd, None)
        .expect("occupant was present");
    assert_eq!(closed.ufd().expect("ufd").ufd_id(), id_a);
    drop(closed);
    tx_test_support::drain_to_quiescence();

    let guard = ebr_guard();
    assert!(
        inner_weak.upgrade(&guard).is_none(),
        "ufd zone slot must be reclaimed after the last OpenFile drops",
    );
    drop(guard);

    // And the fd table no longer reports the slot.
    assert!(proc_cap.fd(chosen_fd).is_none());

    // (5) VFS step_read against a ufd-backed OpenFile returns
    //     EINVAL (phase 0 placeholder — the real agent-side read
    //     path lands in P-10.5).
    let ufd_c = UserfaultFd::new_cap().expect("ufd cap c");
    let file_c =
        OpenFile::new_userfaultfd_cap(ufd_c, ufd_open_file_flags()).expect("openfile cap c");
    let mut buf = [0u8; 8];
    let guard = ebr_guard();
    let outcome = file_c.step_read(&mut buf, &guard);
    drop(guard);
    match outcome {
        StepOutcome::Err(Errno::EINVAL) => {}
        other => panic!("expected v3 Err(EINVAL), got {other:?}"),
    }
    drop(file_c);
    tx_test_support::drain_to_quiescence();
}
