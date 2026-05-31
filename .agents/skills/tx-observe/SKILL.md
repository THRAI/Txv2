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
- Existing scheduler/process/wait-source/futex helpers should remain
  structured enough to correlate with syscall spans and sched tracks.

## Commands

- Decode: `cargo xtask observe replay --file <trace.txtrace> --out json`
- Perfetto: `cargo xtask observe pftrace --file <trace.txtrace> --output <out.pftrace>`
- Analyze: `cargo xtask observe analyze --file <trace.txtrace> [--names <names.json>]`
- Bundle: `cargo xtask observe bundle --file <trace.txtrace> --output-dir <dir>`
- Live host drain:
  `cargo xtask observe live-guest-mem --guest-mem <ram-file> --kernel <elf> --output-dir <dir>`
- Name table: `cargo xtask observe names --kernel <elf> --output <names.json>`

## Checks

- `cargo fmt --check`
- `cargo test -p tx-observe`
- `cargo test` in `tools/tx-trace-daemon`
- `python3 -m py_compile tools/tx-observe-analyze.py`
- focused subsystem tests for any new emit site
- `git diff --check`
- `cargo xtask progress validate`
- `cargo xtask lint docs` when docs/progress/skills change

For OSComp evidence, save the output directory and report `runtime.json`
`complete`, drained count, lost records, overwritten records, and repairs.
