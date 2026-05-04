# VFS spec-reconciliation Phase 1: working `tx-kernel/src/vfs` → doc spec

**Date:** 2026-05-04
**Branch:** `vfs-spec-reconciliation`
**Status:** Complete. Five sub-steps landed; verification gates green.

## Context

The deferred-move decision note
([`2026-05-04-vm-tty-subsystems-move-deferred.md`](2026-05-04-vm-tty-subsystems-move-deferred.md))
identified five drift axes between the working `tx-kernel/src/vfs.rs`
and the doc spec at `TX_EXT4_PLAN_v1_2.md` / `bringup_fs_specs_v_1` /
`SUBSYSTEM_ANATOMY_v2_1.md`. Phase 1 of the four-phase reconciliation
plan closes the working-side drift.

## What changed

| Drift axis | Working before | Doc spec | Working after |
|---|---|---|---|
| `InodeMeta` fields | `{kind, mode, uid, gid, size, nlink, rdev}` (no timestamps; doc-absent `kind`/`rdev`) | `{mode, uid, gid, size, atime, mtime, ctime, nlinks, blocks, flags}` per [`TX_EXT4_PLAN_v1_2.md:433`](../../design/05_filesystem/TX_EXT4_PLAN_v1_2.md) | matches doc; `kind()` is now a `mode & S_IFMT` derivation method; `rdev` removed |
| `DirCursor` shape | `pub struct DirCursor(u64)` | `pub struct DirCursor(pub [u8; 16])` per [`TX_EXT4_PLAN_v1_2.md:448`](../../design/05_filesystem/TX_EXT4_PLAN_v1_2.md) | matches doc; `from_u64`/`as_u64` helpers preserve the u64-encoded common case |
| `Timespec` type | absent | required by InodeMeta atime/mtime/ctime | added with `EPOCH` const |
| `MountOutput` type | absent | defined in [`TX_EXT4_PLAN_v1_2.md:541`](../../design/05_filesystem/TX_EXT4_PLAN_v1_2.md) and [`bringup_fs_specs_v_1:240`](<../../design/05_filesystem/bringup_fs_specs_v_1 (1).md>) | added |
| Subsystem layout | flat 753-line `vfs.rs` | `structure/` + `checks/` + `execution/` per [`SUBSYSTEM_ANATOMY_v2_1.md:35`](../../design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) | decomposed into `vfs/{mod,structure,checks,execution,tests}.rs` |

## Sub-step ledger

1. `8ebae8f` — `vfs: extend InodeMeta to full POSIX shape per bringup_fs_specs`
2. `c17e791` — `vfs: reshape DirCursor to opaque [u8; 16] per TX_EXT4_PLAN_v1_2`
3. `c5466a5` — `vfs: add MountOutput type per TX_EXT4_PLAN_v1_2`
4. (cascade verification — no commit; ran workspace tests)
5. `<this commit>` — `vfs: decompose into structure/checks/execution per SUBSYSTEM_ANATOMY_v2_1`

## Decisions taken

- **`kind` field on `InodeMeta` removed; `kind()` derives from `mode & S_IFMT`.**
  The doc encodes file kind in mode bits; carrying a separate `kind` field
  was redundant denormalization. Callers that wrote
  `InodeMeta::new(InodeKind::X, full_mode)` continue to work because the
  constructor preserves caller-provided S_IFMT bits and OR's the kind in
  only when mode lacks them.
- **`rdev` field removed from `InodeMeta`.** Not in doc. Will be
  reintroduced when device subsystem genuinely needs to thread major/minor
  through stat; currently no caller reads it.
- **`DirCursor::new` signature changed from `new(u64)` to `new([u8; 16])`** to
  match the doc-canonical opaque byte form. The single tx-kernel call site
  (devpts readdir) was updated to use the new `from_u64` helper.
- **`OpenFile::step_read`/`step_write` placed in `execution.rs` (split-impl
  block)**, not `structure.rs`. Per `SUBSYSTEM_ANATOMY_v2_1` §execution,
  step bodies belong in `execution/`; the data type owning them stays in
  `structure/`. Rust's split-impl mechanism keeps method-call ergonomics.

## Doc-blank items deferred

- `FsObjectId` field visibility — kept private with `new()`/`as_u64()`
  accessors. tx-ext4 uses tuple-style construction; will switch to
  `FsObjectId::new()` in Phase 3.
- `NameOwned` vs `InlineName` naming — kept `InlineName` as the working
  name; the deleted skeleton called it `NameOwned`. tx-ext4 will rename
  in Phase 3.
- `RNodeFileType` vs `InodeKind` — kept `InodeKind` (matches POSIX
  `st_mode` lexicon). tx-ext4 will rename in Phase 3.
- `vfs::execution::read_harness::VfsReadHarness` test scaffolding —
  not addressed; tx-ext4 tests that depend on it will be ported or
  deleted in Phase 3.

## Verification

- `cargo fmt --check` — clean.
- `cargo build --workspace` — clean (board crate linker errors are pre-
  existing macOS-host limitations on cross-target linker scripts; not
  caused by this work).
- `cargo test -p tx-kernel --lib -- --test-threads=1` — 183/183 ok.
- `cargo test --workspace --exclude <board crates>` — all green.
- `cargo clippy -p tx-kernel -- -D warnings` — clean.
- `cargo xtask lint arch/unused/docs` — ok.
- `cargo xtask progress validate` — 24 records ok.

## Next step

Phase 2 — bring the tx-subsystems skeleton into spec:
1. Reshape skeleton `Frame` from inline byte buffer to PPN-handle per
   [`PAGE_BACKED_v1.md`](../../design/03_memory-vm/PAGE_BACKED_v1.md).
2. Rename skeleton `Errno` from `Invalid/NoEntry/NotImplemented/...` to
   POSIX `EINVAL/ENOENT/ENOSYS/...` per uniform usage across docs.

Phase 3 then ports tx-ext4 onto the canonical surface; Phase 4 deletes
the skeleton and moves `vm/`+`tty/` into `tx-subsystems`.

## Blockers

None.
