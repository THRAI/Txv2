---
date: 2026-05-01
topic: "RV64 trapframe panic logs and fault-decode parser"
status: complete
---

# RV64 Trapframe Panic Logs and Fault-Decode Parser

## Decision

RV64 QEMU terminating traps now print the saved trap frame after the scalar
`scause`/`sepc`/`stval` summary. The frame dump uses a stable `trapframe:`
block with all integer registers (`x0` through `x31`) plus saved `scause`,
`sepc`, `stval`, and `sstatus`.

`cargo xtask fault-decode --serial` now accepts both the old one-line
`scause`/`sepc`/`stval` format and the richer multi-line `trapframe:` block.
The parser attaches the frame to the preceding trap record and avoids treating
the CSR line inside the frame as a second trap.

The QEMU sentinel runner now recognizes RV64 trap summaries as an immediate
sentinel failure mode. On RV64 QEMU, the error report appends
`fault-decode --serial` output for the captured serial log, so a `sepc` trap
failure carries symbolization and trapframe context without a second manual
command. The annotation path is routed through a small injectable runner helper
so unit tests cover both successful embedded decode output and decoder-failure
reporting without needing a timed QEMU failure.

## Boundary

- This is diagnostic output and host-side parsing only. It does not change trap
  policy, VM fault handling, syscall dispatch, signal delivery, or userspace
  return.
- The scalar summary remains in the log so existing serial snippets and manual
  `fault-decode --scause --sepc --stval` workflows keep working.
- Automatic QEMU annotation is RV64 QEMU only until LA64 has an equivalent
  decoder.

## Verification

- `cargo test -p xtask`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask ci`
- `git diff --check`
