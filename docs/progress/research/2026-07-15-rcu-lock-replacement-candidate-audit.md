# RCU And Lock-Replacement Candidate Audit

Date: 2026-07-15

## Scope

This read-only audit expands the initial candidate matrix in
`OBJECT_API_LANES_v1.md`. It covers owner/root publication, single-binding
slots, per-entry state, and the PageContainer-to-device I/O path. Active design
documents and the live checkout are the evidence base.

The initial multi-agent fanout call hit the thread limit and did not return
waitable IDs synchronously. Delayed readers later reported VM,
PageContainer/I/O, process/FD, IPC/TTY, filesystem/device, and networking
findings. Their results were integrated and every completed agent was closed.
Local spot checks used CodeGraph first and focused source searches afterward.

## Result: Four Replacement Families

| Family | Replaces | Does not replace |
|---|---|---|
| `Published<T>` root | lock-held observation of immutable binding/index versions | writer serialization, semantic reservations, queues |
| private single-binding publication | lock-backed `Option<Cap/Weak/PayloadCap>` and staging `AtomicSlot<T>` | domain lifecycle/revocation checks |
| per-entry atomic/state cell | hot state checks such as resident/withdrawn/referenced | multi-step dirty/writeback/fetch transitions |
| manager/reservation synchronization | nothing through RCU; retain or isolate it | submission queues, completions, range conflicts, protocol state |

RCU is therefore one backend mechanism inside the limited owner language, not
the universal replacement for every `SpinMutex`.

## PageContainer And I/O Submission

### Current hot path

The cached file-page path enters `PageContainer::materialize_page`, then
`materialize_cached_page`, then `materialize_existing_page`. A normal file hit
can acquire the same `PageContainerState` lock four times: range-conflict
observation, lookup, pin snapshot, and post-pin revalidation/mark update.

That lock also protects all of the following:

- `PageCacheIndex` resident bindings;
- `PageSlot` and in-flight fetch maps;
- `PageService` submissions, completions, backend resumes, metadata graphs,
  graph executions, and waiters;
- direct-I/O leases and range reservations;
- `BlockQueue`, trackers, queue depth, and tags.

This is the most important read-side split. Resident hits currently contend
with L4 page submission and L6 block service turns.

### Live manager names

There is no live type named exactly `PageIoSubmissionManager`. The current L4
staging implementation is `PageService`; it owns a `PageRequestQueue` plus
completion and continuation state. The current L6 staging implementation is
`BlockQueue` plus `QueueDepth`, `BlockTagTable`, and request trackers. Both are
embedded under `PageContainerState` today.

`IO_MANAGER_v1.md` already defines the architectural owner as the I/O manager:
PageContainer owns ordinary file data and page state, while L4 owns page
submission and L6 owns block submission. The live embedding is therefore a
staging seam, not the final ownership boundary.

### Required split

```text
PageContainer
  resident: Published<ResidentRoot>
  slots: private stable ResidentPage/PageSlot cells
  io: PageIoSubmissionHandle

PageIoSubmissionManager (L4 owner)
  PageRequestQueue
  completions / backend resumes
  metadata and graph continuations
  waiter routing

BlockSubmissionManager (L6 owner)
  BlockQueue
  QueueDepth
  BlockTagTable
  completion trackers
```

The resident read path becomes guarded root lookup, resident-state validation,
owned `MapPin` acquisition, and an atomic referenced update. It does not enter
either manager.

The managers remain mutable single-owner/service state machines. Submission is
an ownership-transfer commit into a bounded queue or mailbox, not an RCU root
replacement. Extracting these managers also removes their queue activity from
the resident-hit contention domain.

### PC publication ordering

- Install: reserve the stable resident cell and index path, complete content
  initialization, transition the cell to resident, publish the new root, then
  wake waiters.
- Invalidate: reserve the page/range transition, mark the old cell withdrawn,
  publish a root without the binding, retire the old root/path, then release
  frame evidence after outstanding map pins permit it.
- Dirty/writeback/redirty: mutate the stable per-page state machine; do not
  publish a new resident root.
- Fetch/completion: remain generation-checked manager/PageSlot transitions;
  only successful resident installation changes the root.

A whole-map `Published<BTreeMap<...>>` is not acceptable for PC because page
install, readahead, and reclaim would copy the complete map. PC requires a
persistent sparse/radix index with bounded path-copy updates.

## Direct And Conditional Publication Candidates

| Candidate | Current path | Published state | Synchronization retained | Readiness |
|---|---|---|---|---|
| `AddressSpace` recipe index | manual `AtomicPtr<RecipeTree>` plus writer mutex | `Published<RecipeTree>` | `RangeLock`, pmap commit, writer reservation | direct pilot; mechanics already exist |
| `PageContainer` resident index | `PageContainerState` lock around `PageCacheIndex` | persistent sparse `ResidentRoot` | PageSlot transitions, range/direct-I/O reservations, L4/L6 managers | first performance target |
| `VmPmap` metadata | one `VmSpinMutex<VmPmapState>` for lookup, walk, publish, teardown | published sparse mapping-observation root | hardware pmap commit, shootdown, `MapPin`, writer reservation | conditional; separate metadata from hardware authority |
| `SocketTable` / substrate `Index<K,V,N>` | 13 protocol-specific indices; `lookup(&Guard)` still takes an internal spin lock | one owner-private binding snapshot or guarded fixed-index committed state | wildcard/reuse, pair-install, port-allocation, and withdrawal reservations | direct network target after compound visibility is defined |
| `PidNamespace` | global locked numeric-role map | numeric-name binding root | ID/quota and lifecycle reservation | direct after first-class owner lands |
| `FdTable` | separate fd map and CLOEXEC locks plus split allocate/install calls | one fd-entry/flag root | quota, multi-slot close/dup/pair reservation | direct after facade and root consolidation |
| `MountNamespace` | namespace vector plus global `MOUNT_TABLE` | sole mountpoint binding root | attach/detach/move/stack reservation | direct only after duplicate authority is removed |
| `IpcNamespace` | namespace key maps plus global sem/shm/msg/mq ID maps | namespace-owned key and ID roots | ID/quota allocation and object removal reservation | direct only after global tables move into owner |
| `TtyRegistry` | three locked slot arrays plus locked PTY allocator | hardware/alias/PTY binding root | registration, alias collision, PTY number/pair-install reservation | conditional until the owner and atomic registration boundary exist; rings and transport excluded |
| `NetNamespace` configuration | locked device, route, suppression, and extra-address vectors | immutable network configuration root(s) | net-admin mutation reservation | strong additional candidate |
| `ProcessTopology` rosters | locked child/thread/pgrp/session vectors | published roster roots | fork/exit/reparent/setpgid/setsid aggregate reservation | conditional after topology owner transaction lands |
| `UserNamespace` ID maps | locked UID/GID vectors plus separate written flags | write-once immutable UID/GID map values | one-shot authorization/write reservation | direct single-publication candidate |
| userfaultfd registrations | locked registration vector; pending faults separate | published registered-range root | unregister/fault ordering; pending queue remains locked | conditional, read-hot fault candidate |
| epoll interest set | one fd map mixes interests with `last_ready` and oneshot state | published interest root pointing at stable delivery cells | ready/delivery state and wait routing | conditional after binding/state split |
| DEntry child cache | locked weak child map with read-side stale cleanup | private published child-cache root | filesystem lookup and rename/unlink authority | later; needs explicit stale-name revalidation |
| mounted filesystem mapping cache | locked extent/mapping metadata root | published immutable mapping/extent root | journal, allocation, truncate/hole/block-reuse generation rules | strong downstream conditional candidate |
| mounted filesystem read cache | locked lookup/directory/metadata caches with read-side LRU mutation | published observational read snapshot | positive/negative invalidation generation and independent recency accounting | conditional after invalidation authority is unified |

## Single-Binding Publication Candidates

These should not create additional upper API vocabulary. Their owners expose
typed methods while the implementation uses the same private publication
substrate.

- `AtomicSlot<T>` is currently `SpinMutex<Option<T>>`; process VM, credential,
  namespace, net-namespace, TTY termios, and TTY session/pgrp slots therefore
  remain lock-backed staging cells.
- Process, thread, socket, TTY, IPC, and net-namespace identity payload slots
  still use locked `Option<PayloadCap<_>>` in several modules.
- Process parent, process-group, cwd, executable, and controlling-TTY bindings
  are single relation slots, not map roots.
- Boot-once initial process/namespace and lazy wait-point slots should use a
  boot/once cell where mutation is truly one-shot, not RCU.

The public `AtomicSlot` name should eventually disappear from semantic owner
surfaces. It can become a private wrapper over guarded publication while owner
facades keep domain-shaped load/swap operations.

## Retire Or Ownerify Instead Of Publishing In Place

| Current global registry | Preferred disposition |
|---|---|
| raw wait-source registry | retire compatibility IDs as endpoints become self-contained; do not freeze the registry into a new public RCU API |
| `FILE_IO_SERVICE_RUNTIMES` | move service membership to the I/O-manager/runtime owner; a published weak snapshot is only a staging option |
| PageContainer reclaim weak registry | move to reclaim-service ownership or a published weak snapshot; low priority |
| wall-clock timerfd and signalfd routing registries | express lifecycle attachment/owner registries first; publication is secondary |
| static block/net device registries | keep current boot-once publication; they already avoid runtime reader locks |

## False Positives: Keep Mutable Coordination

The following lock-backed containers are not direct RCU candidates:

- `RangeLock`, futex buckets/exact waiters, and `FLOCK_TABLE`;
- `PageService`, `PageRequestQueue`, `BlockQueue`, completion trackers, and
  direct-I/O leases;
- epoll delivery/ready state;
- socket protocol state, receive/transmit buffers, backlog, and packet queues;
- ARP/NDISC/reassembly and bridge-learning caches, which are mutable protocol
  caches with expiry/high churn;
- SysV message contents, semaphore values/undo state, shared-memory attachment
  lists, and POSIX MQ contents;
- TTY rings, line discipline, transport, and ingest serialization;
- signal-pending, AIO, io_uring, userfaultfd pending-fault, and device queues.

These may need sharding, per-entry cells, mailbox ownership, or bounded rings,
but replacing their lock with immutable-root RCU would not preserve their
state-machine semantics.

## Substrate Blockers

1. `Published<T>` and `PublishReservation` exist only in the design document.
2. `epoch::retire_raw` can fail with `RetiredNodePoolExhausted` after a swap;
   there is no retire-capacity reservation/ticket API yet.
3. `Index::lookup(&Guard)` still takes its internal spin lock.
4. `AtomicSlot<T>` still takes a `SpinMutex<Option<T>>` for load/store/swap.
5. PC and pmap need persistent sparse path-copy storage; whole-root map copies
   are not an acceptable steady-state algorithm.
6. Publication shutdown/drop, old-root destructor bounds, concurrent tests,
   and raw-pointer/retire allowlist ratchets are not implemented.

## Recommended Landing Order

1. Add retire-capacity reservation and implement/test `Published<T>`.
2. Fold manual `RecipeIndex` publication into `Published<RecipeTree>` as the
   correctness pilot.
3. Split PC resident state from L4 `PageService` and L6 block runtime; extract
   manager ownership/handles.
4. Add persistent resident index and remove the PC global lock from cached
   reads.
5. Make substrate `Index` guarded reads actually lock-free; validate all
   SocketTable indices.
6. Split and migrate pmap metadata if measurements still identify it as the
   next VM read bottleneck.
7. Land first-class `FdTable`, `PidNamespace`, sole-authority
   `MountNamespace`, and sole-authority `IpcNamespace` facades before changing
   their storage.
8. Migrate TTY, network configuration, topology, user namespace, userfaultfd,
   epoll interest, and DEntry cache roots in that order of demonstrated need.

## Evidence Anchors

| Claim | File:line | Confidence |
|---|---|---|
| PC resident and L4/L6 state share one lock | `crates/tx-subsystems/src/page_backed/mod.rs:649`, `:667` | high |
| cached materialization repeatedly enters PC state | `crates/tx-subsystems/src/page_backed/mod.rs:3064`, `:3166` | high |
| L4 live owner is `PageService` | `crates/tx-subsystems/src/io_manager/page/service.rs:355` | high |
| L6 live queue is `BlockQueue` | `crates/tx-subsystems/src/io_manager/block/mod.rs:439` | high |
| design assigns L4/L6 control-plane ownership to I/O manager | `docs/design/05_filesystem/IO_MANAGER_v1.md:75`, `:174`, `:242` | high |
| substrate Index guarded reads still lock | `crates/tx-substrate/src/index.rs:90`, `:141` | high |
| AtomicSlot is still lock-backed | `crates/tx-substrate/src/slot.rs:19`, `:59` | high |
| pmap observations share the mutation lock | `crates/tx-subsystems/src/vm/pmap.rs:157`, `:185` | high |
| socket owner has 13 fixed-capacity Index roots | `crates/tx-subsystems/src/net/structure/table.rs:116` | high |
| namespace network configuration is split across locked vectors | `crates/tx-subsystems/src/net/namespace.rs:62` | high |
| fd bindings and flags are separate locked containers | `crates/tx-subsystems/src/process/structure.rs:1156`, `:1175` | high |
| mount bindings have duplicate namespace/global authority | `crates/tx-subsystems/src/mount/mod.rs:700`, `:898` | high |
| IPC namespace maps coexist with global ID maps | `crates/tx-subsystems/src/process/nsproxy.rs:131`, `crates/tx-subsystems/src/ipc/sysv_shm/structure.rs:146` | high |
| user namespace maps are write-once but lock-read | `crates/tx-subsystems/src/process/nsproxy.rs:208`, `:263` | high |
| userfaultfd registration and pending queue are separable | `crates/tx-subsystems/src/userfaultfd/mod.rs:160`, `:194` | high |
| epoll mixes registration and delivery state in one map | `crates/tx-subsystems/src/epoll/mod.rs:38`, `:82` | high |
| ext4 mapping reads currently serialize on one metadata-table lock | `crates/tx-ext4/src/planner.rs:81`, `:148` | high |
| retire can fail for lack of a retired node | `crates/tx-substrate/src/epoch/domain.rs:29`, `:292` | high |

## Verification

- Read-only CodeGraph and focused source inspection.
- Canonical contracts were updated in `OBJECT_API_LANES_v1.md`, v5 `LANE-*`,
  `PAGE_BACKED_v1.md`, and `IO_MANAGER_v1.md`; the decision and status records
  were synchronized.
- Scoped tracked and untracked whitespace checks produced no diagnostics.
- Scoped duplicate-`txdoc:` and placeholder scans produced no findings.
- `cargo xtask lint docs` passed with the existing seven stale-vocabulary
  warnings.
- `cargo xtask progress validate` passed all 34 progress records.
- This documentation pass changed no runtime Rust code.

The implementation blocker is unchanged: retirement capacity cannot yet be
reserved before root publication, so `Published<T>` cannot provide infallible
post-commit retirement. The next step is the substrate implementation plan,
followed by the recipe pilot and manager extraction.
