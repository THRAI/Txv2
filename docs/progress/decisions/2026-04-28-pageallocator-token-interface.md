# Decision: PageAllocator Token Interface

**Date:** 2026-04-28

## Decision

- `tx-substrate` exposes a non-dynamic `PageAllocator` trait and installed
  bitmap-backend substrate functions rather than raw `alloc_frame/free_frame`
  authority.
- Frames move through typed states: `FrameReservation` -> `OwnedFrame` -> role
  evidence such as `MapPin`, `CachePin`, `DmaPin`, `PermanentFrame`, or
  `PtFrame`.
- `FrameMeta.refcount` is the generic owner/retainer counter. Mapping, cache,
  and DMA liveness stay in role counters; a frame returns to the allocator only
  when the packed state reaches zero.
- v1 uses a global atomic bitmap backend with a scan hint. Future per-CPU
  magazines may cache `state == 0` allocator-owned frames without changing the
  token interface.
- `ZeroPolicy::Zeroed` is enforced by an installed direct-map scrubber hook; if
  no scrubber is available, the allocator rolls back the reservation and
  returns `AllocError::ZeroScrubUnavailable`.

## Context

- The page allocator has to serve kernel page-table construction first, then
  later VM entity materialization through VM-owned interfaces.
- Raw PPNs must remain observable for PTE encoding and direct-map access, but
  should not be a freeing authority.
- The active page-substrate doc previously described a raw PPN API. It now
  records the typed-token interface and the future CPU-magazine backend policy.

## Verification

- `cargo test -p tx-substrate`
- `cargo xtask lint docs`

## Next Step

- Implement the boot handoff: normalize BootInfo memory regions, subtract
  reserved ranges, carve `FrameMeta[]` and bitmap storage, install
  `BitmapPageAllocator`, and wire the direct-map zero scrubber.

## Blockers

- Full boot integration still depends on the high-half/direct-map pmap plan and
  the substrate-ready `tx_substrate::init::<P>()` path.
