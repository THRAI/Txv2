# Decision: VFS at-Walker Integration

**Date:** 2026-05-24

## Decision

- Syscall modules should route `*at` path resolution through a common dirfd
  anchor helper plus the VFS walker, not each open-code `AT_FDCWD`-only checks.
- `AT_FDCWD` resolves to the process cwd. A real nonnegative dirfd resolves
  through the fd table and must carry a directory dentry; closed fds return
  `EBADF`, and non-directory fds return `ENOTDIR`.
- The VFS resolution driver owns `WalkMode::ParentAndName` semantics: parent
  walks stop at the penultimate component and return the parent dentry for
  mutation steps such as `mkdirat`.
- Syscall-facing consumers use `ResolveRequest` plus `drive_resolve` /
  `try_resolve_now`. The old synchronous walker remains available only as a
  VFS-internal fast path and compatibility implementation detail.
- Filesystem backends implement component operations (`lookup`, `read_link`,
  `load_inode_meta`, mutation hooks) and must not receive whole syscall paths
  or decide dirfd, mount namespace, symlink, root, or empty-path policy.

## Context

- The semi-thue VFS resolver is now functional enough for parent walks, and the
  fd table already carries resolved dentries on open files.
- The old syscall-side pattern rejected most real dirfds before reaching the
  walker, which made future `*at` families duplicate policy and drift from
  Linux/POSIX dirfd behavior.
- `cargo xtask lint invariants vfs-path-interface` now prevents production
  syscall code from reintroducing direct `step_walk` / `walk_from` /
  `walk_to_completion` use or stale `AT_FDCWD`-only comments outside the
  resolver facade.

## Consequences

- New `*at` syscall work should take an explicit dirfd anchor, then call the
  VFS walker for path behavior. Mutation syscalls that need a parent/name pair
  should consume `WalkMode::ParentAndName` rather than splitting and resolving
  parent paths independently.
- Existing legacy `*at` arms can be migrated incrementally. `mkdirat` is the
  first syscall in this slice to accept real directory fds; the follow-up slice
  migrated `openat`, stat/chmod/chown/access, mutation, readlink, rename, and
  utimens-style callers onto the same facade.
- Absolute paths intentionally ignore invalid real dirfds after anchoring from
  the process cwd/mount namespace, matching Linux's high-level behavior while
  preserving the current walker root model.

## Alternatives Considered

- Keep per-syscall `AT_FDCWD` gates: rejected because it preserves the current
  consumer-side VFS integration drift and blocks xattr/stat/chmod/chown `*at`
  work.
- Teach every syscall to inspect open files directly: rejected because fd
  semantics belong in one adapter helper and path semantics belong in the VFS
  walker.
