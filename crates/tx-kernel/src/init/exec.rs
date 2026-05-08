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

impl<P: TxPlatform> CoreInit<P> {

    /// Initramfs slice: walk `BootInfo::initrd` if present and
    /// reproduce its file tree inside the rootfs. Warn-and-skip on
    /// any per-entry failure — a corrupt initramfs should not wedge
    /// boot, which would be confusing when the user simply pointed
    /// `-initrd` at the wrong file. The bake-in `/init` fixture
    /// (registered above) keeps the kernel runnable in that case.
    pub(crate) fn register_initramfs_if_present() {
        let boot_info = <P as tx_hal::BootInfoIf>::boot_info();
        let Some(initrd_range) = boot_info.initrd else { return };
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
        use tx_subsystems::execution::StepOutcome;
        use tx_subsystems::vfs::{Credential, RNodeBacking};

        let root_mount = root_mount()
            .expect("register_busybox_into_tmpfs: ROOT_MOUNT must be populated");
        let fs_ops = root_mount.payload_cap().expect("rootfs payload alive during boot").into_cap().fs_ops.clone();
        let fs_page_backing = root_mount.payload_cap().expect("rootfs payload alive during boot").into_cap().fs_page_backing.clone();
        let root_object_id = root_mount.root().fs_object_id();

        let cred = Credential::root();

        // 1. Create or look up `/bin` directory. Use `mkdir`; on
        //    EEXIST treat the existing dir as the parent.
        let bin_object_id = {
            let guard = tx_substrate::epoch::guard();
            let outcome = fs_ops.mkdir(root_object_id, b"bin", 0o040755, &cred, &guard);
            match outcome {
                StepOutcome::Done((id, _)) | StepOutcome::Advanced((id, _)) => id,
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
            let guard = tx_substrate::epoch::guard();
            let outcome = fs_ops.create_inode(bin_object_id, b"sh", 0o100755, &cred, &guard);
            match outcome {
                StepOutcome::Done(out) => out,
                other => panic!(
                    "register_busybox_into_tmpfs: create_inode(/bin/sh): {other:?}"
                ),
            }
        };

        // 3. Materialise the inode's RNode and grab its
        //    `Cap<PageContainer>`.
        let pc = {
            let guard = tx_substrate::epoch::guard();
            let outcome = fs_ops.materialise_rnode(file_id, file_meta, &guard);
            let rnode = match outcome {
                StepOutcome::Done(rnode) => rnode,
                other => panic!(
                    "register_busybox_into_tmpfs: materialise_rnode: {other:?}"
                ),
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
            let frame_base = tx_substrate::page_allocator::frame_kernel_addr(materialised.ppn)
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
            let guard = tx_substrate::epoch::guard();
            match fs_page_backing.truncate(file_id, size, &guard) {
                StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
                other => panic!(
                    "register_busybox_into_tmpfs: truncate({size}): {other:?}"
                ),
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
        use tx_subsystems::execution::StepOutcome;
        use tx_subsystems::vfs::{Credential, RNodeBacking};

        let root_mount =
            root_mount().expect("register_init_fixture_into_tmpfs: ROOT_MOUNT must be populated");
        let fs_ops = root_mount.payload_cap().expect("rootfs payload alive during boot").into_cap().fs_ops.clone();
        let fs_page_backing = root_mount.payload_cap().expect("rootfs payload alive during boot").into_cap().fs_page_backing.clone();
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
    pub(super) fn drive_bootstrap_exec() {
        use tx_subsystems::vfs::Credential;

        let init = tx_subsystems::process::execution::init_process()
            .expect("drive_bootstrap_exec: INIT_PROCESS must be populated");
        let thread = init
            .nth_thread(0)
            .expect("drive_bootstrap_exec: init has a leader thread post-bootstrap");

        // Bootstrap exec runs as init (root) by construction.
        let cred = Credential::root();
        let envp: &[&[u8]] = &[];

        // Cmdline-driven init path (initramfs slice):
        //   `init=/some/path` -> exec that path with argv=[basename]
        //   `tx.profile=busybox` (no init=) -> /bin/sh argv=[sh]
        //   default -> bake-in /init fixture, argv=[init]
        let (init_path, argv0) = parse_init_from_cmdline::<P>();

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
    pub(super) fn drain_sbi_console_into_tty() {
        let mut buf = [0u8; 64];
        let n = <P as tx_hal::ConsoleIf>::read_bytes(&mut buf);
        if n == 0 {
            return;
        }
        let Some(tty) = console_tty() else { return };
        let guard = tx_substrate::epoch::guard();
        let _ = tx_subsystems::tty::execution::step_ingest(&tty, &buf[..n], &guard);
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
                // Drain SBI debug-console bytes into the boot
                // console TTY on every wake. The QEMU virt UART
                // path *should* deliver bytes via PLIC IRQ 10
                // (`uart_rx_irq_handler`), but in our current
                // configuration that doesn't fire — likely OpenSBI
                // claims the UART in M-mode for its debug-console
                // extension. As a fallback the BSP polls SBI on
                // each idle wake (timer ticks fire ~every 5 ms in
                // smoke; once we get IRQ-driven RX working this
                // becomes redundant, but it's harmless when
                // there are no buffered bytes).
                Self::drain_sbi_console_into_tty();
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
