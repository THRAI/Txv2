# sigprocmask payload fast path

Date: 2026-06-04

## Scope

This pass revisited `rt_sigprocmask` after pthread SMP1 attribution showed the
syscall's remaining cost was mostly service time, not lock waiting. The prior
weak-upgrade scout was preserved, but the current code split the next concrete
cost: reopening `thread.payload` in the direct syscall lane even though
`thread_future` already holds the current `PayloadCap<ThreadPayload>`.

## Changes

- `refresh_deliverable_signal_summary_with_payload` now refreshes the
  deliverable bit from local pending bits plus the cached group-pending summary.
  It no longer calls `select_next_signal` during a mask update, so this refresh
  does not require upgrading the owning process weak handle just to maintain a
  summary hint.
- Added `step_sigprocmask_with_payload`, which applies the mask update using an
  already-resolved thread payload. The existing `step_sigprocmask` remains as
  the fallback API and delegates to the new helper after its normal payload
  lookup.
- Added `dispatch_thread_payload_aspace_oneshot` and wired
  `run_thread` to use it for `NR_RT_SIGPROCMASK`. The old
  `dispatch_thread_aspace_oneshot` remains available for callers that only have
  a thread identity and address space.
- Added `dispatch_direct_trap_payload_oneshot` and wired the direct trap
  syscall lane through it. The first pthread observe run proved this was the
  remaining old route: direct trap syscalls resolved the current payload but
  still called `dispatch_direct_trap_oneshot`, which routed
  `NR_RT_SIGPROCMASK` back through the identity/aspace-only helper.
- The payload-aware syscall path also handles query-only `set == NULL` through
  the supplied payload, avoiding a payload-cap clone for mask reads.
- `tools/oscomp-observe-live.py` now uses `sys.executable` for its nested
  Python stages and invokes `tools/tx-observe-analyze.py` directly. This avoids
  the host `python3` wrapper hang seen with Homebrew Python and keeps the
  workflow stable when launched with `uv run python`.
- `cargo xtask observe analyze` and `cargo xtask observe oscomp-live` now prefer
  `uv run python` when `uv` is available, with an explicit `python3` fallback.

## Verification

Red/green checks:

- `cargo test -p tx-shims dispatch_thread_payload_aspace_oneshot_reuses_resolved_payload -- --nocapture`
  first failed with unresolved
  `dispatch_thread_payload_aspace_oneshot`, then passed after the new lane was
  added. The first full-package filtered run printed the passing unit test but
  then stalled in filtered integration-test cleanup, so the final recorded
  command used `--lib`.

Focused passing checks:

- `cargo test -p tx-shims --lib dispatch_thread_payload_aspace_oneshot_reuses_resolved_payload -- --nocapture`
- `cargo test -p tx-subsystems --lib sigprocmask_ -- --nocapture`
- `cargo test -p tx-subsystems --lib ast_check_ -- --nocapture`
- `cargo check -p tx-shims -q`
- `cargo check -p tx-subsystems -q`
- `RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics" cargo check -p tx-kernel -q`
- `cargo check -p xtask -q`
- `uv run python tools/tests/test_oscomp_observe_live.py`
- `uv run python -m py_compile tools/oscomp-observe-live.py tools/tests/test_oscomp_observe_live.py tools/tx-observe-analyze.py`
- `uv run python tools/oscomp-observe-live.py --help`
- `cargo xtask observe oscomp-live --test pthread-serial1 --smp 1 --dry-run --output-dir target/oscomp/custom-run/test-xtask-dry-run`
- `cargo xtask observe analyze --rawrecords target/oscomp/custom-run/sigprocmask-payload-fast-pthread-serial1-smp1-20260604-rerun/host/trace.rawrecords --names target/oscomp/custom-run/sigprocmask-payload-fast-pthread-serial1-smp1-20260604-rerun/names.json --cache-dir target/oscomp/custom-run/test-xtask-analyze-cache --parquet-dir target/oscomp/custom-run/test-xtask-analyze-parquet --no-sort`
- `rustfmt --edition 2021 crates/tx-shims/src/linux_syscall/mod.rs crates/tx-shims/src/linux_syscall/signal.rs crates/tx-shims/src/linux_syscall/tests.rs crates/tx-kernel/src/thread_future.rs crates/tx-subsystems/src/thread_runtime/execution.rs crates/tx-subsystems/src/signal/mod.rs crates/tx-subsystems/src/signal/tests/delivery.rs`
- `git diff --check -- crates/tx-kernel/src/thread_future.rs crates/tx-shims/src/linux_syscall/mod.rs crates/tx-shims/src/linux_syscall/signal.rs crates/tx-shims/src/linux_syscall/tests.rs crates/tx-subsystems/src/signal/mod.rs crates/tx-subsystems/src/signal/tests/delivery.rs crates/tx-subsystems/src/thread_runtime/execution.rs`

## Measurement

Two pthread SMP1 observe runs were captured after fixing the host wrapper:

- `target/oscomp/custom-run/sigprocmask-payload-fast-pthread-serial1-smp1-20260604`
  was diagnostic. It completed cleanly, but still showed
  `payload_lock_wait` n=10007 total=78.050ms, `payload_lock_held` n=10007
  total=359.591ms, and `payload_cap_clone` n=10007 total=100.095ms. That
  falsified the assumption that the `thread_future` lane covered all pthread
  `rt_sigprocmask` calls.
- `target/oscomp/custom-run/sigprocmask-payload-fast-pthread-serial1-smp1-20260604-rerun`
  is the post direct-trap fix result. Runtime quality:
  `complete=true`, raw records `438868`, lost `0`, overwritten `0`, repairs
  `0`; analyzer exported `spans=32660` and `counters=163404`.

Against the pre-fix reference
`target/oscomp/custom-run/clone-path-light-pthread-serial1-smp1-20260604`:

| Metric | Reference | Rerun |
| --- | ---: | ---: |
| `sys_rt_sigprocmask` total | 2.020s | 1.774s |
| `sys_rt_sigprocmask` p50 | 192us | 155us |
| `sys_rt_sigprocmask` p99 | 354us | 476us |
| `payload_lock_wait` | 72.326ms / n=10007 | 0 / n=0 |
| `payload_lock_held` | 322.860ms / n=10007 | 0 / n=0 |
| `payload_cap_clone` | 100.554ms / n=10007 | 0 / n=0 |
| `mask_compute` | 76.695ms / n=10007 | 102.368ms / n=10007 |
| `mask_store` | 51.781ms / n=7506 | 59.520ms / n=7506 |
| `refresh` | 184.401ms / n=7506 | 224.036ms / n=7506 |

The body time in the rerun was `29.900269000`, slower than the earlier
reference's broader pthread score, so the run should not be read as a whole-body
speedup. The phase counters are the source of truth for this slice: the old
payload reopen/clone work disappeared from direct `rt_sigprocmask`, while the
real mask compute/store/refresh work remains and carries normal run variance
plus probe cost.

## Next step

Treat `rt_sigprocmask` as no longer dominated by the unnecessary payload reopen
path on SMP1. The remaining pthread lane should focus on the larger clone,
VM map/unmap, futex, and scheduler/service-in-critical-section costs; recheck
this path under SMP4 after the separate SMP4 observe-ring fix lands.

## Detail marker follow-up

The next attribution pass needs more than the coarse
`debug.lock_service.thread.payload.sigprocmask.*` counters: those counters now
show the payload reopen path is gone, but the successful direct-trap syscall
still has about 1.77s of `sys_rt_sigprocmask` span time with only about 386ms
explained by the existing phase rows. To make that gap falsifiable, the detailed
markers are now behind a new local cfg, `tx_sigprocmask_detail_metrics`.

Marker families:

- `debug.trap.direct_sigprocmask.*`: direct-trap route time for payload lookup,
  context lookup, preconditions, direct dispatch, trap-frame writeback, optional
  wake handoff, total direct path time, and fast-path decline reasons
  (`no_payload`, `no_active_request`, `no_thread`, `no_process`, `no_aspace`,
  `precondition_failed`, `unsupported`).
- `debug.sigprocmask.detail.*`: shim body time for decode, route shape,
  bad-size/bad-how exits, user read, mask step/query, user write, errors, and
  total shim time.
- The older sampled `debug.sigprocmask.*` breadcrumbs are now gated by the same
  cfg so they do not inflate normal pthread captures.

Suggested focused run:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics --cfg tx_sigprocmask_detail_metrics" \
uv run python tools/oscomp-observe-live.py \
  --test pthread-serial1 \
  --smp 1 \
  --output-dir target/oscomp/custom-run/sigprocmask-detail-pthread-serial1-smp1-20260604
```

Verification for the marker patch:

- `rustfmt --edition 2021 crates/tx-shims/src/linux_syscall/signal.rs crates/tx-kernel/src/trap.rs xtask/src/observe.rs`
- `cargo check -p tx-shims -q`
- `RUSTFLAGS="--cfg tx_sigprocmask_detail_metrics --cfg tx_sigprocmask_phase_metrics" cargo check -p tx-shims --lib -q`
- `cargo check -p tx-kernel -q`
- `RUSTFLAGS="--cfg tx_sigprocmask_detail_metrics --cfg tx_sigprocmask_phase_metrics --cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics" cargo check -p tx-kernel -q -j1`
- `cargo check -p xtask -q`

A cfg-enabled full-package `cargo check -p tx-shims` was stopped after it spent
minutes in the unrelated `dump-kernel-user-layouts` bin target. The library
target, which contains the changed signal implementation, passed with the same
cfgs.

## Detail capture result

Focused run:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics --cfg tx_sigprocmask_detail_metrics" \
uv run python tools/oscomp-observe-live.py \
  --test pthread-serial1 \
  --smp 1 \
  --output-dir target/oscomp/custom-run/sigprocmask-detail-pthread-serial1-smp1-20260604-194302
```

Runtime quality:

- `runtime.json`: `complete=true`, `raw_records=553949`, lost `0`,
  overwritten `0`, repairs `0`.
- Serial body: `b_pthread_createjoin_serial1` time `33.508882000`.
- This is a probe-on attribution run, not a clean timing baseline; the detail
  markers intentionally add work on every `rt_sigprocmask`.

Observed `sys_rt_sigprocmask` span:

| metric | value |
| --- | ---: |
| samples | 10007 |
| total | 2.346s |
| avg | 234us |
| p50 | 204us |
| p99 | 690us |
| max | 27.289ms |

Shim detail counters:

| counter | samples | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `debug.sigprocmask.detail.total_ns` | 10007 | 1.858s | 161us | 574us | 26.555ms |
| `debug.sigprocmask.detail.step_ns` | 10007 | 0.797s | 77us | 271us | 6.264ms |
| `debug.sigprocmask.detail.read_user_ns` | 10007 | 0.476s | 39us | 167us | 26.059ms |
| `debug.sigprocmask.detail.write_user_ns` | 5002 | 0.184s | 32us | 132us | 4.073ms |
| `debug.sigprocmask.detail.decode_ns` | 10007 | 0.031s | 3us | 10us | 284us |

Direct trap route counters:

| counter | samples | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `debug.trap.direct_sigprocmask.total_ns` | 10007 | 4.475s | 397us | 1.273ms | 28.074ms |
| `debug.trap.direct_sigprocmask.dispatch_ns` | 10007 | 2.492s | 216us | 733us | 27.570ms |
| `debug.trap.direct_sigprocmask.context_ns` | 10007 | 0.775s | 71us | 282us | 4.678ms |
| `debug.trap.direct_sigprocmask.payload_ns` | 10007 | 0.377s | 38us | 135us | 738us |
| `debug.trap.direct_sigprocmask.precondition_ns` | 10007 | 0.283s | 25us | 111us | 1.087ms |
| `debug.trap.direct_sigprocmask.writeback_ns` | 10007 | 0.071s | 6us | 29us | 272us |

Route and fallback checks:

- `debug.sigprocmask.detail.route=3` for 5005 calls: payload supplied, set
  present, no oldset writeback.
- `debug.sigprocmask.detail.route=7` for 5002 calls: payload supplied, set
  present, oldset writeback present.
- No direct-fallback reason counters fired (`no_payload`,
  `no_active_request`, `no_thread`, `no_process`, `no_aspace`,
  `precondition_failed`, or `unsupported`).
- The old payload reopen rows remain absent in this detail capture:
  `payload_lock_wait`, `payload_lock_held`, `payload_cap_clone`, and
  `payload_missing` all have no rows. Remaining legacy payload-service rows are
  the real update work: `refresh=214.991ms`, `mask_compute=102.494ms`,
  `mask_store=57.268ms`.

Readout:

The old weak/payload reopen caveat is closed for SMP1: every measured
`rt_sigprocmask` call used the payload-aware direct route and none reopened the
payload lock/cap path. The residual is split between real shim work
(`step_ns`, user read/write) and outer direct-route work, especially context
lookup (`current_userspace_thread_identity`, `upgrade_owner_proc`,
`aspace_cap`) plus payload/precondition checks. The long max tail follows
`read_user_ns`, so any remaining rare tail should be checked against VM
user-copy/pagebacked materialization before attributing it to signal logic.

## Workflow guard follow-up

The live observe wrapper now rejects parallel runs against the same output
directory before image/build/QEMU/analyze start. It creates and holds a
non-blocking advisory `.observe-live.lock` file under the resolved output
directory for the duration of a non-dry-run workflow. The file is intentionally
left in place after unlock so later contenders use the same inode; its contents
record the owning PID and start timestamp for diagnostics.

Verification:

- `uv run python -m py_compile tools/oscomp-observe-live.py tools/tests/test_oscomp_observe_live.py`
- `uv run python tools/tests/test_oscomp_observe_live.py`
- `git diff --check -- tools/oscomp-observe-live.py tools/tests/test_oscomp_observe_live.py`

## Direct context split result

The first detail capture showed `debug.trap.direct_sigprocmask.context_ns` at
`0.775s` total, so the next probe split that parent bracket into the three
concrete calls in `try_direct_trap_syscall`:

- `current_userspace_thread_identity(hart)` → `context_thread_ns`
- `thread.upgrade_owner_proc()` → `context_owner_ns`
- `process.aspace_cap()` → `context_aspace_ns`

Focused run:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics --cfg tx_sigprocmask_detail_metrics" \
uv run python tools/oscomp-observe-live.py \
  --test pthread-serial1 \
  --smp 1 \
  --output-dir target/oscomp/custom-run/sigprocmask-context-split-pthread-serial1-smp1-20260604-211952
```

Runtime quality:

- `runtime.json`: `complete=true`, `raw_records=583345`, lost `0`,
  overwritten `0`, repairs `0`.
- Serial body: `b_pthread_createjoin_serial1` time `34.505337000`.
- This is still probe-on attribution, not a clean benchmark score.

Direct context split:

| counter | samples | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `debug.trap.direct_sigprocmask.context_ns` | 10007 | 1.114s | 101us | 374us | 9.724ms |
| `debug.trap.direct_sigprocmask.context_aspace_ns` | 10007 | 0.366s | 33us | 143us | 7.964ms |
| `debug.trap.direct_sigprocmask.context_owner_ns` | 10007 | 0.335s | 31us | 128us | 720us |
| `debug.trap.direct_sigprocmask.context_thread_ns` | 10007 | 0.163s | 14us | 64us | 1.126ms |

Nearby route buckets in the same run:

| counter | samples | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `debug.trap.direct_sigprocmask.total_ns` | 10007 | 4.950s | 437us | 1.388ms | 35.688ms |
| `debug.trap.direct_sigprocmask.dispatch_ns` | 10007 | 2.579s | 224us | 745us | 27.821ms |
| `debug.trap.direct_sigprocmask.payload_ns` | 10007 | 0.381s | 38us | 138us | 1.198ms |
| `debug.trap.direct_sigprocmask.precondition_ns` | 10007 | 0.312s | 25us | 104us | 16.034ms |

Readout:

The context bucket is distributed rather than dominated by one call. The
largest total is `process.aspace_cap()`, which locks the process payload and
clones the address-space cap; `thread.upgrade_owner_proc()` is close behind,
and `current_userspace_thread_identity` is cheaper but still nonzero because it
locks/clones the per-hart slot. The split does not point to a narrow
sigprocmask-only fix. Any reduction here is a broader fast-syscall context
cache or direct-route API change, while the larger pthread gains should still
come from clone/VM map-unmap or VM user-copy tails.

Verification:

- `rustfmt --edition 2021 crates/tx-kernel/src/trap.rs xtask/src/observe.rs`
- `RUSTFLAGS="--cfg tx_sigprocmask_detail_metrics --cfg tx_sigprocmask_phase_metrics --cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics" cargo check -p tx-kernel -q -j1`
- `cargo check -p xtask -q`
- `git diff --check -- crates/tx-kernel/src/trap.rs xtask/src/observe.rs`

## Cleanup and source attribution follow-up

Manual observe cleanup was needed because this checkout's
`cargo xtask observe` binary does not expose the cleanup subcommand described in
the current observe skill. The pre-cleanup `target/oscomp/custom-run` footprint
was `7.0G`. Removed only explicitly named stale or unrelated run directories:

- `target/oscomp/custom-run/cyclic-iozone-20260604`
- `target/oscomp/custom-run/recipe-bplus-narrow2-pthread-smp1-20260603-continued`
- `target/oscomp/custom-run/whole-suite-chokepoints-20260604`
- `target/oscomp/custom-run/whole-suite-regex-20260604`
- `target/oscomp/custom-run/test-xtask-analyze-cache`
- `target/oscomp/custom-run/test-xtask-analyze-parquet`

The post-cleanup footprint is `3.0G`. The same-day pthread/sigprocmask,
clone-path, and map-path artifacts remain available for the current
investigation, including the clean context split capture at
`target/oscomp/custom-run/sigprocmask-context-split-pthread-serial1-smp1-20260604-211952`.

Source-level attribution for the remaining `rt_sigprocmask` cost:

- Direct-trap payload/context recovery lives in
  `crates/tx-kernel/src/trap.rs::try_direct_trap_syscall`. The measured
  `context_thread_ns`, `context_owner_ns`, and `context_aspace_ns` brackets map
  directly to `current_userspace_thread_identity(hart)`,
  `thread.upgrade_owner_proc()`, and `process.aspace_cap()`. The clean split
  showed no single dominant lookup: the costs are distributed, with
  `aspace_cap()` highest by total but not high enough to justify a
  sigprocmask-only rewrite.
- Direct dispatch maps to
  `tx_shims::linux_syscall::dispatch_direct_trap_payload_oneshot(...)` in the
  same function. Its child shim route is
  `crates/tx-shims/src/linux_syscall/signal.rs::sys_rt_sigprocmask_impl`.
- The shim's largest concrete buckets are the canonical user-copy calls
  `bootstrap_read_user::<u64>(aspace, set_ptr)` and
  `bootstrap_write_user::<u64>(aspace, oldset_ptr, prev_mask)`, plus the mask
  update. The long max tail followed `read_user_ns`, so rare residual tails
  should be checked against VM user-copy/pagebacked materialization before
  blaming signal logic.
- The actual mask update is
  `crates/tx-subsystems/src/thread_runtime/execution.rs::step_sigprocmask_with_payload`:
  one atomic load/compute, an optional atomic store, and
  `refresh_deliverable_signal_summary_with_payload`. The older payload reopen
  path in `step_sigprocmask` did not fire in the latest direct-route capture.

Readout: `rt_sigprocmask` no longer has an obvious narrow fix on SMP1. The
remaining milliseconds are spread across fast-syscall context recovery,
VM-backed user-copy, and small real signal-state work. A broad fast-syscall
context cache could reduce the distributed context cost, but the larger pthread
headroom remains clone/VM map-unmap or VM user-copy attribution rather than more
signal-specific work.
