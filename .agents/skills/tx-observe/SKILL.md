---
name: tx-observe
description: Use when implementing, debugging, or analyzing txKernel observation traces, tx-observe counters, Perfetto tracks, trace-daemon replay/live host drain, OSComp trace windows, page-fault/cold-cache probes, or allocation-track probes.
---

# tx-observe

Use this skill for observation work: kernel-side `tx-observe` emit points,
txtrace ABI changes, `tx-trace-daemon` replay/live capture, `cargo xtask
observe ...`, `tools/tx-observe-analyze.py`, and trace-backed OSComp debugging.

## Read First

- `docs/Txv3/08_OBSERVATION_v1.md`
- `docs/Txv3/08_OBSERVATION_SERIALIZATION_v0.md`
- `docs/Txv3/08_OBSERVATION_HOST_v0.md`
- `crates/tx-observe/src/lib.rs`
- `crates/tx-observe-types/src/{record,payload,header}.rs`
- `tools/tx-trace-daemon/src/{main,decode,replay}.rs`
- `tools/tx-trace-daemon/src/perfetto/{writer,track}.rs`
- `xtask/src/observe.rs`
- `tools/tx-observe-analyze.py`
- relevant `docs/progress/STATUS.md` entries and research notes

## Preserve

- Kernel emit paths stay bounded: no allocation, no blocking, no locks on the
  producer path, and fixed-size 80-byte records with 16-byte payloads unless a
  deliberate txtrace version bump is made.
- The semantic state remains authoritative; observe records are diagnostic
  evidence, not subsystem truth.
- Prefer stable `debug.<family>...` names plus numeric payload values. Let host
  tooling and `names.json` provide labels.
- For long OSComp windows, prefer `live-guest-mem` raw-record hot-path drain and
  post-stop decode. Treat `runtime.json` completeness/loss counters as the
  source of truth for whether the ring captured the whole window.
- Keep analysis binary-first. NDJSON is a compatibility/debug surface for
  grep/jq, not the preferred aggregation substrate. Use direct `.txtrace`
  ingest or live-drain `trace.rawrecords` when summarizing large captures.
- Derived tables are caches/views, not sources of truth. Rebuild them from the
  binary input when decoder logic changes, and keep cache keys tied to input
  content plus analyzer decoder version.
- `live-guest-mem` defaults to raw-record capture only. It should write
  `trace.rawrecords` and `runtime.json`; do not request `--finalize` for large
  analysis captures unless NDJSON or Perfetto output is specifically needed.
- Treat observe artifacts under `target/oscomp/custom-run` as disposable run
  outputs unless they have been copied into docs/progress or a named research
  note. Use `cargo xtask observe cleanup --dry-run` first, then `--yes` only
  after reviewing the candidate list.
- Do not turn high-volume probes on blindly. Add probes at semantic boundaries,
  phase transitions, allocation linearization points, or known long-tail
  suspects.

## Current Probe Families

- VM fault/cold-cache: `debug.vm.fault.*`,
  `debug.pagebacked.fault_step.*`, and `debug.pagebacked.file.*`.
  `cargo xtask observe analyze` reports these in `vm/pagebacked fault cache`.
- Allocation tracks: explicit DS lanes for zone slab growth, page frame/run
  allocation, VM recipe/private-page nodes, PageBacked cache entries,
  AddressSpace caps, and PageContainer caps. The analyzer reports these in
  `allocation tracks`; Perfetto renders them as `debug.alloc.*` tracks.
- Lock metrics: opt-in `tx_substrate::SpinMutex<T, LockMetricsOn>` samples
  `debug.lock.wait_ns`, `debug.lock.service_ns`, `debug.lock.response_ns`,
  `debug.lock.spins`, and `debug.lock.contended` on the explicit `debug.lock`
  track. The global `cfg(tx_lock_metrics)` gate removes all timing/emission
  fields when closed; the default `SpinMutex<T>` type uses `LockMetricsOff` so
  local opt-out remains a separate compile-time monomorphization.
- Subsystems may add narrower local cfg gates as aliases over the wrapped lock.
  VM uses `tx_lock_metrics_vm`: `cfg(tx_lock_metrics_vm)` selects
  `SpinMutex<T, LockMetricsOn>` for VM lock declarations, while the global
  `cfg(tx_lock_metrics)` still controls whether timing/emission code exists.
  Enable both for VM lock data:
  `RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm"`.
- Existing scheduler/process/wait-source/futex helpers should remain
  structured enough to correlate with syscall spans and sched tracks.
- Analyzer-derived tables currently include `spans`, `counters`,
  `allocation_rows`, `sched_intervals`, and `lock_rows`. SQL/Python analysis should query
  those typed tables; do not materialize a wide `records.parquet` on the local
  hot path.

## Commands

- Decode: `cargo xtask observe replay --file <trace.txtrace> --out json`
- Perfetto: `cargo xtask observe pftrace --file <trace.txtrace> --output <out.pftrace>`
- Analyze text:
  `cargo xtask observe analyze --file <trace.txtrace> [--names <names.json>]`
- Analyze live-drain raw records:
  `cargo xtask observe analyze --rawrecords <trace.rawrecords> [--names <names.json>]`
- Analyze with derived-table cache/Parquet:
  `cargo xtask observe analyze --file <trace.txtrace> --cache-dir <dir> --parquet-dir <dir>`
- Analyze with SQL:
  `cargo xtask observe analyze --file <trace.txtrace> --sql "select count(*), quantile_disc(dur, 0.99) from spans" --sql-format csv`
- Analyze with a Python script:
  `cargo xtask observe analyze --file <trace.txtrace> --python-file <script.py>`
- Bundle: `cargo xtask observe bundle --file <trace.txtrace> --output-dir <dir>`
- Integrated OSComp live observe:
  `cargo xtask observe oscomp-live [--test pthread|vm|stdio|regex|...] [--output-dir <dir>] [--python-file <script.py>]`
- Live host drain:
  `cargo xtask observe live-guest-mem --guest-mem <ram-file> --kernel <elf> --output-dir <dir>`
- Live host drain with legacy derived artifacts:
  `cargo xtask observe live-guest-mem --guest-mem <ram-file> --kernel <elf> --output-dir <dir> --finalize`
- Name table: `cargo xtask observe names --kernel <elf> --output <names.json>`
- Cleanup obsolete observe/custom-run artifacts:
  `cargo xtask observe cleanup [--root target/oscomp/custom-run] [--older-than-days N|--all] [--keep-glob PATTERN...] [--include-cache] [--dry-run|--yes]`

## Cleanup Notes

- `observe cleanup` defaults to `target/oscomp/custom-run`,
  `--older-than-days 7`, and dry-run output. It only deletes when `--yes` is
  passed.
- The cleanup scanner is intentionally conservative: it targets known observe
  and custom-run shapes such as `*-host`, `*-analysis`, `build-*`, `*-data`,
  `*-submit`, `.txtrace`, `.ndjson`, `.pftrace`, `.rawrecords`, `*-serial.txt`,
  `*-analyze.txt`, `*-report.md`, `*-names.json`, and `latest-*` pointers.
- Derived cache directories named `cache` or `*-cache` are skipped unless
  `--include-cache` is passed.
- `--older-than-days 0` means "older than this instant", so it will include
  same-day run evidence. A real delete with `--older-than-days 0` requires at
  least one `--keep-glob` unless `--all` is used intentionally. Add keep globs
  for run families that must survive, for example
  `--keep-glob '*full-libcbench-newworkflow*' --keep-glob 'wall-profile*'
  --keep-glob '*pthread-ds-smp4*'`.
- Before deleting, copy any durable evidence into `docs/progress/` or a
  research note. Progress docs, research notes, and source-controlled files are
  not cleanup targets.

## SQL/Python Notes

- SQL mode exposes DuckDB views named `records`, `repairs`, `spans`,
  `counters`, `allocation_rows`, `sched_intervals`, `lock_rows`, and `names`.
- For trace latency percentiles, prefer `quantile_disc(value, 0.50)` and
  `quantile_disc(value, 0.99)` so p50/p99 are observed samples rather than
  interpolated values.
- `--python-file` exports derived Parquet tables, then runs the script with
  `TX_OBSERVE_TABLE_DIR`, `TX_OBSERVE_SPANS_PARQUET`,
  `TX_OBSERVE_COUNTERS_PARQUET`, `TX_OBSERVE_ALLOCATION_ROWS_PARQUET`,
  `TX_OBSERVE_SCHED_INTERVALS_PARQUET`, `TX_OBSERVE_LOCK_ROWS_PARQUET`,
  `TX_OBSERVE_INPUT`, and optional `TX_OBSERVE_NAMES_JSON` environment
  variables.

## Checks

- `cargo fmt --check`
- `cargo test -p tx-observe`
- `cargo test` in `tools/tx-trace-daemon`
- `python3 -m py_compile tools/tx-observe-analyze.py tools/tests/test_tx_observe_analyze.py`
- `python3 -m unittest tools.tests.test_tx_observe_analyze`
- focused subsystem tests for any new emit site
- `git diff --check`
- `cargo xtask progress validate`
- `cargo xtask lint docs` when docs/progress/skills change

For OSComp evidence, save the output directory and report `runtime.json`
`complete`, drained count, lost records, overwritten records, and repairs.
