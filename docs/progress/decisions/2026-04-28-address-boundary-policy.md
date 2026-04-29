# Address Boundary Policy

**Date:** 2026-04-28

## Decision

Address typing is reserved for layers where address meaning is load-bearing:
HAL/pmap, boot symbol crossing, PAGE_SUBSTRATE frame/direct-map mechanics,
VM user-range materialization, and user-access copy gates.

Ordinary kernel subsystems should not traffic in raw `usize` addresses,
untyped `VirtAddr` dereferences, or explicit direct-map arithmetic. Their
public language is semantic evidence: `Cap<T>`, `Weak<T>`,
`IdentRef<'g, T>`, witnesses, reservations, recipes, and role-shaped Frame
tokens.

## Boundary Rules

- A typed address value is not dereference authority.
- Address arithmetic is encapsulated in named helpers for alignment, page
  indexes, range splitting, pmap indexes, and explicit conversions such as
  `phys -> direct-map` or boot-linked symbol -> kernel alias.
- Board code may name linker/static symbols only in its boot-static capture
  surface; other board code consumes typed methods from that capture. RV64 QEMU
  enforces this with `cargo xtask lint arch`, which rejects boot-static address
  capture outside `boot_static.rs`.
- User memory stays as `UserPtr<T>`/`UserRange`-style values and crosses only
  through `UserAccessIf`/copyin/copyout or VM fault materialization.
- PAGE_BACKED and higher layers use semantic evidence and content-relative
  indexes/offsets, not raw direct-map pointers.

## Audit Result

The active boundary is mostly clean: VM already subordinates pmap
materialization to recipes and RangeLock, PAGE_BACKED exposes RNode/PageContainer
caps rather than memory addresses, and PAGE_SUBSTRATE owns FrameMeta/Ppn/direct
map mechanics.

Known cleanup remaining: several sketches still use broad placeholder names
like `VAddr`, `VAddrRange`, `UserBuf`, and direct-map copy comments. During
implementation those should become the typed user-range/direct-map helper
names described in HAL, PAGE_SUBSTRATE, VM, and PAGE_BACKED.

## Verification

Docs updated:

- `docs/design/01_substrate/HAL_v1.md`
- `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`
- `docs/design/03_memory-vm/VM_v1_2.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
