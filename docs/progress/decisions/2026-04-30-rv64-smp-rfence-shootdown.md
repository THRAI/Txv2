# Decision: RV64 SMP RFENCE Shootdown

**Date:** 2026-04-30

## Context

The first SMP pass proved that RV64 QEMU can discover four harts, start APs
through SBI HSM, publish them online, and park them. Online publication now
also means AP-local epoch/zone substrate initialization has run. That still
left pmap shootdown as a local `sfence.vma` path, so a later kernel mapping
teardown could release map pins while parked online APs still held stale TLB
entries.

## Decision

Keep `SmpIf` as the low-level CPU/IPI boundary, and make RV64 QEMU's `PmapIf`
shootdown path issue firmware-mediated remote TLB invalidation after the local
`sfence.vma`. The platform now calls SBI RFENCE
`remote_sfence_vma` / `remote_sfence_vma_asid` for the online hart mask minus
the current hart. This is enough for the current QEMU/OpenSBI target and avoids
inventing a kernel-managed IPI/ack protocol before `SMP_v1`.

## Changed Surface

- `tx-hal-riscv64-qemu-virt::Platform::shootdown_kernel_mapping()` now performs
  local pmap shootdown and SBI remote RFENCE for online remote harts.
- `Platform::shootdown_mapping(asid, ...)` now uses the ASID-scoped SBI RFENCE
  function for user-root invalidations.
- `tx_kernel::CoreInit` emits a `:smp:shootdown:ok` smoke sentinel after APs
  are online by issuing a harmless kernel shootdown invalidation.
- `HAL_v1.md` and `PAGE_SUBSTRATE_v1.md` now distinguish this firmware RFENCE
  implementation from the later kernel-managed IPI/ack fallback.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo test -p tx-substrate --test shootdown`
- `cargo fmt --check`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
  --timeout-ms 15000`

The QEMU serial log showed `Platform HART Count : 4`, `Boot HART ID : 1`,
`txkernel:qemu-riscv64-virt:smp:aps:online`,
`txkernel:qemu-riscv64-virt:smp:shootdown:ok`,
`txkernel:qemu-riscv64-virt:reactor:task:ok`, and
`txkernel:qemu-riscv64-virt:boot:ok`.

## Next Step

The next production-SMP slice should decide whether to harden the bus
typed-declaration/wire-destruction path or to add the permanent
interrupt-driven runtime loop. Scheduler admission and reschedule IPIs still
remain separate from this RFENCE work.

## Blockers

No blocker for RV64 QEMU RFENCE-backed pmap shootdown. Remaining SMP blockers
are scheduler admission on APs, reschedule IPI dispatch, permanent idle/WFI
loop integration, typed bus declaration hardening, epoch-protected bus wire
destruction, and a kernel-managed shootdown fallback for platforms without
firmware RFENCE.
