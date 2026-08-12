# Step Interface Linter Audit

Date: 2026-07-06

## What Changed

Added `cargo xtask lint invariants step-interface` as a production Rust survey
for the StepOp migration surface. The rule is intentionally report-only: it is
not wired into `invariants all` and does not carry a ratchet ceiling yet.

The survey scans current production Rust under:

- `crates/tx-subsystems/src`
- `crates/tx-shims/src`
- `crates/tx-scripts/src`
- `crates/tx-kernel/src`
- `crates/tx-fs/src`
- `crates/tx-ext4/src`
- `crates/tx-ext4-format/src`

It skips test files and `#[cfg(test)]` / `mod tests` contexts. It counts
old-style `fn step_*` surfaces, including trait method step surfaces, and counts
`impl StepOp` wrappers while excluding `OneShotStepOp` marker impls.

## Current Live Counts

`cargo xtask lint invariants step-interface` on the current checkout reports:

- legacy `fn step_*` surfaces: 225
- `impl StepOp` wrappers: 116
- files with legacy step surfaces: 61
- legacy files with no StepOp impls: 40

Largest old-surface buckets:

- `tx-subsystems/net`: 55 old step surfaces, 0 StepOp wrappers
- `tx-subsystems/tty`: 25 old step surfaces, 23 StepOp wrappers
- `tx-subsystems/ipc`: 24 old step surfaces, 0 StepOp wrappers
- `tx-fs`: 18 old step surfaces, 0 StepOp wrappers
- `tx-subsystems/vfs`: 17 old step surfaces, 33 StepOp wrappers
- `tx-subsystems/process`: 16 old step surfaces, 17 StepOp wrappers
- `tx-subsystems/page_backed`: 13 old step surfaces, 8 StepOp wrappers
- `tx-subsystems/futex`: 13 old step surfaces, 2 StepOp wrappers
- `tx-subsystems/pipe`: 11 old step surfaces, 3 StepOp wrappers

The first reported legacy-only files include the fs backends, `epoll`,
`ipc/*/execution.rs`, many `net/execution/step_*.rs` files, `pipe/gift.rs`,
`process/exec_prep.rs`, `userfaultfd/mod.rs`, and `vfs/walker.rs`.

## Related Existing Step Lints

The old v4 outcome vocabulary is already retired in production code:

- `Advanced`: 0
- `AdvancedThenBlocked`: 0
- `WakeCarrier`: 0
- `OnCarrier`: 0
- `InterestConditions`: 0
- `StepOutcome::Blocked(`: 0

`cargo xtask lint invariants step` still fails only on the existing
`step-discipline` ratchet:

- total step functions found by that rule: 184
- steps missing at least one STEP-4 stage comment: 25
- ceiling: 5

The same command reports:

- `.await` in step function bodies: 0
- async `step_*` signatures: 0

`cargo xtask lint invariants no-adhoc-drive` still passes under its current
ceiling:

- files with ad-hoc outcome dispatch: 3
- total ad-hoc outcome sites: 15
- files: `clone_op.rs`, `exec_op.rs`, `vfs/composite.rs`

## Verification

- `cargo fmt -p xtask`
- `cargo fmt --check -p xtask`
- `cargo test -p xtask lint_invariants_step_interface -- --nocapture`
- `cargo xtask lint invariants step-interface`
- `cargo xtask lint invariants step-v4-vocabulary`
- `cargo xtask lint invariants no-adhoc-drive`
- `cargo xtask lint invariants step` was run and failed as expected on the
  pre-existing `step-discipline` ratchet: 25 missing stage-comment entries over
  ceiling 5.

## Next Step

Use `step-interface` as the mechanical owner map for the next StepOp wrap pass.
The first cleanup wave should prioritize legacy-only buckets with no wrappers:
`net`, `ipc`, `tx-fs`, `epoll`, `userfaultfd`, and the standalone walker/gift
files. Do not convert the report into a failing ratchet until the count is low
enough to be useful as a ceiling.

## 2026-07-06 Follow-up: Exec Post-Commit Helpers

The exec post-commit helpers are no longer exposed as old `fn step_*` free
functions. `process/exec_prep.rs` now keeps private non-step helpers, while the
public execution surface is the existing one-shot wrappers:

- `CloseCloexecFdsOp`
- `ResetSignalDispositionsForExecOp`
- `InstallBrkForExecOp`

`tx-scripts::process::exec::script::exec_script` now drives those wrappers
through `drive_oneshot` during Phase 7 instead of directly calling
`step_close_cloexec_fds`, `step_reset_signal_dispositions_for_exec`, and
`step_install_brk_for_exec`.

The two `tx-kernel::init` boot-reactor helpers were also renamed from
`step_boot_reactor_once*` to `boot_reactor_once*`; those were linter false
positives, not StepOp/StepOutcome interfaces.

Current `step-interface` count after this follow-up:

- legacy `fn step_*` surfaces: 219
- `impl StepOp` wrappers: 116
- files with legacy step surfaces: 59
- legacy files with no StepOp impls: 38

Exec caveat: the disabled `crates/tx-shims/src/linux_syscall/exec_op.rs` file
still exists as an unfinished StepOp-shaped refactor, and `sys_execve` still
drives the canonical async `exec_script`. This follow-up fixes the reachable
old post-commit helper surface; fully replacing `exec_script` with `ExecOp`
remains a larger migration.

Verification:

- `cargo test -p tx-scripts exec_script -- --nocapture`
- `cargo test -p tx-subsystems close_cloexec_fds_for_exec -- --nocapture`
- `cargo test -p tx-subsystems install_brk_for_exec -- --nocapture`
- `cargo check -p tx-kernel -q`
- `cargo check -p tx-shims -q`
- `cargo check -p tx-scripts -q`
- `cargo fmt --check -p tx-scripts -p tx-subsystems -p tx-shims -p tx-kernel`
- `cargo xtask lint invariants step-interface`
- `cargo xtask lint invariants step-v4-vocabulary`
- `cargo xtask lint invariants no-adhoc-drive`

## 2026-07-06 Follow-up: TXFS FsOps Method Names

The TXFS-facing legacy step names were trait-method names, not standalone
free-function step bodies. The bounded migration therefore renamed the VFS
backend vtable methods while preserving the same `StepOutcome` contracts and
call flow:

- `FsOps::step_chmod` -> `FsOps::chmod_inode`
- `FsOps::step_chown` -> `FsOps::chown_inode`
- `FsOps::step_read_projected` -> `FsOps::read_projected`
- `FsOps::step_read_projected_with_netns` -> `FsOps::read_projected_with_netns`
- `FsOps::step_write_projected` -> `FsOps::write_projected`
- `FsOps::step_write_projected_with_netns` -> `FsOps::write_projected_with_netns`

Updated implementations/callers cover tmpfs, devfs, bdevfs, procfs, sysfs, the
VFS composite StepOp wrappers, OpenFile projected I/O dispatch, chmod/chown
syscall arms, focused tests, and the cred-check invariant whitelist.

Current `step-interface` count after this follow-up:

- legacy `fn step_*` surfaces: 195
- `impl StepOp` wrappers: 116
- files with legacy step surfaces: 54
- legacy files with no StepOp impls: 33

The `tx-fs` bucket is gone from the report. Compared with the prior exec
follow-up count, this removes 24 production `fn step_*` surfaces: 18 TXFS
backend method implementations and 6 VFS trait/default method surfaces.

Remaining VFS old step names are the separate walker/OpenFile execution
surfaces (`step_walk*`, `step_open*`, `OpenFile::step_read`, `step_write`,
`step_lseek`, `step_ioctl`, and the internal tty ioctl bridge). Those were not
renamed in this TXFS slice because they are real VFS execution entry points and
already have surrounding StepOp wrappers in several paths.

Verification:

- `cargo fmt --check -p tx-fs -p tx-subsystems -p tx-shims -p tx-kernel -p tx-ext4`
- `cargo xtask lint invariants step-interface`
- `cargo xtask lint invariants step-v4-vocabulary`
- `cargo xtask lint invariants no-adhoc-drive`
- `git diff --check -- <touched Rust files>`

Attempted `cargo check -p tx-fs -q`, but the dirty checkout fails before
reaching TXFS due to an unrelated `tx-reactor/src/wait.rs` compile error:
`Deadline::new(self.deadline_ns)` no longer exists; the current API exposes
`Deadline::from_raw`.

## 2026-07-06 Follow-up: execve StepOp Dispatch

The reachable syscall-facing exec path now uses the Step interface. `sys_execve`
constructs `tx_scripts::process::exec::ExecScriptOp` and drives it with
`drive_oneshot`; `exec_script` remains exported for bootstrap and existing
script tests.

The stale disabled `crates/tx-shims/src/linux_syscall/exec_op.rs` shim was
removed. It targeted an older API and was the exec-related source of most
remaining ad-hoc outcome dispatch reported by `no-adhoc-drive`.

Current `step-interface` count after this follow-up:

- legacy `fn step_*` surfaces: 195
- `impl StepOp` wrappers: 116
- files with legacy step surfaces: 54
- legacy files with no StepOp impls: 33

The total StepOp count is unchanged from the TXFS slice because this pass adds
`ExecScriptOp` while deleting the stale disabled `ExecOp` implementation.
The `tx-scripts` bucket now reports `0` legacy step functions and `1` StepOp.

Current `no-adhoc-drive` result after deleting the stale exec shim:

- files with ad-hoc outcome dispatch: 2
- total ad-hoc outcome sites: 2
- remaining files: `crates/tx-shims/src/linux_syscall/clone_op.rs` and
  `crates/tx-subsystems/src/vfs/composite.rs`

Verification:

- `cargo fmt --check -p tx-scripts -p tx-shims`
- `git diff --check -- crates/tx-scripts/src/process/exec/script.rs crates/tx-scripts/src/process/exec/mod.rs crates/tx-scripts/src/adapter.rs crates/tx-shims/src/linux_syscall/proc.rs crates/tx-shims/src/linux_syscall/mod.rs crates/tx-shims/src/linux_syscall/numbers.rs crates/tx-shims/src/linux_syscall/exec_op.rs`
- `cargo check -p tx-scripts -q`
- `cargo check -p tx-shims -q`
- `cargo test -p tx-shims execve -- --nocapture`
- `cargo xtask lint invariants step-interface`
- `cargo xtask lint invariants no-adhoc-drive`
- `cargo xtask lint invariants step-v4-vocabulary`

Attempted `cargo test -p tx-scripts exec_script -- --nocapture`, but the dirty
checkout still fails before the exec tests due to an unrelated timer API mismatch
in `crates/tx-scripts/tests/drive.rs`: it imports `TimerWheel` from
`tx_scripts::adapter::wake`, but that adapter no longer re-exports
`TimerWheel`.
