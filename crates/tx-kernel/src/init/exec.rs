// Bootstrap-exec / userspace-reactor methods of `CoreInit<P>`.
//
// Carved out of `init.rs` (2026-05-08 jumbo split). The methods
// here own:
//
// - busybox / init-fixture initramfs population
// - the cmdline-driven `drive_bootstrap_exec` lane
// - the BSP `run_userspace_reactor_loop` that runs the init
//   thread future to zombification
// - the polled SBI debug-console drain (fallback for the
//   PLIC-IRQ path; see the slice-2 progress note)
// - small console-output helpers used by the above
//
// Methods carry their original signatures + bodies; only the
// enclosing module changed. The single `impl<P: TxPlatform>
// CoreInit<P>` block in this file augments the one in `init.rs`.

use super::*;
use crate::adapter::step_engine::{self as step_engine, page_allocator, StepOutcome};

impl<P: TxPlatform> CoreInit<P> {
    /// Initramfs slice: walk `BootInfo::initrd` if present and
    /// reproduce its file tree inside the rootfs. Warn-and-skip on
    /// any per-entry failure — a corrupt initramfs should not wedge
    /// boot, which would be confusing when the user simply pointed
    /// `-initrd` at the wrong file. The bake-in `/init` fixture
    /// (registered above) keeps the kernel runnable in that case.
    pub(crate) fn register_initramfs_if_present() {
        let boot_info = <P as tx_hal::BootInfoIf>::boot_info();
        let Some(initrd_range) = boot_info.initrd else {
            return;
        };
        if initrd_range.size == 0 {
            return;
        }

        // Resolve the initramfs PhysRange to a kernel direct-map
        // pointer. `boot_memory` reserved the range so nothing else
        // mutates it; the slice is read-only for the duration of
        // the unpack.
        let direct_map_base = <P as tx_hal::PlatformConfig>::DIRECT_MAP_BASE.0;
        let kernel_va = direct_map_base.wrapping_add(initrd_range.start.0);
        // SAFETY: `boot_memory::reserve_initrd` reserves the range
        // before `mount_rootfs_tmpfs` runs; the slice is read-only,
        // covers `initrd_range.size` bytes from a kernel direct-map
        // address, and the page allocator does not hand the range
        // out for any other use during the kernel's lifetime.
        let bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(kernel_va as *const u8, initrd_range.size) };

        let Some(root_mount) = root_mount() else {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":initramfs:skip:no-rootmount\n");
            return;
        };
        match tx_subsystems::initramfs::unpack_into_root_mount(bytes, &root_mount) {
            Ok(stats) => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":initramfs:");
                Self::write_decimal_unsigned(stats.files);
                tx_hal::console_write_str::<P>("-files:");
                Self::write_decimal_unsigned(stats.dirs);
                tx_hal::console_write_str::<P>("-dirs:");
                Self::write_decimal_unsigned(stats.symlinks);
                tx_hal::console_write_str::<P>("-symlinks:");
                Self::write_decimal_unsigned(stats.bytes_total as usize);
                tx_hal::console_write_str::<P>("-bytes:");
                if stats.unsupported > 0 {
                    Self::write_decimal_unsigned(stats.unsupported);
                    tx_hal::console_write_str::<P>("-unsupported:");
                }
                tx_hal::console_write_str::<P>("ok\n");
            }
            Err(error) => {
                // Warn-and-skip: log the failure to the board
                // sentinel and continue with whatever was bake-in.
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":initramfs:fail:");
                let label: &str = match error {
                    tx_subsystems::initramfs::UnpackError::Parse(_) => "parse",
                    tx_subsystems::initramfs::UnpackError::FsOp { op, .. } => op,
                    tx_subsystems::initramfs::UnpackError::UnexpectedAdvance(op) => op,
                };
                tx_hal::console_write_str::<P>(label);
                tx_hal::console_write_str::<P>("\n");
            }
        }
    }

    /// Shell-prompt roadmap Slice 10 (2026-05-08): copy
    /// `busybox_fixture::BUSYBOX_BYTES` into a fresh tmpfs file at
    /// `/bin/sh`. Mirrors `register_init_fixture_into_tmpfs`'s
    /// shape (create_inode → materialise_rnode → page-by-page memcpy
    /// → truncate) but with a different path (`/bin/sh`) and
    /// non-executable -> executable mode bits.
    ///
    /// Compiled in only when `cfg(busybox_baked)` — set by `build.rs`
    /// when `TX_BUSYBOX` is set in the build environment. Host tests
    /// (without TX_BUSYBOX) use the existing `/init` fixture path
    /// exclusively.
    ///
    /// Mode is `0o100755` (S_IFREG | rwx-r-x-r-x). Owner/group are
    /// uid=0/gid=0 (the bootstrap is root); a `chmod +s` step is not
    /// needed — busybox doesn't require setuid.
    ///
    /// Creates the parent `/bin` directory if absent.
    #[cfg(busybox_baked)]
    pub(crate) fn register_busybox_into_tmpfs() {
        use tx_subsystems::vfs::{Credential, RNodeBacking};

        let root_mount =
            root_mount().expect("register_busybox_into_tmpfs: ROOT_MOUNT must be populated");
        let payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap();
        let fs_ops = payload.fs_ops.clone();
        let fs_page_backing = payload.fs_page_backing.clone();
        let root_object_id = root_mount.root().fs_object_id();

        let cred = Credential::root();

        // Boot-time tmpfs ops are synchronous, so Continue/Yield are
        // unreachable and panic if they fire.
        use StepOutcome as V3;

        // 1. Create or look up `/bin` directory. Use `mkdir`; on
        //    EEXIST treat the existing dir as the parent.
        let bin_object_id = {
            let guard = step_engine::guard();
            let outcome = fs_ops.mkdir(root_object_id, b"bin", 0o040755, &cred, &guard);
            match outcome {
                V3::Done((id, _)) => id,
                // EEXIST is unlikely from a clean tmpfs root, but
                // tolerate it: walk to find the existing dir.
                _ => {
                    // Fall back: pretend root_object_id is bin
                    // parent. v1 host paths don't need this branch.
                    panic!("register_busybox_into_tmpfs: mkdir(/bin) failed");
                }
            }
        };

        // 2. Allocate the `/bin/sh` inode.
        let bytes = busybox_fixture::BUSYBOX_BYTES;
        let (file_id, file_meta) = {
            let guard = step_engine::guard();
            let outcome = fs_ops.create_inode(bin_object_id, b"sh", 0o100755, &cred, &guard);
            match outcome {
                V3::Done(out) => out,
                other => panic!("register_busybox_into_tmpfs: create_inode(/bin/sh): {other:?}"),
            }
        };

        // 3. Materialise the inode's RNode and grab its
        //    `Cap<PageContainer>`.
        let pc = {
            let guard = step_engine::guard();
            let outcome = fs_ops.materialise_rnode(file_id, file_meta, &payload, &guard);
            let rnode = match outcome {
                V3::Done(rnode) => rnode,
                other => panic!("register_busybox_into_tmpfs: materialise_rnode: {other:?}"),
            };
            match rnode.backing() {
                RNodeBacking::PageBacked { pc } => pc.clone(),
                other => panic!(
                    "register_busybox_into_tmpfs: tmpfs materialise_rnode \
                     returned non-PageBacked backing: {other:?}"
                ),
            }
        };

        // 4. Populate every page covered by the busybox bytes via
        //    `materialize_anon` + direct-map memcpy. busybox is
        //    typically 500KB-1.5MB; this loop iterates ~250-500
        //    times for 4KB pages.
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE;
        for (idx, chunk) in bytes.chunks(page_size).enumerate() {
            let materialised = pc
                .materialize_anon(
                    tx_subsystems::page_backed::PageIndex::new(idx as u64),
                    tx_subsystems::page_backed::MaterializeAccess::Write,
                )
                .expect("register_busybox_into_tmpfs: materialize_anon");
            let frame_base = page_allocator::frame_kernel_addr(materialised.ppn)
                .expect("register_busybox_into_tmpfs: direct-map view");
            // SAFETY: `materialised.map_pin` keeps the page resident
            // for this scope; destination region covers exactly
            // `chunk.len()` bytes from a freshly materialised anon
            // frame; source and destination do not overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
            }
        }

        // 5. Set the visible size via FsPageBacking::truncate.
        let size = bytes.len() as u64;
        {
            let guard = step_engine::guard();
            match fs_page_backing.truncate(file_id, size, &guard) {
                V3::Done(()) => {}
                other => panic!("register_busybox_into_tmpfs: truncate({size}): {other:?}"),
            }
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":busybox:fixture:ok\n");
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
        use tx_subsystems::vfs::{Credential, RNodeBacking};

        let root_mount =
            root_mount().expect("register_init_fixture_into_tmpfs: ROOT_MOUNT must be populated");
        let payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap();
        let fs_ops = payload.fs_ops.clone();
        let fs_page_backing = payload.fs_page_backing.clone();
        let root_object_id = root_mount.root().fs_object_id();

        let bytes = &init_fixture::INIT_FIXTURE_BYTES[..];

        // Boot-time tmpfs ops are synchronous, so Continue/Yield are
        // unreachable and panic if they fire.
        use StepOutcome as V3;

        // Allocate the inode. Bootstrap process is root by construction.
        let cred = Credential::root();
        let (file_id, file_meta) = {
            let guard = step_engine::guard();
            let outcome = fs_ops.create_inode(root_object_id, b"init", 0o100755, &cred, &guard);
            match outcome {
                V3::Done(out) => out,
                V3::Err(step_engine::Errno::EROFS)
                | V3::Err(step_engine::Errno::ENOSYS)
                | V3::Err(step_engine::Errno::EEXIST) => {
                    // Rootfs is read-only (e.g. ext4 mounted from vda).
                    // The fixture is not needed; the real binary lives on disk.
                    Self::write_board_sentinel_prefix();
                    tx_hal::console_write_str::<P>(":init:fixture:skip:ro\n");
                    return;
                }
                other => panic!("register_init_fixture_into_tmpfs: create_inode(/init): {other:?}"),
            }
        };

        // Materialise the inode's RNode so we can reach the
        // `Cap<PageContainer>`. Tmpfs's Phase-7 override returns
        // `RNodeBacking::PageBacked { pc }` for regular files.
        let pc = {
            let guard = step_engine::guard();
            let outcome = fs_ops.materialise_rnode(file_id, file_meta, &payload, &guard);
            let rnode = match outcome {
                V3::Done(rnode) => rnode,
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
            let frame_base = page_allocator::frame_kernel_addr(materialised.ppn)
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
            let guard = step_engine::guard();
            match fs_page_backing.truncate(file_id, size, &guard) {
                V3::Done(()) => {}
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
    pub(super) fn drive_bootstrap_exec() {
        use tx_subsystems::vfs::Credential;

        let init = tx_subsystems::process::execution::init_process()
            .expect("drive_bootstrap_exec: INIT_PROCESS must be populated");
        let thread = init
            .nth_thread(0)
            .expect("drive_bootstrap_exec: init has a leader thread post-bootstrap");

        // Bootstrap exec runs as init (root) by construction.
        let cred = Credential::root();

        // When the sdcard ext4 mount is present (RV64 QEMU with vda),
        // exec busybox sh to run the oscomp basic-musl test suite.
        // The sdcard's musl/ dir contains busybox (static ET_EXEC),
        // basic_testcode.sh, and per-test binaries under basic/.
        //
        // Use `sh -c "cd /musl/musl && sh basic_testcode.sh"` so the
        // inner sh inherits CWD=/musl/musl/ and basic_testcode.sh's
        // relative `./busybox` / `cd ./basic` references resolve
        // correctly — avoids any kernel-side VFS walk at this stage.
        // DIAGNOSTIC: emit epoch state right before the sdcard exec to
        // identify whether a guard leak pre-dates drive_bootstrap_exec.
        {
            let es = crate::adapter::step_engine::epoch::summary();
            let cpu0 = crate::adapter::step_engine::epoch::cpu_summary(tx_hal::CpuId(0));
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":diag:pre-sdcard-exec:guards=");
            Self::write_decimal_unsigned(es.active_guards);
            tx_hal::console_write_str::<P>(":epoch=");
            Self::write_decimal_unsigned(es.global_epoch as usize);
            tx_hal::console_write_str::<P>(":cpu0-local=");
            Self::write_decimal_unsigned(cpu0.map(|c| c.local_epoch as usize).unwrap_or(999));
            tx_hal::console_write_str::<P>("\n");
        }

        let boot_info = <P as tx_hal::BootInfoIf>::boot_info();
        let sdcard_boot = boot_info.initrd.is_none() && boot_info.cmdline.is_none();

        if super::MUSL_MOUNT.lock().is_some() && sdcard_boot {
            // Per-arch busybox path and test-script chain.
            //
            // la64 sdcard has both glibc/ and musl/ test directories;
            // run both.  rv64 sdcard is musl-only (old code confirmed
            // this: "cd /musl/musl && ./busybox sh basic_testcode.sh").
            //
            // All testcode.sh scripts expect CWD = their own directory
            // and use `./busybox` for echo/cat etc., so we `cd` first.
            let (sdcard_bin, sdcard_cmd): (&[u8], &[u8]) = match P::ARCH {
                tx_hal::Arch::LoongArch64 => (
                    // la64 sdcard has both glibc/ (dynamic) and musl/
                    // (static). Use the musl static busybox; the kernel
                    // does not yet support PT_INTERP (dynamic linker).
                    b"/musl/musl/busybox",
                    b"cd /musl/musl \
                      && ./busybox sh basic_testcode.sh \
                      && ./busybox sh busybox_testcode.sh \
                      && ./busybox sh libctest_testcode.sh \
                      && ./busybox sh libcbench_testcode.sh \
                      && ./busybox sh lua_testcode.sh \
                      && ./busybox sh lmbench_testcode.sh \
                      && ./busybox sh iozone_testcode.sh \
                      && ./busybox sh netperf_testcode.sh \
                      && ./busybox sh iperf_testcode.sh \
                      && ./busybox sh cyclictest_testcode.sh \
                      && ./busybox sh ltp_testcode.sh",
                ),
                tx_hal::Arch::Riscv64 => (
                    b"/musl/musl/busybox",
                    b"cd /musl/musl \
                      && ./busybox sh basic_testcode.sh \
                      && ./busybox sh busybox_testcode.sh \
                      && ./busybox sh libctest_testcode.sh \
                      && ./busybox sh libcbench_testcode.sh \
                      && ./busybox sh lua_testcode.sh \
                      && ./busybox sh lmbench_testcode.sh \
                      && ./busybox sh iozone_testcode.sh \
                      && ./busybox sh netperf_testcode.sh \
                      && ./busybox sh iperf_testcode.sh \
                      && ./busybox sh cyclictest_testcode.sh \
                      && ./busybox sh ltp_testcode.sh",
                ),
            };
            let sdcard_envp: &[&[u8]] = &[b"PATH=/musl/glibc:/musl/musl"];
            let sdcard_argv: &[&[u8]] = &[b"sh", b"-c", sdcard_cmd];
            let outcome = bootstrap_block_on(tx_scripts::process::exec::exec_script::<P>(
                &init,
                &thread,
                sdcard_bin,
                sdcard_argv,
                sdcard_envp,
                &cred,
            ));
            match outcome {
                Ok(()) => {
                    Self::write_board_sentinel_prefix();
                    tx_hal::console_write_str::<P>(":bootstrap-exec:ok\n");
                    return;
                }
                Err(ref e) => {
                    Self::write_board_sentinel_prefix();
                    tx_hal::console_write_str::<P>(":bootstrap-exec:sdcard:fail:");
                    tx_hal::console_write_str::<P>(exec_error_tag(e));
                    tx_hal::console_write_str::<P>("\n");
                    // Fall through to the cmdline-driven path below.
                }
            }
        }

        // busybox sh needs at least PATH to find applet binaries
        // (`ls`, `cat`, etc.) — without it, command lookup short-
        // circuits to "not found" before the kernel's fork/exec path
        // ever runs, and the prompt never returns from the failing
        // command. `/bin` is where our cpio rootfs places every
        // applet symlink.
        let envp: &[&[u8]] = &[b"PATH=/bin"];

        // Cmdline-driven init path (initramfs slice):
        //   `init=/some/path` -> exec that path with argv=[basename]
        //   `tx.profile=busybox` (no init=) -> /bin/sh argv=[sh]
        //   default -> bake-in /init fixture, argv=[init]
        let (init_path, argv0) = parse_init_from_cmdline::<P>();
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":bootstrap-exec:path:");
        tx_hal::console_write_bytes::<P>(init_path);
        tx_hal::console_write_str::<P>("\n");

        // `exec_script` opens its own fresh epoch guards inside V1
        // (`build_aspace_from_image`) and V2
        // (`populate_detached_user_range`); the caller must NOT hold
        // a guard at the call site (per
        // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
        let argv: &[&[u8]] = &[argv0];
        let outcome = bootstrap_block_on(tx_scripts::process::exec::exec_script::<P>(
            &init, &thread, init_path, argv, envp, &cred,
        ));
        match outcome {
            Ok(()) => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:ok\n");
                return;
            }
            Err(ref e) if init_path != b"/init" => {
                // Cmdline picked a non-bake-in path that the kernel
                // can't actually load (e.g. busybox without a working
                // ELF-loader path, or initramfs missing the file).
                // Warn and fall back to the bake-in `/init` fixture
                // so `boot:ok` still fires for the smoke. Production
                // boot would surface the failure to userspace via
                // execve()'s errno path; this is the bootstrap-only
                // pre-userspace fallback.
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:fallback:");
                tx_hal::console_write_str::<P>(exec_error_tag(e));
                tx_hal::console_write_str::<P>("\n");
            }
            Err(e) => {
                // Open Q #3 (DECIDED 2026-05-06): bake-in `/init`
                // failure is a boot invariant violation; panic loudly
                // with the `:bootstrap-exec:fail` board sentinel.
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:fail:");
                tx_hal::console_write_str::<P>(exec_error_tag(&e));
                tx_hal::console_write_str::<P>("\n");
                panic!("bootstrap exec for /init failed: {e:?}");
            }
        }

        // Fallback path: re-drive against the bake-in `/init` fixture.
        let argv: &[&[u8]] = &[b"init" as &[u8]];
        let outcome = bootstrap_block_on(tx_scripts::process::exec::exec_script::<P>(
            &init, &thread, b"/init", argv, envp, &cred,
        ));
        match outcome {
            Ok(()) => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:ok\n");
            }
            Err(e) => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":bootstrap-exec:fail:");
                tx_hal::console_write_str::<P>(exec_error_tag(&e));
                tx_hal::console_write_str::<P>("\n");
                panic!("bootstrap exec for /init fallback failed: {e:?}");
            }
        }
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

    /// Install the timer-sleep seam so `sys_nanosleep` / `sys_clock_nanosleep`
    /// in `tx-shims` can park the calling task until a real deadline fires
    /// in the BSP reactor's timer queue. Must be called before the reactor
    /// task loop starts (BOOT_REACTOR lock must not be held at this call site).
    pub(crate) fn install_sleep_seam() {
        // Clone the TimerQueue Arc while outside the reactor task loop.
        // `sleep_until_ns` will later call `tq.wait_until()` from within
        // a reactor task, acquiring only the TimerQueue's own SpinLock —
        // not the BOOT_REACTOR lock — avoiding re-entrancy deadlock.
        if let Some(tq) = BOOT_REACTOR.with(|reactor| reactor.timer_queue()) {
            tx_subsystems::timer_sleep::install_timer_queue(tq);
        }
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
    /// Polled SBI debug-console drain — see the call site comment
    /// in `run_userspace_reactor_loop`. Drains any bytes the
    /// firmware has buffered on the host stdio side and feeds them
    /// to the boot console TTY via `step_ingest`, which fires the
    /// TTY's wait carrier so a blocked `read` on fd 0 wakes up.
    ///
    /// Cheap when no bytes are pending (`read_bytes` returns 0,
    /// the rest is skipped).
    pub(crate) fn drain_pending_uart_rx_into_tty() -> usize {
        crate::irq::drain_uart_rx_pending()
    }

    pub(super) fn drain_sbi_console_into_tty() -> usize {
        // 2026-05-13: bumped from 64 to 512 bytes to swallow whole shell
        // command lines in a single SBI poll. The 64-byte cap left the
        // 17-byte tail of an 81-character `ln -s` line stranded in the
        // UART FIFO until the PLIC RX IRQ fired; the IRQ-deferred drain
        // path then re-entered `step_ingest` and the `wait_channel.fire`
        // it issued did not propagate to the parked `sys_read` task
        // (root cause still under investigation — see the
        // `tools/shell-tests/busybox-extended.txt` links/chmod-stat
        // groups). Bumping the SBI buffer ensures most realistic shell
        // input fits in one `step_ingest` call so the proven SBI-direct
        // path handles it. The IRQ path stays in place so a quiescent
        // WFI still wakes promptly when bytes arrive.
        let mut buf = [0u8; 512];
        let n = <P as tx_hal::ConsoleIf>::read_bytes(&mut buf);
        if n == 0 {
            return 0;
        }
        let Some(tty) = console_tty() else { return 0 };
        let guard = step_engine::guard();
        let _ = tx_subsystems::tty::execution::step_ingest(&tty, &buf[..n], &guard);
        n
    }

    pub(super) fn run_userspace_reactor_loop() {
        // Wave 1 of the fork/clone/wait4 slice (2026-05-06): install
        // the reactor-submission seam so a future `sys_clone` arm
        // (Wave 2) can route freshly-forked child threads through
        // the BSP boot reactor without taking a circular crate
        // dependency back into `tx-kernel`. The seam lives in
        // `tx_subsystems::reactor_submit` (a function-pointer slot);
        // the platform parameter `P` is captured at install time
        // here so the seam stays parameter-free at the call site.
        Self::install_reactor_submit_seam();
        Self::install_sleep_seam();

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

        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        let wrapper_payload = payload.clone();
        let future_payload = payload.clone();
        let submit_thread = thread.clone();
        // OBS-V1 §13.2: install init's TID on the task's mailbox so
        // observation records for the leader thread carry a non-zero
        // `task_id_low` (read by `notify_emit` and `PayloadDriveBegin`).
        let tid_low = thread.tid.0;
        let submitted = BOOT_REACTOR.with(|reactor| {
            reactor.submit_task_with_meta(
                crate::thread_future::PerHartSlotted::<P, _>::new(
                    wrapper_payload,
                    crate::thread_future::run_thread::<P>(submit_thread, future_payload),
                ),
                boot_runtime::InitialSchedMeta::kernel()
                    .with_affinity(tx_hal::CpuMask::single(current_cpu).bits())
                    .with_task_id(tid_low),
            )
        });
        if submitted.is_none() {
            // Boot reactor not initialised; nothing to drive.
            return;
        }
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":userspace:submitted\n");

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

            // UART IRQ handlers cannot touch TTY state directly
            // because epoch guards are forbidden in IRQ context.
            // They queue bytes in an IRQ-safe buffer and request a
            // reactor wake; consume that buffer here in normal
            // context before deciding whether there is runnable work.
            if Self::drain_pending_uart_rx_into_tty() != 0 {
                continue;
            }
            if Self::drain_sbi_console_into_tty() != 0 {
                continue;
            }

            // Drain any pending child-thread submits posted from
            // sys_clone *before* polling the reactor again. This is
            // the per-loop visibility seam — submits enqueued during
            // the previous poll iteration become visible to the
            // reactor here, outside the inner lock that sys_clone
            // ran under.
            Self::drain_pending_child_submits();

            let step = match Self::step_boot_reactor_once(current_cpu) {
                Some(step) => step,
                None => break,
            };

            // EBR drain. Caps retired during the task polls above
            // (e.g. `Cap<OpenFile>` from `sys_close` / process exit fd
            // table teardown, `Cap<ProcessPayload>` from
            // `step_exit_group`) sit in the per-CPU retired list until
            // an epoch advance lets them be reclaimed. Without this
            // call, the retired list only auto-drains at
            // `RETIRE_THRESHOLD = 64` items — too high for short
            // pipelines, so `Drop for OpenFile`'s
            // `decr_reader`/`decr_writer` (which signal pipe EOF/EPIPE
            // by notifying the peer wait-source) never fires and
            // blocked readers/writers hang.
            //
            // A bounded budget per iteration keeps per-iteration
            // latency predictable. Two-level deferral chains
            // (ProcessPayload → fd table → OpenFile) need ~4 drain
            // rounds to fully propagate; iteration cadence (driven by
            // task polls + 5 ms timer ticks) finishes that in well
            // under a millisecond.
            let drain_stats = step_engine::drain_with_budget(64);

            // Don't enter WFI if EBR reclaimed anything (reclaim callbacks
            // may have called wake_by_ref() on parked tasks, which is
            // invisible to step.should_idle() computed before the drain) or
            // if there are items still pending reclamation (need more epoch
            // advances before they can be reclaimed).
            let ebr_active = drain_stats.reclaimed > 0 || drain_stats.remaining > 0;
            if step.should_idle() && !ebr_active && !init.is_zombie() {
                // When the reactor has no pending deadline, the platform
                // timer was cancelled by `program_hart_loop_deadline`.
                // Re-arm it here so WFI wakes periodically — the drain
                // calls below need to fire on every tick. This must only
                // happen in the userspace reactor loop (not in boot smoke
                // tests) because the timer interrupt fires
                // `try_bounded_maintenance_tick`, which acquires zone
                // locks that boot-time code may already hold.
                if matches!(
                    step.deadline_action,
                    boot_runtime::hart_loop::HartLoopDeadlineAction::Cancel
                ) {
                    P::set_deadline_ns(
                        P::read_ns().saturating_add(crate::init::IDLE_TIMER_PERIOD_NS),
                    );
                }
                P::wait_for_interrupt_once();
                if P::pending_ipi(IpiKind::Reschedule) {
                    P::ack_ipi(IpiKind::Reschedule);
                }
                // Drain UART RX bytes buffered by `uart_rx_irq_handler`
                // during the preceding WFI sleep. The IRQ handler cannot
                // call `epoch::guard()` (irq_depth > 0), so it stores raw
                // bytes in `UART_RX_PENDING`; `drain_uart_rx_pending` runs
                // here with irq_depth == 0 and feeds them to `step_ingest`.
                let _ = crate::irq::drain_uart_rx_pending();
                // Also poll the SBI debug-console as a fallback for
                // platforms where the UART IRQ is claimed by firmware
                // (e.g. OpenSBI M-mode UART handling). Harmless when the
                // PLIC path is active since `read_bytes` returns 0 once the
                // FIFO has already been drained by the IRQ handler.
                Self::drain_sbi_console_into_tty();
                // Flush deferred EBR drops so pipe write-end close
                // propagates to blocked readers. OpenFile::drop() (which
                // calls decr_writer → EOF signal) fires only when EBR
                // reclaims the slot; without an explicit drain here the
                // reactor never calls drain_with_budget unless the retired
                // queue hits RETIRE_THRESHOLD=64, which a simple pipeline
                // never reaches. This ensures EOF propagates within a
                // few timer ticks (~20 ms) after the last writer closes.
                let _ = step_engine::drain_with_budget(usize::MAX);
            }
        }

        // init zombified — dump the observation ring over the console
        // before emitting the exit sentinel so `cargo xtask observe
        // extract --serial <log>` can recover the full `.txtrace` blob
        // from the captured serial output. Boards without an
        // `observation_ring` impl (returning `None`) yield a no-op dump.
        tx_observe::dump_console_hex::<P>(<P as tx_hal::SmpIf>::current_cpu_id());

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
    pub(super) fn write_signed_decimal(value: i32) {
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

    pub(super) fn write_board_sentinel_prefix() {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
    }
}
