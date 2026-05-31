---
name: tx-test-runtime-optimizer
description: Use when txKernel tests, LTP/OSComp/QEMU runs, cargo tests, or benchmark witnesses are slow, timing out, silently hanging, taking much longer than expected, or when the user asks whether a timeout is caused by kernel code, network/filesystem/process structure, harness setup, cold reads, linear scans, or other performance bottlenecks. Guides Codex to bound the run, preserve logs, measure where time is spent, classify the bottleneck, optimize only evidence-backed general behavior, and record the result.
---

# tx-test-runtime-optimizer

Use this skill when test runtime itself is part of the problem. Pair it with
`tx-ltp-timeout-ladder` for LTP/QEMU timeout shape, `tx-xtask` for tooling, and
`tx-debug-logbook` after a conclusion.

## Core Rules

- Treat "make it pass by waiting longer" as diagnostic only, not a fix.
- Do not assume the subsystem under test is the bottleneck. Measure first.
- Preserve the exact command, wall timeout, serial/log path, last progress line,
  and whether the test timed out internally or by host `timeout`.
- Prefer focused witnesses over full suites while diagnosing runtime.
- Do not add test-name, argv, fixed-payload, or score-only shortcuts.
- Optimize general behavior: cache repeated immutable work, reduce repeated
  helper/procfs/sysfs probes, fix measured linear scans, or improve process/fd
  paths when evidence points there.
- If a trace build is used, restore the ordinary non-trace build and submit
  artifact before finishing.

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
