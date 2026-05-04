# VM and TTY → tx-subsystems move deferred

**Date:** 2026-05-04
**Status:** Deferred. Branch `move-vm-tty-to-subsystems` was created, the move
was attempted, then fully reverted with `git reset --hard` and the branch
deleted. Working tree is back to `3cdd293` (origin/main HEAD merge).

## What was attempted

A consolidation move ("option 2"): relocate `vm/`, `tty/`, plus their hard
dependency closure (`page_backed.rs` + `page_backed/`, `vfs.rs`, `mount.rs`,
`device.rs`, `execution.rs`, `sync.rs`, `wait_carrier.rs`, `zones.rs`) from
`crates/tx-kernel/src/` into `crates/tx-subsystems/src/`, deleting the
existing tx-subsystems skeletons that conflicted with the working tx-kernel
implementations, and collapsing `tx-kernel` to `init.rs` + `trap.rs` +
`lib.rs`.

The mechanical relocation succeeded. `tx-subsystems` built standalone (with
dead-code warnings). The blocker was downstream.

## Why it was deferred

`crates/tx-ext4/` is wired against the **deleted skeleton**'s VFS surface,
not against the tx-kernel-derived working VFS. The two surfaces are not just
differently-named — they model different things:

- **`Frame`**: deleted skeleton was an inline byte buffer
  (`{ len: u16, bytes: [u8; 4096] }`) with `Frame::from_bytes`; the
  tx-kernel-derived `Frame` is a physical-page handle (`{ ppn: Ppn }`) with
  no byte-copy constructor. tx-ext4's `FsPageBacking::fetch_page`
  ([crates/tx-ext4/src/pager.rs:30](../../../crates/tx-ext4/src/pager.rs))
  builds a `Frame` from raw page bytes. Against the new `Frame`, that call
  site requires allocating a physical frame, copying bytes via direct map,
  and managing permanent-frame vs cache-pin lifecycle.
- **`InodeMeta`**: deleted skeleton had full POSIX surface
  (`{atime, mtime, ctime, nlinks, blocks, flags, ...}`); the
  tx-kernel-derived `InodeMeta` carries only `{kind, mode, uid, gid, size,
  nlink, rdev}`. tx-ext4 maps complete ext4 metadata into the deleted
  shape ([crates/tx-ext4/src/read_backend.rs:99](../../../crates/tx-ext4/src/read_backend.rs)).
  The new shape cannot represent timestamps or block counts.
- **`DirCursor`**: deleted skeleton was a byte array (used as opaque cursor
  state by tx-ext4 at
  [crates/tx-ext4/src/read_backend.rs:115](../../../crates/tx-ext4/src/read_backend.rs));
  the tx-kernel-derived type is `pub struct DirCursor(u64)`. Different
  iteration model.

This is not a renames-and-paths shim. tx-ext4 was implemented against a
**richer VFS API** that tx-kernel never had, and the spec-shaped skeleton
in `tx-subsystems::vfs::structure` / `::fs_ops` was the home of that
API. Consolidating onto the tx-kernel surface loses ext4 feature parity
(no timestamps, no full POSIX inode metadata, no byte-array cursors).

## Path narrowing during the attempt

Three options were surfaced to the user:

1. Narrow move (`vm` and `tty` only) — fails to compile because both
   import `crate::page_backed::*`, `crate::execution::*`, `crate::sync::*`,
   `crate::wait_carrier::*`. A `tx-subsystems → tx-kernel` dependency would
   resolve those, but `tx-kernel` already depends on `tx-subsystems` —
   cycle.
2. Full consolidation (option 2) — what was attempted; blocked by tx-ext4
   API mismatch.
3. Bridged move (vm/tty + minimal deps; rename skeletons) — would work
   mechanically but produces a workspace with two parallel
   `page_backed`/`vfs`/`mount` namespaces in `tx-subsystems`, and still
   leaves the question of which is canonical.

User chose A: revert. Documented here.

## Why this is real architectural debt

Two parallel VFS APIs exist in the workspace today:

- `tx-kernel/src/vfs.rs` (660 lines) — **working**, used by VM, TTY, Mount.
  Smaller surface; lacks timestamps, byte-array cursors.
- `tx-subsystems/src/vfs/structure/` (814 lines) +
  `tx-subsystems/src/vfs/fs_ops.rs` (169 lines) — **spec-shaped**,
  used by tx-ext4. Richer surface; no actual subsystem implementations
  consume it inside `tx-subsystems` itself.

The same dual-existence applies to `page_backed` (1473-line working
kernel impl vs 184-line skeleton) and `mount` (517 vs 625).

The `MODULE_MAP_v1` doc places these subsystems under `tx-subsystems`. The
working code lives under `tx-kernel`. The skeletons under `tx-subsystems`
are the doc-aligned home but haven't reached parity. Until the two
surfaces are reconciled (either by porting the working kernel
implementations to be richer, or by porting tx-ext4 onto a less rich
surface), `vm` and `tty` cannot move into `tx-subsystems` cleanly.

## Recommended next step

Before re-attempting the move, do a focused VFS surface design pass:

1. Pick a canonical `Frame` model — PPN handle with allocator-backed
   byte-copy helper, or inline byte buffer. Document on `PAGE_BACKED_v1`
   and `BDEV_FS`.
2. Pick a canonical `InodeMeta` shape — minimal POSIX (timestamps + nlinks
   + blocks + flags) and update `VFS_CHECKS_V2.1` plus `bringup_fs_specs`.
3. Pick a canonical `DirCursor` model — fixed `u64` token, or
   filesystem-defined byte-array.
4. Once 1–3 are landed, port the working `tx-kernel/src/vfs.rs` and
   `page_backed.rs` to the canonical shapes and delete the skeleton
   duplicates in `tx-subsystems`.
5. Then `vm` and `tty` move cleanly into `tx-subsystems` with their
   dependency closure.

Estimated 1–2 phases of design + plan + execute work, not a session-sized
mechanical reorg.

## Verification

`git status` clean on `claude/distracted-lichterman-a1f41c`. No build
or test runs were left in flight. Branch `move-vm-tty-to-subsystems`
deleted.
