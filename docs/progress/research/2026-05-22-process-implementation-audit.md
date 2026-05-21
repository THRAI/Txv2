# Process Implementation Audit

Date: 2026-05-22
Scope: `crates/tx-subsystems/src/process/`, `crates/tx-subsystems/src/thread_runtime/`, `crates/tx-shims/src/linux_syscall/`, `crates/tx-scripts/src/process/exec/`

## Context

This audit checks the live process implementation against:

- `docs/design/04_process-signals/PROCESS_v1.md`
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- `docs/design/02_execution/EXEC_v1.md`
- `docs/Txv3/03_STEP_MODEL_v2.md`
- `docs/Txv3/04_SYSCALL_SHAPE_v1.md`

The known topology-container backing mismatch (`DllContainer` in spec vs `Vec`
behind wrapper interfaces in code) is out of scope except where the wrapper
leaks into semantics.

## Findings

1. **Multi-threaded exec arms GroupExit but never collapses or waits.**
   `EXEC_v1` requires the Process peer to detect multi-threaded exec, initiate
   `GroupExit { is_exec: true }`, wait for siblings to terminate, and clear the
   episode after exec commit (`EXEC_v1` lines 249-254). `exec_script` only calls
   `payload.install_exec_group_exit()` and ignores the boolean result
   (`script.rs` lines 803-806). The same file still says there are no sibling
   threads (`script.rs` lines 964-971), but `sys_clone` now accepts
   `CLONE_THREAD` and `step_clone_thread` attaches siblings (`proc.rs` lines
   257-349; `execution.rs` lines 571-595). `install_exec_group_exit` only stores
   a `GroupExitState` (`structure.rs` lines 1337-1356); it does not fan out
   termination, park the initiator, reduce the thread list to one, or clear the
   episode. This violates the thread/process provision (`PROCESS_v1` lines
   134-156), GroupExit flow (`PROCESS_v1` lines 546-589), and exec PONR shape.

2. **`step_fork` publishes pid namespace state before later fallible work.**
   `PROCESS_v1` says pid allocation uses reservation plus infallible commit,
   with drop rollback before commit (`PROCESS_v1` lines 1371-1375). Current
   `step_fork` allocates and registers the child pid at `execution.rs` lines
   392-397, then performs later fallible work: `sign_thread` at line 399 and
   `sign_process_payload` at lines 412-425. If either allocation fails, the pid
   registry retains a partially constructed zombie-like `ProcessIdentity` that
   was never attached to `parent.children` or pgrp members. The global namespace
   implementation is an immediate `BTreeMap::insert` with no reservation object
   (`numbers.rs` lines 53-67, 88-95), so there is no rollback hook.

3. **Thread count is stale after ordinary thread exit, which poisons GroupExit.**
   `step_clone_thread` increments `thread_count` when a sibling is attached
   (`execution.rs` lines 590-593), but `step_thread_exit` removes a thread from
   the roster without decrementing the atomic (`thread_runtime/execution.rs`
   lines 135-143). `install_exec_group_exit` keys exclusively off the atomic
   (`structure.rs` lines 1342-1356), so after thread churn it can overestimate
   live siblings and set `remaining_threads` to a count that will never reach
   zero.

4. **Exit paths unregister the process pid at zombification, breaking wait and
   kill existence semantics.** `step_exit_group` and `step_process_exit` call
   `unregister_pid(process.pid)` / `unregister_pid_number` before publishing
   the zombie (`execution.rs` lines 616-633 and 661-670). `PROCESS_v1` treats
   the zombie shell as pid-addressable until reap (`PROCESS_v1` lines 142-154,
   1303-1312), and `sys_kill` even documents `sig == 0` as succeeding for live
   or zombie targets (`signal.rs` lines 493-496). Because pid lookup goes
   through the global `process_by_pid` map (`execution.rs` lines 49-55),
   a zombie is no longer resolvable by pid before its parent reaps it.

5. **The pid namespace omits pgrp/session targets and role-capable names.**
   `PROCESS_v1` requires one shared pid/tid/pgid/sid number space whose
   `PidName` may target process, thread, process group, or session, including
   role reuse for pid=pgid=sid (`PROCESS_v1` lines 1332-1365). Code only has
   `PidName::Process` and `PidName::Thread` (`numbers.rs` lines 26-47), while
   `step_setpgid` / `step_setsid` create process-group/session identities
   without registering names (`execution.rs` lines 981-1035). Today many
   pgrp/session syscalls are self-only, but this is a correctness blocker for
   cross-pid `getpgid`/`getsid`, pgrp `kill`, and role reuse semantics.

6. **`setsid` and `setpgid` lack key POSIX observe-phase checks.**
   `PROCESS_v1` commits session/pgroup behavior (`PROCESS_v1` lines
   1427-1434), and `step_setpgid` must check target relationship, same session,
   existing-pgroup rules, and "not exec'd since fork" (`PROCESS_v1` lines
   1123-1143). Current `step_setpgid` only supports `pgid == target.pid`, creates
   a fresh group, and moves the target (`execution.rs` lines 981-1008);
   `sys_setpgid` returns `EPERM` for cross-process targets before consulting any
   canonical pid resolver (`proc.rs` lines 623-658). `step_setsid` always
   creates a new session (`execution.rs` lines 1011-1035), while `sys_setsid`
   documents that the process-group-leader `EPERM` rule is not enforced
   (`proc.rs` lines 694-712).

7. **`wait4` implements only zombie reaping, not the committed stop/continue
   surface.** `PROCESS_v1` lists `wait`, `waitpid`, `waitid`, and `wait4` with
   `WEXITED`, `WSTOPPED`, and `WNOHANG` in v1 (`PROCESS_v1` lines 1427-1431).
   `sys_wait4` ignores `WUNTRACED`/`WCONTINUED` because stop/continue is
   deferred in the implementation (`proc.rs` lines 486-492), and
   `step_waitpid_nohang` only looks for children whose payload is gone
   (`execution.rs` lines 843-925). That is a spec gap, not just an
   unimplemented Linux-specific flag.

8. **Group-exit coordination has no completion-channel or initiator wait.**
   `PROCESS_v1` GroupExit state includes a completion channel and an initiator
   wait (`PROCESS_v1` lines 563-589). Current `GroupExitState` has only
   `status`, `is_exec`, and `remaining_threads` (`structure.rs` lines
   835-842). `step_thread_exit` decrements `remaining_threads` but does not wake
   anyone when it reaches zero (`thread_runtime/execution.rs` lines 87-105).
   `step_exit_group` is currently a synchronous all-thread zombifier
   (`execution.rs` lines 607-638), bypassing the per-thread exit sequencing
   described by `THREAD_RUNTIME_v1` and making `group_exit` mostly inert.

9. **Exec performs fallible, allocating work in the pre-PONR phase after
   installing a group-exit episode.** The group-exit install at `script.rs`
   lines 803-806 happens before the partial-last-page eager allocation/read loop
   and stack population (`script.rs` lines 824-962), both of which can return
   errors. `EXEC_v1` puts process collapse after all reversible preparation
   and before the address-space swap (`EXEC_v1` lines 330-343, 1403-1410).
   Because current code does not clear `group_exit` on later error and does not
   actually collapse siblings, an exec failure can leave a stale episode.

10. **The live exec path has drifted away from the StepOp migration surface.**
    `sys_execve` explicitly calls `exec_script` and states that `ExecOp` is an
    unfinished refactor (`proc.rs` lines 153-165). `mod.rs` comments out
    `exec_op` and `clone_op` because they target an older API surface (`mod.rs`
    lines 126-133). Keeping large, stale files under `linux_syscall/` is a code
    smell for reviewers: they look like alternate implementations but are not
    compiled or authoritative.

## Already Mitigated Or Out Of Scope

- The DLL-vs-`Vec` storage concern is mitigated by topology wrapper types in
  `process/topology.rs`; this audit does not count the backing container as a
  gap by itself.
- The older 2026-05-07 interface-drift note mentions fixed 8-slot fd storage
  and `AtomicU32` CLOEXEC. The current tree has moved to `BTreeMap<u32,
  Cap<OpenFile>>` and `BTreeSet<u32>`, so those specific findings are stale.
- The flat `ProcessPayload` shape has been ratified as an amendment in
  `PROCESS_v1` lines 257-370; do not treat absence of literal `Shared<T>` fields
  as a standalone bug unless clone/share semantics leak.

## Recommended Next Steps

1. Fix GroupExit/CLONE_THREAD/exec together: either reject multi-threaded exec
   until collapse is real, or implement the initiator wait, sibling termination
   fan-out, thread-count maintenance, episode clear, and tests.
2. Convert pid/tid allocation and registration to a reservation object or delay
   namespace publication until all fallible fork allocations have succeeded.
3. Keep process pid names live until reap; unregister thread tid at thread
   zombie if desired, but process pid withdrawal belongs to parent reap.
4. Extend `PidName` to cover pgrp/session roles before broadening pgrp/session
   syscalls.
5. Delete or quarantine stale `clone_op.rs` / `exec_op.rs`, or refresh and wire
   them through the dispatch table so there is one migration truth.

## Verification

This was a static audit. Commands run:

```sh
git status --short --branch
rg --files | rg '(^docs/(design|Txv3)|^crates/).*(PROCESS|THREAD_RUNTIME|SIGNAL|EXEC|process|thread|signal|exec|runtime|syscall|task|pid|wait|exit)'
rg -n 'process|ThreadRuntime|syscall|AST|exec|fork|clone|wait|signal' /Users/3y/.codex/memories/MEMORY.md
sed -n '1,220p' docs/design/INDEX.md
sed -n '1,260p' docs/Txv3/INDEX.md
rg -n 'StepOutcome|SubjectContext|GroupExit|exec|fork|clone|wait|pid' docs/design/04_process-signals/PROCESS_v1.md docs/design/02_execution/THREAD_RUNTIME_v1.md docs/design/02_execution/EXEC_v1.md docs/Txv3/03_STEP_MODEL_v2.md docs/Txv3/04_SYSCALL_SHAPE_v1.md
nl -ba crates/tx-subsystems/src/process/{structure.rs,execution.rs,topology.rs,numbers.rs,exec_prep.rs}
nl -ba crates/tx-subsystems/src/thread_runtime/{structure.rs,execution.rs}
nl -ba crates/tx-shims/src/linux_syscall/{mod.rs,proc.rs,signal.rs,ctx.rs,clone_op.rs,exec_op.rs}
nl -ba crates/tx-scripts/src/process/exec/script.rs
git diff --check
cargo xtask lint docs
cargo xtask progress validate
```

`git diff --check` passed. `cargo xtask lint docs` passed with the existing
stale-vocabulary warnings. `cargo xtask progress validate` is still blocked by
the pre-existing invalid plan status `completed` in
`docs/progress/plans/2026-05-19-pthread-shared-clone-thread.json`; the validator
expects `complete`. No code or implementation tests were changed by this audit.
