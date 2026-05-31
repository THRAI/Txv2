# libcbench Throughput Suspect Sweep

Date: 2026-05-29

Scope: current `rv64-qemu` libcbench throughput after the reactor scheduling
update. This note is a suspect register, not a fix plan. It records what the
carpet sweep found across guest evidence, benchmark source, musl pthread source,
syscall dispatch, process/thread lifecycle, futex, VM, and reactor scheduling.

## Current Symptom

The latest full bounded libcbench run now completes instead of timing out. The
saved serial is `target/oscomp/custom-run/os_serial_out_rv.txt`; scoring with
`python3 tools/oscomp-judge.py target/oscomp/custom-run/os_serial_out_rv.txt
target/oscomp/custom-run/testdata` reports `27.88892195361034/27`.
`cargo xtask fault-decode --target rv64-qemu --serial
target/oscomp/custom-run/os_serial_out_rv.txt --all --brief` finds no
`scause`/`sepc`/`stval` trap lines.

Important completed timings from the same serial:

- `b_malloc_sparse`: `15.105796000s`
- `b_malloc_bubble`: `13.201719000s`
- `b_malloc_big1`: `14.249863000s`
- `b_malloc_big2`: `15.109048000s`
- `b_pthread_createjoin_serial1`: `34.854431000s`
- `b_pthread_createjoin_serial2`: `31.220515000s`
- `b_pthread_create_serial1`: `31.706098000s`
- `b_pthread_uselesslock`: `0.144279000s`
- `b_pthread_createjoin_minimal1`: `35.465912000s`
- `b_pthread_createjoin_minimal2`: `24.551947000s`
- `b_stdio_putcgetc`: `27.272210000s`
- `b_stdio_putcgetc_unlocked`: `28.044669000s`
- `b_regex_compile`: `10.551844000s`

The serial log reports a single QEMU hart and no trap lines. Child-submit
counters advance in lockstep with direct submission and terminal drain, so this
run does not look like a lost-child/process-roster bug. Pthread lifecycle is no
longer a suite-completion blocker, but it remains far above reference scale;
malloc, stdio, and regex are now the largest remaining libcbench throughput
families.

## Benchmark and libc Shape

`external/libc-bench/pthread.c:12-95` shows the pthread tests are dense thread
lifecycle loops:

- `serial1`: 2500 create/join pairs.
- `serial2`: 50 batches of 50 creates, then 50 joins.
- `create_serial1`: 2500 creates with 16 KiB stacks.
- `minimal1` and `minimal2`: page-sized stacks and zero guard pages.

`external/musl/src/thread/pthread_create.c:237-390` shows one musl create path
does all of this in sequence: choose stack/guard size, `mmap` stack/TLS memory,
initialize the pthread object, block signals, take the thread-list lock, call
`clone(CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_SETTLS|CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID|CLONE_DETACHED)`,
link the thread, unlock, and restore signals.

`external/musl/src/thread/pthread_join.c:10-25` shows join primarily waits on
`t->detach_state` and then `munmap`s the thread stack. `clear_child_tid` is a
separate lifecycle wake path, not the main pthread join address.

`external/libc-bench/malloc.c:6-79` shows the slow malloc tests are allocation,
write, and free storms. Those stress page fault/materialization and VM recipe
mutation even before pthread lifecycle begins.

## Trace Evidence

Latest focused wake-handoff/no-yield trace:
`target/oscomp/custom-run/libcbench-minimal2-wakehandoff-no-yield.analysis.txt`

- Window: `603306us`, `8192` records.
- `sys_futex`: `n=51`, total `190638us`, avg `3738us`, max `19816us`.
- `drive.FutexWaitOp`: `n=10`, avg `17065us`.
- `yield.OnWaitSource`: `n=10`, avg `16523us`.
- `sys_clone`: `n=33`, total `109979us`, avg `3333us`, max `49531us`.
- `sys_mmap`: `n=33`, total `58596us`, avg `1776us`, max `4652us`.
- `sys_rt_sigprocmask`: `n=136`, total `46775us`, max `15240us`.
- Futex source correlation is clean: registered `5`, notified `5`,
  matched `5`, no unregistered notifications.
- Futex wake latency initially looked unclean: `notify->drain` avg `39899us`,
  max `259300us`; `runnable->pick` avg several ms with many 6-10 ms delays.
  Later raw-record review showed the large `notify->drain` rows were analyzer
  pairing artifacts, not reactor drain stalls. Keep this trace as pre-fix
  historical evidence only; use the corrected 10x50 analysis below for current
  ranking.
- Scheduler counters mostly stay in `Preempted`: `pick:Preempted=79`,
  `runnable:Preempted=13`, `runnable:Boosted=1`; wake hints are
  `WakeHandoff=8 Normal=5 LifecycleWake=1`.
- Clone phase outlier: `payload_fresh.after -> payload_sign.after` max
  `47780us`, while the p50 is `89us`.

Earlier exact-tid-unregister focused trace:
`target/oscomp/custom-run/libcbench-minimal2-exact-tid-unregister-observe-1x50.analysis.txt`

- Window: `70301us`.
- `sys_clone`: `n=38`, avg `286us`, max `2477us`.
- `sys_mmap`: `n=37`, avg `164us`, max `903us`.
- `sys_rt_sigprocmask`: `n=155`, avg `38us`, max `726us`.
- `sys_exit`: `n=36`, avg `134us`, max `977us`.
- `sys_futex`: `n=37`, avg `48us`, max `766us`.

The two traces disagree by almost an order of magnitude. That is itself a
suspect: either current scheduling/timing changed after the exact-tid run, or
the latest trace captured a pathological timer/scheduler epoch. A throughput
fix should not be chosen from one trace without reproducing both regimes.

## Code-Path Sweep

### Syscall Dispatch

- `crates/tx-kernel/src/thread_future.rs:254-838` drives every userspace
  round trip through `start_request`, AST dispatch, context preparation,
  userspace entry, trap consumption, fast lanes, optional full `SyscallCtx`
  construction, syscall result storage, and optional clone handoff.
- `crates/tx-shims/src/linux_syscall/mod.rs:572-603` has fast lanes for
  `rt_sigprocmask` and private anonymous `mmap`.
- `crates/tx-shims/src/linux_syscall/mod.rs:611-644` has direct trap lanes for
  `getppid`, futex, `rt_sigprocmask`, and `set_tid_address`.
- `crates/tx-shims/src/linux_syscall/mod.rs:646-670` keeps `mmap`,
  `munmap`, and `mprotect` out of the generic async dispatcher, but still uses
  an async VM hot lane.

Suspect: even after fast lanes, pthread create/join still performs many full
kernel-user round trips. The current trace shows the entry-side markers
(`start_request`, AST, checkpoint, entry prepare, enter) on almost every `clone`
and `mmap`.

### Clone and Thread Publish

- `crates/tx-shims/src/linux_syscall/proc.rs:310-453` handles
  `CLONE_THREAD` as one-shot, snapshots the parent trap context, drives
  `CloneThreadOp`, writes parent tid when requested, and submits the child to
  the reactor.
- `crates/tx-subsystems/src/process/execution.rs:642-678` allocates a tid,
  signs `ThreadPayload`, signs `ThreadIdentity`, registers tid, seeds context,
  records `clear_child_tid`, attaches the child, and increments thread count.
- `crates/tx-subsystems/src/process/execution.rs:1301-1318` signs
  `ThreadPayload` and `ThreadIdentity`; this is where the latest trace's
  largest clone outlier lands.
- `crates/tx-kernel/src/init/reactor_submit.rs:168-245` drains terminal tasks,
  boxes and submits the child future, enqueues with `preempted_on_submit`, and
  registers the tid-to-task mapping.
- `crates/tx-reactor/src/task.rs:187-204` allocates/reuses a task table slot,
  boxes the future, allocates wake state, allocates a mailbox, constructs the
  task, and stores it.

Suspect: thread lifecycle is paying multiple allocation/signing/Arc/box/lock
costs per create. The observed `payload_fresh -> payload_sign` outlier should
be traced against zone allocation, EBR retirement, and observe dumping before
changing scheduler policy.

### Futex and Join Wake

- `crates/tx-subsystems/src/futex/mod.rs:458-544` performs compare-before-park,
  rechecks under the exact waiter table lock, publishes an exact wait source,
  and yields on it.
- `crates/tx-subsystems/src/futex/mod.rs:605-717` wakes exact waiters with an
  explicit hint. The latest focused trace found registered and notified futex
  sources matched.
- `crates/tx-subsystems/src/thread_runtime/execution.rs:254-284` implements
  `clear_child_tid` as a separate lifecycle wake.

Ruled out for now: futex key mismatch or missing wake is not the leading
suspect in the latest trace. The source correlation is clean. The remaining
problem is latency after notification, especially `runnable->pick`, and the
dominant futex address in the steady 10x50 trace is the shared musl
`__thread_list_lock`, not per-thread `detach_state`.

### Scheduler and Reactor

- `crates/tx-reactor/src/scheduler.rs:44-68` defines `WakeHandoff` as
  requesting userspace preempt but not entering `Boosted`.
- `crates/tx-reactor/src/scheduler.rs:906-938` and `1218-1226` order local
  queues as `Kernel -> Boosted -> aged Preempted -> New -> Preempted`.
- `crates/tx-reactor/src/scheduler.rs:1341-1407` places ordinary and
  `WakeHandoff` userspace wakes into normal/preempted placement unless boosted.
- `crates/tx-reactor/src/scheduler.rs:1431-1464` requeues userspace traps into
  `Preempted` when budget remains.

Suspect: for single-hart pthread storms, `WakeHandoff` avoids overboosting but
still leaves explicitly woken lock waiters behind already-preempted work. The
corrected trace shows wake drain is usually sub-ms; the long tail is the
`Preempted` queue ordering that lets task 0 and other peers run before the
woken waiter. This is not a semantic futex failure; it is a lock-handoff
scheduling policy problem.

### VM and Malloc/Stack Lifecycle

- `external/musl/src/thread/pthread_create.c:293-305` maps each thread stack
  and TLS region; `external/musl/src/thread/pthread_join.c:24` unmaps it.
- `crates/tx-shims/src/linux_syscall/vm.rs:222-390` decodes `mmap`, attempts
  `try_mmap`, and falls back to a driven VM op on `WouldBlock`.
- `crates/tx-shims/src/linux_syscall/vm.rs:484-525` wraps `try_munmap` and
  falls back to a driven unmap op.
- `crates/tx-subsystems/src/vm/execution.rs:462-503` implements synchronous
  `try_mmap`; `864-877` implements synchronous `try_munmap`.
- `crates/tx-subsystems/src/vm/structure/recipe.rs:394-537` publishes immutable
  recipe trees by building a replacement tree, swapping the root pointer, and
  retiring the old root through EBR.
- `crates/tx-subsystems/src/vm/structure/recipe.rs:558-600` publishes a new
  tree on every map and unmap.

Suspect: VM tree publication and EBR retirement are still hot in both malloc
and pthread stack lifecycle. The immutable sharing contract is now structurally
present, but every map/unmap still publishes a new tree and retires the old
root. The trace's `debug.vm.recipe.publish.retire_old` and
`debug.epoch.retire.*` counters confirm this path is active. A range tree or
batched mutation could reduce rewrite/publish cost, but first measure touched
entries, pmap teardown pages, and EBR drain pressure under `malloc_sparse` and
pthread minimal.

### Observe and Measurement Overhead

- `tools/tx-observe-analyze.py:22-83` decodes VM, epoch, zone, futex, scheduler,
  and clone counters.
- Many current debug emitters call `tx_observe::dump_registered_if_requested()`
  inside hot-path trace helpers, including clone and VM instrumentation.

Suspect: observe itself may amplify timings in focused runs. The difference
between the 70 ms exact-tid trace and the 603 ms wake-handoff trace must be
checked with paired `--observe-bracket` and `noobserve` runs before treating
any single microsecond budget as stable.

## Probe Results: Thread Payload Prewarm and Slice-Cap Experiment

Focused `pthread-minimal2 1x50` probes after the initial suspect sweep:

- `pthread-minimal2-1x50-observe-prewarm-a`: prewarm sentinel present
  (`thread-runtime:prewarm:payload=64`), txtrace validated with `8192`
  records and no trap lines. The clone slab outlier was removed:
  `payload_fresh -> payload_sign` max dropped from the earlier `47643us` /
  `52274us` outliers to `1342us`. Futex wait remained the dominant trace
  bucket: `sys_futex` total `351350us`, `drive.FutexWaitOp` avg `29672us`,
  and `drain->pick` avg `11234us`.
- A temporary userspace-preempted 1 ms slice-cap experiment was negative.
  No-observe `pthread-minimal2 1x50` regressed to `1.085854000s`, and the
  observe run regressed to `1.139232000s`. The trace showed shorter individual
  futex waits, but much higher scheduling churn:
  `pick:Preempted=327`, `UserspaceTrap=175`, `SliceExpired=69`, and
  `debug.trap.timer_user` dominated the event stream. The experiment was
  reverted; this is evidence against a blanket shorter preempted-userspace
  slice.
- After reverting the slice-cap experiment and rebuilding, the same no-observe
  focused run completed in `0.606153000s` with no trap lines and the prewarm
  sentinel still present.

Conclusion: prewarming `ThreadPayload` slots is a real tail-latency mitigation
for the clone allocation outlier, but it is not a full pthread throughput fix.
The next scheduler optimization should be more targeted than reducing the
global userspace-preempted slice: likely a lock/wake handoff policy that avoids
timer churn while improving only the waiter that was explicitly woken.

## Probe Results: 10x50 Corrected Wake Pairing

Steady-state `pthread-minimal2 10x50` with bracketed observe:

- Runtime: `6.641901000s`; trace validated with `32768` records and no trap
  lines.
- Focused scale without observe remained roughly linear but slow:
  `1x1=0.053901000s`, `1x5=0.121030000s`,
  `1x50=0.612634000s`, `10x50=6.699286000s`,
  `50x50=27.501891000s`.
- Corrected analyzer output:
  `target/oscomp/custom-run/pthread-minimal2-10x50-observe-steady.analysis.v2.txt`.
  The previous `notify->drain=277337us` and `79227us` rows were false
  pairings. Raw trace inspection showed the cited `source=0x14b task=7`
  notification drained in about `492us`.
- Corrected futex wake summary:
  `notify->drain avg=499us max=2061us`;
  `drain->pick avg=7905us max=17063us`;
  `notify->pick avg=8405us max=17658us`.
  Paths: `drained=42`, `picked_before_drain=1`, `undrained=1`.
- Intermediate scheduler picks between a waiter becoming runnable and being
  picked are dominated by task 0: `task=0:31`, then `task=3:9`, `task=4:7`,
  `task=5:6`, `task=6:5`, `task=7:5`.
- The futex hot address is stable: `WAIT uaddr=0x1039448 n=44 total=977047us`.
  Musl source review maps this pattern to `__thread_list_lock` in
  `external/musl/src/thread/pthread_create.c:20-50`, used by
  `__tl_lock`, `__tl_unlock`, and `__tl_sync`.

Conclusion: the current leading pthread lifecycle bottleneck is not wake drain
and not futex keying. It is lock-handoff latency for musl's shared thread-list
lock: a futex wake makes the waiter runnable quickly, then the waiter waits in
the preempted queue while task 0 continues stack `mmap`/`munmap`, `clone`, and
join-side cleanup work.

## Probe Results: WakeHandoff Front Placement

Implemented the documented scheduler policy that keeps `WakeHandoff` out of
`Boosted` but places a remaining-budget waiter at the front of `Preempted`.
`Normal` userspace wakes still queue behind already-preempted peers.

Host coverage:

- `wake_handoff_fronts_userspace_waiter_without_boosting`
- `userspace_thread_normal_wake_with_budget_queues_behind_preempted_peers`
- full `cargo test -p tx-reactor --test scheduler -- --nocapture`
- `wake_handoff_same_hart_marks_userspace_preempt_without_remote_ipi`

Guest evidence:

- `pthread-minimal2 10x50` no-observe improved from the prior
  `6.699286000s` scale point to `6.077050000s`.
- `pthread-minimal2 10x50` observe improved from `6.641901000s` to
  `6.372112000s`. The corrected trace
  `target/oscomp/custom-run/pthread-minimal2-10x50-wakefront-observe.analysis.txt`
  shows the intended wake-pick effect:
  `notify->drain avg=473us max=1340us`;
  `drain->pick avg=76us max=246us`;
  `notify->pick avg=549us max=1527us`.
- `pthread-minimal2 50x50` no-observe moved only slightly, from
  `27.501891000s` to `27.287651000s`.

Conclusion: this policy fixes the measured futex wake-pick tail, but the full
pthread lifecycle remains dominated by syscall body and lifecycle cost:
`sys_futex` wait time on the thread-list lock, stack `munmap`, stack `mmap`,
`clone`, and signal-mask/syscall framing. The next fix should not further
front/boost futex waiters; it should target the remaining body costs.

## Probe Results: Terminal EBR Drain and Clone Handoff

The next measured clone/task allocation tail was EBR reclamation cadence, not a
lost child. Before the fix, terminal reactor tasks were removed from the
reactor, but retired thread/task payload slots were mainly reclaimed from the
idle reactor path. Dense pthread storms are non-idle, so allocation/signing and
task-submit tails appeared once enough children had exited.

Implemented mitigation:

- direct child submit remains the production path, with queued drain only as
  fallback;
- terminal completed/cancelled task drain removes `THREAD_REACTOR_TASKS`
  entries and performs two bounded `step_engine::drain_with_budget(64)` passes;
- successful clone returns keep one deferred child-publish handoff even when
  direct submit returned `Published`.

Focused evidence:

- `pthread_static_stack 1x50` improved from `534768000 ns` to `517769000 ns`
  after terminal EBR drain, then to `450998000 ns` after restoring the clone
  handoff.
- `pthread_static_stack 50x50` improved from `22695049000 ns` to
  `21442342000 ns`, then to `20961523000 ns`.
- `pthread-minimal2 50x50` improved from about `27.287651s` to
  `23.395502s`.
- Post-EBR observe analysis showed `sys_clone` average dropping from about
  `2138us` to `1170us`, `payload_fresh.after -> payload_sign.after` max
  dropping from about `48ms` to `501us`, and task submit average dropping from
  about `410us` to `66us`.

Full-suite effect:

- The previous full libcbench run timed out at 300s during
  `b_pthread_createjoin_minimal1` with score `20.761311860355192/27`.
- The current full run completed under the same 300s bound and scored
  `27.88892195361034/27`.

Conclusion: terminal EBR drain plus the retained successful-clone handoff is a
real pthread-suite completion fix. It does not make pthread lifecycle close to
reference; it removes the pathological allocation/reclaim tail enough for the
suite to finish.

## Ranked Suspects

1. VM/malloc publication, page-fault, and EBR cost.
   Evidence: libcbench now completes, but `b_malloc_sparse`,
   `b_malloc_bubble`, `b_malloc_big1`, and `b_malloc_big2` still take
   13-15 seconds. These tests are allocation/write/free storms and should be
   investigated separately from pthread noise with VM recipe, pmap, fault, and
   EBR counters.

2. Stdio and regex throughput.
   Evidence: `b_stdio_putcgetc` and `b_stdio_putcgetc_unlocked` are both about
   27-28 seconds, and `b_regex_compile` is about 10.5 seconds. Those now matter
   for real throughput even though the score gate passes.

3. Syscall and pthread lifecycle body cost after wake-pick mitigation.
   Evidence: `WakeHandoff` front placement reduces `drain->pick` to about
   `76us` average in the 10x50 observe trace, and terminal EBR drain plus the
   retained clone handoff moves full pthread cases from timeout territory to
   completion. The remaining pthread timings are still tens of seconds:
   serial1 `34.854431000s`, serial2 `31.220515000s`, create-serial1
   `31.706098000s`, minimal1 `35.465912000s`, and minimal2 `24.551947000s`.

4. Signal-mask syscall density and thread-list lock wake behavior.
   Evidence: musl create/exit uses signal blocking/restoring and thread-list
   lock futexes; latest trace shows `sys_rt_sigprocmask` total `46775us` and a
   `15240us` max.

5. Clone/task construction after EBR mitigation.
   Evidence: EBR drain removes the worst allocation/signing tails, but each
   create still pays thread payload/identity setup, future boxing, mailbox,
   wake-state allocation, task table submit, and reactor bookkeeping.

6. Trace overhead or timer epoch pathologies.
   Evidence: current focused trace is much slower than the earlier exact-tid
   trace; largest gaps include timer-user events and observe instant gaps.

## Ruled Out Unless New Evidence Appears

- Lost child or process roster divergence: full-run child-submit counters stay
  consistent, and libcbench now reaches the group end marker.
- Exact futex keying failure: latest trace has registered/notified/matched
  sources equal.
- Futex overboost regression: latest hint histogram has `WakeHandoff`, `Normal`,
  and `LifecycleWake`, with no futex `PriorityBoost`.
- Broad syscall-yield policy: removing the futex wake immediate yield was part
  of the scheduling contract; reintroducing broad yields would obscure the
  throughput source.
- Pthread as the only libcbench completion blocker: pthread remains slow, but
  the full suite now completes and the largest remaining timing families also
  include malloc, stdio, and regex.

## Next Carpet-Sweep Probes

1. Reproduce the exact-tid and wake-handoff regimes back to back:
   `pthread-minimal2 1x50` with observe, then noobserve, saved under distinct
   names.

2. Add or reuse phase counters around:
   - zone reserve/sign/reclaim for `ThreadPayload` and `ThreadIdentity`;
   - `TaskTable::submit` substeps;
   - `submit_child_thread_now` reactor lock time and terminal drain count;
   - scheduler `notify -> drain -> runnable -> pick` per task;
   - VM recipe rewrite touched entries, pmap teardown pages, EBR retire/drain.

3. Run focused malloc probes separately from pthread:
   - `malloc_sparse`
   - `malloc_big1`
   - `malloc_big2`
   Capture VM recipe and EBR counters without pthread noise.

4. For pthread, run the smallest increasing matrix:
   - `pthread-minimal2 1x1`
   - `pthread-minimal2 1x5`
   - `pthread-minimal2 1x50`
   - `pthread-minimal2 50x50`
   Compare per-operation averages and tail latency.

5. Only after the above, choose an optimization lane:
   - scheduler latency: tune wake-drain/pick policy without boosting ordinary
     readiness;
   - clone cost: thread/task object reuse or cheaper signing;
   - VM cost: batched/range-tree map/unmap publication;
   - trace cost: reduce hot-path dump/name registration overhead.

## Executable Probe Recipes

Run these from the repository root. Use a fresh build first unless the current
kernel image is known to match the checkout:

```sh
cargo xtask build --target rv64-qemu
```

### Pthread Minimal2 1x50 With Bracketed Observe

This is the primary regime-comparison run. It should be repeated at least twice
before accepting a single slow trace as representative.

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --name pthread-minimal2-1x50-observe-a \
  --libcbench-only pthread-minimal2 \
  --libcbench-outer-repeat 1 \
  --libcbench-inner-repeat 50 \
  --observe-bracket \
  --run \
  --timeout 180 \
  --fault-decode

cargo xtask observe extract \
  --serial target/oscomp/custom-run/os_serial_out_rv.txt \
  --output target/oscomp/custom-run/pthread-minimal2-1x50-observe-a.txtrace

cargo xtask observe validate \
  --file target/oscomp/custom-run/pthread-minimal2-1x50-observe-a.txtrace

cargo xtask observe analyze \
  --file target/oscomp/custom-run/pthread-minimal2-1x50-observe-a.txtrace \
  --names target/oscomp/custom-run/pthread-minimal2-1x50-observe-a.names.json \
  --top 20 \
  > target/oscomp/custom-run/pthread-minimal2-1x50-observe-a.analysis.txt
```

Interpretation:

- If `notify->drain` or `runnable->pick` stays in multi-ms territory while
  futex source correlation remains clean, the next lane is scheduler
  drain/pick latency.
- If clone phase outliers stay near tens of ms, trace zone/sign/EBR around
  `sign_thread`.
- If the trace returns to the earlier 70 ms regime, the slow wake-handoff trace
  was likely a measurement/timer regime and must not drive an optimization.

### Pthread Minimal2 1x50 Without Observe

This establishes observe overhead and serial-only runtime.

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --name pthread-minimal2-1x50-noobserve-a \
  --libcbench-only pthread-minimal2 \
  --libcbench-outer-repeat 1 \
  --libcbench-inner-repeat 50 \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 180 \
  --fault-decode
```

Interpretation:

- If noobserve is fast while observe is slow, reduce hot-path trace dumping
  before changing scheduler or VM semantics.
- If both are slow, treat the observe trace as usable for ranking hot buckets.

### Pthread Minimal2 50x50

This is the steady-state version of the currently blocked benchmark family.

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --name pthread-minimal2-50x50-noobserve-a \
  --libcbench-only pthread-minimal2 \
  --libcbench-outer-repeat 50 \
  --libcbench-inner-repeat 50 \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 240 \
  --fault-decode
```

Interpretation:

- If 1x50 is acceptable but 50x50 grows superlinearly, inspect accumulation:
  task-table terminal drain, EBR retire queues, VM recipe count, and futex table
  waiters/subscribers.
- If 1x50 and 50x50 scale linearly but remain too slow, optimize the per-thread
  lifecycle hot path.

### Full Bounded libcbench

Use this only after focused probes move; it is too broad for first diagnosis.

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --name libcbench-full-throughput-check \
  --libcbench-only all \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 300 \
  --fault-decode

python3 tools/oscomp-judge.py \
  target/oscomp/custom-run/os_serial_out_rv.txt \
  target/oscomp/custom-run/testdata
```

## Suspect-to-Signal Matrix

| Suspect | Primary signal | Moves up if | Moves down if |
| --- | --- | --- | --- |
| Scheduler wake-pick latency | `futex wake latency` and scheduler counters from `tools/tx-observe-analyze.py` | clean futex source correlation with sub-ms drain but multi-ms `runnable->pick`, especially with task 0 or peers picked between runnable and waiter | wakefront trace has `drain->pick avg=76us max=246us`, so more futex boosting is not the next lever |
| Clone allocation/signing/task publish | `clone_thread phases`, `debug.task.submit.*`, `debug.child_submit.*` | repeated ms-scale gaps in `payload_fresh -> payload_sign`, future boxing, mailbox, task table submit, or reactor lock | clone phase p95 stays sub-ms across repeated traces |
| VM recipe and EBR churn | `debug.vm.recipe.*`, `debug.epoch.*`, `debug.vm.pmap.*` | malloc probes and pthread stack lifecycle show high touched entries, pmap teardown, retire queue growth, or drain stalls | VM counters stay flat and syscall spans are dominated elsewhere |
| Signal mask and musl thread-list lock | `sys_rt_sigprocmask`, futex address for `__thread_list_lock`, thread-list wakes | sigprocmask or thread-list lock futexes show ms tails independent of clone/VM | direct-trap sigmask stays low and thread-list futexes do not wait |
| Observe/timer amplification | paired observe/noobserve runtimes and largest inter-record gaps | observe-only slowdown, repeated `timer_user` gaps, or hot debug dump gaps | noobserve reproduces the same slow buckets |

## Required New Counters Before a Fix

Do not select a throughput fix until the chosen suspect has counters at both
the caller-facing span and the internal phase boundary:

- scheduler: per wake `notify_ts`, `drain_ts`, `runnable_ts`, `pick_ts`, task id,
  source id, queue, hint, and stop reason before pick;
- clone: tid, zone reserve/sign phase, task future box, mailbox allocation,
  task table slot reuse/fresh, terminal drain count, reactor lock duration;
- VM: map/unmap range, touched recipe entries, pmap pages removed, old-tree
  retire queued, EBR drain reclaimed/remaining;
- signal/thread-list: sigmask action, old/new mask word count, thread-list lock
  futex wait/wake address, wait count, and wake result;
- observe: debug event count per run, dump-registration count, and whether
  bracketed trace was active.

## Fix Selection Rules

- If futex source correlation is clean, `notify->drain` is sub-ms, and
  `notify->pick` is high, fix wake-pick/lock-handoff scheduling latency, not
  futex keying or wake-drain semantics.
- If clone phases are high but wake latency is low, fix thread/task construction
  or zone signing, not wake placement.
- If malloc probes are slow with high recipe/EBR churn, fix VM publication or
  batch behavior before touching pthread-specific code.
- If observe-only runs are slow, reduce trace overhead before interpreting the
  slow trace as kernel runtime behavior.
- If no single bucket is stable across two runs, keep measuring; do not tune the
  scheduler from a one-off trace.

## 2026-05-30 VM fault and pmap fence update

Focused malloc evidence moved the malloc/VM suspect from recipe publication to
fault materialization and user-pmap publication:

- `tools/shell-tests/malloc_sparse_probe.c` reproduces libc-bench
  `b_malloc_sparse`; `10000 4000 0` measured `13.454211s` before the VM fault
  pass.
- A refreshed bracketed `100 4000 1` trace with the current submit kernel
  showed `debug.thread.trap.kind: PageFault=102 Syscall=47 TimerPreempt=20`;
  every page fault was a write fault and all completed successfully.
- The largest gaps were inside `fault_script`, from
  `debug.thread.page_fault.access=2` to `debug.thread.page_fault.ok=1`, up to
  `9434us`.

Two changes were tested:

- Lowered the existing private-anon write prefault gate from 256 pages to 2
  pages and added `debug.vm.fault.prefault.*` counters. This cut the small
  trace's visible page-fault traps from `102` to `25`, but by itself the full
  no-observe probe stayed flat (`13.436566s`), proving the cost mostly moved
  into kernel-side publication work.
- Removed the redundant RV64 user-pmap commit `sfence.vma`. User pmap commits
  are consumed through `activate_user_pmap` on the next userspace entry, and
  that path writes `satp` and fences before `sret`; unmap/protect invalidation
  fences remain in place.

Current validation:

- `malloc-sparse-probe 10000 4000 0` now reports `12.535930s` with no trap
  lines.
- Full `libcbench-musl` completes and scores `27.96149967877003/27`; saved
  serial:
  `target/oscomp/custom-run/libcbench-prefault2-no-commit-sfence-serial.txt`.
- Key timings improved from the previous full run:
  `b_malloc_sparse 15.105796s -> 11.914281s`,
  `b_malloc_bubble 13.201719s -> 10.031123s`,
  `b_malloc_big1 14.249863s -> 11.414609s`,
  `b_malloc_big2 15.109048s -> 12.114183s`,
  `b_pthread_createjoin_serial1 34.854431s -> 24.082311s`,
  `b_pthread_createjoin_serial2 31.220515s -> 21.452642s`,
  `b_pthread_create_serial1 31.706098s -> 18.490959s`,
  `b_pthread_createjoin_minimal1 35.465912s -> 22.468036s`,
  `b_pthread_createjoin_minimal2 24.551947s -> 14.072427s`,
  `b_stdio_putcgetc 27.272210s -> 22.760696s`,
  `b_regex_compile 10.551844s -> 8.863766s`.

Remaining suspect: pmap publication still operates one page at a time under the
VM pmap lock and still stores a `BTreeMap` resident-page row per publish. The
next throughput lever is a real pmap batch publish path for already-resolved
private-anon fault runs, not another scheduler handoff.

## 2026-05-30 Prefault batch publication update

The private-anon prefault path now batches the tail of a sequential write-fault
run under one `Materializer` reservation and publishes the prepared pages
through a best-effort `VmPmap` batch helper. This reduces repeated RangeLock
acquire/release work and keeps speculative tail failure non-fatal: the leading
fault still completes through the canonical single-page path, and any remaining
unpublished tail pages simply refault later.

Validation:

- `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems
  fault_script_prefaults_adjacent_private_anon_write_pages -- --nocapture`
  passed.
- `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems vm -- --nocapture` passed
  (`121` VM-filtered tests).
- Fresh RV64 submit kernel was built and refreshed with
  `cargo xtask build --target rv64-qemu` and
  `cargo xtask oscomp submit --target rv64-qemu --submit target/oscomp/submit`.
- `malloc_sparse_probe 10000 4000 0` completed in `11.950883s` with no trap
  lines, versus the earlier post-fence `12.535930s`.
- Bracketed `malloc_sparse_probe 100 4000 1` produced a valid txtrace
  (`2048` records, `0` framing errors). The trace still shows `25`
  page-fault traps, but prefault tails now publish up to `15` pages in one
  batch.
- Full custom `libcbench-musl` saved at
  `target/oscomp/custom-run/libcbench-batchprefault-serial.txt` completed,
  fault-decode found no trap lines, and
  `python3 tools/oscomp-judge.py target/oscomp/custom-run/libcbench-batchprefault-serial.txt target/oscomp/custom-run/libcbench-batchprefault-data`
  scored `28.169479468902836/27`.

Interpretation:

- The batch-prefault change is correct and mildly helpful, but the decisive
  timing gap remains inside the batch: the trace's largest gap is now
  `debug.vm.fault.prefault.limit_pages=15 ->
  debug.vm.fault.prefault.published_pages=15` at about `7.7ms`.
- Reducing page-fault trap count alone is no longer the main lever. The next
  candidate is batching or lowering the cost of private-frame materialization
  plus resident pmap publication: private-set inserts, map-pin/accounting, HAL
  PTE reserve/commit, and the resident `BTreeMap` row update.

## 2026-05-30 Private-set subphase follow-up

Additional private-anon/private-set counters split the sparse write-miss body
around private frame allocation, cache-pin acquisition, `PrivateFrame`
construction, and `PrivatePageSet::install_if_absent`. The observed
`malloc_sparse_probe 100 4000 1` traces showed private frame allocation itself
was not the dominant floor; the visible outliers were inside
`PrivatePageSet::install_if_absent` and the pmap batch publish tail.

Rejected experiments:

- Disabling the private-anon prefault gate was a negative control, not a fix:
  `malloc_sparse_probe 10000 4000 0` regressed to `36.750522s`.
- A `BTreeMap<VmPageOff, Arc<PrivateFrame>>` private resident-store compromise
  preserved host VM tests but regressed the same no-observe guest probe to
  `113.620097s`, so it was backed out.
- An in-place `Arc::make_mut` treap insert also preserved host VM tests but
  regressed the guest probe to `25.518800s`, so it was backed out.

Restored-state evidence:

- `malloc_sparse_probe 10000 4000 0` now reports `19.059378s` at
  `target/oscomp/custom-run/malloc-sparse-restored-private-tree-noobserve-serial.txt`.
- `cargo xtask fault-decode --target rv64-qemu --serial
  target/oscomp/custom-run/malloc-sparse-restored-private-tree-noobserve-serial.txt
  --all --brief` found no trap lines.
- Host verification after backing out rejected experiments:
  `cargo fmt --check`,
  `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems vm -- --nocapture`, and the
  focused prefault host test.

Interpretation:

- Private-set insertion remains observable in bracketed traces, but the tested
  resident-store replacements made the real guest benchmark worse. Do not
  continue changing the private-set shape without a new trace proving it is the
  dominant no-observe cost.
- The next measured target should return to pmap batch publish phase `5 -> 6`,
  private frame/map-pin materialization, and full-suite malloc/stdio/regex
  validation. The sparse probe is semantically green but timing is unstable and
  still far from the earlier `11.950883s` prefault baseline.

## 2026-05-30 Pmap resident-row follow-up

The `debug.vm.pmap.publish_batch.insert.phase` split showed the old
`publish_batch.phase 5 -> 6` gap landed after `PmapMapping::new` and before the
insert marker, so the resident pmap row update was the right next narrow
experiment. The kept change replaces the VM pmap shadow resident
`BTreeMap<UserPage, PmapMapping>` with a sorted `Vec` wrapper. The wrapper keeps
the observable contract intact: binary lookup by page, ordered range snapshots,
resident-only teardown/protect page enumeration, replacement on duplicate
publish, and pin release on pmap drop.

Validation and guest evidence:

- `cargo fmt --check` passed.
- `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems vm_pmap_publish --
  --nocapture` passed.
- `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems
  fault_script_prefaults_adjacent_private_anon_write_pages -- --nocapture`
  passed.
- `CARGO_INCREMENTAL=0 cargo test -p tx-subsystems vm -- --nocapture` passed
  (`121` VM-filtered tests).
- Final kept sparse probe:
  `target/oscomp/custom-run/malloc-sparse-pmap-resident-vec-simple-final-noobserve-serial.txt`
  reports `malloc_sparse_probe 10000 4000 0` at `11.267564s`, `rc=0`,
  `custom-run:status:0`, and `userspace:exited:0`.
- `cargo xtask fault-decode --target rv64-qemu --serial
  target/oscomp/custom-run/malloc-sparse-pmap-resident-vec-simple-final-noobserve-serial.txt
  --all --brief` found no trap lines.
- The observed simple-vector run
  `target/oscomp/custom-run/malloc-sparse-pmap-resident-vec-observe.analysis.txt`
  remained valid (`4096` records, `0` framing errors) and moved the largest
  recurrent gaps back toward `mmap.commit.phase=0 ->
  recipe.publish.touched_entries` and `private_set.install.phase=2 -> 3`,
  though pmap insert outliers were still visible under observe overhead.

Rejected pmap micro-paths:

- Adding a batch `Vec::reserve` before publication regressed the no-observe
  probe to `14.644637s`; its observed trace moved the top gap to
  `debug.vm.pmap.publish_batch.phase=1 -> phase=2`, so that reserve was backed
  out.
- A known-absent append fast path did not improve the final no-observe result
  (`14.911925s`), so it was also backed out. The simple sorted-vector wrapper is
  the only pmap resident-row shape kept from this round.

Interpretation:

- The resident pmap row store was a real sparse-probe blocker: the kept simple
  vector shape improves the restored private-tree run from `19.059378s` to
  `11.267564s` and slightly beats the earlier batch-prefault baseline
  (`11.950883s`).
- Further pmap micro-tuning is not justified by the current evidence. The next
  useful measurement is broader libcbench malloc/stdio/regex validation and, if
  still hot, a tighter recipe-publication trace for `mmap`/`brk` commit.

## 2026-05-30 Stdio and regex isolation

The custom runner can now isolate more libcbench families through
`--libcbench-only`: `malloc`, `malloc-sparse`, `stdio`, `stdio-putcgetc`,
`stdio-putcgetc-unlocked`, `regex`, and `regex-compile`. This keeps the
benchmark source patching local to `tools/oscomp-custom-run.py` and is covered
by `tools/tests/test_oscomp_custom_run.py`.

Fresh isolated RV64 runs after the pmap resident-vector change:

- `target/oscomp/custom-run/libcbench-stdio-only-pmap-vec-serial.txt`:
  `b_stdio_putcgetc=24.938529000s`,
  `b_stdio_putcgetc_unlocked=23.982173000s`, `userspace:exited:0`, and no
  decodable trap lines. Stdio remains intrinsically slow even outside the
  full-suite pthread/malloc schedule.
- `target/oscomp/custom-run/libcbench-regex-only-pmap-vec-serial.txt`:
  `b_regex_compile=12.411429000s`,
  `b_regex_search("(a|b|c)*d*b")=0.169943000s`, and
  `b_regex_search("a{25}b")=0.239579000s`, with `userspace:exited:0` and no
  trap lines. The regex outlier is compile-specific, not search.

Stdio trace evidence:

- A bracketed `stdio-putcgetc` trace
  (`target/oscomp/custom-run/libcbench-stdio-putcgetc-bracket-pmap-vec-serial.txt`)
  completed the benchmark at `27.697511000s`, but the trace dump happened
  after the benchmark and contaminated the top syscall buckets with console
  dump `writev` traffic. Treat that trace as proof of completion/framing only,
  not a clean runtime ranking.
- A cleaner threshold trace
  (`target/oscomp/custom-run/libcbench-stdio-putcgetc-threshold8k-pmap-vec-serial.txt`)
  extracted to
  `target/oscomp/custom-run/libcbench-stdio-putcgetc-threshold8k-pmap-vec.txtrace`
  validated with `4096` records and `0` framing errors. Its active window was
  `742993us`; `sys_66` (`writev`) had `n=128`, total `132245us`,
  average `1033.2us`, and max `3687us`. Debug counters show
  `debug.writev.enter: fd=3`, `debug.writev.iovcnt: 2`, and paired write
  lengths `1024:128` plus `1:128`. The hot stdio phase is therefore repeated
  tmpfile `writev(fd=3, iovcnt=2)` calls returning `1025` bytes.

Rejected experiment:

- Gathering the two PageBacked `writev` iovecs into one buffer and calling the
  existing buffered write path once preserved a focused host syscall test, but
  it regressed isolated stdio in the guest:
  `target/oscomp/custom-run/libcbench-stdio-only-writev-combine-serial.txt`
  reported `b_stdio_putcgetc=42.435648000s` and
  `b_stdio_putcgetc_unlocked=46.354763000s`, with no trap lines. The fast path
  was backed out. Do not retry a user-buffer combine in `writev` without a new
  trace proving the copy/allocation side is cheaper than the current two
  writes.

Interpretation:

- The current stdio blocker is the VFS/PageBacked/tmpfile write path and its
  syscall/user-copy overhead for 1024-byte writes, not a pmap resident-row
  issue.
- The next useful stdio probe should add phase counters inside the PageBacked
  write path, `OpenFileWriteFromUserOp`/direct user-buffer copy, and any
  tmpfile page-cache materialization rather than selecting another semantic
  shortcut.
- Regex needs a separate `regex-compile` trace; the search paths are not the
  current problem.

## 2026-05-30 PageBacked stdio phase counters

Follow-up instrumentation added counters around the PageBacked write path:

- syscall boundary: `debug.write.pagebacked.*` around user-buffer prefault and
  `drive(OpenFileWriteFromUserOp)`;
- VFS StepOp: `debug.vfs.write_from_user_op.*` around
  `OpenFileWriteFromUserOp::step`;
- PageBacked user-buffer body: `debug.pagebacked.write_user.*`,
  `debug.pagebacked.user_range.*`, and `debug.pagebacked.user_copy.*` around
  validation, per-page materialization, direct-map lookup, `copy_from_user`,
  offset publication, and `grow_size_to`.

Host fallout fixed during the probe:

- `page_backed::user_buffer_tests` allocated `PageContainer::new_cap` without
  first registering subsystem zones. Nearby PageBacked test files already call
  `crate::zones::register_all()`. The user-buffer test fixture now does the
  same in `setup_host_substrate`, which clears the focused
  `NotRegistered` failures.

Guest evidence:

- First run:
  `target/oscomp/custom-run/libcbench-stdio-putcgetc-pagebacked-trace-serial.txt`
  panicked before userspace at the BSP timer smoke deadline assertion
  (`left: None`, `right: Some(...)`). Fault-decode mapped the synthetic
  breakpoint panic to `rust_begin_unwind`; this serial is not libcbench
  evidence. The run's QEMU process was cleaned up.
- Retry:
  `target/oscomp/custom-run/libcbench-stdio-putcgetc-pagebacked-trace-retry-serial.txt`
  passed timer smoke, reached `b_stdio_putcgetc`, dumped at the observe
  threshold, and fault-decode found no trap lines.
- Extracted trace:
  `target/oscomp/custom-run/libcbench-stdio-putcgetc-pagebacked-trace-retry.txtrace`
  validated with `4096` records and `0` framing errors.

Trace interpretation:

- The extra counters significantly increase observe overhead, so do not compare
  this run's absolute `sys_66` latency against the previous cleaner threshold
  trace. Use it only to locate phase boundaries.
- The sampled window still shows the same stdio shape:
  `writev(fd=3, iovcnt=2)` returning `1025` bytes, split into PageBacked
  writes of `1024` and `1` byte.
- `sys_66/writev` appears `35` times in the sampled window; the nested drive
  span is `OpenFileWriteFromUserOp`, and the step span is the generic
  per-iteration `step`.
- The largest gaps remain outside a simple `writev` gather optimization:
  write-side page faults (`debug.thread.page_fault.access=3 ->
  debug.thread.page_fault.ok=1`) reach about `16.6ms`, and many multi-ms gaps
  occur between `debug.thread.trap.kind=2` and the next `sys_66` span.
- PageBacked phase histograms confirm the sampled writes complete the intended
  path: `write_user phase 0..4`, `user_range phase 0..5`, and
  `user_copy phase 0..3`; no PageBacked error counters dominate.

Next probe:

- Split `AddressSpace::copy_from_user` and the VM/user-access page-fault path
  for PageBacked writes. The useful boundary is below `copy_chunk_user`, not a
  new `writev` semantic shortcut.
- Watch for the BSP timer-smoke panic only if it reproduces; the retry reached
  userspace and produced valid trace data.

## 2026-05-30 PageBacked writev one-shot

The next trace split showed the repeated stdio gap was not inside
PageBacked/user-copy itself. `dispatch_writev_hot` preserved the syscall shape
but did not improve the trace, and inner markers proved the cost happened
before the hot-lane body. Splitting the `thread_future` fallback path then
isolated repeated multi-ms gaps between:

- `debug.thread.dispatch.writev_future.after=66`
- `debug.thread.dispatch.writev_box.after=66`

That is the `Box::pin(dispatch_writev_hot(...))` allocation/boxing step. A
direct-await attempt removed the suspected heap-box point but violated the
existing `run_thread_future_stays_within_clone_submit_budget` size guard by
growing the wrapped future to `2120` bytes, so that route was rejected.

Kept fix:

- `dispatch_writev_pagebacked_oneshot` runs before the boxed async fallback.
- It only claims `NR_WRITEV` when the fd resolves to a regular PageBacked file.
- It parses iovecs with `bootstrap_copy_from_user`, prefaults readable user
  ranges, then calls `page_backed::step_write_from_user` per iovec.
- Partial progress returns the byte count; fallback remains available for
  non-PageBacked fds and future yield-before-progress cases.

Trace evidence:

- `target/oscomp/custom-run/libcbench-stdio-putcgetc-writev-pagebacked-oneshot-trace.txtrace`
  validates with `4096` records and `0` framing errors.
- Analysis shows `sys_66/writev n=22`, average `1075.6us`, max `2591us`.
- Counters show `debug.writev.pagebacked_dispatch.enter=22`,
  `debug.writev.pagebacked_oneshot.enter: fd=3` for `22` calls,
  `debug.writev.pagebacked_dispatch.after=21`, and
  `debug.thread.dispatch.writev_pagebacked.after=21`.
- Only one boxed fallback remains in the sampled window:
  `debug.thread.dispatch.writev_future.after=66` and
  `debug.thread.dispatch.writev_box.after=66`. The earlier repeated
  multi-ms boxed-future gaps are no longer present for the PageBacked tmpfile
  writev traffic.

No-observe timing:

- `target/oscomp/custom-run/libcbench-stdio-only-writev-pagebacked-oneshot-serial.txt`
  completed `libcbench-musl` stdio isolation with
  `b_stdio_putcgetc=6.525488000s`,
  `b_stdio_putcgetc_unlocked=6.692102000s`, `userspace:exited:0`, and no
  decodable trap lines.
- This is the first clean post-fix timing for the stdio-only lane and should
  be compared against the earlier pmap-vector isolated run:
  `24.938529000s` / `23.982173000s`.

Interpretation:

- The PageBacked tmpfile stdio blocker was a dispatch-allocation hot-lane
  problem, not a reason to retry the rejected iovec-combine experiment.
- Keep the boxed async fallback and its markers for the remaining non-PageBacked
  or exceptional `writev` cases.
- The broader post-fix measurement completed:
  `target/oscomp/custom-run/libcbench-full-writev-pagebacked-oneshot-serial.txt`
  reached `userspace:exited:0`, had no decodable trap lines, and scores
  `27.76360314008035/27` under `tools/oscomp-judge.py`.
- Full-suite stdio is now much lower than the earlier pmap-vector isolated run:
  `b_stdio_putcgetc=9.651879000s` and
  `b_stdio_putcgetc_unlocked=9.714694000s`. The isolated stdio-only run remains
  the cleaner timing for the lane at `6.525488000s` / `6.692102000s`.
- Current full-suite long poles after this fix are thread lifecycle and regex
  compile: `b_pthread_createjoin_serial1=85.902965000s`,
  `b_pthread_createjoin_serial2=39.326915000s`,
  `b_pthread_create_serial1=40.115729000s`,
  `b_pthread_createjoin_minimal1=45.806089000s`,
  `b_pthread_createjoin_minimal2=33.385800000s`, and
  `b_regex_compile=20.695216000s`.

## 2026-05-30 SMP4 libcbench experiment

Question: can RV64 SMP parallelism improve the current async/thread lifecycle
throughput without another narrow fast path?

Method:

- Rebuilt the private custom-run image with `tools/oscomp-custom-run.py`.
- Bypassed the helper's hard-coded single-core runner and booted the image via
  `make ... oscomp-qemu-rv64-smp4` with
  `OSCOMP_CMDLINE='tx.oscomp.observe=0 tx.oscomp.groups=libcbench-musl'`.
- Kept separate serials for pthread-only and full libcbench:
  - `target/oscomp/custom-run/libcbench-pthread-only-smp4-after-writev-oneshot-serial.txt`
  - `target/oscomp/custom-run/libcbench-full-smp4-after-writev-oneshot-serial.txt`

Both SMP4 boots reported AP/reactor markers, including `smp:aps:online`,
`reactor:dispatch:ipi:ok`, and `reactor:ap-runqueue:ok`. Both reached
`userspace:exited:0`; `cargo xtask fault-decode --target rv64-qemu --serial ...
--all --brief` found no trap lines.

Pthread-only comparison:

| benchmark | single-core | smp4 |
| --- | ---: | ---: |
| `b_pthread_createjoin_serial1` | `36.226766000s` | `40.113108000s` |
| `b_pthread_createjoin_serial2` | `34.352216000s` | `23.988273000s` |
| `b_pthread_create_serial1` | `63.297696000s` | `20.226109000s` |
| `b_pthread_uselesslock` | `0.187540000s` | `0.123082000s` |
| `b_pthread_createjoin_minimal1` | `53.364108000s` | `39.805734000s` |
| `b_pthread_createjoin_minimal2` | `27.449239000s` | `20.146469000s` |

Full-suite comparison against
`target/oscomp/custom-run/libcbench-full-writev-pagebacked-oneshot-serial.txt`:

| benchmark | single-core full | smp4 full |
| --- | ---: | ---: |
| `b_malloc_sparse` | `12.163078000s` | `8.956626000s` |
| `b_malloc_big2` | `12.665155000s` | `7.493007000s` |
| `b_malloc_thread_stress` | `2.774154000s` | `1.915738000s` |
| `b_pthread_createjoin_serial1` | `85.902965000s` | `39.550705000s` |
| `b_pthread_createjoin_serial2` | `39.326915000s` | `23.583054000s` |
| `b_pthread_create_serial1` | `40.115729000s` | `20.576924000s` |
| `b_pthread_createjoin_minimal1` | `45.806089000s` | `37.910496000s` |
| `b_pthread_createjoin_minimal2` | `33.385800000s` | `20.347226000s` |
| `b_stdio_putcgetc` | `9.651879000s` | `6.482566000s` |
| `b_stdio_putcgetc_unlocked` | `9.714694000s` | `6.368331000s` |
| `b_regex_compile` | `20.695216000s` | `14.649655000s` |

The SMP4 full-suite score was `27.817993659734583/27` under
`tools/oscomp-judge.py`, compared with the single-core post-writev score
`27.76360314008035/27`.

Interpretation:

- SMP4 gives real throughput relief for the mixed suite, especially
  create-only, batched pthread, stdio, regex compile, and parts of malloc.
- It is not a substitute for direct lifecycle/syscall optimization:
  pthread-only `serial1` regressed from `36.226766000s` to `40.113108000s`, and
  `minimal1` remains high at `39.805734000s` isolated / `37.910496000s` full.
- The likely next measurement is a bracketed SMP4 trace for `pthread-minimal1`
  or `pthread-serial1` to separate true AP overlap from serialized process,
  VM, signal, and scheduler locks.

## 2026-05-30 pthread-minimal1 lifecycle trace

Question: can the remaining pthread lifecycle cost be explained by
`rt_sigprocmask`, and would a direct mask-state write be enough?

Method:

- `boards/tx-hal-riscv64-qemu-virt/src/lib.rs` now backs observe with four
  1 MiB rings instead of one 4 MiB hart-0-only ring. This preserves the old
  total static footprint while allowing bracketed SMP dumps from whichever hart
  runs the benchmark.
- `tools/oscomp-custom-run.py` now accepts `--libcbench-serial-repeat`, which
  rewrites libc-bench pthread fixed serial loops from `i<2500` to a smaller
  bound for complete traces.
- `pthread-minimal1` was run with bracketed observe on SMP4 at 50 and 10 serial
  iterations:
  - `target/oscomp/custom-run/pthread-minimal1-50-smp4-observe-serial.txt`
  - `target/oscomp/custom-run/pthread-minimal1-10-smp4-observe-serial.txt`

Evidence:

- The 50-iteration run completed `b_pthread_createjoin_minimal1` in
  `0.945423000s`; its trace
  `target/oscomp/custom-run/pthread-minimal1-50-smp4-observe.txtrace`
  validates with `8192` records and `0` framing errors. Fault-decode finds no
  trap lines.
- The 10-iteration run completed in `0.222168000s`; its trace
  `target/oscomp/custom-run/pthread-minimal1-10-smp4-observe.txtrace`
  validates with `4096` records and `0` framing errors. Fault-decode again
  finds no trap lines. This boot printed `reactor:ap-loop:WARN-skipped`, so use
  it as a lifecycle-shape trace, not as proof of clean AP parallel execution.

50-iteration sampled window:

- `sys_futex`: `n=37`, total `53.369ms`, avg `1.442ms`, max `20.181ms`.
- `sys_munmap`: `n=16`, total `44.981ms`, avg `2.811ms`, max `7.286ms`.
- `sys_clone`: `n=18`, total `39.663ms`, avg `2.203ms`, max `17.497ms`.
- `sys_rt_sigprocmask`: `n=73`, total `38.762ms`, avg `0.531ms`,
  max `13.045ms`.
- `sys_mmap`: `n=17`, total `15.227ms`, avg `0.896ms`.
- Largest gaps remain page faults and VM unmap publication:
  `debug.thread.page_fault.access=3 -> ok=1` up to `43.054ms`, and
  `debug.vm.unmap.phase=1 -> debug.vm.recipe.publish.touched_entries`
  in the `3.3-6.3ms` range.

10-iteration sampled window:

- `sys_futex`: `n=17`, total `28.487ms`, avg `1.676ms`, max `13.794ms`.
- `sys_rt_sigprocmask`: `n=36`, total `27.843ms`, avg `0.773ms`,
  max `14.247ms`.
- `sys_clone`: `n=8`, total `21.114ms`, avg `2.639ms`, max `8.229ms`.
- `sys_munmap`: `n=7`, total `18.506ms`, avg `2.644ms`, max `5.731ms`.
- `sys_mmap`: `n=8`, total `10.369ms`, avg `1.296ms`.
- Largest gaps again start with private write page faults:
  `debug.thread.page_fault.access=3 -> ok=1` at about `17.129ms`,
  `17.009ms`, `14.338ms`, `13.725ms`, and `13.361ms`, followed by
  `debug.vm.unmap.phase=1 -> recipe.publish.touched_entries` around
  `3.5-4.7ms`.

Interpretation:

- `rt_sigprocmask` is not just a memory write. The kernel must validate and
  copy the user `set` and optional `oldset`, update per-thread signal state,
  refresh deliverability when the mask changes, and return through the trap
  path. The actual `payload.signal_mask.store(...)` is already the cheap part.
- The slow first `rt_sigprocmask` sample is coupled to lazy user-page
  resolution/materialization. Raw events around the `14.247ms` span show
  `debug.vm.user.resolve.*` and pagebacked/private page work before the
  `debug.sigprocmask.write.after` / return markers. Once those pages are
  resident, later `rt_sigprocmask` calls are mostly a few hundred
  microseconds.
- The current pthread-minimal lifecycle is therefore a mixed cost:
  stack/TLS `mmap`, `clone`, futex join/handoff, stack `munmap`, and lazy
  user-page faults. A direct mask-store shortcut would bypass correctness
  checks and would not remove the observed page-fault or unmap publication
  gaps.

Next probe:

- Split VM user-page resolution/materialization below
  `AddressSpace::copy_from_user`/`copy_to_user` for small scalar syscall
  arguments, especially first-touch private stack/TLS pages.
- Continue the stack lifecycle lane at `munmap`: the repeated
  `unmap.phase=1 -> recipe.publish.touched_entries` gaps are now more
  actionable than another `sigprocmask` special case.
- Re-run the small bracketed trace only after AP loop health is clean if the
  question is SMP parallelism rather than lifecycle phase ranking.

## Page-fault Phase Check: Cold Ext4 Executable Fetch

Follow-up on 2026-05-30 split the `debug.thread.page_fault.*` windows with
VM fault-script, private-read-miss, and PageBacked fault-step counters.

Evidence:

- Single-core `pthread-minimal1` 10-iteration bracket:
  `target/oscomp/custom-run/pthread-minimal1-10-pagefault-phases-serial.txt`
  completed `b_pthread_createjoin_minimal1` in `0.176617000s`; the extracted
  trace
  `target/oscomp/custom-run/pthread-minimal1-10-pagefault-phases.txtrace`
  validates with `4096` records and `0` framing errors. Fault-decode found no
  trap lines.
- Page-fault grouping from that trace found 23 closed faults. Access `3`
  execute faults: `n=12`, all `VmBacking::Page`, total `72.403ms`, average
  `6.034ms`, max `17.859ms`. Access `2` private-anon write faults:
  `n=10`, total `5.821ms`, average `0.582ms`, max `0.722ms`. Access `1`
  read fault: `n=1`, `0.409ms`.
- In the five worst execute faults, almost the entire delay was
  `debug.vm.fault.materialize.phase=1 -> phase=2`, with individual gaps around
  `11.263-15.689ms`. Resolve and publish were hundreds of microseconds or
  less.
- Deeper single-core bracket:
  `target/oscomp/custom-run/pthread-minimal1-10-pagefault-deep-serial.txt`
  completed in `0.192500000s`; the extracted trace
  `target/oscomp/custom-run/pthread-minimal1-10-pagefault-deep.txtrace`
  also validates with `4096` records and `0` framing errors. Fault-decode
  again found no trap lines.
- In the deep trace, access `3` execute faults remained all page-backed:
  `n=12`, total `76.745ms`, average `6.395ms`, max `16.895ms`. The five
  largest gaps moved one level down to
  `debug.pagebacked.fault_step.kind=2 -> debug.pagebacked.fault_step.done=2`
  at `12.354-16.112ms`.

Interpretation:

- `kind=2` is `PageContainerKind::File`; the slow path is
  `VmFaultOutcome::materialize_private_read_miss` calling
  `PageContainer::materialize_page_for_fault_step`, then
  `PageContainer::materialize_file_page`, then the mount's
  `FsPageBacking::fetch_page`.
- For the libcbench sdcard path that backend is ext4:
  `Ext4FsInstance::fetch_page` synchronously calls
  `Ext4Pager::read_page(...)` and then `materialize_frame(...)`. There is no
  current `Yield`/async wait-source split; the delay is a synchronous cold
  executable-page fetch and frame materialization.
- This page-fault check therefore changes the next optimization target. The
  slow first `rt_sigprocmask` span is adjacent to first-touch executable/user
  page faults, not to the signal-mask store. VM resolve, pmap publication, and
  private-anon write faults are not the dominant page-fault cost in this
  window.

Next probe:

- Add counters inside `Ext4FsInstance::fetch_page` and
  `Ext4Pager::read_page` to split inode metadata lookup, extent/block mapping,
  block-device `read_block`, and `materialize_frame` copy/pin work.
- If the cold executable-page fetch repeats across pthread lifecycle batches,
  consider an exec/load prefetch or page-cache retention path before another
  syscall-level fast path.
