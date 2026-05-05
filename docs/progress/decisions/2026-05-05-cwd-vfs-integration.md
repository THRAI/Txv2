# cwd integration — `step_chdir` / `step_getcwd` + PROCESS_v1 §3 amendment

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from waitpid pgrp selectors)
**Status:** Complete. CI green (11 gates). 329 tests pass (322 + 7 new).

## Goal

Wire VFS integration into the process subsystem — specifically the
`getcwd(2)` / `chdir(2)` surface — so processes carry a current
working directory. This is the first VFS↔process seam: it crosses
subsystem boundaries (process holds a `Cap<DEntry>`, VFS owns
DEntry/RNode entities and the path-render walk) without yet
requiring a bootstrapped rootfs.

This pass landed Option A from the cwd-integration scope discussion:
process-side cwd surface only; rootfs bootstrap, fd_table, and
`Shared<T>` sharing for CLONE_FS deferred.

## What landed

### VFS additions

**`InlineName::ROOT` constant** ([vfs/structure.rs](../../../crates/tx-subsystems/src/vfs/structure.rs)).
The public `InlineName::new` constructor rejects empty bytes; the
root dentry needs an empty-name marker so path-render code can
recognise "this is the root, emit `/` and stop here." Added a
`pub const ROOT: Self` with `len = 0` — bypasses the `new` validator
since this is the only legitimate empty-name use.

**`vfs::render_dentry_path(dentry) -> Option<Vec<u8>>`**
([vfs/structure.rs](../../../crates/tx-subsystems/src/vfs/structure.rs)).
Walks a `Cap<DEntry>`'s `parent_hint` chain to the root, accumulating
`InlineName`s leaf-to-root. Reverses, joins non-root components with
`/`, prepends a leading `/`. Returns `None` if any intermediate
`parent_hint` Weak fails to upgrade — the chain is broken (cwd has
been unlinked); POSIX `getcwd(2)` maps this to `ENOENT`.

**`DEntry::parent_hint(&self)`** accessor — returns `Option<Weak<DEntry>>`
for the parent walk. Previously `parent` was private with only a
setter; the path-render helper needs the read.

### PageContainer `Send + Sync`

Adding `Cap<DEntry>` to `ProcessPayload` (and through it to the
`INIT_PROCESS` static) broke the Send/Sync chain because
`PageCacheEntry` carries an internal `*const ()` cache pin that
makes `Cap<PageContainer>` (transitively `Cap<RNode>`, `Cap<DEntry>`)
non-Send. Fixed with `unsafe impl Send + Sync for PageContainer`
mirroring the existing `AddressSpace` and `TtyPayload` precedent —
zone-allocated entities whose internal mutable state is covered by
`SpinMutex` and atomic fields can safely cross hart boundaries via
their `Cap` handles.

### Process additions

**`cwd: SpinMutex<Option<Cap<DEntry>>>` on `ProcessPayload`**
([process/structure.rs](../../../crates/tx-subsystems/src/process/structure.rs)).
`None` until installed via `step_chdir` — `bootstrap_init_process`
leaves it empty until a rootfs lands.

**`ProcessPayload::cwd()` accessor** — snapshots the Cap (clone) so
callers don't hold the lock.

**`step_chdir(target, new_cwd) -> ChdirOutcome`**
([process/execution.rs](../../../crates/tx-subsystems/src/process/execution.rs)).
Replaces the cwd slot. Returns `Replaced { prev }` carrying the
previous cwd (if any) or `ZombieIgnored` if the target has no
payload. Path resolution stays in the syscall driver — this step
takes a pre-resolved `Cap<DEntry>`.

**`step_getcwd(target) -> Option<Vec<u8>>`** — delegates to
`vfs::render_dentry_path`. Returns `None` for zombies, no-cwd-set
processes, and broken parent chains (uniformly ENOENT-equivalent).

**Fork inheritance** — `step_fork` snapshots `parent_cwd` alongside
`parent_aspace` and `parent_cred`, threads it through
`sign_process_payload`. Child gets the same `Cap<DEntry>` — refcount
increment, no copy of the dentry chain. POSIX semantics: fork copies
the cwd reference; `chdir` after fork affects only the calling
process.

### Tests (7 new)

In [process/tests.rs](../../../crates/tx-subsystems/src/process/tests.rs),
using synthetic DEntry chains constructed via `fresh_root_dentry()`
+ `fresh_dentry_under(parent, name, fs_id)` helpers (which mint
small anon `PageContainer`s for the `RNode` backing — content
doesn't matter for cwd-render tests):

- `getcwd_on_process_with_no_cwd_returns_none` — bootstrap init has
  cwd=None; getcwd returns None.
- `chdir_then_getcwd_renders_root_path` — chdir to a root DEntry;
  getcwd returns `b"/"`.
- `chdir_then_getcwd_renders_nested_path` — chain `/` → `/usr` →
  `/usr/bin`; chdir to leaf; getcwd returns `b"/usr/bin"`.
- `chdir_returns_previous_cwd_in_replaced` — second chdir returns
  the first as the `Replaced { prev }` value.
- `chdir_on_zombie_returns_zombie_ignored` — chdir on a zombie is
  a no-op.
- `fork_inherits_parent_cwd` — child's getcwd renders to the same
  path as parent's at fork time.
- `parent_chdir_after_fork_does_not_affect_child` — parent moves to
  `/var`; child keeps `/usr` (separate Cap retainer; no shared
  slot).

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §3 Frame.cwd / Frame.root field exists | ❌ flat-fields, no Frame | ⚠ partial (cwd added directly on ProcessPayload; full Frame container deferred) |
| `Cap<DEntry>` shape per VFS ResolveCtx | n/a | ✓ |
| getcwd path-rendering (§3 deferred work) | ❌ | ✓ via `vfs::render_dentry_path` |
| chdir step | ❌ | ✓ `step_chdir` |
| fork copies cwd | ❌ | ✓ |
| CLONE_FS sharing (§3 Shared<FsContext>) | ❌ | ❌ deferred |
| chroot via Frame.root | ❌ | ❌ deferred (no Frame container yet) |
| §3 spec-impl alignment on cwd type | spec said `Cap<RNode>` | ✓ amended to `Cap<DEntry>` |

### PROCESS_v1 §3 amendment

The spec previously declared `cwd: Cap<RNode>` and `root: Cap<RNode>`
on `Frame`. This was a simplification that lost the named-path edge
needed for `getcwd(2)` rendering. VFS's `ResolveCtx` already takes
`Cap<DEntry>` for cwd-bound resolution; PROCESS aligns with that
shape. Amended in [PROCESS_v1.md §3](../../design/04_process-signals/PROCESS_v1.md)
with a v1.2 note. The DEntry's contained `Cap<RNode>` is reachable
via `dentry.rnode()` for code paths that only care about the inode
identity.

## Deliberately deferred

- **Full `Frame` container.** Spec §3 wraps cwd/root/vm/fd_table/
  sig_actions/fs_context/umask in a single `Frame` struct on
  `ProcessPayload`. Day-1 has flat fields. The container becomes
  meaningful when `Shared<T>` sharing for CLONE_VM / CLONE_FILES /
  CLONE_SIGHAND / CLONE_FS lands; currently each clone is a copy.
- **`Frame.root` (chroot boundary).** No `step_chroot` yet — the
  per-process chroot scope isn't enforceable without a Frame
  container that VFS resolution can consult.
- **Path-string resolution.** `step_chdir` takes a `Cap<DEntry>`,
  not a path string. The syscall driver resolves the path
  externally (via `vfs::resolve` with the caller's current
  `ResolveCtx`) and feeds in the resulting DEntry. POSIX
  `chdir("/usr")` lands when the syscall surface lands.
- **Bootstrapped rootfs.** Init's cwd starts as `None`. No root
  filesystem is mounted at boot. Real userspace exec lands a
  synthesized `/` DEntry → root RNode pair; until then, all cwds
  are test-supplied.
- **`fchdir(2)`** — same shape as `step_chdir` but takes an fd
  rather than a path; lands with the fd_table.
- **Symlink-aware path render.** `vfs::render_dentry_path` walks
  parent_hints unconditionally. POSIX `getcwd` doesn't actually
  resolve symlinks (returns the literal path components), but a
  future `realpath`-style helper would.
- **CLONE_FS / `Shared<FsContext>`.** Day-1 fork copies the cwd Cap
  (refcount); the `prctl(PR_SET_FS, ...)` / clone-with-CLONE_FS
  shape lands with the Frame container.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 329
  tests pass (322 prior + 7 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `vfs: add InlineName::ROOT + render_dentry_path; PageContainer Send+Sync`
- `<this commit>` — `process: cwd: Option<Cap<DEntry>> on ProcessPayload + step_chdir + step_getcwd (PROCESS_v1 §3, amended)`
- `<this commit>` — `process: step_fork copies parent's cwd into child`
- `<this commit>` — `docs: PROCESS_v1 §3 cwd shape amended Cap<RNode> → Cap<DEntry>`
- `<this commit>` — `docs(progress): record cwd integration landing`

## Branch summary so far

`process-topology` now carries 19 commits. The day-1 process subsystem
checklist:

- ✓ Topology + identity/payload split
- ✓ Signal day-1 (Gewalt + catchable + ast_dispatch)
- ✓ Cred + permission check
- ✓ TTY pgrp typed dispatch + foreground-pgrp homing (P3)
- ✓ Children container + parent binding bidirectional
- ✓ SIGCHLD producer + reparent-to-init
- ✓ `step_waitpid_nohang` reaping (full POSIX selector coverage)
- ✓ Boot wiring (pid=1 globally addressable)
- ✓ §8.3 session-leader-tty hangup cascade
- ✓ **cwd / chdir / getcwd**

## Next step

Remaining items from prior next-step lists:

1. **`SigInfo` carrier** — independent signal subsystem work.
   SIGCHLD/SIGHUP currently post without `si_pid` / `si_code` /
   `si_status`.
2. **§8.2 orphan-pgrp SIGHUP** — needs stop-state machinery.
3. **Blocking `waitpid`** — needs reactor channel integration.
4. **First-userspace task submission** — init has no executing
   thread; blocked on EXEC_v1 + userspace-stub.
5. **Bootstrapped rootfs.** First step toward a real path-resolution
   surface — a synthesized `/` DEntry tree at boot that init's cwd
   can point at, and that enables the syscall-driver
   `chdir(path_str)` shape.
6. **`Frame` container.** Wraps cwd/root/aspace/fd_table/sig_actions
   into one struct; enables CLONE_FS / CLONE_VM / CLONE_FILES /
   CLONE_SIGHAND sharing.

## Blockers

None.
