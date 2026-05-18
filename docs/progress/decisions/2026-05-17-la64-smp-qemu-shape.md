# LA64 QEMU SMP lane shape

Date: 2026-05-17

Decision: keep the normal `cargo xtask qemu --target la64-qemu` default at
`-smp 4`, keep the OSComp local lane at `-smp 1`, and make the normal QEMU
lane overrideable with `--smp N`.

Rationale:

- The normal LA64 smoke lane is a platform bring-up lane. Running it with four
  vCPUs preserves coverage for IOCSR mailbox bring-up, AP park/release,
  shootdown, IPI, and AP reactor-loop sentinels.
- The OSComp lane is intentionally shaped like the contest runner and therefore
  stays at one vCPU.
- `cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --smp 1`
  now provides a direct single-vCPU reproduction path without changing the
  default SMP coverage lane.

Validation on 2026-05-17: both the default LA64 smoke lane (`-smp 4`) and the
`--smp 1` override reached `txkernel:qemu-loongarch64-virt:boot:ok`.

Rollback: remove the `--smp` option and restore the old hard-coded target SMP
selection in `xtask/src/qemu.rs`.
