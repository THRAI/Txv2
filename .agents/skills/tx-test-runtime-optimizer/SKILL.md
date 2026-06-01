---
name: tx-test-runtime-optimizer
description: Use when txKernel tests, LTP/OSComp/QEMU runs, cargo tests, or benchmark witnesses are slow, timing out, silently hanging, taking much longer than expected, or when the user asks whether a timeout is caused by kernel code, network/filesystem/process structure, harness setup, cold reads, linear scans, userspace helper churn, or other performance bottlenecks. Also use when resuming prior timeout debugging with existing logs or when the user is worried Codex is rerunning low-value experiments. Guides Codex to read prior evidence first, bound the run, preserve logs, measure where time is spent, classify the bottleneck, optimize only evidence-backed general behavior, commit useful work before risky experiments, and record the result.
---

# tx-test-runtime-optimizer

Use this skill when test runtime itself is part of the problem. Pair it with
`tx-ltp-timeout-ladder` for LTP/QEMU timeout shape, `tx-xtask` for tooling, and
`tx-debug-logbook` after a conclusion.

## Core Rules

- Treat "make it pass by waiting longer" as diagnostic only, not a fix.
- Do not assume the subsystem under test is the bottleneck. Measure first.
- When resuming, read user-named debug logs and progress notes before any new
  experiment. Carry forward accepted and rejected hypotheses explicitly.
- Preserve the exact command, wall timeout, serial/log path, last progress line,
  and whether the test timed out internally or by host `timeout`.
- Prefer focused witnesses over full suites while diagnosing runtime.
- Do not add test-name, argv, fixed-payload, or score-only shortcuts.
- Optimize general behavior: cache repeated immutable work, reduce repeated
  helper/procfs/sysfs probes, fix measured linear scans, or improve process/fd
  paths when evidence points there.
- Commit or otherwise isolate useful evidence-backed changes before starting a
  broader experiment. Stage explicit paths only; do not include local `msp/`
  debug notes unless the user asks.
- If a trace build is used, restore the ordinary non-trace build and submit
  artifact before finishing.

## Resume Protocol

Before running or editing anything in an existing timeout investigation:

1. Check `git status --short` and identify unrelated dirty files.
2. Read every log or progress file named by the user, especially
   `msp/debug-logs/*.md`; these are local memory and usually not committed.
3. Extract four lines into the working update:
   - latest witness command/log and whether it passed or timed out
   - last accepted bottleneck
   - hypotheses already rejected
   - next measurement that can change the decision
4. If the next action would revisit a rejected hypothesis, stop and explain what
   new evidence would justify reopening it.

## Stop Rules

- Do not run more than one new trace for the same hypothesis without making a
  keep/revert/next-target decision from the numbers.
- If a change improves one command but the case still times out, compare the
  full phase budget before continuing. Do not keep optimizing the same command
  after another phase dominates.
- If a profile shows userland control-plane churn (`execve`, `clone`, `wait4`,
  `pipe2`, `dup3`, `fcntl`, short `read`/`ppoll`) dominates, do not refactor the
  kernel datapath unless a fresh trace contradicts it.
- Treat no-fork or standalone userspace experiments as oracles unless they are
  acceptable product direction. Do not present them as the fix after the user
  rejects that path.
- When the user asks for a pass/fail answer, answer with the latest witness
  result before proposing more work.

## Runtime Triage

1. Identify the slow witness:
   - command and log path
   - expected runtime or timeout
   - first slow phase or last visible progress line
   - internal test timeout vs host `timeout`
   - previous faster/slower comparison log if available
2. Bound the next run:
   - Use the shortest timeout that can reach the phase being studied.
   - Name the output log by scope and hypothesis.
   - Check for leftover QEMU or timeout processes before and after long runs.
3. Split correctness from runtime:
   - If there is an errno, `TFAIL`, `TBROK`, panic, or trap, fix semantics first.
   - If the test reaches the expected loop/body and only times out, switch to
     runtime profiling.
4. Classify the owner:
   - datapath/control plane: socket, route, ARP, netlink, protocol, packet flow
   - process/fd churn: `execve`, `clone`, `wait4`, `close`, `dup`, `fcntl`
   - filesystem/image: cold ext4 reads, repeated binary/script opens, metadata
   - waiting/timers: `ppoll`, `nanosleep`, timeout arithmetic, wakeups
   - harness/userland: BusyBox applet grammar, shell pipelines, LTP helpers
   - unsupported surface: protocol, driver, module, or external dependency
5. Optimize only after classification. A good fix should reduce a measured hot
   path and preserve existing witnesses.

## Evidence Ladder

Use the smallest evidence that can decide the next step:

- Plain focused witness: answers "does it pass now?"
- Trace window: answers "which phase and command owns the remaining budget?"
- Counter summary: answers "is the cost kernel datapath or userspace control
  plane?"
- Oracle experiment: answers "would removing this class of work be enough?"
  Mark it as an oracle and keep it out of the submitted fix unless accepted.

Compare only like with like:

- same phase (`setup`, `stress`, `post-stress`, or `all`)
- same trace mode when using `total_s`
- same command labels and loop count
- both wall progress and counters (`syscalls`, `faults`, `clone`, `execve`,
  `wait4`, `pipe2`, `ppoll`, `read<=1`)

For LTP shell/network timeouts, split setup from the loop body. A setup chain
such as `tst_ns_exec ... sh -c "... || echo RTERR"` is not the same owner as a
post-stress `ip neigh show | grep` pipeline. Fix or measure the phase that
actually owns the remaining wall budget.

## Useful Commands

Find the active case and progress markers:

```sh
rg -n "RUN LTP CASE|TINFO|TFAIL|TBROK|Test timed out|PASS LTP CASE|FAIL LTP CASE" <log>
tail -n 120 <log>
pgrep -af "qemu-system|make oscomp-qemu|timeout"
```

Score a completed OSComp/LTP log:

```sh
python3 tools/oscomp-judge.py <log> target/oscomp/testdata
```

Build and run a short RV64 trap-trace probe:

```sh
cargo build -p tx-kernel-riscv64-qemu-virt \
  --target riscv64gc-unknown-none-elf --features trap-trace
make oscomp-submit-rv64
timeout 240s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=<focused-group> \
  OSCOMP_OUT_RV=target/oscomp/<scope>-traptrace-240s.txt
cargo xtask trap-trace \
  --serial target/oscomp/<scope>-traptrace-240s.txt \
  --syscalls > target/oscomp/<scope>-traptrace-240s.syscalls.txt
```

Count syscall names from parsed trace output:

```sh
awk '/ SY / {count[$4]++} END {for (name in count) print count[name], name}' \
  target/oscomp/<scope>-traptrace-240s.syscalls.txt | sort -nr | head -40
```

Split a raw serial trace around a visible marker:

```sh
rg -n "stress|timeout per run|RUN LTP CASE|Network config" <log>
awk 'NR < MARKER_LINE && /kind=SY/ {n++} END {print n}' <log>
awk 'NR >= MARKER_LINE && /kind=SY/ {n++} END {print n}' <log>
```

After trace work, restore the normal artifact:

```sh
cargo xtask build --target rv64-qemu
make oscomp-submit-rv64
```

## Optimization Patterns

- **Repeated applet/script execs:** cache immutable binaries in tmpfs, prefer
  already-mounted helper paths, or reduce shell pipeline stages if semantics
  stay Linux-compatible.
- **Close/fd storms:** inspect close-on-exec, fd-table iteration, inherited fd
  ranges, and whether the implementation scans beyond the highest live fd.
- **Repeated proc/sysfs reads:** cache stable projection data only when Linux
  visibility semantics allow it; invalidate on namespace/link/mount changes.
- **Cold filesystem reads:** distinguish first-run image cost from repeated
  per-iteration cost; prefer cache/page-cache fixes over test-specific copies.
- **Wait-heavy traces:** check wakeup publication, poll readiness, timeout
  conversion, and whether helpers spin through short sleeps.
- **Network-looking slow tests:** compare socket syscall counts to process/file
  counts before refactoring socket or protocol tables.

## Reporting And Recording

Before finishing, state:

- what was slow and where it timed out
- whether semantics are clean or still failing
- measured hot path, not just a guess
- fix attempted and whether runtime changed
- next optimization target
- tests/logs used for verification

Update `docs/progress/STATUS.md` for stable conclusions. For long debugging,
write a detailed local log under `msp/debug-logs/` and do not commit `msp/`.
Run `cargo xtask progress validate` after progress edits.
