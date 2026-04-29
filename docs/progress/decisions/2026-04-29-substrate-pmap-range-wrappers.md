# 2026-04-29: HAL pmap range surface

## Context

HAL now exposes single mapping operations for kernel and VM-owned roots. VM still
needs a range-shaped interface, but range locks, recipes, and fault
rematerialization policy belong above HAL.

## Decision

Add no-alloc range wrappers in `tx_hal::pmap`:

- `PmapRangeReservation<P, N>` reserves up to `N` 4 KiB mappings and rolls back
  the reserved prefix on drop unless committed.
- `commit()` publishes the reserved range with caller-supplied permissions.
- `unmap_page_range()` and `protect_page_range()` collect per-page results into
  caller-provided slices.

This keeps the generic pmap surface next to `PmapIf` while boards retain the
specific page-table walks and PTE mutations. Substrate re-exports the helpers
for compatibility and combines their results with frame-accounting shootdown
batches.

## Verification

- `cargo fmt`
- `cargo test -p tx-substrate --test pmap`

## Next Step

Wire these wrappers into VM `AddressSpace` materialization once the VM layer is
ready. Page-sized map-count batching exists; superpage/multi-frame accounting
and remote-hart shootdown remain.

## Blockers

No blocker for page-sized range operations.
