# TTY process-aware helpers

**Date:** 2026-05-05
**Branch:** local worktree
**Status:** Complete. `cargo test -p tx-subsystems tty -- --test-threads=1` green (71 tests).

## Goal

Take the parts of the TTY staging surface that are already unlocked by the
current Process / Signal implementation and wire them to canonical
process-owned entities instead of stopping at raw-id helper values.

## What landed

- `IoctlCaller::from_process(...)` now builds a TTY caller snapshot from a live
  `Cap<ProcessIdentity>`, including:
  - `session_id` / `pgrp_id`
  - typed `Weak<ProcessGroup>`
  - session-leader detection
  - controlling-tty presence
  - foreground/background classification
  - SIGTTIN / SIGTTOU ignore-state snapshot from `sig_actions`
- New process-aware TTY entry points:
  - `step_ioctl_tiocsctty_for_process`
  - `step_ioctl_tiocnotty_for_process`
  - `step_ioctl_tiocspgrp_for_process`
  - `step_read_for_process`
  - `step_write_for_process`
- The controlling-tty path now updates both sides of the binding in one place:
  - TTY authoritative slot: `tty.session_pgrp`
  - Process-side mirror: `session.controlling_tty`
- TTY signal dispatch now has a small bridge helper
  `deliver_signal_dispatch_for_process(...)`, and the new process-aware
  background read/write helpers use it to post SIGTTIN / SIGTTOU immediately.
- `Session::leader_pgrp_cap()` landed so TTY hangup can resolve a typed
  session-leader pgrp target.
- `step_hangup` now emits a typed `SessionLeaderProcessGroup` target when the
  session-side topology is available, rather than always falling back to a
  raw-pgid-only dispatch.

## What this does not finish

- Numeric `pgid` lookup for full `tcsetpgrp(fd, pgid)` syscall wiring is still
  blocked on the process-side registry / lookup story. The new typed helper
  expects a resolved `Cap<ProcessGroup>`.
- Full fd-table / syscall-driver ioctl routing is still future work; these
  helpers are ready to be called from that layer once it lands.
- The legacy raw-id TTY ioctl/read/write helpers remain in place for existing
  tests and callers.

## Verification

- `cargo fmt --all`
- `cargo test -p tx-subsystems tty -- --test-threads=1`

## Follow-up

Natural next steps once more surrounding infrastructure lands:

1. Thread `ProcessIdentity` into the real tty ioctl syscall path so callers use
   the process-aware helpers by default.
2. Resolve numeric `pgid` to `Cap<ProcessGroup>` so `tcsetpgrp` can stop at the
   typed helper instead of the raw-id fallback.
3. Push the same process-aware bridge into any future `OpenFile::step_ioctl`
   layer so TTY signal side effects are delivered automatically.
