---
name: tx-step-migration
description: Use when wrapping free-function step_* fns into StepOp trait impls, migrating ad-hoc step loops to the central drive() loop, or advancing the v3 step-model vocabulary retirement per docs/Txv3/07_BLAST_RADIUS.md.
---

# tx-step-migration

Use this skill for every step-model migration task: wrapping a subsystem's
`step_*` free functions into `impl StepOp<I>`, swapping ad-hoc outcome loops for
`tx_scripts::drive::drive()`, or retiring any v4 vocabulary identifier
(`Advanced`, `Blocked`, `WakeCarrier`, `OnCarrier`, `InterestConditions`).

## Read First

Progressive disclosure — load only what you need for your task tier.

**Tier 0 — orientation (every task):**
- `docs/Txv3/03_STEP_MODEL_v2.md` — the step primitive, four-variant algebra,
  YieldShape catalog, StepOp trait, five-stage discipline, anti-patterns
- `docs/Txv3/07_BLAST_RADIUS.md` — measured surface (423 `StepOutcome` sites,
  178 `step_*` fns, 43 `Waker` sites), scaffold status, risk register
- `docs/progress/plans/2026-05-09-v3-tdd-migration.md` — PR landing order,
  two-wave fan-out pattern, per-worker brief template

**Tier 1 — wrapping free functions into StepOp:**
- `docs/Txv3/01_CONCEPTS_v5.md` §1 (seven-layer architecture), §11 (five phase
  classes)
- `docs/Txv3/02_INVARIANTS_v5.md` — STEP-1..10, WIT-3..6, YIELD-4/5/8
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md` §3 (five-phase
  commit discipline), §9 (cross-subsystem scripts)
- Current code: `crates/tx-substrate/src/step/mod.rs` (StepOp trait shape),
  any existing `impl StepOp<I> for …Op<'a>` in `crates/tx-subsystems/src/tty/execution/`
  or `crates/tx-subsystems/src/vfs/execution.rs` as worked examples

**Tier 2 — migrating drive loops:**
- `docs/Txv3/04_SYSCALL_SHAPE_v1.md` — upper/lower split, SubjectContext
- `crates/tx-scripts/src/drive.rs` — the central `drive()` function and its
  `DriveMode::classify` matrix
- `crates/tx-shims/src/lib.rs` — `KernelScriptCtx` alias, polymorphic vs
  concrete `impl StepOp<I>` pattern

**Tier 3 — delegate / OnAgent / ExecutionScope:**
- `docs/Txv3/05_DELEGATE_v1.md`
- `docs/Txv3/06_EXECUTION_SCOPE_v1.md`
- `crates/tx-substrate/src/step/agent.rs`
- `crates/tx-substrate/src/step/on_behalf_of.rs`

## Preserve

- **No compat shim.** Type aliases and constructor shims are explicitly
  rejected per `07_BLAST_RADIUS.md` §8. The transition is direct.
- **Four-variant outcome only.** `StepOutcome` uses `Continue`/`Yield`/
  `Done`/`Err`. `Advanced`/`Blocked`/`AdvancedThenBlocked` must not appear in
  new code. Mechanical mapping: `Advanced(T)` at syscall-arm layer → `Done(T)`;
  `Advanced(P)` mid-step → `Continue { progress }`; `Blocked` → `Yield { progress:
  EMPTY, shape }`.
- **StepProgress monoid.** Every `StepOp` picks exactly one `Progress` type.
  Progress accumulates across `Continue`/`Yield` returns via `extend`; `EMPTY`
  is the zero. Do not invent ad-hoc progress tracking.
- **Witnesses never cross step/yield boundaries** (WIT-3, WIT-5, WIT-6). Do not
  store `IdentRef<'g, T>` in `&mut self` (A-2). Do not carry reservation guards
  across yield (A-13, YIELD-8).
- **Five-stage discipline** (STEP-4): observe → upgrade → reserve → commit →
  publish. Skipping or reordering is a violation. Commit phases are infallible;
  errors surface before commit (A-9).
- **Upper/lower split.** Syscall scripts compose typed `StepOp`s identically
  over `&SubjectContext` (upper) and object payload (lower). Do not merge
  credential checks into payload mutation bodies.
- **StepOp does not `.await`** (STEP-2, A-3). Yields go through
  `StepOutcome::Yield { shape }`, not Rust async.
- **Polymorphic where possible.** Ops that don't access identity-specific
  fields use `impl<I: SubjectIdentity> StepOp<I>`. Only ops that call concrete
  identity methods use `impl StepOp<ProcessIdentity>`.

## Implementation Harness

### Wrapping a free function into StepOp

Pattern (clone an existing example from `tty/execution/` or `vfs/execution.rs`):

1. Define an op struct holding the arguments the old free function took.
2. Set `type Output` to the old return value, `type Progress` to the matching
   `StepProgress` impl (`NoProgress` for one-shots, `ByteProgress` for
   read/write, `PageProgress` for page ops, etc.).
3. Delegate the old function body into `fn step(&mut self, ctx: &mut ScriptCtx<I>)`.
4. Map old five-variant outcomes mechanically:
   - `Advanced(P)` mid-operation → `Continue { progress }` or `Done(t)` at
     syscall-arm level
   - `Blocked(carrier, interests)` → `Yield { progress: EMPTY, shape: OnWaitSource { … } }`
   - `AdvancedThenBlocked(P, carrier, interests)` → `Yield { progress, shape: OnWaitSource { … } }`
   - `Done(T)` → `Done(T)`
   - `Err(E)` → `Err(E)`
5. If the op can yield `OnAgent` or `OnTimer`, override `apply_resume()`.
6. Add `#[inline]` on hot-path impls (page_backed, tmpfs, pipe read/write).

### Migrating an ad-hoc loop to drive()

1. Identify the existing loop (typically in `tx-shims/src/linux_syscall/`).
2. Replace with:
   ```rust
   let op = YourOp { args };
   let mut ctx = KernelScriptCtx::new(); // or populated from SyscallCtx
   tx_scripts::drive::drive(op, &mut ctx, DriveMode::Nonblocking).await
   ```
3. If the loop used `Waiting` mode (reactor parking), keep `DriveMode::Waiting`
   — but note `Resolve` currently returns `EAGAIN` until reactor wiring lands.

### Fan-out discipline (per migration plan)

- **Wave 1**: vfs, tty, page-backed, fs, vm, mount-pipe-futex (parallelizable
  with W-fs/W-page-backed coordination)
- **Wave 2**: process-signal, initramfs, ext4, shims (fs/io/mod), scripts,
  kernel (depends on wave 1 landing)

Each worker gets a bounded write-scope (one subsystem directory plus its shim
syscall arms). Do not cross worker boundaries without coordination.

## Checks

- `cargo test -p tx-substrate` — algebra pin tests (monoid laws, classify
  matrix, exhaustive-match smoke)
- `cargo test -p tx-subsystems` — per-subsystem step tests
- `cargo test -p tx-scripts` — drive() integration tests
- `cargo xtask lint invariants step` — all four step lints (discipline, v4 vocabulary, no-await, sync-signature)
- `cargo xtask lint invariants step-discipline` — five-stage comment discipline (STEP-4)
- `cargo xtask lint invariants step-v4-vocabulary` — v4 identifier residue scan
- `cargo xtask lint invariants step-no-await` — `.await` in step fn bodies (A-3)
- `cargo xtask lint invariants step-sync-signature` — `async fn step_*` sigs (STEP-2)
- `cargo xtask lint invariants no-adhoc-drive` — ad-hoc outcome dispatch outside `drive.rs`
- `cargo xtask lint docs` — txdoc tag resolution
- `cargo xtask progress validate` — plan/handoff/worktree record health
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
  when kernel-paths or shims are touched
- `git diff --check`
- Verify no v4 vocabulary identifiers (`Advanced`, `Blocked`,
  `AdvancedThenBlocked`, `WakeCarrier`, `OnCarrier`, `InterestConditions`)
  remain in the touched files

After each PR: re-run `cargo test --workspace --lib --tests -- --test-threads=1`
and compare per-crate counts against the pre-migration baseline.
