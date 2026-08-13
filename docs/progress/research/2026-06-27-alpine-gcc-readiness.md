# Research: Alpine GCC startup readiness

**Date:** 2026-06-27

## Question

How far is txKernel from starting Alpine's native RISC-V GCC enough to compile
and run a simple C program inside the guest?

## Findings

- The toolchain-bearing Alpine rootfs exists at
  `target/rootfs/alpine-rv64-gcc-qemu` and contains the expected compiler
  payload: `/usr/bin/gcc`, `/usr/libexec/gcc/.../cc1`, `/usr/bin/as`,
  `/usr/bin/ld`, `crt1.o`, and `libc.so`.
- The checked-in shell witness
  `tools/shell-tests/alpine-gcc-compile-smoke.txt` is correctly shaped for the
  intended milestone: reach Alpine shell, run `gcc --version`, write
  `/tmp/hello.c`, compile it, and run `/tmp/hello`.
- The current blocking point is earlier than GCC execution. Rebuilding the
  Alpine initramfs from the full GCC rootfs produced a 270M cpio archive
  (`552297 blocks`). The subsequent shell-test never reached `/ # ` because
  boot logged:
  `txkernel:qemu-riscv64-virt:initramfs:fail:materialize_anon`,
  `bootstrap-exec:fallback:path-not-found`, then ran the baked `/init`
  fallback (`child`, `parent`) and exited 0.
- The cpio archive itself contains `/bin/tx-bootstrap-busybox`, `/bin/busybox`,
  `/bin/sh`, the musl loader, GCC, cc1, assembler, linker, and crt files. The
  `path-not-found` is therefore a consequence of initramfs unpack failure, not
  a missing bootstrap binary in the image.
- A temporary diagnostic rootfs at
  `target/rootfs/alpine-rv64-gcc-slim-qemu` removed the two large nonessential
  LTO files (`/usr/bin/lto-dump` and GCC `lto1`), reducing rootfs size from
  about 252M to 169M and cpio output to `342917 blocks`. It failed at the same
  `initramfs:fail:materialize_anon` point under the default 1G Alpine QEMU
  profile. This means the first blocker is not a specific LTO binary or GCC
  execution path.
- Code review shows `unpack_into_root_mount` maps a `materialize_anon` failure
  to `UnpackError::FsOp { op: "materialize_anon", errno: ENOMEM }`, but
  `CoreInit::register_initramfs_if_present` only prints the op label. The
  serial sentinel currently hides the `ENOMEM` detail.
- Increasing QEMU RAM manually to 2G is not a usable bypass today. The kernel
  traps before normal boot while reading the initrd at physical `0xffe00000`;
  `cargo xtask fault-decode` classifies it as a load page fault at
  `core::ptr::read_unaligned`. This indicates an additional rv64 boot/direct-map
  coverage issue when QEMU places the initrd above the current mapped window.
- Prepared an ext4-based Alpine GCC root image at
  `target/images/alpine-gcc-root-rv64-qemu.ext4`. The image is 768M, label
  `TXALPINE`, and was built from a staged copy of
  `target/rootfs/alpine-rv64-gcc-qemu` plus the static bootstrap shell installed
  as both `/bin/tx-bootstrap-busybox` and `/bin/sh`. Host-side `debugfs`
  confirms `/usr/bin/gcc`, GCC `cc1`, `/bin/tx-bootstrap-busybox`, and `/bin/sh`
  are regular executable files in the image; `e2fsck -fn` reports clean ext4
  metadata. This prepares the storage artifact but does not yet connect it to
  the Alpine QEMU boot profile.
- Manual QEMU startup checks show the ext4 image must be attached to the board's
  probed MMIO slot: `-device virtio-blk-device,drive=txblk0,bus=virtio-mmio-bus.0`.
  If the bus is omitted, QEMU places the disk on `virtio-mmio-bus.7`; the
  current RV64 kernel probes only `virtio0`, so boot still prints
  `:devices:block:ok` after registering the scratch `ltpdev`, but there is no
  `vda`, no `:block:ext4-superblock:*`, and no `/musl` mount.
- With explicit bus0 binding, the guest prints `:block:ext4-superblock:ok` and
  `:mount:sdcard:ext4:ok`, reaches the Alpine shell, and `ls -l` can see both
  `/musl/usr/bin/gcc` and
  `/musl/usr/libexec/gcc/riscv64-alpine-linux-musl/15.2.0/cc1`.
- The first ext4 exec layer works for a static ELF: executing
  `/musl/bin/tx-bootstrap-busybox` from the mounted ext4 image enters BusyBox
  and returns its `applet not found` path. This proves the current blocker is no
  longer simply "cannot execute from ext4".
- GCC itself still does not start enough to print version output. Both
  `/musl/usr/bin/gcc --version` and an explicit loader invocation,
  `/musl/lib/ld-musl-riscv64.so.1 --library-path /musl/lib:/musl/usr/lib
  /musl/usr/bin/gcc --version`, produced no version output and did not return
  before the bounded expect timeouts. The next likely layer is dynamic ELF
  loading or ext4-backed large-file segment reads, not rootfs assembly.
- Code-path review says this is not currently an `ExecOp`-driven failure.
  The syscall arm in `crates/tx-shims/src/linux_syscall/proc.rs` still drives
  `tx_scripts::process::exec::exec_script` directly and comments that the
  StepOp-shaped `ExecOp` wrapper is an unfinished refactor. The active
  `exec_script` path already has the `/musl` interpreter fallback for
  `PT_INTERP=/lib/ld-musl-riscv64.so.1`, interpreter LOAD registration,
  BSS partial-page seeding, `AT_BASE`, and interpreter-entry handoff. By
  contrast, `ExecOp` lacks the `/musl`/glibc/musl interpreter fallback,
  maps interpreter segments with simplified `READ_WRITE` protection, omits
  the partial-last-page seeding path, and sets `AT_ENTRY` to the interpreter
  entry instead of the main program entry. That makes `ExecOp` a future
  migration hazard, but not the path exercised by the Alpine shell's
  `execve("/musl/usr/bin/gcc", ...)` today.
- Static follow-up review did not find a stronger current-failure path inside
  the active exec handoff. `exec_script` opens the PT_INTERP loader with the
  `/musl` fallback, registers interpreter LOAD segments with per-flag
  protections, seeds BSS-extending partial pages, sets `AT_BASE` to the
  interpreter load bias, keeps `AT_ENTRY` as the main program entry, and starts
  the first user PC at the interpreter entry.
- A suspected `getdents64` directory-fd issue was checked and mostly ruled out
  for the current walker path. `sys_getdents64` still has stale comments that
  describe descendant directory rnodes as missing `containing_mount_weak()`,
  but the active resolution code creates directory children with
  `RNodeBacking::Directory` plus `new_cap_in_mount`. Ext4's
  `materialise_rnode` still maps non-directory, non-symlink inodes to
  `PageBacked`, but directories are handled before that backend hook is called.
- The common dynamic-loader syscall surface is present in the dispatcher:
  `openat`, `read`, `pread64`, `mmap`, `mprotect`, `munmap`, `brk`, `fstat`,
  `newfstatat`, `statx`, `readlinkat`, `getrandom`, `prlimit64`, `fcntl`,
  `set_tid_address`, and robust-list calls all have arms. There is no visible
  `rseq` constant or dispatch arm, but musl normally treats `rseq` `ENOSYS` as
  an optional-kernel-feature miss, so this is not by itself a convincing
  explanation for a no-output timeout.
- `mmap` is not an obvious first blocker for loader file mappings: it accepts
  the common historical/no-op flags such as `MAP_DENYWRITE`,
  `MAP_EXECUTABLE`, `MAP_NORESERVE`, `MAP_POPULATE`, and `MAP_STACK`, and
  maps file-backed fds through the file's `PageContainer`. `MAP_GROWSDOWN` is
  rejected, but the loader's file mappings should not require it.
- `brk` remains a second-tier performance or allocator-pressure suspect. It
  intentionally caps linear heap growth at 64 pages and returns the unchanged
  break to force musl onto mmap fallback. That is Linux-compatible failure
  shape, but it can amplify recipe count and mmap pressure once GCC starts
  allocating; it does not by itself explain failing to print `--version`.
- Static code review after the ext4 startup check still says the active
  `execve` path is `tx_scripts::process::exec::exec_script`, not the unfinished
  syscall-layer `ExecOp` wrapper. The current `exec_script` path has the pieces
  that would be fatal if missing for Alpine musl: `/musl` interpreter fallback,
  interpreter LOAD mapping, partial BSS page seeding, `AT_BASE`, main
  executable `AT_ENTRY`, and interpreter-entry PC handoff. That keeps `ExecOp`
  as a future migration hazard rather than the current GCC timeout suspect.
- Several early-loader paths were read down as lower-priority suspects. Ext4
  EOF/tail zeroing, PageBacked targeted reads, private file `mmap`/CoW,
  `MAP_FIXED`, `mprotect`, initial PC/SP setup, syscall return writeback,
  `getrandom`, `readlinkat`, `pread64`, and the current syscall-layer `ppoll`
  implementation did not show an obvious no-output hang path. `wait4` blocks
  on child exit as expected, so a wait timeout would more likely mean a child
  process or pipe path is stuck downstream rather than wait4 itself spinning.
- The fork/pipe/CLOEXEC path is better covered than initially suspected.
  `fcntl(F_SETFD)`, `F_DUPFD_CLOEXEC`, `dup3(O_CLOEXEC)`, `pipe2(O_CLOEXEC)`,
  and `exec_script`'s close-on-exec sweep are wired. Pipe endpoint counts are
  maintained by fd-table accounting, with tests covering dup, fork inheritance,
  close, child exit drain, and EOF publication. This makes a plain GCC driver
  pipe EOF leak less likely, though not impossible without a guest syscall
  timeline.
- Confirmed ABI gap: `newfstatat` and `statx` accept
  `AT_SYMLINK_NOFOLLOW` but still route through `StatOp`/`StatxOp`, whose
  walker follows the terminal symlink. The VFS already has `LstatOp` using
  `FinalSymlinkPolicy::NoFollow`, so the missing piece is syscall-layer
  selection. This can misreport symlink library probes such as
  `/musl/usr/lib/libc.so -> ../../lib/ld-musl-riscv64.so.1`.
- Confirmed related ABI gap: `openat` has no `O_NOFOLLOW` handling. The simple
  open path drives `OpenOp -> step_open`, and `step_open` uses final-symlink
  follow semantics. Userspace that expects terminal symlink rejection with
  `ELOOP` instead receives an opened target. This is in the same loader/path
  probing class as the `AT_SYMLINK_NOFOLLOW` stat gap.
- Confirmed fd-accounting bug, but probably not the first GCC `--version`
  blocker: `pidfd_getfd` installs a target process `OpenFile` into the caller
  without calling the same pipe/socketpair endpoint refcount increment used by
  `dup`, `F_DUPFD`, and fork. A duplicated pipe or socketpair fd through
  pidfd_getfd can therefore publish EOF/EPIPE too early. GCC's normal
  `--version` path is unlikely to depend on `pidfd_getfd`, so this is a real
  correctness bug rather than the leading startup hypothesis.
- Follow-up fix pass implemented the three confirmed syscall/VFS bugs above:
  `newfstatat` now selects the nofollow stat path for
  `AT_SYMLINK_NOFOLLOW`, `statx` has a matching nofollow statx path,
  `openat(O_NOFOLLOW)` rejects a terminal symlink with `ELOOP`, and
  `pidfd_getfd` installs duplicated fds through process fd-table accounting so
  pipe/socketpair endpoint counts are incremented before publication.
- The `CLONE_VFORK` path did not show the suspected permanent parent sleep:
  `sys_clone` parks the parent until the child reports `vfork_done`, and
  `sys_execve` calls `ctx.process.notify_vfork_done()` on successful exec and
  on the identity no-op helper path; process exit paths also notify. The
  current gap is coverage: existing tests only assert that vfork takes the async
  path, not that parent wake happens after child exec.
- Auxv remains an ABI-completeness gap. The board publishes an arch platform
  string (`riscv64`) through `AuxvIf`, but `exec_script` currently emits neither
  `AT_PLATFORM` nor `AT_EXECFN`. Musl can generally tolerate their absence, so
  this is lower confidence than the symlink/no-follow mismatches, but it is
  still part of the dynamic-loader compatibility surface.
- Follow-up compile-fix pass cleared the dirty-tree build blockers that were
  hiding the new syscall/VFS coverage. `tx-shims` test compilation needed the
  Linux constant facade exports (`ITIMER_REAL`, `NETLINK_XFRM`, `CLONE_NEWNS`),
  explicit `ShimsTestPmap` type parameters for direct trap helper calls, and a
  netns mount API test root helper. Workspace all-target compilation then
  exposed two test-build hygiene issues: `tx-observe::testing` pulled `std`
  into no_std kernel board bins through dev-feature unification, and board
  panic handlers were compiled under the Rust test harness. `tx-observe`'s
  testing helper now stays no_std by using `alloc::Vec` plus an atomic test
  lock, and board panic handlers are gated out under `cfg(test)`. The
  `tx-substrate` v3 algebra test was also synchronized with the current 48
  variant `Errno` catalog.

## Distance

Current distance to "GCC starts" is now split by boot mode:

- Large initramfs boot is still blocked before shell by eager cpio-to-tmpfs
  materialisation (`materialize_anon` / hidden `ENOMEM`) and the separate 2G
  initrd direct-map issue.
- The ext4 path has cleared storage discovery and `/musl` mount when QEMU
  attaches the disk to `virtio-mmio-bus.0`, but GCC still hangs before printing
  `--version`.

The remaining work is:

1. Stop unpacking large Alpine images by eagerly copying the whole initramfs
   into tmpfs pages, or otherwise provide a boot rootfs shape that does not
   duplicate a 170M-270M cpio into anonymous memory during boot.
2. If larger guest RAM is the chosen short-term bypass, fix the rv64 direct-map
   or initrd placement path so initrd bytes above the first 1G RAM window are
   safely accessible before unpack.
3. Improve the initramfs failure sentinel to print the errno, at least
   `materialize_anon:ENOMEM`, so future runs distinguish memory exhaustion from
   unsupported tmpfs/page-backed operations.
4. Wire the prepared ext4 image into the Alpine boot/test path. Existing
   `xtask image ext4` and QEMU drive attachment are busybox-only today; Alpine
   still needs first-class `--root-drive`/profile support or equivalent manual
   command construction. RV64 must bind the drive to `virtio-mmio-bus.0` unless
   kernel block discovery is generalized beyond `virtio0`.
5. For a static-only next pass, focus on user-visible ABI mismatches after the
   loader entry point rather than `ExecOp`: fd-relative path handling under
   `/musl`, missing optional syscalls such as `rseq`, allocator pressure from
   the 64-page `brk` soft limit, and file-backed fault paths for large ext4
   files such as GCC `cc1`. The confirmed no-follow stat/open and pidfd fd
   accounting bugs from this audit are fixed and covered by host tests. Once
   `gcc --version` returns, rerun
   `tools/shell-tests/alpine-gcc-compile-smoke.txt` and then chase GCC-visible
   gaps such as `execve` of `cc1`, temporary-file I/O, `fork`/`wait`, `pipe`,
   `mmap`, `brk`, `stat`, and linker behavior.
6. Keep `ExecOp` out of the suspected current-failure set unless the syscall
   dispatcher is changed to drive it. If that refactor resumes, first port the
   active `exec_script` dynamic-ELF details listed above and add an Alpine
   dynamic-ELF shell witness before enabling it for production `execve`.

## Verification

- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-gcc-qemu timeout 240s cargo xtask image cpio --profile alpine --target rv64-qemu`
  passed and produced a 270M `target/images/alpine-initramfs-rv64-qemu.cpio`
  containing GCC payloads.
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-gcc-qemu timeout 240s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-gcc-compile-smoke.txt`
  failed waiting for `/ # ` after `initramfs:fail:materialize_anon` and
  fallback `/init` exit.
- Diagnostic slim rootfs construction removed only `lto-dump` and `lto1`;
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-gcc-slim-qemu timeout 240s cargo xtask image cpio --profile alpine --target rv64-qemu`
  passed and produced `342917 blocks`.
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-gcc-slim-qemu timeout 240s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-gcc-compile-smoke.txt`
  reproduced the same `initramfs:fail:materialize_anon` / fallback `/init`
  behavior.
- Manual 2G QEMU run with the slim initramfs trapped before normal boot;
  `cargo xtask fault-decode --target rv64-qemu --scause 0xd --sepc 0xffffffff806a7008 --stval 0xffe00000 --brief`
  reported a load page fault at `core::ptr::read_unaligned`.
- Ext4 artifact preparation:
  `target/images/alpine-gcc-root-rv64-qemu.ext4` was created with
  `/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4 -F -L TXALPINE -d ...`
  after staging the GCC rootfs and bootstrap shell. Verification used
  `debugfs -R 'stat /usr/bin/gcc'`, `debugfs -R 'stat /usr/libexec/gcc/riscv64-alpine-linux-musl/15.2.0/cc1'`,
  `debugfs -R 'stat /bin/tx-bootstrap-busybox'`, `debugfs -R 'stat /bin/sh'`,
  and `/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/e2fsck -fn target/images/alpine-gcc-root-rv64-qemu.ext4`.
- Compile-fix verification passed:
  `cargo test -p tx-shims dispatch_newfstatat_at_symlink_nofollow_stats_link -- --test-threads=1`,
  `cargo test -p tx-shims dispatch_statx_at_symlink_nofollow_stats_link -- --test-threads=1`,
  `cargo test -p tx-shims dispatch_openat_o_nofollow_on_final_symlink_returns_neg_eloop -- --test-threads=1`,
  `cargo test -p tx-shims dispatch_pidfd_getfd_pipe_writer_keeps_pipe_alive_until_dup_closes -- --test-threads=1`,
  `cargo test -p tx-shims --no-run`,
  `cargo test -p tx-observe -- --test-threads=1`,
  `cargo test -p tx-scripts --test drive_observe -- --test-threads=1`,
  targeted `rustfmt --edition 2021 --check`,
  `git diff --check --` over the touched files,
  `cargo xtask progress validate`, and
  `cargo check --workspace --all-targets`.
- Non-compile caveat: a default
  `cargo test -p tx-subsystems unshared_private_installs_allocate_only_leaf_nodes -- --test-threads=1`
  run compiles but fails at runtime because the assertion expects
  `tx_vm_private_page_metrics` counters while the default build has that cfg
  disabled (`cfg!(tx_vm_private_page_metrics)` returns false in the counting
  path).
  `e2fsck` completed all five passes and reported
  `TXALPINE: 2305/49152 files ... 77588/196608 blocks`.
- Manual startup check without a bus binding:
  `timeout 210s expect ... qemu-system-riscv64 ... -device virtio-blk-device,drive=txblk0 ...`
  reached the Alpine shell and printed `:devices:block:ok`, but timed out
  waiting for `:block:ext4-superblock:ok`. QEMU monitor `info qtree` showed the
  disk attached under `virtio-mmio-bus.7`, explaining why the kernel's
  `virtio0` probe did not register `vda`.
- Manual startup check with bus0 binding:
  `timeout 180s expect ... qemu-system-riscv64 ... -device virtio-blk-device,drive=txblk0,bus=virtio-mmio-bus.0 ...`
  observed `:block:ext4-superblock:ok`, `:mount:sdcard:ext4:ok`, the Alpine
  shell prompt, and successful listing of `/musl/usr/bin/gcc` plus GCC `cc1`.
  `/musl/usr/bin/gcc --version` then timed out without version output.
- Layered exec check with bus0 binding:
  `/musl/bin/tx-bootstrap-busybox` executed from ext4 and returned BusyBox's
  `applet not found` message, but
  `/musl/lib/ld-musl-riscv64.so.1 --library-path /musl/lib:/musl/usr/lib
  /musl/usr/bin/gcc --version` timed out without returning.
- Static fix verification after the nofollow/pidfd pass:
  `cargo check -p tx-shims --lib` passed. Existing process fd-table regression
  tests `process_payload_dup_pipe_writer_keeps_pipe_alive_until_all_writer_fds_close`
  and `step_fork_accounts_inherited_pipe_writer_fd` passed. Focused
  `tx-shims` tests for the new syscall cases are added, but the full
  `tx-shims` test crate is currently blocked before execution by unrelated
  dirty-tree compile errors in `socket_fdtable.rs`, `netns_syscalls.rs`,
  `tests.rs`, and `futex_dispatch.rs`.

## Sources

- `tools/shell-tests/alpine-gcc-compile-smoke.txt`
- `xtask/src/image.rs`
- `xtask/src/shell_test.rs`
- `crates/tx-kernel/src/init/exec.rs`
- `crates/tx-subsystems/src/initramfs/mod.rs`
- `crates/tx-subsystems/src/page_backed/mod.rs`
- `crates/tx-substrate/src/boot_memory.rs`
- `boards/tx-hal-riscv64-qemu-virt/src/pmap/`
