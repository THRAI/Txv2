---
name: tx-ltp-syscall
description: >-
  Use when planning, prioritizing, or implementing Linux syscalls for LTP and
  OSComp coverage in txKernel, including ENOSYS reports, failing or skipping
  LTP/OSComp cases, syscall status questions, and completion bookkeeping. LTP
  and OSComp are the gold-standard correctness bar; a syscall is done when the
  relevant tests pass, not when only unit tests pass. Pair with
  tx-syscall-dispatch, tx-step-migration, tx-shell-syscall-fixup, and
  tx-network-test-fixup for socket/network witnesses.
---

# tx-ltp-syscall

Use this skill to do the **planning, prioritization, and bookkeeping** around
Linux syscall implementation for **OSComp + LTP coverage** in txKernel. The
actual implementation mechanics live in three sibling skills (named in
cross-skill pointers below); this skill is what tells an agent *which*
syscall to work on, where the design contract is, what counts as correct,
and how to keep the project's syscall progress record honest when work
lands.

## Gold standard: OSComp + LTP

**A syscall is not "done" because `cargo xtask unit` passes.** It is done
because the relevant **OSComp** test (under `external/oscomp-autotest/`, run
via `cargo xtask oscomp run --target rv64-qemu`) and/or the relevant **LTP**
test (Linux Test Project, run inside the same QEMU image) moves from failed
or skipped to passing — and the project's `SYSCALL_STATUS.md` records that
state change. Everything else — host unit tests, busybox boot, hand-rolled
fixtures — is a sanity check that exists *because* OSComp/LTP is slow and
the inner loop needs to be tight. They are not substitutes for the gold
standard.

Why these two specifically:

- **OSComp** (`external/oscomp-autotest`, `xtask/src/oscomp.rs`) is the
  contest harness this kernel is built against; passing tests are the
  competitive scoreboard and the closest thing the project has to "does it
  work in the real world." `cargo xtask oscomp run` is the canonical
  end-to-end verifier.
- **LTP** is the de facto upstream-kernel correctness suite. Where OSComp
  measures "does the syscall work in a realistic program," LTP measures
  "does the syscall match Linux ABI semantics in every documented edge
  case." Where they disagree (rare), prefer LTP for ABI questions and
  OSComp for "does the test we ship actually finish."

**Practical rule.** Before declaring a syscall complete, name the specific
LTP and/or OSComp test that exercises it and verify that test passes (or,
if the test isn't currently runnable for unrelated reasons, name the
substitute coverage and *why* LTP/OSComp can't run yet — that is itself a
high-priority follow-up). "Manually tested with a shell" is not the bar.

## Test-Driven Fix Integrity

LTP and OSComp tests are witnesses for Linux ABI semantics, not targets to
special-case.

- Do **not** hardcode a test name, binary path, argv pattern, magic input
  buffer, fixed benchmark label, or one-off errno sequence to make a case pass.
  Implement the Linux behavior the test exposes.
- A fixed port, path, or payload in a test harness is evidence only. Kernel
  behavior must remain general across callers.
- If an unsupported Linux surface is intentionally out of scope, return the
  principled Linux-style error and record the blocker instead of faking success.
- If a simple local patch cannot solve the failure without bending the model,
  stop and discuss a refactor plan with the user before making broad changes.
  Name the failing test, the semantic gap, affected modules, risk, and the
  verification set.
- Keep the scope honest: when a network LTP case fails because of a non-network
  prerequisite, record that prerequisite rather than expanding the network task
  silently.

## Read First

- `docs/progress/SYSCALL_STATUS.md` — **the living syscall map.** Headline
  counts, the high-stakes prioritization table, the unwired list by topic,
  and the "already-partial" stub list. Anything you commit must keep this
  file accurate.
- `docs/Txv3/04_SYSCALL_SHAPE_v1.md` — canonical upper/lower split
  discipline and `SubjectContext` threading. Read this *before* writing a
  new dispatch arm.
- `docs/Txv3/03_STEP_MODEL_v2.md` — four-variant `StepOutcome`, the typed
  `StepOp` trait, and the anti-pattern catalog (A-1..A-15). Every syscall
  that mutates state routes through here.
- `docs/Txv3/02_INVARIANTS_v5.md` — SUBJ-*, SCRIPT-V5-*, STEP-*, YIELD-*.
  Cite these when a review asks why the syscall did or did not yield.
- `docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md` — the
  canonical phasing source. New syscalls inherit its discipline
  (`pending_syscall_return` writeback, `Cap` vs `IdentRef` guard rules,
  trap-frame ownership).
- The subsystem doc the syscall touches: pick from `docs/design/` (e.g.
  `04_process-signals/PROCESS_v1.md` for `fork`/`setpgid`/`getpid` work,
  `05_filesystem/VFS_CHECKS_V2.1.md` for path-walk syscalls,
  `03_memory-vm/VM_v1_2.md` for `mmap`/`brk`/`mprotect`).
- For socket/network LTP cases, read
  `docs/progress/research/2026-05-21-ltp-network-prep.md` and load
  `tx-network-test-fixup`.

The first three are non-negotiable. Skip the rest only if you have the
information already and can name where it came from.

## Workflow

The skill's job is to keep the loop tight: don't re-discover the priority
list, don't reinvent dispatch boilerplate, don't ship without updating the
status doc.

### 1. Identify the target

Before reading docs, get the mechanical state straight from the SSOT:

```sh
cargo xtask syscall-status                 # headline + detail-command menu
cargo xtask syscall-status <NAME>          # e.g. `sendfile`, `fork` — is it
                                           # numbered? dispatched? next step?
cargo xtask syscall-status --list-missing  # everything with NR_* but no arm
```

`cargo xtask syscall-status` reads `numbers.rs` + `mod.rs` directly, so its
answers are always current — no doc-drift risk. The autogen "Mechanical
status" section near the top of `SYSCALL_STATUS.md` is the same data
rendered for the doc; `cargo xtask syscall-status --check` is the CI gate
that keeps it honest.

A syscall task usually arrives as one of three shapes:

- **Specific LTP/OSComp test failure** ("`fcntl04` is hitting `-ENOSYS`").
  Trace the test back to the syscall it exercises; if you can't tell
  which `NR_*` is at fault, run the failing test under `cargo xtask
  shell-test` / `cargo xtask oscomp run` and grep the trace for `nr=`.
  The `tx-shell-syscall-fixup` skill is the canonical loop for this.
- **A specific syscall by name** ("implement `sendfile`"). Run
  `cargo xtask syscall-status sendfile` first; it'll tell you whether
  the syscall is numbered, dispatched, or missing, and point at the
  right next-step skill.
- **"Where should I focus?"** Open `SYSCALL_STATUS.md` and read the
  high-stakes table (priority is human-curated; the mechanical state
  beside it is autogenerated). Default recommendation: best LTP impact
  ÷ effort (today: `fork` + CLOEXEC, then `getrlimit`/`setrlimit`, then
  `preadv`/`pwritev`/`fallocate`). Surface the "out of scope v1" rows
  (`ptrace`, `bpf`, `perf_event_open`) only when the user has chartered
  them — otherwise warn that they are flagged out of scope.

### 2. Read the design contract

For every new (or substantially-changed) syscall, the upper/lower split in
`SYSCALL_SHAPE_v1.md` decides which lane it belongs in and what
`SubjectContext` it threads. The subsystem doc decides what entity it acts
on. Read both before writing code; this is where the cheap mistakes are
caught.

If the syscall touches a subsystem whose v4 doc hasn't been refreshed against
v5, prefer the v5 vocabulary in any new prose and cite the v4 anchor only
when restating a row v5 carries forward unchanged. See `tx-meta-alignment`
for the v4/v5 supersession map.

### 3. Classify the dispatch lane

Three lanes: `ImmediateSyscall`, `OneShotStepOp`, full async `drive`. The
`tx-syscall-dispatch` skill has the full classification table and the
migration patterns A/B/C. Don't try to redo that judgment in this skill —
load it.

**Cheap rule of thumb:** pure ABI query → Immediate. Mutation that never
yields → `OneShotStepOp`. Anything that can block on VFS/VM/timer/wait →
full async. When in doubt, read SCRIPT-V5-4/5 and STEP-11/12 in
`INVARIANTS_v5`.

### 4. Implement

Implementation skills do the heavy lifting:

- `tx-syscall-dispatch` — restructuring the dispatch table, adding lane
  markers, adapter re-exports.
- `tx-step-migration` — wrapping a `step_*` free function into a typed
  `StepOp` impl that the dispatcher can drive.
- `tx-shell-syscall-fixup` — the observe→fix→verify loop with
  `trap-trace` + `fault-decode` + `shell-test` when the syscall already
  routes but produces the wrong result.
- `tx-subsystem-manifest` — when a new syscall introduces or consumes a
  reclaimable semantic entity that needs a zone-derived type policy.

The skill body deliberately does not duplicate those mechanics; bouncing
between skills is cheaper than maintaining the same migration pattern in
two places.

### 5. Report high-stakes directions

When the user asks for a recommendation rather than a specific syscall,
respond with a short, structured report:

```text
Recommended next: <syscall or family>
LTP impact: <table row's "+N tests">
Effort: <S/M/L/XL with rationale>
Substrate status: <from the table; cite the prerequisite>
Path: <dispatch lane, target subsystem doc, sibling skill to load>
Risks: <e.g. "needs page-cache coherence path">
Out of scope right now: <if applicable, e.g. ptrace, bpf>
```

Keep the report tight — agents reading this are usually about to decide
whether to start the work, not to read a roadmap. If multiple gaps are
roughly equivalent, list the top 3 and let the user pick.

### 6. Update `SYSCALL_STATUS.md` on completion

This is the bookkeeping discipline this skill exists to enforce. Before you
declare any syscall task complete:

1. **Refresh the autogen section.** Run `cargo xtask syscall-status --regen`.
   This rewrites headline counts and the "defined but not dispatched" table
   directly from `numbers.rs` + `mod.rs` — no eyeballing. CI rejects a
   stale section via `cargo xtask syscall-status --check`.
2. If your change closed a stubbed arm, remove the entry from the
   human-curated **Already-partial** list (the lint can't detect stub
   bodies reliably — this stays manual).
3. If your change wired a previously-unwired syscall, remove it from its
   **Unwired by topic** section and decrement the heading count. (The
   autogen total is mechanical; the curated per-topic breakdown is not.)
4. If your change shifted the high-stakes table — a row landed, an effort
   estimate changed, a substrate prerequisite cleared — update the row or
   delete it.
5. **Record the OSComp/LTP test(s) that newly pass** under the matching
   subsection of "OSComp + LTP coverage". This is the gold-standard
   correctness bar — see the section at the top of this skill.
6. Update **Last refresh** at the top.
7. Add a one-line entry to `docs/progress/STATUS.md` pointing at the
   commit and `SYSCALL_STATUS.md` (per the project's catch-up rule).

`SYSCALL_STATUS.md` is the *only* doc that aggregates this state. Letting
it drift is exactly the kind of project-memory loss that the
`tx-progress-memory` skill exists to prevent — see that skill for the
broader catch-up shape.

## Verification

Verification is two stages: a tight inner loop (host-side, fast) and the
gold-standard outer loop (OSComp + LTP in QEMU). Skipping the outer loop
means the work isn't done — see the "Gold standard" section.

### Inner loop (run during development)

```sh
cargo -q xtask unit                       # build + host unit tests
cargo xtask lint invariants syscall-adhoc-loop
cargo xtask lint invariants syscall-no-await
cargo xtask lint invariants syscall-ctx-bridge
cargo xtask ci                            # full fast gate (incl. all lints)
```

This is what you run between code edits. Green here means "we didn't break
the build or invariants" — it does **not** mean the syscall works.

### Gold-standard outer loop (must run before declaring done)

```sh
cargo xtask test busybox-boot             # full kernel + QEMU smoke sentinel
cargo xtask oscomp run --target rv64-qemu # OSComp harness — the contest scoreboard
```

For LTP specifically, the canonical path is to run the relevant LTP binary
inside the QEMU image (`cargo xtask qemu --target rv64-qemu` then invoke
the LTP binary from the busybox shell — or whatever LTP entry point the
project's image carries). Whichever LTP execution mode is current, the
shape is the same: pick the specific test name that exercises the syscall
you changed (`fork01`, `getrlimit02`, `sendfile02`, …) and confirm it
passes.

Current local shortcuts:

```sh
make oscomp-local-rv64-ltp-musl OSCOMP_LTP=<case>
make oscomp-local-rv64-ltp-musl OSCOMP_LTP=<case1>,<case2>
make oscomp-local-rv64-ltp-batch LTP_BATCH=<batch>
make oscomp-local-rv64-ltp-batch LTP_BATCH=submit
python3 tools/ltp-batches.py --batch <batch>
python3 tools/ltp-batches.py --list
```

`LTP_BATCH=submit` is the local mirror of the no-`tx.oscomp.groups`
submission path. Its whitelist is maintained in
`crates/tx-kernel/src/init/exec.rs::LTP_SUBMIT_CASES` from cases with
nonzero score in `docs/LTP/syscalls/ltp-*-progress.md`. Do not use the
`p0` summary as a whitelist source because it intentionally overlaps the
real module batches. Partial-score cases still belong in the submit
whitelist when they add points; keep known RV-positive cases such as
`futex_wake03` and `setitimer01` even if LA64 is being aligned later.

Known local skips live in `tools/ltp-batches.py::SKIP_CASES` and
`crates/tx-kernel/src/init/exec.rs::LOCAL_LTP_SKIP_SHELL_PATTERN`. These are only for
cases that currently wedge the guest or never reach the normal LTP
summary/guest-exit path. Local command generation skips them for direct
`OSCOMP_LTP=...`, full `ltp-musl`, native runtest loops, and
`tools/ltp-batches.py` batch lists. Do not re-run a skipped case unless the
user explicitly asks to reopen that specific bug.

**The completion report must name at least one LTP and/or OSComp test
that now passes because of this change.** If LTP/OSComp can't currently
run for the affected syscall (e.g. test environment missing, dependency
unimplemented), that itself is a high-priority follow-up — record it in
`SYSCALL_STATUS.md` and STATUS.md instead of silently shipping with weaker
coverage.

### Decoding failures

For trap or fault output from QEMU runs, use
`cargo xtask fault-decode --target rv64-qemu --serial <log>` before
hand-decoding `scause`/`sepc`/`stval`. See `tx-ci-triage` if any of the CI
gates fail in a way you don't recognize, and `tx-shell-syscall-fixup` for
the observe→fix→verify loop when a syscall is wired but produces the
wrong result inside an LTP/OSComp run.

## What Not To Do

- **Don't hardcode for LTP/OSComp.** No branches on testcase names,
  executable names, argv shapes, magic benchmark strings, or exact payloads.
  Fix the semantic behavior or record a principled unsupported surface.
- **Don't add a new `NR_*` without checking `numbers.rs` for an existing
  entry.** Linux RV64 numbers come from `asm-generic/unistd.h`; a
  duplicate definition will silently shadow the real one.
- **Don't return `-ENOSYS` from a *new* dispatch arm without leaving the
  syscall on the Already-partial list.** Silent stubs that aren't tracked
  in `SYSCALL_STATUS.md` are how regressions hide.
- **Don't pick syscalls from the "out of scope v1" rows** (`ptrace`,
  `bpf`, `perf_event_open`, full network stack) without an explicit
  charter — the effort cost dwarfs the LTP impact and the substrate
  isn't ready.
- **Don't ship without updating `SYSCALL_STATUS.md`.** A green build with
  a stale status doc is worse than a red build, because future agents
  pick targets from the doc.
- **Don't duplicate dispatch-lane reasoning in your prose.** When the
  task touches lane classification, defer to `tx-syscall-dispatch` and
  cite SCRIPT-V5-4/5 / STEP-11/12 rather than restating the rule.

## Cross-Skill Pointers

- Lane classification & dispatch-table edits → `tx-syscall-dispatch`.
- Wrapping `step_*` into typed `StepOp` → `tx-step-migration`.
- Observe→fix→verify loop on a failing shell/LTP run →
  `tx-shell-syscall-fixup`.
- New subsystem surface introduced by the syscall →
  `tx-subsystem-manifest`.
- Socket/network syscall failure or OSComp network benchmark →
  `tx-network-test-fixup`.
- CI gate failure encountered along the way → `tx-ci-triage`.
- Recording the progress catch-up → `tx-progress-memory`.

## Done Means

- The target syscall(s) compile, route, and produce the expected ABI
  result against **at least one specific OSComp test and/or LTP test**,
  named in the completion report. (If LTP/OSComp cannot currently run
  against the syscall, name the blocker and add it as a high-priority
  follow-up — do not silently substitute weaker coverage.)
- The fix is semantic, not test-specific; any required broader refactor was
  discussed with the user before implementation.
- `cargo xtask ci` is green; the syscall-family invariants ratchets did
  not regress.
- `cargo xtask oscomp run --target rv64-qemu` covers the test that
  closed the work (or `cargo xtask test busybox-boot` plus the explicit
  LTP invocation that exercises it).
- `docs/progress/SYSCALL_STATUS.md` reflects the new state: headline
  counts recounted via the file's own `grep` commands, topic-section
  moves done, high-stakes table updated, OSComp/LTP coverage section
  updated to name the test(s) that newly pass, **Last refresh** bumped.
- `docs/progress/STATUS.md` has a dated entry pointing at the commit and
  `SYSCALL_STATUS.md`, naming the LTP/OSComp test(s) that passed.
- The final response can name the specific LTP/OSComp test that closed
  the work and the file path of the updated status record.
