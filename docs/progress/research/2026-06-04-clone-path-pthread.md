# clone path pthread probe

Date: 2026-06-04

## Scope

This pass pivoted from the now-characterized `mmap` / `munmap` VM spine to
`clone`, using a narrow observe gate for pthread lifecycle attribution.

The new cfg is `tx_clone_path_metrics`. It emits duration counters only when an
observer is active.

Touched probe sites:

- `sys_clone_oneshot`: parent saved-context load, `step_clone_thread`,
  parent-settid write, reactor submit, and total clone-thread syscall body.
- `step_clone_thread`: TID allocation, thread signing, TID registration,
  context seeding, clear-child-tid setup, thread attach, and total.
- `sign_thread`: payload allocation/sign/cap formation and identity signing.
- `submit_child_thread_now`: payload lookup/clone, reactor submission body,
  terminal-child drain, task registration, and total.

Names were registered in `xtask/src/observe.rs` and
`tools/tx-observe-analyze.py`.

## Verification

Code and tooling checks run for this probe:

- `python3 -m py_compile tools/tx-observe-analyze.py`
- `cargo check -p tx-shims -q`
- `cargo check -p tx-subsystems -q`
- `cargo check -p xtask -q`
- `RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics" cargo check -p tx-kernel -q`
- `rustfmt --edition 2021 crates/tx-shims/src/linux_syscall/proc.rs crates/tx-subsystems/src/process/execution.rs crates/tx-kernel/src/init/reactor_submit.rs xtask/src/observe.rs`
- `git diff --check -- Cargo.toml crates/tx-shims/src/linux_syscall/proc.rs crates/tx-subsystems/src/process/execution.rs crates/tx-kernel/src/init/reactor_submit.rs xtask/src/observe.rs tools/tx-observe-analyze.py docs/progress/STATUS.md docs/progress/research/2026-06-04-mmap-munmap-map-path.md`

The observe build itself also completed with:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_clone_path_metrics --cfg tx_sigprocmask_phase_metrics" \
  python3 tools/oscomp-observe-live.py \
  --test pthread-serial1 \
  --smp 1 \
  --output-dir target/oscomp/custom-run/clone-path-light-pthread-serial1-smp1-20260604
```

`cargo xtask fault-decode --target rv64-qemu --serial ... --summary` found no
trap lines in the SMP1 serial log; it exits with the expected "no
scause/sepc/stval trap lines found" message for a clean serial.

## Primary artifact

`target/oscomp/custom-run/clone-path-light-pthread-serial1-smp1-20260604`

Runtime quality:

- `hart_count=1`
- `raw_records=468,531`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`

The benchmark body completed:

- `b_pthread_createjoin_serial1 (0)`
- `time: 25.039005000`

Derived tables:

- `spans=32,523`
- `counters=193,420`
- allocation, lock, scheduler, and DS rows are empty by design in this narrow
  clone/sigprocmask profile.

## Syscall totals

From `analysis/parquet/spans.parquet`, joined by `name_id`:

| span | n | total | avg | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `sys_clone` | 2501 | 3.576s | 1429.822us | 1397us | 1601us | 2136us | 11.485ms |
| `sys_rt_sigprocmask` | 10007 | 2.020s | 201.863us | 192us | 249us | 354us | 22.112ms |
| `sys_munmap` | 2500 | 1.455s | 582.073us | 571us | 674us | 882us | 2.865ms |
| `sys_mprotect` | 2500 | 1.224s | 489.573us | 478us | 547us | 726us | 3.264ms |
| `sys_futex` | 5000 | 1.206s | 241.199us | 285us | 383us | 517us | 5.797ms |
| `sys_mmap` | 2500 | 0.947s | 378.674us | 369us | 423us | 574us | 1.639ms |
| `sys_exit` | 2500 | 0.887s | 354.722us | 340us | 410us | 553us | 2.057ms |

`sys_clone` remains the top non-parent-wait syscall. `rt_sigprocmask` is still
a useful fixed-cost floor: it does little semantic work, but costs about 192us
p50 in this profile.

## Clone path split

From `analysis/parquet/counters.parquet`:

| counter | n | total | avg | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.clone_path.sys_clone.total_ns` | 2500 | 3.259s | 1303.648us | 1275us | 1465us | 1978us | 11.337ms |
| `debug.clone_path.sys_clone.reactor_submit_ns` | 2500 | 1.790s | 716.200us | 694us | 824us | 1168us | 10.658ms |
| `debug.clone_path.child_submit.total_ns` | 2501 | 1.621s | 648.271us | 628us | 750us | 1024us | 10.556ms |
| `debug.clone_path.sys_clone.step_thread_ns` | 2500 | 1.159s | 463.511us | 451us | 522us | 688us | 1.820ms |
| `debug.clone_path.step_clone_thread.total_ns` | 2500 | 1.126s | 450.252us | 438us | 507us | 673us | 1.797ms |
| `debug.clone_path.child_submit.terminal_drain_ns` | 2501 | 0.783s | 313.134us | 298us | 354us | 597us | 10.028ms |
| `debug.clone_path.child_submit.reactor_with_ns` | 2501 | 0.553s | 221.162us | 216us | 255us | 384us | 952us |
| `debug.clone_path.step_clone_thread.sign_thread_ns` | 2500 | 0.542s | 216.820us | 208us | 238us | 388us | 1.194ms |
| `debug.clone_path.sign_thread.total_ns` | 2501 | 0.500s | 199.851us | 191us | 219us | 368us | 1.172ms |
| `debug.clone_path.step_clone_thread.attach_ns` | 2500 | 0.188s | 75.066us | 73us | 81us | 159us | 406us |
| `debug.clone_path.sign_thread.payload_sign_ns` | 2501 | 0.174s | 69.732us | 65us | 75us | 171us | 606us |
| `debug.clone_path.sign_thread.identity_sign_ns` | 2501 | 0.145s | 58.161us | 55us | 63us | 125us | 479us |

Interpretation:

- The largest clone-only bucket is reactor submission, not TID allocation or
  payload/identity signing.
- Within child submission, terminal-child drain is the largest subphase at
  `0.783s` total / `298us` p50. That means clone is paying return-path cleanup
  work while submitting the next child.
- `step_clone_thread` is still material at `1.126s` total, mostly signing
  (`0.500s`) plus attach/register/seed/clear-child-tid.
- The hand-instrumented clone body (`3.259s`) is close to but below syscall
  span total (`3.576s`), leaving about `0.317s` outside these explicit buckets
  across the run.

## Sigprocmask floor

From the same SMP1 run:

| counter | n | total | avg | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns` | 10007 | 0.323s | 32.263us | 31us | 37us | 57us | 304us |
| `debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns` | 7506 | 0.184s | 24.567us | 24us | 30us | 39us | 465us |
| `debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns` | 10007 | 0.101s | 10.048us | 10us | 12us | 15us | 179us |
| `debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns` | 10007 | 0.077s | 7.664us | 7us | 10us | 12us | 127us |
| `debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns` | 10007 | 0.072s | 7.228us | 7us | 8us | 11us | 131us |
| `debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns` | 7506 | 0.052s | 6.899us | 7us | 8us | 11us | 150us |

The inner sigprocmask payload work totals around `0.81s`, while the syscall span
totals `2.020s`. That leaves a broad shared syscall/trap/dispatch floor outside
the payload lock-service counters. Clone is not just this shared floor: its
explicit clone body accounts for most of the `sys_clone` span.

## Secondary sanity run

An earlier run accidentally used the workflow default `--smp 4`:

`target/oscomp/custom-run/clone-path-light-pthread-serial1-20260604`

It is also complete/lossless (`raw_records=468,800`, zero lost/overwritten/
repairs) and produced the same decomposition shape. Because the current
measurement lane is SMP1 until the SMP4 observe-ring fix lands elsewhere, this
run is recorded only as secondary context.

## Invalid earlier capture

The heavier lifecycle-gated attempt:

`target/oscomp/custom-run/clone-path-pthread-serial1-20260604`

is invalid evidence. It stalled after `terminal_drained=1`, produced
`trace.rawrecords` size 0, and had no trap lines in serial. The lightweight
`tx_clone_path_metrics` gate avoided that failure mode.

## Next step

The next pthread optimization lane should inspect child reactor submission and
terminal-child drain, not clone's TID allocator. A focused follow-up should
split `submit_child_thread_now` / `BOOT_REACTOR.with` internals into:

- terminal drain volume and reclaimed-child count
- task construction and task-table insert
- runnable enqueue / scheduler submit
- any EBR or zone reclaim performed on the clone return path

`rt_sigprocmask` remains useful as a shared syscall-floor sentinel. If that
floor is addressed, it helps the whole pthread lifecycle spine, but the current
clone-specific headroom is primarily in reactor submission and child cleanup.
