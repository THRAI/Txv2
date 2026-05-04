---
name: tx-tty-subsystem
description: Use when implementing, auditing, or integrating txKernel TTY work; points to the canonical TTY docs, the current tty code layout, and the known staging seams that must be revisited after Process, Signal, and VFS mature.
---

# tx-tty-subsystem

Use this skill when work touches termios, line discipline, ptys, controlling
ttys, hangup, devpts, or tty-facing VFS/process integration.

## Read First

- `docs/design/06_devices/TTY.md`
- `docs/design/06_devices/DEVICE.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- `docs/ljs/TTY_DESIGN_PLAN.md`
- `docs/progress/research/2026-05-04-tty-implementation-status.md`

## Current Code Map

- `crates/tx-kernel/src/tty/structure/`
- `crates/tx-kernel/src/tty/ldisc/`
- `crates/tx-kernel/src/tty/checks/`
- `crates/tx-kernel/src/tty/execution/`
- `crates/tx-kernel/src/tty/project.rs`
- `crates/tx-kernel/src/tty/tests.rs`

## Current State

- `TtyIdentity` / `TtyPayload` is implemented as the zone-backed split entity shape.
- Termios defaults, cooked/raw input processing, output post-processing, and
  termios-change handling are implemented.
- TTY execution steps exist for read, write, ingest, ioctl, hangup, pty open,
  hardware registration, and last-master-close handling.
- devfs/devpts-facing projection helpers exist below the final VFS layer.
- TTY-local tests cover line discipline, ioctl, pty lifecycle, and hangup edges.

## Known Staging Seams

- `SessionPgrp` still stores numeric ids; final design wants
  `Cap<Session>` and `Cap<ProcessGroup>`.
- TTY job-control paths currently emit `SignalDispatch` descriptions; final
  delivery must be connected by the Process/Signal subsystem.
- `TtyPayload.termios` is a staged published slot over `Termios`, not yet the
  final `AtomicSlot<Arc<Termios>>` shape.
- `/dev/console` is currently modeled through alias registration rather than a
  fully settled final console integration path.
- Final controlling-tty ownership cleanup still depends on Process/Session/VFS
  lifecycles outside `tty/`.

## Integration Rules

- Prefer `tty::execution::*` and `tty::project::*` entry points over poking tty
  internals from VFS, Process, or Signal code.
- Treat `docs/design/06_devices/TTY.md` as canonical when a staging
  implementation differs from the final design.
- Keep TTY-only work inside `crates/tx-kernel/src/tty/` when possible; widen
  outward only when the other subsystem is ready to replace a staging seam.
- When you remove or replace a staging seam, update progress memory so the next
  session does not have to rediscover the state.

## Done Means

- The implementation or audit explicitly cites the canonical TTY design docs.
- Any changed staging seam is reflected in `docs/progress/`.
- Other subsystems integrate through tty-owned entry points instead of copying
  tty semantics into foreign modules.
