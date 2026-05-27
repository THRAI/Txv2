# Trio — Trap Shell + Syscall Table + tmpfs/devfs

Status: proposed (planning only). Companion to
`docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json`. Three
parts share boundaries (trap-frame ↔ syscall ABI ↔ rootfs/console RNode) and
must be planned together.

## Goal

A static-linked init binary, when produced, can: take a syscall trap on
RV64; have its trap frame handed to the reactor's `UserspaceRunSlot`;
resolve through the syscall dispatch table to a series of `step_*`
calls; `write(1, "hello\n", 6)` reaches `/dev/console` via devfs →
`tty::execution::step_write`; and `exit_group(0)` releases the
process and returns the userspace-run wait. tmpfs is mounted at `/`
in `init.rs`, devfs is mounted at `/dev` after TTY hardware is
registered, and `/dev/console` resolves to the registered hardware
TTY. "Done" is demonstrated by a kernel-side smoke test that
synthesises a `UserspaceTrapInfo::Syscall` (no real userland
binary), runs the dispatch loop, and observes the bytes appear on
`tx_hal::console_write_str` and the process transition to zombie
exit-status `0`. ELF loading and a real first userspace binary
remain out of scope.

## Doc anchors

- `txdoc:THREAD-4-1-SHAPE`, `THREAD-4-2-OWNERSHIP`,
  `THREAD-5-2-THE-INTERRUPT-SUMMARY`, `THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
  (`docs/design/02_execution/THREAD_RUNTIME_v1.md`) — `ThreadPayload` is
  the home for saved user registers and signal state; reactor owns the
  task-future, payload owns the policy-shaped fields.
- `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`,
  `REACTOR-PREEMPTION-TRANSPARENCY`, `REACTOR-AST-ASYNCHRONOUS-TRAP`
  (`docs/design/02_execution/REACTOR_v0.md`) — userspace execution is a
  reactor wait resolved by a trap; AST drains gate userspace re-entry.
- `txdoc:VM-5-1-FAULT-HANDLER` (`docs/design/03_memory-vm/VM_v1_2.md`) —
  the canonical fault script (`AddressSpace::fault_script`) we route
  page faults into.
- `txdoc:VM-5-8-BRK` — `brk_script` is the back-end of the `brk` syscall.
- `txdoc:PROCESS-WHAT-THIS-DOCUMENT-PINS-1`,
  `PROCESS-RELATIONSHIP-OTHER-SUBSYSTEMS-1`
  (`docs/design/04_process-signals/PROCESS_v1.md`) — `step_exit_group`
  and the cwd / aspace / pgrp seams.
- `txdoc:SIGNAL-V1` §3 (mask) and §15.1 (default actions) — `rt_sigprocmask`
  and `rt_sigaction` shape (`docs/design/04_process-signals/SIGNAL_v1.md`).
- `txdoc:VFS-CHECKS-WALKER-MODES-1`,
  `VFS-CHECKS-RUN-WALKER-LOOP-1`,
  `VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`
  (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md`) — open() lookup runs
  through the walker.
- `txdoc:MOUNT-MOUNTPAYLOAD-1`, `MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`
  (`docs/design/05_filesystem/MOUNT_v1.md`) — backend trait surface and
  mount publication ordering.
- `txdoc:TTY-THE-HARDWARE-CONSOLE-PATH-1`,
  `TTY-LOOKUP-1`, `TTY-RNODE-MATERIALIZATION-1`
  (`docs/design/06_devices/TTY.md`) — `register_hardware` /
  `register_console_alias` and devfs RNode materialisation for TTYs.
- `txdoc:HAL-V1` trap-frame contract
  (`docs/design/01_substrate/HAL_v1.md`) — `TrapFrameMut`, `TrapAction`,
  `KernelTrapSink` invariants we hand off to.

## Part 1 — Trap shell

### Surface

- `crates/tx-kernel/src/trap.rs::KernelTrapDispatcher::on_syscall`
  (replacing the `_view, Terminate` stub).
- `crates/tx-kernel/src/trap.rs::KernelTrapDispatcher::on_page_fault`
  (replacing the `_view, _fault, Terminate` stub).
- New module `crates/tx-kernel/src/trap_handoff.rs` (or `trap/handoff.rs`
  if we elect a folder split) — owns the platform-neutral conversion
  `TrapFrameView → SyscallRequest` and `FaultInfo → PageFaultInfo`,
  plus the routing function that resolves the active
  `UserspaceRunSlot` for the current hart.
- New `crates/tx-kernel/src/userspace.rs::CURRENT_RUN_SLOT`: a
  `&'static UserspaceRunSlot` per-hart accessor, populated when the
  reactor dispatches a userspace-run wait. Either a `PerCpu` slot or
  the slot reachable through the active `ThreadPayload` (preferred —
  see payload extension below).
- `tx-subsystems::thread_runtime::current_thread_payload(hart) ->
  Option<PayloadCap<ThreadPayload>>` — new accessor matching how
  `ProcessPayload::current()` will eventually be spelled. For Phase 1
  this can be a per-hart `AtomicSlot<PayloadCap<ThreadPayload>>` set
  by the reactor task wrapper that drives a thread future.

### Wiring

Sequence on syscall trap:

1. RV64 trap stub saves registers into the platform-owned raw frame
   and constructs a `TrapFrameMut<'_>` (already done by tx-hal).
2. `KernelTrapSink<P>::on_syscall(view)` runs.
3. Trap shell calls `trap_handoff::translate_syscall(&view) ->
   SyscallRequest { nr, args }`.
4. Trap shell looks up the active `ThreadPayload` for the current
   hart, snapshots the saved user context into the payload's new
   `saved_user_context: AtomicSlot<UserTrapContext>` field via
   `view.capture_user_context()`, and pulls the
   `UserspaceRunSlot` and `UserspaceRunRequest` from the same
   payload.
5. Trap shell calls
   `slot.complete_interesting_trap(req, UserspaceTrapInfo::Syscall(req))`
   (per `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`). This wakes the
   thread future, which runs the syscall dispatch via Part 2.
6. Trap shell returns `TrapAction::Reschedule` so the reactor runs
   the now-ready thread future on this hart before re-entering
   userspace. The thread future resolves a return value, writes it
   back via `view.set_syscall_return(rv)` or
   `view.set_syscall_error(errno)`, then either restarts a new
   userspace-run wait (start_request → checkpoint_userspace_entry →
   platform `enter_userspace`) or transitions the process out of
   running on `exit_group`.

   **Open question (flagged):** writeback timing. RV64 syscall
   convention writes `a0` *before* re-entering userspace. The
   simplest landing is to defer writeback until userspace re-entry
   in the thread future, not from the trap shell. The trap shell
   thus stores nothing back into the frame; it just resolves the
   wait and reschedules. Confirm with HAL skill before implementing.

Sequence on user page fault:

1. `on_page_fault(view, fault)` runs. If `!fault.from_user`,
   delegate to existing `Terminate` (kernel must not page-fault on
   user mappings via this path).
2. Translate to
   `PageFaultInfo { addr: UserAddr, access, present: false }`.
3. Resolve the `Cap<ProcessIdentity>` for the current thread, then
   its `AddressSpace` (already resolvable via
   `process.payload.aspace`).
4. Synchronously call `aspace.fault_script(VmFault { ... }).await`?
   No — the trap context can't `.await`. Instead the trap shell
   resolves the userspace-run wait with
   `UserspaceTrapInfo::PageFault(info)`; the thread future, woken
   by the resolution, drives `fault_script` through the canonical
   async path (yields on `RangeLock::WouldBlock`,
   `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
5. On `Ok(_)`: thread future loops back to userspace-entry
   checkpoint (no syscall return value). On `Err`: route through
   default-action SIGSEGV via `step_exit_group_with_signal` (already
   implemented per `txdoc:SIGNAL-V1` §15.1, §12.3 — see
   `crates/tx-subsystems/src/process/execution.rs`).

### ThreadRuntime payload extension

Add to `ThreadPayload` (in
`crates/tx-subsystems/src/thread_runtime/structure.rs`):

| field | type | invariant |
|---|---|---|
| `userspace_slot` | `UserspaceRunSlot` | One slot per thread; clone-shared with the task future. Created in the thread's bootstrap helper. |
| `active_request` | `SpinMutex<Option<UserspaceRunRequest>>` | Generation token for the in-flight wait; `None` between waits. Set by `start_request`, cleared by the resolved `UserspaceRunWait` future. |
| `saved_user_context` | `SpinMutex<Option<UserTrapContext>>` | Captured on entry to a syscall/fault trap, restored before next userspace entry. Per `THREAD-5-1-STATE-PLACEMENT`. |
| `pending_syscall_return` | `SpinMutex<Option<Result<i64, i32>>>` | Result of the last completed syscall, drained by the userspace-entry checkpoint and written into the trap frame via `set_syscall_return` / `set_syscall_error` immediately before return. |

Existing `signal_mask`, `thread_pending`, `signal_summary`, `task` stay
untouched. Existing tests in
`crates/tx-subsystems/src/thread_runtime/tests.rs` and
`crates/tx-subsystems/src/signal/tests.rs` must continue to pass —
none of them construct `ThreadPayload` with custom values today, so
adding `Default` + builder helpers for the new fields is sufficient.

Existing test that must still pass: every signal-delivery test in
`crates/tx-subsystems/src/signal/tests.rs`, `post_signal_*`,
`select_next_signal_*`, the AST-dispatch suite under
`docs/progress/plans/2026-05-05-ast-dispatch.md`. They all read
`signal_summary` / `pending` and would break if we accidentally
shrink or rebound those fields.

### Tests

- `crates/tx-kernel/src/trap_handoff/tests.rs` (new):
  - `translate_syscall_packs_a7_to_nr_and_a0_a5_to_args` — feed a
    synthesised `TrapFrameView` and assert
    `SyscallRequest { nr, args } == expected`.
  - `translate_user_pf_marks_write_when_store_fault`.
- `crates/tx-subsystems/src/thread_runtime/tests.rs` (extend):
  - `userspace_slot_round_trip_resolves_with_syscall` — start a
    request, post a `Syscall(req)`, await the wait future to a
    `UserspaceTrapInfo::Syscall` outcome.
  - `pending_syscall_return_drains_at_userspace_entry`.
- `crates/tx-kernel/src/tests/trap_smoke.rs` (new) — integration
  smoke that calls a fake "userspace" closure which immediately
  returns a `Syscall(write)` and asserts the dispatch loop runs once
  through Part 2 and Part 3. Gated behind a host-friendly test
  harness so it does not require the rv64 board.

## Part 2 — Syscall table

### Numbering policy

Source of truth: Linux RV64 ABI (`/usr/include/asm-generic/unistd.h`,
`__NR_*` numbers; same as arm64/risc-v generic ABI). Authoritative
in-tree mirror: a new `crates/tx-shims/src/linux_syscall/numbers.rs`
listing each used number as a `pub const NR_WRITE: u64 = 64;` etc.
This pins the ABI in one file and lets the dispatch table use named
constants. No `pub use` of `libc` numbers — txKernel is no_std and
must own its ABI table.

The dispatch table itself lives in
`crates/tx-shims/src/linux_syscall/mod.rs` and exposes a single
function `dispatch(req: SyscallRequest, ctx: &SyscallCtx<'_>) ->
SyscallResult`. `ctx` carries `Cap<ProcessIdentity>`,
`Cap<ThreadIdentity>`, the bound `AddressSpace`, current-credential
snapshot, and a `Guard<'_>` (per `tx-subsystems::execution::Guard`).

### Entries

| nr | name | step_* called | argument decode | return convention |
|---:|---|---|---|---|
| 64 | `write` | resolve `fd → OpenFile`, then `OpenFile::step_write(bytes, &guard)` (see `crates/tx-subsystems/src/vfs/execution.rs`) | `a0=fd: i32`, `a1=buf: UserAddr`, `a2=len: usize`. UserBuf copy via the (deferred) `aspace::copy_from_user` helper — for now, restrict to fds whose backing is `StructPayload::Tty`/`CharDevice`, where the bytes go directly into the device write path; copy-from-user is bounded by `TTY_WRITE_MAX_INLINE` (e.g. 4 KiB) and uses a kernel-side temporary on the stack (no general copy_from_user in this slice). | `usize` written → `i64`; `Errno → -i32` via `set_syscall_error`. |
| 63 | `read` | `OpenFile::step_read(out, &guard)` | `a0=fd`, `a1=buf`, `a2=len`. Same UserBuf restriction as `write`. | bytes-read or `-errno`. |
| 93 | `exit` | `step_thread_exit(thread, status as i32)` (`crates/tx-subsystems/src/thread_runtime/execution.rs`). For a single-threaded process this also implies process exit; the syscall dispatcher follows up with `step_exit_group(process, ExitStatus::from_raw(status))` if the thread group becomes empty. | `a0=status: i32`. | does not return; thread future ends. |
| 94 | `exit_group` | `step_exit_group(process, ExitStatus::from_normal_exit(status))` (`crates/tx-subsystems/src/process/execution.rs`). | `a0=status: i32`. | does not return. |
| 172 | `getpid` | read `process.identity().pid()` directly. | none. | `pid as i64`. |
| 214 | `brk` | `aspace.brk_script(brk_base, current_brk, requested_brk).await` (`txdoc:VM-5-8-BRK`). `brk_base` and `current_brk` come from a new `ProcessPayload::brk` field (see "Open questions"). | `a0=requested: u64` (zero means "report current"). | new `current_brk as i64`. On `InvalidRange` returns the *unchanged* `current_brk` (Linux semantics — brk never returns negative errno). |
| 135 | `rt_sigprocmask` | `step_sigprocmask(thread, how, &set, oldset_out)` (`crates/tx-subsystems/src/thread_runtime/execution.rs`). | `a0=how: i32`, `a1=set: UserAddr`, `a2=oldset: UserAddr`, `a3=sigsetsize: usize`. Reject `sigsetsize != 8`. Read 8-byte `u64` from user; write 8-byte `u64` if `oldset != 0`. | `0` or `-errno`. |
| 134 | `rt_sigaction` | `step_sigaction(process, signum, &new_action_opt, old_action_out_opt)` (`crates/tx-subsystems/src/signal.rs`). | `a0=signum: i32`, `a1=act: UserAddr`, `a2=oldact: UserAddr`, `a3=sigsetsize: usize`. Reject `sigsetsize != 8`. Decode `struct sigaction` (16 bytes: handler/flags/restorer/mask). | `0` or `-errno`. |

Process / thread / aspace lookup uses
`SyscallCtx::current_thread()` and `current_process()` accessors. fd
table integration is not yet present — this slice introduces a
**stub fd table** on `ProcessPayload`: `fds:
SpinMutex<[Option<Cap<OpenFile>>; 8]>` (see "Open questions" for the
final shape). Init's bootstrap (Part 3 phase-3b) preopens `0/1/2`
as `/dev/console` via the devfs `OpenFile`-construction helper.

### Error path

`step_*` functions return `StepOutcome<T>` (Done / Blocked / Err)
or `Result<T, Errno>` where applicable. Translation rules:

- `StepOutcome::Done(v)` → encode `v` as the syscall return.
- `StepOutcome::Blocked(token)` → re-await on the wait carrier
  inside the thread future (per
  `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`). The dispatcher loops
  until `Done` or `Err`; it never returns `Blocked` to the trap
  shell.
- `StepOutcome::Err(errno)` → `set_syscall_error(errno as i32)` →
  trap-frame writeback applies `-errno` per
  `tx_hal::TrapFrameMut::set_syscall_error`.

Unknown `nr`: `-ENOSYS` (encoded as `-38`). Per Linux ABI any future
syscall slot needs an explicit entry; we deliberately reject by
default.

### Tests

- `crates/tx-shims/src/linux_syscall/tests.rs` (new):
  - `dispatch_write_one_to_console_returns_byte_count` — set up a
    devfs-mounted console, open it as fd 1, call `dispatch` with a
    `SyscallRequest::new(NR_WRITE, [1, kbuf_ptr, 6, 0, 0, 0])`,
    assert tty `console_write_str` saw the bytes.
  - `dispatch_exit_group_marks_process_zombie`.
  - `dispatch_getpid_returns_init_pid_for_init_process`.
  - `dispatch_brk_grow_then_shrink_round_trip`.
  - `dispatch_rt_sigprocmask_block_then_unblock_round_trip`.
  - `dispatch_unknown_nr_returns_neg_enosys`.

## Part 3 — tmpfs + devfs

### tmpfs FsOps surface

File: `crates/tx-fs/src/tmpfs.rs`. Implement the `FsOps` and
`FsPageBacking` traits (per
`crates/tx-subsystems/src/vfs/execution.rs::FsOps` and
`page_backed::FsPageBacking`). Minimum to land:

| trait | method | semantics | PageContainer ops |
|---|---|---|---|
| `FsOps` | `lookup` | search tmpfs in-memory dir BTreeMap by name | none |
| `FsOps` | `load_inode_meta` | clone stored `InodeMeta` | none |
| `FsOps` | `serialize_inode_meta` | overwrite stored `InodeMeta` | none |
| `FsOps` | `create_inode` | allocate a fresh `FsObjectId`, mode∈{S_IFREG,…}, returns `(id, meta)` | none |
| `FsOps` | `mkdir` | same as `create_inode` for directories | none |
| `FsOps` | `unlink` / `rmdir` / `rename` / `link` | mutate dir BTreeMap | none |
| `FsOps` | `symlink` | same as `create_inode` for `S_IFLNK`; target stored inline (≤ `VFS_NAME_MAX`) | none |
| `FsOps` | `readdir` | iterate dir BTreeMap from `DirCursor` | none |
| `FsOps` | `destroy_inode` | drop the inode entry; tmpfs's PageContainer (if any) is detached and the live `Cap` falls when the RNode does | none directly; tmpfs holds a side-table mapping `FsObjectId → Cap<PageContainer>` for regular files |
| `FsPageBacking` | `fetch_page` | look up the file's `Cap<PageContainer>`, then `pc.materialize_page(page, access)` (already per `PageContainerKind::Anon { swap_policy: Reclaimable }`) | `materialize_page` |
| `FsPageBacking` | `flush_page` | tmpfs is in-memory; `Done(())` immediately | none |
| `FsPageBacking` | `truncate` | call `step_truncate(pc, new_size)` (`crates/tx-subsystems/src/page_backed/lifecycle.rs`) | `step_truncate` |
| `FsPageBacking` | `fsync` | `Done(())` immediately | none |

Each tmpfs regular-file inode gets a fresh
`PageContainer::new(PageContainerKind::Anon { swap_policy:
AnonSwapPolicy::Reclaimable }, page_count: 0)`. Page count grows
through `materialize_page` calls; truncation is via
`page_backed::lifecycle::step_truncate`.

The tmpfs root is materialised via `MountOutput { fs_ops:
Arc::new(Tmpfs), fs_page_backing: Arc::new(Tmpfs), root_fs_object_id:
FsObjectId::new(2), root_inode_meta: <S_IFDIR | 0o755> }` — note
`FsObjectId::ROOT == 1` is reserved; tmpfs uses 2 onward.

### devfs FsOps surface

File: `crates/tx-fs/src/devfs.rs`. Static read-only namespace; the
key design lever is that `RNodeBacking` already supports
`StructBacked { payload: StructPayload::Tty(Cap<TtyIdentity>) }` so
devfs RNodes don't need a new backing variant.

| trait | method | semantics |
|---|---|---|
| `FsOps` | `lookup` | look up `name` against the static devfs registry — *not* a tmpfs map. Registry is exactly what `tty::execution::register_console_alias` populates: `crates/tx-subsystems/src/tty/structure/registry.rs::register_devfs_alias`. devfs's `lookup` calls a new `tty::project::resolve_devfs_alias(name) -> Option<Cap<TtyIdentity>>` (added in this slice; trivial wrapper over the existing registry). |
| `FsOps` | `load_inode_meta` | per-name table; `/dev/console` is `S_IFCHR \| 0o620` |
| `FsOps` | `readdir` | iterate the registry snapshot |
| `FsOps` | every mutating op | `Errno::EROFS` |
| `FsPageBacking` | every method | `Errno::ENOSYS` (devfs char devices don't go through page-cache I/O) |

devfs RNode materialisation: for `lookup("console")` returning a
TTY, the upper-layer VFS open path will need to construct an
`RNode::new(fs_object_id, meta_with_S_IFCHR, RNodeBacking::StructBacked
{ payload: StructPayload::Tty(tty) })`. Today the `OpenFile`
read/write dispatch (see `vfs/execution.rs:131`) already routes
`StructBacked { Tty }` to `tty::execution::step_read/step_write`,
so once devfs returns the right RNode there is nothing further to
wire.

`step_read`/`step_write` find the TTY by walking
`OpenFile.rnode().backing()` — which already works. The only new
plumbing is constructing the right `RNode` at devfs lookup time and
ensuring the kernel's open path uses it. Until VFS's `step_open`
exists (deferred), we expose
`crates/tx-fs/src/devfs.rs::open_console_for_init() ->
Cap<OpenFile>` — a one-shot helper used by init.rs to preopen
fds 0/1/2. This bypasses the walker for the bootstrap moment only.

### Mount wiring at boot

In `crates/tx-kernel/src/init.rs::CoreInit::init_substrate_if_ready`,
**after** `init_process_subsystem` and `register_hardware`, add:

1. `Self::register_console_hardware()` — calls
   `tty::execution::register_hardware("console", 0,
   &CONSOLE_BINDING, &guard)` where `CONSOLE_BINDING` is a static
   `CharDeviceBinding` whose `write` calls
   `tx_hal::console_write_str::<P>` and whose `read` returns
   `StepOutcome::Done(0)` for the slice.
2. `Self::mount_rootfs_tmpfs()` — constructs a tmpfs MountOutput,
   builds the root `RNode` + `DEntry` (root has `InlineName::ROOT`),
   then `MountIdentity::new_cap`. Stores the resulting
   `Cap<MountIdentity>` in a new global `ROOT_MOUNT` slot
   (analogous to `INIT_PROCESS`).
3. `Self::mount_devfs_at_dev()` — `mkdir("/dev")` against tmpfs
   (uses tmpfs's `FsOps::mkdir`), then mount devfs at that DEntry.
4. `Self::register_devfs_console_alias()` — `tty::execution::
   register_console_alias("console",
   ROOT_MOUNT_CONSOLE_TTY.clone())`. After this, devfs's `lookup`
   for `"console"` resolves.
5. `Self::bind_init_cwd_and_root()` — calls
   `step_chdir(init_process, root_dentry)` so init has a non-`None`
   cwd.

Order vs. existing init: TTY hardware register **must** precede
devfs mount (so the alias is visible at mount time) and rootfs
mount **must** precede devfs mount (devfs needs a directory entry to
mount on). Process subsystem still initialises before any of this
because `init_process` predates fs (it's a no-cwd process for one
extra phase, then we bind).

### Tests

- `crates/tx-fs/src/tmpfs/tests.rs` (new):
  - `tmpfs_create_then_lookup_round_trip`.
  - `tmpfs_mkdir_then_readdir_yields_dir_entry`.
  - `tmpfs_unlink_drops_inode`.
  - `tmpfs_fetch_page_materialises_anon_then_flush_noop`.
- `crates/tx-fs/src/devfs/tests.rs` (new):
  - `devfs_lookup_console_after_register_hardware_returns_tty_rnode`.
  - `devfs_write_through_openfile_reaches_tty_step_write`.
  - `devfs_create_returns_erofs`.
- `crates/tx-kernel/src/init/tests.rs` (extend or create):
  - `boot_smoke_mounts_root_and_dev_and_resolves_console`.

## Cross-cutting risks

1. **Trap-frame writeback timing (highest risk).** The plan defers
   `set_syscall_return` until userspace re-entry, executed inside
   the thread future, not from the trap shell. This requires the
   thread future to hold a `TrapFrameMut<'_>` across an
   await — which is impossible (`TrapFrameMut` is not `Send` and
   borrows the per-hart raw frame). The honest design is:
   - The trap shell snapshots `view.capture_user_context()` into
     `ThreadPayload.saved_user_context`.
   - The trap shell resolves the userspace-run wait and returns
     `Reschedule`.
   - The reactor polls the thread future, which runs the syscall
     dispatch and stores the result into
     `ThreadPayload.pending_syscall_return`.
   - When the reactor next selects this thread for userspace
     re-entry, the platform's `enter_userspace` shim restores the
     saved context, then writes the pending return value into the
     **fresh** trap frame's a0 slot before `sret`. This means the
     userspace-entry path must learn to consult `ThreadPayload`. Doc
     says payload owns this state (`THREAD-5-1-STATE-PLACEMENT`),
     code does not yet have a userspace-entry shim. The trio plan
     assumes a follow-up lane will land that shim; for the bootstrap
     smoke test we synthesise a fake entry shim that reads the
     payload directly.
2. **Cap vs IdentRef in syscall ctx.** The dispatcher holds
   `Cap<ProcessIdentity>` and `Cap<ThreadIdentity>` while it
   `.await`s a step. Per the Cap invariants in
   `crates/tx-substrate/src/zone/`, `Cap` is fine across yields
   (it's epoch-managed). `IdentRef<'g, T>` is **not** — it borrows
   a guard. Each step that requires a guard must be entered with a
   fresh `tx_substrate::epoch::guard()` inside that step's call
   site, never reused across an `.await` boundary. This is already
   the existing pattern in `vm::execution::fault_script`; copying
   that discipline into the dispatcher is the safe choice.
3. **Signal/AST interactions during syscall.** A pending signal
   selected before userspace re-entry must run the
   `checkpoint_userspace_entry` AST hook (see
   `tx-reactor::userspace::UserspaceRunSlot::checkpoint_userspace_entry`).
   The slice does not implement signal-handler-frame setup, so the
   only valid AST decision in this slice is `EnterUserspace`. If
   any signal is pending at userspace-entry, default action runs
   immediately via `step_exit_group_with_signal` (already
   implemented for SIGKILL per
   `docs/progress/plans/2026-05-05-step-exit-group-with-signal.md`).
   Document this restriction explicitly so a follow-up lane can
   loosen it.
4. **RV64 trap-frame layout uncertainty.** `TrapFrameView` documents
   `syscall_args[0]` as the first arg; the writeback rules in
   `set_syscall_return` mutate `syscall_args[0]` to the return.
   This conflates `a0` and the return register because they
   happen to be the same on RV64. Cross-arch portability isn't a
   risk yet (only RV64 is the target), but the syscall dispatcher
   should use `set_syscall_return` / `set_syscall_error`
   exclusively rather than ever reading `syscall_args[0]` after
   dispatch.
5. **Potential doc/code mismatch on devfs.** Skill
   `tx-tty-subsystem` notes "`/dev/console` is currently modeled
   through alias registration rather than a fully settled final
   console integration path." The trio's devfs plan doubles down
   on alias registration as the intentional V1 mechanism, matching
   today's code. If a future TTY rev replaces alias registration
   with something else, devfs.rs has to follow. Flagging here so
   the implementer knows to re-check `tty/structure/registry.rs`
   before each integration point.
6. **stub fd table on `ProcessPayload`.** Adding even a fixed-size
   `fds: SpinMutex<[Option<Cap<OpenFile>>; 8]>` field to
   `ProcessPayload` touches a hot type with many existing tests
   (chdir, fork, signal, wait). Provide `Default` and ensure
   `step_fork` clones the slice (`OpenFile` clone semantics at
   fork time per VFS_CHECKS — for now, share the same `Cap`; full
   `dup`-shape sharing belongs to a follow-up).
7. **`brk_base` storage on Process.** `step_brk` needs a per-process
   `brk_base` and `current_brk`. They are not on `ProcessPayload`
   today. Add `brk_base: AtomicU64` and `current_brk: AtomicU64`,
   initialised at exec time (deferred — for the smoke test,
   bootstrap init at brk_base=0x6000_0000 explicitly inside
   `bootstrap_init_process`). Flag this as a temporary bootstrap
   value to be replaced when ELF loading lands.

## Out of scope (deliberately deferred)

- ELF loader (`execve` syscall, image parsing, entry-point trampoline).
- `fork`, `clone`, `execve`, `wait4`, `waitid` syscalls. (`fork`'s VM
  half exists in `vm::execution::fork_aspace`; the syscall driver
  does not.)
- Real init binary loading from initramfs / cpio.
- `procfs`, `sysfs`, `bdevfs`, `devpts`, `tx_ext4` backends.
- General `copy_from_user` / `copy_to_user` for arbitrary VAs. The
  trio uses bounded inline buffers for the ≤4 KiB write path and
  declines other patterns.
- Userspace signal-handler frame setup (sigreturn, alt stack).
- `/dev/console` `read` returning real input (line discipline ingest
  exists per `tx-tty-subsystem`; binding it to the VFS read path is
  next slice).
- `tx-shims::posix_signal` module population (ABI struct decode for
  rt_sigaction sits inside `linux_syscall` for now; `posix_signal`
  remains a stub).

## Phasing

Land in this order; each step is a self-contained PR.

1. **Trap shell skeleton (Part 1).** `trap_handoff` module,
   `ThreadPayload` payload extension, no semantic dispatch yet.
   Trap shell still returns `Terminate` for unknown user activity
   but resolves the userspace-run wait correctly when one exists.
   Tests: `userspace_slot_round_trip_resolves_with_syscall`.
2. **2a — Minimal write/exit dispatch.** `linux_syscall::dispatch`
   handles only `NR_WRITE`, `NR_EXIT`, `NR_EXIT_GROUP`, `NR_GETPID`
   (everything that doesn't need fd table beyond fd≤2 / brk / sig).
   Tests: dispatch_write_returns_byte_count (with a fake openfile
   wired to a stub TTY), dispatch_exit_group_marks_zombie,
   dispatch_getpid.
3. **3a — devfs only, no rootfs.** Bring up devfs as a standalone
   namespace bound to the TTY registry; expose
   `open_console_for_init`. Tests:
   `devfs_lookup_console_after_register_hardware`,
   `devfs_write_through_openfile_reaches_tty_step_write`. Init
   preopens fds 0/1/2 directly via `open_console_for_init`; this
   lets phase 2a's smoke tests run end-to-end without VFS walker.
4. **2b — rest of syscall table.** Add `read`, `brk`,
   `rt_sigprocmask`, `rt_sigaction`. Requires brk-base on
   `ProcessPayload`. Tests: brk-grow-shrink, sigprocmask
   round-trip, sigaction round-trip.
5. **3b — tmpfs + mount wiring.** Bring up tmpfs, mount it at `/`,
   `mkdir /dev`, mount devfs at `/dev`. Bind init's cwd to `/`.
   Tests: tmpfs CRUD, mount integration test that resolves
   `/dev/console` via the existing VFS walker.
6. **End-to-end smoke.** A `cargo test -p tx-kernel` test that
   feeds a fake userspace closure: it returns
   `Syscall(write(1, "hi\n", 3))`, then `Syscall(exit_group(0))`.
   Assert: console output observed, init becomes a zombie with
   `ExitStatus::from_normal_exit(0)`.

## Open questions

1. **Trap-frame writeback site.** Confirmed-with-user needed: should
   the trap shell write `a0` directly (forgoing the
   captured-context/restore round trip) or always defer to the
   userspace-entry shim? Plan currently chooses defer; the simpler
   alternative is to write directly from the trap shell after the
   thread future resolves and *before* returning `Reschedule`.
   The simpler alternative blocks signal delivery between the syscall
   resolving and userspace re-entry, so the deferred approach is
   probably correct, but it's a bigger lift.
2. **`SyscallCtx` ownership.** Should the dispatcher own a
   `&Guard<'_>` for the duration of one syscall, or freshly call
   `tx_substrate::epoch::guard()` per step? Existing scripts
   freshly-guard per step (see `vm::execution::fault_script`).
   Plan adopts that pattern but flags it explicitly because a
   future "bulk syscall" optimisation might want guard reuse.
3. **fd table shape.** Fixed-size 8-slot vs. growable
   `BTreeMap<u32, Cap<OpenFile>>`. The plan picks fixed-size for
   the slice. POSIX requires `dup3`/`fcntl(F_DUPFD)` semantics
   eventually; that's a follow-up.
4. **`brk_base` initialisation.** Hard-coded `0x6000_0000` in
   `bootstrap_init_process` for the smoke test. Confirm this
   doesn't collide with any RV64 QEMU virt platform mapping —
   `tx-substrate`'s direct-map and pmap user-region constants
   should bound it.
5. **Console `read` on devfs.** `tty::execution::step_read` already
   exists, but plumbing it through devfs's `OpenFile::step_read`
   requires the `RNodeBacking::StructBacked` path (already there).
   Test: a future slice may want canonical-mode line-buffered
   read; the trio's `read` syscall returns 0 bytes if no input is
   available — confirm with TTY skill that this is acceptable
   non-blocking behaviour for the slice.
6. **`exit` vs. `exit_group` for single-threaded init.** Spec says
   `exit` is per-thread; for a single-threaded process it
   *implicitly* triggers `exit_group`. Plan codes this in the
   dispatcher. Confirm with PROCESS_v1 §8.4 (exit ordering)
   whether the dispatcher should call `step_exit_group` directly
   or whether `step_thread_exit` chains internally when the thread
   group becomes empty.

