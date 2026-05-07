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
    self, MountFlags, MountIdentity, MountOptions, MountPayload, SourceLabel,
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
        // ELF-loader Phase 7: register the embedded `/init` fixture
        // into the rootfs tmpfs and drive `exec_script` synchronously
        // so the init leader's `saved_user_context` is seeded with
        // the new image's entry-point + initial stack pointer. The
        // BSP reactor loop below then picks up the seeded context
        // on its first poll and re-enters userspace through the
        // production trap return path.
        //
        // Open Q #3 (DECIDED 2026-05-06) — bootstrap exec failure
        // is a boot-time invariant violation. The helper panics
        // with the `:bootstrap-exec:fail` board sentinel on `Err`;
        // CI catches the panic loudly and we never reach
        // `boot_sentinel` (`:boot:ok`). No fallback to the trio's
        // hand-built userspace path: that path is retired by the
        // Phase 7 dual-codepath cleanup, and reintroducing it
        // would re-create the dual code path the trio just
        // collapsed.
        //
        // Order (per Phase 7 plan): runs after
        // `bind_init_cwd_and_root` (init has cwd + fds 0/1/2) and
        // before `boot_sentinel` so the panic-sentinel discipline
        // is unambiguous.
        if P::SUBSTRATE_BOOT_READY {
            Self::run_bootstrap_exec_for_init();
        }
        Self::boot_sentinel();
        // Pre-ELF Phase 7: wrap the init leader's thread future as a
        // reactor task and enter the BSP reactor loop. Returns when
        // init zombifies (`run_thread` resolves), at which point we
        // emit `:userspace:exited:N` and shut down. With Phase 7 of
        // the ELF-loader plan, the leader's `saved_user_context` was
        // seeded by `run_bootstrap_exec_for_init` above; the first
        // iteration of `run_thread` re-enters userspace at the
        // fixture's `_start` and the eventual `exit_group(0)` syscall
        // zombifies init.
        if P::SUBSTRATE_BOOT_READY {
            Self::run_userspace_reactor_loop();
        }
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
            //
            // Pre-ELF Phase 5 (item 9) inserts `install_irq_handlers`
            // between `register_console_hardware` and
            // `mount_rootfs_tmpfs`: the UART RX handler reads the
            // boot console TTY from `CONSOLE_TTY` (populated by
            // `register_console_hardware`); registration must follow
            // that slot being populated. The PLIC's enable bits are
            // zero until `unmask` runs inside
            // `install_irq_handlers`, so a stray pre-registration
            // trap is structurally impossible (Cross-cutting risk
            // #4 in the pre-ELF plan).
            Self::register_console_hardware();
            Self::install_irq_handlers();
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

    /// Pre-ELF Phase 5 (item 9): install the kernel's IRQ dispatch
    /// table and unmask the platform's UART IRQ.
    ///
    /// Delegates to `crate::irq::install_irq_handlers::<P>` which
    /// registers the UART RX handler under
    /// `<P as IrqIf>::UART_IRQ`, publishes
    /// `IRQ_DISPATCH_TABLE` to the platform via
    /// `<P as IrqIf>::install_dispatch_table`, then unmasks. The
    /// UART RX handler reads the boot console TTY from `CONSOLE_TTY`,
    /// so this step must follow `register_console_hardware`.
    ///
    /// **Order invariant:** runs after `register_console_hardware`
    /// (the handler reads `console_tty()`) and before
    /// `mount_rootfs_tmpfs` (no transitive dependency, but the
    /// existing trio order has been preserved verbatim except for
    /// this insertion).
    pub(crate) fn install_irq_handlers() {
        crate::irq::install_irq_handlers::<P>();

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":irq:install:ok\n");
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
        // Allocate ids through the centralised allocators
        // (`txdoc:MOUNT-MOUNTPAYLOAD-1`,
        // `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`). The
        // allocators are deterministic from cold start: rootfs claims
        // `MountId(1)` / `DevId(1)`; `mount_devfs_at_dev` then claims
        // `MountId(2)` / `DevId(2)`. Existing trio boot-smoke
        // assertions on the literal ids stay valid.
        let payload = MountPayload::new_cap(
            mount_output.fs_ops.clone(),
            mount_output.fs_page_backing.clone(),
            None,
            mount::allocate_dev_id(),
            MountOptions::default(),
            "tmpfs",
            SourceLabel::Static("rootfs"),
        )
        .expect("mount_rootfs_tmpfs: payload reservation");

        // Materialise the root RNode + DEntry. The root dentry
        // carries `InlineName::ROOT` per
        // `crates/tx-subsystems/src/vfs/structure.rs`'s root marker
        // contract (path render emits a single `/` for empty-name
        // dentries). The root rnode also carries a
        // `containing_mount` weak pointing at the rootfs payload so
        // the VFS walker (`crate::vfs::walker::step_walk`) can resolve
        // the in-scope `FsOps` from any dentry rooted on this rnode.
        // Without the hint the walker emits `ENODEV` on the first
        // interior component (per `fs_ops_for` in `walker.rs`).
        let root_rnode = {
            let raw = RNode::new(
                mount_output.root_fs_object_id,
                mount_output.root_inode_meta,
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = tx_substrate::zone::reserve_for::<RNode>()
                .expect("mount_rootfs_tmpfs: root rnode reservation");
            tx_substrate::zone::sign_for(res, raw)
        };
        let _root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode.clone())
            .expect("mount_rootfs_tmpfs: root dentry reservation");

        let mount = MountIdentity::new_cap(
            mount::allocate_mount_id(),
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
        // Bootstrap path runs as root by construction.
        let cred = Credential::root();
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

        // Build the devfs payload before its root RNode so the rnode
        // can carry a `containing_mount` weak — same reason as the
        // rootfs root rnode above (the walker's `fs_ops_for` returns
        // `None` and falls through to `ENODEV` if the hint is
        // missing).
        let devfs_fs_ops = tx_fs::devfs::Devfs::fs_ops_arc();
        let devfs_fs_page_backing = tx_fs::devfs::Devfs::fs_page_backing_arc();

        let devfs_payload = MountPayload::new_cap(
            devfs_fs_ops,
            devfs_fs_page_backing,
            None,
            mount::allocate_dev_id(),
            MountOptions::default(),
            "devfs",
            SourceLabel::Static("devfs"),
        )
        .expect("mount_devfs_at_dev: payload reservation");

        let devfs_root_rnode = {
            let raw = RNode::new(
                tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
                InodeMeta::new(
                    tx_subsystems::vfs::InodeKind::Directory,
                    tx_fs::devfs::DEVFS_ROOT_MODE,
                ),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&devfs_payload);
            let res = tx_substrate::zone::reserve_for::<RNode>()
                .expect("mount_devfs_at_dev: devfs root rnode reservation");
            tx_substrate::zone::sign_for(res, raw)
        };

        // Snapshot the rootfs's payload before consuming `root_mount`
        // into the new mount's `parent` slot. The mount-table
        // registration below keys on the rootfs payload + `/dev`'s
        // FsObjectId on rootfs.
        let rootfs_payload = root_mount.payload().clone();

        let dev_mount = MountIdentity::new_cap(
            mount::allocate_mount_id(),
            Some(dev_dentry_on_root),
            devfs_root_rnode,
            Some(root_mount),
            devfs_payload,
            MountFlags::empty(),
        )
        .expect("mount_devfs_at_dev: mount identity reservation");

        // Publish the mount in the kernel's mount-point registry so
        // the VFS walker (`crate::vfs::walker::step_walk`) can cross
        // from rootfs into devfs at `/dev`. Per
        // `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`: register *after*
        // the mount's payload is signed and *before* the slot
        // publishes, so any walker observation that races us either
        // sees the registered mount or no mount at all (never a
        // half-built one).
        mount::register_mount(&rootfs_payload, dev_object_id, dev_mount.clone());

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

    /// ELF-loader Phase 7: register the embedded `/init` fixture into
    /// the rootfs tmpfs and synchronously drive `exec_script` against
    /// init. On `Err` panics with `:bootstrap-exec:fail` (Open Q #3
    /// DECIDED 2026-05-06).
    ///
    /// Two sub-steps:
    /// 1. `register_init_fixture_into_tmpfs` — call
    ///    `FsOps::create_inode` on the rootfs to allocate `/init`,
    ///    materialise its RNode (through tmpfs's Phase-7
    ///    `materialise_rnode` override) so we can reach the inode's
    ///    `Cap<PageContainer>`, copy the fixture bytes into the
    ///    container's anon pages via the kernel direct map, and
    ///    update the inode's visible size via `step_truncate`.
    /// 2. `drive_bootstrap_exec` — `block_on(exec_script::<P>(...))`
    ///    against init; on `Ok(())` the eight-phase EXEC_v1 protocol
    ///    has seeded `process.aspace` with the new aspace and
    ///    `thread.payload().saved_user_context` with the fixture's
    ///    entry-point + initial stack pointer.
    pub(crate) fn run_bootstrap_exec_for_init() {
        Self::register_init_fixture_into_tmpfs();
        Self::drive_bootstrap_exec();
    }

    /// Sub-step 1 of `run_bootstrap_exec_for_init`: copy
    /// `init_fixture::INIT_FIXTURE_BYTES` into a fresh tmpfs file at
    /// `/init`.
    ///
    /// Uses the production `FsOps` surface end-to-end:
    /// `create_inode` to allocate the inode + name binding,
    /// `materialise_rnode` to grab the inode's `Cap<PageContainer>`
    /// (Phase 7 of the ELF-loader plan added the tmpfs override),
    /// `materialize_anon` per page + direct-map memcpy to populate
    /// the page contents, and `FsPageBacking::truncate` to set the
    /// visible size.
    pub(crate) fn register_init_fixture_into_tmpfs() {
        use tx_subsystems::execution::StepOutcome;
        use tx_subsystems::vfs::{Credential, RNodeBacking};

        let root_mount =
            root_mount().expect("register_init_fixture_into_tmpfs: ROOT_MOUNT must be populated");
        let fs_ops = root_mount.payload().fs_ops.clone();
        let fs_page_backing = root_mount.payload().fs_page_backing.clone();
        let root_object_id = root_mount.root().fs_object_id();

        let bytes = &init_fixture::INIT_FIXTURE_BYTES[..];

        // Allocate the inode. Bootstrap process is root by construction.
        let cred = Credential::root();
        let (file_id, file_meta) = {
            let guard = tx_substrate::epoch::guard();
            let outcome = fs_ops.create_inode(root_object_id, b"init", 0o100755, &cred, &guard);
            match outcome {
                StepOutcome::Done(out) => out,
                other => panic!("register_init_fixture_into_tmpfs: create_inode(/init): {other:?}"),
            }
        };

        // Materialise the inode's RNode so we can reach the
        // `Cap<PageContainer>`. Tmpfs's Phase-7 override returns
        // `RNodeBacking::PageBacked { pc }` for regular files.
        let pc = {
            let guard = tx_substrate::epoch::guard();
            let outcome = fs_ops.materialise_rnode(file_id, file_meta, &guard);
            let rnode = match outcome {
                StepOutcome::Done(rnode) => rnode,
                other => panic!("register_init_fixture_into_tmpfs: materialise_rnode: {other:?}"),
            };
            match rnode.backing() {
                RNodeBacking::PageBacked { pc } => pc.clone(),
                other => panic!(
                    "register_init_fixture_into_tmpfs: tmpfs materialise_rnode \
                     returned non-PageBacked backing: {other:?}"
                ),
            }
        };

        // Populate every page covered by the fixture bytes via
        // `materialize_anon` + direct-map memcpy. Mirrors
        // `tx_scripts::process::exec::script::tests::ExecTestFs::add_regular_with_bytes`'s
        // pattern, but against the production tmpfs surface.
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE;
        for (idx, chunk) in bytes.chunks(page_size).enumerate() {
            let materialised = pc
                .materialize_anon(
                    tx_subsystems::page_backed::PageIndex::new(idx as u64),
                    tx_subsystems::page_backed::MaterializeAccess::Write,
                )
                .expect("register_init_fixture_into_tmpfs: materialize_anon");
            let frame_base = tx_substrate::page_allocator::frame_kernel_addr(materialised.ppn)
                .expect("register_init_fixture_into_tmpfs: direct-map view");
            // SAFETY: `materialised.map_pin` keeps the page resident
            // for the duration of this scope; the destination region
            // covers exactly `chunk.len()` bytes from the freshly
            // materialised anon frame; source and destination do not
            // overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
            }
        }

        // Set the visible byte-size on both the PageContainer (so
        // `read_exact_at` in `exec_script::<P>` knows the file's
        // length) and the inode meta (so subsequent `load_inode_meta`
        // reports the right `meta.size`).
        let size = bytes.len() as u64;
        {
            let guard = tx_substrate::epoch::guard();
            match fs_page_backing.truncate(file_id, size, &guard) {
                StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
                other => panic!("register_init_fixture_into_tmpfs: truncate({size}): {other:?}"),
            }
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":init:fixture:ok\n");
    }

    /// Sub-step 2 of `run_bootstrap_exec_for_init`: drive
    /// `exec_script` synchronously and panic with
    /// `:bootstrap-exec:fail` on `Err`.
    ///
    /// `exec_script` is `async`, but the only `.await` points it
    /// reaches today are `read_exact_at` against the file's
    /// `Cap<PageContainer>` (immediate under tmpfs, since every page
    /// is anon and resident after `register_init_fixture_into_tmpfs`)
    /// and `populate_detached_user_range` (also immediate against
    /// detached anon segments). A noop-waker `block_on` poll loop
    /// resolves the future without going through the reactor.
    fn drive_bootstrap_exec() {
        use tx_subsystems::vfs::Credential;

        let init = tx_subsystems::process::execution::init_process()
            .expect("drive_bootstrap_exec: INIT_PROCESS must be populated");
        let thread = init
            .nth_thread(0)
            .expect("drive_bootstrap_exec: init has a leader thread post-bootstrap");

        // Bootstrap exec runs as init (root) by construction.
        let cred = Credential::root();
        let argv: &[&[u8]] = &[b"init" as &[u8]];
        let envp: &[&[u8]] = &[];

        // `exec_script` opens its own fresh epoch guards inside V1
        // (`build_aspace_from_image`) and V2
        // (`populate_detached_user_range`); the caller must NOT hold
        // a guard at the call site (per
        // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
        let outcome = bootstrap_block_on(tx_scripts::process::exec::exec_script::<P>(
            &init, &thread, b"/init", argv, envp, &cred,
        ));
        match outcome {
            Ok(()) => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:ok\n");
            }
            Err(e) => {
                // Open Q #3 (DECIDED 2026-05-06): panic loudly with
                // the `:bootstrap-exec:fail` board sentinel.
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:fail\n");
                panic!("bootstrap exec for /init failed: {e:?}");
            }
        }
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

    /// Wave 1 of the fork/clone/wait4 slice: install the reactor-
    /// submission hook in `tx_subsystems::reactor_submit` so a
    /// future `sys_clone` arm in `tx-shims` (Wave 2) can submit
    /// freshly-forked child threads to the BSP boot reactor without
    /// taking a circular dependency back into `tx-kernel`.
    ///
    /// The installed function pointer routes through
    /// [`Self::submit_child_thread_into_boot_reactor`], which
    /// captures the platform parameter `P` so the seam can stay
    /// parameter-free at the read site.
    pub(crate) fn install_reactor_submit_seam() {
        tx_subsystems::reactor_submit::install_submit_child_thread(
            Self::submit_child_thread_into_boot_reactor,
        );
    }

    /// Reactor-submission hook body. Captured by
    /// [`Self::install_reactor_submit_seam`] as a plain `fn` pointer
    /// (the platform parameter `P` is monomorphised at install time
    /// so the resulting fn pointer is parameter-free).
    ///
    /// Builds `PerHartSlotted::<P, _>::new(payload,
    /// run_thread::<P>(child_thread, payload))` and submits via
    /// `BOOT_REACTOR.submit_task`. `child_thread.payload_cap()`
    /// returning `None` would indicate the child is already a
    /// zombie, which violates `step_fork`'s post-condition and
    /// `seed_child_leader_context`'s precondition; we treat it as a
    /// silent no-op rather than panicking because the syscall arm
    /// has its own error reporting path.
    fn submit_child_thread_into_boot_reactor(
        _child_process: tx_substrate::zone::Cap<tx_subsystems::process::ProcessIdentity>,
        child_thread: tx_substrate::zone::Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) {
        let Some(payload) = child_thread.payload_cap() else {
            // Kernel-invariant violation: a freshly-cloned child
            // thread should always be live-with-payload. Wave 2's
            // sys_clone has its own error reporting path; we don't
            // panic here so the caller sees the error surface.
            return;
        };
        let task_payload = payload.clone();
        let _ = BOOT_REACTOR.with(|reactor| {
            reactor.submit_task(crate::thread_future::PerHartSlotted::<P, _>::new(
                task_payload.clone(),
                crate::thread_future::run_thread::<P>(child_thread, task_payload),
            ));
        });
    }

    /// Pre-ELF Phase 7: submit init's leader thread future as a
    /// reactor task, then drive the BSP hart-loop until the future
    /// resolves. Resolution happens when `run_thread` returns — the
    /// only `return` paths inside the future today are
    /// `SyscallResult::NoReturn` (i.e. `exit_group` zombified the
    /// process), the page-fault `Err` SIGSEGV route, and the
    /// `Fatal` trap arm. All three end with a zombified init.
    ///
    /// On exit, emits `:userspace:exited:N` where `N` is init's
    /// recorded `ExitStatus::wait_status_word()`, then returns to
    /// the caller, which calls `system_off`.
    ///
    /// **Wiring choice.** The Phase 7 plan considered both
    /// `Reactor::run_until_idle_on_hart_with_reschedule` and
    /// `Reactor::run_forever_on_hart`. Neither exists in tree today.
    /// The board crate's secondary harts already drive the boot
    /// reactor through `step_hart_loop_at` (see
    /// `secondary_reactor_loop`), so the BSP uses the same shape:
    /// each iteration advances time, runs ready tasks, programs the
    /// next deadline, and idles via `wait_for_interrupt_once` if
    /// the loop went idle. This is the closest seam to the plan's
    /// guesses and keeps the BSP and APs symmetric.
    fn run_userspace_reactor_loop() {
        // Wave 1 of the fork/clone/wait4 slice (2026-05-06): install
        // the reactor-submission seam so a future `sys_clone` arm
        // (Wave 2) can route freshly-forked child threads through
        // the BSP boot reactor without taking a circular crate
        // dependency back into `tx-kernel`. The seam lives in
        // `tx_subsystems::reactor_submit` (a function-pointer slot);
        // the platform parameter `P` is captured at install time
        // here so the seam stays parameter-free at the call site.
        Self::install_reactor_submit_seam();

        let Some(init) = tx_subsystems::process::execution::init_process() else {
            // No init process — nothing to drive. Skip cleanly.
            return;
        };
        let Some(thread) = init.nth_thread(0) else {
            return;
        };
        let Some(payload) = thread.payload_cap() else {
            return;
        };

        let task_payload = payload.clone();
        let submit_thread = thread.clone();
        let submitted = BOOT_REACTOR.with(|reactor| {
            reactor.submit_task(crate::thread_future::PerHartSlotted::<P, _>::new(
                task_payload.clone(),
                crate::thread_future::run_thread::<P>(submit_thread, task_payload),
            ));
        });
        if submitted.is_none() {
            // Boot reactor not initialised; nothing to drive.
            return;
        }

        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        P::enable_timer_wakeups();

        // Drive the BSP reactor loop until init zombifies. Each
        // iteration is a `step_hart_loop_at` step: advance time, run
        // ready tasks, program the next deadline. Block on WFI when
        // the step reports idle so we don't spin-wait for the next
        // userspace trap (which is the only event that resolves the
        // thread future's pending wait).
        loop {
            if init.is_zombie() {
                break;
            }

            let step = match Self::step_boot_reactor_once(current_cpu) {
                Some(step) => step,
                None => break,
            };

            if step.should_idle() && !init.is_zombie() {
                P::wait_for_interrupt_once();
                if P::pending_ipi(IpiKind::Reschedule) {
                    P::ack_ipi(IpiKind::Reschedule);
                }
            }
        }

        // init zombified — emit the exit sentinel.
        let status_word = init
            .exit_status()
            .map(|s| s.wait_status_word())
            .unwrap_or(0);
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":userspace:exited:");
        Self::write_signed_decimal(status_word);
        tx_hal::console_write_str::<P>("\n");
    }

    /// Render a signed decimal int into the platform console without
    /// allocating. `tx_hal::console_write_str` is byte-oriented so
    /// the formatter writes one chunk per call. Stack-bounded:
    /// `i32::MIN` produces 11 bytes (`-2147483648`).
    fn write_signed_decimal(value: i32) {
        let mut buf = [0u8; 11];
        let mut idx = buf.len();
        let mut v: i64 = value as i64;
        let negative = v < 0;
        if negative {
            v = -v;
        }
        if v == 0 {
            idx -= 1;
            buf[idx] = b'0';
        } else {
            while v > 0 {
                idx -= 1;
                buf[idx] = b'0' + (v % 10) as u8;
                v /= 10;
            }
        }
        if negative {
            idx -= 1;
            buf[idx] = b'-';
        }
        let s = core::str::from_utf8(&buf[idx..])
            .expect("write_signed_decimal: ASCII digits are always UTF-8");
        tx_hal::console_write_str::<P>(s);
    }

    fn write_board_sentinel_prefix() {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
    }
}

/// Synchronously poll a future to completion using a noop waker.
///
/// Used by `CoreInit::drive_bootstrap_exec` to drive `exec_script`'s
/// future without spinning up the reactor: the boot path runs before
/// the BSP reactor loop is entered, and `exec_script` only awaits on
/// page-pull operations that resolve immediately under tmpfs.
///
/// The bound `1024` polls is chosen to mirror the matching pattern in
/// `tx-shims`'s and `tx-scripts`'s test suites; reaching the cap
/// signals a logic bug (a never-resolving future inside the
/// boot-time exec path) and triggers a panic.
fn bootstrap_block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // Build a no-op waker without `alloc::sync::Arc`'s `Wake` trait,
    // because tx-kernel's runtime allocator at boot does not have
    // `Arc::new` plumbing wired by the time `bootstrap_block_on` is
    // called. The raw-waker shape is a stable `core` API that
    // sidesteps the `alloc` requirement entirely.
    fn raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }

    // SAFETY: the raw waker's vtable functions are all no-op or
    // re-construction; the data pointer is never dereferenced.
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut future = future;
    // SAFETY: `future` lives on this stack frame for the duration of
    // the loop and is never moved after pinning.
    let mut pinned = unsafe { Pin::new_unchecked(&mut future) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("bootstrap_block_on: future did not resolve in 1024 polls");
}

mod init_fixture;

/// DAC + setuid slice (Wave 5, Part 8): sibling fixture for the
/// end-to-end setuid smoke. See `init_setuid_fixture.rs`'s module
/// header for the deviation from Plan Q4 (extend-in-place was
/// authored before the fork/clone/wait4 slice rewrote the existing
/// fixture into a fork+wait+exit binary; sibling fixture keeps both
/// smokes independently pinned).
#[cfg(test)]
mod init_setuid_fixture;

/// fd-ops slice (Wave 4, Part 8): sibling fixture for the
/// `openat → write → lseek → read → close → exit_group` byte-pin
/// smoke. See `init_lseek_fixture.rs`'s module header for the
/// sibling-vs-extend rationale (mirrors the setuid sibling
/// decision so each fd-ops/DAC/fork test owns its own pinned ABI).
#[cfg(test)]
mod init_lseek_fixture;

#[cfg(test)]
mod tests;
