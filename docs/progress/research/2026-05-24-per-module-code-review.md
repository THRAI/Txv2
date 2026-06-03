# Per-module code review: smells and spec drift

Date: 2026-05-24

## Scope

This review used parallel read-only workers plus main-thread verification to
audit code smells and contract drift by architectural home. The review did not
implement fixes or lint rules. It compared live code against active specs and
classified only concrete, currently observable drift.

Primary spec anchors:

- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
- `docs/Txv3/03_STEP_MODEL_v2.md`
- `docs/Txv3/04_SYSCALL_SHAPE_v1.md`
- `docs/Txv3/08_SYSV_IPC_v1.md`
- `docs/Txv3/10_SCHED_SMP_v1.md`
- `docs/design/05_filesystem/MOUNT_v1.md`
- `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`
- `docs/design/05_filesystem/BDEV_FS.md`

Primary code surfaces inspected:

- `crates/tx-hal`, `boards/tx-hal-*`, `boards/tx-kernel-*`
- `crates/tx-substrate`, `crates/tx-reactor`, `crates/tx-kernel`
- `crates/tx-subsystems/src/{process,thread_runtime,signal,vm,page_backed,vfs,mount,tty}`
- `crates/tx-subsystems/src/{ipc,pipe,eventfd,signalfd,futex,epoll,timerfd,userfaultfd,aio,io_uring}`
- `crates/tx-fs`, `crates/tx-scripts`, `crates/tx-shims`

## Verdict

Ready: mostly for HAL/board separation, substrate/reactor boundary shape, and
many fd-like wake-source primitives; not ready for the full mount/VFS evidence
contract, full SysV blocking semantics, or the v3 closed yield catalog.

The highest-value follow-up is not broad cleanup. It is a short sequence of
contract-closing slices:

1. Mount/VFS evidence and topology: carry mount evidence through VFS witnesses,
   OpenFile, process frame/root/cwd state, and remove namespace-less global
   mount fallback.
2. SysV msg/sem blocking: replace blocking `EAGAIN` placeholders with
   `OnWaitSource` waits and `IPC_RMID` waiter abort to `EIDRM`.
3. v3 yield catalog: remove or explicitly defer `OnEdge`, and move `OnAgent`
   deadline handling out of the yield shape.
4. AIO borrowed-worker yield handling: stop collapsing lower-half `Yield` into
   short I/O or `EIO` for yield-capable fds.

## Findings

### P1 contract drift

| Area | Finding | Evidence | Spec anchor | Status | Lint candidate |
|---|---|---|---|---|---|
| VFS/Mount | VFS open/witness paths do not carry mount evidence. | `crates/tx-subsystems/src/vfs/checks.rs:29` has `EntityAtPath { dentry, rnode }`; `crates/tx-subsystems/src/vfs/structure.rs:991` `OpenFile` lacks mount and mount payload pin; `crates/tx-subsystems/src/process/structure.rs:920` `Frame` lacks cwd/root mount caps. | `txdoc:MOUNT-CROSS-DOC-EDITS-1`, `txdoc:MOUNT-BDY-2` | Live drift. | `mount-evidence-shape`: require mount fields on VFS witnesses/OpenFile and process frame/root/cwd context. |
| VFS/Mount | Mount topology still has a global fallback table parallel to `MountNamespace`. | `crates/tx-subsystems/src/mount/mod.rs:315` has namespace-local mounts; `crates/tx-subsystems/src/mount/mod.rs:495` has `static MOUNT_TABLE`; `crates/tx-subsystems/src/vfs/resolution/step.rs:346` can fall back to `crate::mount::mount_for` when no namespace is supplied. | `txdoc:MOUNT-THE-THREE-BOUNDARY-INVARIANTS-1`, `txdoc:MOUNT-BOUNDARY-TABLE-1` | Live legacy scaffold. | `mount-topology-owner`: reject non-test global mount topology and namespace-less mount crossing. |
| SysV IPC | Blocking SysV semaphore/message operations return `EAGAIN` instead of yielding. | `crates/tx-subsystems/src/ipc/sysv_sem/execution.rs:157`; `crates/tx-subsystems/src/ipc/sysv_msg/execution.rs:142`; `crates/tx-subsystems/src/ipc/sysv_msg/execution.rs:185`. | `txdoc:IPC-V1-SEM-1`, `txdoc:IPC-V1-MSG-1`, `txdoc:IPC-V1-LAYOUT-1` | Live drift; blocking and nowait paths collapse. | `ipc-blocking-yield`: flag non-`IPC_NOWAIT` branches returning `EAGAIN` in SysV/POSIX IPC blocking operations. |
| SysV IPC | `IPC_RMID` marks/withdraws sem/msg objects but does not abort parked waiters or map to `EIDRM`. | `crates/tx-subsystems/src/ipc/sysv_sem/execution.rs:223`; `crates/tx-subsystems/src/ipc/sysv_msg/execution.rs:253`. | `txdoc:IPC-V1-RMID-1`, `txdoc:IPC-V1-SEM-1` | Latent until blocking waits land, but part of the same contract gap. | `ipc-rmid-abort`: require RMID paths touching wait-capable payloads to publish abort/cancel. |

### P2 contract drift and high-value smells

| Area | Finding | Evidence | Spec anchor | Status | Lint candidate |
|---|---|---|---|---|---|
| Step model | `YieldShape::OnAgent` carries `deadline` even though deadlines belong to `WaitProtocol`. | `crates/tx-substrate/src/step/mod.rs:187`; active spec says deadlines are not yield-shape fields. | `txdoc:STEP-V2-YIELD-SHAPE-1` | Live drift. | `yield-shape-catalog`: reject unexpected fields on closed-catalog variants. |
| Step model | `YieldShape::OnEdge` exists and is admitted by driver classification while the active v3 landing defers it. | `crates/tx-substrate/src/step/mod.rs:211`; `crates/tx-substrate/src/step/mod.rs:561`; `docs/Txv3/07_BLAST_RADIUS.md` says no `OnEdge` in this landing. | `txdoc:STEP-V2-YIELD-SHAPE-1` | Premature catalog extension. | `yield-shape-catalog`: compare enum variants to active closed catalog, with explicit allowlist for deferred work. |
| VFS | VFS resume/driver surface is not spec-shaped. | `crates/tx-subsystems/src/vfs/resolution/driver.rs:172` maps errors into empty `Walking`; `driver.rs:198` resumes without `IOResult` and with `mount_namespace = None`. | `txdoc:VFS-CHECKS-THE-DRIVER-1` | Live drift. | `vfs-driver-shape`: require `resume_walker` to consume IO result and namespace context; flag public fake continue state. |
| VFS/Mount | Symlink RNodes are materialized without containing mount evidence. | `crates/tx-subsystems/src/vfs/resolution/step.rs:417` records `materialise:symlink-no-mount` before `RNode::new_cap`. | `txdoc:MOUNT-CROSS-DOC-EDITS-1` | Live drift for symlink/readlink-style paths. | `rnode-mount-materialization`: flag walker `RNode::new_cap` calls where mount payload is in scope but not used. |
| AIO/Shims | Borrowed AIO read/write collapses lower-half `Yield` into short I/O or `EIO`. | `crates/tx-shims/src/linux_syscall/aio.rs:234`; `aio.rs:285`. | `txdoc:SYSCALL-V1-SYS-READ-1`, `txdoc:SYSCALL-V1-SQE-READ-1`, `txdoc:SYSCALL-V1-PROPERTIES-1` | Live canary limitation for yield-capable fds. | `shim-yield-collapse`: flag `StepOutcome::Yield` mapped directly to errno or short I/O in borrowed-worker dispatch. |
| Bdevfs | Public zero-state `BdevFs` can materialize RNodes without the required devt-to-PageContainer coherence index. | `crates/tx-fs/src/bdevfs/mod.rs:212`; `bdevfs/mod.rs:790`. | `txdoc:BDEV-FS-ZONE-DERIVED-TYPE-POLICY-1`, `txdoc:BDEV-FS-SHAPE-1`, `txdoc:BDEV-FS-FSOPS-IMPLEMENTATION-1` | Live public convenience impl; production appears intended to use `BdevFsMountPayload`. | `bdevfs-production-surface`: forbid direct `FsOps for BdevFs` outside tests or mark it test-only. |

### P3 smells

| Area | Finding | Evidence | Spec anchor | Status | Lint candidate |
|---|---|---|---|---|---|
| Scheduler | `SchedClass` exposes RT/deadline/idle classes while the v1 SMP spec only admits `Normal` and `Kernel`. | `crates/tx-reactor/src/scheduler.rs:52`; `txdoc:SCHED-SMP-V1-DEFERRED-1`. | `txdoc:SCHED-SMP-V1-DEFERRED-1`, `txdoc:SCHED-5-PHASE-2-EXTENSIONS` | Dormant API surface, not observed as active policy. | `sched-deferred-class`: require deferred classes to be cfg/test-only or annotated. |
| Thread runtime | Stop path documents a busy-wait placeholder instead of a wait-source/reactor park. | `crates/tx-subsystems/src/thread_runtime/structure.rs:202`. | `txdoc:SIGNAL-THE-DELIVERY-SELECTION-ALGORITHM-1`, `txdoc:STEP-V2-YIELD-SHAPE-1` | Known staging smell. | `stale-scaffold`: flag busy-wait placeholder text in runtime/semantic paths. |

## Existing lint coverage

Existing gates already cover several relevant smell classes:

- `cargo xtask lint boundary` and `cargo xtask boundary-report` cover raw
  substrate/reactor access outside adapters. Current report: substrate outside
  adapters is 33 lines under the ceiling 34; reactor outside adapters is 0.
- `cargo xtask lint invariants all` covers step comments, stale v4 vocabulary,
  `.await` in step bodies, checks purity, witness scope, signal publish order,
  script boundary imports, guard storage, ad-hoc drives, syscall ad-hoc loops,
  syscall awaits, and syscall context bridge metrics.
- `cargo xtask lint docs` covers Markdown links, txdoc tag shape, stale doc
  vocabulary warnings, and Rust comment references to active txdoc tags.

Useful live ratchet observations from this audit:

- `subject-context` is still a ratchet, not semantic proof: `_ctx` appears 119
  times under ceiling 120.
- `no-adhoc-drive` is exactly at ceiling: 4 files, 19 sites.
- `syscall-adhoc-loop` is exactly at ceiling: 10 files, 84 sites.
- `syscall-ctx-bridge` is informational: 36/118 syscall functions are bridged
  to `ScriptCtx`; several families still sit at 0 bridged.
- `boundary-report` already computes adapter verb-ratio. Many adapters are
  alias-only, which is acceptable as a migration state but useful as a future
  `adapter-quality` ratchet once adapter ownership is stable.

## No-finding slices

The workers did not find concrete new drift in these sampled areas:

- HAL crate/board dependency direction, runtime HAL manager patterns, and
  board join points.
- Page allocator / `FrameMeta` substrate shape.
- Cross-hart reschedule bridge through `SmpIf`.
- Process topology basics, signal shim zone/entity ownership, core VM
  mmap/munmap/mprotect shape, VM RangeLock declared-range comments, and TTY
  typed session/pgrp direction.
- `eventfd`, `timerfd`, `pipe`, `futex`, `signalfd`, `userfaultfd`, and
  `epoll` wake-source primitives in this pass.
- `tmpfs`, `devfs`, `procfs`, `devpts`, `tx_ext4_bridge`, and `fat_bridge`.
- `tx-services`, `tx-policy`, and `tx-drivers` against the cited module/device
  role specs.
- `tx-scripts` exec path against the requested `EXEC_v1` and syscall-shape
  anchors.

## Dispatch fix follow-up

The first dispatched fix pass closed the concrete, non-premature items from the
musl-prioritized list:

- **SysV msg/sem blocking:** implemented v3 wait-source outcomes for blocking
  `msgsnd`, `msgrcv`, and `semop` when `IPC_NOWAIT` is absent. `IPC_RMID` now
  wakes the relevant wait channels and removed ids return `EIDRM` on retry.
  The remaining, intentionally deferred piece is typed
  `AbortReason::Canceled` delivery through v3 `Arc<WaitSource>` subscriber
  lists; current SysV payloads still own legacy channels/source ids.
- **Mount/VFS visible path boundary:** fixed `bootstrap_mount` to register the
  mount table entry with the parent mountpoint payload key used by the walker,
  rather than the child/source payload. Broader OpenFile/process cwd/root mount
  evidence and `..` traversal topology remain their own slice.
- **AIO yield preservation:** deferred. The native AIO worker dispatcher
  returns a terminal `IoEvent`, so preserving lower `StepOutcome::Yield` needs a
  continuation/requeue contract rather than a local `aio.rs` patch.
- **Premature architecture-only findings:** `YieldShape::OnAgent.deadline`,
  `OnEdge`, scheduler deferred classes, BdevFs convenience, and thread-stop
  parking were not changed in this fix pass.

## Recommended next slices

1. **Mount/VFS evidence closure.** Add mount evidence to VFS witnesses,
   OpenFile, process frame/root/cwd state, and symlink materialization; remove
   namespace-less global fallback once callers pass mount namespace/context.
2. **SysV drive/abort completion.** Drive the new msg/sem v3 wait outcomes from
   the syscall/script layer and upgrade SysV payload wait ownership to real
   v3 `WaitSource` subscriber lists so `IPC_RMID` can deliver typed
   `AbortReason::Canceled`, with the script layer mapping cancellation to
   `EIDRM`.
3. **Closed-catalog cleanup.** Align `YieldShape` with `03_STEP_MODEL_v2`:
   remove `OnAgent.deadline`, gate or remove `OnEdge`, and add a catalog lint.
4. **AIO yield preservation.** Make borrowed AIO lower halves preserve yield
   shape through driver handling instead of returning short I/O or `EIO`.
5. **Lint ratchets.** After each code slice, add or tighten the narrow lint
   that would have caught the fixed drift. Do not add broad regex gates before
   a concrete violation is closed.

## Verification

Read-only analysis and report writing used:

- `cargo xtask boundary-report`
- `cargo xtask lint invariants all`

Final Markdown/progress validation is recorded in `docs/progress/STATUS.md`.

## Musl convention cross-check

Follow-up cross-check against `external/musl` distinguishes musl-visible ABI
risks from architecture-only review findings. This does not erase the Tx spec
drift above; it only changes prioritization when the next goal is OSComp/musl
compatibility.

| Finding | Musl convention evidence | Musl-visible risk | False-positive assessment |
|---|---|---|---|
| SysV msg/sem blocking returns `EAGAIN` instead of waiting | `external/musl/src/ipc/msgsnd.c` and `msgrcv.c` call `syscall_cp(SYS_msgsnd/msgrcv, ...)`; `semop.c` calls `SYS_semop`; `semtimedop.c` calls `SYS_semtimedop` / `SYS_semtimedop_time64`. | High for applications that use blocking SysV msg/sem. Musl expects kernel blocking/cancellation-point behavior, not an immediate `EAGAIN` without `IPC_NOWAIT`. | Not a false positive. Keep as musl-relevant. |
| SysV `IPC_RMID` does not abort parked waiters to `EIDRM` | Musl exposes `IPC_RMID` through `msgctl.c`, `semctl.c`, and generic `include/sys/ipc.h`; waiters use the blocking syscalls above. | Conditional but real: it becomes visible once blocking waits are implemented and one thread removes a queue/sem while another is blocked. | Not a false positive, but second-order behind the blocking-wait implementation. |
| AIO borrowed-worker yield collapse | Musl POSIX AIO is user-thread based in `external/musl/src/aio/aio.c`: worker threads call normal `read`/`pread`/`write`/`pwrite`/`fsync`; `aio_suspend.c` waits on musl atomics/futexes. It does not drive Linux native `io_submit` for `aio_read`/`aio_write`. | Low for musl POSIX AIO. Still relevant for direct Linux AIO syscall tests/users (`io_setup`, `io_submit`) and Tx's Linux-AIO canary. | False positive if framed as a musl POSIX-AIO blocker; valid as Linux native AIO / Tx shim drift. |
| Mount/VFS evidence, topology, symlink mount evidence, VFS resume shape | Musl path APIs are thin syscall wrappers: `openat.c`, `execve.c`, `fexecve.c`, `execvp.c`, and `mount.c`. `shm_open.c` and `sem_open.c` map names under `/dev/shm`. | Conditional. Musl cares about Linux-visible path, `/proc/self/fd`, `/dev/shm`, mount flags such as `NOEXEC`, and namespace/mount behavior when tests exercise them; it does not observe Tx's internal witness shape directly. | Not a false positive for Tx architecture. Musl priority should be tied to concrete path/mount tests, not the internal evidence refactor alone. |
| `YieldShape::OnAgent.deadline` and premature `OnEdge` | No musl libc wrapper depends on Tx's internal `YieldShape` enum. | None directly. It can affect future FUSE/userfaultfd/epoll-edge style Linux features, but not ordinary musl libc behavior. | Architecture-only finding. False positive if treated as musl compatibility priority. |
| Scheduler exposes deferred RT/deadline classes | Musl `sched_setscheduler.c` / `sched_getscheduler.c` currently return `ENOSYS`, while pthread scheduling paths may still issue `SYS_sched_setscheduler` when explicit scheduling attributes are requested. | Low unless tests request pthread explicit scheduling or scheduler syscalls. Internal `SchedClass` enum shape is invisible to musl. | Mostly architecture-only. Keep as low-priority smell, not musl blocker. |
| Thread stop busy-wait placeholder | Musl has signal constants and signal wrappers, but the finding concerns Tx internal stop parking. | Conditional: visible only under stop/continue job-control style tests or signal-heavy workloads. | Not a false positive, but not a generic musl libc priority. |
| Zero-state `BdevFs` convenience impl | Musl does not know the backend type; it observes block-device/file behavior through mounted filesystems and syscalls. | Low unless the zero-state backend is mounted in an OSComp path and causes incoherent page-cache behavior. | Architecture/backend smell. False positive if prioritized as libc-facing without a concrete mounted path. |

Revised musl-facing priority:

1. SysV msg/sem blocking semantics, then `IPC_RMID` waiter abort.
2. Mount/VFS only where a Linux-visible test needs it: `/dev/shm`, procfd
   `fexecve`, symlink traversal across mount boundaries, `NOEXEC`/`NOSUID`, or
   mount namespace behavior.
3. Direct Linux native AIO syscall behavior, not musl POSIX AIO.
4. Treat `YieldShape` catalog, scheduler class surface, BdevFs convenience, and
   thread stop parking as architecture/staging cleanup unless a guest test
   names them.
