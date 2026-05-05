//! Phase 3b boot-wiring tests.
//!
//! Exercises the new ordered steps `register_console_hardware` →
//! `mount_rootfs_tmpfs` → `mount_devfs_at_dev` →
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

/// Serialise every test in this module: they all touch the global
/// `INIT_PROCESS` / mount / TTY slots plus the per-CPU epoch domain
/// (which forbids guard nesting on the same CPU). One lock keeps the
/// test set deterministic; same shape `tx-fs` uses.
static INIT_TEST_LOCK: Mutex<()> = Mutex::new(());

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

impl tx_hal::TrapIf for TestPlatform {}
impl tx_hal::UserAccessIf for TestPlatform {}
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

impl tx_hal::PowerIf for TestPlatform {
    fn system_off() -> ! {
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
    tx_substrate::testing::init_host_for_test_once();
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
    crate::init::reset_boot_state_for_test();
    CONSOLE_CAPTURED_LEN.store(0, Ordering::Release);
    CONSOLE_CAPTURED_BYTES
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
    CoreInit::<TestPlatform>::mount_rootfs_tmpfs();
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
    assert_eq!(root.payload().fstype, "tmpfs");
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
    assert_eq!(dev.payload().fstype, "devfs");
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
    use tx_subsystems::execution::StepOutcome;
    use tx_subsystems::vfs::{walker, Credential, RNodeBacking, StructPayload};

    let _serial = setup();
    drive_boot_wiring();

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let cwd = init.cwd().expect("init cwd must be bound");
    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(walker::step_walk(cwd, b"/dev/console", &cred, &guard));
    drop(guard);

    let dentry = match outcome {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
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
    let guard = tx_substrate::epoch::guard();
    match stdout.step_write(b"hi\n", &guard) {
        tx_subsystems::execution::StepOutcome::Done(written) => {
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
// Phase 6 — end-to-end userspace round-trip smoke
// ---------------------------------------------------------------------------
//
// The Trio plan §"Phasing" item 6 (lines 505-509) calls for a single
// `cargo test -p tx-kernel` test that simulates a fake userspace
// returning two traps in sequence — `Syscall(write(1, "hi\n", 3))` and
// `Syscall(exit_group(0))` — and asserts that the bytes reach the
// platform console (post-OPOST) and the init process zombifies with
// `ExitStatus::Exited(0)`.
//
// Because no real RV64 trap shell or `enter_userspace` shim exists in
// host-test mode, this test synthesises the per-iteration loop the
// reactor task wrapper *would* drive:
//
//   1. Install `init.leader.payload` in the per-hart slot via
//      `set_current_thread_payload(0, _)`. This mirrors the call the
//      future reactor task wrapper makes before each `Future::poll` of
//      a thread future (`txdoc:THREAD-5-1-STATE-PLACEMENT`).
//   2. Pop the next `UserspaceTrapInfo` from a Vec representing the
//      program's syscall sequence (the "fake userspace closure").
//   3. `slot.start_request()` opens the userspace-run wait;
//      `slot.complete_interesting_trap(req, info)` resolves it. We
//      do not actually `.await` the wait future inside this loop —
//      after `complete_interesting_trap` succeeds we know which
//      `SyscallRequest` was produced, and we feed it directly into
//      `linux_syscall::dispatch`.
//   4. The dispatcher's return is encoded into
//      `pending_syscall_return` (Plan B writeback discipline,
//      `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`).
//   5. The **fake entry shim** drains `pending_syscall_return` into a
//      test-local log. It does NOT touch a real `TrapFrameMut` — host
//      tests have no platform trap frame. This is the explicit
//      Phase-6 synthesis called out in the plan's "Cross-cutting risks
//      #1": the userspace-entry shim that would write `set_syscall_return`
//      into a fresh trap frame does not yet exist in production code,
//      so the test stages a host-side facsimile.
//   6. The loop terminates when (a) the queue is empty or (b) the
//      dispatcher returned `SyscallResult::NoReturn` (exit_group's
//      "thread does not return to userspace" outcome).
//
// The test is gated by `INIT_TEST_LOCK` (shared with the Phase 3b
// boot-wiring tests) because it bootstraps and tears down the same
// global `INIT_PROCESS` / mount / TTY slots.

/// Phase 6 deliverable. The full chain from a synthesised userspace
/// trap through to console capture and zombie exit-status.
#[test]
fn boot_smoke_userspace_round_trip_writes_console_then_exits() {
    use tx_reactor::userspace::{SyscallRequest, UserspaceTrapInfo};
    use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult, NR_EXIT_GROUP, NR_WRITE};
    use tx_subsystems::process::ExitStatus;
    use tx_subsystems::thread_runtime::{
        clear_current_thread_payload, drain_pending_syscall_return, set_current_thread_payload,
    };

    let _serial = setup();
    drive_boot_wiring();

    // Resolve init + leader thread + leader payload. After
    // `drive_boot_wiring` the leader's payload is alive (no exit has
    // run); `payload_cap_for_test` is a `cfg(any(test, feature =
    // "test-support"))` accessor on `ThreadIdentity` introduced as the
    // minimal additive seam this Phase 6 test needs.
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap_for_test()
        .expect("leader payload must be alive before any exit");

    // Stage the per-hart slot the way a reactor task wrapper would.
    // The Trio plan's "Cross-cutting risks #1" notes that the
    // production reactor wrapper does not yet exist; this test stages
    // it manually. `set_current_thread_payload` returns the previous
    // slot; we expect None on a fresh test.
    assert!(
        set_current_thread_payload(0, payload.clone()).is_none(),
        "per-hart slot must be empty before installing the leader payload"
    );

    // Snapshot console bytes captured before the loop so we can assert
    // the new bytes belong to this test's syscalls and not a leftover.
    let baseline_bytes_len = CONSOLE_CAPTURED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();

    // Dispatch context. `aspace_cap` is `Some` for the live init.
    let aspace = init
        .aspace_cap()
        .expect("init aspace must be alive pre-exit");
    let ctx = SyscallCtx::new(init.clone(), leader.clone(), aspace);

    // Synthesise the userspace trap queue. The `write` syscall takes a
    // kernel-side buffer per Phase 2a's bootstrap exemption (TODO
    // copy_from_user lane); `b"hi\n".as_ptr() as u64` is the kernel VA
    // of the static byte slice.
    let write_buf: &[u8] = b"hi\n";
    let queue: std::vec::Vec<UserspaceTrapInfo> = std::vec![
        UserspaceTrapInfo::Syscall(SyscallRequest::new(
            NR_WRITE,
            [1, write_buf.as_ptr() as u64, 3, 0, 0, 0],
        )),
        UserspaceTrapInfo::Syscall(SyscallRequest::new(NR_EXIT_GROUP, [0, 0, 0, 0, 0, 0])),
    ];

    // Per-iteration log of what the fake entry shim drained from
    // `pending_syscall_return`. Phase 6's assertion: index 0 is
    // `Some(Ok(3))` (write returned 3 bytes); index 1 is `None` (the
    // exit_group dispatch returned `NoReturn`, never wrote a value).
    let mut drained_log: std::vec::Vec<Option<Result<i64, i32>>> = std::vec::Vec::new();

    for trap in queue {
        // (1) Open the userspace-run wait, mirroring what the thread
        // future would do at re-entry.
        let wait = payload
            .userspace_slot()
            .start_request()
            .expect("start userspace-run wait");
        let req_token = wait.request();
        payload.set_active_userspace_request(Some(req_token));

        // (2) Resolve the wait with the synthesised trap. This is
        // exactly what `trap_handoff::hand_off_syscall` would do on a
        // real syscall trap. The wait future itself is dropped on the
        // next `start_request`; we pull the inner `SyscallRequest`
        // out of `trap` directly because the host loop has no
        // executor running the wait.
        let _ = payload
            .userspace_slot()
            .complete_interesting_trap(req_token, trap)
            .expect("complete_interesting_trap on freshly-started request");
        // The thread future would now consume the resolution and
        // clear `active_request`; mirror that here so a re-entry
        // doesn't panic with `Busy`.
        payload.set_active_userspace_request(None);
        // Drop the wait future explicitly; its `Drop` impl clears
        // any leftover `active` state.
        drop(wait);

        // (3) Drive the dispatcher with the request that was inside
        // the trap. `block_on` is local to this module — see helper
        // below.
        let req = match trap {
            UserspaceTrapInfo::Syscall(r) => r,
            other => panic!("Phase 6 queue must only carry syscall traps; saw {other:?}",),
        };
        let result = block_on(dispatch(req, &ctx));

        // (4) Plan B writeback: store the dispatcher's outcome into
        // `pending_syscall_return`. `NoReturn` does not write the
        // slot (the thread never re-enters userspace).
        match result {
            SyscallResult::Return(v) => payload.store_pending_syscall_return(Some(Ok(v))),
            SyscallResult::Error(e) => payload.store_pending_syscall_return(Some(Err(e))),
            SyscallResult::NoReturn => {
                // No writeback. Loop will terminate on the
                // post-iteration `NoReturn` check.
            }
        }

        // (5) Fake userspace-entry shim. In a real RV64 boot, this is
        // where the platform `enter_userspace` path would
        // `set_syscall_return` / `set_syscall_error` on a *fresh*
        // trap frame before `sret`. Host tests have no trap frame, so
        // we drain the slot and stash it in `drained_log`; the
        // post-loop assertions verify the values match Plan B
        // expectations.
        let drained = drain_pending_syscall_return(&payload);
        drained_log.push(drained);

        // (6) Terminate the loop on `NoReturn` — the dispatcher will
        // never produce a value to drain, and the next iteration
        // would observe a zombie process (no payload, no fds).
        if matches!(result, SyscallResult::NoReturn) {
            break;
        }
    }

    // Tidy the per-hart slot. A real reactor wrapper would call this
    // after each poll completes (paired with the
    // `set_current_thread_payload` at poll start, per
    // `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`).
    let cleared = clear_current_thread_payload(0);
    assert!(
        cleared.is_some(),
        "per-hart slot must still hold the leader payload at end-of-loop \
         (cleared exactly once)"
    );

    // ---- Assertions ----
    //
    // (a) Console capture: TTY's N_TTY ldisc applies OPOST/ONLCR
    //     before the bytes hit the platform transport, so `b"hi\n"`
    //     becomes `b"hi\r\n"` at the device boundary. We compare the
    //     **trailing** capture (post-baseline) so any pre-loop console
    //     output from `drive_boot_wiring` (the boot sentinel writes
    //     etc.) doesn't pollute the assertion.
    let captured = CONSOLE_CAPTURED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        captured.len() >= baseline_bytes_len,
        "console byte buffer must not shrink during the loop"
    );
    let new_bytes = &captured[baseline_bytes_len..];
    assert!(
        new_bytes.windows(b"hi\r\n".len()).any(|w| w == b"hi\r\n"),
        "post-OPOST console bytes must contain b\"hi\\r\\n\"; got {:?}",
        new_bytes,
    );

    // (b) Drained syscall returns. The `write(1, "hi\n", 3)` arm
    //     returns `Ok(3)` — three input bytes consumed, regardless of
    //     OPOST expansion at the device transport (Linux semantics:
    //     `write(2)` reports the count of *input* bytes accepted).
    //     The `exit_group(0)` arm yields `NoReturn`, which the fake
    //     shim never drains, so the slot read returns `None`.
    assert_eq!(
        drained_log.len(),
        2,
        "loop must run exactly two iterations (write, exit_group)",
    );
    assert_eq!(
        drained_log[0],
        Some(Ok(3)),
        "first syscall (write) returns Ok(3) bytes",
    );
    assert_eq!(
        drained_log[1], None,
        "second syscall (exit_group) returns NoReturn — nothing to drain",
    );

    // (c) Process state: init must be a zombie with
    //     `ExitStatus::Exited(0)`. Mirrors the assertion shape used
    //     by tx-shims's `dispatch_exit_group_marks_process_zombie`.
    assert!(init.is_zombie(), "exit_group must zombify init",);
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
}

// ---------------------------------------------------------------------------
// Local async-future driver. Mirrors `tx-shims/src/linux_syscall/tests.rs`'s
// `block_on` because the dispatcher is `async` and Phase 2b's `brk` arm
// `.await`s; the Phase 6 queue only triggers the synchronous arms today
// but we keep the spin-poll shape so later additions to the queue do not
// silently skip pending futures.
// ---------------------------------------------------------------------------

struct NoopWake;

impl std::task::Wake for NoopWake {
    fn wake(self: std::sync::Arc<Self>) {}
    fn wake_by_ref(self: &std::sync::Arc<Self>) {}
}

fn block_on<F: core::future::Future>(mut fut: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    let waker = Waker::from(std::sync::Arc::new(NoopWake));
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
