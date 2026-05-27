# Research: TTY implementation status and follow-up seams

**Date:** 2026-05-04

## Question

Where are the canonical TTY design docs, what is implemented now under
`crates/tx-kernel/src/tty/`, and which seams must future work revisit after
Process, Signal, and full VFS integration land?

## Findings

- Canonical design references for TTY work are:
  - `docs/design/06_devices/TTY.md`
  - `docs/design/06_devices/DEVICE.md`
  - `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
  - local staged plan: `docs/ljs/TTY_DESIGN_PLAN.md`
- Current TTY implementation lives at `crates/tx-kernel/src/tty/` and is split
  into:
  - `structure/`: `TtyIdentity`, `TtyPayload`, termios, winsize, ring, registry
  - `ldisc/`: cooked/raw input pipeline, output post-processing, termios-change handling
  - `checks/`: live-tty, foreground-pgrp, session-leader gates
  - `execution/`: read, write, ingest, ioctl, hangup, openpty, master-close, hardware registration
  - `project.rs`: devfs/devpts-facing materialization helpers below a full VFS
  - `tests.rs`: tty-local coverage for ldisc, pty, hangup, ioctl, and boundary cases
- The current code already implements the main tty-only slice described by the
  design:
  - zone-backed `TtyIdentity` / `TtyPayload` factoring
  - termios defaults and N_TTY-style line discipline behavior
  - tty read/write/ioctl/ingest entry points
  - pty master/slave allocation and peer-linked payloads
  - hangup behavior that preserves identity while dropping payload
  - devfs/devpts projection helpers that can be wrapped by future VFS code
- The implementation intentionally contains staging seams where the design
  expects other subsystems that do not exist yet:
  - full syscall/fd-table ioctl entry wiring is still missing even though the
    VFS-local `OpenFile::step_ioctl` tty dispatch surface now exists
  - `TtyPayload.termios` is staged as a published slot over `Termios`, not yet
    the final `AtomicSlot<Arc<Termios>>` shape from `TTY.md`
  - non-canonical read completion only partially follows the design target:
    `VMIN` is honored for the `VTIME == 0` slice, while timer-driven `VTIME`
    behavior remains unimplemented
  - `tcsets` still runs the ingest linearizer inline after publication, rather
    than waking a reactor-scheduled synthetic ingest pass
  - hardware tty ingest still relies on `step_poll_hardware_input()` as a
    bridge, not full IRQ/reactor auto-wiring
  - `/dev/console` currently behaves through alias registration, not a fully
    separate finalized console integration path
  - full session cleanup, controlling-tty ownership, and VFS lifecycle hooks
    still need the external modules to land
- The current external surfaces that other modules should prefer are:
  - `tty::execution::{register_hardware, register_console_alias, step_openpty, step_read, step_write, step_ingest, step_hangup, step_master_close_last, step_ioctl_*}`
  - `tty::project::{open_ptmx, open_devfs_tty_by_name, devfs_rnode_by_name, devpts_rnode_by_index, open_file_for_tty}`
  - `vfs::{OpenFileIoctl, OpenFileIoctlCaller}` plus
    `OpenFile::step_ioctl(...)` for tty-backed open files
- Verified tty-local implementation state before this note:
  - `cargo test -p tx-kernel tty -- --test-threads=1`
  - `cargo test -p tx-kernel --lib -- --test-threads=1`
  - `cargo fmt --check`
  - `git diff --check`
  - manual smoke by user: `cargo xtask build --target rv64-qemu`,
    `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel`

## Applicability To txKernel

- Future TTY work should start from `docs/design/06_devices/TTY.md`, then read
  `docs/ljs/TTY_DESIGN_PLAN.md`, then inspect `crates/tx-kernel/src/tty/`.
- Process, Signal, and VFS work should integrate through existing tty entry
  points instead of duplicating tty state management in their own modules.
- When external subsystems become available, the first TTY follow-up work is:
  - replace staged session/pgrp ids with retained process-owned identities
  - connect `SignalDispatch` outputs to real signal delivery
  - finish the docs-directed termios publication model
  - tighten console and controlling-tty lifecycle integration
- If future work changes one of these staging seams, update this progress note
  or add a newer one so later sessions can see what was replaced.

## Sources

- `docs/design/06_devices/TTY.md`
- `docs/design/06_devices/DEVICE.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- `docs/ljs/TTY_DESIGN_PLAN.md`
- `crates/tx-kernel/src/tty/`
