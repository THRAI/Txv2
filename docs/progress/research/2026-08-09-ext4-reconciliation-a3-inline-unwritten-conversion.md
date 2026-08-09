# A3 Inline Unwritten Extent Conversion

Date: 2026-08-09

## Result

`plan_write_page` now recognizes depth-0 through bounded depth-2 unwritten extents before
ordinary hole allocation. It splits the matching extent into an optional
unwritten prefix, one initialized target block, and an optional unwritten
suffix. The physical mapping is reused. Non-overflowing conversion contains no
allocation claim, bitmap, group-descriptor, or superblock update.

The depth-0 implementation emits an inode-table after-image and the ordered
data write. A non-overflowing depth-1 conversion emits the selected
extent-leaf after-image, adding an inode-table after-image only when the write
grows the file. A full depth-1 leaf splits when the inode root has capacity:
the plan claims one new extent-node block and includes bitmap, GDT,
superblock, both leaf after-images, and the updated inode root. A full inode
root grows into two depth-one index nodes. A depth-two leaf can likewise split
when its parent has capacity, or split its full parent and carry the sibling
into a non-full inode root. Root-full carry remains `EOPNOTSUPP`-equivalent.

## Evidence

- `pager_initializes_one_inline_unwritten_block_without_new_claims` verifies
  the physical target, the three exact post-conversion extents, zero new claims,
  and no source-image mutation.
- `pager_initializes_one_depth_one_unwritten_block_without_new_claims` verifies
  the selected leaf after-image, exact extent shape, size growth, zero claims,
  and unchanged source inode, leaf, and data pages.
- `pager_keeps_depth_two_initialized_writeback_on_the_existing_mapping_path`
  proves this depth-one helper leaves the existing initialized deep-tree path
  reachable.
- `pager_splits_full_depth_one_unwritten_leaf_without_data_claim` verifies one
  metadata-node claim, two leaf after-images, updated root keys, and untouched
  source inode, leaf, and bitmap pages.
- `pager_grows_full_depth_one_root_for_unwritten_conversion_without_data_claim`
  verifies three claims and depth-one to depth-two root promotion.
- `pager_splits_full_depth_two_unwritten_leaf_into_nonfull_parent_without_data_claim`
  verifies the captured root -> parent -> leaf split path and its updated parent.
- `pager_carries_full_depth_two_parent_for_unwritten_leaf_without_data_claim`
  verifies two metadata-node claims, split parent index ordering, and the root
  sibling index.
- `cargo test -p tx-ext4-format -- --test-threads=1` passed 83 tests plus
  doc-tests.
- `cargo xtask lint invariants ext4-no-direct-home-write` passed.

## Boundary

This covers depth-0, depth-1, and a bounded depth-2 path: depth-one leaf split,
full-root growth, depth-two leaf split into a non-full parent, and full-parent
carry into a non-full root. Root-full carry, depth-3+ conversion, recursive
freeing, and Docker/debugfs shape witnesses belong to the remaining A3-A5
path. No runtime mutation lowering changed.

## A4/A5 Recursive Release Update

The same worktree now connects `plan_truncate_size` shrink plans and
`plan_destroy_inode` to a bounded recursive extent-release planner. The planner
validates extent depth, key ordering, child-home cycles, reserved metadata
aliases, released-vs-retained physical overlap, and duplicate releases before
emitting any after-image. Child extent-node after-images are emitted before
their parent state; removed child homes and initialized data blocks become
sorted revoke/deferred-free claims. Root depth-one single-child collapse is
represented directly in the inode after-image. Journal data and extent-node
homes are included in the reserved set when a readable journal inode is
present.

The public shrink path updates block bitmaps, merged GDT pages, the superblock
free-block count, extent-node after-images, and the inode size/`i_blocks`
after-image in one immutable plan. The public destroy path keeps the existing
zero-link, empty-directory, inode bitmap, classic-orphan, and used-directory
count checks while allowing indexed trees. Extension-only truncate remains a
size-only plan, matching the existing runtime contract.

Evidence:

- `pager_recursively_truncates_depth_two_tree_without_home_writes` covers a
  depth-two root with two indexed parents, descendant release, retained leaf
  after-image, root key reduction, and unchanged source homes.
- `pager_destroys_depth_two_indexed_inode_without_home_writes` covers indexed
  destroy releasing data, leaf, and parent homes while retaining inode and
  allocation lifecycle after-images and unchanged source homes.
- `pager_recursively_truncates_depth_three_tree_without_home_writes` covers
  the bounded depth-three descent and verifies sorted data/node revoke claims,
  paired deferred frees, and unchanged source homes.
- `docker_e2fsck_accepts_depth_two_fragmented_truncate_after_images` has
  Docker `debugfs` create 1,400 sparse allocated extents, which e2fsprogs
  stores as a depth-two unwritten tree. It applies the bounded Tx conversion
  plan to initialize each physical mapping, then applies the recursive
  truncate metadata after-images and passes Docker `e2fsck -fn`.
- That fixture now compares `debugfs bmap /file 0` before and after truncate,
  proving the retained mapping does not drift.
- `docker_e2fsck_accepts_depth_two_fragmented_unlink_destroy_after_images`
  takes the same Linux-generated tree through Tx unwritten conversion, unlink,
  and zero-link destroy before Docker `e2fsck -fn` accepts the result. It
  caught and closed two compatibility defects: non-metadata-csum profiles now
  preserve `bg_itable_unused`, and final inode destruction clears the free
  inode's mode and `i_dtime` after unlink has cleared the orphan head.
- `cargo test -p tx-ext4-format -- --test-threads=1` passed 88 tests plus
  doc-tests; the current `cargo -q xtask unit` run passed `655 + 114 + 71 +
  167`.
- `cargo xtask lint invariants ext4-no-direct-home-write` reported 0
  violations; docs lint passed with 7 existing warning-only stale-vocabulary
  mentions; scoped rustfmt and `git diff --check` passed.

## A6 Runtime Lowering Update

The current public runtime paths now have depth-two host witnesses without
introducing a depth-specific shortcut. `FsPageBacking::truncate` obtains the
immutable plan from `Ext4Pager`, stages it through `JournalMutationRuntime`,
and checkpoints the retained leaf after-image, allocation metadata, and inode
after-image. `FsOps::destroy_inode` follows the same runtime and retains every
recursive revoke/deferred-free claim in `MutationHandle` until checkpoint.

Evidence:

- `ext4_depth_two_truncate_public_path_checkpoints_descendant_after_images`
  stages five metadata records and one revoke page, completes the public
  truncate, then checks the persistent bitmap, retained leaf, retained parent,
  inode size, and `i_blocks` state.
- `runtime_retains_depth_two_destroy_deferred_frees_until_checkpoint` admits
  the depth-two destroy plan through `JournalMutationRuntime`, observes its
  active transaction frontier, and proves every deferred-free claim rejects
  reuse while the handle is live.
- `ext4_depth_two_destroy_public_path_checkpoints_inode_and_allocation_metadata`
  drives the public destroy entry through durable checkpoint settlement.
- The test runtime pool now mirrors the 32-page production configuration so a
  metadata-only transaction with five after-images, one revoke page, descriptor,
  commit, checkpoint copies, and journal state pages is admitted rather than
  failing an artificial test-only capacity limit.

## Full Depth-Two Root Carry Update

When a depth-two unwritten conversion splits both the selected leaf and its
full parent, a full inode root no longer rejects the plan. The immutable plan
claims the right leaf, right parent, and two new depth-two root nodes; the
inode root becomes a two-entry depth-three index. The existing after-image
ordering is retained: leaf nodes, parent nodes, then the new root nodes and
inode-table image. No data-block allocation or home write is introduced.

`pager_grows_full_depth_two_root_for_unwritten_leaf_without_data_claim` proves
the four metadata-only claims, five re-homed root entries, depth-three inode
root, and unchanged inode/parent/leaf/bitmap source images. The focused test
and the full `cargo test -p tx-ext4-format -- --test-threads=1` suite passed.

Boundary remains explicit: follow-on depth-3+ unwritten conversion, crash-cut
campaigns, and candidate acceptance remain pending. These host planner and
runtime regressions do not establish Tier 2 or production compatibility.

## Depth-Three Descent Update

`plan_write_page` now descends depth-three through the existing index path with
per-node depth/layout validation and a cycle guard. When the selected leaf has
capacity, conversion reuses the physical block and emits one extent-node
after-image without allocation or inode-root mutation. A leaf overflow still
returns `Unsupported` until the complete ancestor carry plan is implemented.

`pager_converts_depth_three_unwritten_leaf_without_new_claims` proves the
root -> index -> index -> leaf path, exact prefix/initialized/suffix extents,
unchanged source homes, and zero allocation claims. The full format suite now
passes 59 pager-mock tests plus all host-tool and journal tests.

The existing Docker fixture remains intentionally depth-two: a Linux-generated
depth-three tree requires each depth-one parent to carry 340 leaf children and
the depth-two root to carry four such parents, which is a materially larger
shape than the current 64 MiB fixture. This is a compatibility-evidence gap,
not permission to infer Linux depth-three support from the mock.
