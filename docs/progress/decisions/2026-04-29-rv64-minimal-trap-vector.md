# RV64 minimal trap vector

## Context

HAL and page-substrate docs require a minimal trap vector before the generic
kernel proceeds into allocation-using and later VM/user work. The code surface
had `TrapIf`, but it was an empty trait and no selected point installed `stvec`.

## Decision

`tx_hal::entry::<P, K>()` now calls `P::install_minimal_trap_vector()` before
`BootPlatformIf::boot_handoff()`. Generic `tx_kernel::kernel_main::<P>()`
continues to call `P::install_kernel_trap_vector()` after `P::init_later()`.

RV64 QEMU installs a direct-mode `stvec` pointing at a minimal assembly vector.
The vector captures `scause`, `sepc`, and `stval`, prints them through the SBI
console, and spins. For now `install_minimal_trap_vector()` and
`install_kernel_trap_vector()` install the same panic vector; the later full
trap-shell slice can replace the handler without changing boot sequencing.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

## Next

Return to pmap process-root/range work, committed page-table intermediate
ownership, and ASID/global shootdown batching.
