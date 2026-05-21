# Process Implementation Status Quo

Date: 2026-05-22
Scope: `crates/tx-subsystems/src/process/`, `crates/tx-subsystems/src/thread_runtime/`, `crates/tx-scripts/src/process/exec/`

## Context

This records the post-audit implementation state after the first fix pass over
the process lifecycle gaps. The earlier static audit remains the historical
input snapshot:

- `docs/progress/research/2026-05-22-process-implementation-audit.md`

The known `DllContainer` vs `Vec` topology backing mismatch remains out of
scope because the process topology wrapper interfaces are the semantic
boundary.

## Current State

- `PidName` now models process, thread, process-group, and session roles.
  Bootstrap, `setpgid`, and `setsid` register role-capable names in the shared
  pid/tid/pgid/sid value space.
- Fork now publishes the child process pid only after the later fallible leader
  thread and payload allocations have succeeded.
- Process pid identity now remains addressable while the process is zombie and
  is withdrawn at parent reap.
- Ordinary thread exit decrements `ProcessPayload::thread_count`; live-thread
  accounting now uses the atomic count rather than the roster length.
- Exec collapse has a process-side helper that zombifies sibling threads,
  leaves the initiating thread live, and clears the temporary exec group-exit
  episode. The production `exec_script` path calls this helper after reversible
  ELF/stack preparation and before address-space replacement.
- `setsid` rejects zombie targets and existing process-group leaders.
- The process `StepOp` wrapper smoke tests now live in
  `crates/tx-subsystems/src/process/step_op_wraps.rs`.

## Remaining Gaps

- `wait4`/`waitpid` still covers zombie reaping but not the committed
  stop/continue surface (`WUNTRACED`, `WCONTINUED`, waitid-style events).
- `setpgid` still only handles fresh self-rooted process groups; joining an
  existing group, cross-process target checks, same-session checks, and the
  "not exec'd since fork" rule remain follow-up work.
- Group-exit coordination is still synchronous and local. It does not yet have
  the spec's explicit completion channel or true initiator wait path.
- Stale syscall migration artifacts (`clone_op.rs`, `exec_op.rs`) remain
  present but not wired as the authoritative dispatch path.
- `cargo test -p tx-subsystems ...` package-level focused runs are currently
  blocked before the target test by `crates/tx-subsystems/tests/v3_signal_mailbox.rs`
  calling `post_signal` with the old three-argument signature.
- `cargo xtask progress validate` remains blocked by the pre-existing invalid
  `completed` status in
  `docs/progress/plans/2026-05-19-pthread-shared-clone-thread.json`; the schema
  expects `complete`.

## Verification

Passed:

```sh
cargo fmt
cargo test -p tx-scripts exec_script_collapses_sibling_threads_before_aspace_swap
cargo test -p tx-subsystems --lib ordinary_thread_exit_decrements_live_thread_count
cargo test -p tx-subsystems --lib exec_group_collapse_keeps_initiator_and_clears_episode
cargo test -p tx-subsystems --lib last_thread_exit_zombifies_process_keeps_identity
cargo test -p tx-subsystems --lib zombie_process_pid_remains_resolvable_until_reap
cargo test -p tx-subsystems --lib setsid_rejects_existing_process_group_leader
cargo check -p tx-subsystems
```

Blocked:

```sh
cargo test -p tx-subsystems ordinary_thread_exit_decrements_live_thread_count
```

The package-level test compile fails in the unrelated `v3_signal_mailbox`
integration test because it still calls `post_signal(&thread, sig, info)`
instead of the current `post_signal(&thread, sig, routing, info)` signature.
