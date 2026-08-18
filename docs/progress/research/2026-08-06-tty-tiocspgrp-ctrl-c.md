# TTY TIOCSPGRP foreground Ctrl-C fix

**Date:** 2026-08-06

## Symptom

In the Alpine ext4 rootfs under RV64 QEMU, an interactive BusyBox `ping`
continued after the user typed Ctrl-C. The UART byte path, N_TTY VINTR
classification, TTY-to-signal bridge, process-group fanout, and default SIGINT
termination path were already present.

## Cause

BusyBox `ash` creates a foreground process group for the external command and
uses `tcsetpgrp`, which reaches the `TIOCSPGRP` arm in Linux `sys_ioctl`.
That arm called the legacy raw-id helper. It updated
`SessionPgrp.foreground_pgid`, so `TIOCGPGRP` could report the new number, but
left `SessionPgrp.foreground_pgrp` pointing at the old typed process group.
VINTR delivery follows the typed reference, so SIGINT missed `ping`.

This violated `txdoc:TTY-SESSION-PGRP-MODEL-1`: numeric pgids are namespace
signifiers and must be resolved to the canonical `ProcessGroup` before the TTY
binding is published.

## Fix

The syscall arm now resolves the supplied pgid through
`process_group_by_pgid(Pgid(...))` and calls the existing
`step_ioctl_tiocspgrp_for_process` helper. The helper checks same-session
membership and publishes a binding whose numeric and typed foreground-group
views agree. A missing pgid returns `ESRCH`.

The syscall test covers both an unbound TTY and a successful rebinding. The
successful case asserts `foreground_pgrp_cap()` names the child's canonical
group, defending the exact stale-reference regression. A shell-test sends byte
`0x03` to a foreground BusyBox `ping` and requires the shell prompt to return.

## Verification

- `cargo test -p tx-shims dispatch_ioctl_tiocspgrp -- --test-threads=1`:
  2 passed.
- `cargo xtask build --target rv64-qemu`: passed.
- `cargo xtask shell-test --target rv64-qemu --script
  tools/shell-tests/busybox-ping-ctrl-c.txt`: passed; loopback `ping` printed
  statistics and returned to `/ #` after `0x03`.
- `cargo xtask build --target rv64-qemu --release` and
  `cargo xtask oscomp submit --target rv64-qemu --submit
  local-images/rv-submit --release`: passed; refreshed `kernel-rv` is
  byte-identical to the release ELF.
- Four-hart QEMU with the release `kernel-rv`, Alpine ext4 mother disk behind a
  temporary qcow2 overlay, and `/bin/busybox ping 127.0.0.1`: Ctrl-C printed
  statistics and returned to `/ #`; the temporary overlays were removed and
  the mother disk was not modified.

The broad `cargo -q xtask unit` gate is not green in the current dirty
worktree: the existing tx-shims suite has 44 unrelated failures in its full
serial run, and tx-kernel has three unrelated failures in the unit gate. These
are baseline blockers outside this focused TTY fix; the targeted host and QEMU
witnesses pass.

## Next

Restart any QEMU instance that still has the old ELF loaded, using the refreshed
`local-images/rv-submit/kernel-rv`. A running VM cannot acquire changed kernel
code without rebooting.
