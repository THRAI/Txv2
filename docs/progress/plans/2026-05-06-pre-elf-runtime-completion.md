# Pre-ELF Runtime Completion

Status: proposed (planning only). Companion to
`docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json`.
Closes the trio decision note's follow-ups #1-#6 and #8-#10
(`docs/progress/decisions/2026-05-06-trio-trap-syscall-tmpfs-devfs.md`).
Out of scope: ELF loader, `execve`, first real userspace binary
(next slice).

## Goal

Complete the runtime path that the trio left half-instantiated, so
the kernel can drive an end-to-end userspace round-trip without
synthesising a fake reactor driver or a fake userspace-entry shim.
At "done":

- (a) the Phase 6 fake driver in
  `crates/tx-kernel/src/init/tests.rs` is deleted because the
  production reactor task wrapper that drives the thread future and
  the production userspace-entry shim that drains
  `pending_syscall_return` together resolve the loop in spec terms;
- (b) page faults on user mappings are handled by
  `AddressSpace::fault_script` from inside the thread future, with
  `Err(_)` routed through `step_exit_group_with_signal(SIGSEGV)`,
  retiring the `TODO(phase-2)` in
  `crates/tx-kernel/src/trap_handoff.rs::hand_off_user_pf`;
- (c) `/dev/console` opens by path through a real VFS walker, so
  `tx_fs::devfs::open_console_for_init` becomes a thin wrapper that
  goes through `step_open` (the bootstrap exemption is retired);
- (d) UART RX bytes drive `tty::execution::step_ingest` via the
  `IrqDispatchTable` already plumbed through the HAL, so a blocked
  `read(0, ...)` on init's stdin actually wakes when bytes arrive;
- (e) the four minor cleanups land (public `SpinMutex`, `Errno`
  extension, `InlineName: Ord`, `MountId`/`DevId` allocators).

Demonstrable smoke: a single host test in `tx-kernel` that drives
the trio's existing `write(1, "hi\n", 3)` → `exit_group(0)` chain
through `Reactor::submit_task` (no manual
`set_current_thread_payload` calls), plus a follow-up smoke that
synthesises a UART RX byte and observes a blocked `read` on fd 0
unblock with the byte returned.

## Doc anchors

Reactor task wrapper + userspace-entry shim:

- `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`,
  `txdoc:REACTOR-PREEMPTION-TRANSPARENCY`,
  `txdoc:REACTOR-AST-ASYNCHRONOUS-TRAP`
  (`docs/design/02_execution/REACTOR_v0.md`) — userspace execution
  is a reactor wait resolved by a trap; AST drains gate userspace
  re-entry.
- `txdoc:THREAD-4-1-SHAPE`, `txdoc:THREAD-4-2-OWNERSHIP`,
  `txdoc:THREAD-5-1-STATE-PLACEMENT`,
  `txdoc:THREAD-5-2-THE-INTERRUPT-SUMMARY`,
  `txdoc:THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT`,
  `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
  (`docs/design/02_execution/THREAD_RUNTIME_v1.md`) — thread future
  shape, payload-vs-task ownership split, where saved user context
  lives, the interrupt-predicate contract that gates AST decisions.
  (Note: the brief cited `THREAD-5-3-AST-CHECKPOINTS`, but no such
  anchor exists; the real anchor at §5.3 is
  `THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT`. Flagged under
  Cross-cutting risks.)
- `docs/design/02_execution/SCHEDULER_v0.md` — how the scheduler
  picks the next thread to run; the wrapper plugs in around the
  `Reactor::run_until_idle_on_hart_with_reschedule` poll site.
- `docs/design/01_substrate/HAL_v1.md` — `RawTrapFrame`,
  `TrapFrameView`, `TrapFrameMut`, `TrapAction`, `KernelTrapSink`
  invariants. The HAL trait surface today exposes `TrapFrameMut`,
  `TrapFrameMut::set_syscall_return`,
  `TrapFrameMut::set_syscall_error`, and
  `TrapFrameMut::capture_user_context`, but does **not** yet expose
  a portable `enter_userspace` / `return_to_userspace` entry — the
  RV64 board has `tx_hal_riscv64_qemu_virt::return_to_userspace`,
  symbol-only. Flagged under Cross-cutting risks.
- `docs/design/02_execution/STEP_MODEL_v1.md` — the await/yield
  discipline the thread future and the wrapper must obey.

Page-fault async dispatch:

- `txdoc:VM-5-1-FAULT-HANDLER`, `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`
  (`docs/design/03_memory-vm/VM_v1_2.md`) — the canonical fault
  script and the cross-await discipline the thread future must
  honour when calling `aspace.fault_script(fault).await`.
- `docs/design/04_process-signals/SIGNAL_v1.md` §15.1 — default
  action SIGSEGV; `step_exit_group_with_signal` already implements
  it (`crates/tx-subsystems/src/process/execution.rs:483`).

VFS walker:

- `txdoc:VFS-CHECKS-WALKER-MODES-1`,
  `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`,
  `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`
  (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md`).
- `txdoc:MOUNT-MOUNTPAYLOAD-1`,
  `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`
  (`docs/design/05_filesystem/MOUNT_v1.md`) — mount-point boundary
  crossings the walker must honour.

IRQ dispatch + UART RX:

- `docs/design/06_devices/DEVICE.md` — IRQ dispatch model;
  tier-1/tier-2 device classification; how a device's IRQ handler
  is registered.
- `txdoc:TTY-LOOKUP-1`, `txdoc:TTY-RNODE-MATERIALIZATION-1`
  (`docs/design/06_devices/TTY.md`) — RX-side ldisc ingest path
  (`step_ingest`); devfs RNode materialisation for TTYs.
- `docs/design/01_substrate/HAL_v1.md` — `IrqIf`, `IrqDispatchTable`,
  `IrqHandlerFn`, `IrqHandled`, `install_dispatch_table` (already
  in HAL today).

## Part 1 — Reactor task wrapper + userspace-entry shim (items 1+2)

### Surface

New module `crates/tx-subsystems/src/thread_runtime/task_wrapper.rs`
(or co-located with `execution.rs`) containing:

- `pub fn spawn_thread_task(reactor: &SharedReactor, thread:
  Cap<ThreadIdentity>, payload: PayloadCap<ThreadPayload>) ->
  TaskKey` — wraps the thread future in a task, calls
  `reactor.with(|r| r.submit_task(future))`, stores the resulting
  `TaskKey` into `thread_runtime::structure::ThreadPayload.task`.
- `pub async fn thread_future(thread: Cap<ThreadIdentity>, payload:
  PayloadCap<ThreadPayload>)` — the per-thread driver loop. Body is
  the userspace round-trip:
    1. `let wait = payload.userspace_slot().start_request()?;`
       (already in `crates/tx-reactor/src/userspace.rs:234`).
    2. `payload.set_active_userspace_request(Some(wait.request()))`.
    3. Call into the userspace-entry shim
       (`prepare_userspace_entry_payload`, see below) to build the
       fresh trap-frame writeback view, then transition to user
       (in production: HAL `return_to_userspace`; under test:
       deliver a synthetic `Syscall(req)` via the existing fake
       userspace queue; see Tests).
    4. `.await` resolves the wait → match `UserspaceTrapInfo`:
       - `Syscall(req)` → call
         `tx_shims::linux_syscall::dispatch(req, &ctx).await`,
         then `payload.store_pending_syscall_return(Some(result))`,
         loop.
       - `PageFault(info)` → see Part 2.
       - other variants → propagate per spec.
    5. Loop until the syscall handler returns
       `SyscallResult::NoReturn` (process/thread exit).

Per-hart slot management (already in
`crates/tx-subsystems/src/thread_runtime/structure.rs`):

- `set_current_thread_payload(hart, payload)` and
  `clear_current_thread_payload(hart)` already exist. The wrapper
  must call `set_*` synchronously inside an outer
  `core::future::poll_fn`-style adapter that wraps the
  thread-future poll: set on entry, clear on Pending/Ready return.
  This is the **task wrapper** as distinct from the **thread
  future** itself — the thread future does not hold the per-hart
  slot guard; the wrapper does, and only across one synchronous
  poll.

The wrapper is a `Future` adapter spelled roughly as:

```text
PerHartSlotted<F> where F: Future {
    inner: F,
    payload: PayloadCap<ThreadPayload>,
    // hart not stored — re-read via PercpuIf at every poll.
}
impl Future for PerHartSlotted {
    fn poll(self, cx) -> Poll<F::Output> {
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let _prev = set_current_thread_payload(hart, self.payload.clone());
        let out = self.project().inner.poll(cx);
        clear_current_thread_payload(hart);
        out
    }
}
```

The wrapper is **never** held across an `.await`: it lives on the
stack of `Reactor::run_until_idle_on_hart_with_reschedule`'s poll
loop (`crates/tx-reactor/src/runtime.rs:388-403`), which polls
exactly once per scheduler decision, then yields control. This
satisfies `txdoc:REACTOR-PREEMPTION-TRANSPARENCY`.

Userspace-entry shim:

- New module `crates/tx-subsystems/src/thread_runtime/entry.rs`
  (or `crates/tx-kernel/src/userspace_entry.rs`).
- `pub fn prepare_userspace_entry_payload(payload:
  &ThreadPayload)` — produces a `UserTrapContext` (already a HAL
  type, `Pod`-shaped) ready to be loaded into the platform's raw
  trap frame: starts from `payload.saved_user_context()`,
  overlays any drained `pending_syscall_return` into `a0`
  (mirroring what `TrapFrameMut::set_syscall_return` /
  `set_syscall_error` would do), and returns the merged
  `UserTrapContext`. **No `TrapFrameMut<'_>` is constructed here**
  — the trap-frame mutation is the platform's responsibility, in
  the trap-vector return path, after this function returns.
- `pub fn enter_userspace<P: TxPlatform>(merged: UserTrapContext)
  -> !` — the platform shim. On RV64 today this builds an
  `Rv64TrapFrame` from `UserTrapContext` and calls the existing
  `tx_hal_riscv64_qemu_virt::return_to_userspace(&frame)` (the
  actual symbol; the brief said "enter_userspace" but the platform
  symbol is `return_to_userspace`). Add a portable alias
  `TrapIf::enter_userspace_with_context` so the kernel does not
  reach into the board crate directly. The wrapper this exposes is
  the second site of the two-site discipline
  (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`).

### Wiring

Sequence per scheduler tick:

1. Reactor's per-hart poll loop
   (`Reactor::run_until_idle_on_hart_with_reschedule` at
   `crates/tx-reactor/src/runtime.rs:388-403`) picks a `TaskKey`
   and calls `future.as_mut().poll(&mut cx)`.
2. `PerHartSlotted` wrapper sets
   `current_thread_payload(hart) = Some(payload)`.
3. The thread future runs to its next `.await` — either
   `wait.await` (waiting on a userspace round-trip) or an inner
   `.await` from the syscall dispatch.
4. The wrapper clears `current_thread_payload(hart)` and returns
   `Poll::{Ready,Pending}` to the reactor.
5. When a userspace trap fires, the trap-shell
   (`crates/tx-kernel/src/trap.rs::KernelTrapDispatcher::on_syscall
   /on_page_fault`) reads `current_payload_for_hart(hart)`
   (already wired), captures
   `view.capture_user_context()` into `payload.saved_user_context`,
   resolves the wait via `slot.complete_interesting_trap`, and
   returns `TrapAction::Reschedule`. Already implemented in
   `trap_handoff::hand_off_syscall` /`hand_off_user_pf`.
6. Reactor next-poll resumes the thread future at the `.await`.
   On `UserspaceTrapInfo::Syscall(req)` it runs
   `linux_syscall::dispatch(req, &ctx).await`. The dispatch
   currently lives at `crates/tx-shims/src/linux_syscall/mod.rs`
   and already returns a `SyscallResult` enum.
7. Thread future stores
   `payload.store_pending_syscall_return(Some(...))`, calls
   `prepare_userspace_entry_payload`, then re-issues
   `start_request` and `enter_userspace_with_context`. Per
   `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE` the shim is the
   *only* site that mutates the fresh trap frame's user-visible
   registers.

### ThreadPayload usage / new accessors

Existing accessors (already in
`crates/tx-subsystems/src/thread_runtime/structure.rs`):

- `userspace_slot()`, `active_userspace_request`,
  `set_active_userspace_request`, `saved_user_context`,
  `store_saved_user_context`, `store_pending_syscall_return`,
  `drain_pending_syscall_return`. All sufficient.
- `set_current_thread_payload`, `clear_current_thread_payload`,
  `current_thread_payload`. Sufficient — the wrapper consumes
  these directly.
- `task: SpinMutex<Option<TaskKey>>` already present; add a
  setter `pub fn install_task_key(&self, key: TaskKey)` so
  `spawn_thread_task` can record the handle without poking the
  `pub(crate)` field.

New helpers (small):

- `ThreadPayload::install_task_key(&self, key: TaskKey)` — public
  setter for the existing `task` slot.
- A `core::future::poll_fn`-shaped `prepare_userspace_entry`
  helper that the thread future calls inline (does not need to be
  pub; lives next to `thread_future`).

### Tests

- `crates/tx-subsystems/src/thread_runtime/tests.rs` (extend):
  - `task_wrapper_sets_per_hart_slot_around_poll`. Spawns a
    thread future that asserts `current_thread_payload(0).is_some()`
    inside its body and `is_none()` outside; runs the reactor once.
  - `thread_future_drains_pending_syscall_return_into_user_context`.
  - `thread_future_round_trips_syscall_via_real_dispatch`.
- `crates/tx-kernel/src/init/tests.rs` (replace existing
  `boot_smoke_userspace_round_trip_writes_console_then_exits`):
  same syscall sequence, but driven by
  `Reactor::submit_task(thread_future(...))` and a host
  test-only `enter_userspace` adapter that drains the entry-shim
  payload into a synthetic `UserspaceTrapInfo::Syscall(req)`.
  The assertion shape is unchanged: `b"hi\r\n"` on the captured
  console, `init.is_zombie()` with `ExitStatus::Exited(0)`. The
  `payload_cap_for_test` and manual `set_current_thread_payload`
  calls are gone.

## Part 2 — Page-fault async dispatch (item 3)

### Surface

Inside the thread future's match arm
(`UserspaceTrapInfo::PageFault(info)`):

- Resolve `aspace = process.aspace()` (already on
  `ProcessPayload`).
- Build `VmFault { addr: UserAddr, kind: VmFaultKind, ... }` from
  the reactor-side `PageFaultInfo`.
- `match aspace.fault_script(fault).await { ... }`. The function
  already exists at `crates/tx-subsystems/src/vm/execution.rs:152`
  with signature
  `pub async fn fault_script(&self, fault: VmFault) ->
  Result<PmapPublishOutcome, VmFaultError>`.

### Wiring

- The trap shell already snapshots context and resolves the wait
  with `UserspaceTrapInfo::PageFault(info)` (Phase 1 of trio,
  `crates/tx-kernel/src/trap_handoff.rs:214`).
- Thread future arm:
    - `Ok(_)` (publication succeeded) → loop back to the
      userspace-entry checkpoint without writing a syscall return.
      `pending_syscall_return` is `None`.
    - `Err(VmFaultError::WouldBlock)` is internally retried by
      `fault_script`'s loop; it never reaches the caller.
    - `Err(_other)` → call
      `crate::process::execution::step_exit_group_with_signal(&process,
      Signum::SIGSEGV)` (already exists at
      `crates/tx-subsystems/src/process/execution.rs:483`).
      Thread future then resolves `Poll::Ready(())` and the task
      drops.

### AST interactions

Per `txdoc:THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT` (the real
anchor; brief misnamed it `THREAD-5-3-AST-CHECKPOINTS`): AST runs
at the userspace-entry checkpoint, not inside the page-fault arm.
The thread future calls
`payload.userspace_slot().checkpoint_userspace_entry(req,
ast_batch, |ast| { ... })` (existing surface in
`crates/tx-reactor/src/userspace.rs:373`) before the next
`enter_userspace_with_context` call. For this slice the
checkpoint's only valid `UserspaceEntryDecision` is
`EnterUserspace`; signal-handler frame setup (sigreturn, alt
stack) remains deferred. Default-action SIGSEGV from the
fault-arm route uses the existing `step_exit_group_with_signal`
path; no per-handler frame yet.

### Tests

- `crates/tx-subsystems/src/thread_runtime/tests.rs`:
  - `thread_future_pf_ok_loops_back_to_userspace_entry` — script
    a `PageFault` resolution with a recipe that materialises;
    assert the future re-issues `start_request` without writing
    a syscall return.
  - `thread_future_pf_err_routes_sigsegv_and_zombifies` — script
    a fault for an address with no recipe; assert
    `process.is_zombie()` with the SIGSEGV-derived exit status
    (`ExitStatus::TerminatedBySignal(SIGSEGV)`).

## Part 3 — VFS walker / `step_open` (item 8)

### Surface

New module `crates/tx-subsystems/src/vfs/walker.rs` (lives next to
`execution.rs`):

- `pub async fn step_walk(rooted_at: Cap<DEntry>, path: &[u8],
  cred: &Credential, guard: &Guard<'_>) -> StepOutcome<Cap<DEntry>>`
  — full walker per `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`.
- `pub async fn step_open(rooted_at: Cap<DEntry>, path: &[u8],
  flags: OpenFileFlags, mode: u16, cred: &Credential, guard:
  &Guard<'_>) -> StepOutcome<Cap<OpenFile>>` — calls `step_walk`,
  then materialises the `OpenFile` over the returned RNode (the
  `RNodeBacking::StructBacked { Tty }` and `Directory` branches
  already work today; tmpfs page-backed branches still return
  `ENOSYS` per Phase 3a — that is fine for this slice).
- The existing `OpenFile::step_read` / `step_write` dispatch
  needs no change.

### Wiring

- Path component iteration: split on `/`; honour leading `/` as
  "absolute, restart at root" (the `/` root is reachable via the
  `init.cwd()` call returning a DEntry whose parent chain leads
  to root via `containing_mount.parent_mount()`).
- Mount-point crossing per
  `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1` and
  `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`: when the current
  DEntry has `mounted: Some(Weak<MountIdentity>)` set, the walker
  upgrades and switches to that mount's root RNode. The `mounted`
  hint is set during `mount_devfs_at_dev` (already wired).
- Symlink chasing: walker chases up to `SYMLOOP_MAX = 40` hops
  (POSIX default), then returns `Errno::ELOOP`. Implementation: a
  per-walk hop counter incremented each time the walker observes
  `RNodeBacking::Symlink { target }`; on hit, the walker substitutes
  the target into the remaining path component stream and continues.
  Absolute-target symlinks restart from the rooted-at DEntry's mount
  root; relative-target symlinks resolve against the symlink's
  parent DEntry. The slice does not yet reach symlinks on the
  bootstrap path (`/dev/console` is not a symlink), but the
  semantics are correct so future paths work. (Decided per user
  direction 2026-05-06; was previously "ELOOP immediately on
  Symlink".)
- Permission check: `Credential` is threaded through but the slice
  defers the actual mode-check to "always allow for init"; matches
  the existing `bind_init_cwd_and_root` surface today.
- Replace `tx_fs::devfs::open_console_for_init`:
  - Keep the function name and signature.
  - Body becomes `let init = process::execution::init_process()?;
    let root = init.cwd_or_root(); step_open(root,
    b"/dev/console", RDWR, 0, init.cred(), &guard).await`.
  - Init's bootstrap (`bind_init_cwd_and_root` in init.rs) keeps
    invoking it for fds 0/1/2 — same call site, same returned
    `Cap<OpenFile>`.

### Tests

- `crates/tx-subsystems/src/vfs/tests.rs` (extend):
  - `step_walk_resolves_dev_console_to_devfs_root_then_console`
    — mounts tmpfs+devfs as in the trio's boot smoke, walks
    `/dev/console`, asserts the returned DEntry's RNode backing
    is `StructBacked { Tty(...) }` with the registered console
    cap.
  - `step_walk_crosses_mount_point_at_dev`.
  - `step_walk_chases_relative_symlink_to_target`.
  - `step_walk_chases_absolute_symlink_from_root`.
  - `step_walk_returns_eloop_after_40_hops` — synthesise a 41-deep
    symlink chain in tmpfs; assert `Errno::ELOOP`.
  - `step_walk_returns_enoent_for_missing_component`.
  - `step_open_round_trips_tmpfs_create_then_open` — `mkdir
    /tmp` (via tmpfs `FsOps::mkdir`), `step_open(/tmp/foo,
    O_CREAT|O_RDWR, 0o644)`, write, close, `step_open(/tmp/foo,
    O_RDONLY)`, read.
- `crates/tx-fs/src/devfs/tests.rs` (extend):
  - `open_console_for_init_now_goes_through_walker` — assert the
    function's call site visits `step_open` (exposed via a
    test-only spy or counter on the walker).

## Part 4 — IRQ dispatch + UART RX (item 9)

### Surface

- HAL already exposes `IrqDispatchTable`, `IrqHandlerFn`,
  `IrqHandled`, and `IrqIf::install_dispatch_table` at
  `crates/tx-hal/src/lib.rs:1109-1162`. RV64 board's
  `Platform::dispatch_irq` already consults the installed table at
  `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:521-534`. **No new
  HAL trait surface is required for this slice.**
- New file `crates/tx-kernel/src/irq.rs`:
    - `static IRQ_DISPATCH_TABLE: SpinMutex<IrqDispatchTable>`
      using `tx_substrate::SpinMutex` (lands together with Part 5
      so the trio's TAS shims can be deleted in the same PR; see
      Phasing item 1).
    - `pub fn register_irq_handler(irq: u32, handler:
      IrqHandlerFn)` — populates `IRQ_DISPATCH_TABLE.entries[irq]`.
      Idempotent on identical handler; panics on conflict.
    - `pub(crate) fn install_irq_handlers<P: TxPlatform>()` — the
      one-shot boot step: calls
      `register_irq_handler(<P as IrqIf>::UART_IRQ,
      uart_rx_irq_handler)`, then
      `<P as IrqIf>::install_dispatch_table(&IRQ_DISPATCH_TABLE)`,
      then `<P as IrqIf>::set_priority(<P as IrqIf>::UART_IRQ, 1)`
      and `unmask(<P as IrqIf>::UART_IRQ)`. The IRQ number flows
      from the platform parameter; tx-kernel never names a board
      constant directly.
- New file `crates/tx-kernel/src/uart_rx.rs` (or in the board
  crate):
    - `pub fn uart_rx_irq_handler(irq: u32) -> IrqHandled` —
      drains UART RX bytes via
      `tx_hal::ConsoleIf::read_bytes`, then calls
      `tty::execution::step_ingest(&console_tty, &bytes,
      &guard)`. Returns `IrqHandled::Wake` on any byte ingested
      (so `KernelTrapDispatcher::on_external_irq` returns
      `TrapAction::Reschedule` and the reactor wakes the blocked
      `read`). Returns `IrqHandled::NotMine` if no console TTY is
      registered.
    - The UART IRQ number is platform-specific and is exposed via
      a new associated const on the HAL trait (`tx-hal/src/lib.rs`):
      `trait IrqIf { const UART_IRQ: u32; ... }`. The board crate
      `boards/tx-hal-riscv64-qemu-virt/src/lib.rs` provides
      `impl IrqIf for Platform { const UART_IRQ: u32 = 10; ... }`
      (QEMU virt 16550 mapping; verify against the board's
      existing PLIC constants). Future IRQ-driven devices extend
      `IrqIf` with more associated consts (`TIMER_IRQ`, etc.) or
      collapse into an `IrqMap` const struct once there are
      several. Plan locks in: tx-kernel reads
      `<P as IrqIf>::UART_IRQ` only — no `pub const` constant in
      tx-kernel and no per-board cfg in tx-kernel.

### Wiring

- `crates/tx-kernel/src/init.rs::CoreInit::init_substrate_if_ready`:
  add `Self::install_irq_handlers()` after
  `register_console_hardware()` and before
  `mount_rootfs_tmpfs()`. Order: the IRQ handler reads
  `console_tty()` from the existing `BootSpinMutex<Option<Cap>>`
  slot, so registration must follow the slot being populated by
  `register_console_hardware`.
- `KernelTrapDispatcher::on_external_irq`
  (`crates/tx-kernel/src/trap.rs:47`) is already correct: it
  claims the IRQ from the PLIC, calls `P::dispatch_irq(irq)`
  (which walks the installed table), then completes. The only
  change is to route `IrqHandled::Wake` correctly — already
  done.
- `tty::execution::step_read` blocking semantic: when the input
  queue is empty, `step_read` already returns
  `Blocked(WaitToken::new(tty.raw() as u64, TTY_READABLE))`
  (`crates/tx-subsystems/src/tty/execution/step_read.rs:46`).
  `step_ingest` already calls
  `tty.input_readable.fire(TTY_READABLE)` after pushing bytes
  (`crates/tx-subsystems/src/tty/execution/step_ingest.rs:49`).
  The wait-carrier mechanism is already wired.
- The trio's `read` short-circuit
  (`crates/tx-shims/src/linux_syscall/mod.rs:412-422`,
  `TODO(phase-blocking-read)`) is replaced with a `wait_on_token`
  loop. The carrier is the same `WaitToken` shape; the reactor's
  generic await (the same shape `vm::execution::fault_script`
  uses for `RangeLock::WouldBlock`) drives it. Re-poll
  `step_read` after each wake.
- `unmask` the UART IRQ at boot via
  `<P as IrqIf>::unmask(<P as IrqIf>::UART_IRQ)` and
  `set_priority(<P as IrqIf>::UART_IRQ, 1)` inside
  `install_irq_handlers`.

### Tests

- `crates/tx-kernel/src/irq/tests.rs` (new):
  - `register_irq_handler_populates_dispatch_table_slot`.
  - `install_irq_handlers_publishes_table_to_platform`.
  - `dispatch_irq_routes_uart_rx_to_tty_step_ingest` (host: a
    fake `Platform` impl with a queueable `read_bytes` source;
    inject one byte; call `dispatch_irq(VIRT_UART_IRQ)`; assert
    the registered console TTY's input queue grew by one).
- `crates/tx-kernel/src/init/tests.rs` (extend):
  - `boot_smoke_blocked_read_unblocks_on_uart_rx` — the
    end-to-end smoke that the brief calls "the existing host
    smoke is replaced by one that doesn't synthesise the
    driver". Sequence: spawn init's thread future via
    `Reactor::submit_task`; thread issues
    `read(0, buf, 8)`; the future blocks on the wait-carrier;
    test injects one byte through the fake platform's UART RX;
    `dispatch_irq` fires; `step_ingest` runs; carrier wakes;
    reactor re-polls; `read` returns 1.

## Part 5 — Minor cleanups (items 4 / 5 / 6 / 10)

### tx-substrate public SpinMutex (item 4)

**Decision (Open Q #5):** move `SpinMutex` into `tx-substrate`. A
spin mutex is a synchronization primitive with no semantic content
and no entity ownership; per `CONCEPTS_v4.md` ("substrate provides
zone, index, epoch, mutation, bus, page, and reservation
primitives") this is unambiguously substrate territory. The current
location in `tx_subsystems::sync` is a layering artefact from
first-need; leaving it there forces every future substrate-internal
site that wants a mutex to either reach upward (forbidden) or grow
another shim.

In-tree state today (verified):
- `crates/tx-subsystems/src/sync.rs:4` defines `SpinMutex` /
  `SpinMutexGuard` as `pub(crate)`.
- `crates/tx-kernel/src/init.rs::BootSpinMutex` (~lines 77–127) is
  a duplicate inline TAS shim.
- `crates/tx-fs/src/tmpfs.rs::SpinMutex` (~lines 47–80) is another
  duplicate inline TAS shim.

Mechanical move:

1. Create `crates/tx-substrate/src/sync.rs` containing the
   relocated `SpinMutex` + `SpinMutexGuard` as `pub`. Match the
   existing impl byte-for-byte (atomic exchange acquire/release;
   spin loop hint; no poisoning — the kernel doesn't unwind).
2. `crates/tx-substrate/src/lib.rs`: `pub mod sync;` and add
   `pub use sync::{SpinMutex, SpinMutexGuard};` at the crate root
   so callers can reach it as `tx_substrate::SpinMutex`.
3. Delete `crates/tx-subsystems/src/sync.rs` and the
   `pub(crate) use sync::SpinMutex` re-exports it spawned. Replace
   all `tx_subsystems::sync::SpinMutex` import sites with
   `tx_substrate::SpinMutex`.
4. Delete `crates/tx-kernel/src/init.rs::BootSpinMutex` and its
   guard; re-point every consumer (the `INIT_PROCESS`,
   `ROOT_MOUNT`, `DEV_MOUNT`, `CONSOLE_TTY` slots and the new
   `IRQ_DISPATCH_TABLE` lock from Part 4) to
   `tx_substrate::SpinMutex`.
5. Delete `crates/tx-fs/src/tmpfs.rs::SpinMutex` and re-point the
   tmpfs directory + inode-store locks to
   `tx_substrate::SpinMutex`.
6. Audit other call sites: `tx-subsystems`'s `process`, `signal`,
   `thread_runtime`, `tty`, `mount` modules currently use the
   `pub(crate) use crate::sync::SpinMutex` re-export — flip them
   to `tx_substrate::SpinMutex`.

Crate-dep impact: `tx-substrate` already has no upstream deps from
this; consumers (`tx-kernel`, `tx-subsystems`, `tx-fs`) already
depend on `tx-substrate`, so no Cargo.toml changes are needed
beyond removing the now-dead `mod sync;` in `tx-subsystems`.

Total churn: ~50–80 lines mostly in `use` statements + the move.
Single PR.

### Errno EEXIST + ENOTEMPTY (item 5)

- `crates/tx-subsystems/src/execution.rs:10-27` — extend the
  `Errno` enum with `EEXIST` and `ENOTEMPTY` (POSIX numbers 17
  and 39 respectively when serialising to Linux ABI; the enum
  itself is symbolic).
- `crates/tx-fs/src/tmpfs.rs` — flip the call sites that today
  return `EINVAL` (for "name already exists") to `EEXIST`, and
  the call site in `rmdir` that returns `EBUSY` for "directory
  not empty" to `ENOTEMPTY`.
- `crates/tx-shims/src/linux_syscall/mod.rs::errno_to_i32` —
  add the two new arms (`EEXIST → 17`, `ENOTEMPTY → 39`).
- Tests: extend `tmpfs_unlink_drops_inode` and the directory
  tests to assert the new error variants.

### InlineName: Ord (item 6)

- `crates/tx-subsystems/src/vfs/structure.rs:208-249` — derive
  `Ord, PartialOrd` on `InlineName`. The lexicographic byte
  ordering on the active-prefix slice is the intended semantic
  (`as_bytes()`-shaped); a manual `impl Ord` is required because
  the inline `[u8; VFS_NAME_MAX]` makes the derive include
  trailing zero bytes. Implementation: `impl Ord` calls
  `self.as_bytes().cmp(other.as_bytes())`.
- `crates/tx-fs/src/tmpfs.rs` — flip the directory layer key
  from `Vec<u8>` to `InlineName`. Caller sites: tmpfs's per-dir
  `BTreeMap<TmpfsName, FsObjectId>` becomes
  `BTreeMap<InlineName, FsObjectId>`.

### MountId / DevId allocators (item 10)

- `crates/tx-subsystems/src/mount.rs` — add static
  `NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1)` and
  `NEXT_DEV_ID: AtomicU32 = AtomicU32::new(1)`. Both reserve `0`
  for "unset/sentinel".
- `pub fn allocate_mount_id() -> MountId` and `pub fn
  allocate_dev_id() -> DevId` (matching the existing
  `allocate_tid` shape in
  `crates/tx-subsystems/src/thread_runtime/structure.rs:350`).
- Bootstrap: `init.rs::mount_rootfs_tmpfs` claims `MountId(1)` /
  `DevId(1)` first, `mount_devfs_at_dev` claims `(2, 2)`.
  Subsequent mounts (none in this slice) use the allocator.
  Preserving the literal `MountId(1)/(2)` boot-time values keeps
  the trio's commit-history boot smoke `==` assertions valid
  without churn.

## Cross-cutting risks

1. **Per-hart slot lifetime / `Send` discipline.** The
   `PerHartSlotted` adapter sets `current_thread_payload(hart)`
   synchronously inside `Future::poll`, before forwarding to the
   wrapped thread future. Because the thread future itself can
   `.await` (and resume on a different hart in a future SMP-aware
   build), the slot must be cleared **on every poll exit**, not
   only on `Ready`. The code path above clears unconditionally
   after `inner.poll`. The slot is `PayloadCap<ThreadPayload>` —
   a Cap is `Send` and safely cloneable across yields per zone
   invariants. The slot is **not** held across an `.await`; the
   wrapper's own poll implementation never calls `.await`
   itself.

2. **`TrapFrameMut<'_>` escape.** `TrapFrameMut` is `!Send`. The
   userspace-entry shim does not produce a `TrapFrameMut`: it
   produces a `UserTrapContext` (a `Pod` value, `Send`+`Copy`).
   The `TrapFrameMut` is constructed only inside the platform's
   trap-vector return path, synchronously, never across an
   `.await`. The boundary is the
   `enter_userspace_with_context(merged: UserTrapContext) -> !`
   surface — past this call the platform owns the raw frame.

3. **AST drain ordering vs userspace re-entry.** The thread
   future must call
   `slot.checkpoint_userspace_entry(req, ast_batch, decide)`
   *before* `prepare_userspace_entry_payload` writes any pending
   syscall return — otherwise a signal posted between the
   syscall resolution and userspace re-entry can be observed too
   late. The checkpoint surface in
   `crates/tx-reactor/src/userspace.rs:373` already enforces
   "consume AST then decide", and an `EnterUserspace` decision
   is the only valid one for this slice. Tests must script a
   synthetic AST-pending case and assert the checkpoint runs
   before any `pending_syscall_return` drain.

4. **IRQ registration races at boot.** `install_irq_handlers`
   must precede the first trap that could fire on this
   IRQ. Today the kernel has no in-flight IRQ source until UART
   RX bytes are physically present; on QEMU virt, `unmask` is
   gated by `install_irq_handlers` itself. Order in
   `init_substrate_if_ready`:
   `register_console_hardware` (creates the TTY the IRQ handler
   ingests into) → `install_irq_handlers` (registers the
   handler, installs the table, then unmasks) → rest of boot.
   The PLIC's enable bits are zero until `unmask`, so a stray
   pre-registration trap is structurally impossible.

5. **Walker ↔ devfs bootstrap chicken-and-egg.** `step_open` for
   `/dev/console` needs the mount table populated, the console
   alias registered, and init's cwd bound. Today
   `bind_init_cwd_and_root` runs `step_chdir` then calls
   `open_console_for_init` for fds 0/1/2 (`init.rs:540-549`).
   When `open_console_for_init` becomes a wrapper over
   `step_open`, the same call order works: by the time fds are
   preopened, all three preconditions hold. Verify the function
   is no longer called from anywhere that runs before
   `mount_devfs_at_dev`.

6. **Cap-vs-IdentRef discipline.** The thread future holds
   `Cap<ThreadIdentity>`, `Cap<ProcessIdentity>`, and
   `PayloadCap<ThreadPayload>` across `.await`. All three are
   epoch-managed and safe across yields. **No `IdentRef<'g, _>`
   crosses an `.await`**: each `step_*` call site that needs a
   guard freshly calls `tx_substrate::epoch::guard()` inside the
   step (the existing pattern in `vm::execution::fault_script`).
   The walker's `step_walk` follows the same discipline: each
   loop iteration takes a fresh guard, observes the dentry+rnode
   layer for one component, drops the guard before any
   `.await`.

7. **HAL portable userspace-entry symbol.** `tx-hal` does not
   expose a portable `enter_userspace` /
   `return_to_userspace` trait method today. The RV64 board has
   `tx_hal_riscv64_qemu_virt::return_to_userspace` (concrete
   symbol). This slice must add a `TrapIf::enter_userspace_with_
   context(ctx: UserTrapContext) -> !` default method that the
   RV64 board overrides to call its existing function. Other
   platforms (none built today) get the default-panicking
   stub. Surface this addition under HAL boundary review per
   `tx-hal-axhal` skill.

8. **Doc-vs-code anchor mismatch.** The brief cited
   `txdoc:THREAD-5-3-AST-CHECKPOINTS`; the actual anchor is
   `txdoc:THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT`. The plan
   uses the real anchor. No doc edit required for this slice;
   a separate `tx-meta-alignment` follow-up may want to add an
   `AST-CHECKPOINTS` alias if downstream materials want it.

## Out of scope (deliberately deferred)

- ELF loader (`execve` syscall, image parsing, entry trampoline,
  auxv build-up, stack canonicalisation).
- `fork`, `clone`, `wait4`, `waitid` syscalls.
- Real init binary load from initramfs / cpio.
- Userspace signal-handler frame setup (sigreturn, alt stack).
- General `copy_from_user` / `copy_to_user`. Walker keeps using
  bounded inline buffers in the bootstrap moment, same as the
  trio's `TTY_WRITE_MAX_INLINE = 4096`.
- Per-handler signal mask wiring (`sa_mask` decode shipped in
  Phase 2b but is not yet honoured by the delivery path).
- SMP runtime (multi-hart). The plan assumes single-hart for the
  slice, same as the trio. The per-hart slot machinery is in
  place but tested only on hart 0.
- Reclaim and writeback policy in PageBacked (`§8` and `§12`,
  explicitly deferred by design).
- Walker symlink chasing (returns `Errno::ELOOP` for now).
- Walker permission checks (cred is threaded but the slice
  defers the actual mode-bit gate — same as today's
  `bind_init_cwd_and_root`).

## Phasing

Each step is a self-contained PR. Land in this order; later parts
benefit from earlier ones (e.g. Part 2 depends on Part 1's task
wrapper; the IRQ wake path in Part 4 only meaningfully tests
under Part 1's reactor-driven scheduling).

1. **M — Part 5 minor cleanups.** Errno extension +
   `InlineName: Ord` + tmpfs key flip + move `SpinMutex` into
   `tx_substrate` (per Open Q #5 decision) + retire the two
   in-crate TAS shims. Opportunistic landing that benefits later
   parts. Estimated 1 PR, ~150 LOC net.
2. **L — Part 1 reactor task wrapper + userspace-entry shim.**
   The big one. Replaces the Phase 6 fake driver. Adds the
   `PerHartSlotted` adapter, `thread_future`,
   `prepare_userspace_entry_payload`, the
   `TrapIf::enter_userspace_with_context` portable surface, and
   the RV64 board override. Tests rewrite the Phase 6 smoke.
3. **M — Part 2 page-fault async dispatch.** Depends on Part 1's
   task wrapper. Adds the `PageFault` arm in `thread_future`,
   wires `step_exit_group_with_signal(SIGSEGV)` on `Err`. Two
   targeted thread-future tests.
4. **M — Part 3 VFS walker `step_walk` + `step_open`.** Retires
   `open_console_for_init`'s bootstrap exemption. Walker tests
   for tmpfs+devfs cross-mount; existing boot smoke remains
   green because `open_console_for_init` becomes a thin
   wrapper.
5. **L — Part 4 IRQ dispatch table walked at boot + UART RX
   ingest.** Adds `crates/tx-kernel/src/irq.rs`,
   `install_irq_handlers`, the UART RX handler, the blocking
   `read` re-poll loop. End-to-end smoke that an injected UART
   RX byte unblocks a blocked `read` on init's fd 0.
6. **S — Part 5 mount/dev id allocators.** Bundled into Part 5
   chronologically but landed as its own PR because it touches
   `init.rs` boot ordering and is independent of the other
   minors.
7. **S — End-to-end host smoke.** A single
   `tx-kernel` test that exercises trap shell → reactor task
   wrapper → thread future → `linux_syscall::dispatch` → tty +
   devfs through real walker → real RX path, with no
   synthesised driver. Combines Parts 1–4. Mostly an
   integration-test stitch; small as new code.

## Open questions

1. **Task wrapper poll site — DECIDED 2026-05-06: reactor-tick.**
   `KernelTrapSink::on_syscall` returns `TrapAction::Reschedule`;
   the next `Reactor::run_until_idle_on_hart_with_reschedule`
   loop iteration polls the resolved thread future. Rejected the
   sync-in-trap-shell alternative because:
   (a) preemption-transparency per `txdoc:REACTOR-PREEMPTION-TRANSPARENCY`
   requires the trap shell stays minimal and never holds the
   scheduler-loop mutex semantics;
   (b) signals posted between syscall resolution and userspace
   re-entry must traverse the AST checkpoint, which lives in the
   thread future, not the trap shell;
   (c) the same path is what page-fault dispatch and IRQ-driven
   wakeups use — single, uniform shape.

2. **Where the entry shim writes `a0` — DECIDED 2026-05-06: HAL
   hook only.** Only `TrapIf::enter_userspace_with_context` writes
   a0, by merging the drained `pending_syscall_return` into the
   merged `UserTrapContext` before sret. The trap-shell-side
   alternative is rejected because it has a real correctness bug,
   not just a code-shape one: writing a0 onto the trapping frame
   *before* the AST checkpoint runs breaks Linux EINTR semantics
   for syscalls that wake on signal rather than completion.
   Userspace would observe a successful return *and then* the
   signal handler, when the signal was supposed to short-circuit
   the return. The HAL-hook discipline matches
   `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`: one write site, one
   merge point, AST drain before either runs.

3. **Walker symlink semantics — DECIDED 2026-05-06: chase up to
   `SYMLOOP_MAX = 40` hops, then `ELOOP`.** POSIX default. The
   walker maintains a per-walk hop counter and substitutes the
   symlink target into the remaining component stream on each hit
   (absolute targets restart from the rooted-at mount root;
   relative targets resolve against the symlink's parent). See
   Part 3's "Wiring" + tests
   `step_walk_chases_relative_symlink_to_target`,
   `step_walk_chases_absolute_symlink_from_root`,
   `step_walk_returns_eloop_after_40_hops`. The bootstrap
   `/dev/console` path has no symlinks, so the slice is safe
   regardless; this lands the correct semantics so future paths
   work.

4. **IRQ handler registration mechanism — DECIDED 2026-05-06:
   explicit registration in `init::install_irq_handlers`.** A
   single centralised init step in `init_substrate_if_ready`
   calls `register_irq_handler(irq, handler)` per device, then
   `<P as IrqIf>::install_dispatch_table(&IRQ_DISPATCH_TABLE)`.
   Linkme rejected for seven reasons:

   - **Test substitutability.** Linkme aggregates at link time
     and is immutable at runtime; under `cargo test` every test
     binary's compiled units include every `#[distributed_slice]`
     in the dep graph. Tests can't construct a controlled subset
     of handlers and can't reset the table between runs. Explicit
     registration lets each test build exactly the table it wants.
   - **Boot ordering.** Linkme runs at static-init time via
     `.init_array` / `__mod_init_func` / `.CRT$XCU`. txKernel has
     explicit `CoreInit` ordering (substrate → process → console
     hardware → mount wiring → IRQ install). The IRQ handler reads
     `CONSOLE_TTY` from a `BootSpinMutex<Option<Cap>>` slot that is
     populated by `register_console_hardware`; "register handler"
     and "register the device the handler talks to" must stay in
     the same ordered phase, which linkme cannot express.
   - **No-std / RV64 bare-metal section conventions.** Linkme
     depends on host-toolchain link sections. On RV64 bare-metal
     ELF the kernel chooses its own layout via the linker script;
     layering linkme on top creates collision risk and adds an
     opaque dependency on host-platform linker behaviour to a
     hand-rolled cold-start sequence.
   - **Discoverability / review surface.** `grep -r
     "register_irq_handler"` should show every handler. Linkme
     makes the table opaque to grep — any crate (including
     transitive deps) can register by adding `#[distributed_slice]`.
     IRQ dispatch is a kernel security boundary; explicit,
     reviewed, single-call-site registration is mandatory.
   - **Mutation / hot-plug.** IRQ registration must eventually
     support runtime mutation (kernel modules, hot-plug devices,
     dynamic device-tree probing). Linkme is read-only by design.
     Explicit registration scales naturally; linkme would force a
     future migration.
   - **Per-platform IRQ numbering.** UART IRQ is platform-specific
     (RV64 QEMU virt: 10; LoongArch: different; K210: different).
     Linkme registration sites need the number at compile time;
     `init::install_irq_handlers<P: TxPlatform>` has the platform
     in scope and pulls the number from platform constants —
     clean late-binding, no per-board cfg dance.
   - **txKernel-specific rule.** The long-term plan's
     `trap-shell-and-kernel-sink` step explicitly says "Keep trap
     dispatch out of linkme." IRQ dispatch is part of the
     trap-side machinery (called from
     `KernelTrapDispatcher::on_external_irq`). Same rule applies.

5. **`tx_substrate` vs `tx_subsystems` for the public
   `SpinMutex` — DECIDED 2026-05-06: move into `tx_substrate`.**
   A spin mutex is a synchronization primitive with no semantic
   content and no entity ownership — that is the textbook
   substrate primitive per `CONCEPTS_v4.md` ("substrate provides
   zone, index, epoch, mutation, bus, page, and reservation
   primitives; semantic subsystems own entities and transitions").
   The `tx_subsystems::sync` location is a layering artefact from
   first-need; leaving it there forces every future
   substrate-internal site that needs a mutex to either reach
   upward (forbidden) or grow another shim. The cross-crate move
   is mechanical: relocate `tx_subsystems::sync::SpinMutex` to
   `tx_substrate::sync::SpinMutex`, flip its visibility to `pub`,
   rewrite `use` lines in tmpfs / init / thread-runtime / signal /
   process / tty, delete the two TAS shims
   (`tx-kernel/src/init.rs::BootSpinMutex`,
   `tx-fs/src/tmpfs.rs::SpinMutex`). Re-points the IRQ table's
   mutex too (Part 4's `IRQ_DISPATCH_TABLE` lock). ~50 lines of
   churn for a permanent layering fix.

6. **`VIRT_UART_IRQ` location — DECIDED 2026-05-06: associated
   const on `IrqIf`.** The IRQ number is a fact about the board's
   PLIC topology, not the kernel; it must come through the
   `P: TxPlatform` parameter. Concretely: add
   `IrqIf::UART_IRQ: u32` to the trait surface (axHal-style), the
   board crate provides the value (`10` for QEMU virt 16550,
   verified against `boards/tx-hal-riscv64-qemu-virt/src/`),
   `init::install_irq_handlers<P>` reads `<P as IrqIf>::UART_IRQ`.
   The free `pub const VIRT_UART_IRQ` alternative is rejected: it
   looks fine for one device but immediately fails when a second
   IRQ-driven device lands (timer, virtio, …) because tx-kernel's
   install code would have to know which platform it is compiling
   against. Associated-const late-binding sidesteps that. Future
   IRQs extend the trait with more associated consts, or — once
   there are several — collapse into a single `IrqMap` associated
   type / const struct.
