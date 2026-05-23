# IPC and VFS integration audit

Date: 2026-05-22

## Scope

Initial audit plus follow-up implementation slices. The check compared the
live IPC and VFS/Mount/PageBacked code against these active specs:

- `docs/Txv3/08_SYSV_IPC_v1.md`
- `docs/design/00_meta-framework/NAMESPACE_VIEW_v1.md`
- `docs/design/05_filesystem/MOUNT_v1.md`
- `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`

Primary code surfaces inspected:

- `crates/tx-subsystems/src/process/nsproxy.rs`
- `crates/tx-subsystems/src/ipc/{namespace,sysv_shm,sysv_msg,sysv_sem,posix_mq}/`
- `crates/tx-shims/src/linux_syscall/ipc.rs`
- `crates/tx-subsystems/src/{mount,vfs,page_backed}/`
- `crates/tx-shims/src/linux_syscall/{fs_basic,fs_mut,fs_path}.rs`
- `crates/tx-fs/src/{tmpfs,devfs,procfs}/`
- `crates/tx-ext4/src/`
- `crates/tx-kernel/src/init*.rs`

## Verdict

Ready: mostly for the current host-tested SysV shm, POSIX mq, tmpfs/devfs/proc,
and ext4 mount/data-plane slices; not ready for the full `08_SYSV_IPC_v1` or
`MOUNT_v1` contracts.

The current tree has enough integration for basic musl-visible shm mappings,
SysV msg/sem nonblocking happy paths, POSIX mq fd dispatch, `/dev/shm`
tmpfs-backed POSIX shm path setup, and VFS/PageBacked filesystem access. The
spec-complete model still has hard gaps around blocking/waiter-abort semantics,
named-sem payload integration, mount-namespace-aware procfs projections,
MountNamespace ownership, lazy umount accounting, and the VFS witness/resume
model.

## Follow-up implementation

2026-05-22: The first audit slice closed the SysV msg/sem keyed
`IPC_RMID` namespace-withdrawal gap. `msgctl(IPC_RMID)` and
`semctl(IPC_RMID)` now route through nsproxy-aware helpers that remove stale
`IpcNamespace.sysv_msg` / `IpcNamespace.sysv_sem` entries after successful
removal, with syscall-dispatch regressions proving same-key recreate works.
The remaining `IPC_RMID` gaps in this note are waiter abort/cancellation and
blocking StepOp semantics, not keyed namespace-table withdrawal.

2026-05-22: The second audit slice closed POSIX mq namespace-scoped name
resolution. `mq_open` now resolves names through `IpcNamespace.posix_mq`,
`mq_unlink` withdraws only the caller's namespace binding, and the remaining
global POSIX mq table is an id-to-identity liveness registry for fd holders.
Subsystem regressions cover same-name creation across two IPC namespaces and
namespace-local unlink behavior.

2026-05-22: The third audit slice closed the `CLONE_NEWIPC` publication gap.
Default fork still shares the parent's namespace bundle; the new fork-options
path lets `sys_clone(SIGCHLD | CLONE_NEWIPC, ...)` publish a replacement
`NsProxy` whose `ipc_ns` cap points at a fresh empty `IpcNamespace` with
copied limits. `CLONE_THREAD | CLONE_NEWIPC` is rejected because IPC namespace
selection is process-scoped in the current model. The remaining namespace gap
is identity-table authority: IPC namespace maps still store ids while global
registries own object caps.

2026-05-22: The fourth audit slice made POSIX mq namespace entries
identity-cap authoritative. `IpcNamespace.posix_mq` now maps names directly to
`Cap<PosixMqIdentity>`, and `mq_open` reopens from that cap rather than
round-tripping through a namespace-stored mqid. The global mqid table remains
as a compatibility registry for existing fd/SysV-msg bridge code; the remaining
identity-table gap is now the SysV shm/msg/sem namespace maps.

2026-05-22: The fifth audit slice made SysV shm namespace entries
identity-cap authoritative. `IpcNamespace.sysv_shm` now maps keys directly to
`Cap<ShmSegmentIdentity>`, and `shmget` reuses the namespace-held segment cap
instead of resolving a stored shmid through the global table. The global shmid
table remains as a compatibility registry for id-based attach/stat paths and
delayed `IPC_RMID` detach cleanup. The remaining identity-table gap is now
SysV msg/sem.

2026-05-22: The sixth audit slice made SysV msg/sem namespace entries
identity-cap authoritative. `IpcNamespace.sysv_msg` and
`IpcNamespace.sysv_sem` now map keys directly to `Cap<MsgQueueIdentity>` and
`Cap<SemArrayIdentity>`. `msgget` and `semget` reuse the namespace-held caps
instead of resolving stored ids through global registries. The global msgid and
semid tables remain compatibility registries for id-based send/receive/control
paths. All current IPC namespace maps named in `08_SYSV_IPC_v1.md` are now
cap-authoritative in the day-1 `BTreeMap` form.

2026-05-22: The seventh audit slice wired SysV sem undo into process exit.
`step_exit_group` and the last-thread `step_process_exit` path now call
`step_sem_undo(process)` before the process payload is torn down, and the undo
walker applies the stored inverse deltas from `ProcessPayload.sem_undos`.
Forked children start with empty undo state, so the process-owned storage shape
matches the spec better than the earlier pid-keyed array-local map. This closes
the Linux-visible exit-restoration behavior for current semop callers.

2026-05-23: The eighth audit slice closed the SysV sem `GETALL`/`SETALL`
control gap for the syscall-visible path. `semctl(SETALL)` now copies a
userspace `unsigned short[]` into `SemCtlArg::All`, validates array length and
Linux `SEMVMX` range, updates all semaphore values under the array payload
lock, bumps `changed_seq`, and wakes `changed_channel`. `semctl(GETALL)` now
returns all values through `SemCtlResult::All` and the shim copies them back to
userspace.

2026-05-23: The ninth audit slice closed the procfs projection/data-plane gap
for SysV IPC and the POSIX mq fdinfo surface. `/proc/sysvipc/msg`,
`/proc/sysvipc/sem`, and `/proc/sysvipc/shm` now render live rows from the IPC
tables, `/proc/<pid>/fdinfo/<fd>` reports mq attributes for POSIX mq
descriptors, `semctl(GETPID)` tracks the last modifying pid, `RNodeBacking::Projected`
now carries a procfs schema/key pair, and mount parsing carries `NOSUID`,
`NODEV`, `NOEXEC`, and `NOATIME` with exec-time `NOEXEC` enforcement. Remaining
gaps are the blocking/waiter-abort shapes, `/dev/shm`, mount namespaces, lazy
umount, and the spec witness/resume model.

2026-05-23: The tenth audit slice wired the first production mount namespace
path. `NsProxy` now carries an optional `mnt_ns` cap, boot publishes the init
mount namespace after rootfs mount creation, fork inherits the bundle, and boot
/ syscall mount publication registers entries in the current `MountNamespace`
when present. The VFS walker now has a namespace-aware entrypoint whose mount
crossing consults the supplied namespace table instead of the legacy global
fallback. Remaining mount namespace gaps are open-file/FsContext mount caps and
payload pins, lazy umount/detached cwd semantics, full `..` boundary handling,
and mount-namespace-aware procfs rendering.

2026-05-23: The eleventh audit slice fixed the `cargo check -p tx-kernel`
compile blocker by keeping `Guard<'_>` and `MaterializedPagePin` out of the
`run_thread` future's async state. VM fault resolution now uses synchronous
resolve/materialize/publish helpers that drop guard and page-pin evidence
before returning a `WaitToken` to the async loop. The same slice mounted tmpfs
at `/dev/shm` during boot, publishing the mount to both the legacy mount table
and the init `MountNamespace`. POSIX shm now has the spec's normal VFS/tmpfs
path; named sem files are path-resolvable as `/dev/shm/sem.*` tmpfs files, but
the spec's `SemArrayPayload { nsems = 1 }` file payload reuse is still a
follow-up.

2026-05-23: The NsProxy wiring audit confirms the core view-layer shape is
present but not yet complete. `ProcessPayload` owns an atomic `NsProxy` slot;
fork inherits the immutable bundle; `CLONE_NEWIPC` publishes a replacement
bundle with a fresh empty `IpcNamespace` and copied limits; boot publishes the
init `MountNamespace` into the bundle after rootfs construction. The main drift
is now consumer-side: `NsProxy.mnt_ns` is bootstrap-optional rather than the
spec's always-present cap, `CLONE_NEWNS`/`unshare`/`setns` mount namespace paths
are still deferred, and several syscall helpers still call legacy `step_walk`
or `walk_from` wrappers instead of the namespace-aware process wrapper.

## IPC findings

### Aligned or mostly aligned

- `NsProxy` carries an `ipc_ns` field, and `IpcNamespace` has per-kind SysV key
  maps plus limits. Default fork shares the bundle, while
  `CLONE_NEWIPC` now publishes a fresh IPC namespace with copied limits. POSIX
  mq names plus SysV shm/msg/sem keys are namespace-authoritative identity caps
  in the current `BTreeMap` form; replacing those maps with `IndexTable`
  remains a substrate-shape follow-up, not an authority gap.
- SysV shm is the strongest slice. `shmget` creates a persistent PageBacked
  segment and stores the identity cap in `IpcNamespace.sysv_shm`; `shmat` maps
  it through VM, `shmdt` validates the VMA and decrements attach count,
  process exit sweeps per-address-space attaches, and `shmctl(IPC_RMID)` now
  withdraws keyed namespace bindings while preserving existing attachments
  until the last detach.
- SysV msg/sem syscall dispatch and musl LP64 control layouts have useful host
  coverage. Their namespace entries now store identity caps directly, so the
  ABI-facing constants/layouts and key-table authority are no longer the
  primary risks.
- POSIX mq has real fd-shaped descriptors via `OpenFileBacking::PosixMq`,
  descriptor flags, send/receive/getattr/setattr/notify dispatch, wait-source
  polling at the syscall layer, and epoll-readiness coverage.

### Blocking gaps against `08_SYSV_IPC_v1.md`

- SysV msg/sem `IPC_RMID` only partially match the namespace-withdraw plus
  waiter abort contract. Keyed namespace entries are now withdrawn on
  successful `msgctl`/`semctl(IPC_RMID)`, but the paths still do not abort all
  object waiters with the script-side `EIDRM` mapping.
- SysV msg/sem blocking is incomplete. `msgsnd`, `msgrcv`, and `semop` have
  wait channels and sequence counters, but blocking paths still return
  `EAGAIN` rather than yielding `OnWaitSource` with prepared, sequenced
  predicates.
- SysV msg uses a linear `Vec<Msg>` with search; the spec expects per-type
  buckets plus `queue_seq` for typed receive predicates.
- SysV sem `SEM_UNDO` now lives on `ProcessPayload.sem_undos` and is drained at
  process exit. Remaining gaps are waiter abort, blocking `semop`, and full
  `SEM_UNDO` coverage on more advanced edge cases.
- SysV sem control coverage is partial: `GETALL` and `SETALL` now round-trip
  through the syscall layer, but `GETNCNT`, `GETZCNT`, and `GETPID` are still
  zero stubs.
- POSIX mq name resolution now uses `IpcNamespace.posix_mq`, the namespace
  table stores identity caps, and `mq_unlink` is namespace-scoped. Remaining
  POSIX mq work is blocking/deadline semantics and projection/fdinfo, not name
  authority.
- POSIX mq blocking/timed send and receive loop in the syscall layer using
  readiness polling. That is useful, but it is not the typed StepOp +
  `WaitProtocol.deadline` shape the spec names.
- `/dev/shm` is now covered by a boot-time tmpfs mount and normal VFS/tmpfs
  file creation works for POSIX shm-style names plus `/dev/shm/sem.*` named-sem
  paths. Remaining named-sem work is the deeper payload integration: files
  should carry/reach a `SemArrayPayload` with `nsems = 1` instead of being only
  ordinary page-backed tmpfs files.
- `/proc/sysvipc/{sem,shm,msg}` and POSIX mq projection files are now wired at
  the procfs renderer level, but the broader procfs/projection model still has
  no mount-namespace-aware projection registry or fdinfo family beyond POSIX mq.

## VFS/Mount/PageBacked findings

### Aligned or mostly aligned

- `MountPayload` hosts `FsOps` and `FsPageBacking`, and
  `PageContainerKind::File` carries a `MountPayloadPin` plus `FsObjectId`. This
  matches the PageBacked file-identity direction.
- The runtime has usable mount publication through the global mount table.
  Boot mounts root tmpfs, devfs, procfs, bdev-fs, and ext4 `/musl`; the syscall
  path can mount tmpfs, vfat-as-tmpfs, devfs, proc, and ext4.
- The walker crosses registered mountpoints, and `RNode.containing_mount` lets
  fd and directory operations recover the current `FsOps`.
- tmpfs and ext4 both implement the `FsOps`/`FsPageBacking` shape. tmpfs is
  usable for regular-file PageBacked reads/writes/truncate/fsync; ext4 is
  mountable and can materialize file-backed `PageContainer`s for reads.

### Blocking gaps against `MOUNT_v1.md` and `VFS_CHECKS_V2.1.md`

- `MountNamespace` is now wired into `NsProxy` as a bootstrap-optional cap and
  syscall/boot mount publication can populate per-namespace mount tables.
  Remaining gaps: `Frame`/FsContext still does not hold cwd/root mount caps or
  payload pins, procfs mount rendering still snapshots the legacy global table,
  and some older VFS/script helpers still use the global fallback until their
  path-resolution wrappers accept a mount namespace.
- The NsProxy placement itself matches `NAMESPACE_VIEW_v1`: it is
  process-payload execution-context state and remains immutable after
  publication. The implementation intentionally deviates for boot by storing
  `mnt_ns: Option<Cap<MountNamespace>>`; that keeps early process construction
  working, but every steady-state syscall path should treat `None` as a boot
  scaffold rather than silently falling back forever.
- The VFS walker is synchronous and cap-based. `run_walker` and
  `resume_walker` are shells over `walk_to_completion`; `NeedIO` collapses to
  `EAGAIN` instead of carrying the spec's guard-scoped `IdentRef` state,
  `ResumeToken`, and revalidation protocol.
- The walker uses parent hints and a best-effort `mount_root` approximation
  for `..`. It does not implement `WalkTrail` mount-boundary markers,
  `DotDotResult::{Cross, StayAtRoot, DetachedFail}`, or detached-cwd semantics.
- `OpenFile` does not carry `mount: Cap<MountIdentity>` or a
  `MountPayloadPin`. That leaves the closed accounting model incomplete for
  open files and fd-table references.
- FsContext/cwd/root mount pins are not implemented. Process state stores cwd
  as a `DEntry`, but not the paired mount cap/payload pin required by
  `MOUNT_v1`.
- Lazy umount is not implemented as specified. `umount` removes the global
  mount-table entry synchronously; there is no namespace withdraw plus detached
  but payload-held state, and no wait-for-zero pin discipline.
- Mount flags are partially represented/enforced. `READ_ONLY`, `NO_ATIME`,
  `NOSUID`, `NODEV`, and `NOEXEC` exist and the syscall path parses the common
  Linux bits; exec enforces `NOEXEC`. Remaining gap: `NODEV` is not enforced
  at device-open/mknod boundaries.
- There is an intentional code/spec drift: `RNode.containing_mount` and backend
  `materialise_rnode` constructors are live and useful, while `MOUNT_v1` says
  backends must not construct RNodes and lists the RNode-to-MountPayload
  back-link as a Phase 2 deferral. This needs an architecture decision:
  either bless the implemented shape by updating the spec, or move RNode
  construction back into VFS.
- `RNodeBacking::Projected` in code is a bare variant, while
  `PAGE_BACKED_v1` specifies a projection schema plus key. procfs works through
  its current backend surface, but the projected-content abstraction is not
  spec-complete.
- ext4 still has backend completeness gaps: `flush_page`, `truncate`, and
  `fsync_file` return `ENOSYS`, so read paths are much stronger than writeback
  and durability paths.

## Implementation entry order

1. Close SysV msg/sem wait semantics: typed StepOps, prepared sequenced
   predicates, waiter abort on RMID, and blocking `semop`.
2. Wire named POSIX semaphore files under `/dev/shm/sem.*` to the
   `SemArrayPayload { nsems = 1 }` reuse model, and harden remaining syscall
   path helpers that still rely on global mount fallback.
3. Decide the Mount/VFS drift around backend `materialise_rnode` and
   `RNode.containing_mount`, then update either spec or code before more
   workers rely on both.
4. Finish MountNamespace ownership: add open-file/FsContext mount caps and
   payload pins, implement lazy umount/detached-root semantics, and migrate
   remaining global-fallback/procfs mountinfo consumers.
5. Convert the VFS walker toward the spec witness/resume model, including
   `WalkTrail` mount-boundary state and `..` semantics.
6. Fill the remaining projection-adjacent gaps: mount-namespace-aware procfs
   integration, `/dev/shm`, and any non-procfs `Projected` consumers that still
   need schema/key publication.

## Verification

- `cargo test -p tx-subsystems --lib ipc::sysv_shm::tests -- --nocapture`
  - Passed: 6 tests.
- `cargo test -p tx-shims --lib linux_syscall::tests::ipc_dispatch -- --nocapture`
  - Passed: 9 tests after follow-up slices.
- `cargo test -p tx-subsystems --lib ipc::sysv_msg::tests -- --nocapture`
  - Passed: 1 test after follow-up slices.
- `cargo test -p tx-subsystems --lib ipc::sysv_sem::tests -- --nocapture`
  - Passed: 1 test after follow-up slices.
- `cargo test -p tx-subsystems --lib process::tests::fork_with_clone_newipc_publishes_fresh_empty_ipc_namespace -- --nocapture`
  - Passed after follow-up slices.
- `cargo test -p tx-subsystems --lib process::tests::fork_inherits_mount_namespace_from_nsproxy_bundle -- --nocapture`
  - Passed after mount-namespace follow-up slice.
- `cargo test -p tx-subsystems --lib vfs::walker::tests::step_walk_uses_mount_namespace_table_before_global_fallback -- --nocapture`
  - Passed after mount-namespace follow-up slice.
- `cargo check -p tx-subsystems -p tx-shims`
  - Passed after mount-namespace follow-up slice.
- `cargo test -p tx-subsystems --lib mount:: -- --nocapture`
  - Passed after mount-namespace follow-up slice.
- `cargo test -p tx-subsystems --lib process::tests:: -- --nocapture`
  - Passed: 98 tests after SEM_UNDO follow-up slice.
- `cargo test -p tx-shims --lib linux_syscall::tests::mq_dispatch -- --nocapture`
  - Passed: 14 tests.
- `cargo test -p tx-subsystems --lib vfs:: -- --test-threads=1`
  - Passed: 40 tests, 11 ignored existing walker flake tests.
- `cargo test -p tx-subsystems --lib mount:: -- --nocapture`
  - Passed: 7 tests.
- `cargo test -p tx-kernel --lib init::tests::boot_smoke_walker_resolves_dev_shm_to_tmpfs_mount -- --nocapture`
  - Passed after `/dev/shm` tmpfs mount follow-up slice.
- `cargo test -p tx-kernel --lib init::tests::boot_smoke_dev_shm_accepts_posix_shm_and_named_sem_files -- --nocapture`
  - Passed after `/dev/shm` tmpfs mount follow-up slice.
- `cargo test -p tx-subsystems --lib fault_script -- --nocapture`
  - Passed after VM fault async-state compile fix.
- `cargo check -p tx-subsystems -p tx-shims -p tx-kernel`
  - Passed after VM fault async-state compile fix.
- `cargo check -p tx-subsystems`
  - Passed during the NsProxy wiring audit.
- `cargo test -p tx-subsystems fork_with_clone_newipc_publishes_fresh_empty_ipc_namespace`
  - Passed during the NsProxy wiring audit; one unrelated warning remains in
    `v3_signal_interrupt_wake.rs`.
- `cargo test -p tx-subsystems fork_inherits_mount_namespace_from_nsproxy_bundle`
  - Passed during the NsProxy wiring audit; one unrelated warning remains in
    `v3_signal_interrupt_wake.rs`.
- `cargo test -p tx-shims dispatch_clone_with_clone_newipc_publishes_fresh_ipc_namespace`
  - Passed during the NsProxy wiring audit.
- `cargo test -p tx-fs --lib tmpfs -- --nocapture`
  - Passed: 21 tests.
- `cargo test -p tx-ext4 --lib -- --nocapture`
  - Passed: 8 tests.

No guest QEMU IPC/VFS smoke was run during this audit.
