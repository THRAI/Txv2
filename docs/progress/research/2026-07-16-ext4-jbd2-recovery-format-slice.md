# Ext4 JBD2 Recovery Format Slice

Date: 2026-07-16

## Change

Added a no-std, mount-independent legacy JBD2 recovery routine in
`tx-ext4-format`. It consumes `BlockImage` plus already-discovered
`JournalGeometry`, scans at most one logical ring traversal from `s_start`,
and installs metadata after-images only after the descriptor's matching commit
record has been read. It restores escaped JBD2 magic bytes and issues a
barrier after each installed transaction.

The slice explicitly rejects deleted tags. Truncate, unlink, block reuse, and
revoke are still outside the supported durability surface.

## Verification

- `cargo test -p tx-ext4-format --test jbd2_recovery -- --nocapture`
- `cargo test -p tx-ext4-format --tests -- --nocapture`
- `cargo check -p tx-ext4-format`
- `git diff --check -- crates/tx-ext4-format/src/lib.rs crates/tx-ext4-format/src/journal_replay.rs crates/tx-ext4-format/tests/jbd2_recovery.rs`

The focused witnesses cover a committed after-image, an incomplete
descriptor/payload tail that is ignored, and a transaction that wraps from the
last journal ring page back to `s_first`.

## Next

Integrate replay before the discovered-journal runtime is built, preserve the
recovered transaction sequence in `JournalRing`, and define safe journal
superblock state publication. Do not route guest read-write ext4 mounts to the
discovered-journal helper until those recovery and pool-policy pieces have
separate evidence.

## Blockers

`cargo fmt --check` currently reports formatting deltas in unrelated parallel
worktree files, so this slice formats only its own new Rust files. The
host-tool ext4 integration test is skipped on this machine because
`mkfs.ext4`, `debugfs`, `dumpe2fs`, and `e2fsck` are absent.
