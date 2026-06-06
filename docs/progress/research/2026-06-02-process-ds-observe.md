# 2026-06-02: Process identity DS observe run

## Scope

Measured process identity/tree data-structure method costs in a full pthread
libcbench window. The run used only the process DS gates:

```sh
RUSTFLAGS="--cfg tx_ds_metrics --cfg tx_ds_metrics_process" \
  cargo xtask observe oscomp-live \
    --name process-ds-pthread-retry-20260602-200224 \
    --test pthread \
    --timeout 900
```

This isolates process DS method timing. It does not include zone or page
allocator DS rows, and it does not include process lock rows.

## Artifacts

- Run directory:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/`
- Serial log:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/serial.txt`
- Runtime:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/host/runtime.json`
- Raw records:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/host/trace.rawrecords`
- Report:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/report.json`
- Derived Parquet:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/`
- Name table:
  `target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/names.json`

The first attempt, `process-ds-pthread-20260602-195927`, failed before QEMU
with `No space left on device`. Only disposable incremental build caches were
removed before retrying; `target/oscomp/custom-run` evidence was preserved.

## Capture Quality

`runtime.json` reported a complete lossless live drain:

| field | value |
| --- | ---: |
| complete | true |
| raw_records | 6,923,021 |
| lost_records | 0 |
| overwritten_records | 0 |
| repairs | 0 |
| hart_count | 4 |
| ring_bytes | 2,097,152 |
| finalized | false |

Only hart 1 produced records in this run:

| hart | producer | consumer | lost | overwritten | framing_errors |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 0 | 0 | 0 | 0 | 0 |
| 1 | 6,923,020 | 6,923,020 | 0 | 0 | 0 |
| 2 | 0 | 0 | 0 | 0 | 0 |
| 3 | 0 | 0 | 0 | 0 | 0 |

## Benchmark Window

The serial log reached the full pthread libcbench group end and exited
successfully:

| body | seconds |
| --- | ---: |
| `b_pthread_createjoin_serial1` | 51.414451 |
| `b_pthread_createjoin_serial2` | 39.576037 |
| `b_pthread_create_serial1` | 38.369547 |
| `b_pthread_uselesslock` | 0.120356 |
| `b_pthread_createjoin_minimal1` | 50.426688 |
| `b_pthread_createjoin_minimal2` | 34.963404 |

Serial sentinels:

```text
#### OS COMP TEST GROUP START libcbench-musl ####
#### OS COMP TEST GROUP END libcbench-musl ####
txkernel:qemu-riscv64-virt:userspace:exited:0
txkernel:zone:summary:epoch=25034:guards=0:zones=32
```

## Derived Tables

| table | rows |
| --- | ---: |
| spans | 142,193 |
| counters | 5,137,102 |
| allocation_rows | 568,690 |
| sched_intervals | 0 |
| lock_rows | 0 |
| ds_method_rows | 50,089 |

`lock_rows=0` is expected for this run because `tx_lock_metrics` and
`tx_lock_metrics_process` were not enabled.

## Process DS Summary

The declared process DS slots total 50,089 observed method rows and 1.589s of
observed method time:

| metric | value |
| --- | ---: |
| rows | 50,089 |
| total_ns | 1,588,563,000 |
| total_ms | 1,588.563 |
| p50_ns | 29,000 |
| p95_ns | 64,000 |
| p99_ns | 121,000 |
| max_ns | 23,463,000 |

Interpretation: process identity/tree DS is visible, but it is not the main
pthread wall-time source. Nearly all process DS time in this trace is in four
high-volume operations: TID namespace unregister, TID namespace register,
thread detach, and thread attach.

## Full Declared Process DS Table

Percent is the share of total observed process DS method time in this trace.
Zero rows mean the observe slot was declared in `names.json` but the pthread
window did not execute that method under the enabled gates.

| method | n | total_ns | total_ms | pct_ds | p50_ns | p95_ns | p99_ns | max_ns |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.ds.process.pid_namespace.unregister_tid_number` | 12,501 | 558,146,000 | 558.146 | 35.135 | 39,000 | 88,000 | 153,000 | 1,603,000 |
| `debug.ds.process.pid_namespace.register_tid` | 12,501 | 485,440,000 | 485.440 | 30.558 | 33,000 | 74,000 | 124,000 | 23,463,000 |
| `debug.ds.process.threads.detach` | 12,501 | 421,694,000 | 421.694 | 26.546 | 30,000 | 61,000 | 107,000 | 3,710,000 |
| `debug.ds.process.threads.attach` | 12,501 | 118,710,000 | 118.710 | 7.473 | 8,000 | 16,000 | 33,000 | 609,000 |
| `debug.ds.process.pid_namespace.unregister_pid_number` | 6 | 1,225,000 | 1.225 | 0.077 | 48,000 | 980,000 | 980,000 | 980,000 |
| `debug.ds.process.children.snapshot` | 18 | 839,000 | 0.839 | 0.053 | 26,000 | 319,000 | 319,000 | 319,000 |
| `debug.ds.process.pid_namespace.resolve_pid_number_as` | 6 | 656,000 | 0.656 | 0.041 | 31,000 | 368,000 | 368,000 | 368,000 |
| `debug.ds.process.group_members.retain` | 6 | 509,000 | 0.509 | 0.032 | 45,000 | 281,000 | 281,000 | 281,000 |
| `debug.ds.process.threads.nth` | 7 | 371,000 | 0.371 | 0.023 | 26,000 | 240,000 | 240,000 | 240,000 |
| `debug.ds.process.children.retain` | 6 | 237,000 | 0.237 | 0.015 | 15,000 | 166,000 | 166,000 | 166,000 |
| `debug.ds.process.threads.snapshot` | 6 | 196,000 | 0.196 | 0.012 | 27,000 | 50,000 | 50,000 | 50,000 |
| `debug.ds.process.pid_namespace.register_pid` | 6 | 186,000 | 0.186 | 0.012 | 29,000 | 39,000 | 39,000 | 39,000 |
| `debug.ds.process.children.drain` | 6 | 142,000 | 0.142 | 0.009 | 7,000 | 103,000 | 103,000 | 103,000 |
| `debug.ds.process.threads.drain` | 6 | 93,000 | 0.093 | 0.006 | 7,000 | 58,000 | 58,000 | 58,000 |
| `debug.ds.process.group_members.attach` | 6 | 60,000 | 0.060 | 0.004 | 7,000 | 19,000 | 19,000 | 19,000 |
| `debug.ds.process.children.attach` | 6 | 59,000 | 0.059 | 0.004 | 7,000 | 18,000 | 18,000 | 18,000 |
| `debug.ds.process.children.detach` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.children.is_empty` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.children.len` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.group_members.count_live` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.group_members.detach` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.group_members.is_empty` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.group_members.len` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.group_members.snapshot_live` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.pid_namespace.register_pgrp` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.pid_namespace.register_session` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.pid_namespace.resolve_pid_number` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.pid_namespace.with_namespace` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.session_members.attach` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.session_members.is_empty` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.session_members.len` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.session_members.snapshot_live` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.threads.count` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.threads.find_by_tid` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |
| `debug.ds.process.threads.retain` | 0 | 0 | 0.000 | 0.000 | 0 | 0 | 0 | 0 |

## Top Span Context

The process DS total should be read against the syscall/span totals. Top spans
from the same trace:

| span | n | total_us | p50_ns | p99_ns | max_ns |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sys_wait4` | 6 | 215,102,438.000 | 38,377,573,000 | 51,564,957,000 | 51,564,957,000 |
| `sys_clone` | 12,507 | 33,706,666.000 | 2,162,000 | 9,385,000 | 99,810,000 |
| `sys_rt_sigprocmask` | 50,046 | 16,730,757.000 | 289,000 | 1,080,000 | 79,257,000 |
| `sys_futex` | 20,680 | 16,256,476.000 | 202,000 | 6,411,000 | 83,892,000 |
| `sys_munmap` | 10,001 | 14,138,700.000 | 1,145,000 | 5,404,000 | 42,439,000 |
| `sys_mmap` | 12,501 | 13,154,417.000 | 962,000 | 2,890,000 | 43,748,000 |
| `drive.FutexWaitOp` | 5,505 | 9,130,685.000 | 369,000 | 8,246,000 | 83,410,000 |
| `sys_mprotect` | 7,501 | 7,852,480.000 | 895,000 | 4,625,000 | 48,660,000 |
| `yield.OnWaitSource` | 2,678 | 7,225,271.000 | 2,187,000 | 8,954,000 | 83,072,000 |
| `sys_exit` | 12,501 | 6,918,338.000 | 471,000 | 1,576,000 | 46,653,000 |
| `step` | 8,201 | 1,669,389.000 | 98,000 | 955,000 | 18,421,000 |
| `sys_openat` | 6 | 67,111.000 | 6,107,000 | 20,090,000 | 20,090,000 |
| `drive.OpenOp` | 6 | 62,563.000 | 5,614,000 | 18,506,000 | 18,506,000 |
| `sys_writev` | 12 | 29,959.000 | 1,467,000 | 12,026,000 | 12,026,000 |
| `sys_exit_group` | 6 | 15,736.000 | 1,136,000 | 9,349,000 | 9,349,000 |
| `drive.OpenFileWriteOp` | 12 | 10,814.000 | 536,000 | 4,607,000 | 4,607,000 |
| `sys_clock_gettime` | 12 | 4,370.000 | 162,000 | 1,643,000 | 1,643,000 |
| `sys_ioctl` | 6 | 4,114.000 | 395,000 | 1,817,000 | 1,817,000 |
| `sys_set_tid_address` | 6 | 1,164.000 | 182,000 | 251,000 | 251,000 |

## Reproduction Queries

Table counts:

```sh
duckdb --no-stdin -csv -c "
SELECT 'spans' AS table_name, count(*) AS rows
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/spans.parquet')
UNION ALL SELECT 'counters', count(*)
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/counters.parquet')
UNION ALL SELECT 'allocation_rows', count(*)
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/allocation_rows.parquet')
UNION ALL SELECT 'sched_intervals', count(*)
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/sched_intervals.parquet')
UNION ALL SELECT 'lock_rows', count(*)
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/lock_rows.parquet')
UNION ALL SELECT 'ds_method_rows', count(*)
FROM read_parquet('target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/ds_method_rows.parquet');
"
```

Full declared process DS aggregate:

```sh
duckdb --no-stdin -csv -c "
WITH names AS (
  SELECT CAST(e.key AS UINTEGER) AS id, CAST(e.value AS VARCHAR) AS name
  FROM read_json_auto(
    'target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/names.json',
    maximum_object_size=10000000
  ), unnest(map_entries(name_table)) AS t(e)
),
agg AS (
  SELECT
    method_id,
    count(*) AS n,
    sum(value) AS total_ns,
    quantile_disc(value, 0.50) AS p50_ns,
    quantile_disc(value, 0.95) AS p95_ns,
    quantile_disc(value, 0.99) AS p99_ns,
    max(value) AS max_ns
  FROM read_parquet(
    'target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/ds_method_rows.parquet'
  )
  GROUP BY method_id
),
total AS (
  SELECT sum(total_ns) AS all_ns FROM agg
)
SELECT
  n.name AS method,
  coalesce(a.n, 0) AS n,
  coalesce(a.total_ns, 0) AS total_ns,
  round(coalesce(a.total_ns, 0) / 1000000.0, 3) AS total_ms,
  round(100.0 * coalesce(a.total_ns, 0) / NULLIF((SELECT all_ns FROM total), 0), 3) AS pct_ds,
  coalesce(a.p50_ns, 0) AS p50_ns,
  coalesce(a.p95_ns, 0) AS p95_ns,
  coalesce(a.p99_ns, 0) AS p99_ns,
  coalesce(a.max_ns, 0) AS max_ns
FROM names n
LEFT JOIN agg a ON a.method_id = n.id
WHERE n.name LIKE 'debug.ds.process.%'
ORDER BY total_ns DESC, method;
"
```

Raw process DS rows can be exported for per-event inspection with:

```sh
duckdb --no-stdin -csv -c "
WITH names AS (
  SELECT CAST(e.key AS UINTEGER) AS id, CAST(e.value AS VARCHAR) AS name
  FROM read_json_auto(
    'target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/names.json',
    maximum_object_size=10000000
  ), unnest(map_entries(name_table)) AS t(e)
)
SELECT
  d.ts,
  d.hart,
  n.name AS method,
  d.zone_id,
  d.metric_id,
  d.value AS duration_ns
FROM read_parquet(
  'target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224/analysis/parquet/ds_method_rows.parquet'
) d
JOIN names n ON n.id = d.method_id
WHERE n.name LIKE 'debug.ds.process.%'
ORDER BY d.ts;
"
```

## Caveats And Next Step

- This run has no `debug.ds.substrate.*` rows. It was intentionally scoped to
  `tx_ds_metrics_process`; compare zone allocation against
  `target/oscomp/custom-run/zone-current-pthread-minimal1-20260602-192224/` or
  rerun with `tx_ds_metrics_zone` and `tx_ds_metrics_page_allocator`.
- This run has no `lock_rows`. For direct method-work versus lock-service
  attribution, rerun with `tx_lock_metrics`, `tx_lock_metrics_process`,
  `tx_ds_metrics`, and `tx_ds_metrics_process` together.
- The single `register_tid` max of 23.463ms is a tail worth inspecting, but the
  aggregate signal is that process identity/tree DS is smaller than the VM,
  futex, mmap/munmap, mprotect, and syscall-span costs in the same trace.
