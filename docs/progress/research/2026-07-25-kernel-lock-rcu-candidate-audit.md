# Kernel Lock And Identity RCU Shape Audit

Date: 2026-07-25

## Scope

This audit classifies current kernel locks by semantics before the identity RCU
implementation starts. It focuses on the paths most likely to matter for a
large guest rustc build: trap/syscall identity resolution, VM/page-cache
lookup, fd/cwd/path lookup, and filesystem namespace traversal.

The checkout is heavily dirty. Findings describe the current on-disk tree and
do not claim that uncommitted runtime work is present in `HEAD`. This audit and
the accompanying plan update change documentation only.

## Decision

Use three implementation families and do not collapse them into one generic
"RCU slot":

| Family | Exact role | Examples |
|---|---|---|
| `PublishedBinding<T, Cap<T>/PayloadCap<T>>` | one current retained Zone target | identity payload, address space, current userspace identity owner |
| `Published<Root>` | immutable value/index snapshot | recipe tree, PageContainer resident index, cwd/fd/dentry roots |
| retained semantic coordination | queue, state machine, reservation, hardware commit | RangeLock, PageSlot, PTE/shootdown, futex, pipe, completion |

The identity implementation remains P0. The broader audit adds P1/P2 targets
but does not make them prerequisites for the first identity landing.

## Identity Shape

The synchronous trap path has one per-hart identity root, not separately
published identity and payload roots:

```text
CURRENT_USERSPACE_OWNER[hart]
  PublishedBinding<ThreadIdentity, Cap<ThreadIdentity>>
    -> ThreadIdentity.payload
       PublishedBinding<ThreadPayload, PayloadCap<ThreadPayload>>
    -> ThreadIdentity.owner_proc
       Weak<ProcessIdentity>
         -> ProcessIdentity.payload
            PublishedBinding<ProcessPayload, PayloadCap<ProcessPayload>>
              -> ProcessPayload.frame.vm
                 PublishedBinding<AddressSpace, Cap<AddressSpace>>
```

One Guard covers this synchronous walk. `DirectTrapContext<'g>` holds the five
`IdentRef`s privately in the kernel, then exposes only a borrowed domain view
to syscall shims. Owned `SyscallCtx` remains the boundary type for yield,
storage, continuation, and cross-hart work.

Timer/external/IPI IRQ paths cannot create an epoch Guard. The existing
current-userspace `PayloadCap<ThreadPayload>` table therefore remains as a
strong IRQ operational anchor in the first migration. It is not read by the
new synchronous identity resolver. Poll-scoped current-task tables also remain
unchanged because they are written around every `Future::poll`.

Per-hart publication carries an owner ticket containing the entry hart,
userspace request revision, thread key, and payload key. Normal completion
clears the recorded entry hart conditionally; timer preemption retains the
ticket; migration clears the old ticket before publishing the new hart. This
closes the current risk of clearing a different hart after the future resumes.
`ThreadPayload` splits the active owner into an atomic `entry_hart` commit
marker and a request writer cell. The synchronous resolver validates only the
atomic hart and takes no lock; timer/fallback reads the request under its lock.
Cleanup compares the request before key-conditional per-hart clears, so both a
same-thread stale ticket and a different-thread hart takeover are rejected.
Timer preemption retains the old ticket only until the owner future consumes
it; every re-entry, including on the same hart, closes that ticket before
publishing the next request.

## Direct RCU Candidates

| Priority | Current owner/lock | Target | Retained coordination |
|---|---|---|---|
| P0 | `ThreadIdentity.payload` | `PublishedBinding<ThreadPayload, PayloadCap<_>>` | exit status and thread-exit transition |
| P0 | `ProcessIdentity.payload` | `PublishedBinding<ProcessPayload, PayloadCap<_>>` | exec/exit/topology reservations |
| P0 | current userspace thread identity table | one per-hart identity owner binding | owner ticket, request revision, IRQ payload anchor |
| P0 | `ProcessPayload.frame.vm` | `PublishedBinding<AddressSpace, Cap<_>>` | exec phase-6 commit/revalidation |
| P0/P1 | `cred`, `nsproxy`, `net_namespace` staging slots | matching Cap/PayloadCap binding | credential/namespace writer reservations |
| P1 | `ProcessPayload.cwd` | `Published<ProcessCwdState>` | path validation and chdir commit |
| P1 | socket/net-namespace identity payloads | matching payload binding | close/teardown state machines |

`cwd` is an immutable value bundle containing dentry and mount Caps. It is a
`Published<Root>` candidate, not a legal `PublishedBinding` evidence type.
Process parent is weak and likewise must not be forced into the Cap/PayloadCap
binding primitive.

## Split Before Publication

| Priority | Coarse lock | Published read view | Lock/reservation that remains |
|---|---|---|---|
| P1 | `PageContainerStateCell` | persistent `ResidentRoot` mapping page index to stable resident refs | PageSlot, I/O service, leases, completion, range/direct-I/O |
| P1 | `fds` plus `fd_cloexec` | unified persistent `FdTableRoot` | allocation, dup/close, pair install, exec PoNR |
| P1 | `DEntry.children` | persistent weak-child cache root | canonical insert/remove, stale cleanup, rename authority |
| P2 | `VmPmap.state` | immutable `PmapReadRoot` of mapping observations | MapPin ownership, HAL PTE mutation, rollback, shootdown |
| P2 | mount/namespace lookup tables | sole-authority immutable lookup root | bind/move/umount aggregate commit |
| P2 | tmpfs inode/directory state | per-directory persistent roots | link/unlink/rename cross-inode transaction |
| P2 | signal action table | immutable action snapshot | action replacement/reset writer; pending queue remains mutable |
| P3 | socket/network configuration | immutable configuration view | protocol FSM, queues, net-admin transaction |

`RecipeIndex.current` is the completed reference shape:
`Published<RecipeTree>` serves guarded readers while `RecipeIndex.mutation`
serializes derive/prepare/publish.

## Locks That Must Remain

- VM `RangeLock`: interval admission, writer preference, RAII release, wait,
  wake, and retry-from-scratch semantics.
- `VmPmap` ownership/PTE path: `MaterializedPagePin`, HAL reserve/commit,
  rollback, protect/unmap, and TLB shootdown.
- `PageSlot.inner`: fetch/resident/dirty/writeback/error states, generation,
  redirty, and stale-completion rejection.
- PageBacked I/O services, request/lease/completion maps, range reservations,
  direct-I/O pins, and waiter routing.
- futex waiter maps, pipe rings, socket/TCP state machines, signal pending
  queues, and other consume/update/wake protocols.

RCU may provide a read-only observation view beside these locks, but it cannot
perform their admission, ownership transfer, or hardware ordering.

## Landing Order

1. Add and close `PublishedBinding` layout, ordering, race, and API ratchets.
2. Migrate `ThreadIdentity.payload`.
3. Migrate only the current-userspace `ThreadIdentity` owner table; retain the
   timer-IRQ payload anchor and poll-scoped tables.
4. Migrate `ProcessIdentity.payload` and `ProcessPayload.frame.vm`.
5. Build the one-Guard resolver and borrowed direct syscall view.
6. Run SMP correctness and baseline/binding/borrowed-context A/B measurements.
7. Use rustc attribution to choose between PageContainer resident publication
   and cwd/fd/dentry publication as the first P1 follow-up.
8. Consider pmap observation and broader namespace/configuration roots only
   after P1 evidence.

## Evidence

| Claim | File:line | Confidence |
|---|---|---|
| current per-hart payload/identity tables are lock-backed and poll-scoped tables are set/cleared around every poll | `crates/tx-subsystems/src/thread_runtime/structure.rs:548`, `crates/tx-kernel/src/thread_future.rs:306` | high |
| direct trap currently reads userspace payload and identity separately | `crates/tx-kernel/src/trap.rs:175`, `:199` | high |
| userspace owner fields are installed before entering userspace | `crates/tx-kernel/src/thread_future.rs:737` | high |
| normal cleanup currently derives the clear hart after the wait | `crates/tx-kernel/src/thread_future.rs:515` | high |
| epoch Guard creation is forbidden in IRQ context | `crates/tx-substrate/src/epoch/domain.rs:368` | high |
| `ThreadIdentity.payload` and `ProcessIdentity.payload` are lock-backed options | `crates/tx-subsystems/src/thread_runtime/structure.rs:43`, `crates/tx-subsystems/src/process/structure.rs:171` | high |
| RecipeIndex already uses immutable root publication plus writer serialization | `crates/tx-subsystems/src/vm/structure/recipe.rs:77`, `:207` | high |
| AddressSpace separates recipes, pmap, and RangeLock | `crates/tx-subsystems/src/vm/structure/address_space.rs:40` | high |
| pmap observations share the ownership/mutation lock | `crates/tx-subsystems/src/vm/pmap.rs:157`, `:185` | high |
| PageContainer resident, PageSlot, and in-flight I/O maps share one state lock | `crates/tx-subsystems/src/page_backed/mod.rs:689`, `:707`, `:871` | high |
| PageSlot is a generation-checked mutable state machine | `crates/tx-subsystems/src/page_backed/slot.rs:90`, `:118` | high |
| fd entries and CLOEXEC bits use separate locks | `crates/tx-subsystems/src/process/structure.rs:1222`, `:1241` | high |
| dentry child lookup locks a weak-child map and performs stale cleanup | `crates/tx-subsystems/src/vfs/structure.rs:852`, `:904` | high |

## Verification Performed

- Read-only CodeGraph exploration plus focused source/doc checks.
- No Rust implementation or runtime benchmark was performed.
- Documentation lint, progress validation, placeholder, stale-shape, JSON, and
  whitespace checks are recorded in the identity migration plan and STATUS
  catch-up after the associated documentation update.
