# Direct-I/O L4-to-L6 bridge (2026-07-14)

## Change

`PageContainer` direct-I/O leases can now enter the existing owned L6 block
queue. A submitted direct lease asks the mount-hosted neutral `BackendPlanner`
to produce one mapped data `BioPlan`; the `DirectIoBlockTracker` records the
resulting block request id, including an adjacent-merge result. The buffer,
DMA pins, and range reservation remain in the PageContainer registry until the
tagged device completion is consumed.

Tagged completion first resolves L6 tag/depth ownership. Page/metadata/graph
completion continues through `PageService`; a direct tracker match explicitly
authorizes the otherwise-untracked completion, and then calls PageContainer's
existing direct terminal path outside the state lock. Successful direct writes
therefore retain the conservative clean-cache invalidation rule; reads retain
clean cache pages.

## Evidence

- `cargo test -p tx-subsystems --lib page_backed --no-default-features -- --nocapture --test-threads=1`: 135 passed.
- `cargo test -p tx-subsystems --lib io_manager::page --no-default-features -- --nocapture --test-threads=1`: 47 passed.
- `cargo check -p tx-subsystems --lib --no-default-features`: passed.

The new end-to-end test supplies a direct-target planner and an owned fake
device executor. It observes one L6 dispatch and tagged terminal completion,
then verifies both the direct tracker and the DMA/range registry are empty.

## Boundary

This bridge accepts only one mapped data bio per direct submission. It does
not yet wait/retry a full queue, handle metadata-first/graph direct plans,
zero-fill direct reads of holes, allocate direct-write holes, or expose an
`O_DIRECT` syscall. The latter needs per-open-file flag separation from pipe
packet mode plus an async syscall operation that advances the L4/L6 service.
