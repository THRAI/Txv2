# Ext4 JBD2 Revoke Recovery

Date: 2026-07-18

## Change

`Ext4MutationPlan` can now retain deduplicated revoked physical blocks.
The JBD2 transaction image chunks them into revoke pages between metadata
payloads and the commit record. The mount-owned ring reserves those pages,
the journal staging pool retains them through commit, and checkpointing still
writes only metadata home blocks.

Recovery now scans all consecutive committed transactions before applying any
metadata payload. It records the latest revoke sequence for each home block,
using wrapping JBD2 sequence order, then skips a payload whose transaction
sequence is not newer than that revoke. This prevents an older, committed
metadata image from overwriting a block that a later transaction freed and
made eligible for reuse.

The first producer is `Ext4Pager::plan_truncate_tail`. For a block-aligned
shrink of a file with an inline initialized extent root, or a depth-1 indexed
root with one child leaf, it derives immutable block-bitmap, group-descriptor,
superblock, and inode-table after-images and adds every released data block to
`Ext4MutationPlan::revokes`. An indexed child that becomes empty is itself
released and revoked; one that retains extents is an `ExtentNode` after-image.
A shrink through only a trailing hole still produces the changed inode-table
after-image. The source image remains untouched in every case.

This is deliberately limited to released blocks in one allocation group and a
single indexed child. It rejects partial-block EOF, multi-child/deeper trees,
uninitialized extents, and cross-group releases instead of publishing an
incomplete allocator mutation.
It is not yet wired into `FsPageBacking::truncate`, `step_fsync`, or unlink;
the legacy truncate path therefore remains outside the durable release path.

## Verification

- `cargo test -p tx-ext4-format` passed: 36 tests, including revoke across a
  journal sequence wrap plus inline and indexed tail truncate/revoke coverage.
- `rustfmt --check` passed for changed `tx-ext4-format` Rust files.
- `git diff --check` passed.
- `cargo test -p tx-ext4 --lib [--no-default-features]` is blocked before
  `tx-ext4` compiles because this isolated baseline lacks
  `tx_hal::MonotonicCounterIf`, required by `tx-observe`.

## Next

Route the sealed truncate plan through `Ext4MutationPlanSource` and the
mount-owned mutation runtime, replacing the legacy direct size write. Then add
a crash/reuse witness. Extend the format planner separately for multi-group
releases, indexed extent trees, partial EOF, and unlink; do not advertise
end-to-end free/reuse durability until those execution and crash witnesses land.
