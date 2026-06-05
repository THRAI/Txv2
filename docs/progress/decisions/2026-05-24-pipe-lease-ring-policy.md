# Pipe Lease Ring Policy

Date: 2026-05-24

## Decision

Pipe is an ordered, waitable stream transport, not the owner of file-cache
policy. PageBacked owns `PageLease` creation, retain/release evidence, and
destination install/copy fallback. Pipe stores descriptor slots that may carry
ordinary anonymous pipe pages, PageBacked leases, or VM-produced
`UserPageGift` transfer tokens.

The byte-stream pipe ring defaults to 16 page slots and can be resized through
`fcntl(F_SETPIPE_SZ)`. `PIPE_BUF` writes reserve enough tail-merge and free-slot
capacity before publishing bytes, so small writes remain atomic without
allocating per syscall.

## Consequences

- Full page-aligned PageBacked `splice(file -> pipe -> file)` can share a frame
  through a retained lease when the destination page is absent.
- If the destination page already exists, PageBacked copies from the lease into
  the resident frame and marks it dirty.
- `StructBacked` and projected non-pipe endpoints do not become generic splice
  storage backends.
- Watchqueue/notification pipes remain a separate future `PipeMode`, not a
  `PipeStorage` variant mixed into ordinary byte-stream data.

## Tech Debt

`vmsplice(SPLICE_F_GIFT)` is accepted only as staged compatibility until the
page-gift implementation plan lands in code:
`docs/progress/plans/2026-06-04-page-gift.md`.

The active-doc contract is now explicit:

- VM produces `UserPageGift` tokens after materializing an eligible full-page
  private user mapping, acquiring substrate `GiftPin` transfer evidence, and
  freezing the old writable PTE materialization.
- Pipe stores gifts only as ordered descriptor payloads.
- PageBacked consumes gifts through install/copy policy.

Until the VM primitive and descriptor wiring are implemented, Tx may copy user
iov bytes into pipe buffers for compatibility, but must not advertise copied
buffers as stealable gifts.

## Verification

- `cargo test -p tx-subsystems pipe_ -- --nocapture`
- `cargo test -p tx-subsystems page_backed -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::fd_ops_wave3 -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::splice_dispatch -- --nocapture`
