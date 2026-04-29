# 2026-04-29: HAL owns the generic pmap surface

## Context

`tx_substrate::pmap` contained no-alloc range helpers that only depended on
`PmapIf` and HAL pmap types. That made the substrate look like it owned part of
the pmap API surface even though the board-specific PTE operations already live
behind `PmapIf`.

## Decision

Move the generic range/session helpers to `tx_hal::pmap`:

- `PmapRangeReservation<P, N>`
- `reserve_page_range()`
- `unmap_page_range()`
- `protect_page_range()`
- `PmapRangeError`

`tx_substrate::pmap` now re-exports those helpers for compatibility. Specific
pmap operations, page-table walking, PTE encoding, ASID allocation, and
bootstrap layout remain in the board implementation.

## Consequences

The boundary is cleaner:

- `tx-hal` owns portable pmap vocabulary and surface logic.
- board crates own concrete architecture/page-table operations.
- `tx-substrate` owns frame metadata, map-count pins, slab, and shootdown
  accounting that depends on `FrameMeta`.

## Verification

- `cargo fmt`
- `cargo test -p tx-substrate --test pmap`
- `cargo test -p tx-substrate`
- `cargo test -p tx-hal-riscv64-qemu-virt`

## Next Step

Completed by the later RV64 pmap module/helper extraction: the board pmap now
uses `pmap/mod.rs` plus responsibility-specific child modules for topology/PTE
helpers, PT-node ownership, kernel mappings, process roots, boot pipeline, and
tests.
