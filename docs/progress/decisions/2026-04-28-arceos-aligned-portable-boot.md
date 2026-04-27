# Decision: ArceOS-Aligned Portable Boot

**Date:** 2026-04-28

## Decision

- txKernel adopts the ArceOS/axplat boot separation shape: concrete board HAL
  crates own `_start`, linker placement, first-stack setup, BSS clearing,
  firmware register conventions, early console, and shutdown.
- Board binary crates remain the static `ActivePlatform` selectors and export
  `rust_entry(cpu_id, firmware_arg)`.
- `tx_hal::entry::<P, K>` translates raw firmware registers into `BootHandoff`
  through `BootPlatformIf` before calling the generic kernel continuation.
- The RV64 QEMU smoke milestone uses SBI console output and the serial sentinel
  `txkernel:qemu-riscv64-virt:boot:ok`.
- QEMU smoke execution belongs to `cargo xtask ci-slow`; `cargo xtask ci`
  remains the fast compile/lint lane.
- Active design docs carry the implementation contract: new platforms must
  follow the same `_start -> rust_entry -> tx_hal::entry -> BootHandoff ->
  tx_kernel::kernel_main::<P>` shape before layering BusyBox, filesystem, or
  OSComp tests.

## Context

- ArceOS keeps platform-specific boot mechanics below the generic runtime:
  the platform owns `_start` and calls a generic main handoff. txKernel borrows
  that separation while keeping stricter compile-time board selection.
- The previous skeleton had board binaries defining stub `_start` functions,
  which made the generic entry path too easy to tie to a single board shape.
- A deterministic serial sentinel is needed before OSComp images become useful
  as test inputs.

## Consequences

- Generic `tx-kernel` receives typed boot facts and must not depend on concrete
  firmware registers, DTBs, SBI details, or board crates.
- RV64 QEMU now has a real linker script and boot assembly path; LA64 and the
  M1 Dock mock stay compile-first until their boot protocols are implemented.
- CI has two lanes: fast compile/lint and slow QEMU smoke. Slow failures should
  point at the serial log and the HAL boot design docs.
- Architecture lint rejects raw firmware boot values in generic `tx-kernel`;
  later stronger lint can validate board-binary minimalism once LA64 and M1
  Dock move from compile-first stubs to platform-owned `_start` paths.

## Alternatives Considered

- Keep `_start` in the board binary. Rejected because it blurs the platform
  crate's ownership of boot mechanics and makes portable kernel main harder to
  enforce.
- Put platform auto-selection in `tx-kernel` like some axhal integrations.
  Rejected because txKernel requires static board binary selection with no
  runtime HAL manager or generic-kernel board imports.
