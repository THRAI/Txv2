# fork / clone / wait4

**Date:** 2026-05-06
**Branch:** `feat/fork-clone-wait4`
**Plan:** [`docs/progress/plans/2026-05-06-fork-clone-wait4.md`](../plans/2026-05-06-fork-clone-wait4.md)
**Research:** [`docs/progress/research/2026-05-06-fork-clone-wait4-scaffolding.md`](../research/2026-05-06-fork-clone-wait4-scaffolding.md)
**Status:** Complete (host-test scope; bare-`SIGCHLD` clone + blocking
`wait4` + 5+1 introspection arms + musl startup stubs; LTP `execve05`
unblocked). 4 phase commits + 1 chore on top of the ELF loader slice.
tx-kernel 31/31, tx-fs 16/16 serial, tx-shims 48/48, tx-scripts 29/29,
tx-subsystems 377/377 serial, tx-substrate sync 2/2. `cargo check
--workspace`, `cargo check -p tx-kernel-riscv64-qemu-virt --target
riscv64gc-unknown-none-elf`, `cargo fmt --check`, `cargo xtask progress
validate` all green.

## Goal

Land bare-`SIGCHLD` `clone()` + blocking `wait4()` so static-musl
binaries can fork-and-wait. The biggest LTP-coverage unlock per the
ELF loader decision note's follow-up list. Demonstrable target: the
hand-encoded fixture binary from the ELF loader slice extended into a
fork+wait+exit RV64 program (parent forks child, child writes
`"child\n"` and exits, parent waits, writes `"parent\n"`, exits;
total 317 bytes). Layer A smoke (production-paths-up-to-divergence)
proves the bootstrap-exec → fixture pipeline; Layer B (full
reactor-driven round trip) deferred.

## What landed

### Wave 1 — Kernel-side prerequisites (commit `e697631`)

**Part 1A — `seed_child_leader_context` helper:**

`crates/tx-subsystems/src/process/execution.rs:328` — public free function:

```rust
pub fn seed_child_leader_context(
    child_thread: &Cap<ThreadIdentity>,
    parent_user_ctx: &UserTrapContext,
);
```

Clones the parent's `UserTrapContext`, sets `regs[10] = 0` (RV64 `a0`;
Linux `fork()` returns 0 in the child) and `pc = parent_pc + 4` (RV64
`ecall` is 4 bytes; skip past it so the child resumes after, not
retries). Calls `child_thread.payload_cap()?.store_saved_user_context(Some(child_ctx))`;
panics with stable string sentinel `:clone:no-context` if `payload_cap()`
is `None` (kernel-invariant violation; matches `:bootstrap-exec:fail`
precedent from the ELF loader slice).

API drift from plan: signature uses `&Cap<ThreadIdentity>` (caller
resolves the leader thread) rather than the plan's
`&Cap<ProcessIdentity>` (function resolves internally). Matches the
natural `step_fork`-then-seed sequencing in the syscall arm.

**Part 1B — `exit_port` Channel on `ProcessPayload`:**

New fields in `process/structure.rs::ProcessPayload`:

```rust
pub(crate) exit_port: Channel,
pub(crate) exit_port_carrier_id: u64,
```

Initialised in `sign_process_payload` (registered with
`wait_carrier::register_wait_channel`); mirrors the
`TtyIdentity::wait_channel` + `wait_carrier_id` pattern from pre-ELF
Phase 5.

Public constant `EXIT_PORT_CHILD_ZOMBIFIED: u64 = 0x1` (interest
mask). Future stop/cont events get their own bits — explicit
documentation comment, no premature taxonomy.

Public `ProcessIdentity` accessors:

- `exit_port_carrier_id() -> Option<u64>`
- `exit_port_wait_token() -> Option<WaitToken>`
- `fire_exit_port(mask) -> usize`

Routed through helper methods (not the brief's `&Channel` borrow)
because `ProcessPayload` sits behind `SpinMutex<Option<PayloadCap<...>>>`
and a borrowed `&Channel` cannot outlive the lock guard. The
`ProcessPayload::exit_port() -> &Channel` borrow (where it composes
per `sig_actions()` precedent) is still exposed for in-payload uses.

Fire site at `post_sigchld_to_parent` (`process/execution.rs:521`):
after the existing `step_kill_process` SIGCHLD post, the parent's
`fire_exit_port(Mask::from_bits(EXIT_PORT_CHILD_ZOMBIFIED))` runs.

Carrier-lifetime cleanup is Cross-cutting Risk #1 in the plan and
explicitly deferred beyond the slice (registry leak per dead process;
same pre-existing leak shape as `TtyIdentity` in tree).

**Reactor-submission seam (function-pointer in `tx-subsystems`):**

`crates/tx-subsystems/src/reactor_submit.rs` (new):

```rust
pub type SubmitChildThreadFn =
    fn(child_process: Cap<ProcessIdentity>, child_thread: Cap<ThreadIdentity>);

pub static SUBMIT_CHILD_THREAD: AtomicSlot<SubmitChildThreadFn> = AtomicSlot::new();

pub fn submit_child_thread(child: Cap<ProcessIdentity>, thread: Cap<ThreadIdentity>);
pub fn install_submit_child_thread(hook: SubmitChildThreadFn);
pub fn submit_child_thread_fn() -> Option<SubmitChildThreadFn>;
```

**Choice rationale:** function-pointer over `PmapIf`-shaped trait. The
`tx-shims → tx-kernel` direction is a circular dep (tx-kernel depends
on tx-shims after the ELF loader Wave 4). Hosting the slot in
tx-subsystems (which both tx-shims and tx-kernel depend on) breaks the
cycle. tx-kernel's `CoreInit::install_reactor_submit_seam` (`init.rs:1114`)
installs `submit_child_thread_into_boot_reactor`, a per-`P`
monomorphised hook that captures the platform parameter and the
`BOOT_REACTOR` static. tx-shims's `sys_clone` reads the slot via
`reactor_submit::submit_child_thread`.

Wired into `run_userspace_reactor_loop` so the slot is populated
before any user syscall fires.

**POSIX `wait_status_word` migration (Q#3 DECIDED):**

`crates/tx-subsystems/src/process/structure.rs:97` —
`ExitStatus::wait_status_word` replaced in tree:

- `Exited(code) → (code & 0xff) << 8` (was raw `code`)
- `Signaled(sig) → sig.raw() as i32 & 0x7f` (was `128 + sig`)

Per Linux `<sys/wait.h>`: `WIFEXITED(s) = (s & 0x7f) == 0`;
`WEXITSTATUS(s) = (s >> 8) & 0xff`;
`WIFSIGNALED(s) = (((s & 0x7f) + 1) >> 1) > 0`;
`WTERMSIG(s) = s & 0x7f`.

Migrated 3 trio/pre-ELF/ELF-loader smokes that referenced the
shell-shape encoding: `process/tests:281`, `signal/tests:1162`,
`signal/tests:1306`. The `:userspace:exited:N` boot sentinel encoding
unchanged for `Exited(0)` (both encodings give 0); the production
`init_fixture` exits 0 so `:userspace:exited:0` is preserved. The
trio's `128 + sig` was the *shell* convention (bash-style program
exit code), not the kernel↔userspace `wait4` ABI; the migration
reflects that the kernel uses the POSIX encoding now that real
userspace is running.

13 new tx-subsystems tests in the touch zones.

### Wave 2 — tx-shims arms (commit `2af7011`)

**Part 2 — `NR_CLONE = 220`:**

`sys_clone(flags, stack, parent_tid_uaddr, tls, child_tid_uaddr, ctx)`
in `tx-shims/src/linux_syscall/mod.rs`. Strict bare-`SIGCHLD`
validation (any other flag bit, or zero flags → `-EINVAL`); non-zero
stack → `-EINVAL` (`posix_spawn` / `pthread_create` deferred);
`parent_tid_uaddr` / `tls` / `child_tid_uaddr` ignored.

Drives:

1. Read `parent_user_ctx` from `ctx.thread.payload_cap()?.saved_user_context()`;
   panic with `:clone:no-context` if `None` (Q#2 DECIDED).
2. Call `step_fork::<P>(&ctx.process)` — returns
   `Cap<ProcessIdentity>` only (NOT a tuple as the plan sketched);
   leader thread fetched via `child.nth_thread(0)`.
3. Call `seed_child_leader_context(&child_thread, &parent_user_ctx)`.
4. Call `reactor_submit::submit_child_thread(child.clone(), thread.clone())`.
   Panic with `:clone:no-reactor-seam` if the slot is not installed.

Returns child PID to parent. Errors: `ParentZombie → -ESRCH`,
`Vm(_) → -EAGAIN`, `Zone(_) → -ENOMEM`. Additional sentinels
`:clone:no-payload` and `:clone:no-leader` for related kernel-invariant
violations.

**Part 4 — 5+1 introspection arms:**

| nr | name | wraps | notes |
|---|---|---|---|
| 173 | `getppid` | `ProcessIdentity::parent_pid()` | init returns 0 per Linux |
| 154 | `setpgid` | `step_setpgid` | `Unimplemented → -EPERM`, `Zone → -ENOMEM` |
| 155 | `getpgid` | `pgrp_cap().pgid` | self-only pretty much |
| 81 | `getpgrp` | direct `-ENOSYS` | musl uses `getpgid(0)` |
| 156 | `getsid` | `pgrp_cap().session_cap().sid` | self-only |
| 157 | `setsid` | `step_setsid` | returns new sid (`Sid` not `()`) |

**Part 5 — musl-startup stubs:**

| nr | name | shape |
|---|---|---|
| 96 | `set_tid_address` | returns `ctx.thread.tid().raw() as i64`; `tidptr` ignored; `TODO(phase-tls)` |
| 99 | `set_robust_list` | returns `0`; `head`/`len` ignored; `TODO(phase-futex)` |

16 new tx-shims tests.

### Wave 3 — `NR_WAIT4 = 260` with blocking (commit `45bd7d4`)

`sys_wait4(pid, wstatus_uaddr, options, rusage_uaddr, ctx)`:

- Reject `rusage_uaddr != 0 → -EINVAL` (txKernel doesn't track rusage;
  `TODO(phase-rusage)`).
- Selector mapping (4 POSIX cases): `pid > 0 → WaitTarget::Pid`,
  `pid == 0 → WaitTarget::MyPgrp`, `pid == -1 → WaitTarget::Any`,
  `pid < -1 → WaitTarget::Pgid`.
- Re-poll loop: `step_waitpid_nohang(&ctx.process, target)` →
  - `Ok((pid, status))`: write the i32 wait status word to
    `wstatus_uaddr` if non-zero (kernel-side `core::ptr::write_volatile`
    with `TODO(phase-userva)` mirroring `sys_write`'s bootstrap-buffer
    pattern); return `Return(pid.raw() as i64)`.
  - `Err(NoChildren)`: return `Error(-ECHILD)`.
  - `Err(NoneReady)` and `WNOHANG`: return `Return(0)`.
  - `Err(NoneReady)` and not `WNOHANG`:
    `wait_carrier::wait_on_token(ctx.process.exit_port_wait_token()?).await`
    — Wave 1's exit-port carrier; same shape `sys_read` uses for the
    TTY input wait. After the await, drop the guard and re-enter the
    loop top (standard double-check pattern post-wake).
- `WUNTRACED` / `WCONTINUED` accepted but ignored (Linux silently
  drops unknown wait4 option bits).

API drift from plan: `step_waitpid_nohang` is **synchronous** (not
async) and takes no `&Guard`; it grabs its own snapshot of
`parent.children` internally. Returns `Result<(Pid, ExitStatus),
WaitError>` where `WaitError = {NoChildren, NoneReady}`; no
`StepOutcome` wrapper.

10 new tx-shims tests including the load-bearing
`dispatch_wait4_blocking_resolves_when_child_zombifies` — pin the
dispatch future, poll once → `Pending`; manually call
`step_exit_group(&child, Exited(0))` (routes through
`post_sigchld_to_parent → fire_exit_port`); spin-poll → `Ready` with
`Return(child_pid)`.

### Wave 4 — Fork+wait+exit fixture + Layer A smoke (commit `2f7b250`)

**Fixture extension** (`crates/tx-kernel/src/init/init_fixture.rs`):

Extended from 218 → **317 bytes**: 64-byte Ehdr + 56-byte PT_PHDR +
56-byte PT_LOAD + 128 bytes code (32 instructions) + 6 bytes
`"child\n"` + 7 bytes `"parent\n"`.

Layout:

- **Pre-branch (8 instructions):** `clone(SIGCHLD, 0)` + `bnez a0,
  parent_path`. The `bnez` sits at PC = entry+28; parent_path at
  entry+68; displacement = 40 bytes. B-type encoding splits the
  immediate as `imm[12|10:5|4:1|11]`; result is `0x02051463`
  (LE bytes `63 14 05 02`). Documented in the file.
- **Child path (9 instructions):** `write(1, "child\n", 6)` +
  `exit_group(0)`.
- **Parent path (15 instructions):** `wait4(-1, NULL, 0, NULL)` +
  `write(1, "parent\n", 7)` + `exit_group(0)`.

Pin tests updated:

- `fixture_first_instruction_is_li_a7_64` renamed to `_li_a7_220`
  (NR_CLONE not NR_WRITE).
- `fixture_msg_bytes_at_offset_212` replaced with
  `fixture_msg_bytes_present_for_child_and_parent` (walks bytes
  finding both messages).
- `fixture_size_matches_constant` → 317.
- New: `fixture_bnez_branch_offset_targets_parent_path` decodes the
  bits, sign-extends, verifies `bnez_pc + offset == parent_path_vaddr`.

**Layer A smoke** (`init/tests.rs`):

`boot_smoke_fork_wait_seeds_init_for_clone_at_entry` drives
`drive_boot_wiring + run_bootstrap_exec_for_init`. Asserts:

- `init.aspace_cap()` differs after exec from before.
- `saved_user_context.pc == INIT_FIXTURE_ENTRY_VADDR`.
- The fixture's first instruction at `INIT_FIXTURE_ENTRY_VADDR`
  decodes to `li a7, 220` (NR_CLONE), proving the bootstrap-exec
  wiring + the new fixture content match.

**Layer B** (full reactor-driven fork+wait round trip with
panic-as-yield instruction decoder) deferred. Past the brief's
~200-line / no-TestPlatform-extension threshold; would require a
simulator that decodes RV64 bytes from the post-exec aspace, switches
between parent/child `ThreadPayload::userspace_slot` between
iterations, and simulates `post_sigchld_to_parent` resolution of the
parent's blocking NR_WAIT4. With Wave 3's tx-shims arm tests pinning
dispatch in isolation and ELF-loader Wave 5's smoke pinning the
bootstrap-exec → reactor-driven write+exit pipeline on the simpler
hello-world fixture, Layer A's "exec front-end seeds NR_CLONE at PC"
assertion closes the loop without an integration smoke. Future
follow-up.

## Decisions

- **Q#1 (research): blocking wait4 in scope.** Adds an `exit_port`
  reactor wait-channel; unblocks LTP `wait401` + most of
  `waitpid03..13`. ~14 more LTP tests covered.
- **Q#2 (research): NR_WAITID deferred.** Requires `siginfo_t`
  plumbing the trio doesn't have today. Wait for a future `siginfo`
  slice.
- **Q#1 (plan): `exit_port` interest mask = `EXIT_PORT_CHILD_ZOMBIFIED
  = 0x1`.** No premature taxonomy; future bits land alongside their
  wakers.
- **Q#2 (plan): `saved_user_context == None` at clone-time panics
  with `:clone:no-context` sentinel.** Matches the precedent of ELF
  loader's `:bootstrap-exec:fail`. Kernel-invariant violation, not a
  userspace error.
- **Q#3 (plan): replace `ExitStatus::wait_status_word` in tree with
  POSIX `<sys/wait.h>` encoding; migrate the 3 trio/pre-ELF smokes
  that reference it.** The `128 + sig` shell-style encoding belongs
  in userspace shells, not in the kernel.
- **Q#4 (plan): extend `init_fixture.rs` in place** into a
  fork+wait+exit binary. The existing hello-world output isn't
  asserted by any surviving smoke; single source of truth keeps
  fixture maintenance light.
- **Q#5 (plan): per-process `exit_port` granularity.** Per-pgrp has
  no v1 benefit since all wait4 callers walk children via
  `step_waitpid_nohang`.

## LTP coverage matrix

| dir | tests | day-1 MVP coverage |
|---|---|---|
| fork | 10 | ~6 outright (fork01 child-pid, fork04 anon-private environ, fork07–10 fd-inheritance) |
| clone | 11 | 1 outright (clone01 bare-SIGCHLD); clone02..11 need flag-matrix support |
| wait | 2 | 2 outright (wait01 ECHILD, wait02 retrieves exit status) |
| wait4 | 3 | 3 outright (wait401 blocking — newly unlocked by Wave 3 — wait402 ECHILD, wait403 ESRCH) |
| waitpid | 11 | ~5 outright; the rest need WUNTRACED/WCONTINUED + setpgrp(0,0) flow |
| waitid | 11 | 0 (deferred — needs NR_WAITID + siginfo) |
| (execve) | execve05 | newly unlocked by NR_CLONE |

Bare-SIGCHLD + WNOHANG + blocking wait4 covers ~17 of 48 outright +
LTP `execve05`. Remaining wait4 / waitpid coverage gates on either
fork's expanded selector flags or NR_WAITID's `siginfo_t` plumbing.

## Out of scope (deliberately deferred)

- **Clone flags beyond bare SIGCHLD:** `CLONE_VFORK | CLONE_VM`
  (`posix_spawn`); the 10-flag pthread_create set
  (`CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD
  | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID
  | CLONE_SYSVSEM`).
- **NR_WAITID + siginfo plumbing.** Requires extending
  `step_waitpid_nohang`'s return shape with cred + signum so the
  driver can compose `siginfo_t`; no `SigInfo` carrier exists today.
- **Real `set_tid_address` / `set_robust_list` semantics.** Today's
  stubs return tid / 0; full futex-on-thread-exit + robust-list
  registration is a future TLS / futex slice.
- **Per-task AST plumbing for signal-handler frame setup.** Still
  uses the empty-batch `AstBatch::default()` shortcut.
- **Carrier-lifetime cleanup for `exit_port`.** Pre-existing
  carrier-leak shape on `TtyIdentity` extends to `ProcessPayload`;
  cleanup is a future maintenance slice.
- **`rusage` tracking.** `wait4` rejects non-NULL `rusage_uaddr` →
  `-EINVAL`. Real `rusage` accounting is a future slice.
- **Layer B end-to-end smoke** (full reactor-driven fork+wait round
  trip via instruction-decoder simulator).

## Follow-ups in priority order

1. **Real CSPRNG** — replace constant `[0; 16]` `AT_RANDOM` (still on
   the immediate to-do list from the ELF loader slice).
2. **DAC permission checks + setuid** — unblocks LTP `execve02` and
   the 0700 / setuid tests in the wider LTP suite.
3. **Real per-task AST plumbing** — signal-handler frame setup
   beyond `EnterUserspace`. Unlocks LTP wait* tests that gate on
   stop/cont signals.
4. **`NR_WAITID` + siginfo plumbing** — closes the LTP `waitid*`
   directory.
5. **`sys_open` syscall arm + `O_CLOEXEC` threading** —
   `OpenFileFlags::cloexec` is plumbed; just need the syscall arm.
6. **`CLONE_VFORK | CLONE_VM`** — unlocks `posix_spawn` (musl shells
   use it).
7. **Layer B end-to-end smoke** for the fork+wait round trip via
   instruction-decoder simulator.
8. **`RawTrapFrame`/`TrapFrameMut` portable HAL surface** — RV64
   board internals today.
9. **Real initramfs cpio unpack at boot** — replace hand-encoded
   fixture with real init binary loaded from disk.

## Verification

- `cargo test -p tx-kernel --lib` — 31/31.
- `cargo test -p tx-fs --lib -- --test-threads=1` — 16/16.
- `cargo test -p tx-shims --lib` — 48/48.
- `cargo test -p tx-scripts --lib` — 29/29.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 377/377.
- `cargo test -p tx-substrate` — sync 2/2.
- `cargo check --workspace` clean.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf` clean.
- `cargo fmt --check` clean.
- `cargo xtask progress validate` ok.

## Commits on `feat/fork-clone-wait4`

| sha | wave | scope |
|---|---|---|
| `a0c0c40` | chore | research + plan (Q1=blocking-wait4 in; Q2=NR_WAITID deferred; 4 minor Qs decided) |
| `e697631` | 1 | kernel-side prerequisites (1A seed_child + 1B exit_port + reactor seam + POSIX wait_status_word migration) |
| `2af7011` | 2 | tx-shims arms (NR_CLONE + introspection + musl stubs) |
| `45bd7d4` | 3 | NR_WAIT4 with blocking via wait_carrier on exit_port |
| `2f7b250` | 4 | extend init fixture into fork+wait+exit + Layer A smoke |
