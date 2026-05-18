//! PR-9 phase 5 (D5 Path A): subject-population integration tests.
//!
//! Pin the wiring from `SyscallCtx` into `KernelScriptCtx`'s subject
//! slot. Phase 3b threaded an *empty* `KernelScriptCtx::new()` into
//! the four wired syscall arms (sys_read, sys_write, sys_pipe2,
//! sys_clone); phase 5 replaces that with
//! `KernelScriptCtx::new().with_subject(SubjectContext::from_thread(
//!     ctx.process.clone(), ctx.thread.clone(),
//!     SubjectAuthority::new(cred_cap, restrictions_placeholder),
//! ))`.
//!
//! This test exercises the production helper
//! `tx_shims::linux_syscall::build_subject_script_ctx`, which is the
//! shared seam each of the four arms calls at entry. Asserts that the
//! returned `KernelScriptCtx`:
//!
//! 1. has a `Some(subject)` (not the phase-3b empty context),
//! 2. carries the calling process+thread caps,
//! 3. carries a `Cap<Cred>` whose deref matches the calling process's
//!    current cred (i.e. wired through `SyscallCtx::cred_cap`).
//!
//! The restrictions placeholder is a substrate-side placeholder until
//! PR-K lands the real append-only stack (D5 §7); we assert only that
//! the cap is *present*, not its value.

extern crate alloc;

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_shims::adapter::step_engine::Cap;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::{build_subject_script_ctx, SyscallCtx};

// -------- Stub pmap (lifted from tx-shims tests.rs) ----------------

struct StubPmap;

impl PlatformConfig for StubPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "shims-v3-test";
}

#[derive(Default)]
struct StubPmapState {
    next_root: usize,
}

static STUB_PMAP_STATE: LazyLock<Mutex<StubPmapState>> =
    LazyLock::new(|| Mutex::new(StubPmapState { next_root: 1 }));

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = STUB_PMAP_STATE.lock().expect("stub pmap lock");
        let id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * USER_PAGE_SIZE)),
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
}

impl EntropyIf for StubPmap {}
impl tx_hal::AuxvIf for StubPmap {}

// -------- Setup -----------------------------------------------------

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    proc_cap
        .nth_thread(0)
        .expect("alive process has leader thread")
}

fn make_ctx<'a>(process: Cap<ProcessIdentity>, thread: Cap<ThreadIdentity>) -> SyscallCtx<'a> {
    let aspace = process.aspace_cap().expect("alive aspace");
    SyscallCtx::new(process, thread, aspace)
}

// -------- The test --------------------------------------------------

#[test]
fn build_subject_script_ctx_populates_a_non_empty_subject() {
    let _g = setup();

    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread_cap = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread_cap.clone());

    let script_ctx = build_subject_script_ctx(&ctx);

    // (1) Subject is populated (phase-3b was `None`).
    let subject = script_ctx
        .subject()
        .expect("phase-5 must populate the subject slot");

    // (2) Calling process+thread caps thread through. Cap equality is
    // by slot key, so cloning the same cap yields the same key.
    assert_eq!(subject.process().key(), proc_cap.key());
    assert_eq!(
        subject
            .thread()
            .expect("from_thread sets thread slot")
            .key(),
        thread_cap.key(),
    );

    // (3) Authority carries a `Cap<Cred>` matching the process's
    // current cred snapshot. Bootstrap init is root.
    let cred_cap = subject.authority().cred();
    assert!(cred_cap.uid.is_root());
    assert!(cred_cap.euid.is_root());

    // (3b) The restrictions cap is present (placeholder shape until
    // PR-K). We don't assert its slot key — it's per-call placeholder.
    let _restrictions_cap = subject.authority().restrictions();
}
