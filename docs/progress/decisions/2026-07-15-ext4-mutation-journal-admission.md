# Ext4 Mutation Journal Admission

## Decision

Buffered ext4 writeback admits an immutable `Ext4MutationPlan` while L4 still
owns the `IoDataSource` and a short-lived epoch guard. `tx-ext4-format` plans
metadata only; `JournalMutationRuntime` retains journal pages and maps the
L4-owned source into the ordered-data graph. L5 never retains the guard or
reads a PageContainer frame.

The mount builder
`mount_ext4_read_write_with_mutation_journal_io_manager_planner` binds the
format provider after opening the backend through a `Weak<Ext4FsInstance<_>>`.
This avoids a planner/backend strong-reference cycle. Mapped-page plans now
include an unchanged inode-table after-image as the metadata anchor required
for an ordered-data fsync transaction; hole plans retain their bitmap and inode
after-images.

## Verification

- `cargo test -p tx-ext4 --no-default-features -- --nocapture --test-threads=1`
- `cargo test -p tx-ext4-format --no-default-features -- --nocapture --test-threads=1`
- `cargo check -p tx-ext4 --lib --no-default-features`
- `cargo check -p tx-ext4-format --lib --no-default-features`
- `cargo xtask progress validate`

The host-tool ext4 test was skipped because `mkfs.ext4`, `debugfs`,
`dumpe2fs`, and `e2fsck` are not installed in this environment.

## Next Step

Replace compatibility `step_fsync` with a stateful L4 `FsyncOp` that retains a
captured dirty-generation frontier, waits for each writeback receipt, submits
one fsync request, and consumes its terminal journal-commit result. A later
checkpoint service must submit `JournalFsyncSource::take_checkpoint_graph()`;
mount replay and power-cut witnesses remain open.
