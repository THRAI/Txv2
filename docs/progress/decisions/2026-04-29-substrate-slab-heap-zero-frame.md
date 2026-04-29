# Substrate slab heap and zero-frame anchor

## Context

PAGE_SUBSTRATE_v1 requires the page substrate to return with both typed frame
allocation and a kernel heap available. The frame allocator was already live,
but allocation-using CoreInit phases still had no `GlobalAlloc` path and the
zero frame was not claimed as permanent frame evidence.

## Decision

`tx_substrate::init::<P>()` now initializes a no-std slab heap after the bitmap
allocator and typed PT-node source are available. The heap uses small
power-of-two slab classes up to 2 KiB, routes page-sized and larger allocations
through contiguous frame runs, and returns fully empty slab pages to the frame
allocator. Kernel targets get `KernelGlobalAllocator` through `#[global_allocator]`.

Boot also claims one zeroed frame as a permanent zero-frame anchor. The
allocator token model now supports `OwnedFrame::into_permanent_frame()`, which
keeps the frame's refcount and marks it `reserved | direct_mapped` for the
kernel lifetime.

`TrapIf::install_kernel_trap_vector()` is now a concrete HAL method with a
default no-op, and generic `tx_kernel::kernel_main::<P>()` calls it after
`P::init_later()`. This exposes the boundary for the later real trap-vector
slice without making HAL own subsystem policy.

## Verification

- `cargo test -p tx-substrate`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo fmt --check`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci` (10 passed, 1 LA64 target skipped because the local target is not installed)

## Next

Continue the PAGE_SUBSTRATE_v1 checklist with pmap root/range/protect APIs,
committed page-table teardown, ASID/global shootdown batching, and the real
RV64 trap vector.
