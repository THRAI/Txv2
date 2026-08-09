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
  tests passed, one Docker test ignored in the ordinary suite.
- The explicit ignored Docker runtime test passed and final `e2fsck -fn` was
  clean.
- The witness is depth two and uses a single public flush plus metadata
  settlement. Linux-generated depth-three split/carry, crash-cut acceptance,
  and the Rust workload/SubmissionManager handoff remain open. It is not a
  Tier 2 or production acceptance claim.
