# VFS ResolvedTarget and MountedDentry Change Scope

Date: 2026-07-11
Scope: read-only fanout on the first two non-RCU VFS cleanup abstractions:
`ResolvedTarget` / path-resolution helper shape, and `MountedDentry` /
`MountedNode` backend-discovery helper shape.

## Summary

The first implementation slice should keep `ResolvedTarget` syscall-shim local.
The repeated behavior lives in `tx-shims`: user path copy, `dirfd` anchoring,
empty-path handling, follow versus no-follow behavior, parent/name splitting,
and syscall-specific errno mapping. VFS core already exposes enough walker and
open surfaces for this slice.

`MountedDentry` has a wider ownership question. A shim-local helper is the
lowest-risk first step for syscall cleanup, but the underlying concept is a VFS
boundary: `DEntry/RNode -> MountPayload -> FsOps/FsPageBacking`. VFS also has
direct-only helpers and panic sites today. The recommended staged approach is:

1. introduce a shim-local mounted-dentry helper to collapse duplicated syscall
   lookup logic;
2. avoid fd-only `MountedNode` claims for descendant RNodes until the mount weak
   stamping contract is settled;
3. after the syscall helper is proven, promote the concept into VFS if
   `vfs::composite` and walker helpers are being cleaned in the same phase.

## Affected Surfaces

### Path and target resolution

- `crates/tx-shims/src/linux_syscall/fs_basic.rs`
  - `dirfd_anchor_for_path` is the best existing seed for the common anchor
    helper.
  - `sys_openat` combines dirfd anchoring, `O_NOFOLLOW`, `O_CREAT`
    parent/name, `O_TMPFILE`, and `O_TRUNC`.
  - `sys_statx` and `sys_newfstatat` repeat fd/empty-path/follow behavior.
- `crates/tx-shims/src/linux_syscall/fs_path.rs`
  - `resolve_cwd`, `resolve_path_at`, and path-mode syscalls repeat dirfd
    anchoring plus walk behavior.
  - `sys_fchmodat`, `sys_fchownat`, and `sys_faccessat2_impl` are low-risk
    consumers after the helper exists.
- `crates/tx-shims/src/linux_syscall/fs_mut.rs`
  - `resolve_cwd_for_path` duplicates dirfd anchoring and ignores its `_path`
    parameter.
  - `split_path` and `create_then_walk` are the parent/name seed.
  - `mkdirat`, `unlinkat`, `symlinkat`, `linkat`, `mknodat`, `utimensat`, and
    `renameat2` repeat parent/name resolution and mounted FsOps lookup.
  - mount API syscalls can use the same anchor helper, but root/mountpoint
    special cases should stay mount-specific.
- `crates/tx-shims/src/linux_syscall/fs_handle.rs`
  - `resolve_path_target` is already a local mini-`ResolvedTarget`, including
    empty-path, follow, and no-follow lookup behavior.

### Mounted backend discovery

- `crates/tx-shims/src/linux_syscall/fs_path.rs`
  - `fs_ops_for_dentry` ascends `parent_hint` and maps missing payload to
    caller-specific errno.
  - `mount_payload_for_dentry` is intended to ascend but currently stops if the
    current RNode has no weak; this differs from `fs_ops_for_dentry`.
- `crates/tx-shims/src/linux_syscall/fs_mut.rs`
  - `fs_page_backing_for_dentry` repeats the parent-hint ascent logic.
  - `mount_is_read_only` and `mount_identity_for_dentry` layer extra mount
    policy/topology lookup on the same boundary.
- `crates/tx-shims/src/linux_syscall/fs_basic/dir_sync.rs`
  - `fs_ops_for_rnode` only works when the fd's RNode directly carries
    `containing_mount_weak`, so descendant-directory behavior remains limited.
- `crates/tx-shims/src/linux_syscall/fs_handle.rs`
  - `payload_for_rnode` and `mount_payload_from_fd` repeat direct RNode weak
    lookup and map failures differently.
- `crates/tx-subsystems/src/vfs/walker.rs` and
  `crates/tx-subsystems/src/vfs/composite.rs`
  - VFS has direct-only `fs_ops_for`, `fs_ops_for_rnode`, and
    `mount_payload_for` helpers; composite call sites still have panic-style
    `expect("NoFsOps ...")` paths.

## Recommended Slices

### Slice 1: shim-local `ResolvedTarget`

Add a helper module under `crates/tx-shims/src/linux_syscall/` that centralizes:

- `dirfd` to anchor dentry;
- path copy result consumption, but not user-copy itself unless call sites are
  being migrated in the same patch;
- full entity resolution;
- parent/name resolution;
- no-follow terminal lookup for symlink-sensitive syscalls;
- empty-path fd target for syscall families that allow it.

Do not create a VFS-core `ResolvedPath` type in this slice because
`vfs::resolution::PathResolution` already exists and has a different meaning.

### Slice 2: shim-local `MountedDentry`

Add one helper shape that carries a dentry plus its in-scope `MountPayload` and
exposes `fs_ops()` / `fs_page_backing()` accessors. Use explicit constructors
for the supported policy:

- direct-only, for paths where a missing weak is a real stale object;
- parent-hint ascent, for current syscall dentry call sites;
- fd/RNode direct lookup, only where callers already require direct mount weak.

Keep syscall-specific errno mapping at call sites or in small adapter methods.
Do not claim fd-only descendant RNodes are solved by this slice.

### Slice 3: VFS promotion, only after Slice 2 proves useful

If the shim-local helper removes duplication cleanly, promote the mounted
boundary into VFS and use it to replace direct-only walker helpers and
`vfs::composite` panic sites. That phase should also decide the descendant
directory mount-stamping contract.

## Verification Plan

Narrow host tests for the first implementation pass:

- `cargo test -p tx-shims --lib fd_ops_wave2 -- --nocapture --test-threads=1`
- `cargo test -p tx-shims --lib stat_family -- --nocapture --test-threads=1`
- `cargo test -p tx-shims --lib file_mutation -- --nocapture --test-threads=1`
- `cargo test -p tx-subsystems --lib vfs::walker -- --nocapture --test-threads=1`
- `cargo test -p tx-fs tmpfs_ -- --nocapture --test-threads=1`
- `cargo test -p tx-ext4 ext4_v3_ -- --nocapture --test-threads=1`

Coverage gaps to close if implementation touches those areas:

- syscall-level `fsync`, `fdatasync`, and `syncfs`;
- `name_to_handle_at` and `open_by_handle_at`;
- `mknodat`;
- non-root directory cases for `getdents64`, `fsync`, `syncfs`, and handle
  APIs;
- cross-mount negative tests for `renameat2` / `linkat` if mounted identity is
  made explicit.

## Hazards

The current worktree is dirty across the exact files this work would touch:
`fs_basic.rs`, `fs_mut.rs`, `fs_path.rs`, `fs_handle.rs`, `dir_sync.rs`, and
several VFS files. Treat existing edits as user-owned and keep the first patch
small.

`OpenFile::rnode()` still panics for non-VFS fd kinds. Any fd-side mounted-node
helper must be called only after fd-kind dispatch has proven the file is VFS
backed.
