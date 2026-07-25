# VFS Redundancy and Bad-Smell Fanout

Date: 2026-07-11
Scope: read-only audit of the current VFS, mount, filesystem backend, and
syscall-facing filesystem surfaces.

## Summary

The current VFS implementation is functional but carries several transitional
surfaces that now behave like architectural debt:

- the typed `require_*` witness facade is smaller than the active design
  contract and is not the dominant syscall-facing path;
- walker state is `Cap`-heavy and `parent_hint`-driven while the active design
  still describes an `IdentRef` plus `WalkTrail` model;
- mount namespace traversal falls back to a global mount table because mount
  publication into per-process namespaces is incomplete;
- mount topology is represented both in `MountNamespace` and in a global
  `MOUNT_TABLE`, with different registration semantics;
- syscall shims duplicate `dirfd` resolution, `FsOps` / `MountPayload`
  discovery, fd installation, and fd-kind dispatch;
- `OpenFileBacking` has grown into a broad fd bus while legacy `rnode()` access
  panics for non-VFS fd kinds, forcing fragile pre-dispatch in generic syscall
  paths.

The following splits should *not* be treated as redundancy: VFS versus Mount,
`FsOps` versus `FsPageBacking`, and `RNodeBacking` versus `OpenFileBacking`.
Those are intentional boundaries in the active docs. The debt is mostly in the
bridges and fallback paths between them.

## Findings

### 1. Typed witness facade is not the main path

Design intent: `docs/design/05_filesystem/VFS_CHECKS_V2.1.md` defines six
closed walker modes and a `require_*` facade layer for typed witnesses.

Live state: `crates/tx-subsystems/src/vfs/require.rs` currently exposes only
`require_entity`, `require_directory`, and `require_parent_and_name`.
`crates/tx-subsystems/src/vfs/composite.rs` still manually splits
parent/name, calls `walker::step_walk`, or calls `walk_to_completion`
directly for lstat/statx no-follow paths.

Risk: syscall-facing operations can bypass the witness surface and drift in
errno mapping, no-follow handling, and mount/FsOps lookup behavior.

### 2. Walker design model and live model coexist

Design intent: warm walks carry `IdentRef` state and use `WalkTrail`;
`Cap` is used to cross yields.

Live state: `crates/tx-subsystems/src/vfs/resolution/state.rs` stores
`Cap<DEntry>` in `WalkingState.current` / `mount_root`. `WalkTrail` still
exists in the same module, but ordinary `..` traversal in
`crates/tx-subsystems/src/vfs/resolution/step.rs` uses
`DEntry::parent_hint()`.

Risk: the code weakens the design's zero-refcount warm-walk claim and makes
rename / mount-boundary behavior depend on stale-tolerant parent hints rather
than an explicit walker-local trail.

### 3. Mount namespace isolation has a known global fallback

`crates/tx-subsystems/src/vfs/resolution/step.rs` first consults the supplied
`MountNamespace`, then falls back to `crate::mount::mount_for(...)`. The
comment says this is required because per-process namespace tables are
currently incomplete for bind/move mounts.

Risk: namespace-aware walkers can still observe global mounts. This is a
correctness ceiling for mount namespace isolation, not just a cleanup issue.

### 4. Mount topology has two live registries

The mount doc says `MountNamespace.mountpoint_index` is the authoritative
crossing binding. Live code has both `MountNamespace.mounts` and the global
`MOUNT_TABLE`. The namespace-local registration path upserts an existing
mountpoint entry, while global `register_mount` intentionally stacks entries
for LIFO mount semantics.

Risk: the same mount concept has two publication sites with different
semantics. `umount` already removes from both namespace and global tables to
avoid stale global fallback crossings, which confirms the dual registry is a
behavioral risk rather than harmless duplication.

### 5. Mount/backend output shapes are repeated

`MountOutput` exists in both VFS execution and Mount, while `FsOutput` repeats
the same backend-output shape plus `fstype`.

Risk: backend factory signatures can drift, and future filesystem workers may
pick the wrong output vocabulary when adding a backend or mount path.

### 6. Synthetic filesystems are forced to provide mostly-empty page backing

`MountPayload` unconditionally stores both `Arc<dyn FsOps>` and
`Arc<dyn FsPageBacking>`. Synthetic filesystems such as devfs, procfs, and
sysfs therefore provide `FsPageBacking` implementations that mostly return
`ENOSYS`.

Risk: the type shape says every mounted filesystem has a page-back backing even
when the backend is projection/device-only. This is not fatal, but it creates
boilerplate and makes real PageBacked capability harder to see mechanically.

### 7. bdevfs has two public backend shapes with different coherence semantics

Production boot uses the payload-backed bdevfs path with a devt-to-weak-PC
coherence index. A zero-state public `BdevFs` path still exists and explicitly
lacks that coherence index.

Risk: the design requires one PC per devt. The zero-state public backend is an
attractive wrong route for future callers because it exposes the same broad
filesystem interface while violating the intended coherence rule.

### 8. Device route drift leaves stale VFS vocabulary

The device doc describes pseudo-devices such as `/dev/null` and `/dev/zero` as
projected Route A entries. Current devfs materializes them as
`StructPayload::CharDevice`. `StructPayload::BlockDevice` also remains even
though the design says block devices should route through PageBacked bdev-fs;
VFS read/write/ioctl arms return `ENOSYS` for the struct-backed block-device
variant.

Risk: `StructPayload::BlockDevice` is stale vocabulary or an attractive wrong
route, while `/dev/zero` as a char-device route may affect future mmap /
projection semantics.

### 9. Mount/FsOps discovery is duplicated and inconsistent

Live helpers include:

- `walker::fs_ops_for`, `walker::fs_ops_for_rnode`, and
  `walker::mount_payload_for` in `crates/tx-subsystems/src/vfs/walker.rs`;
- `fs_ops_for_dentry` and mount-payload ascent helpers in
  `crates/tx-shims/src/linux_syscall/fs_path.rs`;
- `fs_page_backing_for_dentry` in `crates/tx-shims/src/linux_syscall/fs_mut.rs`;
- direct rnode weak lookup in
  `crates/tx-shims/src/linux_syscall/fs_basic/dir_sync.rs`;
- handle-specific payload lookup in
  `crates/tx-shims/src/linux_syscall/fs_handle.rs`.

The fallback errno differs by call site (`EROFS`, `ENOSYS`, `ENODEV`,
`ESTALE`), and `vfs/composite.rs` still has `expect("NoFsOps ...")` sites.

Risk: missing mount stamps may be reported as user-visible errno in one path
and panic in another.

### 10. Descendant mount stamping is partially fixed but comments are stale

`RNode::new_cap_in_mount` says descendant directory RNodes carry the
containing mount, and `materialise_child` does this for directories when a
`mount_payload` exists. However, shim comments still describe freshly
materialized child RNodes as lacking `with_containing_mount`, and
`getdents64` still contains a fallback that returns `ENOSYS` when an fd's
directory RNode lacks a mount weak.

Risk: current code and comments disagree about whether descendant directory
RNodes are mount-stamped. This is a maintenance trap around `getdents64`,
stat-family metadata, and handle APIs.

### 11. `OpenFileBacking` is a fd bus with panic-based legacy access

`OpenFileBacking` now includes VFS RNodes plus userfaultfd, AIO, signalfd,
epoll, io_uring, eventfd, timerfd, POSIX mq, pidfd, kernel-object, mount-api,
and socketpair fd kinds. `OpenFile::rnode()` panics for every non-RNode kind.

Risk: generic syscall paths must pre-dispatch every non-VFS fd kind before any
VFS-shaped access. `sys_read` / `sys_write` already contain long pre-dispatch
chains to avoid this footgun.

### 12. Syscall shim repetition is now visible architecture debt

Examples:

- `dirfd` / `AT_FDCWD` resolution appears in `fs_basic.rs`, `fs_path.rs`,
  `fs_mut.rs`, and `fs_handle.rs`;
- `sys_openat` repeats the allocate-fd / install-file / set-cloexec / return
  pattern across procfs namespace files, netns files, tmpfile, `/dev/tty`,
  simple open, and create/truncate open;
- `statfs` ignores path identity and `fstatfs` only checks fd existence before
  returning the same hard-coded layout;
- `STAT_META_OVERRIDES` is global side state, and `utimensat` records an
  override even when backend metadata serialization fails or is ignored;
- `sys_close` does best-effort page-backed `fsync` in the syscall shim and
  ignores errors.

Risk: Linux ABI behavior can drift by syscall cluster, and durability /
metadata policy is leaking into shims instead of living behind VFS/PageBacked
interfaces.

## Suggested Cleanup Order

1. Add a single syscall-local helper for fd installation and cloexec handling,
   then use it to simplify `sys_openat` special paths.
2. Add one canonical shim helper for `dirfd + path -> root dentry` and one
   canonical helper for `DEntry/RNode -> MountPayload/FsOps/FsPageBacking`,
   with one errno policy per caller class.
3. Collapse mount publication onto one authoritative namespace-index path, then
   remove the global fallback from namespace-aware walks.
4. Decide whether descendant directory RNodes are guaranteed to carry
   `containing_mount`. If yes, delete stale comments and fallback assumptions;
   if no, fix `materialise_child` / tests and make the limitation explicit.
5. Retire or privatize the zero-state bdevfs backend path so callers cannot
   bypass the devt-to-PC coherence index.
6. Expand `require_*` or intentionally de-scope it. Avoid leaving the typed
   witness facade and direct `walk_to_completion` calls as parallel public
   routes.
7. Replace panic-based `OpenFile::rnode()` use in generic fd paths with a
   non-panicking fd operation dispatch seam, or split VFS-open files from
   generic fd objects more explicitly.

## Verification

Read-only audit. No implementation tests were run.

Progress catch-up verification:

- `cargo xtask progress validate` passed.
- `git diff --check -- docs/progress/STATUS.md docs/progress/research/2026-07-11-vfs-redundancy-bad-smell-fanout.md`
  passed.
