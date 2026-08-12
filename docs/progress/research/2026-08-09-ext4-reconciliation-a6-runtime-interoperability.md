# A6 Runtime Interoperability Witness

Date: 2026-08-09

## Result

The public `tx-ext4` runtime now has an explicit Linux-generated image witness.
The ignored test
`docker_linux_depth_two_unwritten_flush_survives_tx_runtime_settlement` creates
a 64 MiB Tier 1 ext4 image with Docker `mke2fs`/`debugfs`, fragments `/file`
into Linux-generated unwritten extents, and repairs the fixture's metadata
checksums with Docker `e2fsck -fy` before the test starts. `Ext4Pager` opens the
same file-backed `BlockImage`, confirms a depth-two extent root, and discovers
the image's own JBD2 geometry. The public read-write mount then uses that
geometry in `JournalMutationRuntime`.

The test drives `FsPageBacking::flush_page` for a real unwritten logical page,
settles the ordered-data transaction through the next public
`FsOps::chmod_inode` mutation, and drops the mount. Docker `e2fsck -fn` accepts
the resulting image. This proves Linux -> Tx format/runtime admission and Tx ->
Linux persisted extent metadata through the current public mount, PageBacked,
and VFS entry points. The test ran explicitly on 2026-08-09 and passed in 0.84s.

The complementary ignored test
`docker_linux_depth_three_parent_carry_flush_survives_tx_runtime_settlement`
uses a 3 GiB Docker/debugfs fixture with the writable Tier 1
`metadata_csum` profile. Linux builds a depth-three fragmented unwritten tree,
then two in-leaf insertions fill one depth-three parent. The test finds a
three-block unwritten extent in a near-full sibling leaf below that parent,
opens the same file-backed image through `Ext4Pager`, mounts it through
`mount_ext4_read_write_with_mutation_journal_io_manager_planner`, and flushes
the middle logical block. `FsOps::chmod_inode` settles the ordered data and
metadata transaction. Docker `e2fsck -fn` accepts the resulting file. The
explicit test passed on 2026-08-09 in 136.03s.

## Format Fix

The first direct e2fsck run exposed that `ExtentNode::encode_*` leaves the
external extent-tail checksum zero when the Linux image has `metadata_csum`.
`extent_block_csum32` now computes the ext4 checksum over inode number,
generation, and the external node body; `Ext4Pager::plan_write_page` refreshes
every `MetaRole::ExtentNode` after-image before exposing the immutable plan.
The checksum repair is limited to metadata after-images and preserves the
existing no-direct-home-write ownership boundary.

## Evidence And Boundary

- `cargo test -p tx-ext4-format -- --test-threads=1`: 61 pager tests and all
  host/journal tests passed; one existing 3 GiB Docker test remains ignored.
- `cargo test -p tx-ext4 --lib --no-default-features -- --test-threads=1`: 72
  tests passed, with the two explicit Docker runtime witnesses ignored in the
  ordinary suite.
- The explicit ignored Docker runtime test passed and final `e2fsck -fn` was
  clean.
- Both the depth-two and depth-three parent-carry witnesses use one public
  flush plus metadata settlement, while preserving PageBacked page lifecycle,
  ext4 immutable planning/JBD2 staging, and Mount settlement ownership.
- Crash-cut acceptance, a timestamped Tier 1 candidate campaign, and the Rust
  workload/SubmissionManager handoff remain open. These witnesses are not a
  Tier 2 or production acceptance claim.
