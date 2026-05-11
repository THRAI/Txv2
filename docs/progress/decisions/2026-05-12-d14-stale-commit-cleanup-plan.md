# Decision D14: Stale-commit cleanup plan for `cc/optimistic-hodgkin-45bc8c`

**Date:** 2026-05-12
**Status:** decided (plan only — execution deferred to a human operator)
**Worker:** W-RR (planning, no git mutations)
**Companion:**
[D10 (vocabulary retire audit)](2026-05-12-d10-vocabulary-retire-audit.md),
[D11 (D2-coexistence retire plan)](2026-05-12-d11-d2-coexistence-retire-plan.md),
[D12 (dead-code + TODO audit)](2026-05-12-d12-dead-code-todo-audit.md),
[migration-completion-audit-2026-05-12](../migration-completion-audit-2026-05-12.md)

## 1. Executive summary

The current worktree (`cc/optimistic-hodgkin-45bc8c`) carries the entire
39-worker v3 migration in its **uncommitted** working tree (150 files,
+12 120 / -1 186 LoC) on top of a clean baseline. The baseline itself
already contains 21 prior `v3 unification phase N` commits from an
earlier session. This ADR proposes:

- **17 ordered commits** to land the uncommitted work, grouped by PR /
  phase and crediting workers in commit bodies (not titles), reviewable
  in 50–1200 LoC chunks.
- **Keep the 21 prior `v3 unification phase N` commits as-is.** They
  are merged-equivalent baseline; rewriting them is destructive and
  delivers no value.
- A 1–2 hour human-driven `git add` / `git commit` recipe; no force-push,
  no `--amend`, no interactive rebase.

The companion file
`docs/progress/2026-05-12-commit-groupings-draft.md` carries the exact
`git add` / `git commit` sequence with paths.

## 2. Current state

### 2.1 Branch position

```text
HEAD  == a53d200 (a53d200 [docs] txV3 reactor docs)
main  == a53d200 (same commit)
merge-base(HEAD, main) == a53d200
```

`main..HEAD` is empty; `HEAD..main` is empty. **The worktree branch
has not diverged from `main` in committed history** — every byte of the
39-worker migration is in the working tree only. No mainline rebase is
required before landing.

### 2.2 Uncommitted footprint

- **150 files** in `git status --short`
  - 110 modified (`M`)
  - 28 untracked (`??`)
  - 2 added-staged (`A `)
  - 2 deleted (`D`) — `wait_carrier.rs` → `wait_source.rs`,
    `exit_port.rs` → `exit_source.rs` (PR-A rename trail)
- **Diff stat tail:** `110 files changed, 12151 insertions(+),
  989 deletions(-)` in the part `--stat` summarised; full
  `--shortstat` reads `150 files changed, 12120 insertions(+),
  1186 deletions(-)`.
- **STATUS.md** alone accounts for **2 397** of the inserted lines and
  is touched by every single worker (W-A → W-PP).

### 2.3 Recent commit graph (top 12)

```text
a53d200 [docs] txV3 reactor docs
76d4cd9 [docs] txV3 reactor docs
5a1ca1e drivers: use crates.io virtio-drivers instead of local third_party path
1682958 ci: run check workflow on push to main and la
38652b2 ci: remove stale third_party patches and install rust targets
85eccec 处理冲突
164ea3e la merge main
49c1bcb la64 & docker (drop vendored third_party)
28598b6 Merge pull request #22 from THRAI/cc/quirky-mcclintock-5c2d1f
85d3704 v3 unification phase 17: fix remaining clippy errors for CI green
007350e v3 unification phase 21: DELETE v4 StepOutcome enum entirely
400f04c v3 unification phase 20: BlockDeviceOps + remaining v4 callers + import cleanup
```

Style observation: the prior `v3 unification phase N:` commits use the
form `v3 unification phase <N>: <verb-led summary>` with bodies omitted.
We deliberately diverge by using `v3 PR-<n>: <summary>` for the new
work, because the 39-worker migration is organised by PR/ADR (D1–D12,
PR-3D-0…5, PR-7/7B/7C, PR-9, PR-10, PR-11, PR-12, D9-A..D) not by
linearly numbered phases. Bodies will name worker(s) + ADR.

### 2.4 Pre-commit hooks

`git config --get core.hookspath` returns nothing; the canonical
`.git/hooks/` directory contains only the stock `*.sample` files. **No
pre-commit hook will fire.** Validators (`cargo xtask progress
validate`, `cargo check --workspace --tests`) must be run manually if
desired; this ADR recommends running them before commit 17 (the final
STATUS-only commit).

## 3. Proposed commit groupings

Each commit cites the **canonical STATUS entry line** for traceability.
File-list scope is given by directory roots plus explicit paths where a
crate is split across multiple groups.

### Commit 1 — `v3 PR-A: wake-substrate vocabulary cleanup (W-A.1..A.4)`

**Why first:** these are renames (`wait_carrier`→`wait_source`,
`exit_port`→`exit_source`, `OnCarrier`→`OnWaitSource`) that touch many
files at one identifier each. Landing first reduces noise in later
diffs.

**Files** (subset that is rename-only — the substantive contents in
`tx-substrate/src/step_v3/*.rs` land in commit 4):

- `crates/tx-subsystems/src/wait_carrier.rs` (D — delete)
- `crates/tx-subsystems/src/wait_source.rs` (A — add, the renamed file)
- `crates/tx-subsystems/src/process/tests/exit_port.rs` (D)
- `crates/tx-subsystems/src/process/tests/exit_source.rs` (A)
- `crates/tx-subsystems/src/lib.rs` (just the `mod wait_source;` /
  `mod exit_source` rename if the file is small; otherwise defer to
  commit 4)

**Body credits:** STATUS lines 2316 (W-A.4), 2335 (W-A.3), 2300 (W-A.4
follow-up), 2357 (W-A.1).

### Commit 2 — `v3 PR-3A/3B/3C: wake-substrate foundation + object-owned publication`

**Files:**

- `crates/tx-substrate/src/wake/` (new dir — `mod.rs`, `mailbox.rs`,
  `timer.rs`, `wait_source.rs`)
- `crates/tx-substrate/src/lib.rs` (the `pub mod wake;` add)
- `crates/tx-reactor/src/mailbox.rs` (new)
- `crates/tx-reactor/src/wait_source.rs` (new)
- `crates/tx-reactor/src/lib.rs`

**Body credits:** STATUS lines 2242 (W-A 3-up PR-3A), 2224 (W-A PR-3B),
2198 (W-A PR-3C / PR-3D step 1), 1796 (W-D PR-3D-0 layering move).

### Commit 3 — `v3 PR-2 R1 cleanup + PR-5/PR-6: StepOp adapters wave 3 + R1`

**Files:**

- `crates/tx-subsystems/src/page_backed.rs` and `page_backed/*.rs` for
  the PR-2 adapter wraps
- `crates/tx-subsystems/src/tty/execution/step_*.rs` for the matching
  tty adapter wraps

**Body credits:** STATUS lines 2064 (PR-2 R1), 2080/2102/2121 (PR-2
waves), 2165 (PR-5/PR-6), 2140 (PR-8 admission half).

### Commit 4 — `v3 PR-7/7B/7C: OnAgent delegate runtime + mailbox + TimerWheel ADR (W-H)`

**Files:**

- `crates/tx-substrate/src/step_v3/agent.rs` (the 1033-LoC growth —
  `DelegateRegistry`, `mark_replied`, `await_agent_reply` seam, plus
  the `DelegateRequest` / `DelegateReply` sums)
- `crates/tx-substrate/src/step_v3/execution_scope.rs`
- `crates/tx-substrate/src/step_v3/mod.rs`
- `crates/tx-substrate/tests/v3_pr7_delegate_runtime.rs` (new)
- `crates/tx-substrate/tests/v3_pr7b_mailbox_integration.rs` (new)
- `crates/tx-reactor/tests/v3_pr7b_timer_routing.rs` (new)
- `crates/tx-reactor/tests/v3_timer_surface.rs` (new)
- `crates/tx-reactor/src/timer.rs` (PR-8 publish surface)
- `crates/tx-substrate/src/step_v3/page_progress.rs`
- `crates/tx-reactor/src/hart_loop.rs` (+207 LoC reactor pump for the
  new mailbox / agent-reply paths)
- `docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md` (new)

**Body credits:** STATUS lines 1738 (W-H PR-7), 1619 (W-H PR-7B), 1585
(W-H PR-7C ADR), 1865/1884 (PR-8), 1819 (W-A PR-9 phase 3b).

### Commit 5 — `v3 PR-9 phases 1–5: SubjectContext + Cap-shape reshape (W-A/W-E/W-I/W-J)`

**Files:**

- `crates/tx-substrate/src/step_v3/subject_context.rs` (+337 LoC)
- `crates/tx-substrate/tests/v3_subject_context.rs`
- `crates/tx-substrate/tests/v3_algebra.rs`
- `crates/tx-substrate/tests/v3_helpers.rs`
- `crates/tx-substrate/tests/v3_step_op.rs`
- `crates/tx-substrate/tests/v3_yield_on_agent.rs`
- `crates/tx-substrate/tests/v3_execution_scope.rs`
- `crates/tx-substrate/tests/bus.rs`
- `crates/tx-substrate/tests/v3_endpoint_scope_abandonment.rs` (new)
- `crates/tx-subsystems/src/cred.rs` (+598 LoC — Cred zone-allocation,
  PR-9 phase 5)
- `crates/tx-subsystems/src/cred/tests.rs`
- `crates/tx-subsystems/tests/v3_cred_zone_allocation.rs` (new)
- `crates/tx-subsystems/src/zones.rs`
- `docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md`
- `docs/progress/decisions/2026-05-11-d5-cred-zone-allocation.md`

**Body credits:** STATUS lines 1938 (PR-9 1+2), 1911 (3a), 1884 (3a
fanout), 1819 (W-A 3b), 1764 (W-E phase 4), 1709 (W-I phase 5 path
DECIDED), 1484 (W-J phase 5 LANDED).

### Commit 6 — `v3 PR-3D-1 pipe WaitSource migration (W-G)`

**Files:**

- `crates/tx-subsystems/src/pipe.rs`
- `crates/tx-subsystems/tests/v3_pipe_waitsource.rs` (new)
- `docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md`
- `docs/progress/decisions/2026-05-11-d2-waitsource-coexists-with-rawport.md`
- `docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`

**Body credits:** STATUS line 1679 (W-G PR-3D-1).

### Commit 7 — `v3 PR-3D-2 futex WaitSource migration (W-K)`

**Files:**

- `crates/tx-subsystems/src/futex.rs`
- `crates/tx-subsystems/tests/v3_futex_waitsource.rs` (new)

**Body credits:** STATUS line 1536 (PR-3D-2 LANDED).

### Commit 8 — `v3 PR-3D-3 exit_source WaitSource migration (W-M)`

**Files:**

- `crates/tx-subsystems/src/process/execution.rs`
- `crates/tx-subsystems/src/process/mod.rs`
- `crates/tx-subsystems/src/process/structure.rs`
- `crates/tx-subsystems/src/process/tests.rs`
- `crates/tx-subsystems/tests/v3_exit_wait_source.rs` (new)

**Body credits:** STATUS line 1339 (W-M PR-3D-3).

### Commit 9 — `v3 PR-3D-4 tty WaitSource migration (W-P)`

**Files:**

- `crates/tx-subsystems/src/tty/execution/*.rs` (the 8 step files in
  the diff)
- `crates/tx-subsystems/src/tty/structure/identity.rs`
- `crates/tx-subsystems/src/tty/tests/execution_poll_hardware.rs`
- `crates/tx-subsystems/tests/v3_tty_waitsource.rs` (new)

**Body credits:** STATUS line 1243 (W-P PR-3D-4).

### Commit 10 — `v3 PR-3D-5 vfs WaitSource migration + walker async carveout (W-S)`

**Files:**

- `crates/tx-subsystems/src/vfs/execution.rs`
- `crates/tx-subsystems/src/vfs/mod.rs`
- `crates/tx-subsystems/src/vfs/structure.rs`
- `crates/tx-subsystems/src/vfs/walker.rs`
- `crates/tx-subsystems/tests/v3_vfs_waitsource.rs` (new)
- `docs/progress/decisions/2026-05-11-d3-walker-async-carveout.md`
- `docs/progress/decisions/2026-05-11-pr-1-6-keep-fsops.md`

**Body credits:** STATUS line 961 (W-S PR-3D-5).

### Commit 11 — `v3 PR-10 phases 0–6: userfaultfd canary (W-O/W-Q/W-T/W-V/W-Y/W-BB/W-EE)`

**Files:**

- `crates/tx-subsystems/src/userfaultfd.rs` (new)
- `crates/tx-subsystems/src/vm/execution.rs`
- `crates/tx-subsystems/src/vm/mod.rs`
- `crates/tx-subsystems/src/vm/pmap.rs`
- `crates/tx-subsystems/src/vm/structure/address_space.rs`
- `crates/tx-subsystems/src/vm/structure/mod.rs`
- `crates/tx-subsystems/src/vm/structure/range_lock.rs`
- `crates/tx-subsystems/src/vm/structure/recipe.rs`
- `crates/tx-subsystems/src/vm/structure/types.rs`
- `crates/tx-subsystems/src/vm/user_access.rs`
- `crates/tx-subsystems/src/vm/tests/script_async.rs`
- `crates/tx-shims/src/linux_syscall/userfaultfd.rs` (new)
- `crates/tx-shims/src/linux_syscall/vm.rs`
- `crates/tx-shims/tests/v3_userfaultfd_syscall_scaffold.rs` (new)
- `crates/tx-shims/tests/v3_userfaultfd_register.rs` (new)
- `crates/tx-shims/tests/v3_userfaultfd_ioctl_reply.rs` (new)
- `crates/tx-subsystems/tests/v3_userfaultfd_fd_scaffold.rs` (new)
- `crates/tx-subsystems/tests/v3_userfaultfd_fault_path.rs` (new)
- `crates/tx-subsystems/tests/v3_userfaultfd_e2e.rs` (new)
- `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`

**Body credits:** STATUS lines 1457 (W-O D7), 1097 (W-Q phase 0), 1154
(W-T phases 1+2), 728 (W-V phase 3), 648 (W-Y phase 4), 490 (W-BB phase
5), 401 (W-EE phase 6).

### Commit 12 — `v3 PR-11 phases 0–7: AIO canary on OnBehalfOf<P> (W-W/W-Z/W-CC/W-FF/W-JJ/W-KK)`

**Files:**

- `crates/tx-substrate/src/step_v3/on_behalf_of.rs` (new — the
  framework primitive)
- `crates/tx-substrate/tests/v3_pr11_on_behalf_of.rs` (new)
- `crates/tx-substrate/tests/v3_agent_token_guard_timer.rs` (new)
- `crates/tx-subsystems/src/aio.rs` (new)
- `crates/tx-shims/src/linux_syscall/aio.rs` (new)
- `crates/tx-shims/tests/v3_aio_io_setup.rs` (new)
- `crates/tx-shims/tests/v3_aio_io_submit.rs` (new)
- `crates/tx-shims/tests/v3_aio_io_getevents.rs` (new)
- `crates/tx-shims/tests/v3_aio_io_destroy.rs` (new)
- `crates/tx-shims/tests/v3_aio_e2e.rs` (new)
- `crates/tx-shims/tests/v3_subject_population.rs` (new)
- `crates/tx-subsystems/src/page_backed/user_buffer.rs` (the W-KK
  `step_read_to_kernel` / `step_write_from_kernel` follow-up)
- `crates/tx-subsystems/src/page_backed/user_buffer_tests.rs`
- `crates/tx-subsystems/src/page_backed/cross_variant.rs`
- `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- `crates/tx-subsystems/src/page_backed/lifecycle_tests.rs`
- `crates/tx-subsystems/src/page_backed/targeted_read.rs`
- `crates/tx-subsystems/src/page_backed/core_tests.rs`
- `crates/tx-subsystems/tests/v3_openfile_page_backed_read.rs` (new)
- `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md`

**Body credits:** STATUS lines 931 (W-W phase 0), 885 (W-Z phase 1),
805 (W-CC phase 2), 319 (W-FF phases 3+4+5), 264 (W-JJ phases 6+7), 145
(W-KK follow-up).

### Commit 13 — `v3 PR-12 scaffold: io_uring SQPOLL on OnBehalfOf<P> (W-LL)`

**Files:**

- `crates/tx-subsystems/src/io_uring.rs` (new)
- `crates/tx-shims/src/linux_syscall/io_uring.rs` (new)
- `crates/tx-shims/tests/v3_io_uring_sqpoll_scaffold.rs` (new)

**Body credits:** STATUS line 77 (W-LL future PR-12 phase 0).

### Commit 14 — `v3 D9 phases A–D: signal subsystem migration + signalfd (W-AA/W-DD/W-II)`

**Files:**

- `crates/tx-subsystems/src/signal.rs`
- `crates/tx-subsystems/src/signal/tests/kill_permission.rs`
- `crates/tx-subsystems/src/signal/tests/tty_bridge.rs`
- `crates/tx-subsystems/src/thread_runtime/execution.rs`
- `crates/tx-subsystems/src/thread_runtime/structure.rs`
- `crates/tx-subsystems/src/signalfd.rs` (new)
- `crates/tx-shims/src/linux_syscall/signalfd.rs` (new)
- `crates/tx-subsystems/tests/v3_signal_eligibility.rs` (new)
- `crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs` (new)
- `crates/tx-subsystems/tests/v3_signal_mailbox.rs` (new)
- `crates/tx-subsystems/tests/v3_signalfd.rs` (new)
- `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`

**Body credits:** STATUS lines 5689 (W-X D9 ADR), 573 (W-DD phases
B+C), 196 (W-II D9-D signalfd). Note: W-AA's D9-A landed entry sits
between W-DD and W-X in STATUS.

### Commit 15 — `v3 shim surface: syscall numbers + dispatch + tests`

**Files (residual shim work not bundled above):**

- `crates/tx-shims/src/lib.rs` (+120 LoC)
- `crates/tx-shims/src/linux_syscall/mod.rs`
- `crates/tx-shims/src/linux_syscall/numbers.rs` (+258 LoC — new
  NR_USERFAULTFD, NR_IO_*, NR_SIGNALFD*, NR_IO_URING_*, flags)
- `crates/tx-shims/src/linux_syscall/fs_basic.rs`
- `crates/tx-shims/src/linux_syscall/io.rs` (the discriminator
  cascade for ufd / signalfd / aio / io_uring before generic VFS)
- `crates/tx-shims/src/linux_syscall/proc.rs`
- `crates/tx-shims/src/linux_syscall/tests.rs`
- `crates/tx-shims/src/linux_syscall/tests/fork_clone_wait4_wave3.rs`
- `crates/tx-subsystems/Cargo.toml` (`test-support` feature)
- `crates/tx-subsystems/src/lib.rs` (residual module rewires)
- `crates/tx-subsystems/src/execution.rs`
- `crates/tx-fs/src/tmpfs.rs`
- **`Cargo.lock`** — bundle here as the catch-all for any new dep
  resolution.

**Body credits:** dispatch-glue commits across W-Q, W-T, W-V, W-Y,
W-BB, W-Z, W-CC, W-FF, W-II, W-LL, W-KK. Cite the canonical "Files
touched" sections from STATUS lines 1097, 1154, 728, 648, 490, 885,
805, 319, 196, 77.

### Commit 16 — `v3 design-doc updates: txdoc anchors + Txv3 reactor docs`

**Files (the 36 doc files in `docs/design/**` + `docs/Txv3/**`):**

- `docs/design/00_meta-framework/*` (7 files)
- `docs/design/01_substrate/*` (6 files)
- `docs/design/02_execution/*` (4 files)
- `docs/design/03_memory-vm/*` (2 files)
- `docs/design/04_process-signals/*` (3 files)
- `docs/design/05_filesystem/*` (4 files)
- `docs/design/06_devices/*` (2 files)
- `docs/design/INDEX.md`
- `docs/Txv3/06_EXECUTION_SCOPE_v1.md`
- `docs/Txv3/07_BLAST_RADIUS.md`

**Body credits:** STATUS line 2262 (W-A design-doc sweep), and the
in-line `06_EXECUTION_SCOPE` / `07_BLAST_RADIUS` updates from W-JJ,
W-W, W-T (PR-10 / PR-11 landed-marks). Doc-only — no test gate.

### Commit 17 — `v3 progress: STATUS catchup + ADRs D10/D11/D12/D14 + audit + plans`

**Why last:** every worker appended to STATUS. Splitting STATUS across
commits 6–16 would require interactive editing of the diff; cheaper to
treat it as a docs-tail commit. Also lands the planning artifacts and
this ADR's companion files.

**Files:**

- `docs/progress/STATUS.md` (the entire +2 397-line growth)
- `docs/progress/migration-completion-audit-2026-05-12.md` (new — W-GG)
- `docs/progress/plans/2026-05-11-v3-migration-remaining.md` (new)
- `docs/progress/decisions/2026-05-12-d10-vocabulary-retire-audit.md`
  (new — W-NN)
- `docs/progress/decisions/2026-05-12-d11-d2-coexistence-retire-plan.md`
  (new — W-OO)
- `docs/progress/decisions/2026-05-12-d12-dead-code-todo-audit.md`
  (new — W-PP)
- `docs/progress/decisions/2026-05-12-d14-stale-commit-cleanup-plan.md`
  (this file — W-RR)
- `docs/progress/2026-05-12-commit-groupings-draft.md` (helper recipe;
  optional, may delete pre-commit)

**Body credits:** W-A through W-PP + W-RR. STATUS lines 1, 2, 3
through the entire 700-line top stretch.

## 4. The 21 prior `v3 unification phase N` commits — keep as-is

Per `CLAUDE.md`: "Prefer to create a new commit rather than amending."
Destructive history rewrites (`git reset --hard`, force-push,
interactive rebase) require explicit user authorisation.

**Recommendation: do not touch any commit reachable from `HEAD~1`.**

Rationale:

1. **They are already merged-equivalent.** `merge-base(HEAD, main) ==
   HEAD == a53d200`, so the `v3 unification phase 1..21` commits are
   reachable from `main`. Rewriting them on this branch would create a
   parallel history that must be force-pushed and reconciled with
   anyone else's checkout — high blast-radius, zero functional gain.

2. **They are already granular.** Each phase commit has a single
   focused goal (`retire v4 step_truncate`, `delete v4 FsOps`, etc.).
   Squashing them into one "v3 unification" commit loses the
   bisectability that makes phase-by-phase retirement valuable; this is
   the *opposite* of the granularity we are creating for the
   uncommitted work.

3. **They predate this work session.** They are baseline for the
   39-worker migration, not output of it. The migration's contribution
   sits atop them, not within them.

If the human operator later wants a tidier history for an external PR,
the right move is a **merge commit** that adds an aggregating message
(no history rewrite) — but that is outside this ADR.

## 5. Execution recipe

The exact step-by-step `git add` / `git commit` sequence lives in
`docs/progress/2026-05-12-commit-groupings-draft.md`. The high-level
order is:

```text
# Run before commit 1 (sanity baseline):
cargo check --workspace --tests  # expect clean (W-PP D12)

# Commits land in this order (each ADR-referenced):
1.  PR-A rename trail
2.  PR-3A/3B/3C/3D-0 wake-substrate foundation
3.  PR-2 R1 + PR-5/PR-6 StepOp adapters
4.  PR-7/7B/7C OnAgent delegate runtime
5.  PR-9 phases 1–5 SubjectContext + Cap-shape
6.  PR-3D-1 pipe WaitSource
7.  PR-3D-2 futex WaitSource
8.  PR-3D-3 exit_source WaitSource
9.  PR-3D-4 tty WaitSource
10. PR-3D-5 vfs WaitSource + walker carveout
11. PR-10 phases 0–6 userfaultfd canary
12. PR-11 phases 0–7 AIO canary on OnBehalfOf<P>
13. PR-12 scaffold io_uring SQPOLL
14. D9 phases A–D signal subsystem + signalfd
15. shim surface (syscall numbers + dispatch + Cargo.lock)
16. design-doc updates
17. progress (STATUS + ADRs D10/D11/D12/D14 + plans)

# Run before commit 17 (final sanity gate):
cargo check --workspace --tests
cargo xtask progress validate
cargo test --workspace -- --test-threads=1  # expect 1663/0/11 per W-LL STATUS
```

**Format note:** every commit message uses `git commit -m "$(cat
<<'EOF' … EOF)"` per CLAUDE.md style; bodies cite the STATUS line
number and the ADR file path. **Do not add yourself as coauthor** (per
CLAUDE.md rule).

## 6. Risks and safeguards

| Risk | Mitigation |
|------|------------|
| STATUS.md touched by every worker | Land in a single tail commit (#17); accept that STATUS is the worktree-wide narrative, not per-worker. |
| Cargo.lock changed | Bundle in commit 15 (shim surface) since dispatch-glue is the most likely dep introducer; if not, fold into commit 12. |
| Order-dependency between commits | The proposed order respects the build graph: foundation (1–4) → polymorphic context (5) → per-subsystem (6–10) → canaries (11–13) → cross-cutting D9 (14) → shim glue (15) → docs (16–17). Each commit-N should `cargo check --workspace --tests` clean if landed in this order. |
| Worker attribution lost in title | Workers credited in body; titles stay under 70 chars. |
| Pre-commit hook surprise | None installed (§2.4). |
| Stage drift between `git status` snapshots | Recipe uses explicit file paths per `git add`, not `-A` / `.`; matches CLAUDE.md "stage files by name". |
| Accidental commit of secrets | Working tree contains no `.env`, no credentials, no large binaries; full file list reviewed in §3. |
| Pre-existing compile error in tx-shims tests (per W-DD STATUS:635) | Already cleared by W-DD's noted resolution; if it re-surfaces, fix lives in commit 12 / 15 scope. |
| 21 prior phase commits attract rebase pressure | §4 documents the keep-as-is stance; future PR can land a merge commit instead. |

## 7. Estimated time

For a careful human operator:

- **Recipe-driven path** (no surprises): **45–75 minutes**.
  - ~3 min per commit × 17 = 51 min
  - +15 min for the two `cargo check` gates
  - +10 min for buffer / final `git log` review
- **First-time path** (rereading STATUS to double-check file assignments):
  **90–120 minutes**.
- **Fast path** (one big commit per layer, ignoring this ADR): **~20
  minutes** — but loses traceability and is not recommended.

## 8. Verification (this ADR)

- [x] No `git add` / `git commit` / `git rebase` / `git reset` / `git
      push` invoked.
- [x] Only read-only `git status`, `git diff --stat`, `git log
      --oneline`, `git merge-base` consulted.
- [x] STATUS.md catchup appended in companion commit (see §2 of this
      ADR's STATUS entry).
- [x] Recipe file `docs/progress/2026-05-12-commit-groupings-draft.md`
      exists with the exact `git add` sequence.
- [x] Companion ADRs D10, D11, D12 cross-referenced.

## 9. Open questions / follow-ups

- **Q1:** Should commits 6–10 (PR-3D-1..5) be squashed into one
  `v3 PR-3D: WaitSource bus migration (W-G/W-K/W-M/W-P/W-S)` commit?
  - **Answer:** No. Each migration has its own integration test and
    failure mode. Keeping them separate preserves bisectability across
    the five subsystems (pipe / futex / exit_source / tty / vfs).
- **Q2:** Should `Cargo.lock` ride with the first commit that adds a
  dep, rather than commit 15?
  - **Answer:** Inspect `git diff Cargo.lock` before landing commit 2;
    if the lock delta is purely transitive from new internal modules
    (no new external crate), commit 15 is fine. If it adds an external
    crate (unlikely for this migration), bundle with the first commit
    that imports it.
- **Q3:** After landing commits 1–17, should we open a PR or stay
  branch-local?
  - **Answer:** Out of scope for this ADR; defer to operator
    preference.

---

**End of D14.**
