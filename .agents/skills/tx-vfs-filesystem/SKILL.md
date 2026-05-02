---
name: tx-vfs-filesystem
description: Use when implementing or auditing VFS, Mount, PageBacked filesystem interfaces, bdev-fs, devfs, filesystem backends, kernel-facing ext4 integration, or filesystem/device fanout planning.
---

# tx-vfs-filesystem

Use this skill before filesystem implementation or fanout. Its job is to keep
workers from inventing incompatible VFS, Mount, PageBacked, and backend trait
spellings.

## Read First

- `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`
- `docs/design/05_filesystem/MOUNT_v1.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- `docs/design/05_filesystem/BDEV_FS.md`
- `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md`
- `docs/design/06_devices/DEVICE.md`
- `docs/design/03_memory-vm/VM_v1_2.md`
- relevant `docs/progress/research/` and worktree records

## Preserve

- VFS owns `DEntry`, `RNode`, path walking, witnesses, open-file shape, and
  syscall-facing checks. Filesystem backends must not own VFS live nodes.
- Mount owns mount topology and `MountPayload`; it hosts backend traits but is
  not the VFS walker, page cache, or block driver.
- PageBacked owns `PageContainer`, page-cache identity, file-backed
  materialization, and uniform read/write/mmap/truncate/fsync behavior.
- bdev-fs maps block devices into PageBacked raw-device file semantics; it does
  not create a new semantic entity class.
- devfs exposes device RNodes over static device bindings. Do not introduce
  tier-3 dynamic discovery unless the scope explicitly says so.
- Kernel-facing ext4 consumes canonical `FsOps`, `FsPageBacking`, block-device,
  mount, and PageBacked traits. Keep host ext4 work free of VFS/VM imports.
- Broad VFS/PageBacked runtime implementation waits on VM `AddressSpace` and a
  shared interface seam. Before that, limit work to scouts, type inventories,
  or interface-only patches.

## Implementation Harness

- First produce or read an interface-readiness note naming canonical type/trait
  spellings and lane boundaries.
- Split future workers by owner: VFS walker/RNode, Mount, PageBacked, bdev-fs,
  devfs/device, kernel-facing ext4, and ext4 host-format.
- Do not let two workers edit shared traits or `docs/progress/STATUS.md`.
  Coordinator owns shared status catch-up.
- Any implementation lane must state which docs it implements and which
  cross-subsystem seams remain deferred.

## Checks

- `cargo fmt --check`
- targeted crate tests for the touched subsystem
- `cargo xtask progress validate`
- `cargo xtask lint docs` when docs are touched
- `git diff --check`

Use static greps to keep `tx-ext4-format` free of kernel-interface imports and
production `tx-ext4` free of Tokio unless the architecture changes.
