use core::{
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
};

use tx_hal::{BootHandoff, CpuId, CpuMask, IpiKind, TxPlatform};
use tx_substrate::zone::Cap;
use tx_substrate::SpinMutex;
use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::{Guard, StepOutcome};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::tty::structure::TtyIdentity;
use tx_subsystems::vfs::{Credential, DEntry, InlineName, InodeMeta, RNode, RNodeBacking};

const AP_REACTOR_WAIT_SPINS: usize = 100_000;

static BOOT_REACTOR: tx_reactor::SharedReactor = tx_reactor::SharedReactor::empty();
static AP_REACTOR_TASK_DONE_CPUS: AtomicU64 = AtomicU64::new(0);
static BSP_REACTOR_TIMER_DONE_CPUS: AtomicU64 = AtomicU64::new(0);

/// Global root-mount slot. Populated by `mount_rootfs_tmpfs` after
/// the process subsystem has bootstrapped. The slot retains a strong
/// `Cap<MountIdentity>` for the kernel lifetime, mirroring
/// `tx_subsystems::process::execution::INIT_PROCESS`.
static ROOT_MOUNT: SpinMutex<Option<Cap<MountIdentity>>> = SpinMutex::new(None);

/// Global devfs-mount slot. Populated by `mount_devfs_at_dev`.
/// Retained alongside `ROOT_MOUNT` so the mount table remains live
/// after `init_substrate_if_ready` returns.
static DEV_MOUNT: SpinMutex<Option<Cap<MountIdentity>>> = SpinMutex::new(None);

/// Global TTY identity for the boot console hardware. Populated by
/// `register_console_hardware`; consulted by
/// `register_devfs_console_alias` to publish `/dev/console`.
static CONSOLE_TTY: SpinMutex<Option<Cap<TtyIdentity>>> = SpinMutex::new(None);

/// Snapshot the boot-time root mount cap. Returns `None` until
/// `mount_rootfs_tmpfs` has run (test pre-bootstrap or boot-time
/// pre-mount). Pairs with `ROOT_MOUNT`'s strong-retainer slot so
/// integration tests can observe the mount-table contents without
/// reaching inside `init.rs`.
pub fn root_mount() -> Option<Cap<MountIdentity>> {
    ROOT_MOUNT.lock().clone()
}

/// Snapshot the boot-time devfs mount cap. Returns `None` until
/// `mount_devfs_at_dev` has run.
pub fn dev_mount() -> Option<Cap<MountIdentity>> {
    DEV_MOUNT.lock().clone()
}

/// Snapshot the boot-time console TTY cap. Returns `None` until
/// `register_console_hardware` has run.
pub fn console_tty() -> Option<Cap<TtyIdentity>> {
    CONSOLE_TTY.lock().clone()
}

#[cfg(test)]
pub fn reset_boot_state_for_test() {
    *ROOT_MOUNT.lock() = None;
    *DEV_MOUNT.lock() = None;
    *CONSOLE_TTY.lock() = None;
}

/// Static `CharDeviceOps` impl that forwards `write` to
/// `tx_hal::console_write_str::<P>` and `read` to a zero-byte stub.
///
/// `register_hardware` requires the binding's ops to live for
/// `'static`, so the impl is a zero-sized type and we instantiate it
/// once per platform via the `CONSOLE_BINDING` static below. The
/// `read` arm returns `Done(0)` per the Phase 3b plan: the boot
/// console is write-driven during the trio slice, and a real
/// blocking read shape lands with input-driver work post-trio.
struct ConsoleCharOps<P: TxPlatform> {
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> ConsoleCharOps<P> {
    const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }
}

// `ConsoleCharOps<P>` is always `Send + Sync` regardless of `P` because
// `PhantomData<fn() -> P>` is a zero-sized fn-pointer marker that the
// compiler treats as thread-safe. The impl therefore only needs the
// `TxPlatform + 'static` bounds the binding actually consumes.
impl<P: TxPlatform> CharDeviceOps for ConsoleCharOps<P> {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        // The HAL exposes byte-oriented console writes; tx-kernel's
        // existing init code uses `console_write_str` which calls
        // `P::write_bytes` under the hood. We bypass the str
        // adapter so non-UTF-8 bytes (e.g., raw control sequences)
        // round-trip unchanged.
        <P as tx_hal::ConsoleIf>::write_bytes(bytes);
        StepOutcome::Done(bytes.len())
    }
}

struct SmpRescheduleSignal<P: TxPlatform> {
    _platform: PhantomData<P>,
}

impl<P: TxPlatform> SmpRescheduleSignal<P> {
    const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }
}

impl<P: TxPlatform> tx_reactor::RescheduleSignal for SmpRescheduleSignal<P> {
    fn send_reschedule_ipi(&mut self, target_hart: tx_reactor::HartId) {
        <P as tx_hal::SmpIf>::send_ipi(CpuId(target_hart.0), IpiKind::Reschedule);
    }
}

/// Skeleton H3 boot spine for the generic kernel mainline.
///
/// This type names the ordering that used to live inline in `kernel_main`:
/// platform early init, substrate bring-up when the board supports it,
/// platform later init, full kernel trap vector installation, a tiny reactor
/// smoke task, the board boot sentinel, and shutdown. VFS, device, scheduler,
/// process, and userspace init are intentionally deferred until their
/// substrate contracts exist.
pub struct CoreInit<P: TxPlatform> {
    _platform: PhantomData<P>,
}

impl<P: TxPlatform> CoreInit<P> {
    pub fn boot(handoff: BootHandoff) -> ! {
        Self::init_early(handoff);
        Self::init_substrate_if_ready(handoff);
        Self::boot_sentinel();
        P::system_off()
    }

    fn init_early(handoff: BootHandoff) {
        P::init_early(handoff);
    }

    fn init_substrate_if_ready(handoff: BootHandoff) {
        if P::SUBSTRATE_BOOT_READY {
            tx_substrate::init::<P>();
            Self::init_later(handoff);
            Self::install_kernel_trap_vector();
            Self::init_boot_reactor();
            Self::boot_secondary_cpus();
            Self::run_smp_shootdown_smoke();
            Self::run_smp_ipi_smoke();
            Self::run_reactor_dispatcher_smoke();
            Self::run_zone_smoke();
            Self::run_bsp_reactor_runtime_smoke();
            Self::run_bsp_reactor_timer_idle_smoke();
            Self::init_process_subsystem();

            // ---- Phase 3b boot wiring ----
            //
            // Order invariants (per the trio plan §"Mount wiring at
            // boot"):
            // 1. `init_process_subsystem` runs first — every step
            //    below assumes `INIT_PROCESS` is populated.
            // 2. TTY hardware register **must** precede devfs mount
            //    so the alias is visible at devfs lookup time.
            // 3. Rootfs mount **must** precede devfs mount: devfs
            //    needs a `/dev` directory entry on the rootfs to
            //    mount onto.
            // 4. The console alias must be re-published after devfs
            //    is mounted (`mount_devfs_at_dev` enters a fresh
            //    devfs registry observation window).
            // 5. Init's cwd + fds 0/1/2 are bound last because they
            //    consume the root dentry and the registered console
            //    alias.
            //
            // Future moves of this block must preserve the order.
            Self::register_console_hardware();
            Self::mount_rootfs_tmpfs();
            Self::mount_devfs_at_dev();
            Self::register_devfs_console_alias();
            Self::bind_init_cwd_and_root();

            // Deferred H4 spine slots:
            // - post-substrate init hooks
            // - VFS before device init (now partially landed via
            //   tmpfs+devfs mount wiring above; full step_open / VFS
            //   walker is deferred).
            // - post-device init hooks
            // - scheduler/userspace init
            //
            // Keep these as explicit placeholders until the named subsystems
            // have concrete no_std initialization contracts.
        }
    }

    fn init_process_subsystem() {
        // Allocates an `AddressSpace` for init using the boot platform's
        // pmap, constructs pid=1 via `bootstrap_init_process`, and
        // registers the resulting `Cap` in the global `INIT_PROCESS`
        // slot. Subsequent `step_process_exit` / `sever_children` calls
        // resolve "the kernel's init process" through this slot for
        // reparenting (per `PROCESS_v1` §8.1).
        //
        // The local `Cap` returned by `bootstrap_init_process` is
        // dropped at end-of-scope; `INIT_PROCESS` retains the strong
        // reference for the entire kernel lifetime.
        let aspace =
            tx_subsystems::vm::AddressSpace::new_cap_for_platform::<P>().expect("init aspace");
        let _init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":process:init:ok\n");
    }

    /// Register the boot console as a hardware TTY and stash its cap
    /// in `CONSOLE_TTY` so subsequent steps can publish it under the
    /// `/dev/console` devfs alias. Per `txdoc:TTY-THE-HARDWARE-
    /// CONSOLE-PATH-1` (`docs/design/06_devices/TTY.md` §7).
    ///
    /// **Order invariant:** must precede `mount_devfs_at_dev`.
    /// devfs's `lookup` resolves through `tty::project::resolve_devfs
    /// _alias`, which only sees aliases that `register_hardware` has
    /// already published into the TTY registry.
    pub(crate) fn register_console_hardware() {
        // The binding's `ops` need a `'static` lifetime; the platform-
        // typed wrapper is itself `'static` because `P: 'static`.
        // `CharDeviceBinding` is `Copy + 'static`, so we publish a
        // single static binding per platform via a function-local
        // `static` (each `CoreInit::<P>` instantiation gets its own
        // copy at codegen time).
        static CONSOLE_OPS: SpinMutex<()> = SpinMutex::new(());
        let _serial = CONSOLE_OPS.lock();

        // Allocate the ops + binding once and leak. `register_hardware`
        // expects a `&'static CharDeviceBinding`; we must not free
        // either the ops or the binding for the kernel lifetime.
        // SAFETY: the `Box::leak` shape is the standard tx-fs pattern
        // (see `crates/tx-fs/src/devfs/tests.rs` for the equivalent),
        // and tx-kernel boots are one-shot (no re-entry).
        // NB: tx-kernel is `no_std`, but we use `alloc::boxed::Box`
        // because the global allocator is initialised by the time
        // `init_process_subsystem` returns.
        use alloc::boxed::Box;
        let ops_static: &'static ConsoleCharOps<P> =
            Box::leak(Box::new(ConsoleCharOps::<P>::new()));
        let binding: &'static CharDeviceBinding = Box::leak(Box::new(CharDeviceBinding {
            // Major 5 / minor 1 mirrors Linux's
            // `/dev/console`. Nothing in the trio depends on the
            // exact devt; pick a stable pair.
            devt: DevT::new(5, 1),
            name: "console",
            ops: ops_static,
        }));

        let guard = tx_substrate::epoch::guard();
        let tty = match register_hardware("console", 0, binding, &guard) {
            StepOutcome::Done(tty) => tty,
            other => panic!("register_console_hardware: register_hardware failed: {other:?}"),
        };
        drop(guard);

        *CONSOLE_TTY.lock() = Some(tty);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":tty:console:ok\n");
    }

    /// Mount tmpfs as the rootfs.
    ///
    /// Builds a fresh `Tmpfs` instance, hands it to `MountPayload`
    /// + `MountIdentity::new_cap` (per
    /// `txdoc:MOUNT-MOUNTPAYLOAD-1` /
    /// `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`,
    /// `docs/design/05_filesystem/MOUNT_v1.md`), and stores the
    /// resulting cap in `ROOT_MOUNT`. The mount has no parent and
    /// no mountpoint dentry (it *is* the namespace root), per the
    /// `MountIdentity::new_cap` shape that already accepts
    /// `mountpoint: None` / `parent: None`.
    ///
    /// **Order invariant:** must precede `mount_devfs_at_dev`. The
    /// rootfs supplies the directory `/dev` is mounted on top of.
    pub(crate) fn mount_rootfs_tmpfs() {
        let (_tmpfs, mount_output) = tx_fs::tmpfs::Tmpfs::new_root();
        let payload = MountPayload::new_cap(
            mount_output.fs_ops.clone(),
            mount_output.fs_page_backing.clone(),
            None,
            // dev_id picks an arbitrary stable id for the rootfs
            // mount; full DevId allocation is a follow-up.
            DevId::new(1),
            MountOptions::default(),
            "tmpfs",
            SourceLabel::Static("rootfs"),
        )
        .expect("mount_rootfs_tmpfs: payload reservation");

        // Materialise the root RNode + DEntry. The root dentry
        // carries `InlineName::ROOT` per
        // `crates/tx-subsystems/src/vfs/structure.rs`'s root marker
        // contract (path render emits a single `/` for empty-name
        // dentries).
        let root_rnode = RNode::new_cap(
            mount_output.root_fs_object_id,
            mount_output.root_inode_meta,
            RNodeBacking::Directory,
        )
        .expect("mount_rootfs_tmpfs: root rnode reservation");
        let _root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode.clone())
            .expect("mount_rootfs_tmpfs: root dentry reservation");

        let mount = MountIdentity::new_cap(
            // mount_id is opaque; pick a stable id for the rootfs
            // (`MountId(1)`). Full mount-id allocation is a
            // follow-up that does not affect the trio surface.
            MountId::new(1),
            None,
            root_rnode,
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount_rootfs_tmpfs: mount identity reservation");

        *ROOT_MOUNT.lock() = Some(mount);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":mount:rootfs:tmpfs:ok\n");
    }

    /// Mount devfs at `/dev`.
    ///
    /// Looks up the rootfs's `Tmpfs` backend through
    /// `ROOT_MOUNT`'s `MountPayload::fs_ops` and calls `mkdir("/dev")`
    /// against the root inode (`tmpfs.root_fs_object_id ==
    /// FsObjectId::new(2)`). The resulting directory inode then
    /// serves as the mountpoint for devfs.
    ///
    /// **Order invariant:** must follow `mount_rootfs_tmpfs` (needs
    /// the rootfs DEntry) and precede `register_devfs_console_alias`
    /// (the alias is republished after the mount publication so its
    /// observation window matches devfs's).
    pub(crate) fn mount_devfs_at_dev() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("mount_devfs_at_dev: ROOT_MOUNT must be populated");

        // mkdir("/dev") on the rootfs. The rootfs's fs_ops is the
        // tmpfs instance whose `FsOps::mkdir` actually mutates the
        // tmpfs directory map.
        let guard = tx_substrate::epoch::guard();
        let cred = Credential::default();
        let (dev_object_id, dev_meta) = match root_mount.payload().fs_ops.mkdir(
            tx_fs::tmpfs::TMPFS_ROOT_OBJECT_ID,
            b"dev",
            0o755,
            &cred,
            &guard,
        ) {
            StepOutcome::Done(out) => out,
            other => panic!("mount_devfs_at_dev: tmpfs mkdir(/dev) failed: {other:?}"),
        };
        drop(guard);

        // Build the `/dev` mountpoint DEntry on the rootfs.
        let dev_rnode_in_root = RNode::new_cap(dev_object_id, dev_meta, RNodeBacking::Directory)
            .expect("mount_devfs_at_dev: /dev rnode-on-rootfs reservation");
        let dev_dentry_on_root = DEntry::new_cap(
            InlineName::new(b"dev").expect("mount_devfs_at_dev: /dev inline name"),
            dev_rnode_in_root,
        )
        .expect("mount_devfs_at_dev: /dev dentry-on-rootfs reservation");

        // Build the devfs root RNode. devfs is stateless / static;
        // its root meta is `S_IFDIR | 0o755` per Phase 3a.
        let devfs_fs_ops = tx_fs::devfs::Devfs::fs_ops_arc();
        let devfs_fs_page_backing = tx_fs::devfs::Devfs::fs_page_backing_arc();
        let devfs_root_rnode = RNode::new_cap(
            tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
            InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory,
                tx_fs::devfs::DEVFS_ROOT_MODE,
            ),
            RNodeBacking::Directory,
        )
        .expect("mount_devfs_at_dev: devfs root rnode reservation");

        let devfs_payload = MountPayload::new_cap(
            devfs_fs_ops,
            devfs_fs_page_backing,
            None,
            DevId::new(2),
            MountOptions::default(),
            "devfs",
            SourceLabel::Static("devfs"),
        )
        .expect("mount_devfs_at_dev: payload reservation");

        let dev_mount = MountIdentity::new_cap(
            MountId::new(2),
            Some(dev_dentry_on_root),
            devfs_root_rnode,
            Some(root_mount),
            devfs_payload,
            MountFlags::empty(),
        )
        .expect("mount_devfs_at_dev: mount identity reservation");

        *DEV_MOUNT.lock() = Some(dev_mount);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":mount:devfs:ok\n");
    }

    /// Re-publish `console` under devfs's alias table.
    ///
    /// `register_hardware("console", ...)` already published the
    /// alias once (see `register_console_hardware` and
    /// `tty::structure::registry::register_devfs_alias`). Calling
    /// `register_console_alias` here lets the trio plan's spec read
    /// — devfs's `lookup` is what resolves `/dev/console`, and the
    /// alias must be visible **after** `mount_devfs_at_dev` so any
    /// follow-up code that re-binds the registry sees the same
    /// snapshot devfs's lookup walks.
    ///
    /// **Order invariant:** runs after `mount_devfs_at_dev` and
    /// before `bind_init_cwd_and_root` (which preopens
    /// `/dev/console` for fds 0/1/2).
    pub(crate) fn register_devfs_console_alias() {
        let tty =
            console_tty().expect("register_devfs_console_alias: console TTY must be registered");
        match register_console_alias("console", tty) {
            StepOutcome::Done(()) => {}
            other => {
                panic!("register_devfs_console_alias: register_console_alias failed: {other:?}")
            }
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":devfs:alias:console:ok\n");
    }

    /// Install init's initial cwd at the rootfs root and preopen
    /// fds 0/1/2 against `/dev/console`.
    ///
    /// Until VFS's `step_open` exists, the bootstrap helper
    /// `tx_fs::devfs::open_console_for_init()` materialises an
    /// `OpenFile` directly from the registered TTY (see
    /// `crates/tx-fs/src/devfs.rs`'s docstring). Once the walker
    /// lands, this becomes
    /// `step_open("/dev/console", O_RDWR)` invoked from the same
    /// helper.
    ///
    /// **Order invariant:** must run last; consumes the root
    /// dentry, the `/dev/console` alias, and the `INIT_PROCESS`
    /// slot all populated by earlier steps.
    pub(crate) fn bind_init_cwd_and_root() {
        let init = tx_subsystems::process::execution::init_process()
            .expect("bind_init_cwd_and_root: INIT_PROCESS must be populated");

        // Re-derive the root dentry from `ROOT_MOUNT.root` so the
        // chdir target shares the same RNode the mount table
        // exposes. We could have stashed the dentry from
        // `mount_rootfs_tmpfs` directly, but going through the
        // mount keeps the boot wiring spec-shaped: cwd is always
        // a DEntry over a mount's root rnode.
        let root_mount =
            root_mount().expect("bind_init_cwd_and_root: ROOT_MOUNT must be populated");
        let root_rnode = root_mount.root().clone();
        let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode)
            .expect("bind_init_cwd_and_root: cwd dentry reservation");
        let _outcome = tx_subsystems::process::execution::step_chdir(&init, root_dentry);

        // Preopen fds 0/1/2. Each call materialises a fresh
        // `OpenFile` over the same console TTY; the kernel's fd
        // table holds three independent `Cap<OpenFile>` capability
        // instances. POSIX-shape dup3 sharing is a follow-up.
        for fd in 0..3 {
            let console = tx_fs::devfs::open_console_for_init();
            let _prev = init.set_fd(fd, Some(console));
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":init:cwd-fds:ok\n");
    }

    fn init_later(handoff: BootHandoff) {
        P::init_later(handoff);
    }

    fn install_kernel_trap_vector() {
        P::install_kernel_trap_vector();
    }

    fn init_boot_reactor() {
        let _ = BOOT_REACTOR.init();
    }

    fn boot_secondary_cpus() {
        let started = P::boot_secondary_cpus(Self::secondary_cpu_entry);
        if started > 0 {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":smp:aps:online\n");
        }
    }

    fn run_smp_shootdown_smoke() {
        if P::online_cpu_count() <= 1 {
            return;
        }

        P::shootdown_kernel_mapping(tx_hal::PmapInvalidation::new(tx_hal::VirtAddr(0), 4096));
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":smp:shootdown:ok\n");
    }

    fn run_smp_ipi_smoke() {
        let targets = CpuMask::from_bits(
            P::online_cpus().bits()
                & !CpuMask::single(<P as tx_hal::SmpIf>::current_cpu_id()).bits(),
        );
        if targets.is_empty() {
            return;
        }

        P::clear_ipi_ack_cpus(IpiKind::Reschedule, targets);
        P::broadcast_ipi(targets, IpiKind::Reschedule);
        let acked = P::wait_for_ipi_ack_cpus(targets, IpiKind::Reschedule);
        assert_eq!(acked, targets.count(), "SMP IPI smoke acknowledgements");

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":smp:ipi:ok\n");
    }

    fn run_reactor_dispatcher_smoke() {
        let Some(target_cpu) = Self::first_remote_online_cpu() else {
            return;
        };

        let target_hart = tx_reactor::HartId(target_cpu.0);
        let current_hart = tx_reactor::HartId(<P as tx_hal::SmpIf>::current_cpu_id().0);
        let mask = tx_reactor::wait::Mask::from_bits(0x1);
        let targets = CpuMask::single(target_cpu);
        Self::clear_ap_reactor_task_done(targets);

        let channel = BOOT_REACTOR
            .with(|reactor| {
                let channel = reactor.channel();
                reactor.submit_task_with_meta(
                    {
                        let channel = channel.clone();
                        async move {
                            let _ = channel.wait(mask).await;
                            Self::mark_ap_reactor_task_done(target_cpu);
                        }
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(target_cpu).bits()),
                );
                channel
            })
            .expect("boot reactor must be initialized before AP dispatcher smoke");

        let parked = BOOT_REACTOR
            .with(|reactor| reactor.run_until_idle_on_hart(target_hart))
            .expect("boot reactor must be initialized before AP dispatcher smoke");
        assert!(
            parked.polled <= 1,
            "reactor dispatcher smoke initial poll count"
        );

        P::clear_ipi_ack_cpus(IpiKind::Reschedule, targets);
        assert_eq!(
            channel.fire(mask),
            1,
            "reactor dispatcher wake registration"
        );

        let mut signal = SmpRescheduleSignal::<P>::new();
        let report = BOOT_REACTOR
            .with(|reactor| reactor.drain_wakes_for_hart(current_hart, &mut signal))
            .expect("boot reactor must be initialized before AP dispatcher smoke");
        assert_eq!(report.remote_ipis, 1, "reactor dispatcher remote IPI count");

        let acked = P::wait_for_ipi_ack_cpus(targets, IpiKind::Reschedule);
        assert_eq!(acked, targets.count(), "reactor dispatcher IPI ack");

        let ran = Self::wait_for_ap_reactor_task_done(targets);
        assert_eq!(ran, targets.count(), "reactor AP loop work completion");

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:dispatch:ipi:ok\n");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:ap-loop:ok\n");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:ap-runqueue:ok\n");
    }

    fn first_remote_online_cpu() -> Option<CpuId> {
        let online = P::online_cpus();
        let current = <P as tx_hal::SmpIf>::current_cpu_id();
        let mut cpu = 0;
        while cpu < u64::BITS as usize {
            let candidate = CpuId(cpu);
            if candidate != current && online.contains(candidate) {
                return Some(candidate);
            }
            cpu += 1;
        }
        None
    }

    unsafe extern "C" fn secondary_cpu_entry(cpu_id: usize) -> ! {
        let cpu_id = CpuId(cpu_id);
        P::install_early_percpu(cpu_id);
        P::init_early_secondary(cpu_id);
        tx_substrate::init_on_ap(cpu_id).expect("tx_kernel AP substrate initialization failed");
        P::init_later_secondary(cpu_id);
        P::install_kernel_trap_vector();
        P::mark_cpu_online(cpu_id);
        Self::secondary_reactor_loop(cpu_id)
    }

    fn secondary_reactor_loop(cpu_id: CpuId) -> ! {
        P::enable_ipi_wakeups();
        P::enable_timer_wakeups();
        loop {
            if Self::run_secondary_reactor_once(cpu_id) {
                continue;
            }

            P::wait_for_interrupt_once();
            if P::pending_ipi(IpiKind::Reschedule) {
                P::ack_ipi(IpiKind::Reschedule);
            }
        }
    }

    fn run_secondary_reactor_once(cpu_id: CpuId) -> bool {
        Self::step_boot_reactor_once(cpu_id).is_some_and(|step| !step.should_idle())
    }

    fn step_boot_reactor_once(cpu_id: CpuId) -> Option<tx_reactor::hart_loop::HartLoopStep> {
        let hart = tx_reactor::HartId(cpu_id.0);
        let now_ns = P::read_ns();
        let mut signal = SmpRescheduleSignal::<P>::new();
        let step = BOOT_REACTOR.with(|reactor| {
            tx_reactor::hart_loop::step_hart_loop_at(reactor, hart, now_ns, &mut signal)
        })?;
        Self::program_hart_loop_deadline(step.deadline_action);
        Some(step)
    }

    fn program_hart_loop_deadline(action: tx_reactor::hart_loop::HartLoopDeadlineAction) {
        match action {
            tx_reactor::hart_loop::HartLoopDeadlineAction::Arm { deadline_ns } => {
                P::set_deadline_ns(deadline_ns)
            }
            tx_reactor::hart_loop::HartLoopDeadlineAction::Cancel => P::cancel_deadline(),
        }
    }

    fn clear_ap_reactor_task_done(cpus: CpuMask) {
        let bits = cpus.bits();
        AP_REACTOR_TASK_DONE_CPUS.fetch_and(!bits, Ordering::AcqRel);
    }

    fn mark_ap_reactor_task_done(cpu_id: CpuId) {
        let bit = Self::cpu_bit(cpu_id);
        if bit == 0 {
            return;
        }

        AP_REACTOR_TASK_DONE_CPUS.fetch_or(bit, Ordering::Release);
    }

    fn wait_for_ap_reactor_task_done(cpus: CpuMask) -> usize {
        let target = cpus.bits();
        for _ in 0..AP_REACTOR_WAIT_SPINS {
            let done = AP_REACTOR_TASK_DONE_CPUS.load(Ordering::Acquire) & target;
            if done == target {
                return cpus.count();
            }
            core::hint::spin_loop();
        }

        (AP_REACTOR_TASK_DONE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
    }

    fn cpu_bit(cpu_id: CpuId) -> u64 {
        CpuMask::single(cpu_id).bits()
    }

    fn run_zone_smoke() {
        tx_subsystems::zones::run_smoke::<P>().expect("tx_kernel zone smoke failed");
    }

    fn run_bsp_reactor_runtime_smoke() {
        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        BOOT_REACTOR
            .with(|reactor| {
                reactor.submit_task_with_meta(
                    async {
                        Self::write_board_sentinel_prefix();
                        tx_hal::console_write_str::<P>(":reactor:task:ok\n");
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(current_cpu).bits()),
                );
            })
            .expect("boot reactor must be initialized before BSP runtime smoke");

        let step =
            Self::step_boot_reactor_once(current_cpu).expect("boot reactor runtime step failed");
        assert_eq!(step.stats.polled, 1, "BSP runtime smoke task poll count");
        assert_eq!(
            step.stats.completed, 1,
            "BSP runtime smoke task completion count"
        );

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:runtime-loop:ok\n");
    }

    fn run_bsp_reactor_timer_idle_smoke() {
        const TIMER_SMOKE_DELTA_NS: u64 = 5_000_000;

        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        let cpu_bit = Self::cpu_bit(current_cpu);
        if cpu_bit == 0 {
            return;
        }

        BSP_REACTOR_TIMER_DONE_CPUS.fetch_and(!cpu_bit, Ordering::AcqRel);
        let deadline_ns = P::read_ns().saturating_add(TIMER_SMOKE_DELTA_NS);

        BOOT_REACTOR
            .with(|reactor| {
                let channel = reactor.channel();
                let mask = tx_reactor::wait::Mask::from_bits(0x1);
                reactor.submit_task_with_meta(
                    async move {
                        let outcome = channel
                            .wait_event(
                                mask,
                                tx_reactor::wait::WaitProtocol::InterruptibleTimeout(deadline_ns),
                                || false,
                            )
                            .await;
                        assert_eq!(outcome, tx_reactor::wait::WaitOutcome::TimedOut);
                        BSP_REACTOR_TIMER_DONE_CPUS.fetch_or(cpu_bit, Ordering::Release);
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(current_cpu).bits()),
                );
            })
            .expect("boot reactor must be initialized before BSP timer smoke");

        let armed =
            Self::step_boot_reactor_once(current_cpu).expect("boot reactor timer arm step failed");
        assert_eq!(
            armed.next_deadline_ns,
            Some(deadline_ns),
            "BSP timer smoke deadline"
        );

        P::enable_timer_wakeups();
        for _ in 0..AP_REACTOR_WAIT_SPINS {
            let step = Self::step_boot_reactor_once(current_cpu)
                .expect("boot reactor timer idle step failed");
            if Self::bsp_timer_smoke_done(cpu_bit) {
                assert!(step.observed_timer_wakes(), "BSP timer smoke wake");
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":reactor:timer-idle:ok\n");
                return;
            }
            P::wait_for_interrupt_once();
        }

        panic!("BSP reactor timer idle smoke did not complete");
    }

    fn bsp_timer_smoke_done(cpu_bit: u64) -> bool {
        BSP_REACTOR_TIMER_DONE_CPUS.load(Ordering::Acquire) & cpu_bit != 0
    }

    fn boot_sentinel() {
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":boot:ok\n");
    }

    fn write_board_sentinel_prefix() {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
    }
}

#[cfg(test)]
mod tests;
