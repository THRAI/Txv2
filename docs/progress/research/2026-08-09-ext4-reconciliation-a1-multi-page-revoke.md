# A1 Single-Transaction Multi-Page Revoke

Date: 2026-08-09

## Result

A1 is complete in the clean `codex/ext4-worktree-reconciliation` worktree.
The current ownership path now accepts every revoke page generated for one
immutable `Ext4MutationPlan`; it does not introduce concurrent transactions or
cross-transaction replay.

`Jbd2TransactionImage` sorts and deduplicates revoke blocks, emits as many
`Jbd2Revoke` pages as necessary, and keeps them between metadata after-images
and the commit record. Recovery collects all matching-sequence revoke pages
before the commit and suppresses only the affected after-images of that same
transaction.

The runtime now carries `Vec<JournalBio>` and `Vec<LbaRange>` for revoke
records. `JournalRing::reserve_with_revoke_pages` reserves the exact order
`descriptor -> metadata -> all revoke pages -> commit`. Staging retains every
revoke record lease in `PreparedJournalTransaction.records`; the regression
test proves a two-page revoke image remains unavailable from the pool until the
current `JournalFsyncSource` checkpoint completion releases the transaction.

## Boundaries

- No VFS, Mount, PageBacked, VM, or shared interface types changed.
- `MutationHandle` and deferred-free ownership remain the current authority.
- The old multi-transaction/cross-transaction replay behavior remains deferred.
- Page-count, layout mismatch, ring overflow, incomplete commit, and codec
  parse errors retain their existing fail-closed behavior.

## Verification

- `cargo test -p tx-ext4-format --test jbd2_transaction_image -- --test-threads=1`: 3 passed.
- `cargo test -p tx-ext4-format --test jbd2_recovery -- --test-threads=1`: 11 passed.
- `cargo test -p tx-ext4 --test journal_transaction_plan -- --test-threads=1`: 4 passed.
- `cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1`: 10 passed.
- `cargo test -p tx-ext4 --test mutation_lifecycle -- --test-threads=1`: 7 passed.
- `cargo xtask lint invariants ext4-no-direct-home-write`: 0 violations.
- `cargo -q xtask unit`: `tx-shims` 655, `tx-kernel` 114, `tx-ext4` 68, `tx-scripts` 167 passed.
- Scoped `rustfmt --check` and `git diff --check` passed.

## Next And Blocker

Next is A2: extract cross-block-group allocation and release planning into the
immutable format-side mutation planner. The integration worktree has no local
historical Tier 1 `acceptance-receipt.json`, so receipt-integrity verification
cannot run here; this does not affect the host regression result and does not
constitute new crash, e2fsck, or xfstests acceptance evidence.
