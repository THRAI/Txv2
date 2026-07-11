# tx-observe Demo Run Report

Date: 2026-06-28 04:28 CST

## Purpose

This run exercises the tx-observe pipeline end to end with the built-in
synthetic demo trace. The goal is not to measure kernel performance; it is to
prove that the current tooling can generate a txtrace-v0 file, validate the
binary framing, replay it to JSON, render a Perfetto file, derive analyzer
tables, run SQL, and produce a report-ready summary.

The demo shape is a synthetic `sys_read` path:

```text
L0 sys_read
  L2 drive.tx_subsystems::pipe::PipeReadOp
    L4 step.iteration_0 -> Yield(OnWaitSource)
    L3 yield.OnWaitSource
      L3 wake.notify
      L3 resume
    L4 step.iteration_1 -> Done(4096)
```

## Commands

```sh
mkdir -p target/observe-demo-20260628-042830
cargo xtask observe demo --output target/observe-demo-20260628-042830/trace.txtrace --with-yields
cargo xtask observe validate --file target/observe-demo-20260628-042830/trace.txtrace
cargo xtask observe replay --file target/observe-demo-20260628-042830/trace.txtrace --out json > target/observe-demo-20260628-042830/replay.ndjson
cargo xtask observe pftrace --file target/observe-demo-20260628-042830/trace.txtrace --output target/observe-demo-20260628-042830/trace.pftrace --names target/observe-demo-20260628-042830/trace.names.json
cargo xtask observe analyze --file target/observe-demo-20260628-042830/trace.txtrace --names target/observe-demo-20260628-042830/trace.names.json > target/observe-demo-20260628-042830/analyze.txt
cargo xtask observe analyze --file target/observe-demo-20260628-042830/trace.txtrace --names target/observe-demo-20260628-042830/trace.names.json --sql "select count(*) as spans, sum(dur) as total_ns, max(dur) as max_ns from spans" --sql-format csv > target/observe-demo-20260628-042830/sql-summary.csv
cargo xtask observe analyze --file target/observe-demo-20260628-042830/trace.txtrace --names target/observe-demo-20260628-042830/trace.names.json --parquet-dir target/observe-demo-20260628-042830/parquet > target/observe-demo-20260628-042830/parquet-summary.txt
```

## Artifacts

```text
target/observe-demo-20260628-042830/
  trace.txtrace        txtrace-v0 binary, 1560 bytes
  trace.names.json     generated name table
  replay.ndjson        12 replayed JSON records
  trace.pftrace        Perfetto binary trace, 488 bytes
  analyze.txt          text analyzer report
  sql-summary.csv      SQL output; includes xtask command echo plus CSV rows
  parquet/             derived Parquet tables
```

The Parquet directory contains:

```text
spans.parquet
counters.parquet
allocation_rows.parquet
sched_intervals.parquet
lock_rows.parquet
ds_method_rows.parquet
_tx_observe_parquet.json
```

## Validation Result

`cargo xtask observe validate` reported:

```text
txtrace v0: 1 hart, 16 slots/hart, 12 records, 0 framing errors
```

This proves that the demo trace was structurally valid: one hart ring, sixteen
slots, twelve records, and no framing damage.

## Replay Result

`replay.ndjson` contains twelve records:

| Seq | Kind | Level | Span | Parent | Meaning |
|---:|---|---|---|---|---|
| 0 | SpanBegin | Boundary | 0x1 | 0x0 | `sys_read` enter, sysno 63 |
| 1 | SpanBegin | Drive | 0x2 | 0x1 | `PipeReadOp` drive begin |
| 2 | SpanBegin | Step | 0x3 | 0x2 | first step begin |
| 3 | SpanEnd | Step | 0x3 | 0x0 | first step yielded |
| 4 | SpanBegin | Yield | 0x4 | 0x2 | wait-source yield begin |
| 5 | Instant | Yield | 0x0 | 0x2 | `wake.notify`, source 0x1234 |
| 6 | Instant | Yield | 0x0 | 0x4 | `resume`, generation 7 |
| 7 | SpanEnd | Yield | 0x4 | 0x0 | yield span end |
| 8 | SpanBegin | Step | 0x5 | 0x2 | second step begin |
| 9 | SpanEnd | Step | 0x5 | 0x0 | second step done, progress 4096 |
| 10 | SpanEnd | Drive | 0x2 | 0x0 | drive end |
| 11 | SpanEnd | Boundary | 0x1 | 0x0 | syscall exit, ret 4096 |

The parent links reconstruct the expected nesting:

```text
0x1 sys_read
  0x2 drive.tx_subsystems::pipe::PipeReadOp
    0x3 step.iteration_0
    0x4 yield.OnWaitSource
    0x5 step.iteration_1
```

## Analyzer Summary

The text analyzer reported:

```text
records=12 trace_records=12 window=11.0us
kinds={'SpanBegin': 5, 'SpanEnd': 5, 'Instant': 2}
unclosed_spans=0
```

Span totals:

| Span | Count | Duration |
|---|---:|---:|
| `sys_63` / `sys_read` | 1 | 11.0 us |
| `drive.tx_subsystems::pipe::PipeReadOp` | 1 | 9.0 us |
| `yield.OnWaitSource` | 1 | 3.0 us |
| `step.iteration_0` | 1 | 1.0 us |
| `step.iteration_1` | 1 | 1.0 us |

The derived Parquet summary agreed:

```text
spans=5 counters=0 allocation_rows=0 sched_intervals=0 lock_rows=0 ds_method_rows=0
```

The SQL query over analyzer views returned:

```csv
spans,total_ns,max_ns
5,25000,11000
```

`sum(dur)=25000 ns` is the sum of nested span durations, not elapsed wall time.
The outer trace window is 11 us; nested spans intentionally double-count time
when summed.

## Wait-Source Evidence

The analyzer reconstructed the synthetic wait-source event:

```text
by source: source=0x1234:mask=0x1=1
by task: task=66:gen=7=1
recent:
  ts=6000 wake.notify source=0x1234 mask=0x1 task=66 gen=7
```

This demonstrates that L3 producer-side wake records and resume records survive
the full pipeline. Because this is a compact synthetic trace, there are no
registration/drain/runnable/pick markers, so futex wake-latency attribution is
reported as incomplete:

```text
paths: undrained=1
incomplete=1
```

That is expected for this demo; a real QEMU/OSComp run with scheduler and wait
registration markers is required for full wake-latency attribution.

## Coverage Demonstrated

This run verified these surfaces:

- txtrace-v0 binary generation.
- names table generation.
- binary framing validation.
- replay to NDJSON.
- Perfetto `.pftrace` generation.
- span reconstruction.
- L0/L2/L3/L4 nesting.
- wait-source notify and resume decoding.
- analyzer text report.
- SQL query over analyzer views.
- Parquet export for derived tables.

## Not Covered

This was a synthetic demo trace, so it did not cover:

- QEMU live-drain `trace.rawrecords` capture.
- `runtime.json` completeness/loss accounting.
- real syscall workload timing.
- allocation rows.
- lock metrics.
- DS method metrics.
- scheduler intervals.
- VM/pagebacked cold-cache probes.
- L5/L6 substrate phase or mutation events.

## Warnings Observed

The run surfaced existing build warnings:

- `xtask/src/oscomp.rs` has an unused `std::collections::BTreeSet` import.
- `tx-trace-daemon` has dead-code warnings in Perfetto flow/span/track/writer
  support.

These warnings did not prevent trace generation, validation, replay, Perfetto
output, analyzer output, SQL, or Parquet export.

## Next Step

For a workload-backed report, run `cargo xtask observe oscomp-live --test <case>`
or `live-guest-mem` against a bounded QEMU workload, then report
`runtime.json` completeness, drained count, lost records, overwritten records,
and the same derived-table summaries.
