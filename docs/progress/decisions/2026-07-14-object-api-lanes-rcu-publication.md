# Object API Lanes And RCU Publication Boundary

Date: 2026-07-14

## Decision

Semantic owner/root objects compose three shared lane families:
`BindingLane`, `ProjectionLane`, and `ReadinessLane`. Identity/evidence remains
the role-shaped `Cap`/`Weak`/`IdentRef`/operational-evidence language;
reservation is a binding transition phase; publication/RCU remains an
owner-private backend.

The canonical contract is
[`OBJECT_API_LANES_v1.md`](../../design/00_meta-framework/OBJECT_API_LANES_v1.md).

## Alternatives Rejected

- Zone-backing every data-structure node: this gives storage nodes false
  semantic identity and couples allocator migration to API migration.
- Exporting generic CRUD and RCU traits from substrate: this moves domain
  meaning down and makes backend mechanics part of caller permissions.
- Giving identity, reservation, publication, projection, readiness, and ID
  allocation equal public lane status: this duplicates existing type/stage
  language and expands the global catalog without generic consumers.

## Consequences

- Upper callers see owner lanes and domain types, not concrete containers.
- Zone remains the lifetime substrate for semantic entities; binding values
  stay container-owned; observer nodes remain private.
- `Published<T>` is the first planned shared RCU publication primitive.
- VM `RecipeIndex` is the first migration candidate; its outer API and node
  allocator remain unchanged during the initial migration.
- Wake endpoints remain hints delivered through mailbox fanout. Publication
  precedes notification, and wake always causes fresh observation.

## Verification And Next Step

Documentation verification passed `cargo xtask lint docs`, scoped placeholder
and txdoc scans, and scoped `git diff --check`. `cargo xtask progress validate`
is currently blocked by the unrelated untracked
`docs/progress/plans/2026-07-14-vdso-migration.json`, whose top-level status is
`in_progress` rather than a plan-schema value (`proposed`, `active`, `blocked`,
`complete`, or `canceled`).

Next, review the exact upper interaction signatures and placement, then write a
staged implementation plan for the lane facade, publication primitive, linter
ratchets, and VM pilot. No Rust implementation is part of this decision.

## 2026-07-15 Interface Refinement

The upper interaction shape is accepted with one clarification: lane traits
are owner implementation contracts, not broad capabilities passed to checks,
scripts, shims, or other subsystems. Owner facades expose concern-specific
resolve, transition, projection, and readiness operations. Runtime read-only
filesystem or mount semantics remain domain authorization checks, not missing
mutation traits.

The owner catalog now makes `PidNamespace` and process-local `FdTable`
first-class owners, keeps VFS path resolution facade-only, makes
`MountNamespace` the sole mount-binding authority, and classifies registry
roots separately from semantic leaves. `ReadinessLane` receives a
caller-provided guard but returns an owned opaque report. The canonical object
catalog and exact domain type families are recorded in
`OBJECT_API_LANES_v1.md §11`.

Verification passed `cargo xtask lint docs`, scoped txdoc/placeholder/
whitespace scans, and scoped `git diff --check`. Full
`cargo xtask progress validate` is blocked by the unrelated
`docs/progress/plans/2026-07-14-elf-exec-loader.json`: one step uses
`in_progress`, while the step schema requires `in-progress`.

Next, turn the accepted catalog into a staged implementation plan. The first
slice should establish facade/ratchet scaffolding and `Published<T>`, then
migrate VM recipe publication without changing the `AddressSpace` facade.

## 2026-07-15 Lock-Replacement And I/O-Manager Amendment

The candidate audit refines "RCU candidate" into four owner-private
implementation families: published roots, single-binding publication,
per-entry atomic/state cells, and retained manager/reservation synchronization.
These are not new lane traits or caller-visible object languages.

The first two publication landings have different purposes:

- VM `RecipeIndex -> Published<RecipeTree>` is the correctness pilot because
  the live tree already implements immutable-root, guarded-read publication;
- `PageContainer.resident -> Published<ResidentRoot>` is the first performance
  target because cached page materialization currently contends with L4 and L6
  manager state under one coarse PC lock.

PageContainer publication therefore requires an ownership split, not only an
atomic-pointer replacement. L4 `PageIoSubmissionManager` and L6
`BlockSubmissionManager` own requests, queues, completions, graphs, tags,
depth, merge, and service state behind typed handles. PageContainer owns the
persistent resident sparse root, frame evidence, per-page generation/state,
and range coherency. Generation validation precedes resident installation;
withdrawal publication precedes waiter wake and range/direct-I/O reservation
release.

The expanded conditional matrix includes pmap observation metadata, network
configuration, process topology, user-namespace maps, userfaultfd ranges,
epoll interests, DEntry weak caches, and immutable filesystem mapping roots.
Each remains blocked until its owner transaction, retention, stale-observation,
and revocation/revalidation rules are explicit; pmap migration is additionally
measurement-gated. Queue/state-machine surfaces remain excluded.

`SocketTable` is the direct network publication target after wildcard/reuse,
same-owner pair installation, and cross-namespace compound visibility are
defined. Port occupancy remains an advisory projection; bind/listen
reservation remains authoritative. Filesystem lookup/directory/metadata caches
are conditional observational roots, distinct from authoritative extent and
journal state.

IPC and TTY require authority consolidation before publication. IPC global ID
tables, removed-ID lists, and namespace key/name maps move behind one
`IpcNamespace`; TTY hardware, aliases, PTY numbers, and pair installation move
behind one `TtyRegistry` reservation/receipt boundary.

The next implementation-plan checkpoint is now: intrusive per-CPU retirement
and `Published<T>`, recipe pilot, manager extraction, PageContainer resident
publication, then substrate `Index` guarded-read migration. Namespace/table
owners follow only after duplicate authorities and split commits are removed.

## 2026-07-18 RCU State-Machine Contraction

The RCU design uses one existing explicit object lifecycle state machine and
one epoch engine; it does not introduce four peer state languages.

Zone slots contract from five stable states to `Free`, `Reserved`, `Live`, and
`Retiring`. The final retention holder performs one non-yielding local-retire
transaction that first changes `Live(retain = 1)` to
`Retiring(next = none)`, coherently samples an epoch after that barrier, then
installs the previous bag head in metadata and publishes the slot as the new
head. There is no stable `Dead` state. In `Retiring`, the 32 retain bits hold
the next `SlotKey`, one spare metadata bit records `has_next`, and generation
remains unchanged until reclaim completes.

The epoch domain owns three tagged bags per CPU. Each bag has separate Zone
slot and generic `RcuHead` intrusive heads. Reader state is represented only by
`local_epoch == 0` versus a nonzero guarded epoch; bag phase is derived from the
bag heads, epoch tag, and global epoch rather than stored in another enum. A
ring slot must be empty before epoch advancement reuses it, so bounded reclaim
lag creates epoch backpressure instead of publication failure.

`Published<T>` is a linear protocol, not another state machine.
`PublishReservation` existence represents the prepared writer; commit performs
the release root swap and infallible intrusive enqueue before releasing the
writer claim, while reservation Drop leaves the old root authoritative.
Generic published allocations carry a private two-word `RcuHead`; Zone slots
add no header bytes. Because enqueue no longer allocates a retired descriptor,
the planned public `RetireTicket` and retire-capacity error are removed.

This is a target design amendment only. The live Rust implementation still has
the five-state Zone lifecycle and fixed per-CPU retired-node pools; migrating
those mechanics and their tests is the next substrate implementation slice.

Verification passed `cargo xtask lint docs`, `cargo xtask progress validate`,
scoped stale-vocabulary and placeholder scans, and scoped
`git diff --check`. Cargo emitted unrelated stale incremental-object cleanup
warnings while both xtask commands completed successfully.

## 2026-07-18 RCU Contract Review Closure

A three-way read-only review found that the layer split was consistent but the
algorithm contract still lacked retire/epoch linearization, reader ordering,
drain reentrancy, offline transfer, and fallible node-allocation rules. The
canonical documents now close those gaps.

`Published<T>` restores a narrow `PublishError::Allocation`: `try_new` and
`prepare_replace` allocate their private nodes before publication and may fail,
while `commit` remains infallible. `RetireTicket` and retire-capacity errors are
removed everywhere. The publication family is cross-crate-visible only to
owner implementations and must not enter facades, lanes, scripts, or shims.
Exclusive `Published<T>` Drop destroys only the current root; epoch bags retain
ownership of historical roots, and epoch shutdown drains them after global
quiescence.

The epoch contract now names the exact reader ordering, forbids nested owned
Guards and IRQ Guard entry, and gives `LocalRetireGuard` the CPU pin,
preemption/interrupt exclusion, local bag serialization, and `retire_active`
publication duties. Zone retirement and published-root replacement establish
their no-new-reader barrier first, then coherently sample the global epoch with
an AcqRel RMW, then use distinct crate-private SlotKey and `RcuHead` enqueue
methods. Advance scans reader epochs, active retire sections, and a stable CPU
membership version before one AcqRel increment.

Bounded drain removes a batch while locally serialized and runs callbacks only
after releasing local exclusion, so callback-driven recursive retirement cannot
overwrite remainder heads. Ring reuse requests owner-CPU maintenance and may
apply backpressure. CPU offline marks draining, waits without holding the
membership lock, then transfers all tagged heads before removing the CPU from
the epoch set. `SlotKey` capacity, zero-key handling, erased Zone reclaim
dispatch, four-state rollback/reclaim transitions, and generation-wrap
quarantine are also explicit.

This remains a documentation closure. The live five-state/fixed-pool Rust path
must be replaced test-first in the implementation slice.

Verification passed `cargo xtask lint docs`, `cargo xtask progress validate`,
scoped canonical-doc retired-vocabulary/placeholder scans,
trailing-whitespace scans, and scoped `git diff --check`. Cargo again emitted
only unrelated stale incremental-object cleanup warnings.
