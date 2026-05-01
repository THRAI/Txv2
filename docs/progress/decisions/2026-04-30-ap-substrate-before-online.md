# Decision: AP Substrate Init Before Online Publication

**Date:** 2026-04-30

## Context

The first SMP boot pass let RV64 QEMU APs set `tp`, install trap vectors, mark
online, and park. While continuing toward production SMP, we found that the
RV64 `install_early_percpu` helper also marked the CPU online. That made the
BSP's online mask observe a secondary hart before AP-local substrate state was
ready.

## Decision

Make online publication the final AP bring-up step owned by generic
`CoreInit`, not a side effect of `PercpuIf::install_early_percpu`. Add
`tx_substrate::init_on_ap(cpu)` as the AP-local substrate entrypoint and call it
from `secondary_cpu_entry` after early per-CPU/platform hooks and before
`P::mark_cpu_online(cpu)`.

Today `init_on_ap` initializes EBR CPU-local state and Zone per-CPU buckets.
That means future scheduler admission can treat `SmpIf::online_cpus()` as
"this CPU may enter epoch guards and zone paths", not merely "the low
trampoline reached Rust".

## Changed Surface

- `tx_substrate::init_on_ap(cpu)` now wraps `epoch::init_on_ap(cpu)` and
  `zone::init_on_ap(cpu)` with a typed `ApInitError`.
- `tx_kernel::CoreInit::secondary_cpu_entry` calls AP substrate init before
  `init_later_secondary`, trap-vector installation, online publication, and
  parking.
- RV64 QEMU `install_early_percpu` now only installs `tp`; it no longer marks
  the CPU online.
- `HAL_v1.md` and `PAGE_SUBSTRATE_v1.md` now state that AP-local substrate init
  precedes online publication.

## Verification

- `cargo test -p tx-substrate --test ap_init`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

The `ap_init` test proves that CPU 1 can enter an epoch guard only after
`tx_substrate::init_on_ap(CpuId(1))`, and that an out-of-range AP is rejected.

## Next Step

The next SMP runtime slice can now move to interrupt/IPI dispatch with a
stronger online invariant. AP scheduler admission, reschedule IPIs, and the
permanent idle/WFI loop remain pending.

## Blockers

None for AP-local substrate initialization. The current AP zone bucket init only
initializes zones registered before the AP call; later static-zone registration
discipline still needs to be frozen before broad subsystem admission.
