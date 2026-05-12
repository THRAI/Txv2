# Commit groupings recipe — `cc/optimistic-hodgkin-45bc8c`

**Date:** 2026-05-12
**Companion ADR:**
[D14 stale-commit cleanup plan](decisions/2026-05-12-d14-stale-commit-cleanup-plan.md)
**Status:** draft recipe — verify each `git add` glob matches `git
status --short` before each commit.

This file is the **human-execution recipe** for landing the 39-worker
v3 migration as 17 ordered commits. Run from the repo root. Each block
is one commit. Do not skip the `git status` between commits — drift
detection.

## Pre-flight

```bash
# Confirm baseline matches D14 §2.1
git rev-parse HEAD                          # expect a53d200
git status --short | wc -l                  # expect 150
git diff --stat HEAD | tail -1              # expect ~150 / 12120 / 1186
cargo check --workspace --tests             # expect clean
```

If any check fails: stop, diagnose. Do not proceed.

## Commit 1 — PR-A rename trail

```bash
git add crates/tx-subsystems/src/wait_carrier.rs            # delete
git add crates/tx-subsystems/src/wait_source.rs             # add
git add crates/tx-subsystems/src/process/tests/exit_port.rs # delete
git add crates/tx-subsystems/src/process/tests/exit_source.rs # add

git status --short        # sanity: only the four above + untracked dirs
git commit -m "$(cat <<'EOF'
v3 PR-A: wake-substrate vocabulary cleanup

Renames wait_carrier->wait_source and exit_port->exit_source per the
W-A.1..A.4 wave. Companion ADR refs:

- STATUS.md lines 2316 / 2335 / 2300 / 2357.
- docs/progress/decisions/2026-05-12-d10-vocabulary-retire-audit.md.

Workers: W-A.
EOF
)"
```

## Commit 2 — PR-3A/3B/3C/3D-0 wake-substrate foundation

```bash
git add crates/tx-substrate/src/wake/
git add crates/tx-substrate/src/lib.rs
git add crates/tx-reactor/src/mailbox.rs
git add crates/tx-reactor/src/wait_source.rs
git add crates/tx-reactor/src/lib.rs

git commit -m "$(cat <<'EOF'
v3 PR-3A/3B/3C/3D-0: wake-substrate foundation + object-owned publication

Lands the wake-substrate scaffold in tx-substrate::wake and the
reactor-side mailbox + wait-source surfaces. PR-3D-0 relocates
TaskMailbox to tx-substrate::wake per W-D.

STATUS.md refs: 2242 / 2224 / 2198 / 1796.

Workers: W-A (PR-3A/3B/3C), W-D (PR-3D-0).
EOF
)"
```

## Commit 3 — PR-2 R1 + PR-5/PR-6 StepOp adapters

```bash
git add crates/tx-subsystems/src/page_backed.rs
git add crates/tx-subsystems/src/page_backed/core_tests.rs
# (page_backed/user_buffer*.rs lands later with PR-11 phase 6/7)
# (page_backed/cross_variant.rs / lifecycle*.rs / targeted_read.rs
#  also land later with PR-11)
git add crates/tx-subsystems/src/tty/execution/step_hangup.rs
git add crates/tx-subsystems/src/tty/execution/step_ingest.rs
git add crates/tx-subsystems/src/tty/execution/step_ioctl.rs
git add crates/tx-subsystems/src/tty/execution/step_master_close.rs
git add crates/tx-subsystems/src/tty/execution/step_openpty.rs
git add crates/tx-subsystems/src/tty/execution/step_poll_hardware.rs
git add crates/tx-subsystems/src/tty/execution/step_read.rs
git add crates/tx-subsystems/src/tty/execution/step_write.rs

git commit -m "$(cat <<'EOF'
v3 PR-2 R1 + PR-5/PR-6: StepOp adapter wraps for page_backed + tty

Wave-3 + R1 cleanup of the StepOp adapter scaffolding. Adapter wraps
land alongside the free-fn caller path per
docs/Txv3/03_STEP_MODEL_v2.md §2.1 (intentional dead_code per D12).

STATUS.md refs: 2064 / 2080 / 2102 / 2121 / 2165.

Workers: W-A (waves 1-3 + R1).
EOF
)"
```

## Commit 4 — PR-7/7B/7C OnAgent delegate runtime

```bash
git add crates/tx-substrate/src/step_v3/agent.rs
git add crates/tx-substrate/src/step_v3/execution_scope.rs
git add crates/tx-substrate/src/step_v3/mod.rs
git add crates/tx-substrate/src/step_v3/page_progress.rs
git add crates/tx-substrate/tests/v3_pr7_delegate_runtime.rs
git add crates/tx-substrate/tests/v3_pr7b_mailbox_integration.rs
git add crates/tx-reactor/tests/v3_pr7b_timer_routing.rs
git add crates/tx-reactor/tests/v3_timer_surface.rs
git add crates/tx-reactor/src/timer.rs
git add crates/tx-reactor/src/hart_loop.rs
git add crates/tx-reactor/src/agent_reply.rs
git add docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md

git commit -m "$(cat <<'EOF'
v3 PR-7/7B/7C: OnAgent delegate runtime + mailbox + TimerWheel ADR

DelegateRegistry, mark_replied, await_agent_reply seam, plus
DelegateRequest/Reply closed sums. PR-8 timer surface publish lands
adjacent. ADR D6 records the TimerWheel layering choice.

STATUS.md refs: 1738 / 1619 / 1585 / 1865 / 1884.

Workers: W-H (PR-7/7B/7C), W-A (PR-9 phase 3a/3b fanout adjustments).
EOF
)"
```

## Commit 5 — PR-9 phases 1–5 SubjectContext + Cap-shape

```bash
git add crates/tx-substrate/src/step_v3/subject_context.rs
git add crates/tx-substrate/tests/v3_subject_context.rs
git add crates/tx-substrate/tests/v3_algebra.rs
git add crates/tx-substrate/tests/v3_helpers.rs
git add crates/tx-substrate/tests/v3_step_op.rs
git add crates/tx-substrate/tests/v3_yield_on_agent.rs
git add crates/tx-substrate/tests/v3_execution_scope.rs
git add crates/tx-substrate/tests/bus.rs
git add crates/tx-substrate/tests/v3_endpoint_scope_abandonment.rs
git add crates/tx-subsystems/src/cred.rs
git add crates/tx-subsystems/src/cred/tests.rs
git add crates/tx-subsystems/tests/v3_cred_zone_allocation.rs
git add crates/tx-subsystems/src/zones.rs
git add docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md
git add docs/progress/decisions/2026-05-11-d5-cred-zone-allocation.md

git commit -m "$(cat <<'EOF'
v3 PR-9 phases 1-5: SubjectContext + Cap-shape reshape + Cred zone

Cap-shape reshape across the substrate (W-A 3-up, W-E phase 4), Cred
zone-allocation + SubjectContext population (W-J phase 5). D1 records
the ScriptCtx trait-bound identity decision; D5 records Cred
zone-allocation.

STATUS.md refs: 1938 / 1911 / 1884 / 1819 / 1764 / 1709 / 1484.

Workers: W-A, W-E, W-I (decided), W-J.
EOF
)"
```

## Commit 6 — PR-3D-1 pipe WaitSource

```bash
git add crates/tx-subsystems/src/pipe.rs
git add crates/tx-subsystems/tests/v3_pipe_waitsource.rs
git add docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md
git add docs/progress/decisions/2026-05-11-d2-waitsource-coexists-with-rawport.md
git add docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md

git commit -m "$(cat <<'EOF'
v3 PR-3D-1: pipe WaitSource migration

First D2/D4 coexistence migration: pipe runs both Channel + WaitSource
under the bus boundary clarified by D4.

STATUS.md ref: 1679. Worker: W-G.
EOF
)"
```

## Commit 7 — PR-3D-2 futex WaitSource

```bash
git add crates/tx-subsystems/src/futex.rs
git add crates/tx-subsystems/tests/v3_futex_waitsource.rs

git commit -m "$(cat <<'EOF'
v3 PR-3D-2: futex WaitSource migration

STATUS.md ref: 1536. Worker: W-K.
EOF
)"
```

## Commit 8 — PR-3D-3 exit_source WaitSource

```bash
git add crates/tx-subsystems/src/process/execution.rs
git add crates/tx-subsystems/src/process/mod.rs
git add crates/tx-subsystems/src/process/structure.rs
git add crates/tx-subsystems/src/process/tests.rs
git add crates/tx-subsystems/tests/v3_exit_wait_source.rs

git commit -m "$(cat <<'EOF'
v3 PR-3D-3: exit_source WaitSource migration

STATUS.md ref: 1339. Worker: W-M.
EOF
)"
```

## Commit 9 — PR-3D-4 tty WaitSource

```bash
# step_*.rs already in commit 3? NO — verify git status that tty
# step files are NOT already staged. If they are, this commit is empty.
# The tty WaitSource migration touches the *structure* + *tests*,
# whereas commit 3's adapter wraps touched the *execution* step files.
# In practice the 8 step_*.rs files belong to commit 3 (StepOp wraps)
# and the structure/tests files belong here.

git add crates/tx-subsystems/src/tty/structure/identity.rs
git add crates/tx-subsystems/src/tty/tests/execution_poll_hardware.rs
git add crates/tx-subsystems/tests/v3_tty_waitsource.rs

git commit -m "$(cat <<'EOF'
v3 PR-3D-4: tty WaitSource migration

Note: tty execution/step_*.rs adapter wraps landed in PR-2 R1
(commit 3); this commit adds the WaitSource-shaped identity field +
the poll-hardware test + the integration test.

STATUS.md ref: 1243. Worker: W-P.
EOF
)"
```

## Commit 10 — PR-3D-5 vfs WaitSource + walker carveout

```bash
git add crates/tx-subsystems/src/vfs/execution.rs
git add crates/tx-subsystems/src/vfs/mod.rs
git add crates/tx-subsystems/src/vfs/structure.rs
git add crates/tx-subsystems/src/vfs/walker.rs
git add crates/tx-subsystems/tests/v3_vfs_waitsource.rs
git add docs/progress/decisions/2026-05-11-d3-walker-async-carveout.md
git add docs/progress/decisions/2026-05-11-pr-1-6-keep-fsops.md

git commit -m "$(cat <<'EOF'
v3 PR-3D-5: vfs WaitSource migration + walker async carveout

The last mechanical bus consumer. D3 records the walker async carveout
shape; pr-1-6-keep-fsops records why FsOps stays in PR-1.6.

STATUS.md ref: 961. Worker: W-S.
EOF
)"
```

## Commit 11 — PR-10 phases 0–6 userfaultfd canary

```bash
git add crates/tx-subsystems/src/userfaultfd.rs
git add crates/tx-subsystems/src/vm/execution.rs
git add crates/tx-subsystems/src/vm/mod.rs
git add crates/tx-subsystems/src/vm/pmap.rs
git add crates/tx-subsystems/src/vm/structure/address_space.rs
git add crates/tx-subsystems/src/vm/structure/mod.rs
git add crates/tx-subsystems/src/vm/structure/range_lock.rs
git add crates/tx-subsystems/src/vm/structure/recipe.rs
git add crates/tx-subsystems/src/vm/structure/types.rs
git add crates/tx-subsystems/src/vm/user_access.rs
git add crates/tx-subsystems/src/vm/tests/script_async.rs
git add crates/tx-shims/src/linux_syscall/userfaultfd.rs
git add crates/tx-shims/src/linux_syscall/vm.rs
git add crates/tx-shims/tests/v3_userfaultfd_syscall_scaffold.rs
git add crates/tx-shims/tests/v3_userfaultfd_register.rs
git add crates/tx-shims/tests/v3_userfaultfd_ioctl_reply.rs
git add crates/tx-subsystems/tests/v3_userfaultfd_fd_scaffold.rs
git add crates/tx-subsystems/tests/v3_userfaultfd_fault_path.rs
git add crates/tx-subsystems/tests/v3_userfaultfd_e2e.rs
git add docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md

git commit -m "$(cat <<'EOF'
v3 PR-10 phases 0-6: userfaultfd OnAgent canary

End-to-end userfaultfd validation of the OnAgent runtime.
Phase 0 fd scaffold (W-Q); phases 1+2 register + reply registry (W-T);
phase 3 UFFDIO_REGISTER + VmEntry::ufd_registration (W-V); phase 4
fault-path OnAgent branch + await_agent_reply (W-Y); phase 5 reply
ioctls + pending-fault queue (W-BB); phase 6 OnAgent canary +
ProcessUfdDispatch (W-EE). ADR D7 records the readiness plan.

STATUS.md refs: 1457 / 1097 / 1154 / 728 / 648 / 490 / 401.

Workers: W-O (D7), W-Q, W-T, W-V, W-Y, W-BB, W-EE.
EOF
)"
```

## Commit 12 — PR-11 phases 0–7 AIO canary on OnBehalfOf<P>

```bash
git add crates/tx-substrate/src/step_v3/on_behalf_of.rs
git add crates/tx-substrate/tests/v3_pr11_on_behalf_of.rs
git add crates/tx-substrate/tests/v3_agent_token_guard_timer.rs
git add crates/tx-subsystems/src/aio.rs
git add crates/tx-shims/src/linux_syscall/aio.rs
git add crates/tx-shims/tests/v3_aio_io_setup.rs
git add crates/tx-shims/tests/v3_aio_io_submit.rs
git add crates/tx-shims/tests/v3_aio_io_getevents.rs
git add crates/tx-shims/tests/v3_aio_io_destroy.rs
git add crates/tx-shims/tests/v3_aio_e2e.rs
git add crates/tx-shims/tests/v3_subject_population.rs
git add crates/tx-subsystems/src/page_backed/user_buffer.rs
git add crates/tx-subsystems/src/page_backed/user_buffer_tests.rs
git add crates/tx-subsystems/src/page_backed/cross_variant.rs
git add crates/tx-subsystems/src/page_backed/lifecycle.rs
git add crates/tx-subsystems/src/page_backed/lifecycle_tests.rs
git add crates/tx-subsystems/src/page_backed/targeted_read.rs
git add crates/tx-subsystems/tests/v3_openfile_page_backed_read.rs
git add docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md

git commit -m "$(cat <<'EOF'
v3 PR-11 phases 0-7: AIO canary on OnBehalfOf<P>

End-to-end AIO validation of the OnBehalfOf<P> framework primitive.
Phase 0 framework (W-W); phase 1 io_setup fd scaffold (W-Z); phase 2
io_submit + worker dispatch (W-CC); phases 3+4+5 real dispatch +
io_getevents + io_destroy (W-FF); phases 6+7 e2e + doc updates (W-JJ);
follow-up PageBacked dispatch ENOSYS closure (W-KK). ADR D8 records
the plan.

STATUS.md refs: 931 / 885 / 805 / 319 / 264 / 145.

Workers: W-W, W-Z, W-CC, W-FF, W-JJ, W-KK.
EOF
)"
```

## Commit 13 — PR-12 scaffold io_uring SQPOLL

```bash
git add crates/tx-subsystems/src/io_uring.rs
git add crates/tx-shims/src/linux_syscall/io_uring.rs
git add crates/tx-shims/tests/v3_io_uring_sqpoll_scaffold.rs

git commit -m "$(cat <<'EOF'
v3 PR-12 scaffold: io_uring SQPOLL on OnBehalfOf<P>

Second canary for the OnBehalfOf<P> framework. Zero new framework
primitives — proves W-W's "zero additional framework work" prediction.

STATUS.md ref: 77. Worker: W-LL.
EOF
)"
```

## Commit 14 — D9 phases A–D signal subsystem + signalfd

```bash
git add crates/tx-subsystems/src/signal.rs
git add crates/tx-subsystems/src/signal/tests/kill_permission.rs
git add crates/tx-subsystems/src/signal/tests/tty_bridge.rs
git add crates/tx-subsystems/src/thread_runtime/execution.rs
git add crates/tx-subsystems/src/thread_runtime/structure.rs
git add crates/tx-subsystems/src/signalfd.rs
git add crates/tx-shims/src/linux_syscall/signalfd.rs
git add crates/tx-subsystems/tests/v3_signal_eligibility.rs
git add crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs
git add crates/tx-subsystems/tests/v3_signal_mailbox.rs
git add crates/tx-subsystems/tests/v3_signalfd.rs
git add docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md

git commit -m "$(cat <<'EOF'
v3 D9 phases A-D: signal subsystem migration + signalfd subsystem

D9-A post-after-lock-drop + thread mailbox (W-AA, STATUS earlier
session); D9-B eligibility scan + D9-C interrupt-wake integration pin
(W-DD); D9-D signalfd subsystem + sys_signalfd4 syscall (W-II). ADR
D9 records the wake-migration plan.

STATUS.md refs: 5689 / 573 / 196. Workers: W-X (ADR), W-AA, W-DD, W-II.
EOF
)"
```

## Commit 15 — shim surface + Cargo.lock catch-all

```bash
git add crates/tx-shims/src/lib.rs
git add crates/tx-shims/src/linux_syscall/mod.rs
git add crates/tx-shims/src/linux_syscall/numbers.rs
git add crates/tx-shims/src/linux_syscall/fs_basic.rs
git add crates/tx-shims/src/linux_syscall/io.rs
git add crates/tx-shims/src/linux_syscall/proc.rs
git add crates/tx-shims/src/linux_syscall/tests.rs
git add crates/tx-shims/src/linux_syscall/tests/fork_clone_wait4_wave3.rs
git add crates/tx-subsystems/Cargo.toml
git add crates/tx-subsystems/src/lib.rs
git add crates/tx-subsystems/src/execution.rs
git add crates/tx-fs/src/tmpfs.rs
git add Cargo.lock

git commit -m "$(cat <<'EOF'
v3 shim surface: syscall dispatch + numbers + Cargo.lock

Linux-syscall surface glue for the new fd kinds (ufd / signalfd / aio /
io_uring): dispatch arms, NR_* numbers, flag constants. tx-subsystems
test-support feature gate. Cargo.lock catch-all bundled here.

STATUS.md refs: dispatch glue from W-Q (1097), W-T (1154), W-V (728),
W-Y (648), W-BB (490), W-Z (885), W-CC (805), W-FF (319), W-II (196),
W-LL (77), W-KK (145).

Workers: W-Q, W-T, W-V, W-Y, W-BB, W-Z, W-CC, W-FF, W-II, W-LL, W-KK.
EOF
)"

# Sanity gate after dispatch + numbers + lib glue land:
cargo check --workspace --tests
```

## Commit 16 — design-doc updates

```bash
git add docs/design/
git add docs/Txv3/06_EXECUTION_SCOPE_v1.md
git add docs/Txv3/07_BLAST_RADIUS.md

git commit -m "$(cat <<'EOF'
v3 design-doc updates: txdoc anchors + Txv3 reactor docs

Wide doc sweep across docs/design/** and docs/Txv3/** for the 39-worker
migration: PR-10 / PR-11 landed-marks in 07_BLAST_RADIUS §5.2; AIO
worker + migration order in 06_EXECUTION_SCOPE §8.2 / §12; txdoc tag
maintenance across the meta-framework, substrate, execution, memory-vm,
process-signals, filesystem, and devices sections; INDEX.md refresh.

STATUS.md refs: 2262 (design-doc sweep), and inline doc updates from
W-W, W-T, W-JJ.

Workers: W-A (sweep), W-W, W-T, W-JJ.
EOF
)"
```

## Commit 17 — progress (STATUS + ADRs + plans + audits)

```bash
git add docs/progress/STATUS.md
git add docs/progress/migration-completion-audit-2026-05-12.md
git add docs/progress/plans/2026-05-11-v3-migration-remaining.md
git add docs/progress/decisions/2026-05-12-d10-vocabulary-retire-audit.md
git add docs/progress/decisions/2026-05-12-d11-d2-coexistence-retire-plan.md
git add docs/progress/decisions/2026-05-12-d12-dead-code-todo-audit.md
git add docs/progress/decisions/2026-05-12-d14-stale-commit-cleanup-plan.md
git add docs/progress/2026-05-12-commit-groupings-draft.md

# Final sanity gates before the tail commit:
cargo check --workspace --tests
cargo xtask progress validate
# Optional but recommended: full test sweep
# cargo test --workspace -- --test-threads=1   # expect 1663/0/11

git commit -m "$(cat <<'EOF'
v3 progress: STATUS catchup + ADRs D10/D11/D12/D14 + plans + audit

Tail commit landing the worktree-wide narrative for the 39-worker v3
migration. STATUS.md catchup (+2397 LoC); ADRs D10 (vocabulary retire
audit, W-NN), D11 (D2 coexistence retire plan, W-OO), D12 (dead-code +
TODO audit, W-PP), D14 (this stale-commit cleanup plan, W-RR);
migration-completion audit (W-GG); v3-migration-remaining plan.

STATUS.md refs: lines 1-700 (the most recent migration entries).

Workers: W-A through W-PP + W-RR (planning-only).
EOF
)"

# Sanity tail:
git status --short          # expect EMPTY
git log --oneline -20       # expect commits 17..1 on top of a53d200
```

## Post-flight

```bash
# Spot-check: every commit listed below should be present, in order.
git log --oneline | head -17

# Verify zero working-tree drift:
git status

# Optional: spot-check that bisection works for the WaitSource series
# git bisect start; git bisect bad HEAD~12; git bisect good HEAD~17
```

## What to do if a commit gate fails

If `cargo check --workspace --tests` fails between commits:

1. **Do not amend.** The commit landed; amending modifies it
   destructively and may lose work per CLAUDE.md.
2. Fix the issue with a follow-up commit (`v3 fixup: …`).
3. Re-run `cargo check --workspace --tests` to confirm green.

If `cargo xtask progress validate` fails on commit 17:

1. Read the validator output — likely a JSON shape issue in the new
   ADR / plan files. Fix the file, `git add`, amend the same commit
   only if it's the **most recent** commit and nothing else has landed
   on top (this is the one CLAUDE.md amend-exception: a fresh post-fail
   re-commit, not modifying older history).

## What this recipe is NOT

- Not a rebase. Commits land linearly on top of `a53d200`.
- Not a force-push. After completion the branch is fast-forwarded only.
- Not interactive. Every step is scripted with explicit file paths.
- Not idempotent. Re-running after partial completion requires
  inspecting `git status` to skip already-landed files.

---

End of recipe.
