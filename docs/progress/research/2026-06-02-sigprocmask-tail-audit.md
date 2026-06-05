# rt_sigprocmask Tail Audit

## Context

The pthread process-DS observe run reported:

```text
sys_rt_sigprocmask n=50,046 total=16,730,757.0us p50=289.0us p99=1,080.0us max=79,257.0us
```

Artifact:

```text
target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224
```

The question was whether the tail is caused by Arc-like payload retention or a
similar long operation inside `sys_rt_sigprocmask`.

## Implementation Shape

`sys_rt_sigprocmask_thread_aspace` is a compact synchronous arm:

1. Decode `how`, `set`, `oldset`, and `sigsetsize`.
2. If `set != 0`, read one `u64` through `bootstrap_read_user`, which uses
   `AddressSpace::read_user`.
3. If this is not query-only, call `step_sigprocmask`.
4. If `oldset != 0`, write one `u64` through `bootstrap_write_user`, which uses
   `AddressSpace::write_user`.

`step_sigprocmask` upgrades the thread payload, loads the current atomic mask,
computes the new mask, stores only if it changed, then refreshes deliverability
only on a real change.

The payload upgrade is not `Arc`. It takes `ThreadIdentity.payload:
SpinMutex<Option<PayloadCap<ThreadPayload>>>`, clones a zone `PayloadCap`, and
drops the lock. `PayloadCap::clone()` delegates to `Cap::clone()`, which is a
zone retain CAS loop over slot metadata.

## Trace Findings

DuckDB query over the worst spans:

```sh
duckdb --no-stdin -csv -c "
WITH s AS (
  SELECT *
  FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/spans.parquet')
  WHERE sysno=135
)
SELECT span, task_id, process_id, begin_hart, end_hart, dur AS dur_ns,
       sched_coverage AS sched_ns,
       dur - coalesce(sched_coverage,0) AS offcpu_or_uncovered_ns,
       sched_segments, sched_harts, ret, errno
FROM s
ORDER BY dur DESC
LIMIT 20;
"
```

Result summary:

- Worst span `0x100000000012249`: `79,257,000ns`, hart `1->1`, `ret=0`,
  `errno=0`.
- Top 20 slow spans had `task_id=NULL`, `process_id=NULL`,
  `sched_coverage=NULL`, and no scheduler interval attribution because this
  capture exported `sched_intervals=0`.

Table counts from the same derived parquet:

```text
sched=0
lock=0
ds=50,089
counters=5,137,102
spans=142,193
```

Join of slow `rt_sigprocmask` spans against process DS rows:

```sh
duckdb --no-stdin -csv -c "
WITH s AS (
  SELECT *
  FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/spans.parquet')
  WHERE sysno=135
),
d AS (
  SELECT *
  FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/ds_method_rows.parquet')
)
SELECT s.span, s.dur AS span_ns, count(d.ts) AS ds_events,
       coalesce(sum(d.value),0) AS ds_ns, max(d.value) AS ds_max_ns
FROM s
LEFT JOIN d ON d.ts BETWEEN s.begin AND s.end
WHERE s.dur >= 5000000
GROUP BY s.span, s.dur
ORDER BY s.dur DESC
LIMIT 25;
"
```

Every listed slow span had `ds_events=0`, `ds_ns=0`.

Join of the top 20 slow spans against named counters in the derived tables:

```text
counters_in_span=0 for every top-20 slow rt_sigprocmask span
```

This was incomplete as an attribution statement: the artifact's `names.json`
does not contain the VM user-access debug names, and these counters were
emitted with `span=0,parent=0`, so the derived span-parent join hides them.
Raw timestamp-window decode of the worst span does show the VM user-access
phases:

```text
span 0x100000000012249 begin=107114632000 end=107193889000 dur=79,257,000ns
runtime complete=true raw_records=6,923,021 lost=0 overwritten=0 repairs=0

+0ns        SpanBegin sysno=135 hart=1 seq=3550132
+419,000    debug.vm.user.copy_in.len       8
+429,000    debug.vm.user.copy_in.chunk     8
+434,000    debug.vm.user.copy_in.phase     0
+439,000    debug.vm.user.resolve.kind      0
+444,000    debug.vm.user.resolve.phase     0
+449,000    debug.vm.user.resolve.phase     1
+623,000    debug.vm.user.resolve.phase     2
+685,000    debug.vm.user.resolve.phase     3
+699,000    debug.vm.user.copy_in.phase     1
+731,000    debug.vm.user.copy_in.phase     2
+741,000    debug.vm.user.copy_in.copied    8
+767,000    debug.vm.user.copy_in.phase     3
+78,394,000 debug.vm.user.resolve.kind      1
+78,439,000 debug.vm.user.resolve.phase     0
+78,446,000 debug.vm.user.resolve.phase     1
+79,106,000 debug.vm.user.resolve.phase     2
+79,187,000 debug.vm.user.resolve.phase     3
+79,257,000 SpanEnd ret=0 errno=0 hart=1 seq=3550156
```

The long hole is therefore not in the initial `set` read and not in the
write-side VM resolution itself. The gap is between successful copy-in and the
first old-mask write resolution marker, which localizes it to
`step_sigprocmask` plus its post-mutation deliverability refresh.

Code path in that interval:

- `crates/tx-subsystems/src/thread_runtime/execution.rs`: `step_sigprocmask`
  upgrades the payload, updates the atomic signal mask, and calls
  `refresh_deliverable_signal_summary(thread)` only when the mask changes.
- `crates/tx-subsystems/src/signal/mod.rs`: the refresh calls
  `select_next_signal(thread).is_some()`, then updates the interrupt summary.
- `select_next_signal` takes `thread.payload`, drops it, upgrades
  `thread.owner_proc`, takes `proc.payload`, then re-takes `thread.payload` to
  re-check the mask before group-pending selection.

That `thread.payload -> proc.payload -> thread.payload` reacquire pattern is
the remaining suspect. It could be lock wait on either payload lock, service
time while a lock is held, or latency inside the owner-process weak-cap
upgrade. `Weak::upgrade()` itself is a single `observe(...)?` followed by
`IdentRef::to_cap().ok()`; the inner CAS retries are not counted at this API
boundary without a broader substrate instrumentation change.

## Cross-Run Comparison

Neighboring full pthread artifacts with the same 50,046 `rt_sigprocmask` count
show similar aggregate totals but different max outliers:

```text
process-ds-pthread-retry-20260602-200224        total=16,730,757us max=79,257us
recipe-bplus-arc-pthread-lock-20260602-124405   total=16,843,088us max=22,588us
recipe-bplus-parentcopy-pthread-lock-20260602-151000 total=16,694,623us max=15,368us
recipe-bplus-fanout16-pthread-lock-20260602-132000    total=16,231,738us max=26,103us
recipe-bplus-slice-pthread-lock-20260602-100614       total=18,688,280us max=17,482us
```

This does not support the claim that the B+ Arc variant uniquely created the
`rt_sigprocmask` tail. The total is mostly stable across variants; the max is a
rare outlier that moves between captures.

## First-Pass Conclusion

The first-pass evidence did not support "Arc caused the `rt_sigprocmask` long
tail."

More precise conclusion:

- The semantic mask store is cheap: one payload upgrade, one atomic mask
  load/store, and optional deliverability refresh only when the mask changes.
- The payload handle path is zone `Cap` retention under a small
  `ThreadIdentity.payload` spin lock, not `Arc`.
- In the cited run, slow `rt_sigprocmask` spans contain no process DS rows and
  no lock rows. Raw VM markers show the worst outlier is not initial copy-in or
  write-side VM resolution; it sits between the copy-in completion and the
  start of old-mask write resolution.
- The remaining suspect is post-mutation work in `step_sigprocmask`, especially
  `refresh_deliverable_signal_summary()` / `select_next_signal()` and its
  payload-lock reacquire pattern.
- This did not look like a pure measurement error: the raw record drain is
  complete, the span begin/end are well formed on hart 1, and the VM phase
  markers placed the 78ms hole inside a concrete interval. It was still not
  fully attributed until the next capture identified which internal operation
  owned the hole.

## Follow-up Instrumentation

Instrumentation added for the next run:

- `debug.signal.select.thread1.lock.request/acquired/release`
- `debug.signal.select.owner.upgrade.request/done/miss`
- `debug.signal.select.proc.lock.request/acquired/release`
- `debug.signal.select.thread2.lock.request/acquired/release`
- `debug.signal.select.thread_pending.hit`
- `debug.signal.select.group_pending.hit`
- `debug.signal.select.done`

Interpretation:

- `*.lock.request -> *.lock.acquired`: lock wait / contention.
- `*.lock.acquired -> *.lock.release`: service time while that lock is held.
- `owner.upgrade.request -> owner.upgrade.done`: weak-cap upgrade latency.
- `owner.upgrade.miss`: dead/missing owner process.
- `thread2.*`: the group-pending re-check reacquire, distinct from the first
  thread-pending pass.

Rerun a focused pthread capture with the current `debug.sigprocmask.*` and
`debug.signal.select.*` markers present:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_process --cfg tx_ds_metrics --cfg tx_ds_metrics_process" \
  cargo xtask observe oscomp-live --name sigprocmask-phase-pthread-YYYYMMDD-HHMMSS --test pthread --timeout 900
```

Then join the slowest `sys_rt_sigprocmask` spans against:

- `debug.sigprocmask.enter/read.after/step.after/write.after/return`
- `debug.signal.select.*` request/acquired/release markers
- `debug.vm.user.copy_in.*` and `debug.vm.user.resolve.*`
- `lock_rows` for `ThreadIdentity.payload` or process locks if they are added
  to the observed lock set

Do not optimize `step_sigprocmask` or replace payload retention from this
artifact alone.

## Follow-up Measurement

The focused phase-only pthread-minimal1 rerun with the per-operation markers is
lossless:

```text
target/oscomp/custom-run/lock-service-step-sigprocmask-pthread-minimal1-20260602-230201
runtime complete=true raw_records=1,446,316 lost_records=0 overwritten_records=0 repairs=0
```

Guest body:

```text
b_pthread_createjoin_minimal1 time: 45.070008s
userspace:exited:0
```

`sys_rt_sigprocmask` aggregate in that run:

```text
n=10,007 total=5.231679s p50=461us p95=921us p99=1.802ms max=66.992ms
```

`step_sigprocmask` phase timers:

```text
debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns
  n=7,506 total=1.789111s p50=197us p95=396us p99=795us max=46.651ms
debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns
  n=10,007 total=409.778ms p50=32us p95=68us p99=166us max=19.651ms
debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns
  n=10,007 total=102.618ms p50=8us p95=16us p99=51us max=3.305ms
debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns
  n=10,007 total=99.248ms p50=8us p95=16us p99=42us max=1.137ms
debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns
  n=10,007 total=123.400ms p50=10us p95=20us p99=64us max=919us
debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns
  n=7,506 total=66.476ms p50=8us p95=15us p99=35us max=730us
```

Worst `rt_sigprocmask` span in the lossless follow-up run:

```text
span=0x6c8c dur=66.992ms
+0.209ms  debug.vm.user.copy_in.phase=3
+19.730ms payload_lock_wait.duration_ns=10us
+19.872ms payload_cap_clone.duration_ns=78us
+19.895ms payload_lock_held.duration_ns=19.651ms
+20.214ms mask_compute.duration_ns=286us
+20.254ms mask_store.duration_ns=19us
+20.269ms debug.signal.select.thread1.lock.request
+20.299ms debug.signal.select.thread1.lock.acquired
+20.326ms debug.signal.select.thread1.lock.release
+35.897ms debug.signal.select.owner.upgrade.request
+66.331ms debug.signal.select.owner.upgrade.done
+66.505ms debug.signal.select.proc.lock.request
+66.605ms debug.signal.select.proc.lock.acquired
+66.616ms debug.signal.select.thread2.lock.request
+66.655ms debug.signal.select.thread2.lock.acquired
+66.716ms debug.signal.select.thread2.lock.release
+66.771ms debug.signal.select.proc.lock.release
+66.788ms debug.signal.select.done
+66.924ms refresh.duration_ns=46.651ms
```

Important timestamp caveat: the duration counters are emitted after the measured
scope exits. Use the counter value as the duration; the counter timestamp is the
emission point, not the interval start.

## Updated Conclusion

The long tail is not a measurement error in the available lossless captures, and
it is not explained by B+ Arc churn, user copy-in, write-side VM resolution, or
generic lock contention.

The final lossless sample attributes the worst observed `rt_sigprocmask` span to
`step_sigprocmask` post-copy work:

- The lock wait bucket is tiny in the worst sample (`payload_lock_wait=10us`)
  and modest in aggregate (`p99=51us`), so this is not ordinary lock queueing.
- The largest bucket is
  `refresh_deliverable_signal_summary()` / `select_next_signal()` with
  `refresh.duration_ns=46.651ms`.
- Inside that refresh, the dominant marked gap is
  `owner.upgrade.request -> owner.upgrade.done`, about `30.434ms`.
- There is also a separate `19.651ms` measured
  `ThreadIdentity.payload` lock-held interval before the refresh. That interval
  includes payload-cap clone/release-side work as measured by the scoped timer;
  the direct `payload_cap_clone` timer in the same sample is only `78us`.

So the current root-cause bucket is weak-cap owner-process upgrade latency during
signal deliverability refresh, plus a smaller but real thread-payload lock-held
tail. The next useful probe is inside the substrate cap upgrade/retain path to
split one slow upgrade from many fast retries; optimizing the signal mask store
or replacing payload retention is not justified by this evidence.

## Cap Upgrade Probe

Follow-up instrumentation now adds sparse counters around the
`IdentRef::to_cap()` CAS loop behind `tx_cap_upgrade_metrics`:

```text
debug.cap.upgrade.to_cap.duration_ns
debug.cap.upgrade.to_cap.attempts
debug.cap.upgrade.to_cap.retries
```

The probe emits only when an upgrade is slow (`>=100us`) or retried, so it can
test the suspected weak-cap CAS/retry choke without flooding the observe ring.
The earlier high-volume local probes are still available but split behind
narrower cfgs:

```text
tx_signal_select_metrics        enables debug.signal.select.*
tx_sigprocmask_phase_metrics    enables debug.lock_service.thread.payload.sigprocmask.*
```

Recommended lean rerun:

```sh
RUSTFLAGS="--cfg tx_cap_upgrade_metrics" \
  cargo xtask observe oscomp-live \
    --name sigprocmask-cap-upgrade-pthread-minimal1-YYYYMMDD-HHMMSS \
    --test pthread-minimal1 \
    --timeout 900
```

If `debug.cap.upgrade.to_cap.retries` spikes inside slow `sys_rt_sigprocmask`
spans, the 30ms gap is CAS retry/starvation on the process identity slot. If
`duration_ns` spikes with `attempts=1`, the delay is outside the CAS retry loop
and the next probe should split epoch guard/registry lookup/metadata load from
the successful retain CAS.

## Cap Upgrade Rerun and Fast Path

The lean cap-upgrade pthread-minimal1 rerun is lossless:

```text
target/oscomp/custom-run/sigprocmask-cap-upgrade-pthread-minimal1-20260603-lean
runtime complete=true raw_records=1,344,894 lost_records=0 overwritten_records=0 repairs=0
```

`sys_rt_sigprocmask` aggregate in that run:

```text
n=10,007 total=5.004761s p50=409us p95=1.035ms p99=1.965ms max=24.454ms
```

The `IdentRef::to_cap()` counters do not confirm the CAS-retry hypothesis in
this capture:

```text
debug.cap.upgrade.to_cap.attempts    n=127 total=127 p50=1 p95=1 p99=1 max=1
debug.cap.upgrade.to_cap.retries     n=127 total=0   p50=0 p95=0 p99=0 max=0
debug.cap.upgrade.to_cap.duration_ns n=127 total=51.195ms p50=175us p95=1.961ms p99=3.727ms max=5.547ms
```

Only three of the top twenty `rt_sigprocmask` spans carried cap-upgrade samples,
and each was a single-attempt, zero-retry upgrade. The worst observed
`rt_sigprocmask` span in this lean run was `24.454ms` and had no cap-upgrade
counter in its timestamp window. Current conclusion: the owner-process upgrade
is still an avoidable tail bucket, but this run does not show CAS retry as the
mechanism.

Mitigation added after the rerun:

- `ThreadPayload` now carries a conservative `group_pending_summary` bitset.
- Process-group signal producers mark live member thread hints before publishing
  to authoritative `ProcessPayload.group_pending`.
- `refresh_deliverable_signal_summary()` first checks thread-pending bits and
  the per-thread group hint. If both are empty after the current mask, it updates
  the denormalized summary without upgrading `owner_proc`.
- Direct `select_next_signal()` remains authoritative and still checks
  `ProcessPayload.group_pending`, so delivery selection does not trust the hint.
- `ast_check()` resynchronizes thread hints from the authoritative group-pending
  snapshot after clearing a group signal. Stale nonzero hints are allowed because
  they only cost an upgrade; false zero is avoided by publishing hints before the
  process bit.

This fast path targets the common no-group-pending `sigprocmask` refresh path,
not the substrate CAS loop. A follow-up observe should compare the same
pthread-minimal1 window with this mitigation enabled and `tx_cap_upgrade_metrics`
still on; success means fewer `owner_proc` upgrades and lower
`sys_rt_sigprocmask` p99/max even if cap-upgrade attempts remain `1`.

## Fast-Path Rerun and Mechanism Narrowing

The first fast-path plus Weak-upgrade probe rerun at 4 harts timed out and is
not usable latency evidence:

```text
target/oscomp/custom-run/sigprocmask-weak-upgrade-fastpath-pthread-minimal1-20260603-rerun
qemu timed out after 900s
host/trace.rawrecords = 0 bytes
no runtime.json/report.json
serial parked after libcbench-musl start
```

A bounded non-live smoke with the same patched tree completed
`pthread-minimal1` and did not show traps, so the 4-hart live timeout is
classified as a failed observe run shape rather than evidence that the fast path
broke the benchmark:

```text
target/oscomp/custom-run/sigprocmask-fastpath-smoke-20260603
b_pthread_createjoin_minimal1 time: 16.104965s
userspace:exited:0
fault-decode: no scause/sepc/stval trap lines found
```

The usable low-volume rerun used one hart and only `tx_cap_upgrade_metrics`:

```sh
RUSTFLAGS="--cfg tx_cap_upgrade_metrics" \
  cargo xtask observe oscomp-live \
    --name sigprocmask-weak-upgrade-fastpath-pthread-minimal1-20260603-smp1 \
    --test pthread-minimal1 \
    --timeout 300 \
    --smp 1
```

Runtime quality:

```text
target/oscomp/custom-run/sigprocmask-weak-upgrade-fastpath-pthread-minimal1-20260603-smp1
runtime complete=true raw_records=1,268,305 lost_records=0 overwritten_records=0 repairs=0
b_pthread_createjoin_minimal1 time: 23.770552s
userspace:exited:0
```

`sys_rt_sigprocmask` aggregate:

```text
n=10,007 total=1.867709s p50=181us p95=232us p99=286us max=14.481ms
```

Cap and Weak upgrade counters:

```text
debug.cap.upgrade.to_cap.attempts    n=2  total=2 p50=1 p95=1 p99=1 max=1
debug.cap.upgrade.to_cap.retries     n=2  total=0 p50=0 p95=0 p99=0 max=0
debug.cap.upgrade.to_cap.duration_ns n=2  total=1.421ms p50=119us p99=1.302ms max=1.302ms

debug.cap.upgrade.weak.total.duration_ns    n=30 p50=137us p95=564us p99=1.372ms max=1.372ms
debug.cap.upgrade.weak.registry.duration_ns n=30 p50=65us  p95=343us p99=354us   max=354us
debug.cap.upgrade.weak.meta.duration_ns     n=30 p50=6us   p95=67us  p99=68us    max=68us
debug.cap.upgrade.weak.to_cap.duration_ns   n=30 p50=19us  p95=456us p99=1.355ms max=1.355ms
debug.cap.upgrade.weak.outcome              n=30 all success
```

The decisive join result:

```text
cap_rows_in_sigprocmask=0
spans_with_cap_rows=0
```

So after the fast path, no sampled Cap/Weak upgrade rows fall inside any
`rt_sigprocmask` span in the clean one-hart rerun, and the CAS retry mechanism
is not confirmed (`attempts=1`, `retries=0` for every sampled
`IdentRef::to_cap()`).

The residual max span is no longer in signal refresh. The worst
`rt_sigprocmask` span in this rerun was `14.481ms`; its in-span markers show a
page-backed user copy-in materialization gap:

```text
span=0x1 dur=14.481ms
+0.437ms   debug.vm.user.copy_in.len=8
+0.520ms   debug.vm.user.resolve_page.phase=4
+0.524ms   debug.vm.user.pagebacked.phase=0
+14.064ms  debug.vm.user.pagebacked.phase=1
+14.163ms  debug.sigprocmask.read.after=8
+14.241ms  debug.sigprocmask.step.after=8
+14.308ms  debug.sigprocmask.write.after=8
+14.312ms  debug.sigprocmask.return=8
```

`debug.vm.user.pagebacked.phase=0 -> 1` brackets
`PageContainer::materialize_page(...)` in the user-access path. This is a VM
page-backed/cold-materialization tail on the user `set` copy-in, not the old
post-copy signal-refresh/owner-upgrade tail.

Fast-path/code changes made in this slice:

- `ThreadPayload` keeps a conservative `group_pending_summary`.
- `post_group_pending_signal()` posts the authoritative process bit and
  synchronizes every live thread's group hint and deliverable summary.
- `ast_check()` synchronizes all thread hints after clearing a group-pending
  bit.
- `step_clone_thread()` and the test sibling helper synchronize a new thread's
  hint immediately after attaching it to a process, so spawned siblings do not
  start from a false-zero group hint.
- `step_sigprocmask()` now calls
  `refresh_deliverable_signal_summary_with_payload(thread, &payload)` after the
  mask store, reusing the `PayloadCap<ThreadPayload>` it already acquired
  instead of re-locking `ThreadIdentity.payload` just to run the no-pending fast
  check.
- Weak-upgrade metrics now split slow upgrades into registry/meta/to-cap
  buckets, and `xtask observe names` carries stable labels for the new counters.

Updated conclusion: the original long-tail sample was real and the previous
lossless phase run correctly localized it to `select_next_signal()` owner
upgrade work. The suspected CAS-retry submechanism is not confirmed. The
effective mitigation is to avoid calling into that owner-process upgrade on the
common no-thread-pending/no-group-pending refresh path. After that fast path,
`rt_sigprocmask` p99 drops to the low hundreds of microseconds in the clean
one-hart rerun, and the remaining worst sample belongs to VM user-page
materialization.
