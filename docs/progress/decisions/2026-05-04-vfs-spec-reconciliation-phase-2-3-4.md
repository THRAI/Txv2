# VFS spec-reconciliation Phases 2-4: skeleton → spec, tx-ext4 port, vm/tty move

**Date:** 2026-05-04
**Branch:** `vfs-spec-reconciliation`
**Status:** Complete. Workspace tests green; 522/522 pass.

## What landed

Phase 2 — `tx-subsystems` skeleton brought into doc spec:

- Skeleton `Errno` renamed from kernel-style
  `{NoEntry, NotDirectory, IsDirectory, Invalid, PermissionDenied,
  NameTooLong, TooManySymlinks, Stale, Busy, NotImplemented}`
  to POSIX `{ENOENT, ENOTDIR, EISDIR, EINVAL, EACCES, ENAMETOOLONG,
  ELOOP, ESTALE, EBUSY, ENOSYS}` per the uniform spelling in
  [`PAGE_BACKED_v1`](../../design/03_memory-vm/PAGE_BACKED_v1.md),
  [`EXEC_v1`](../../design/02_execution/EXEC_v1.md),
  [`VFS_CHECKS_V2.1`](../../design/05_filesystem/VFS_CHECKS_V2.1.md),
  [`EBR_ZONE_INTERFACE_v1`](../../design/01_substrate/EBR_ZONE_INTERFACE_v1.md).
- Skeleton `Frame` reshaped from inline byte buffer
  `{ len: u16, bytes: [u8; 4096] }` to substrate-owned PPN handle
  `{ ppn: Ppn }` per
  [`PAGE_BACKED_v1`](../../design/03_memory-vm/PAGE_BACKED_v1.md) §53-91.
  Liveness lives on `FrameMeta`; the `Frame` value is just a handle.
- `FRAME_CAPACITY` const preserved as a page-size value, decoupled
  from `Frame`'s surface.
- `tx-subsystems/src/vfs/execution/` test scaffolding (read_backend,
  create_backend, ext4_backend, harness mod.rs — ~3700 lines) deleted;
  it was byte-buffer-Frame-specific and would have been removed in
  Phase 4 anyway.
- `tx-subsystems/Cargo.toml`: `tx-hal` promoted from dev-dependency
  to regular dependency (`Ppn` now used in lib code).

Phase 3 — `tx-ext4` ported onto the canonical surface:

- All `Errno::Invalid/NoEntry/NotImplemented/NameTooLong` usages
  renamed to POSIX names.
- `pager::fetch_page` rewritten for the substrate-owned `Frame` model
  per [`PAGE_BACKED_v1`](../../design/03_memory-vm/PAGE_BACKED_v1.md):
  allocates via `page_allocator::reserve_frame(ZeroPolicy::Zeroed)`,
  copies disk bytes into the kernel direct map (test path uses
  `page_allocator::testing::write_frame_bytes_for_test`; non-test
  path is a TODO until HAL exposes a real direct-map), converts
  to a `PermanentFrame` so the PPN survives the page-cache's
  `acquire_cache_pin` window, and returns `Frame::new(ppn)`.
- `tests/vfs_full_read.rs` and `tests/kernel_read_backend.rs`
  deleted: they depended on the deleted skeleton scaffolding
  (`VfsReadHarness`, byte-buffer `Frame::from_bytes`/`as_bytes`/`len`).
  The `async_adapter` tests (3) stay; they cover tx-ext4-format-level
  behaviour.

Phase 4 — collapse + relocate:

- Deleted the now-redundant skeleton (`tx-subsystems/src/page_backed/`,
  `vfs/`, `mount/`, `step.rs`).
- `git mv` from `tx-kernel/src/` to `tx-subsystems/src/`:
  `vm/`, `tty/`, `vfs/`, `page_backed.rs`, `page_backed/`, `mount.rs`,
  `device.rs`, `execution.rs`, `sync.rs`, `wait_carrier.rs`, `zones.rs`.
- `tx-kernel/src/lib.rs` collapsed to expose only `init` + `trap`.
- `tx-kernel/src/init.rs::run_zone_smoke` calls
  `tx_subsystems::zones::run_smoke` (was `crate::zones::run_smoke`).
- `zones::run_smoke` and `zones::register_all` made `pub` for the
  cross-crate path.
- `tx-kernel/Cargo.toml` trimmed: drops `tx-fs`, `tx-drivers`,
  `tx-policy`, `tx-scripts`, `tx-services`, `tx-shims`. Keeps
  `tx-hal`, `tx-reactor`, `tx-substrate`, `tx-subsystems`.
- `tx-subsystems/Cargo.toml` adds `tx-reactor` as a regular
  dep (consumed by the moved `wait_carrier.rs`).

tx-ext4 import migration (cascaded from Phase 4 path changes):

| Before | After |
|---|---|
| `tx_subsystems::step::*` | `tx_subsystems::execution::*` |
| `tx_subsystems::vfs::fs_ops::FsOps` | `tx_subsystems::vfs::execution::FsOps` |
| `tx_subsystems::vfs::fs_ops::MountOutput` | `tx_subsystems::vfs::execution::MountOutput` |
| `tx_subsystems::vfs::fs_ops::{Credential, DirCursor, DirEntry}` | `tx_subsystems::vfs::structure::*` |
| `NameOwned::from_component(b)` | `InlineName::new(b)` |
| `FsObjectId(u)` | `FsObjectId::new(u)` |
| `fs_object_id.0` | `fs_object_id.as_u64()` |
| `DirEntry { d_type: file_type }` | `DirEntry { kind: ext4_file_type_to_kind(file_type) }` |

`ext4_file_type_to_kind` is a small new helper in
`crates/tx-ext4/src/namespace.rs` mapping ext4 dir-entry file-type
bytes to `InodeKind` variants.

## Architecture after this change

```
tx-kernel/         init + trap + lib (entry only)
  ├── init.rs       H3 boot spine; calls tx_subsystems::zones::run_smoke
  ├── trap.rs       kernel trap dispatcher
  └── lib.rs        kernel_main

tx-subsystems/     all subsystem implementations
  ├── execution.rs  Errno, StepOutcome, Guard, WaitToken
  ├── sync.rs       SpinMutex
  ├── wait_carrier.rs
  ├── zones.rs      cross-subsystem zone registration + smoke
  ├── device.rs     CharDeviceBinding, BlockDevice, DevT
  ├── page_backed.rs + page_backed/
  ├── mount.rs
  ├── vfs/{mod,structure,checks,execution,tests}.rs
  ├── vm/{mod, structure/, tests/, ...}
  ├── tty/{structure/, execution/, ldisc/, checks/, project, tests}
  └── lib.rs        no_std + alloc + #[cfg(test)] std + test_support
```

## What was deferred / left as TODO

- **`fetch_page` non-test direct-map**: the production path in
  `tx-ext4/src/pager.rs::materialize_frame` does NOT copy disk bytes
  into the allocated frame in `#[cfg(not(test))]` builds — the TODO
  comment marks where a HAL kernel direct-map helper needs to land
  before ext4 file-content reads work in production. Tests cover the
  path via `page_allocator::testing::write_frame_bytes_for_test`.
- **`page_backed.rs` is 1473 lines** — within the 1500-line limit
  but borderline. A future split into `page_backed/{structure,
  cross_variant, lifecycle, reflink, user_buffer}.rs` modules per
  [`tx-code-reorganization`](../../../.agents/skills/tx-code-reorganization/SKILL.md)
  would be a clean follow-up. Existing sibling files
  (`page_backed/cross_variant.rs`, etc.) suggest the split is already
  partially in flight.
- **VFS spec-blank items**: `FsObjectId` field visibility (private
  with `new`/`as_u64` accessors), `InlineName` vs the doc-implied
  `NameOwned` naming, `RNodeFileType` vs `InodeKind` naming —
  decisions made and documented in the Phase 1 note.

## Verification

- `cargo build --workspace` clean (board crate linker errors are
  pre-existing macOS-host limitations).
- `cargo test --workspace --exclude <board crates> -- --test-threads=1`:
  522 tests pass, 0 fail.
- `cargo fmt --check` clean.
- `cargo xtask lint arch / unused / docs` all ok.
- `cargo xtask progress validate`: 24 records ok.

## Commit ledger

- `f52a888` — `docs(progress): record vm/tty move deferral and root-cause finding`
- `8ebae8f` — `vfs: extend InodeMeta to full POSIX shape per bringup_fs_specs`
- `c17e791` — `vfs: reshape DirCursor to opaque [u8; 16] per TX_EXT4_PLAN_v1_2`
- `c5466a5` — `vfs: add MountOutput type per TX_EXT4_PLAN_v1_2`
- `<phase 1.5>` — `vfs: decompose into structure/checks/execution per SUBSYSTEM_ANATOMY_v2_1`
- `2e0d1f8` — `docs(progress): record VFS spec-reconciliation Phase 1 completion`
- `<phase 2>` — `tx-subsystems: bring skeleton into doc spec (Phase 2)`
- `1b91e5d` — `tx-ext4: port to canonical VFS surface (Phase 3)`
- `<phase 4>` — `collapse: move VFS/VM/TTY/page_backed/etc. into tx-subsystems (Phase 4)`

## Next step

The branch is ready to merge. Suggested follow-ups (separate work):
1. Land the HAL kernel direct-map helper so tx-ext4's
   `fetch_page` works outside `#[cfg(test)]`.
2. Split `page_backed.rs` (1473 lines, borderline) into the
   already-suggested submodule layout.
3. Continue the broader `MODULE_MAP_v1` cleanup: `process/`,
   `signal/`, `bdev_fs/`, `tmpfs/`, `procfs/` are still
   stubs/unbuilt.

## Blockers

None.
