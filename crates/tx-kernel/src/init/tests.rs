//! Phase 3b boot-wiring tests.
//!
//! Exercises the new ordered steps `register_console_hardware` →
//! `mount_rootfs_from_boot_media` → `mount_devfs_at_dev` →
//! `register_devfs_console_alias` → `bind_init_cwd_and_root`. The
//! tests bypass `init_substrate_if_ready` (which depends on the full
//! HAL `BootHandoff` shape) and call each method on a stub
//! `TxPlatform` directly.
//!
//! End-to-end mount-walker assertions (`step_lookup("/dev/console")`)
//! are downgraded per the trio plan §"Phase 3b tests": the VFS walker
//! / `step_open` infrastructure is deferred. The tests instead assert
//! the substrate-level facts the trio actually pins down — global
//! mount slots populate, tmpfs's `/dev` mkdir succeeds, devfs's
//! console alias resolves, and init's cwd + fds 0/1/2 are bound.

use core::sync::atomic::{AtomicUsize, Ordering};

use std::sync::Mutex;

use tx_hal::{
    AllocError, Arch, Asid, BootHandoff, BootInfo, BootPlatformIf, BootProtocol, ConsoleIf, InitIf,
    PhysAddr, PlatformConfig, PlatformInfo, PmapError, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PtNode,
};

/// Page size used to fabricate distinct test pmap roots. tx-hal
/// surfaces `PAGE_SIZE` through `Arch` rather than as a free constant;
/// hardcoding `4096` keeps this test isolated to the boot-wiring
/// surface.
const TEST_PAGE_SIZE: usize = 4096;

use crate::init::{console_tty, dev_mount, root_mount, CoreInit};

use crate::adapter::step_engine::{self as step_engine, guard, page_allocator, StepOutcome};
/// Serialise every test in this module against the rest of tx-kernel's
/// test set: they all touch the global `INIT_PROCESS` / mount / TTY
/// slots plus the per-CPU epoch domain (which forbids guard nesting
/// on the same CPU). One lock keeps the kernel test set
/// deterministic; same shape `tx-fs` uses.
use crate::test_serialise::KERNEL_TEST_LOCK as INIT_TEST_LOCK;

/// Test-only platform satisfying every `TxPlatform` super-trait. The
/// pmap surface uses a host-side `Mutex`-guarded `BTreeMap` mirroring
/// `tx_subsystems::vm::TestPmap`'s shape; the rest are no-ops because
/// the boot-wiring tests don't touch reactor / SMP / time paths.
struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-kernel-init-test";
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

static EMPTY_BOOT_INFO: BootInfo = BootInfo::empty();
static TEST_PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: TestPlatform::BOARD,
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl tx_hal::BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &EMPTY_BOOT_INFO
    }
}

impl tx_hal::PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &TEST_PLATFORM_INFO
    }
}

impl tx_hal::AuxvIf for TestPlatform {}

/// Capture every byte the kernel writes to the platform console.
/// Lets tests assert that writes through the preopened fds 1/2 reach
/// the underlying transport — no need for a fake `CharDeviceBinding`,
/// because `register_console_hardware` already wires the
/// `ConsoleCharOps::<TestPlatform>` instance to forward into
/// `ConsoleIf::write_bytes`.
static CONSOLE_CAPTURED_LEN: AtomicUsize = AtomicUsize::new(0);

/// Byte-level capture of every `ConsoleIf::write_bytes` call.
/// Phase 6's end-to-end smoke asserts the post-OPOST byte stream
/// (`b"hi\r\n"`); the `len`-only counter above is kept for the
/// existing Phase 3b test which doesn't pin the exact bytes.
static CONSOLE_CAPTURED_BYTES: Mutex<std::vec::Vec<u8>> = Mutex::new(std::vec::Vec::new());

impl ConsoleIf for TestPlatform {
    fn write_bytes(bytes: &[u8]) {
        CONSOLE_CAPTURED_LEN.fetch_add(bytes.len(), Ordering::AcqRel);
        CONSOLE_CAPTURED_BYTES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
    }
}

/// Snapshot of the most recent merged `UserTrapContext` produced by
/// `prepare_userspace_entry_payload` and handed to the Phase-7
/// simulator override. The smoke's primary correctness assertion:
/// the merged `a0` register matches the `pending_syscall_return`
/// value the dispatcher recorded (Plan B writeback discipline).
static LAST_USERSPACE_CTX: Mutex<Option<tx_hal::UserTrapContext>> = Mutex::new(None);

/// Per-call capture of `a0` so the smoke can assert the writeback
/// across every `enter_userspace_with_context` invocation in the
/// test. Reset only by the next `setup()` — persists across the
/// per-iteration `run_thread` invocations that the smoke chains.
static USERSPACE_A0_LOG: Mutex<std::vec::Vec<usize>> = Mutex::new(std::vec::Vec::new());

impl tx_hal::TrapIf for TestPlatform {
    /// Inverted-loop simulator: stand in for a real `sret` into
    /// userspace.
    ///
    /// In production this method's body diverges via `sret` into
    /// user mode and returns only when the trap handler chooses
    /// `TrapAction::Reschedule` (the platform's trap-shell longjmps
    /// back to this call site, restoring the kernel sp / ra / s-regs
    /// the userspace-entry shim stashed before sret).
    ///
    /// In the host smoke we **return immediately** after recording
    /// the merged `UserTrapContext`. With the inverted `run_thread`
    /// loop (`crates/tx-kernel/src/thread_future.rs`), the
    /// caller's next step is `entry_wait.await`, which yields
    /// `Poll::Pending` because no trap-shell has resolved the wait
    /// yet. The driver then resolves the wait with a scripted
    /// `UserspaceTrapInfo` and re-polls — exactly the production
    /// sequencing (trap arrives → trap shell resolves wait + Reschedule
    /// → `enter_userspace_with_context` returns → future awaits the
    /// resolved wait → match-and-dispatch).
    ///
    /// Behaviour:
    /// 1. Snapshot the merged `UserTrapContext` into
    ///    `LAST_USERSPACE_CTX` and log `regs[10]` (the `a0` slot,
    ///    RV64 ABI) into `USERSPACE_A0_LOG` so the smoke can
    ///    assert Plan B writeback discipline produced the right
    ///    merged value.
    /// 2. Return.
    fn enter_userspace_with_context(ctx: &tx_hal::UserTrapContext, _root: &tx_hal::PmapRoot) {
        *LAST_USERSPACE_CTX.lock().unwrap_or_else(|e| e.into_inner()) = Some(*ctx);
        USERSPACE_A0_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ctx.regs[10]);
    }
}
impl tx_hal::SignalFrameIf for TestPlatform {}
impl tx_hal::IrqIf for TestPlatform {}

impl tx_hal::TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl tx_hal::PercpuIf for TestPlatform {}
impl tx_hal::CacheIf for TestPlatform {}
impl tx_hal::DmaIf for TestPlatform {}
impl tx_hal::SmpIf for TestPlatform {}

impl tx_hal::EntropyIf for TestPlatform {}

impl tx_hal::PowerIf for TestPlatform {
    fn system_off() -> ! {
        #[allow(clippy::empty_loop)]
        loop {}
    }
}

// --- minimal pmap test impl ---------------------------------------

static TEST_PMAP_NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);

impl tx_hal::PmapIf for TestPlatform {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let root_id = TEST_PMAP_NEXT_ROOT.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * TEST_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: tx_hal::VirtAddr,
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

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        // Boot-wiring tests don't traverse intermediate page-table
        // nodes; AddressSpace creation and root-pmap allocation are
        // sufficient. A real allocator returns `Exhausted` here
        // rather than fabricating a bogus PtNode (which would risk
        // silent acceptance of unintended mapping work).
        Err(AllocError::Exhausted)
    }
}

// --- test setup ---------------------------------------------------

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = INIT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = tx_subsystems::zones::register_all();
    // Reset every global slot the boot wiring touches. The
    // `cross_crate_test_support::reset_*` helpers are gated on the
    // `test-support` feature (enabled by tx-kernel's
    // `[dev-dependencies]`).
    tx_subsystems::cross_crate_test_support::reset_init_process();
    tx_subsystems::cross_crate_test_support::reset_pid_counter();
    tx_subsystems::cross_crate_test_support::reset_tid_counter();
    tx_subsystems::cross_crate_test_support::reset_mount_table();
    tx_subsystems::cross_crate_test_support::reset_mount_id_counter();
    tx_subsystems::cross_crate_test_support::reset_dev_id_counter();
    tx_subsystems::cross_crate_test_support::reset_reactor_submit_seam();
    crate::init::reset_boot_state_for_test();
    crate::irq::reset_dispatch_table_for_test();
    tx_subsystems::device::reset_block_registry_for_test();
    CONSOLE_CAPTURED_LEN.store(0, Ordering::Release);
    CONSOLE_CAPTURED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    // Phase 7 smoke statics — clear so a previous run's captures
    // don't bleed into the next test's assertions.
    *LAST_USERSPACE_CTX.lock().unwrap_or_else(|e| e.into_inner()) = None;
    USERSPACE_A0_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    guard
}

fn bootstrap_init() {
    // Mirror what `init_process_subsystem` does, without going
    // through the full `init_substrate_if_ready` shell (which
    // requires a real `BootHandoff` + reactor smoke flow).
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<TestPlatform>()
        .expect("test aspace");
    let _init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");
}

fn drive_boot_wiring() {
    bootstrap_init();
    CoreInit::<TestPlatform>::register_console_hardware();
    // Pre-ELF Phase 5 (item 9): mirrors the production boot order.
    // `TestPlatform`'s `IrqIf` impl uses the trait-default
    // `UART_IRQ = 0`, which `install_irq_handlers` accepts without
    // wiring an actual unmask (TestPlatform's `unmask` is a no-op).
    // The dispatch table still gets published, exercising the
    // platform-publication path in the boot-wiring smoke.
    CoreInit::<TestPlatform>::install_irq_handlers();
    CoreInit::<TestPlatform>::init_block_devices();
    CoreInit::<TestPlatform>::mount_rootfs_from_boot_media();
    CoreInit::<TestPlatform>::mount_devfs_at_dev();
    CoreInit::<TestPlatform>::register_devfs_console_alias();
    CoreInit::<TestPlatform>::bind_init_cwd_and_root();
}

// --- tests --------------------------------------------------------

/// **Downgrade note (per trio plan §"Phase 3b tests"):** end-to-end
/// `step_lookup("/dev/console")` cannot run yet — VFS's walker
/// (`step_open` etc.) is deferred. The downgraded shape asserts:
/// - tmpfs root mount cap is in `ROOT_MOUNT`,
/// - devfs mount cap is in `DEV_MOUNT` and points at the rootfs's
///   `/dev` DEntry as its mountpoint,
/// - the registered TTY's devfs alias resolves through the live
///   registry (`tty::project::resolve_devfs_alias(b"console")`).
#[test]
fn boot_smoke_mounts_root_and_dev_and_resolves_console() {
    let _serial = setup();
    drive_boot_wiring();

    // Root mount populates and points at a tmpfs payload over the
    // root rnode.
    let root = root_mount().expect("ROOT_MOUNT must be populated");
    assert_eq!(
        root.payload_cap()
            .expect("root payload alive")
            .into_cap()
            .fstype,
        "tmpfs"
    );
    assert_eq!(
        root.root().fs_object_id(),
        tx_fs::tmpfs::TMPFS_ROOT_OBJECT_ID
    );
    // Rootfs is the namespace root: no parent, no mountpoint
    // dentry.
    assert!(root.parent().is_none());
    assert!(root.mountpoint().is_none());

    // Devfs mount populates and is parented at the rootfs.
    let dev = dev_mount().expect("DEV_MOUNT must be populated");
    assert_eq!(
        dev.payload_cap()
            .expect("dev payload alive")
            .into_cap()
            .fstype,
        "devfs"
    );
    assert_eq!(
        dev.root().fs_object_id(),
        tx_fs::devfs::DEVFS_ROOT_OBJECT_ID
    );
    let dev_parent = dev.parent().expect("dev mount must have rootfs parent");
    assert_eq!(dev_parent.id(), root.id());
    let dev_mountpoint = dev
        .mountpoint()
        .expect("dev mount must have mountpoint dentry");
    // Mountpoint dentry's name is `dev`.
    assert_eq!(dev_mountpoint.name().as_bytes(), b"dev");

    // The console alias resolves: `register_devfs_console_alias`
    // republished `console` against the boot console TTY.
    let resolved = tx_subsystems::tty::project::resolve_devfs_alias(b"console")
        .expect("console alias must resolve");
    let registered = console_tty().expect("CONSOLE_TTY must be populated");
    assert_eq!(resolved, registered);
}

/// Phase 6: assert that the VFS walker resolves `/dev/console`
/// end-to-end after `mount_devfs_at_dev` registers the mount via
/// `mount::register_mount`. Pre-Phase-6 the walker would fail at the
/// rootfs→devfs crossing (no entry in the mount-point registry) and
/// `open_console_for_init` would fall back to its legacy direct-RNode
/// path. With the registration wired the walker now succeeds, the
/// fallback is unreachable on the boot path, and the terminal DEntry's
/// RNode is a `StructBacked { Tty(...) }` for the boot console.
#[test]
fn boot_smoke_walker_resolves_dev_console_after_mount_registration() {
    use tx_subsystems::vfs::{walker, Credential, RNodeBacking, StructPayload};

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let cwd = init.cwd().expect("init cwd must be bound");
    let cred = Credential::root();
    let guard = guard();
    use step_engine::StepOutcome as V3;
    let outcome = block_on(walker::step_walk(cwd, b"/dev/console", &cred, &guard));
    drop(guard);

    let dentry = match outcome {
        V3::Done(d) => d,
        other => {
            panic!("step_walk(/dev/console) must succeed after mount registration, got {other:?}",)
        }
    };
    assert_eq!(dentry.name().as_bytes(), b"console");

    // The terminal RNode must be a StructBacked Tty matching the
    // registered console TTY — proving the walker materialised
    // through devfs's `materialise_rnode` hook and not via the
    // legacy direct-RNode bootstrap path.
    let registered = console_tty().expect("CONSOLE_TTY must be populated");
    match dentry.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => {
            assert_eq!(*tty, registered);
        }
        other => panic!("expected StructBacked Tty backing, got {other:?}"),
    }
}

#[test]
fn boot_smoke_init_fds_preopened_to_console() {
    let _serial = setup();
    drive_boot_wiring();

    let init =
        tx_subsystems::process::execution::init_process().expect("INIT_PROCESS must be populated");

    // Each of fds 0/1/2 is bound to a console-shaped OpenFile.
    for fd in 0..3 {
        let file = init
            .fd(fd)
            .unwrap_or_else(|| panic!("init.fd({fd}) must be Some after bind_init_cwd_and_root"));
        // Sanity: the OpenFile is read+write (per
        // `open_console_for_init`).
        assert!(file.flags().read);
        assert!(file.flags().write);
    }

    // Drive a write through fd 1 — the bytes must reach the
    // platform console (`TestPlatform::write_bytes` increments
    // `CONSOLE_CAPTURED_LEN`).
    let baseline = CONSOLE_CAPTURED_LEN.load(Ordering::Acquire);
    let stdout = init.fd(1).expect("fd 1");
    let guard = guard();
    match stdout.step_write(b"hi\n", &guard) {
        StepOutcome::Done(written) => {
            assert_eq!(written, 3, "step_write reports the requested byte count");
        }
        other => panic!("fd 1 step_write failed: {other:?}"),
    }
    drop(guard);

    // The TTY ldisc rewrites LF to CR LF (ONLCR) before the bytes
    // hit the platform console; we only assert that *some* bytes
    // arrived, not the exact post-cooking count, because tx-fs's
    // devfs tests already pin the ONLCR shape.
    let after = CONSOLE_CAPTURED_LEN.load(Ordering::Acquire);
    assert!(
        after > baseline,
        "TestPlatform::write_bytes must observe the fd-1 write \
         (baseline={baseline}, after={after})"
    );

    // init's cwd is now Some — `bind_init_cwd_and_root` ran
    // `step_chdir`. We can't observe the dentry directly without
    // the ChdirOutcome's `prev` (which `bind_init_cwd_and_root`
    // discards), but `step_getcwd` returns `Some` byte-vec when
    // the cwd is set.
    let cwd_bytes =
        tx_subsystems::process::execution::step_getcwd(&init).expect("init cwd must be set");
    assert_eq!(
        cwd_bytes, b"/",
        "init's rendered cwd path is the namespace root"
    );
}

// ---------------------------------------------------------------------------
// Phase 7 — end-to-end production-path userspace round-trip smoke
// ---------------------------------------------------------------------------
//
// Replaces the trio's Phase 6 "fake driver" smoke (deleted alongside
// this module's introduction). Where Phase 6 manually orchestrated
// `start_request` / `complete_interesting_trap` / `dispatch` /
// `pending_syscall_return` drains by hand, Phase 7 drives the
// real production code path:
//
//   - `crate::thread_future::run_thread::<TestPlatform>` — the
//     production thread future (`txdoc:THREAD-4-1-SHAPE`).
//   - `crate::thread_future::PerHartSlotted` — the per-hart slot
//     adapter that brackets every poll.
//   - `tx_shims::linux_syscall::dispatch` — the real syscall
//     dispatcher (write → `step_write` → tty / devfs walker →
//     console; exit_group → `step_exit_group` → process zombie).
//   - `tx_subsystems::thread_runtime::execution::prepare_userspace_entry_payload`
//     — Plan B writeback discipline
//     (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`); the merged
//     `UserTrapContext` lands in `LAST_USERSPACE_CTX` for assertion.
//
// Simulator strategy (post loop-inversion, 2026-05-08).
// =====================================================
//
// `run_thread`'s loop is now ENTER-FIRST: each iteration opens an
// entry-side wait, runs the AST checkpoint, calls
// `prepare_userspace_entry_payload`, dives into userspace via
// `enter_userspace_with_context`, and only THEN awaits the
// (typically already-resolved) wait to receive the trap.
//
// On a real platform `enter_userspace_with_context` returns when
// the trap shell longjmps back on `TrapAction::Reschedule`; the
// wait was resolved before the longjmp, so `entry_wait.await` is
// a fast Poll::Ready pop.
//
// In the host smoke:
//   1. The TestPlatform override of `enter_userspace_with_context`
//      records the merged `UserTrapContext` and **returns** (no
//      panic — see the impl above).
//   2. The next step inside `run_thread` is `entry_wait.await`,
//      which yields `Poll::Pending` because no driver-injected
//      trap has resolved the wait yet.
//   3. The driver inspects `LAST_USERSPACE_CTX` (the merged ctx
//      this iteration's userspace would see), then resolves the
//      wait via `complete_interesting_trap` with the scripted
//      `UserspaceTrapInfo`.
//   4. Re-poll runs the match-and-dispatch, falls through to the
//      top of the loop, opens a fresh entry-side wait, and the
//      cycle repeats. A single long-lived future drives the
//      whole script — no per-iteration future invocation needed.
//
// `exit_group` short-circuits the dispatch arm with
// `SyscallResult::NoReturn`, so the future returns cleanly with
// `Poll::Ready(())` without a second `enter_userspace_with_context`
// call.
//
// Coverage:
//   [✓] Console captures `b"hi\r\n"` post-OPOST.
//   [✓] init zombifies with `ExitStatus::Exited(0)`.
//   [✓] `live_thread_count() == 0`.
//   [✓] Production `run_thread` future drives the whole script.
//   [✓] `PerHartSlotted` adapter installs/clears the per-hart slot
//       around each poll.
//   [✓] `prepare_userspace_entry_payload` produces the merged
//       `UserTrapContext`; the smoke asserts `regs[10]` equals
//       the dispatcher's encoded return (Plan B writeback
//       discipline correctness check).
//   [✓] Walker-driven write path: fd 1 was preopened through
//       `step_open(/dev/console)` so the write travels devfs
//       `materialise_rnode` → tty → ConsoleIf::write_bytes.
//
// What is still not covered (deferred to a real-board slice):
//   [-] Real `sret` into a userspace binary.
//   [-] Real trap shell delivery (`KernelTrapDispatcher::on_syscall`
//       chain). Phases 5/6 test that path separately.
//   [-] The platform's actual reschedule longjmp (host platform's
//       `enter_userspace_with_context` is a recording no-op rather
//       than a real round-trip).

/// Inverted-loop deliverable. Drives a single long-lived
/// `run_thread` future across a scripted sequence of userspace
/// traps and asserts the production code path's effects:
///
///   - **Iteration 1 (the pre-write dive).** Future opens an
///     entry-side wait, runs the AST checkpoint, calls
///     `prepare_userspace_entry_payload` (drains pending=None on
///     the first iteration), and dives into userspace via
///     `enter_userspace_with_context`. The override records the
///     merged context (`a0` = 0, the seeded baseline) and
///     returns. Future then awaits `entry_wait` and yields
///     `Poll::Pending`.
///   - **Driver injects write(1, "hi\n", 3).** Resolves the wait
///     by calling `complete_interesting_trap` on the active
///     request token, with a baseline `saved_user_context`.
///   - **Iteration 2 (the post-write dive).** Future resumes,
///     dispatches `write` (walker → tty → ConsoleIf::write_bytes),
///     stores `pending_syscall_return = Ok(3)`, falls through to
///     the loop top. Opens a fresh wait, runs the AST checkpoint,
///     calls `prepare_userspace_entry_payload` which now merges
///     `Ok(3)` into `a0`, and dives again. The override records
///     `a0` = 3 and returns. Future awaits `entry_wait` →
///     `Poll::Pending`.
///   - **Driver injects exit_group(0).** Resolves the wait.
///   - **Iteration 3 (the exit_group dive).** Future dispatches
///     `exit_group` → `SyscallResult::NoReturn` → returns
///     cleanly with `Poll::Ready(())`. No third
///     `enter_userspace_with_context` call.
///
/// The smoke exercises the production code path end-to-end:
/// `PerHartSlotted` poll bracket, `run_thread` body, real
/// `linux_syscall::dispatch`, real walker-resolved fd, real
/// `prepare_userspace_entry_payload` (Plan B writeback) — twice
/// per the inverted loop's enter-then-await shape.
#[test]
fn boot_smoke_production_userspace_loop_writes_console_then_exits() {
    use crate::adapter::boot_runtime::userspace::{SyscallRequest, UserspaceTrapInfo};
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    use tx_shims::linux_syscall::{NR_EXIT_GROUP, NR_WRITE};
    use tx_subsystems::process::ExitStatus;

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap()
        .expect("leader payload must be alive before any exit");

    let baseline_bytes_len = CONSOLE_CAPTURED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();

    *LAST_USERSPACE_CTX.lock().unwrap_or_else(|e| e.into_inner()) = None;
    USERSPACE_A0_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();

    // The inverted loop's first iteration calls
    // `prepare_userspace_entry_payload`, which panics if
    // `saved_user_context` is None. In production this is seeded by
    // `exec_script` during `drive_bootstrap_exec`. The smoke skips
    // exec and seeds a zero baseline directly so the first
    // iteration's merged a0 is 0 (asserted below).
    payload.store_saved_user_context(Some(tx_hal::UserTrapContext {
        regs: [0; 32],
        pc: 0,
        status: 0,
        fp: tx_hal::UserFpContext::empty(),
    }));

    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);

    let future = crate::thread_future::run_thread::<TestPlatform>(leader.clone(), payload.clone());
    let wrapped =
        crate::thread_future::PerHartSlotted::<TestPlatform, _>::new(payload.clone(), future);
    let mut boxed = std::boxed::Box::new(wrapped);
    // SAFETY: `boxed` is owned for the duration of this test and
    // never moved after pinning.
    let mut pinned = unsafe { Pin::new_unchecked(&mut *boxed) };

    // ------------------------------------------------------------------
    // Poll #1: the pre-write dive. Future runs the entry-side wait
    // open + AST checkpoint + prepare_* (drains None) + records ctx
    // (a0=0) + returns + awaits entry_wait → Pending.
    // ------------------------------------------------------------------
    match pinned.as_mut().poll(&mut cx) {
        Poll::Pending => {}
        Poll::Ready(()) => {
            panic!("run_thread first poll returned Ready before any trap was injected")
        }
    }
    let pre_write_ctx = LAST_USERSPACE_CTX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("first dive must have recorded the merged UserTrapContext");
    assert_eq!(
        pre_write_ctx.regs[10], 0,
        "first dive's merged a0 must be the seeded baseline (0) — \
         no syscall has run yet, so prepare_* drained pending=None",
    );

    // ------------------------------------------------------------------
    // Driver: resolve the active wait with Syscall(write).
    // ------------------------------------------------------------------
    let write_buf: &[u8] = b"hi\n";
    let write_trap = UserspaceTrapInfo::Syscall(SyscallRequest::new(
        NR_WRITE,
        [1, write_buf.as_ptr() as u64, 3, 0, 0, 0],
    ));
    let active = payload
        .active_userspace_request()
        .expect("future must have published an active wait token before yielding");
    payload
        .userspace_slot()
        .complete_interesting_trap(active, write_trap)
        .expect("complete_interesting_trap on the active request (write)");

    // ------------------------------------------------------------------
    // Poll #2: the post-write dive. Future awaits resolve → matches
    // Syscall arm → dispatches write (Ok(3) into pending) → top of
    // loop → prepare_* drains pending → ctx.a0=3 → records ctx → returns
    // → awaits new entry_wait → Pending.
    // ------------------------------------------------------------------
    match pinned.as_mut().poll(&mut cx) {
        Poll::Pending => {}
        Poll::Ready(()) => panic!("run_thread second poll returned Ready unexpectedly"),
    }
    let post_write_ctx = LAST_USERSPACE_CTX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("post-write dive must have recorded the merged UserTrapContext");
    assert_eq!(
        post_write_ctx.regs[10], 3,
        "post-write merged a0 must equal write's return value (3) — \
         Plan B writeback discipline correctness check",
    );

    let captured = CONSOLE_CAPTURED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let new_bytes = &captured[baseline_bytes_len..];
    assert!(
        new_bytes.windows(b"hi\r\n".len()).any(|w| w == b"hi\r\n"),
        "post-OPOST console bytes must contain b\"hi\\r\\n\"; got {:?}",
        new_bytes,
    );

    // ------------------------------------------------------------------
    // Driver: resolve the active wait with Syscall(exit_group(0)).
    // ------------------------------------------------------------------
    let exit_trap =
        UserspaceTrapInfo::Syscall(SyscallRequest::new(NR_EXIT_GROUP, [0, 0, 0, 0, 0, 0]));
    let active = payload
        .active_userspace_request()
        .expect("future must have published an active wait token before yielding (exit_group)");
    payload
        .userspace_slot()
        .complete_interesting_trap(active, exit_trap)
        .expect("complete_interesting_trap on the active request (exit_group)");

    // ------------------------------------------------------------------
    // Poll #3: future awaits resolve → matches Syscall arm → dispatch
    // exit_group → NoReturn → return → Poll::Ready.
    // ------------------------------------------------------------------
    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(()) => {}
        Poll::Pending => panic!("run_thread third poll returned Pending; expected Ready"),
    }

    assert!(init.is_zombie(), "exit_group must zombify init");
    assert_eq!(
        init.exit_status(),
        Some(ExitStatus::Exited(0)),
        "exit_group(0) records ExitStatus::Exited(0)",
    );
    assert_eq!(
        init.live_thread_count(),
        0,
        "init's thread group must drain to zero on exit_group",
    );

    let final_a0_log = USERSPACE_A0_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        final_a0_log,
        std::vec![0usize, 3usize],
        "exactly two enter_userspace_with_context calls — pre-write \
         baseline (a0=0) and post-write merged (a0=3); exit_group's \
         NoReturn short-circuits before a third dive",
    );

    drop(boxed);
    let _ = tx_subsystems::thread_runtime::clear_current_thread_payload(0);
}

// ---------------------------------------------------------------------------
// Local async-future driver. Mirrors `tx-shims/src/linux_syscall/tests.rs`'s
// `block_on` because the walker / dispatcher are `async` (the walker
// uses `.await` for `step_walk` recursion). Phase 7's production-path
// smoke uses its own catch_unwind/re-poll loop directly rather than
// going through `block_on`, but `NoopWake` is shared.
// ---------------------------------------------------------------------------

fn block_on<F: core::future::Future>(mut fut: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on the stack for the duration of the loop;
    // we never move it after `Pin::new_unchecked`.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve in 1024 polls");
}

/// **End-to-end bootstrap-exec smoke (production-paths-up-to-divergence
/// per the brief's degrade option).** Drives the boot wiring, then
/// `run_bootstrap_exec_for_init` (which registers the hand-encoded
/// RV64 ELF fixture into tmpfs and drives `exec_script` via
/// `block_on`). Asserts the bootstrap front-end seeds the thread's
/// `saved_user_context` with the fixture's entry-point and atomically
/// replaces the AddressSpace.
///
/// The full reactor-loop drive (write → exit_group → zombie) is
/// covered by `boot_smoke_production_userspace_loop_writes_console_then_exits`
/// above; this smoke is the missing exec-front-end link between
/// boot wiring and that loop. Together they pin the full pipeline.
///
/// Cites: `txdoc:EXEC-19-BOOTSTRAP`,
/// `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
#[test]
fn boot_smoke_bootstrap_exec_seeds_init_user_context_from_fixture() {
    use crate::init::init_fixture::{INIT_FIXTURE_ENTRY_VADDR, INIT_FIXTURE_FILE_SIZE};

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap()
        .expect("leader payload must be alive pre-exec");

    // Pre-exec invariants: aspace is the bootstrap aspace; saved
    // user context is None (no userspace has been entered yet).
    let aspace_before = init
        .aspace_cap()
        .expect("init aspace populated by bootstrap");
    assert!(
        payload.saved_user_context().is_none(),
        "saved_user_context starts None pre-exec"
    );

    // Drive the bootstrap exec: registers
    // /init = INIT_FIXTURE_BYTES into tmpfs, then drives
    // exec_script via block_on. Failure panics with
    // `:bootstrap-exec:fail` per Open Q #3 DECIDED.
    CoreInit::<TestPlatform>::run_bootstrap_exec_for_init();

    // EXEC-PONR boundary crossed: the AddressSpace has been atomically
    // replaced (different Cap key) and saved_user_context is now
    // seeded with the fixture's entry-point.
    let aspace_after = init.aspace_cap().expect("init aspace populated post-exec");
    assert_ne!(
        aspace_before.key(),
        aspace_after.key(),
        "Phase 6 atomic replace_aspace produced a fresh Cap"
    );

    let saved = payload
        .saved_user_context()
        .expect("saved_user_context seeded by Phase 6");
    assert_eq!(
        saved.pc as u64, INIT_FIXTURE_ENTRY_VADDR,
        "saved pc matches fixture's hand-encoded e_entry"
    );
    // sp lives at regs[2] per RV64 SysV ABI; should be the
    // 16-byte-aligned initial_sp from build_initial_user_stack
    // (somewhere in the [USER_STACK_TOP_DEFAULT - 16 KiB,
    // USER_STACK_TOP_DEFAULT) range).
    use tx_subsystems::vm::scripts::{USER_STACK_INITIAL_RESERVATION, USER_STACK_TOP_DEFAULT};
    let sp = saved.regs[2] as u64;
    assert!(
        sp <= USER_STACK_TOP_DEFAULT
            && sp > USER_STACK_TOP_DEFAULT - USER_STACK_INITIAL_RESERVATION,
        "saved sp {sp:#x} lands inside the initial 16 KiB stack reservation \
         (USER_STACK_TOP_DEFAULT={USER_STACK_TOP_DEFAULT:#x})"
    );
    assert_eq!(sp & 0xF, 0, "saved sp is 16-byte aligned per SysV ABI");

    // Init must not be zombie — the reactor loop hasn't run; only
    // the exec front-end has executed.
    assert!(
        !init.is_zombie(),
        "init is still alive post-bootstrap-exec; reactor hasn't run"
    );

    // Sanity: the fixture file is reachable via the walker
    // (the production tmpfs.materialise_rnode override Phase 7 added
    // is what makes this resolve to a PageBacked rnode).
    let fixture_size = INIT_FIXTURE_FILE_SIZE;
    assert!(
        fixture_size > 0,
        "fixture size constant is non-zero ({fixture_size} bytes)"
    );
}

// ---------------------------------------------------------------------------
// Wave 1 fork/clone/wait4 slice — reactor-submission seam smoke
// ---------------------------------------------------------------------------
//
// The seam lives in `tx_subsystems::reactor_submit`: a function-pointer
// slot that `tx-kernel`'s init populates with a closure binding the
// boot reactor and the platform parameter `P`. Wave 2's `sys_clone`
// arm in `tx-shims` reads the slot to submit a freshly-cloned child
// thread to the reactor without taking a circular dependency back
// into `tx-kernel`.
//
// This smoke verifies the install seam end-to-end:
//   - Before any boot wiring, `submit_child_thread_fn()` reports
//     `None` (slot empty, fresh from `reset_reactor_submit_seam`).
//   - After `install_reactor_submit_seam`, the slot resolves to a
//     non-null fn pointer that is `submit_child_thread_into_boot_reactor`.
//   - Calling the routed fn with a dummy live ProcessIdentity +
//     ThreadIdentity is a clean no-op (the boot reactor is not
//     initialised in the test scaffolding so `BOOT_REACTOR.with`
//     short-circuits via its `init` check).

#[test]
fn reactor_submission_seam_submits_child_thread_smoke() {
    use tx_subsystems::reactor_submit;

    let _serial = setup();
    bootstrap_init();

    // Pre-condition: the seam slot is empty (setup() resets it).
    assert!(
        reactor_submit::submit_child_thread_fn().is_none(),
        "fresh setup must leave the reactor-submit seam slot empty",
    );

    // Install. The hook captures `TestPlatform` as the platform
    // parameter `P` so the resulting fn pointer is parameter-free.
    crate::init::CoreInit::<TestPlatform>::install_reactor_submit_seam();

    // Post-condition: the slot now resolves.
    let installed = reactor_submit::submit_child_thread_fn()
        .expect("install_reactor_submit_seam populates the slot");

    // Fn-pointer identity must equal the platform-typed
    // `submit_child_thread_into_boot_reactor` we asked for.
    let expected = crate::init::CoreInit::<TestPlatform>::submit_child_thread_into_boot_reactor
        as reactor_submit::SubmitChildThreadFn;
    assert_eq!(
        installed as usize, expected as usize,
        "installed fn pointer must equal the platform-typed routing helper",
    );

    // Drive the routed call once with a freshly forked init child.
    // The boot reactor is not initialised in this scaffolding, so
    // `BOOT_REACTOR.with` returns `None` inside the routed body —
    // the call is a clean no-op (no panic, no submission).
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated by bootstrap_init");
    let child = tx_subsystems::process::step_fork::<TestPlatform>(&init).expect("fork");
    let leader = child
        .nth_thread(0)
        .expect("fresh child has a leader thread");
    reactor_submit::submit_child_thread(child.clone(), leader);
    // No assertion on reactor side-effects — the BOOT_REACTOR slot
    // is not initialised in the test scaffolding (init_boot_reactor
    // is gated behind init_substrate_if_ready). The smoke covers
    // the fn-pointer routing; production reactor-side coupling is
    // exercised once the BSP reactor loop runs at boot.
}

// ---------------------------------------------------------------------------
// Wave 4 fork/clone/wait4 slice — Part 7 end-to-end smoke (Layer A).
// ---------------------------------------------------------------------------
//
// **Layer choice: A (production-paths-up-to-divergence).** The brief
// recommended Layer A unless Layer B (full reactor-driven fork+wait
// round trip with an instruction-stream simulator) was a clean
// extension of ELF loader Wave 5's smoke. Wave 5 only scripts a
// single linear program (write+exit_group) without branching or
// inter-thread coordination; Layer B for fork+wait would require:
//   - decoding the actual `bnez` from the saved-context PC + reading
//     fixture bytes via the post-exec AddressSpace (cross-task
//     iteration over both parent and child threads),
//   - extending TestPlatform/PerHartSlotted to switch which
//     ThreadPayload's userspace_slot the simulator drives between
//     iterations,
//   - simulating the kernel-side `post_sigchld_to_parent` resolution
//     that wakes the parent's NR_WAIT4 wait,
//     and
//   - capturing/coordinating the `pending_syscall_return` snapshots
//     across both processes.
// That is well past 200 lines of simulator-decoder-of-actual-instructions
// without TestPlatform extensions. Per the brief: "fall back to Layer A
// and document Layer B as a follow-up."
//
// **What Layer A pins.** Together with Wave 3's tx-shims arm tests
// (which independently prove NR_CLONE/NR_WAIT4/NR_WRITE/NR_EXIT_GROUP
// dispatch on isolated payloads) and the ELF loader's Wave 5 smoke
// (which proves the bootstrap-exec → reactor-driven write+exit
// pipeline against the original hello-world fixture), Layer A's
// assertion that the new fixture's bootstrap-exec seeds the right
// entry-point with `li a7, 220` (NR_CLONE) at PC closes the wiring
// loop end-to-end without an integration smoke.
//
// **Layer B follow-up.** Tracked as a deferral note; the recommended
// shape is a follow-up smoke that uses the panic-as-yield TestPlatform
// pattern with an instruction-stream simulator that decodes the
// fixture's RV64 byte stream as it walks PC, runs syscall args
// through `linux_syscall::dispatch`, and coordinates the parent's
// blocking wait against the child's exit-port post.

/// Wave 4 Part 7 — Layer A. Drives boot wiring + bootstrap exec
/// against the new fork+wait+exit fixture; asserts that
/// `saved_user_context.pc` lands on the fixture entry-point AND
/// that the fixture's first instruction at that entry decodes to
/// `li a7, 220` (NR_CLONE).
///
/// This is the missing exec-front-end → fixture-content
/// alignment pin: combined with the Wave 1+2+3 tx-shims tests
/// (which exercise dispatch arms in isolation) and the Wave 5
/// hello-world smoke (which exercises the reactor-driven loop on
/// a simpler program), the fork+wait pipeline is end-to-end
/// pinned without a heavyweight integration simulator.
///
/// Cites: `txdoc:EXEC-19-BOOTSTRAP`,
/// `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
#[test]
fn boot_smoke_fork_wait_seeds_init_for_clone_at_entry() {
    use crate::init::init_fixture::{INIT_FIXTURE_BYTES, INIT_FIXTURE_ENTRY_VADDR};

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap()
        .expect("leader payload must be alive pre-exec");

    // Pre-exec: snapshot the bootstrap aspace for the
    // atomic-replace assertion below.
    let aspace_before = init
        .aspace_cap()
        .expect("init aspace populated by bootstrap");
    assert!(
        payload.saved_user_context().is_none(),
        "saved_user_context starts None pre-exec"
    );

    // Drive the bootstrap exec: registers /init = INIT_FIXTURE_BYTES
    // into tmpfs, then drives exec_script via block_on. Failure
    // panics with `:bootstrap-exec:fail` per Open Q #3 DECIDED.
    CoreInit::<TestPlatform>::run_bootstrap_exec_for_init();

    // EXEC-PONR boundary crossed: aspace atomically replaced.
    let aspace_after = init.aspace_cap().expect("init aspace populated post-exec");
    assert_ne!(
        aspace_before.key(),
        aspace_after.key(),
        "Phase 6 atomic replace_aspace produced a fresh Cap"
    );

    let saved = payload
        .saved_user_context()
        .expect("saved_user_context seeded by Phase 6");
    assert_eq!(
        saved.pc as u64, INIT_FIXTURE_ENTRY_VADDR,
        "saved pc matches fixture's hand-encoded e_entry — the \
         bootstrap front-end planted the new fork+wait entry-point",
    );

    // Wave 4's distinguishing assertion (vs the hello-world
    // smoke): the fixture's first instruction at the seeded PC
    // must be `li a7, 220` (NR_CLONE) — not the hello-world's
    // `li a7, 64` (NR_WRITE). The fixture's byte stream is the
    // source of truth; the seeded PC must point at byte offset 176
    // (the `_start` label).
    let first_insn = u32::from_le_bytes(INIT_FIXTURE_BYTES[176..180].try_into().unwrap());
    assert_eq!(
        first_insn, 0x0dc00893,
        "fixture's first instruction at INIT_FIXTURE_ENTRY_VADDR \
         must be `li a7, 220` (NR_CLONE) — Wave 4 fork+wait+exit \
         shape, not the hello-world predecessor",
    );

    // Init must not be zombie post-bootstrap-exec — the reactor
    // loop hasn't run; only the exec front-end has executed.
    assert!(
        !init.is_zombie(),
        "init is still alive post-bootstrap-exec; reactor hasn't run"
    );
}

// ---------------------------------------------------------------------------
// DAC + setuid Wave 5 — Part 8 end-to-end smoke (Layer A).
// ---------------------------------------------------------------------------
//
// **Layer choice: A (production-paths-up-to-divergence).** Mirrors the
// fork/clone/wait4 Wave 4 smoke's choice: drive the production exec
// front-end with the new sibling fixture, assert on cred state and
// `saved_user_context.pc` post-exec; defer the reactor-driven
// instruction-level execution to a future integration smoke. Per the
// Wave 5 brief: "Wave 5 doesn't need to drive the reactor loop; the
// assertions are on cred state + saved_user_context post-exec."
//
// **What this smoke pins.** Together with the Wave 4 Part 5 unit
// tests (which prove `step_apply_suid_for_exec` in isolation against
// synthesized cred + file-meta inputs) and the Wave 3 walker DAC
// tests (which prove the EACCES branch on a non-executable binary),
// this smoke closes the end-to-end pipeline:
//
//   - Production `exec_script::<P>` resolves the path through the
//     real walker (against a real tmpfs mount populated via the
//     real `create_inode` / `materialise_rnode` / `step_chmod` /
//     `step_chown` surfaces).
//   - Phase 1 execute-perm check passes (the file's mode bits and
//     the caller's effective uid/gid line up).
//   - Phase 3.5 cred recompute fires: the binary's `S_ISUID` bit
//     plus file owner uid=1000 changes `cred.euid` from 1001 to
//     1000 and copies through to `cred.suid`. Real uid stays 1001
//     (Linux preserves it).
//   - Phase 6 atomic AddressSpace replace + saved_user_context
//     seeding still happens with the new cred installed.
//
// **Sibling fixture (`init_setuid_fixture.rs`).** The Plan's Q4 was
// authored before the fork/clone/wait4 slice rewrote
// `init_fixture.rs` into a fork+wait+exit binary. Extending it
// further into a third behaviour (drop-privs → execve → observe
// euid) would invalidate the existing fork+wait pin tests. The
// sibling-fixture path keeps both smokes independently pinned;
// see `init_setuid_fixture.rs`'s module header for the deviation.

/// Wave 5 Part 8 — Layer A. Drives boot wiring + bootstrap exec
/// against a sibling setuid fixture; asserts that Phase 3.5's cred
/// recompute installs the file owner's uid as the new effective
/// uid AND that `saved_user_context.pc` lands on the fixture's
/// entry-point.
///
/// Setup steps (all against production surfaces):
///   1. `drive_boot_wiring` — bootstraps init with root cred + full
///      caps (per `bootstrap_init_process`).
///   2. `register_setuid_fixture_into_tmpfs(uid=1000, gid=1000)` —
///      creates `/setuid-target` in the rootfs tmpfs, copies the
///      sibling fixture bytes, then `step_chown`s the file owner
///      to 1000:1000 and `step_chmod`s the mode to
///      `S_ISUID | 0o755`. Both mutations run as root (CAP_FOWNER)
///      so non-privileged-clears don't fire.
///   3. `clear_caps_for_test` + `set_cred_ids_for_test(1001, ...)`
///      — drops init's cred to a non-privileged uid 1001 so the
///      slice's DAC enforcement actually fires on the
///      `step_apply_suid_for_exec` recompute.
///   4. `block_on(exec_script(/setuid-target, ...))` with a
///      walker-side `Credential { uid: 1001, gid: 1001, no caps }`.
///
/// Post-exec assertions (the load-bearing setuid checks):
///   - `init.cred().uid == 1001` (real uid unchanged at exec).
///   - `init.cred().euid == 1000` (S_ISUID bit set effective uid
///     to file owner — Phase 3.5 recompute correctness check).
///   - `init.cred().suid == 1000` (saved-set tracks new euid per
///     Linux semantics).
///   - `init.cred().gid == 1001` (no S_ISGID bit, so gid family
///     unchanged).
///   - `saved_user_context.pc == INIT_SETUID_FIXTURE_ENTRY_VADDR`
///     (Phase 6 still seeded the entry-point with the new cred).
///   - The detached AddressSpace was atomically replaced (PoNR
///     boundary crossed).
///
/// Cites: `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`,
/// `txdoc:EXEC-12-3-INSTALL-NEW-CREDENTIAL`,
/// `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
#[test]
fn boot_smoke_setuid_exec_seeds_post_setuid_euid_and_at_secure() {
    use crate::init::init_setuid_fixture::{
        INIT_SETUID_FIXTURE_ENTRY_VADDR, INIT_SETUID_FIXTURE_LOAD_VADDR,
    };
    use tx_subsystems::cred::CapabilitySet;
    use tx_subsystems::vfs::Credential;

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap()
        .expect("leader payload must be alive pre-exec");

    // Step 2: register the setuid fixture as /setuid-target with
    // owner uid=1000, gid=1000, mode = S_ISUID | 0o755.
    register_setuid_fixture_into_tmpfs(1000, 1000);

    // Step 3: drop init's cred to non-root + clear caps. Done in
    // this order so that bootstrap-aspace-creation and the file
    // registration both run as root (CAP_FOWNER for the chmod /
    // chown), and only the subsequent exec runs as 1001.
    tx_subsystems::cross_crate_test_support::clear_caps_for_test(&init);
    tx_subsystems::cross_crate_test_support::set_cred_ids_for_test(
        &init, 1001, 1001, 1001, 1001, 1001, 1001,
    );

    // Pre-exec sanity: the cred-drop produced what we expect.
    let pre = init.cred().expect("init cred pre-exec");
    assert_eq!(pre.uid.raw(), 1001);
    assert_eq!(pre.euid.raw(), 1001);
    assert_eq!(pre.suid.raw(), 1001);
    assert_eq!(
        pre.effective_caps,
        CapabilitySet::EMPTY,
        "clear_caps_for_test must zero effective_caps so the slice's \
         DAC checks aren't short-circuited by CAP_DAC_OVERRIDE",
    );

    let aspace_before = init
        .aspace_cap()
        .expect("init aspace populated by bootstrap");
    assert!(
        payload.saved_user_context().is_none(),
        "saved_user_context starts None pre-exec"
    );

    // Step 4: drive exec_script with the post-drop walker cred.
    // The cred's effective_caps is empty so CAP_DAC_OVERRIDE
    // doesn't bypass the execute-bit check; the file's mode is
    // S_ISUID | 0o755 so the world-X bit allows uid 1001 to
    // execute.
    let walker_cred = Credential {
        uid: 1001,
        gid: 1001,
        effective_caps: CapabilitySet::EMPTY,
    };
    let argv: &[&[u8]] = &[b"setuid-target" as &[u8]];
    let envp: &[&[u8]] = &[];

    let result = block_on(tx_scripts::process::exec::exec_script::<TestPlatform>(
        &init,
        &leader,
        b"/setuid-target",
        argv,
        envp,
        &walker_cred,
    ));
    assert!(
        result.is_ok(),
        "exec_script(/setuid-target) must succeed for a 0o4755 \
         binary executed by uid 1001 — got {:?}",
        result,
    );

    // ----- Post-exec invariants ---------------------------------

    // EXEC-PONR boundary crossed: aspace atomically replaced.
    let aspace_after = init.aspace_cap().expect("init aspace populated post-exec");
    assert_ne!(
        aspace_before.key(),
        aspace_after.key(),
        "Phase 6 atomic replace_aspace produced a fresh Cap"
    );

    // The load-bearing setuid assertions: Phase 3.5's cred
    // recompute installed the file owner's uid as the new
    // effective uid + saved-set, while leaving the real uid
    // untouched.
    let post = init.cred().expect("init still alive post-exec");
    assert_eq!(
        post.uid.raw(),
        1001,
        "real uid unchanged at exec (Linux preserves it across \
         setuid binary execution)",
    );
    assert_eq!(
        post.euid.raw(),
        1000,
        "S_ISUID bit set effective uid to file owner (1000)",
    );
    assert_eq!(
        post.suid.raw(),
        1000,
        "saved-set uid copied from new effective uid per Linux \
         setuid-on-exec semantics",
    );
    // gid family stays untouched: the fixture's mode is
    // `S_ISUID | 0o755` (no S_ISGID), so no setgid recompute.
    assert_eq!(
        post.gid.raw(),
        1001,
        "real gid unchanged (no S_ISGID on fixture)",
    );
    assert_eq!(
        post.egid.raw(),
        1001,
        "effective gid unchanged (no S_ISGID on fixture)",
    );
    assert_eq!(
        post.sgid.raw(),
        1001,
        "saved-set gid unchanged (no S_ISGID on fixture)",
    );

    // saved_user_context.pc landed on the fixture's hand-encoded
    // entry-point. Combined with the cred assertions above, this
    // pins that the new cred is installed *before* Phase 6
    // re-seeds the user context (Phase 3.5 ordering).
    let saved = payload
        .saved_user_context()
        .expect("saved_user_context seeded by Phase 6");
    assert_eq!(
        saved.pc as u64, INIT_SETUID_FIXTURE_ENTRY_VADDR,
        "saved pc matches the sibling fixture's hand-encoded e_entry",
    );

    // Sanity: the load vaddr constant matches the fixture's
    // PT_LOAD `p_vaddr` (defends against a future drift in the
    // fixture's hand-encoded headers).
    assert_eq!(INIT_SETUID_FIXTURE_LOAD_VADDR, 0x10000);

    // Init must not be zombie post-bootstrap-exec — the reactor
    // loop hasn't run; only the exec front-end has executed.
    assert!(
        !init.is_zombie(),
        "init is still alive post-setuid-exec; reactor hasn't run"
    );
}

/// Test helper: register the setuid fixture as `/setuid-target` in
/// the rootfs tmpfs with mode `S_ISUID | 0o755`, owner `(uid, gid)`.
///
/// The rootfs tmpfs's `create_inode` records the caller's uid/gid
/// (always root in the bootstrap path) — there is no `mode +
/// uid + gid` overload on the production `create_inode` surface, so
/// the helper writes the bytes with default owner first, then
/// `step_chown`s + `step_chmod`s as root (which carries CAP_FOWNER
/// after `drive_boot_wiring`). This avoids the silent-clear-S_ISUID
/// rule that fires on non-privileged chowns.
///
/// Mirrors the production `register_init_fixture_into_tmpfs`
/// shape; the only differences are (a) the path (`/setuid-target`
/// vs `/init`) and (b) the post-creation chown + chmod to install
/// the setuid mode + non-root owner.
fn register_setuid_fixture_into_tmpfs(file_uid: u32, file_gid: u32) {
    use tx_subsystems::vfs::{Credential, RNodeBacking, S_ISUID};

    let root_mount =
        crate::init::root_mount().expect("register_setuid_fixture: ROOT_MOUNT must be populated");
    let fs_ops = root_mount
        .payload_cap()
        .expect("rootfs payload alive in test")
        .into_cap()
        .fs_ops
        .clone();
    let fs_page_backing = root_mount
        .payload_cap()
        .expect("rootfs payload alive in test")
        .into_cap()
        .fs_page_backing
        .clone();
    let root_object_id = root_mount.root().fs_object_id();

    let bytes = &crate::init::init_setuid_fixture::INIT_SETUID_FIXTURE_BYTES[..];

    // Allocate the inode as root. `0o100755` = S_IFREG | 0755.
    // (We chmod to S_ISUID below; doing it here would still be
    // valid, but splitting create + chmod exercises the slice's
    // step_chmod path under privileged cred.)
    let cred = Credential::root();
    let (file_id, file_meta) = {
        let guard = guard();
        let outcome =
            fs_ops.create_inode(root_object_id, b"setuid-target", 0o100755, &cred, &guard);
        match outcome {
            StepOutcome::Done(out) => out,
            other => panic!("register_setuid_fixture: create_inode(/setuid-target): {other:?}"),
        }
    };

    // Materialise the inode's RNode so we can populate page
    // contents — same shape as register_init_fixture_into_tmpfs.
    let pc = {
        let guard = guard();
        let outcome = fs_ops.materialise_rnode(file_id, file_meta, &guard);
        let rnode = match outcome {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("register_setuid_fixture: materialise_rnode: {other:?}"),
        };
        match rnode.backing() {
            RNodeBacking::PageBacked { pc } => pc.clone(),
            other => panic!(
                "register_setuid_fixture: tmpfs materialise_rnode \
                 returned non-PageBacked backing: {other:?}"
            ),
        }
    };

    // Copy the fixture bytes into the file's pages — direct-map
    // memcpy, same shape as register_init_fixture_into_tmpfs.
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE;
    for (idx, chunk) in bytes.chunks(page_size).enumerate() {
        let materialised = pc
            .materialize_anon(
                tx_subsystems::page_backed::PageIndex::new(idx as u64),
                tx_subsystems::page_backed::MaterializeAccess::Write,
            )
            .expect("register_setuid_fixture: materialize_anon");
        let frame_base = page_allocator::frame_kernel_addr(materialised.ppn)
            .expect("register_setuid_fixture: direct-map view");
        // SAFETY: the materialised frame is held resident for the
        // duration of this scope; the destination region covers
        // exactly `chunk.len()` bytes; source/dest don't overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
        }
    }

    // Set the visible byte-size so `read_exact_at` knows the file
    // length during exec.
    let size = bytes.len() as u64;
    {
        let guard = guard();
        match fs_page_backing.truncate(file_id, size, &guard) {
            StepOutcome::Done(()) => {}
            other => panic!("register_setuid_fixture: truncate({size}): {other:?}"),
        }
    }

    // Step_chown to set the file's owner uid/gid to the requested
    // values. Cred is root (CAP_FOWNER), so the chown is privileged
    // and the silent-clear-S_ISUID rule does NOT fire.
    {
        let guard = guard();
        match fs_ops.step_chown(file_id, Some(file_uid), Some(file_gid), &cred, &guard) {
            StepOutcome::Done(()) => {}
            other => {
                panic!("register_setuid_fixture: step_chown({file_uid}, {file_gid}): {other:?}")
            }
        }
    }

    // Step_chmod to install S_ISUID. Cred is root (CAP_FOWNER) so
    // the owner-or-CAP_FOWNER permission gate passes. The new mode
    // bits are masked to `& 0o7777` by the tmpfs backend; the
    // caller's `0o4755` is preserved (S_ISUID = 0o4000, plus 0o755
    // for owner-rwx + group/world rx).
    let new_mode = S_ISUID | 0o755;
    {
        let guard = guard();
        match fs_ops.step_chmod(file_id, new_mode, &cred, &guard) {
            StepOutcome::Done(()) => {}
            other => panic!("register_setuid_fixture: step_chmod({new_mode:#o}): {other:?}"),
        }
    }
}
