# Decision: Kernel Shootdown Map Accounting

**Date:** 2026-04-29

## Decision

- Substrate now provides `KernelShootdownBatch<'a, A, const N>` as a no-alloc
  kernel-unmap accounting primitive.
- The batch consumes a page-sized `PmapUnmapResult` and the matching `MapPin`.
  It issues `P::shootdown_kernel_mapping()` first, then drops the `MapPin`,
  so `FrameMeta.map_count` cannot reach zero before the invalidation happens.
- Push validation rejects non-4 KiB results and physical frames that do not
  match the supplied `MapPin`.
- Dropping an unissued batch is a debug assertion and intentionally leaks
  pending pins rather than releasing them before shootdown.

## Context

- HAL owns PTE mutation and invalidation mechanics, but substrate owns
  `FrameMeta` and role counters. The dependency direction stays substrate →
  HAL: HAL never calls into frame accounting.
- The full process-root pmap path will need ASID-scoped batching and multi-page
  results. This slice lands the kernel page-sized form used to encode the
  ordering invariant in executable code.

## Verification

- `cargo test -p tx-substrate --test shootdown`
- `cargo test -p tx-substrate`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo fmt --check`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `git diff --check`

## Next Step

- Add intermediate-table teardown/release, then generalize shootdown batching
  for process roots and ASID-scoped invalidations.

## Blockers

- Slab/global allocator initialization and the minimal trap vector are still
  deferred.
