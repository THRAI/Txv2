# 2026-06-01: Process lock observe run

## Scope

Measured process lock timing with the wrapped `ProcessSpinMutex` path enabled:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_process" \
  cargo xtask build --target rv64-qemu
```

The focused image used the pthread-only libcbench payload and live-drained
`trace.rawrecords` through the binary-first analyzer.

## Artifacts

- Partial pthread capture:
  `target/oscomp/custom-run/process-lock-pthread-20260601-200855/`
- Raw drain:
  `target/oscomp/custom-run/process-lock-pthread-20260601-200855/host/trace.rawrecords`
- Runtime:
  `target/oscomp/custom-run/process-lock-pthread-20260601-200855/host/runtime.json`
- Parquet:
  `target/oscomp/custom-run/process-lock-pthread-20260601-200855/analysis/parquet/`
- Queueing CSV:
  `target/oscomp/custom-run/process-lock-pthread-20260601-200855/analysis/process-lock-queueing.csv`

`runtime.json` reported `raw_records=1000017`, `complete=true`,
`lost_records=0`, `overwritten_records=0`, and `repairs=0`.

## Result

The capture reached `b_pthread_createjoin_serial1` and stopped at the observe
threshold before the libcbench group-end sentinel. It is therefore a partial
pthread phase, not a complete libcbench window.

The derived tables contained:

- `spans=19227`
- `counters=603401`
- `allocation_rows=50818`
- `sched_intervals=0`
- `lock_rows=192777`

The lock table showed wait/service/response rows only; no spin or contended
rows were emitted in this window. The high-volume process locks were:

| lock | acquisitions | rho | S avg ns | predicted R ns | response p99 ns | response max ns |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.lock.process.identity.payload` | 57580 | 0.081049 | 46564.95 | 50671.84 | 201000 | 3333000 |
| `debug.lock.process.threads` | 2633 | 0.002515 | 31587.54 | 31667.17 | 130000 | 570000 |
| `debug.lock.process.pid_namespace` | 2626 | 0.005224 | 60493.53 | 60811.22 | 181000 | 8513000 |
| `debug.lock.process.payload.group_exit` | 1311 | 0.000528 | 11869.57 | 11875.83 | 39000 | 151000 |

Interpretation: this captured phase does not show meaningful process-lock
contention. The tails are dominated by service time inside critical sections,
not by queued spinning: `spins=0` and `contended=0` for every process lock row
in the CSV.

## Workflow notes

The first low-level manual QEMU attempt omitted `-accel tcg,thread=multi` and
panicked in the BSP timer smoke. Adding `-accel tcg,thread=multi` let the
partial pthread capture reach userspace.

The current checkout does not parse `tx.oscomp.observe_dump=0`; the active
kernel parser recognizes `tx.oscomp.observe_threshold=N`. A high-threshold
retry was attempted with:

```sh
tx.oscomp.observe=1 tx.oscomp.observe_threshold=100000000 tx.oscomp.groups=libcbench-musl
```

That retry panicked before userspace at `crates/tx-kernel/src/init.rs:1808`
with `BSP timer smoke deadline` (`armed.next_deadline_ns = None`). The failed
retry produced no raw records and should be treated as a complete-window
rerun blocker, not as lock evidence.

## Follow-up

Before a complete pthread lock window can be trusted, root-cause the
intermittent BSP timer-smoke failure in the manual shared-RAM QEMU path, or use
the integrated observe runner once the local checkout's `xtask observe
oscomp-live` dispatch is in sync with the progress notes.

## Redo After BSP Timer-Smoke Fix

After fixing the BSP timer-smoke race, reran the pthread process-lock check
with the fixed kernel and the same pthread-only libcbench image.

Live drain attached from process start still trapped before the libcbench group
started:

```text
scause=0x000000000000000c sepc=0xffffffc0813f6088 stval=0xffffffc0813f6088
```

`fault-decode` classified it as an S-mode instruction page fault in the
direct-map range, with the stack pointing at
`pmap::pt_node::alloc_pt_node_from_bag`. A no-drain baseline with the exact
same kernel and image reached pthread bodies, so this is live-drain specific
and not a timer-smoke or pthread-image failure.

A delayed-attach live-drain run avoided that pre-userspace trap:

- Run:
  `target/oscomp/custom-run/process-lock-pthread-delayed-drain-20260601-210402/`
- Serial:
  `target/oscomp/custom-run/process-lock-pthread-delayed-drain-20260601-210402/serial-file.txt`
- Raw records:
  `target/oscomp/custom-run/process-lock-pthread-delayed-drain-20260601-210402/host/trace.rawrecords`
- Parquet:
  `target/oscomp/custom-run/process-lock-pthread-delayed-drain-20260601-210402/analysis/parquet/`
- Queueing CSV:
  `target/oscomp/custom-run/process-lock-pthread-delayed-drain-20260601-210402/analysis/process-lock-queueing.csv`

The serial run completed the pthread group:

```text
#### OS COMP TEST GROUP START libcbench-musl ####
b_pthread_createjoin_serial1 (0)
b_pthread_createjoin_serial2 (0)
b_pthread_create_serial1 (0)
b_pthread_uselesslock (0)
b_pthread_createjoin_minimal1 (0)
b_pthread_createjoin_minimal2 (0)
#### OS COMP TEST GROUP END libcbench-musl ####
```

`fault-decode` found no trap lines in this completed delayed-drain run.

Drain quality caveat: because the host drain attached after the group started,
`runtime.json` reported `raw_records=6817228`, `lost_records=1282874`,
`overwritten_records=0`, and `complete=false`. The raw file was structurally
complete (`599916064` bytes, divisible by 88), but the sample is lossy and is
not a full accounting window.

Derived table counts from the lossy delayed-attach sample:

- `spans=115981`
- `counters=3897201`
- `allocation_rows=670284`
- `sched_intervals=0`
- `lock_rows=1294672`

Top process-lock queueing rows:

| lock | acquisitions | rho | S avg ns | predicted R ns | response p99 ns | response max ns |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.lock.process.identity.payload` | 377720 | 0.074157 | 43984.02 | 47507.00 | 212000 | 14407000 |
| `debug.lock.process.threads` | 21416 | 0.002530 | 26450.27 | 26517.36 | 112000 | 2961000 |
| `debug.lock.process.pid_namespace` | 21406 | 0.004838 | 50283.00 | 50527.47 | 166000 | 32728000 |
| `debug.lock.process.payload.group_exit` | 10691 | 0.000532 | 11008.98 | 11014.84 | 52000 | 1173000 |

Interpretation remains unchanged from the earlier partial lossless sample: in
the captured pthread rows, process locks do not show queueing contention.
`spins=0` and `contended=0` for every process lock row, while `rho` stays low
for the high-volume locks. The visible tails are service-time tails inside
critical sections, not spin-wait queueing.

Next step: root-cause the live-drain-specific pre-userspace instruction page
fault so future lock captures can attach at process start without losing the
early part of the trace.

## Live-Drain Start-At-Process Fix

Root cause work found two live-drain issues in the host path:

- `drain_live_once` trusted whatever bytes were visible at the exported ring
  symbol and wrote `consumer=producer` even before a per-hart ring header had
  the initialized shape.
- `live-guest-mem` always replayed raw records into NDJSON/PFTrace before
  writing `runtime.json`, which turned large pthread captures into multi-minute
  post-run finalization jobs even though binary analysis only needs
  `trace.rawrecords`.

The daemon now gates live reads and `consumer` writes on an initialized header:
the hart id must match the ring index, flags must be zero, `consumer <=
producer`, the visible window must fit the slot count, and `seq` must match the
published producer value (allowing the one-record in-flight transient). A unit
test covers the uninitialized-header case and proves the daemon leaves the
consumer field untouched. `live-guest-mem` now writes raw records and
`runtime.json` by default; `--finalize` opt-in preserves the legacy
NDJSON/PFTrace materialization path.

Verification:

- `cargo fmt --check`
- `cargo test -q --manifest-path tools/tx-trace-daemon/Cargo.toml live_drain`
- `cargo check -q -p xtask`
- Raw-only smoke:
  `target/oscomp/custom-run/live-raw-only-smoke-20260601/` wrote
  `trace.rawrecords` plus `runtime.json` and no NDJSON/PFTrace files
  (`finalized=false`).

Full process-start pthread live drain then completed without the earlier trap:

- Run:
  `target/oscomp/custom-run/process-lock-pthread-start-drain-rawonly-20260601-214243/`
- Serial:
  `target/oscomp/custom-run/process-lock-pthread-start-drain-rawonly-20260601-214243/serial-file.txt`
- Runtime:
  `complete=true`, `raw_records=7904411`, `lost_records=0`,
  `overwritten_records=0`
- Raw file:
  `target/oscomp/custom-run/process-lock-pthread-start-drain-rawonly-20260601-214243/host/trace.rawrecords`
  (`695588168` bytes, divisible by 88)
- Parquet:
  `target/oscomp/custom-run/process-lock-pthread-start-drain-rawonly-20260601-214243/analysis/parquet/`
- Queueing CSV:
  `target/oscomp/custom-run/process-lock-pthread-start-drain-rawonly-20260601-214243/analysis/process-lock-queueing.csv`

`fault-decode --elf ... --serial ... --all` found no trap lines. The pthread
group reached:

```text
#### OS COMP TEST GROUP START libcbench-musl ####
b_pthread_createjoin_serial1 (0)
b_pthread_createjoin_serial2 (0)
b_pthread_create_serial1 (0)
b_pthread_uselesslock (0)
b_pthread_createjoin_minimal1 (0)
b_pthread_createjoin_minimal2 (0)
#### OS COMP TEST GROUP END libcbench-musl ####
txkernel:qemu-riscv64-virt:userspace:exited:0
```

Derived table counts from the complete raw-only sample:

- `spans=133792`
- `counters=4528425`
- `allocation_rows=738338`
- `sched_intervals=0`
- `lock_rows=1514463`

Top process-lock queueing rows from the complete sample:

| lock | acquisitions | rho | service p50 ns | service p99 ns | response p99 ns | response max ns |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.lock.process.payload.fds` | 86 | 0.000038 | 29000 | 643000 | 644000 | 644000 |
| `debug.lock.process.payload.cwd` | 26 | 0.000011 | 37000 | 549000 | 571000 | 571000 |
| `debug.lock.process.identity.pgrp` | 33 | 0.000011 | 34000 | 506000 | 529000 | 529000 |
| `debug.lock.process.identity.payload` | 441932 | 0.090099 | 26000 | 97000 | 99000 | 15209000 |
| `debug.lock.process.pid_namespace` | 25026 | 0.004931 | 29000 | 55000 | 59000 | 2167000 |
| `debug.lock.process.threads` | 25037 | 0.003005 | 20000 | 38000 | 41000 | 304000 |
| `debug.lock.process.payload.group_exit` | 12501 | 0.000670 | 8000 | 12000 | 15000 | 127000 |

All process-lock rows still have `spins=0` and `contended=0`. The complete
capture therefore confirms the delayed-run interpretation: measured pthread
process-lock cost is dominated by critical-section service time, not spin-wait
queueing.
