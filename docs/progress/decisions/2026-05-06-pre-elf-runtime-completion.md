# Pre-ELF Runtime Completion

**Date:** 2026-05-06
**Branch:** `feat/pre-elf-runtime`
**Plan:** [`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`](../plans/2026-05-06-pre-elf-runtime-completion.md)
**Status:** Complete (host-test scope; production loop wired and ready
for ELF). 7 phase commits on top of the planning chore. tx-kernel
19/19, tx-fs 13/13 serial, tx-shims 12/12, tx-subsystems 344/344
serial, tx-substrate sync 2/2. `cargo check --workspace`, `cargo
check -p tx-kernel-riscv64-qemu-virt --target
riscv64gc-unknown-none-elf`, `cargo fmt --check`, and `cargo xtask
progress validate` all clean.

## Goal

Land everything between the trio (which got us to a host-test fake
driver running write/exit through real subsystem code) and the ELF
loader (which would let init be a real binary). Specifically: the
production reactor task wrapper + userspace-entry shim that retire
the trio's Phase 6 fake driver, real page-fault async dispatch in
the thread future, the VFS walker so `/dev/console` resolves by path,
explicit IRQ dispatch + UART RX → tty ingest → blocking-read carrier,
and four bundled minor cleanups (Errno + InlineName: Ord + public
SpinMutex + MountId/DevId allocators). All open questions from the
planning phase locked in before implementation.

## What landed

### Wave 1 — Phase 1 minor cleanups + Phase 4 VFS walker (commit `04edaa6`)

Two phases bundled in one commit because they share files
(`vfs/structure.rs`, `execution.rs::Errno`, `process/structure.rs`,
`tmpfs.rs`, `linux_syscall/mod.rs`).

**Phase 1 — minor cleanups (Part 5 sub-items #4, #5, #6):**

- Move `SpinMutex` / `SpinMutexGuard` from `tx_subsystems::sync`
  (`pub(crate)`) into `tx_substrate::sync` (`pub`) per Open Q #5
  DECIDED. A spin mutex is a substrate primitive per `CONCEPTS_v4.md`
  ("substrate provides zone, index, epoch, mutation, bus, page, and
  reservation primitives; semantic subsystems own entities and
  transitions"). Re-exported as `tx_substrate::SpinMutex` at the
  crate root.
- Delete the two duplicate inline TAS shims:
  `tx-kernel/src/init.rs::BootSpinMutex` (51 LOC + guard impl) and
  `tx-fs/src/tmpfs.rs::SpinMutex` (51 LOC). Their slots
  (`ROOT_MOUNT`, `DEV_MOUNT`, `CONSOLE_TTY`, `CONSOLE_OPS` serial;
  tmpfs `state` lock) re-pointed.
- Rewrite 13 `use crate::sync::SpinMutex` import sites in
  `tx-subsystems` (signal, page_backed, wait_carrier, tty/structure
  ×3, vm/pmap, vm/structure ×2, thread_runtime/structure,
  process ×2) to `use tx_substrate::SpinMutex`.
- `Errno` extended with `EEXIST` (POSIX 17) and `ENOTEMPTY` (POSIX
  39); tmpfs flipped from `EINVAL`/`EBUSY` to the proper variants.
- `InlineName` grew a manual `Ord`/`PartialOrd` impl that compares
  `as_bytes()` (a derive would compare `len` first then include the
  trailing zero pad — wrong). tmpfs's per-directory map flipped from
  `BTreeMap<TmpfsName=Vec<u8>, FsObjectId>` to
  `BTreeMap<InlineName, FsObjectId>`.

**Phase 4 — VFS walker (Part 3):**

- New `crates/tx-subsystems/src/vfs/walker.rs` with
  `pub async step_walk(rooted_at, path, cred, guard) -> StepOutcome<Cap<DEntry>>`
  and
  `pub async step_open(rooted_at, path, flags, mode, cred, guard) -> StepOutcome<Cap<OpenFile>>`.
  Cites `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1` and
  `MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`.
- Mount-point crossing: added `mount::register_mount` /
  `mount::mount_for` registry that the walker consults alongside
  `child_dentry.mounted_hint()`.
- **Symlink chasing implemented** per Open Q #3 DECIDED: chase up to
  `SYMLOOP_MAX = 40` hops, then `Errno::ELOOP`. Absolute-target
  restart from the rooted_at's mount root; relative-target splice
  from the symlink's parent. `RNodeBacking::Symlink` target shape
  changed from `Box<InlineName>` to `Box<[u8]>` (InlineName rejects
  `/`). `Errno::ELOOP` (POSIX 40) added.
- New `FsOps::read_link` trait method, default-`ENOSYS`, with tmpfs
  override.
- `open_console_for_init` redirected through `step_open` with a
  synchronous `block_on` shim. Falls back to legacy direct-RNode
  path until init's mount is registered with `mount::register_mount`
  (wired in Phase 6).

10 new walker tests; tmpfs adds 2 errno tests; `tx-subsystems`
331 → 341 serial; `tx-fs` 10 → 13 serial; `tx-substrate` 2/2.

### Wave 2 — Phase 6 mount/dev id allocators + register_mount wire-up (commit `e757bc0`)

- New `static NEXT_MOUNT_ID: AtomicU64 = 1` and
  `static NEXT_DEV_ID: AtomicU32 = 1` in `mount.rs`. `pub fn
  allocate_mount_id() -> MountId` / `allocate_dev_id() -> DevId` use
  `fetch_add(1, Relaxed)` and return the pre-increment value.
  Deterministic-from-cold-start: first call returns `MountId(1)` /
  `DevId(1)`, second returns `(2)`, etc — the trio's existing
  assertions on `MountId(1)` / `MountId(2)` and `DevId(1)` /
  `DevId(2)` continue to hold without test churn.
- `init.rs::mount_devfs_at_dev` now snapshots
  `rootfs_payload = root_mount.payload().clone()` before
  `MountIdentity::new_cap` consumes the parent cap, then calls
  `mount::register_mount(&rootfs_payload, dev_object_id,
  dev_mount.clone())` immediately before publishing into
  `DEV_MOUNT`. Both `mount_rootfs_tmpfs` and `mount_devfs_at_dev`
  flipped from `MountId::new(N)` / `DevId::new(N)` to the
  allocators.
- Side-fix the agent caught: rootfs and devfs root rnodes were
  missing `with_containing_mount`, so the walker's `fs_ops_for`
  returned `None` and emitted `ENODEV` on the first interior
  component. Both now build via
  `RNode::new(...).with_containing_mount(&payload)` + manual
  `zone::reserve_for`/`sign_for`.
- New `FsOps::materialise_rnode` override on devfs (~50 lines): for
  `InodeKind::CharDevice`, recovers the alias index from
  `fs_object_id`, looks up the entry, and returns `RNodeBacking::
  StructBacked { Tty }`. With this + `register_mount` + the
  containing_mount fix, the walker resolves `/dev/console`
  end-to-end through real path lookup. The legacy fallback in
  `open_console_for_init_legacy` is now dead code on the boot path;
  left in place because its docstring already flags it as
  bootstrap-staging.

New boot smoke `boot_smoke_walker_resolves_dev_console_after_mount_registration`.
tx-kernel 8 → 9.

### Wave 3 — Phase 2 reactor task wrapper + userspace-entry shim + HAL hook (commit `abc9fb8`)

The keystone phase. Plan B writeback discipline (Open Q #2) and AST
drain ordering (Cross-cutting risk #3) are both observed.

**HAL extension** (`crates/tx-hal/src/trap.rs`):
- New `TrapIf::enter_userspace_with_context(_ctx: UserTrapContext) -> !`
  with default-panic impl. The single platform-side site that
  mutates user-visible registers per `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`.
- RV64 board impl (`boards/tx-hal-riscv64-qemu-virt/src/trap.rs`):
  build a fresh `Rv64TrapFrame`, call existing
  `restore_user_context(&ctx)`, then `return_to_userspace(&frame)`.

**Userspace-entry shim** (`tx-subsystems/src/thread_runtime/execution.rs:183`):
- `pub fn prepare_userspace_entry_payload(payload) -> UserTrapContext`.
  Drains via existing `drain_pending_syscall_return`, encodes
  Linux-ABI `a0` into `regs[USER_CONTEXT_A0_INDEX = 10]`: `Ok(v) →
  v as u64`, `Err(errno) → (-errno) as i64 as u64`. Clears
  `active_userspace_request`. Panics if `saved_user_context` is
  unset (thread-future invariant violation).

**Production thread future + per-hart slot** (`crates/tx-kernel/src/thread_future.rs`):
- `pub struct PerHartSlotted<F>` Future-impl wrapper that calls
  `set_current_thread_payload(hart, payload.clone())` before poll,
  clears on Ready, leaves set on Pending so the trap shell can find
  the payload while userspace executes.
- `pub async fn run_thread<P: TxPlatform>(payload)` loops:
  `start_request → await UserspaceRunWait → match UserspaceTrapInfo`:
  - `Syscall(req)`: build `SyscallCtx`, await
    `linux_syscall::dispatch`, store in `pending_syscall_return`;
    `NoReturn` returns from the future.
  - `PageFault(_)`: Phase 3 placeholder (replaced in Phase 3).
  - `Fatal`: same SIGSEGV shape.
  Then drain ASTs via `slot.checkpoint_userspace_entry_batch(req,
  AstBatch::default(), |_ast| EnterUserspace)` **before**
  `prepare_userspace_entry_payload` (Cross-cutting risk #3 — AST
  drain ordering). Then `prepare_userspace_entry_payload` +
  divergent `<P as TrapIf>::enter_userspace_with_context(ctx)`.
- Hybrid Shape B with explicit yield: pure Shape B with no `.await`
  was unworkable because the async transform never yields control
  to the trap shell. Awaiting `UserspaceRunWait` is the natural
  Pending point.
- `tx-shims` promoted from dev-dep to runtime dep of tx-kernel.
- 7 new tests (3 shim + 4 thread_future); tx-subsystems 341 → 344;
  tx-kernel 9 → 13.

Init wiring deferred to Phase 7 (per the brief).

### Wave 4a — Phase 3 page-fault async dispatch (commit `b49b302`)

Replaces the SIGSEGV-only PageFault placeholder Phase 2 left.

- `aspace.fault_script(VmFault).await` per `txdoc:VM-5-1-FAULT-HANDLER`
  + `VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`.
- `Ok(_)`: arm exits the match without `return`, falls through to
  the existing AST-drain → entry-shim → divergent enter tail.
  `pending_syscall_return` never touched on either branch — Plan B
  preserved (the merged context's `a0` comes from
  `saved_user_context` for fault returns).
- `Err(_)`: `step_exit_group_with_signal(&process, Signum::SIGSEGV)`
  per `SIGNAL_v1` §15.1, then return.
- New `pf_access_to_vm_access` bridges reactor's `PageFaultAccess`
  (Read/Write/Execute/**Unknown**) into vm's `AccessMode`
  (Read/Write/Execute), collapsing `Unknown → Read` defensively.
- `present: bool` from reactor's `PageFaultInfo` is dropped at the
  VM boundary; the fault script re-derives presence from the recipe.

3 new tests; tx-kernel 13 → 16.

### Wave 4b — Phase 5 IRQ dispatch + UART RX + sys_read blocking (commit `3cb95d6`)

Open Q #4 (no linkme — explicit `register_irq_handler`) and Open Q
#6 (`IrqIf::UART_IRQ` associated const, no free `pub const
VIRT_UART_IRQ` in tx-kernel) both enforced.

**HAL** (`tx-hal/src/lib.rs`):
- New `IrqIf::UART_IRQ: u32` associated const with default `0`.
  RV64 board: `const UART_IRQ: u32 = 10` (QEMU virt 16550).

**TTY wait carrier** (`tx-subsystems/src/tty/structure/identity.rs`):
- `TtyIdentity` grows `wait_channel: Channel` +
  `wait_carrier_id: u64`, registered via
  `wait_carrier::register_wait_channel` at construction.
- `step_read` embeds `tty.wait_carrier_id()` in its `WaitToken`
  (was `tty.raw() as u64`, an unregistered carrier id — the trio's
  blocking-read short-circuit was masking this).
- `step_ingest` fires `tty.wait_channel()` alongside the existing
  BIF-5 `RawQueue` readiness wire after every `LineCommitted` /
  `QueuedForRead` linearizer flush. Necessary — without it
  `wait_on_token` returns `None` and the new sys_read loop spins
  forever.

**IRQ dispatch** (new `crates/tx-kernel/src/irq.rs`):
- `static IRQ_DISPATCH_TABLE: SpinMutex<IrqDispatchTable>`.
- `pub fn register_irq_handler(irq, fn)` idempotent on identical
  fn-ptr; panics on conflict.
- `pub(crate) install_irq_handlers::<P: IrqIf + ConsoleIf>()`
  registers `uart_rx_irq_handler::<P>` under `<P as IrqIf>::UART_IRQ`,
  publishes table via `install_dispatch_table`, then
  `set_priority(.., 1)` and `unmask`.
- `uart_rx_irq_handler::<P>` drains up to 64 RX bytes via
  `<P as ConsoleIf>::read_bytes`, looks up `crate::init::console_tty()`,
  calls `step_ingest`. Returns `IrqHandled::Wake` on any byte ingested.

**Boot wiring** (`init.rs`): inserted `Self::install_irq_handlers()`
between `register_console_hardware()` and `mount_rootfs_tmpfs()`.
Boot trace: `:process:init:ok → :tty:console:ok → :irq:install:ok →
:mount:rootfs:tmpfs:ok → :mount:devfs:ok → :devfs:console:alias:ok →
:init:cwd:ok → :boot:ok`.

**`sys_read` blocking** (`tx-shims/src/linux_syscall/mod.rs`):
- Replaced trio's `TODO(phase-blocking-read)` `Done(0)` short-circuit
  with a `wait_carrier::wait_on_token(token).await` loop that
  re-polls `step_read`. Mirrors `sys_write`'s partial-success policy
  and `vm::execution::fault_script`'s carrier-await idiom.
- Replaced `dispatch_read_zero_when_console_empty_returns_zero` test
  with `dispatch_read_blocks_until_tty_input_then_returns_byte`
  (the old non-blocking semantic no longer holds).

3 new IRQ tests; tx-kernel 16 → 19.

### Wave 5 — Phase 7 end-to-end production smoke + kernel_main reactor loop (commit `69a3efd`)

**`kernel_main` → reactor loop** (`init.rs::run_userspace_reactor_loop`):
- Called from `boot()` after `boot_sentinel`.
- Fetches init's leader thread + payload via
  `ThreadIdentity::payload_cap()` (Phase 7 promotion of
  `payload_cap_for_test`), builds
  `PerHartSlotted::<P, _>::new(payload, run_thread::<P>(thread,
  payload))`, submits via `BOOT_REACTOR.with(|r| r.submit_task(...))`,
  drives the BSP hart-loop via `step_boot_reactor_once(current_cpu)`
  — the same shape secondary CPUs use; the plan's guesses
  (`run_until_idle_on_hart_with_reschedule` / `run_forever_on_hart`)
  do not exist.
- Loop exits on `init.is_zombie()`; emits `:userspace:exited:N`
  sentinel (where N is `ExitStatus::wait_status_word()`) before
  `system_off`.
- `:boot:ok` still emits unchanged. On RV64 boards with no
  userspace binary loaded yet, the loop parks in WFI — intended
  pre-ELF state.

**Structural fix in `run_thread`'s loop** (uncovered by end-to-end
driving): the Phase 2 shape called `checkpoint_userspace_entry_batch`
on the resolution-side `req_token` after `wait.await` had already
consumed the slot's active state (`UserspaceRunWait::poll` clears it
on Resolved). Restructured to use a fresh **entry-side** request
after dispatch, run AST checkpoint on it, hold its
`UserspaceRunWait` alive across the divergent
`enter_userspace_with_context` call so the trap shell can resolve
it on the next userspace trap. Each loop iteration now uses two
requests (resolution + entry).

**Test rewrite** (`init/tests.rs`):
- Deleted the trio Phase 6 fake-driver smoke
  `boot_smoke_userspace_round_trip_writes_console_then_exits`.
- Added new `boot_smoke_production_userspace_loop_writes_console_then_exits`
  using Option C (pragmatic limit-to-divergence): each `Future::poll`
  wrapped in `std::panic::catch_unwind`; `<TestPlatform as TrapIf>`
  override of `enter_userspace_with_context` captures the merged
  `UserTrapContext` into a static and panics with
  `SMOKE_YIELD_PANIC`. Per-syscall fresh future invocation
  sidesteps the host-test inability to resume an async state
  machine past a divergent call (in production the trap-vector
  return path provides resume).
- Asserts: post-OPOST `b"hi\r\n"` console capture; init zombies
  with `ExitStatus::Exited(0)`; production `run_thread` drives
  the loop; `PerHartSlotted` brackets each poll; walker → tty →
  ConsoleIf path for fd 1 write; merged `regs[10]` (RV64 a0)
  equals dispatcher's encoded return — the Plan B writeback
  correctness check the brief called primary.

tx-kernel stays at 19/19 (deleted fake driver + added production
smoke; net 0).

## Decisions

All six Open Questions decided 2026-05-06 before implementation:

- **Q#1 task wrapper poll site**: reactor-tick. `on_syscall` returns
  `TrapAction::Reschedule`; the next reactor loop iteration polls
  the resolved thread future. Sync-in-trap-shell rejected for
  preemption-transparency (`txdoc:REACTOR-PREEMPTION-TRANSPARENCY`),
  AST drain ordering, and uniform shape with page-fault + IRQ
  wakeups.
- **Q#2 a0 writeback site**: HAL hook only.
  `TrapIf::enter_userspace_with_context` is the only site that
  writes `a0`. Trap-shell-side writeback rejected for a real
  correctness bug — pre-writing `a0` onto the trapping frame
  before the AST checkpoint runs breaks Linux EINTR semantics.
- **Q#3 walker symlink semantics**: chase up to `SYMLOOP_MAX = 40`
  then `ELOOP` (POSIX default).
- **Q#4 IRQ registration mechanism**: explicit
  `register_irq_handler` in `init::install_irq_handlers`. NOT
  linkme. Seven reasons documented in the plan: test
  substitutability, boot ordering vs static-init,
  no-std/RV64 section conventions, discoverability/review
  surface, mutation/hot-plug, per-platform IRQ numbering, and
  the txKernel "keep trap dispatch out of linkme" rule.
- **Q#5 SpinMutex location**: move into `tx_substrate` (substrate
  primitive per `CONCEPTS_v4.md`).
- **Q#6 UART_IRQ location**: associated const `IrqIf::UART_IRQ` on
  the HAL trait. tx-kernel reads `<P as IrqIf>::UART_IRQ` only.

## Out of scope (deliberately deferred)

- ELF loader (`execve` syscall, image parsing, entry trampoline) —
  the **immediate next slice**. Once it lands, the production
  reactor loop will pick up the loaded binary on the first
  `enter_userspace_with_context` call.
- `fork`/`clone`/`execve`/`wait4` syscall drivers.
- Real init binary loading from initramfs.
- Userspace signal-handler frame setup (sigreturn, alt stack). The
  AST loop uses an empty-batch `AstBatch::default()` shortcut that
  only validates `EnterUserspace`; real per-task AST plumbing
  (`checkpoint_userspace_entry` against `&mut task.ast_slot`) needs
  the per-task slot exposure.
- General `copy_from_user` / `copy_to_user` for arbitrary VAs.
- `RawTrapFrame` and `TrapFrameMut` exposure as portable HAL
  surfaces (currently RV64 board internals).
- Kernel-mode fault recovery (`KERNEL_FIXUP_TABLE`).
- SMP runtime (multi-hart). The plan assumed single-hart for the
  slice; the BSP loop is the only one driven so far.

## Follow-ups in priority order

1. **ELF loader + `execve`** — unblocks first userspace.
   `tx-scripts/src/lib.rs` is still an 8-line stub. Replace
   bootstrap `brk_base` (`0x6000_0000`) with
   binary-derived value at exec time.
2. Per-task AST slot plumbing in `tx-reactor` so
   `checkpoint_userspace_entry` (not `_batch`) can wire signal
   handlers. Unblocks signal-handler frame setup / sigreturn.
3. `fork` / `clone` syscall drivers (the VM half exists in
   `vm::execution::fork_aspace`; the syscall driver does not).
4. Real initramfs / cpio unpack into tmpfs at boot.
5. `wait4` / `waitid` syscalls.
6. Retire `open_console_for_init_legacy` (dead code on the boot
   path after Phase 6's `register_mount` wire-up).
7. `RawTrapFrame` / `TrapFrameMut` portable HAL surfaces.
8. `KERNEL_FIXUP_TABLE`-style kernel-mode fault recovery.
9. Multi-hart secondary-CPU bringup.

## Verification

- `cargo test -p tx-kernel --lib` — 19/19 (5 trap_handoff + 3 boot
  smokes + 4 thread_future + 3 page-fault + 3 IRQ + 1 production
  smoke).
- `cargo test -p tx-fs --lib -- --test-threads=1` — 13/13 (5 devfs
  + 5 tmpfs + 2 errno + 1 walker fallback).
- `cargo test -p tx-shims --lib` — 12/12 (5 Phase 2a + 7 Phase 2b,
  with the read test rewritten to assert blocking behaviour).
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 344/344
  (331 baseline + 10 walker + 3 prepare-shim).
- `cargo test -p tx-substrate` — sync 2/2 + existing tests preserved.
- `cargo check --workspace` clean.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
  clean (verifies kernel_main → reactor loop compiles on the real
  board target).
- `cargo fmt --check` clean.
- `cargo xtask progress validate` ok.

## Commits on `feat/pre-elf-runtime`

| sha | wave | scope |
|---|---|---|
| `04edaa6` | 1 | Phase 1 minor cleanups + Phase 4 VFS walker |
| `e757bc0` | 2 | Phase 6 mount/dev id allocators + register_mount wire-up |
| `abc9fb8` | 3 | Phase 2 reactor task wrapper + entry shim + HAL hook |
| `b49b302` | 4 | Phase 3 page-fault async dispatch |
| `3cb95d6` | 4 | Phase 5 IRQ dispatch + UART RX + sys_read blocking |
| `69a3efd` | 5 | Phase 7 end-to-end smoke + kernel_main reactor loop |
