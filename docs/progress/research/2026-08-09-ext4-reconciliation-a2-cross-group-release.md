# A2 Cross-Block-Group Release Extraction

Date: 2026-08-09

## Result

The format-side immutable mutation planner now supports releasing initialized
inline extent blocks from more than one ext4 block group for both
`plan_truncate_size` and `plan_destroy_inode`.

`plan_block_releases` snapshots and clears each involved block bitmap exactly
once. `plan_group_free_block_increments` then merges the corresponding group
descriptor edits by descriptor home page, preserving the one-metadata-home
rule of `Ext4MutationPlan`. Destroy additionally merges the inode-group free
inode and used-directory changes into that same GDT after-image when needed.
The superblock free-block count changes once for the aggregate release count.

All release bitmaps, GDT pages, and the superblock after-image are planned
before either path records revoke/deferred-free claims. The source image stays
unchanged during planning.

## Boundaries

- The scope remains `tx-ext4-format`; VFS, Mount, PageBacked, VM, and runtime
  journal ownership did not change.
- Only current inline, initialized extent leaves are enabled. Indexed roots,
  deep trees, unwritten extents, and cross-group allocation still fail closed.
- This does not introduce multi-transaction journaling or change A1's
  single-transaction revoke ownership.
- The JBD2 state-update codec test fixture now writes a valid initial checksum
  before calling `Jbd2Superblock::parse`; the integration baseline validates
  checksums while the older historical fixture predates that validation.

## Verification

- `cargo test -p tx-ext4-format --test jbd2_codec -- --test-threads=1`: 7 passed.
- `cargo test -p tx-ext4-format --test pager_mock -- --test-threads=1`: 44 passed.
- `cargo test -p tx-ext4-format -- --test-threads=1`: all 72 tests and doc-tests passed.
- `cargo -q xtask unit`: `tx-shims` 655, `tx-kernel` 114, `tx-ext4` 68 passed.
- `cargo xtask lint invariants ext4-no-direct-home-write`: 0 violations.
- `cargo xtask ext4 tier1 --dry-run`: passed.
- `cargo xtask lint docs`: passed with 7 existing warning-only stale-vocabulary mentions.
- Scoped `rustfmt --check` and `git diff --check` passed.

## Next And Blocker

Next, assess cross-group allocation independently from release planning. It
must preserve the current immutable plan and merged GDT-after-image model; it
must not pull indexed extent-root growth into this slice. The clean worktree
still has no historical Tier 1 acceptance receipt, so these results are host
regression evidence only, not renewed crash, e2fsck, or xfstests acceptance.
`cargo xtask progress validate` is blocked by the unrelated missing
`docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md` reference
from `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`.
