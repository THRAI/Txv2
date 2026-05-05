# Trio: Trap Shell + Syscall Table + tmpfs/devfs

**Date:** 2026-05-06
**Branch:** `feat/trio-trap-syscall-tmpfs-devfs`
**Plan:** [`docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md`](../plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md)
**Status:** Complete (host-test scope). 6 phase commits on top of the
chore baseline. tx-kernel 8/8, tx-fs 10/10 serial, tx-shims 12/12,
tx-subsystems 331/331 serial, `cargo check --workspace`, `cargo fmt
--check`, and `cargo xtask progress validate` all green.

## Goal

Land the smallest coherent slice that unblocks every downstream step
on the long-term `kernel_main` checklist's first-userspace path:
trap shell handoff, a fixed-table syscall dispatcher, an in-tree
filesystem backend (tmpfs), an in-tree devfs that resolves
`/dev/console`, and a boot-wiring sequence that mounts both and
preopens init's fds 0/1/2 to the console. End-to-end demonstration
is a host-test smoke that synthesises a fake userspace, runs
`write(1, "hi\n", 3)` then `exit_group(0)` through the full chain,
and observes `b"hi\r\n"` (post-OPOST) on the captured console with
init transitioning to zombie `Exited(0)`.

ELF loading, fork/exec/clone, real init binary load, IRQ_HANDLERS
dispatch, and the production reactor task wrapper / userspace-entry
shim remain explicitly out of scope.

## What landed

### Phase 1 — trap shell skeleton (commit `f2d84b7`)

`crates/tx-kernel/src/trap_handoff.rs` (new) replaces
`KernelTrapDispatcher::on_syscall` and `on_page_fault`'s
`TrapAction::Terminate` stubs with handoff to a
`UserspaceRunSlot::complete_interesting_trap`. New helpers:

- `SyscallRequest`, `PageFaultInfo`, `translate_syscall<P>`,
  `translate_user_pf<P>`.
- `current_payload_for_hart`, `hand_off_syscall`, `hand_off_user_pf`,
  `outcome_to_trap_action`.

`ThreadPayload` (in `crates/tx-subsystems/src/thread_runtime/structure.rs`)
extended with the four plan-mandated trap-frame slots:
`userspace_slot`, `active_request`, `saved_user_context`,
`pending_syscall_return`. Plus a 64-slot per-hart
`current_thread_payload` registry (`set/clear/get` accessors and
`drain_pending_syscall_return` helper) — the seam the future reactor
task wrapper will hit around `Future::poll`, and the userspace-entry
shim's drain hook for Plan B.

Trap shell never writes the trap frame: per **`txdoc:THREAD-5-1-STATE-PLACEMENT`**
the saved user context lives on the payload, and writeback to a
*fresh* trap frame's `a0` is the userspace-entry shim's job.

### Phase 3a — devfs FsOps (commit `91f2175`)

`crates/tx-fs/src/devfs.rs` (new) implements `FsOps` over the live
TTY alias snapshot: `lookup`/`load_inode_meta`/`readdir` resolve
through the existing alias registry; mutating ops return
`Errno::EROFS`; `FsPageBacking` returns `ENOSYS` (char-device I/O
does not page-cache). `open_console_for_init() -> Cap<OpenFile>`
gives Phase 3b a one-shot bootstrap helper for fd preopen before
the VFS walker exists.

Two additive seams in `tx-subsystems::tty`:

- `tty::project::resolve_devfs_alias(name) -> Option<Cap<TtyIdentity>>`
- `tty::structure::registry::devfs_alias_snapshot() -> Vec<TtyAliasEntry>`

The write-test asserts `b"hi\r\n"` reaches the binding — the
post-OPOST `\n→\r\n` expansion is the strongest correctness signal
that the path traverses the line discipline.

### Phase 2a — write/exit/exit_group/getpid (commit `abefad4`)

`crates/tx-shims/src/linux_syscall/{numbers.rs,mod.rs,tests.rs}` (new):

- `NR_WRITE=64`, `NR_EXIT=93`, `NR_EXIT_GROUP=94`, `NR_GETPID=172`.
- `pub async fn dispatch(req, ctx) -> SyscallResult` with
  `Return(i64) | Error(i32) | NoReturn`.
- `SyscallCtx` carries `Cap<ProcessIdentity>`, `Cap<ThreadIdentity>`,
  `Cap<AddressSpace>`.
- `write` bounds the user buffer to `TTY_WRITE_MAX_INLINE = 4096` and
  reads the kernel-side slice via `from_raw_parts` with
  `TODO(phase-userva)` for the real `copy_from_user`.
- `exit` chaining: per `PROCESS_v1.md` §7.3.1 step 3 and
  `thread_runtime/execution.rs:44-54`, `step_thread_exit` already
  chains to `step_process_exit` on the last-thread case; the
  dispatcher therefore calls **only** `step_thread_exit`. Calling
  `step_exit_group` additionally would double-zombify and corrupt
  the recorded exit status. (Plan open question #6 resolved.)

`ProcessPayload.fds: SpinMutex<[Option<Cap<OpenFile>>; 8]>` added with
`step_fork` cloning slot-by-slot. `ProcessIdentity::fd / set_fd /
nth_thread` public accessors so `tx-shims` does not poke pub(crate)
fields.

`tx-subsystems` `test-support` feature widens `reset_*_for_test`
helpers to `cfg(any(test, feature = "test-support"))` so test setup
in sibling crates can reset the static init-process slot.

### Phase 2b — read/brk/rt_sigprocmask/rt_sigaction (commit `119602b`)

Four more dispatch arms:

- `read`: same fd-resolve + kernel-slice + `step_read` await loop
  pattern. Initial `Blocked(token)` translates to `Return(0)` per
  plan open question #5; `TODO(phase-blocking-read)` to plumb real
  `wait_on_token` once an input source is wired.
- `brk` (per `txdoc:VM-5-8-BRK`): `requested == 0` short-circuits to
  "report current". Any `brk_script` error including `InvalidRange`
  returns the *unchanged* `current_brk` — Linux: brk never returns
  negative errno.
- `rt_sigprocmask`: rejects `sigsetsize != 8`. `step_sigprocmask`
  already returns `SigprocmaskChange::Replaced { prev, new }`, so
  no separate oldset accessor is needed.
- `rt_sigaction`: rejects `sigsetsize != 8`. **Resolved an ABI
  ambiguity in the plan**: the modern `rt_sigaction` struct on RV64
  is 32 bytes (`sa_handler` u64 + `sa_flags` u64 + `sa_restorer` u64
  + `sa_mask` u64), not 16 — RV64 has no legacy `sigaction` syscall
  per `linux/include/uapi/asm-generic/signal.h`. `SIGACTION_BYTES =
  32`. SIG_DFL=0, SIG_IGN=1, anything else `Handler(addr)`.

`ProcessPayload` gets `brk_base: AtomicU64` and `current_brk:
AtomicU64`, both seeded at `BOOTSTRAP_BRK_BASE = 0x6000_0000` in
`bootstrap_init_process` with `TODO(phase-elf-loader)`. `step_fork`
clones the pair. `ProcessIdentity::brk_base / current_brk /
set_current_brk / sig_disposition` accessors.

### Phase 3b — tmpfs + 5-step boot wiring (commit `ffc098c`)

`crates/tx-fs/src/tmpfs.rs` (new): `FsOps` + `FsPageBacking` over
in-memory `BTreeMap<TmpfsName, FsObjectId>` directory + inode store.
Regular files use `PageContainer::new_cap(PageContainerKind::Anon
{ swap_policy: Reclaimable }, 1024)`. `FsObjectId::new(2)+` (root
== 1 reserved). Cross-directory rename + hardlink stubbed `ENOSYS`.

`init.rs` extended with the 5 ordered boot steps after
`init_process_subsystem`:

1. `register_console_hardware` — `tty::execution::register_hardware`
   with a static `ConsoleCharOps<P>` whose write calls
   `tx_hal::ConsoleIf::write_bytes`.
2. `mount_rootfs_tmpfs` — `Tmpfs::new_root()` →
   `MountIdentity::new_cap(MountId(1), None, root_rnode, None,
   payload, MountFlags::default())`. Stored in `ROOT_MOUNT`.
3. `mount_devfs_at_dev` — `tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, "dev",
   ...)` (no `step_mkdir` in the VFS step layer yet) →
   `MountIdentity::new_cap(MountId(2), Some(/dev dentry on rootfs),
   devfs_root_rnode, Some(ROOT_MOUNT), ...)`. Stored in `DEV_MOUNT`.
4. `register_devfs_console_alias` —
   `tty::execution::register_console_alias("console", CONSOLE_TTY)`.
5. `bind_init_cwd_and_root` — `step_chdir(init, root_dentry)` plus
   `init.set_fd(0/1/2, Some(open_console_for_init()))`.

Two boot-smoke tests assert: `ROOT_MOUNT` and `DEV_MOUNT` populated;
`dev.parent() == root` and `dev.mountpoint().name() == b"dev"`;
`resolve_devfs_alias(b"console") == CONSOLE_TTY`; `init.fd(0/1/2)`
non-None; `step_write` through fd 1 captures bytes via
`TestPlatform::write_bytes`; `step_getcwd(init) == b"/"`.

Signature gaps reconciled vs the plan: `Errno` lacks
`EEXIST`/`ENOTEMPTY` (mapped to `EINVAL`/`EBUSY` with TODOs);
`InlineName` lacks `Ord` (dir keyed by `Vec<u8>`); `tx_substrate`
lacks a public `SpinMutex` (in-crate TAS shim, documented to
disappear when substrate exposes one); `step_chdir` does NOT take
a `&Guard`.

### Phase 6 — end-to-end smoke (commit `6828b00`)

A single new host test
`init::tests::boot_smoke_userspace_round_trip_writes_console_then_exits`
stitches the full chain: fake userspace queue
(`Syscall(write(1,"hi\n",3))` → `Syscall(exit_group(0))`) →
`UserspaceRunSlot::start_request` + `complete_interesting_trap` →
`linux_syscall::dispatch` → `OpenFile::step_write` → TTY ldisc OPOST
→ `ConsoleIf::write_bytes` capture → Plan B writeback into
`pending_syscall_return` → fake userspace-entry shim drains the slot
into a test-local log. Loop terminates on `SyscallResult::NoReturn`.

Asserts: console captures `b"hi\r\n"`; `init.is_zombie()` with
`ExitStatus::Exited(0)`; drained log = `[Some(Ok(3)), None]`.

Per-hart slot is staged manually via
`set_current_thread_payload(0, _)` / `clear_current_thread_payload(0)`
because the production reactor task wrapper that would normally do
this still does not exist (Cross-cutting risk #1 in the plan).
The fake driver replaces both that wrapper and the userspace-entry
shim as a host-test facsimile.

One additive seam: `ThreadIdentity::payload_cap_for_test()` gated
on `cfg(any(test, feature = "test-support"))`, mirroring the
existing `cross_crate_test_support` pattern.

## Decisions

- **Plan B for syscall-return writeback** is locked in: the trap
  shell never writes the trap frame; `pending_syscall_return` is
  drained by a (still-future) userspace-entry shim and `a0` is
  written against a *fresh* trap frame on user re-entry, not the
  one the syscall trapped on. Rationale: `TrapFrameMut<'_>` is not
  `Send` and cannot be held across an `.await` in the thread
  future; deferring writeback also keeps the AST signal-delivery
  hook's preemption-transparent semantic intact for any future
  signal pending between syscall return and user re-entry.
- **`exit` chains via `step_thread_exit`'s last-thread branch**, not
  a separate dispatcher-level `step_exit_group` call. (Plan open
  question #6.)
- **`rt_sigaction` is the modern 32-byte struct**; RV64 has no
  legacy `sigaction` syscall.
- **`brk_base` bootstrap value is `0x6000_0000`** with
  `TODO(phase-elf-loader)` for replacement when ELF loading lands.

## Out of scope (explicitly deferred)

- ELF loader (`execve` syscall, image parsing, entry trampoline).
- `fork`/`clone`/`execve`/`wait4` syscall drivers.
- Real init binary loading from initramfs.
- `procfs`/`sysfs`/`bdevfs`/`devpts`/`tx_ext4` backends.
- General `copy_from_user`/`copy_to_user`.
- Userspace signal-handler frame setup (sigreturn, alt stack).
- Real `tty::execution::step_read` blocking semantics; trio returns
  `Done(0)` on empty input.
- Production reactor task wrapper that drives the thread future and
  stages the per-hart `current_thread_payload` slot around
  `Future::poll`.
- Production userspace-entry shim that drains `pending_syscall_return`
  and writes `a0` into a fresh `TrapFrameMut`.
- Full `step_open` through the VFS walker; `/dev/console` is reached
  via the `open_console_for_init` bootstrap helper.
- `MountId` / `DevId` allocators — hardcoded `MountId(1)`/`MountId(2)`
  and `DevId(1)`/`DevId(2)` for the slice.
- Cross-directory rename + hardlink in tmpfs.

## Follow-ups in priority order

1. Production reactor task wrapper that drives a thread future and
   stages the per-hart `current_thread_payload` slot around
   `Future::poll`. Lets the Phase 6 fake driver be deleted.
2. Production userspace-entry shim that drains
   `pending_syscall_return` and writes `a0` into a fresh
   `TrapFrameMut`. Plan B's missing piece.
3. Page-fault async dispatch in the thread future
   (`TODO(phase-2)` in `trap_handoff::hand_off_user_pf`); call
   `aspace.fault_script(VmFault { ... }).await` and route
   `Err(_) -> step_exit_group_with_signal(SIGSEGV)`.
4. `tx-substrate` public `SpinMutex` so tmpfs and the init globals
   can drop their TAS shims.
5. `Errno` extension for `EEXIST`/`ENOTEMPTY`; tmpfs maps both onto
   `EINVAL`/`EBUSY` today.
6. `InlineName: Ord` so tmpfs's directory layer doesn't have to key
   on `Vec<u8>`.
7. ELF loader + `execve` syscall (lifts the bootstrap `brk_base`
   and unblocks real init binary load).
8. Real `step_open` through the VFS walker so `/dev/console`
   resolves by path.
9. IRQ_HANDLERS dispatch table walk at boot.
10. `MountId` / `DevId` allocators.

## Verification

- `cargo test -p tx-kernel --lib` — 8/8 (5 trap_handoff + 2 boot smoke
  + 1 userspace round-trip).
- `cargo test -p tx-fs --lib -- --test-threads=1` — 10/10 (5 devfs +
  5 tmpfs; serial because of the shared `FS_TEST_LOCK`, which exists
  because `tx_substrate::epoch::local::enter` panics on guard
  nesting).
- `cargo test -p tx-shims --lib` — 12/12 (5 Phase 2a + 7 Phase 2b).
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 331/331
  (baseline preserved). Pre-existing parallel-test global-state
  flakiness is unchanged from baseline; tracked separately.
- `cargo check --workspace` clean.
- `cargo fmt --check` clean.
- `cargo xtask progress validate` ok.

## Commits on `feat/trio-trap-syscall-tmpfs-devfs`

| sha | phase | scope |
|---|---|---|
| `f2d84b7` | 1 | trap shell skeleton + ThreadPayload trap-frame fields |
| `91f2175` | 3a | devfs FsOps + `/dev/console` alias resolution |
| `abefad4` | 2a | linux_syscall dispatch (write/exit/exit_group/getpid) + fd table |
| `119602b` | 2b | linux_syscall dispatch (read/brk/rt_sigprocmask/rt_sigaction) |
| `ffc098c` | 3b | tmpfs FsOps + 5-step boot wiring |
| `6828b00` | 6 | end-to-end userspace round-trip smoke |
