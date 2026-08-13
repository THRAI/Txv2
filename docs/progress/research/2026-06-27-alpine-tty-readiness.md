# Research: Alpine TTY readiness

**Date:** 2026-06-27

## Question

How far is txKernel from a usable Alpine TTY path, especially for the current
interactive shell and BusyBox `vi` workflows?

## 2026-06-27 controlling-TTY closure update

The controlling-terminal slice described by the earlier audit is now
implemented and guest-verified. The current Alpine shell no longer prints
`can't access tty; job control turned off`; BusyBox `tty` returns `/dev/ttyS0`
with status 0; `/dev/ttyS0` and `/dev/console` both report device number
`4:64`; caller-relative `/dev/tty` reports `5:0`; and
`/proc/self/fd/0` readlink/stat now follows the caller fd table to the same
TTY device. The focused guest witness was:

```text
TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 180s cargo xtask shell-test --target rv64-qemu --profile alpine --script /tmp/tx-alpine-tty-complete-probe-v2.shelltest
```

Important guest lines from that run:

```text
crw--w----    1 root     root        4,  64 ... /dev/console
crw-------    1 root     root        5,   0 ... /dev/tty
crw--w----    1 root     root        4,  64 ... /dev/ttyS0
readlink /proc/self/fd/0 -> /dev/ttyS0
stat /proc/self/fd/0 -> 4:40 character special file
tty -> /dev/ttyS0
T74Z_TTY_RC_0
```

The code changes that closed it are:

- boot registers hardware TTY identity `ttyS0` and keeps `console` as a devfs
  alias to the same `TtyIdentity`;
- devfs alias object ids now map back to the TTY device number for path
  `stat`/`statx`;
- `openat` does best-effort controlling-tty acquisition for eligible session
  leaders unless `O_NOCTTY`/`O_PATH` applies;
- `/dev/tty` is a syscall-layer caller-relative special case rather than a
  global devfs node;
- TTY read/write syscalls route through process-aware job-control wrappers;
- `/proc/self/fd/N` readlink/stat/statx resolves through the caller fd table
  so libc/BusyBox ttyname probes can match fd metadata to `/dev/ttyS0`.

The older findings below remain useful as the investigation trail. The next
section records the follow-up closure for raw/non-canonical `VMIN/VTIME`,
VINTR foreground signal delivery, and externally seeded winsize.

## 2026-06-27 raw timing, VINTR, and winsize closure update

The three remaining Alpine TTY follow-ups are now implemented and
guest-verified.

- Raw/non-canonical reads now use a TTY-owned wait plan:
  `tty_read_wait_plan` decides whether a read is immediately complete, must wait
  indefinitely, or should wait for `VTIME` deciseconds. `step_read` now honors
  `VMIN` when `VTIME != 0`, and the syscall TTY read path waits for either the
  TTY readable token or the computed deadline before returning a short read.
  TTY poll/select readiness uses the same completion predicate instead of just
  checking the raw readable bit.
- The checked-in BusyBox `vi` edit smoke keeps the single batched
  `send "iguest-vi-edit-ok\n\e:wq\n"` directive, but shell-test now treats
  ESC/control bytes inside a `send` as interactive key boundaries instead of
  one paste burst. That preserves ordinary text batching while giving BusyBox
  vi enough time to resolve ESC before `:wq`; the Alpine/QEMU witness exits vi
  and verifies the written file.
- VINTR now has both host and guest proof. The console RX path calls
  `ingest_console_tty_bytes`, drives `step_ingest`, drops the epoch guard, and
  delivers the deferred TTY signal through
  `deliver_signal_dispatch_for_process`. On the syscall side, `setpgid` now
  supports the shell subset where a parent moves a direct child into a fresh
  child-led pgrp, and `TIOCSPGRP` resolves the typed pgrp cap before rebinding
  the TTY foreground group. The Alpine guest witness runs
  `sleep 30; echo VINTR_SHOULD_NOT_PRINT`, sends `^C`, returns to `/ #`, and
  then prints `VINTR_AFTER`.
- Initial hardware winsize is now externally seeded. xtask appends
  `tx.tty.rows=N tx.tty.cols=M` to QEMU cmdlines, preferring
  `TX_TTY_ROWS`/`TX_TTY_COLS`, then host `stty size < /dev/tty`, then the
  fallback `24 80`. Kernel boot parses those cmdline tokens and registers the
  boot UART TTY with that winsize. This is intentionally a host/QEMU-launch
  seed, not a live QEMU readback path: the current SBI serial byte-stream still
  has no guest-visible geometry channel.

Focused verification passed:

```text
cargo test -p tx-subsystems noncanonical_vmin_with_vtime_blocks_until_threshold_is_met -- --test-threads=1
cargo test -p tx-kernel dispatch_irq_vintr_delivers_sigint_to_foreground_pgrp -- --test-threads=1
cargo test -p tx-kernel tty_winsize_cmdline_requires_positive_rows_and_cols -- --test-threads=1
cargo check -p tx-kernel --lib
TX_TTY_ROWS=33 TX_TTY_COLS=101 TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 180s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-vi-edit-smoke.txt
TX_TTY_ROWS=33 TX_TTY_COLS=101 TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 180s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-tty-vintr-focused-probe.txt
TX_TTY_ROWS=33 TX_TTY_COLS=101 TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 180s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-tty-winsize-seeded-smoke.txt
```

`cargo -q xtask unit` remains blocked by unrelated dirty-tree failures outside
this TTY slice: `tx-shims` lib-test compile errors (`E0283`, `E0425`, `E0432`)
and `tx-ext4` test
`tests_v3::ext4_v3_truncate_surface_enosys_but_fsync_is_accepted` expecting
`Err(ENOSYS)` while the current implementation returns `Done(())`.

Guest evidence from the winsize witness:

```text
cat /proc/cmdline
tx.profile=alpine init=/bin/tx-bootstrap-busybox console=ttyS0 tx.tty.rows=33 tx.tty.cols=101
stty size
33 101
```

Guest evidence from the VINTR witness:

```text
sleep 30; echo VINTR_SHOULD_NOT_PRINT
^C
/ #
echo VINTR_AFTER
VINTR_AFTER
```

Guest evidence from the batched BusyBox `vi` witness:

```text
send "iguest-vi-edit-ok\n\e:wq\n"
'/tmp/vi-edit.txt' 2L, 18C
cat /tmp/vi-edit.txt; echo vi-edit-status-$?
guest-vi-edit-ok
vi-edit-status-0
```

## Findings

- The basic Alpine console path is working. The guest boots with
  `console=ttyS0`, registers the console alias, reaches the Alpine bootstrap
  shell, and reads/writes through `/dev/console`.
- The prior winsize blocker is fixed for hardware TTY payloads:
  `stty size` reports `24 80`, and the `vi -c q` reopen smoke passes twice in
  one session.
- That `stty size` value is not read from QEMU today. The syscall chain is
  `ioctl(TIOCGWINSZ)` -> `sys_ioctl` -> `step_ioctl_tiocgwinsz` ->
  `TtyPayload.window_size`; hardware TTY payloads are seeded by
  `DEFAULT_HARDWARE_WINSIZE = Winsize::new(24, 80)` in
  `crates/tx-subsystems/src/tty/structure/payload.rs`. `TIOCSWINSZ` already
  writes the same payload slot and fires the staged `WinsizeChanged` /
  SIGWINCH side effect, so post-boot guest `stty rows N cols M` has a kernel
  storage path, but the initial value is still a local default.
- The current QEMU/console path has no guest-visible winsize source. Both
  `cargo xtask qemu --interactive` and `cargo xtask shell-test` use
  `-display none -serial mon:stdio` plus `console=ttyS0`. The HAL `ConsoleIf`
  exposes only `write_bytes` and `read_bytes`; the rv64 QEMU board implements
  those via legacy SBI console putchar/getchar. QEMU accepts stdio chardev
  `rows=` / `cols=` options, but a QEMU virt DTB dump with
  `-chardev stdio,id=ser0,rows=40,cols=100,signal=off -serial chardev:ser0`
  still exposes only `stdout-path = /soc/serial@10000000` and no
  rows/cols/width/height property. Under the current serial/SBI byte-stream
  design, there is nothing for txKernel to read back as terminal geometry.
- The current shell still prints `sh: can't access tty; job control turned off`.
  That is the clearest guest-visible sign that controlling-terminal/job-control
  integration is incomplete, not just a `vi` quirk.
- A focused `/dev` probe under the noisy trap-trace kernel confirmed the guest
  namespace gap behind that message: BusyBox `tty` reports `not a tty`, `ls -l
  /dev/console /dev/tty /dev/ttyS0` reports that `/dev/tty` and `/dev/ttyS0`
  are absent, `test -r/-w /dev/tty` fails, and `echo ... >/dev/tty` fails with
  `can't create /dev/tty: Read-only file system`. The same run still has a
  working fd-0 TTY for `TCGETS`/`TCSETS`, so this is not a total console
  failure.
- The code path explains the probe: boot registers the hardware TTY with
  `register_hardware("console", 0, ...)` and later re-publishes the same cap
  under the `console` alias. `tty::project::resolve_devfs_alias` can resolve
  `ttyS<N>` through the hardware table, but devfs `readdir` enumerates only
  registered alias snapshot entries, and the boot registration name is
  `console`, not `ttyS0`. A closer lookup review found a sharper split:
  `Devfs::lookup` first calls `resolve_devfs_alias(name)`, but if that succeeds
  through the hardware table rather than the alias snapshot it never assigns an
  object id, so `ttyS0` still falls through to `ENOENT`. `load_inode_meta`,
  `materialise_rnode`, and `readdir` are likewise index-based over alias
  entries rather than the hardware table. There is also no special `/dev/tty`
  node that resolves to the caller's controlling tty.
- The trace reinforces the controlling-tty gap: early `TIOCGWINSZ` on fds 0/1/2
  succeeds, `TCGETS`/`TCSETS` on fd 0 succeeds, but `TIOCGPGRP` returns
  `-EINVAL`, matching an unbound `TtyIdentity.session_pgrp`. `sys_openat` has no
  controlling-tty side effect after opening a TTY path; it resolves, installs an
  fd, and does not implement Linux's "session leader opening a terminal without
  `O_NOCTTY` acquires a controlling terminal" rule.
- A `setsid` + `/dev/console` reopen probe separated controlling-tty acquisition
  from plain path reopen support. BusyBox `setsid` exists and succeeds; the
  nested shell then opens `/dev/console` for stdin with `openat(...,
  flags=0x8000)` and `dup3`s it to fd 0 successfully. The next redirection,
  `>/dev/console`, uses `openat(..., flags=0x8241)` and fails with
  `-ENOSYS`, producing `sh: can't create /dev/console: Function not
  implemented`. `0x8241` is `O_WRONLY | O_CREAT | O_TRUNC | O_LARGEFILE`.
  `sys_openat` applies `O_TRUNC` through the mounted backend's
  `FsPageBacking::truncate` before materialising the `OpenFile`, and devfs only
  treats `/dev/null` truncation as a no-op; `/dev/console` still returns
  `ENOSYS`. Linux ignores `O_TRUNC` on character devices, so this is an
  independent devfs/VFS parity blocker for shell redirections to TTY nodes.
- A narrower stdin-only `setsid` probe removes the `O_TRUNC` variable. The
  nested shell successfully reopens `/dev/console` onto fd 0 and then prints
  `not a tty` for BusyBox `tty`, while `stty size` still reports `24 80` and
  the nested command exits 0. The trap trace shows `setsid -> 2`,
  `openat(..., 0x8000) -> 3`, `dup3(3, 0, 0) -> 0`, successful `TCGETS` /
  `TIOCGWINSZ` ioctls, and no `TIOCSCTTY (0x540e)`. This confirms that plain
  reopening works, termios still works on the fd, and controlling-terminal
  acquisition is missing independently of the stdout/stderr redirection bug.
- TTY ioctl coverage is ahead of raw I/O coverage. `sys_ioctl` handles
  `TCGETS`, `TCSETS*`, `TIOCGPGRP`, `TIOCSPGRP`, `TIOCGWINSZ`,
  `TIOCSWINSZ`, `TIOCSCTTY`, and `TIOCNOTTY`; the VFS typed ioctl surface
  routes `TIOCSCTTY`, `TIOCNOTTY`, and `TIOCSPGRP` through process-aware
  helpers.
- The `read(2)` / `write(2)` syscall path still drives
  `OpenFileReadOp` / `OpenFileWriteOp`, whose TTY arms call
  `tty::execution::step_read` and `step_write`, not the process-aware
  `step_read_for_process` / `step_write_for_process` helpers. This leaves
  foreground-pgrp checks and background `SIGTTIN` / `SIGTTOU` delivery
  exercised only by local tests, not by real Alpine fd I/O.
- VINTR is recognized by the line discipline, but `step_ingest` currently
  records a `SignalDispatch` and fires a deferred TTY event; the real input
  path does not yet prove end-to-end `^C` delivery to the foreground pgrp.
- Non-canonical input is only partly Linux-complete. `VMIN` is honored for the
  `VTIME == 0` slice, while `VTIME != 0` falls back to the staged behavior.
  `TCSETSW` and `TCSETSF` currently alias to immediate `TCSETS`.
- The focused edit smoke remains red: after
  `TERM=vt100 vi /tmp/vi-edit.txt`, sending `iguest-vi-edit-ok\n ESC :wq\n`
  leaves BusyBox vi in the editor screen and the script times out waiting for
  the shell prompt. The captured screen shows insert mode (`I`) after the
  batch, so the escape/command sequence is not being interpreted as a clean
  exit path.
- A follow-up trap-trace pass narrowed the `vi` edit failure further: the
  shell-test harness decodes `\e` correctly and sends the whole edit command
  in one `write_all`, while the kernel console drain batches up to 512 bytes
  into one `step_ingest` call. In raw mode every queued byte fires
  `TTY_READABLE`; `sys_ppoll`'s TTY readiness path checks only that bit, and
  `step_read` currently disables its `VMIN` threshold handling whenever
  `VTIME != 0`. BusyBox vi therefore sees `ESC` followed immediately by
  queued `:wq` bytes during its ESC ambiguity timeout and treats them as ESC
  follow-up input instead of a command-mode `:wq`.
- The control experiment split the same vi interaction into separate sends
  (`insert text`, delay, `ESC`, delay, `:wq`) and passed under both the default
  kernel and a `trap-trace` kernel. In that passing trace, vi's ESC timeout
  `ppoll` returns `0` before the later `:wq` arrives; in the failing batched
  trace, the corresponding timeout `ppoll` returns `1` and vi reads the queued
  bytes.

## Verification

- Passed:
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 180s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-vi-reopen-default-winsize-smoke.txt`
- Failed as expected for the open blocker:
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 120s cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-vi-edit-smoke.txt`
  timed out after the scripted `ESC :wq` batch, still inside vi.
- Passed control:
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 120s cargo xtask shell-test --target rv64-qemu --profile alpine --script /tmp/tx-alpine-vi-split-trace.shelltest`
  with the edit, ESC, and `:wq` split across sends. The trap-trace variant also
  passed after
  `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf --features trap-trace`;
  syscall view saved to
  `/tmp/tx-alpine-vi-split-traptrace-feature-syscalls.txt`.
- Investigated `/dev/tty` probe:
  `/tmp/tx-alpine-tty-probe.log` shows `tty` -> `not a tty`,
  `/dev/tty` and `/dev/ttyS0` absent, `/dev/tty` write failure, successful
  `TCGETS`/`TCSETS` on fd 0, and `TIOCGPGRP` returning `-EINVAL`.
- Investigated `setsid` + `/dev/console` reopen probe:
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 120s cargo xtask shell-test --target rv64-qemu --profile alpine --script /tmp/tx-alpine-setsid-console-probe.shelltest`
  reproduced `sh: can't create /dev/console: Function not implemented`. After
  `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf --features trap-trace`,
  the trap-trace run saved
  `/tmp/tx-alpine-setsid-console-traptrace-run.log`; syscall view saved to
  `/tmp/tx-alpine-setsid-console-traptrace-syscalls.txt`. Key sequence:
  `setsid -> 2`, `openat(..., 0x8000) -> 3`, `dup3(3, 0, 0) -> 0`,
  `openat(..., 0x8241) -> -38 (ENOSYS)`.
- Investigated stdin-only `setsid` + `/dev/console` reopen probe:
  `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu timeout 120s cargo xtask shell-test --target rv64-qemu --profile alpine --script /tmp/tx-alpine-setsid-stdin-only-probe.shelltest`
  passed and printed `nested-stdin-start`, `not a tty`,
  `nested-tty-status-1`, `24 80`, `nested-stty-status-0`, and
  `setsid-stdin-status-0`. The trap-trace run saved
  `/tmp/tx-alpine-setsid-stdin-only-traptrace-run2.log`; syscall view saved to
  `/tmp/tx-alpine-setsid-stdin-only-syscalls2.txt`. Key sequence:
  `setsid -> 2`, `openat(..., 0x8000) -> 3`, `dup3(3, 0, 0) -> 0`,
  `ioctl(fd0, TCGETS) -> 0`, `ioctl(fd0, TIOCGWINSZ) -> 0`, with no
  `TIOCSCTTY`.
- Investigated winsize source:
  `qemu-system-riscv64 -chardev stdio,help` lists `rows=<num>` and
  `cols=<num>`, but
  `qemu-system-riscv64 -machine virt,dumpdtb=... -display none -chardev stdio,id=ser0,rows=40,cols=100,signal=off -serial chardev:ser0 -S`
  followed by `dtc -I dtb -O dts ...` showed no guest-visible terminal-size
  property. Code review found no `ConsoleIf` / rv64 SBI / `register_hardware`
  / boot console hook that could query QEMU for rows/cols.

## Next Steps

1. Expose the hardware console under Linux-shaped names as well as the current
   `/dev/console` alias: at minimum `/dev/ttyS0` for the registered hardware
   TTY. The current devfs path must either give hardware-table entries stable
   object ids and enumerate them, or register `ttyS0` as a real devfs alias in
   addition to `console`.
2. Add `/dev/tty` as a caller-relative controlling-tty special node rather than
   an ordinary static alias. It should resolve through the calling process's
   session controlling TTY and fail with the Linux-shaped no-controlling-tty
   errno when absent.
3. Decide where controlling-tty acquisition belongs for Alpine: either an
   explicit bootstrap `TIOCSCTTY` against fd 0, or the Linux `openat` side effect
   for session leaders opening a terminal without `O_NOCTTY`. The current
   syscall trace has no successful `TIOCSCTTY`, and `TIOCGPGRP` stays unbound.
4. Make `O_TRUNC` on devfs character devices a Linux-compatible no-op, not just
   on `/dev/null`; `/dev/console` redirection currently fails before any
   controlling-tty acquisition can be tested.
5. Route real TTY `read(2)` / `write(2)` syscalls through process-aware TTY
   entry points, or add equivalent caller-aware VFS op wrappers.
6. Add a focused `^C` shell-test that starts a foreground long-running command,
   sends VINTR, and expects SIGINT/EINTR-visible return to the prompt.
7. Fix or explicitly stage the non-canonical `VMIN/VTIME` readiness contract:
   `ppoll` and `read` should agree on whether a TTY fd is read-completable
   under the current termios, not only whether the input queue has any byte.
8. Keep the split-send vi edit script as a control witness while turning the
   batched `ESC :wq` smoke into the regression target for raw-mode timing.
9. If the default winsize should reflect the host terminal, the smallest current
   architecture path is host-seeded boot configuration: have xtask read the host
   TTY size and append explicit `tx.tty.rows=N tx.tty.cols=M`-style cmdline
   tokens, then seed the console hardware TTY from `BootInfo::cmdline`.
   Reading it "from QEMU" would require a new guest-visible device/side channel,
   such as a future virtio-console/hvc integration, not the existing SBI serial
   path.

## Sources

- `docs/design/06_devices/TTY.md`
- `docs/progress/STATUS.md`
- `crates/tx-subsystems/src/tty/`
- `crates/tx-subsystems/src/vfs/execution.rs`
- `crates/tx-shims/src/linux_syscall/{fs_basic.rs,io.rs}`
- `crates/tx-kernel/src/init.rs`
- `crates/tx-fs/src/devfs/mod.rs`
- `crates/tx-hal/src/lib.rs`
- `boards/tx-hal-riscv64-qemu-virt/src/{lib.rs,sbi.rs,dtb.rs}`
- `xtask/src/{qemu.rs,shell_test.rs}`
- `tools/shell-tests/alpine-vi-reopen-default-winsize-smoke.txt`
- `tools/shell-tests/alpine-vi-edit-smoke.txt`
- `/tmp/tx-alpine-tty-probe.log`
- `/tmp/tx-alpine-setsid-console-traptrace-syscalls.txt`
- `/tmp/tx-alpine-setsid-stdin-only-syscalls2.txt`
