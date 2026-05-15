---
name: tx-syscall-dispatch
description: Use when migrating txKernel syscall dispatch to the v3 three-lane model — classifying syscalls as ImmediateSyscall, OneShotStepOp, or Full async drive; restructuring the dispatch table; or verifying migration progress with lint ratchets. Triggered by tasks involving syscall dispatch restructuring, lane classification, drive_oneshot migration, or immediate syscall fast-path.
---

# tx-syscall-dispatch

Use this skill for every syscall dispatch migration task: classifying a
syscall into one of three lanes, restructuring the dispatch table, migrating
a syscall from free-function call to `drive_oneshot()` or `drive()`, or
checking migration progress with lint ratchets.

Companion to `tx-step-migration` (which covers step-function wrapping and the
`StepOp` trait). This skill covers the **dispatch side** — the syscall entry
point that routes into the step algebra.

## Read First

- `docs/Txv3/04_SYSCALL_SHAPE_v1.md` §6 — three-lane dispatch model,
  `ImmediateCtx`, `OneShotStepOp`, full async script
- `docs/Txv3/03_STEP_MODEL_v2.md` §5.3 — `drive_oneshot()` spec and
  `OneShotStepOp` contract
- `docs/Txv3/02_INVARIANTS_v5.md` — SCRIPT-V5-4/5, STEP-11/12
- `docs/Txv3/07_BLAST_RADIUS.md` §5.4 — lane counts and landing order
- Current code: `crates/tx-shims/src/linux_syscall/mod.rs` (dispatch table),
  `crates/tx-shims/src/linux_syscall/immediate.rs` (immediate lane),
  `crates/tx-shims/src/linux_syscall/ctx.rs` (`build_subject_script_ctx`)

## Three Lanes

```
dispatch()
├── Lane 1: ImmediateSyscall (17) — pure ABI queries, no StepOp, no yield
│     getpid, getuid, umask, times, uname, rt_sigreturn, ...
│     Takes &ImmediateCtx (narrower than SyscallCtx)
│
├── Lane 2: OneShotStepOp (43) — mutations that never yield
│     setuid, sigaction, setsid, close, chdir, mkdir, ...
│     drive_oneshot(&mut op, &mut script_ctx)
│
└── Lane 3: Full async drive (29) — may yield via VFS/VM/timer
      read, write, openat, futex_wait, poll, execve, ...
      drive(op, &mut script_ctx, mode).await
```

Full classification table in [references/dispatch-lanes.md](references/dispatch-lanes.md).

## Migration Patterns

### Pattern A: Migrate to Immediate Lane

For syscalls that are pure synchronous reads (no guard, no VFS, no timer, no
yield). Example:

```rust
// BEFORE — in proc.rs, called from dispatch as sys_getpid(ctx)
pub(super) fn sys_getpid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pid.0 as i64)
}

// AFTER — in immediate.rs, dispatch constructs ImmediateCtx::from(ctx)
pub fn getpid(ctx: &ImmediateCtx) -> SyscallResult {
    SyscallResult::Return(ctx.process.pid.0 as i64)
}
```

Dispatch update:
```rust
let ictx = ImmediateCtx::from(ctx);
match req.nr {
    NR_GETPID => return getpid(&ictx),
    _ => {} // fall through to script lanes
}
```

**Checks:** The function body must NOT contain `.await`, `drive(`, `drive_oneshot(`, `StepOutcome`, `YieldShape`, `guard()`. Verified by `cargo xtask lint invariants syscall-ctx-bridge`.

### Pattern B: Migrate to OneShotStepOp

For syscalls that are semantic transitions (mutations) but never yield.
Requires an existing `StepOp<I, Progress = NoProgress>` impl in the subsystem.

Step 1 — Add marker in subsystem (e.g., `tx-subsystems/src/cred/mod.rs`):
```rust
impl<I: SubjectIdentity> OneShotStepOp<I> for SetuidOp {}
```

Step 2 — Update syscall call site (e.g., `tx-shims/src/linux_syscall/cred.rs`):
```rust
// BEFORE
pub(super) fn sys_setuid(args, ctx) -> SyscallResult {
    let target = Uid(args[0] as u32);
    cred_change_to_result(step_setuid(&ctx.process, target))
}

// AFTER
pub(super) fn sys_setuid(args, ctx) -> SyscallResult {
    let target = Uid(args[0] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetuidOp { target: ctx.process.clone(), new_uid: target };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(v3errno)),
    }
}
```

**Preconditions for drive_oneshot:**
- The op struct must implement `StepOp<I, Progress = NoProgress>` with `OneShotStepOp<I>`
- The syscall must NOT call `.await` or register on WaitSource/DelegateEndpoint
- Result translation: `Ok(T)` → syscall return value, `Err(Errno)` → negative errno

### Pattern C: Migrate to Full Async Drive

For syscalls that may yield. Requires a `StepOp` impl (any `Progress` type).

```rust
// Pattern (see sys_write in io.rs for working example)
pub(super) async fn sys_xxx(args, ctx) -> SyscallResult {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let guard = step_engine::guard();
    let mode = if flags.nonblocking { DriveMode::Nonblocking } else { DriveMode::Waiting };
    let op = XxxOp { ... };
    match drive(op, &mut script_ctx, mode, None, None, None).await {
        Ok(result) => SyscallResult::Return(result as i64),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(v3errno)),
    }
}
```

## Adapter Setup

When adding `OneShotStepOp` / `drive_oneshot` to a new subsystem, update its
adapter file:

```rust
// In tx-subsystems/src/<subsys>/adapter.rs, add to step_engine re-exports:
pub use tx_substrate::step::{ ..., OneShotStepOp, drive_oneshot };

// In tx-shims/src/adapter.rs, add to step_engine re-exports:
pub use tx_substrate::step::{ ..., OneShotStepOp, ..., drive_oneshot };
```

And in `tx-shims/src/linux_syscall/mod.rs`, add the op struct to imports:
```rust
use tx_subsystems::cred::{ ..., SetuidOp };
```

## Verification

### Lint ratchets (run after every migration batch)

```bash
cargo xtask lint invariants syscall-adhoc-loop   # V3::/V3Out:: manual match sites
cargo xtask lint invariants syscall-no-await     # .await in sys_* fn bodies
cargo xtask lint invariants syscall-ctx-bridge   # build_subject_script_ctx adoption
cargo xtask lint invariants all                  # full invariants suite
```

### Compile check

```bash
cargo check -p tx-shims -p tx-subsystems
```

### Integration test

```bash
cargo xtask test busybox-boot   # full kernel build + QEMU smoke
cargo xtask unit                # unit test suite
```

## Current Baseline (2026-05-15)

| Lane | Count | Ceiling | Status |
|------|-------|---------|--------|
| Immediate | 17/17 migrated | — | done |
| One-shot (StepOp exists) | 1/43 migrated (setuid) | — | in progress |
| Full async | 2/29 migrated (read, write) | — | in progress |
| Ad-hoc V3:: loops | 8 files / 86 sites | 8 | ok |
| .await in syscall fns | 15 sites | 60 | ok |
| ScriptCtx bridging | 5/88 (6%) | — | info |
