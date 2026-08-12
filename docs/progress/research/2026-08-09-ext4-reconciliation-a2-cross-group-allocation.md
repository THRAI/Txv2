# A2 Cross-Group Allocation Extraction

Date: 2026-08-09

## Current Result

The current immutable `Ext4MutationPlan` path can allocate the data block and
the first external extent-node block across block groups when a hole write
spills a full inline extent root. Allocation is deterministic by group then
bitmap bit. Every affected bitmap produces one after-image, descriptors that
share a GDT home are folded into one after-image, and the superblock free-block
count is decremented once by the total number of claims.

The planner rejects a block marked free in the bitmap when it aliases known
metadata: the superblock, GDT, a group bitmap, or an inode-table block. An
exhausted bitmap also returns before any home-image mutation. The current
`GroupDesc` format model does not expose the historical `BLOCK_UNINIT` or
stored bitmap-checksum fields, so this extraction deliberately does not import
that newer on-disk API.

## Evidence

- `pager_spills_inline_root_with_claims_from_two_groups` proves claims at 63
  and 127, two bitmap after-images, one shared GDT after-image, one superblock
  after-image, and no source-image write.
- `pager_rejects_metadata_alias_and_exhausted_block_plans_without_home_write`
  proves both rejection paths leave source metadata unchanged.
- `pager_rejects_incomplete_group_allocation_without_home_write` proves a
  missing group descriptor fails before any home-image mutation.
- `docker_e2fsck_accepts_inline_root_spill_after_images` builds a real ext4
  fixture with `debugfs`, applies data and metadata after-images through the
  pager's L6 adapter, and passes Docker `e2fsck -fn`.
- `cargo test -p tx-ext4-format -- --test-threads=1` passed 76 tests plus
  doc-tests.
- `cargo -q xtask unit`, `cargo xtask lint invariants
  ext4-no-direct-home-write`, and `git diff --check` passed.

## Boundary

This completes the A2 allocation slice only: inline-root depth-0 to depth-1
spill, not indexed insertion, leaf split, root growth, unwritten conversion,
or recursive freeing. Those paths remain A3 through A5. The Docker oracle
proves the generated fixture's on-disk consistency, not a broader Tier 2
compatibility claim.
