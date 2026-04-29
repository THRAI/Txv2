# 2026-04-29: RV64 pmap helper extraction

## Context

After the first pmap module split, the board pmap facade still carried
Sv39 PTE encoding helpers, page-table index helpers, alignment checks, and
QEMU/Sv39 page-size constants. That kept unrelated details in the bootstrap
pipeline file.

## Decision

Extract the remaining low-level helpers into board-private modules:

- `pmap/pte.rs` owns PTE flags, leaf/branch encoding, PTE inspection,
  permission validation, kernel-alias permission selection, and physical
  page-table decoding through the direct map.
- `pmap/topology.rs` owns Sv39/QEMU address constants, 4 KiB / 2 MiB / 1 GiB
  sizes, direct-map address formation, page-table index helpers, alignment
  validation, user-top validation, and SATP value construction.

The pmap facade continues to re-export these helpers for sibling modules and
tests, so behavior and call sites remain stable while responsibility is
clearer.

Follow-up cleanup moved the large inline pmap unit-test module to
`pmap/tests.rs`, kept it as a private unit-test module rather than a
crate-level integration test, and made `pmap::topology` the explicit board
configuration namespace for non-pmap callers. This keeps board-private pmap
helpers private without widening the public HAL surface just to test internal
PTE and page-table behavior.

A later comment pass added top-of-module docs and group-level comments across
the pmap split. The comments describe responsibility boundaries and local state
machines by section rather than repeating every function signature.
The top docs were then refined to name the core data structures/state each
module maintains, the main state-mutating/data-flow functions, and the helper
function groups.

The facade was then moved from `src/pmap.rs` to `src/pmap/mod.rs`, matching the
directory split. The facade and its major child modules now order code to match
their header docs: data structures/state first, core lifecycle or data-flow
functions second, and helper machinery after. This keeps the files easier to
scan without changing the HAL surface or pmap behavior.

## Verification

- `cargo fmt`
- `cargo test -p tx-hal -p tx-substrate -p tx-hal-riscv64-qemu-virt`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `cargo fmt --check`
- `cargo xtask progress validate`
- `git diff --check`

## Next Step

The main remaining board pmap facade content is bootstrap/high-half pipeline.
A later cleanup can split that pipeline out of `pmap/mod.rs` once the next
behavioral boot slice needs it.

## Blockers

No blocker for this extraction.
