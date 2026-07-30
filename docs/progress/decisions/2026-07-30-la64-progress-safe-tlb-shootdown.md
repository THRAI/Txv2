# LA64 progress-safe TLB shootdown and page-table lifetime

Date: 2026-07-30

## Context

The LA64 BuildStorm guest could stop after `BUILDSTORM_TOOLCHAIN ok`. A QEMU
register snapshot decoded against the submitted LA kernel showed one hart in
`la64_remote_tlb_shootdown` while other harts spun in
`VmPmap.state` acquisition. The shootdown originator held that same pmap
transaction lock.

RV64 did not reproduce the cycle because SBI RFENCE runs through firmware and
does not require a target hart to take a maskable S-mode interrupt. LA64 used a
board IPI, while syscall and page-fault handling intentionally runs with
`CRMD.IE == 0`. The old protocol also serialized every shootdown with one
global lock and acknowledged requests with one shared bit.

The complete audit found four related lifetime/progress defects:

- a pmap switch did not publish one atomic lifetime boundary around the
  software tuple, residency bit, CSR writes, and local INVTLB;
- Ordinary LA64 unmap immediately pruned and freed empty intermediate page
  tables before the caller issued its remote TLB shootdown.
- a CPU could publish itself stopped and permanently mask interrupts while a
  sender still regarded it as a synchronous shootdown target;
- the shared kernel PGDH bootstrap mapping used a boolean flag, so concurrent
  first users could mutate the same page-table tree.

The committed page-table-node registry was also a 4096-entry linear array.
Keeping nodes alive correctly would turn that array into both an early
capacity limit and an O(N) SMP lock bottleneck.

## Decision

LA64 QEMU uses the following protocol.

### Per-hart generation mailbox

Each target hart owns `requested`, `completed`, and `servicing` atomics.
A sender increments the target's requested generation, sends the board IPI,
then waits until completed reaches the generation it published. Requests may
coalesce because the LA64 implementation currently performs a conservative
full INVTLB on the target.

There is no global shootdown lock. While waiting, a sender drains its own
inbound mailbox. The mailbox service routine allocates nothing and acquires no
VM, heap, registry, or IPI locks.

### Progress with maskable interrupts

The pmap-state and vmalloc-map spin loops invoke the mailbox service routine
while contended. Every BSP/AP reactor boundary, prepared interrupt wait,
unbounded ASID/PGDH wait, and shutdown drain also invokes the same hook. This
closes both the specific lock cycle and the more general case where the target
is executing with `CRMD.IE == 0` without currently contending on a lock.
Platforms with firmware RFENCE keep the HAL hook as a no-op, so RV64's normal
path is unchanged.

The shared LA64 hardware IPI vector is retired before a TLB mailbox is drained.
After clearing the hardware vector, the handler reasserts it when any software
IPI kind remains pending. Therefore a request arriving immediately before the
clear cannot be lost, and a request arriving after it raises a new interrupt.

### ASID switch publication

Address-space switching is a three-phase transaction:

1. `begin`: change a per-hart sequence from even to odd, publish the incoming
   tuple and residency bit, and retain outgoing residency.
2. hardware: write ASID/PGDL/PGDH and execute full local INVTLB.
3. `finish`: publish the active tuple, clear outgoing residency, clear the
   switching tuple, and return the sequence to even.

Both the Rust activation path and the no-return userspace-entry assembly obey
that order with interrupts masked across the hardware transition. Root
teardown uses a seqlock-style stable read and treats every odd sequence as
non-quiescent. It never repairs or clears another hart's residency by
inference. Destroying root A only deactivates the current hart if it is
actually running A; an unrelated active root B is left untouched.

LA64 performs a full local INVTLB on every actual address-space switch.
Consequently a hart that has completed switching away retains no tagged TLB
history, and the current residency mask is sufficient for ordinary LA64
mapping shootdowns. This differs intentionally from RV64's separate residency
and history masks.

### Intermediate page-table lifetime

Ordinary user and kernel unmap clears only the leaf. It never frees committed
intermediate tables before shootdown:

- process L0/L1/L2 nodes remain owned by the root and are recursively released
  only after ASID residency reaches zero during root destruction;
- bounded kernel/vmalloc intermediate tables remain resident;
- reservation nodes that were never committed may still be rolled back
  immediately.

The LA64 committed-node registry now matches RV64's 8192-entry open-addressed
hash table with tombstones instead of scanning a 4096-entry `Option` array.

### Target acquisition and CPU offline

Scheduler-visible online state is not enough to synchronize a synchronous
request with permanent CPU parking. Each LA64 target therefore has an
`accepting` bit and a sender-user count:

1. a sender checks `accepting`, increments the user count, rechecks
   `accepting`, and only then publishes a mailbox generation;
2. an offlining CPU deactivates its user root, withdraws scheduler-online and
   `accepting`, services its mailbox while prior sender users drain, performs
   a final mailbox service, and only then disables interrupts permanently;
3. the sender releases its target user only after the requested completion
   generation is visible.

This is a pin/drain protocol, not a timeout. It prevents both waiting forever
on a parked CPU and freeing shared pmap state while a pre-existing sender is
still completing.

### Shared kernel PGDH initialization

Kernel high-half bootstrap mapping uses `UNINIT -> BUILDING -> READY`, with one
CAS-elected builder. Waiters service inbound shootdowns while observing
`BUILDING`. A failed builder returns the state to `UNINIT`; the mapping walk is
idempotent, so a later builder can complete the valid partial tree. No two
harts mutate the shared PGDH tree concurrently.

## Rejected alternatives

- A larger global shootdown lock does not solve the progress cycle and creates
  lock-order cycles between concurrent senders.
- A timeout would permit frame or ASID reuse while stale translations remain,
  converting a visible hang into silent memory corruption.
- Enabling arbitrary nested interrupts throughout syscall/fault handling is a
  much larger kernel preemption change and is not required for this protocol.
- Pruning empty intermediate tables immediately saves little memory but cannot
  satisfy the required `leaf invalidation -> remote completion -> reuse`
  ordering with the current HAL return type.

## Consequences

LA64 pays conservative full remote INVTLB cost and retains empty intermediate
tables longer. In return, shootdown completion no longer depends solely on a
maskable interrupt, page-table nodes cannot be reused under stale hardware
walks, CPU shutdown has an explicit target-acquisition boundary, and
ASID/root reuse has an explicit hardware-aligned lifetime boundary.
Future range-mailbox optimization may replace full INVTLB without changing the
generation or lifetime protocol.
