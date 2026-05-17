# SysV / POSIX IPC — v1

<!-- txdoc:TXV3-SYSV-IPC-V1 -->

**Status.** v1 (Txv3 refresh, 2026-05).
**Purpose.** Specify the System V and POSIX IPC subsystem family (semaphores, shared memory, message queues) as a worked composition of v3 framework primitives. This doc is the canary that proves the v3 vocabulary is sufficient for a non-trivial Linux subsystem family without extending any closed catalog.
**Audience.** Subsystem authors implementing IPC; reviewers evaluating whether v3 primitives compose for a real Linux module.
**Companion documents.** `01_CONCEPTS_v5.md`, `02_INVARIANTS_v5.md`, `03_STEP_MODEL_v2.md`, `05_DELEGATE_v1.md`, `06_EXECUTION_SCOPE_v1.md`. Existing v4 dependencies: `NAMESPACE_VIEW_v1.md` (namespace lens), `PAGE_BACKED_v1.md` (shm segments), `SUBSYSTEM_ANATOMY_v2_1.md` (module layout), `SIGNAL_ATTACHMENTS_v1.md` (mq_notify signal).

---

## 1. Position

<!-- txdoc:IPC-V1-POSITION-1 -->

SysV and POSIX IPC compose from existing v3 primitives. The subsystem family adds:

- **Zero new closed-catalog members.** No new `YieldShape`, no new `ExecutionScope`, no new `AbortReason`, no new bus primitive, no new restriction kind.
- **Zero new framework cells.** Every operation is a typed `StepOp`; every block is `YieldShape::OnWaitSource`; every namespace lens applies through existing `NsProxy`.
- **One new subsystem module tree** (`tx-subsystems/src/ipc/`) and **one new NsProxy member** (`IpcNamespace`).

The implication is structural: if a 30-syscall family like SysV+POSIX IPC drops into v3 without architecture review, the framework's vocabulary is genuinely sufficient over canonical POSIX rather than just sufficient over the subsystems it was designed against.

---

## 2. Namespace placement

<!-- txdoc:IPC-V1-NAMESPACE-1 -->

`IpcNamespace` is a new member of `NsProxy`. `CLONE_NEWIPC` creates a fresh namespace; default and uninitialized clones share the parent's.

```rust
struct NsProxy {
    // existing
    pid: Cap<PidNamespace>,
    mnt: Cap<MountNamespace>,
    user: Cap<UserNamespace>,
    net: Cap<NetNamespace>,
    // …

    // new
    ipc: Cap<IpcNamespace>,
}

pub struct IpcNamespace {
    // SysV: keyed by integer key_t (or IPC_PRIVATE-generated id).
    sysv_sem: IndexTable<SysvKey, Cap<SemArrayIdentity>>,
    sysv_shm: IndexTable<SysvKey, Cap<ShmSegmentIdentity>>,
    sysv_msg: IndexTable<SysvKey, Cap<MsgQueueIdentity>>,

    // POSIX: keyed by name path.
    posix_mq: IndexTable<PosixMqName, Cap<MsgQueueIdentity>>,

    // Defaults, limits (e.g. SEMMNI, MSGMNB) per-namespace.
    limits: IpcLimits,
}
```

POSIX shared memory (`shm_open`) does **not** appear here — it resolves through the mount namespace's `/dev/shm` tmpfs, not through `IpcNamespace`. POSIX named semaphores resolve through `/dev/shm` similarly. Only the SysV-shaped keyed surfaces and the POSIX mq path-shaped surface live here.

Namespace lens rules from `NAMESPACE_VIEW_v1` apply: each script's `IpcNamespace` lookup is determined by its `SubjectContext.process.nsproxy.ipc`.

---

## 3. Subsystem module layout

<!-- txdoc:IPC-V1-LAYOUT-1 -->

Per `SUBSYSTEM_ANATOMY_v2_1`, each IPC kind is its own four-module subsystem:

```
tx-subsystems/src/ipc/
  namespace/                      // IpcNamespace itself
    structure.rs                  // identity table layouts
    execution.rs                  // step_clone_newipc, step_set_limits

  sysv_sem/
    structure/                    // SemArrayIdentity / SemArrayPayload
    execution/                    // step_semget, step_semop, step_semctl
    checks/                       // require_sem_op_permitted, require_owner_or_cap
    projection.rs                 // /proc/sysvipc/sem row

  sysv_shm/
    structure/                    // ShmSegmentIdentity / ShmSegmentPayload
    execution/                    // step_shmget, step_shmat, step_shmdt, step_shmctl
    checks/
    projection.rs                 // /proc/sysvipc/shm row

  sysv_msg/
    structure/                    // MsgQueueIdentity / MsgQueuePayload
    execution/                    // step_msgget, step_msgsnd, step_msgrcv, step_msgctl
    checks/
    projection.rs                 // /proc/sysvipc/msg row

  posix_mq/                       // POSIX message queue; thin wrapper over sysv_msg
    structure.rs                  // PosixMqInstance (fd-shaped handle)
    execution/                    // step_mq_open, step_mq_send, step_mq_receive,
                                  // step_mq_notify, step_mq_unlink
    projection.rs                 // /proc/<pid>/fdinfo entry shape
```

Each subsystem follows the standard observe → upgrade → reserve → commit → publish discipline. Each maintains its own `WaitSource`(s) on its payload; each yields `OnWaitSource` for blocking variants.

---

## 4. SysV semaphores

<!-- txdoc:IPC-V1-SEM-1 -->

### 4.1 Structure

```rust
pub struct SemArrayIdentity {
    id: SemArrayId,
    key: Option<SysvKey>,          // None for IPC_PRIVATE
    cred: Cap<Credential>,         // creator's
    perm: IpcPerm,
    nsems: u16,
}

pub struct SemArrayPayload {
    sems: [AtomicSemValue; nsems], // sized per-array; small-vec on heap
    sem_changed_source: WaitSource,
    sem_changed_seq: AtomicU64,    // bumped on every commit
    last_op: AtomicTime,
    waiters_index: SubscriberIndex, // owned by sem_changed_source
}
```

The `sem_changed_source` is the single `WaitSource` for the array. Any commit bumps `sem_changed_seq` before calling `notify()`; waiters' `PreparedPredicate::Sequenced` snapshots `sem_changed_seq` at observe time and re-checks under the source's lock at registration time.

### 4.2 step_semop

```rust
pub struct SemopOp {
    array: Cap<SemArrayPayload>,
    ops: SmallVec<[SemOp; 8]>,     // user-supplied op array, copied in
    undo_link: Option<Cap<SemUndoList>>, // if SEM_UNDO requested
    resume: Option<SemopResume>,
}

impl StepOp for SemopOp {
    type Output = ();              // success/failure flows through Result
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        let guard = epoch::guard();
        let payload = require_sem_array_alive(&self.array, &guard)?;
        let perm_w = require_sem_op_permitted(&payload, &self.ops, &ctx.subject, &guard)?;

        let seq_snapshot = payload.sem_changed_seq.load(Ordering::Acquire);

        match try_apply_atomically(&payload, &self.ops) {
            ApplyResult::Applied => {
                // Commit phase: ops applied via CAS, all-or-nothing.
                // Publish: bump seq and notify under source lock.
                if let Some(undo) = &self.undo_link {
                    undo.record_deltas(&self.ops);
                }
                payload.sem_changed_seq.fetch_add(1, Ordering::AcqRel);
                payload.sem_changed_source.notify(Interest::SemChanged);
                Done(())
            }
            ApplyResult::WouldBlock => {
                if self.ops.has_ipc_nowait() {
                    return Err(Errno::EAGAIN);
                }
                Yield {
                    progress: NoProgress::EMPTY,
                    shape: YieldShape::OnWaitSource {
                        source: payload.sem_changed_source.id(),
                        interests: Interest::SemChanged,
                        registration: prepared_predicate_for_semop(
                            &payload, &self.ops, seq_snapshot,
                        ),
                    },
                }
            }
            ApplyResult::Removed => Err(Errno::EIDRM),
        }
    }
}
```

### 4.3 Compound predicate

`prepared_predicate_for_semop` builds a `PreparedPredicate::Sequenced`:

```rust
PreparedPredicate::Sequenced {
    seq: payload.sem_changed_seq.clone(),
    observed: seq_snapshot,
}
```

Under the source's critical section, `register_prepared` reads `seq.load()` and compares to `observed`:

- If equal: the array hasn't changed since observation → register; on resume, `try_apply_atomically` re-runs against fresh state under fresh guard.
- If different: someone committed; return `ConditionChanged`; driver re-invokes step.

This is YIELD-10-compliant: the predicate is one atomic load, no lock, no allocation, no subsystem callback.

### 4.4 SEM_UNDO

Per-process undo list lives on `ProcessPayload`:

```rust
pub struct ProcessPayload {
    // existing fields …
    sem_undos: SmallMap<SemArrayId, Cap<SemUndoList>>,
}
```

When `step_semop` commits ops with `SEM_UNDO`, it records the inverse deltas in the process's undo list for that array. At process exit, the existing exit step (which publishes on `Process.exit_source`) walks `sem_undos`, applies each undo against the still-live arrays, and drops the records. Already-deleted arrays produce silent no-ops.

The exit-step's walk is a normal multi-step sequence: each undo application is a sub-step of `step_process_exit`. No new framework hook.

### 4.5 step_semctl IPC_RMID

```rust
pub struct SemctlRmidOp {
    array_id: SemArrayId,
}

impl StepOp for SemctlRmidOp {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        let guard = epoch::guard();
        let array_cap = require_sem_array_resolvable(self.array_id, &ctx.subject, &guard)?;
        let perm_w = require_sem_array_remove_perm(&array_cap, &ctx.subject, &guard)?;
        let payload_cap = upgrade_cap(&array_cap)?;
        let payload = payload_cap.payload_ref()?;

        // Reserve: namespace withdrawal token.
        let withdraw_tok = ctx.ipc_ns()?.sysv_sem.reserve_withdraw(self.array_id)?;

        // Commit: withdraw from namespace.
        ipc_index::withdraw_commit(withdraw_tok);

        // Publish: abort all waiters.
        for waiter in payload.sem_changed_source.iter_subscribers() {
            if let Some(mailbox) = waiter.mailbox.upgrade() {
                mailbox.post(WakeHint::Abort {
                    generation: Some(waiter.generation),
                    reason: AbortReason::Canceled,
                });
            }
        }

        Done(())
    }
}
```

Waiters resume seeing `Aborted(Canceled)`. The SysV `semop` script (the driver-level composition over `SemopOp`) catches the abort and translates to `errno = EIDRM` rather than the default `EINTR`:

```rust
async fn sys_semop(...) -> Result<(), Errno> {
    let op = SemopOp::new(...);
    match drive(op, ctx, DriveMode::Waiting, protocol).await {
        Ok(()) => Ok(()),
        Err(Errno::EINTR) if op.was_object_removed() => Err(Errno::EIDRM),
        Err(e) => Err(e),
    }
}
```

`op.was_object_removed()` is a script-level check: re-resolve the array_id against the namespace; if not present, the abort was an `IPC_RMID`, not a signal interrupt. This is the **Canceled + script-side errno mapping** path described in §8.

---

## 5. SysV shared memory

<!-- txdoc:IPC-V1-SHM-1 -->

### 5.1 Structure

```rust
pub struct ShmSegmentIdentity {
    id: ShmSegmentId,
    key: Option<SysvKey>,
    cred: Cap<Credential>,
    perm: IpcPerm,
    size: usize,
}

pub struct ShmSegmentPayload {
    // The backing is a standard PageBacked anonymous shared region.
    backing: Cap<PageBackedAnonShared>,
    attach_count: AtomicU32,
    marked_for_deletion: AtomicBool,
}
```

The segment **is** a `PageBacked` object per `PAGE_BACKED_v1`. No new memory subsystem; `shmat` is `mmap` against the segment's backing.

### 5.2 Operations

- `step_shmget`: reserve segment slot + frame allocation + IPC namespace index commit. One-step.
- `step_shmat`: thin wrapper over `vm::MmapOp` with the segment's backing. Increments `attach_count`. Address-hint and `SHM_RND` are existing `mmap` features.
- `step_shmdt`: thin wrapper over `vm::MunmapOp` over the matching VMA. Decrements `attach_count`.
- `step_shmctl_rmid`: namespace-withdraw + set `marked_for_deletion`. Does **not** abort attachers; reclamation happens when the last attacher detaches and the last `Cap` drops.

The marked-for-deletion + last-detach-reclaims behaviour is standard Cap-refcount semantics with one extra atomic flag to gate new `shmat` calls:

```rust
fn require_shmat_permitted(seg: &ShmSegmentPayload, ...) -> Result<...> {
    if seg.marked_for_deletion.load(Ordering::Acquire) {
        return Err(Errno::EIDRM);
    }
    // perm check, namespace lens, …
}
```

No new framework. The segment is a regular `PageBacked` Cap from the framework's POV; only the IPC subsystem maintains the `attach_count` and `marked_for_deletion` semantics.

---

## 6. SysV message queues

<!-- txdoc:IPC-V1-MSG-1 -->

### 6.1 Structure

```rust
pub struct MsgQueueIdentity {
    id: MsgQueueId,
    key: Option<SysvKey>,
    cred: Cap<Credential>,
    perm: IpcPerm,
    limits: MsgQueueLimits,
}

pub struct MsgQueuePayload {
    // Per-type FIFO buckets. Each bucket is its own intrusive linked list.
    buckets: SmallMap<MsgType, MsgFifo>,
    used_bytes: AtomicUsize,
    total_msgs: AtomicU32,

    // Two wait sources: one for space, one for arrivals.
    send_source: WaitSource,
    recv_source: WaitSource,

    // Sequence counter shared by both sources (any change bumps it).
    queue_seq: AtomicU64,
}
```

Per-type buckets are the key implementation choice: they make `msgrcv` predicates for `msgtyp > 0` an atomic load (`bucket[type].count`) rather than a list walk.

### 6.2 step_msgsnd

```rust
impl StepOp for MsgsndOp {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        let guard = epoch::guard();
        let payload = require_msg_queue_alive(&self.queue, &guard)?;
        require_msg_send_perm(&payload, &ctx.subject, &guard)?;

        let used = payload.used_bytes.load(Ordering::Acquire);
        if used + self.msg_len <= payload.limits.max_bytes {
            // Has space; commit.
            commit_send(&payload, self.msg_type, &self.msg_body)?;
            return Done(());
        }

        if self.flags.contains(IPC_NOWAIT) {
            return Err(Errno::EAGAIN);
        }

        // Pattern A: atomic predicate "used + len <= cap".
        Yield {
            progress: NoProgress::EMPTY,
            shape: YieldShape::OnWaitSource {
                source: payload.send_source.id(),
                interests: Interest::QueueHasSpace,
                registration: PreparedWaitRegistration {
                    source: payload.send_source.downgrade(),
                    interests: Interest::QueueHasSpace,
                    predicate: PreparedPredicate::Atomic {
                        check: predicate_msgsnd_has_space,
                        data: pack_atomic_data(&payload.used_bytes, self.msg_len,
                                               payload.limits.max_bytes),
                    },
                },
            },
        }
    }
}
```

### 6.3 step_msgrcv

The predicate depends on `msgtyp`:

| `msgtyp` | Predicate |
|---|---|
| `0` (any) | atomic: `payload.total_msgs > 0` |
| `> 0` (exact) | atomic: `payload.buckets[msgtyp].count > 0` |
| `< 0` (≤ \|t\|) | **sequenced**: snapshot `queue_seq`; under source lock, check `queue_seq == observed`; if changed, retry step |

The `msgtyp < 0` case is the only one needing the sequenced pattern, and it's the same shape as `semop`'s compound predicate. No new mechanism.

```rust
fn build_msgrcv_predicate(payload: &MsgQueuePayload, msgtyp: i32, seq_snapshot: u64)
    -> PreparedPredicate
{
    match msgtyp {
        0 => PreparedPredicate::Atomic {
            check: predicate_total_msgs_nonzero,
            data: pack_atomic_data(&payload.total_msgs),
        },
        t if t > 0 => PreparedPredicate::Atomic {
            check: predicate_bucket_nonempty,
            data: pack_atomic_data(&payload.buckets[t].count),
        },
        _ /* t < 0 */ => PreparedPredicate::Sequenced {
            seq: payload.queue_seq.clone(),
            observed: seq_snapshot,
        },
    }
}
```

### 6.4 IPC_RMID for msg queues

Same shape as semaphore IPC_RMID: namespace-withdraw + abort both `send_source` and `recv_source` waiters with `AbortReason::Canceled` + script-side errno → `EIDRM`.

---

## 7. POSIX wrinkles

<!-- txdoc:IPC-V1-POSIX-1 -->

### 7.1 POSIX semaphores

- **Named (`sem_open`)**: file in `/dev/shm` (a tmpfs mount). Open is a normal VFS path resolution; the file's payload is a `SemArrayPayload` with `nsems = 1`. Reuses §4 entirely.
- **Anonymous (`sem_init` in shared memory)**: lives in user-mapped shared memory. The kernel sees only the futex-shaped operations (`futex(FUTEX_WAIT, ...)`); these are handled by the futex subsystem, not by IPC. Out of scope for this doc.

### 7.2 POSIX shared memory

`shm_open` + `mmap`. `shm_open` is a normal `open` against `/dev/shm`. `mmap` is normal VM mmap with a shared-anonymous backing. **Zero IPC subsystem code** beyond the `/dev/shm` mount setup.

### 7.3 POSIX message queues

Thin wrapper over §6 with fd-shaped handles:

```rust
pub struct PosixMqInstance {
    queue: Cap<MsgQueuePayload>,     // shared with sysv_msg
    name: PosixMqName,               // path-shaped signifier
    open_flags: OpenFlags,
}
```

- `mq_open` resolves the name against `IpcNamespace.posix_mq`, returns an fd.
- `mq_send`/`mq_receive` reuse `MsgsndOp`/`MsgrcvOp` from §6.
- `mq_timedsend`/`mq_timedreceive`: same ops, driven with `WaitProtocol.deadline = Some(...)`. **No new mechanism** — `OnWaitSource + timeout` is exactly the protocol-deadline pattern from `03_STEP_MODEL_v2 §5.1`.
- `mq_unlink`: namespace withdraw; existing fd holders keep the queue alive until close.

### 7.4 mq_notify with SIGEV_SIGNAL

`mq_notify` registers a one-shot signal-attachment subscription on the queue: when a message arrives at a previously-empty queue, fire a signal at the registered process. This is a `SIGNAL_ATTACHMENTS_v1` row:

```
transition:   MsgQueue.first_message_arrived
publishes on: MsgQueue.notify_signal_port
delivers:     SIGEV_SIGNAL to registered process
```

The one-shot guard is "registered ∧ queue was empty at register time ∧ no notify has fired since registration." Implemented as a single `AtomicOption<NotifyRegistration>` on the payload, CAS'd to None on fire. No new framework cell.

**`SIGEV_THREAD`** is glibc-side (a userspace thread is spawned to consume the signal). The kernel doesn't see it.

---

## 8. IPC_RMID + waiter abort

<!-- txdoc:IPC-V1-RMID-1 -->

The design decision for IPC removal-while-waiting:

**Use `AbortReason::Canceled` + script-side errno mapping. Do not add `AbortReason::ObjectRemoved`.**

Rationale:

1. The IPC script knows it issued a wait against an IPC object. On `Aborted(Canceled)`, it has the context to distinguish "signal" from "object removed" (re-resolve the id; if not present in namespace, removal happened).
2. Keeping `AbortReason` minimal preserves the closed-catalog discipline (one fewer member; one less catalog-extension review).
3. The errno is `EIDRM` for IPC, but other subsystems may want different errnos for the same logical case; the script-layer translation is the right place for that policy.

The pattern is general: any future subsystem with "object withdrawn while waiter parked" semantics maps `Aborted(Canceled)` to its own subsystem-appropriate errno at the script layer.

If a second subsystem with identical semantics emerges, this decision can be revisited and `AbortReason::ObjectRemoved` can be admitted with an ARCH-3 review. Until then, the catalog stays at five members.

---

## 9. /proc/sysvipc projection

<!-- txdoc:IPC-V1-PROC-1 -->

`/proc/sysvipc/{sem,shm,msg}` is a projection over `IpcNamespace.sysv_*` tables. Per `MAP-10` and the projection-row pattern from `NAMESPACE_VIEW_v1`:

```
projection-row source: IpcNamespace.sysv_sem table iteration
filter:                ipc namespace lens (the reader's nsproxy.ipc)
render:                fixed-format text rows per array
```

Each iteration acquires an epoch guard, walks the table, renders each row from the `SemArrayIdentity` + (epoch-guarded read of) `SemArrayPayload` fields. Mid-mutation reads see consistent per-array state because all per-array mutations linearize against the array's own commits.

POSIX mq has no `/proc` counterpart; per-fd state is in `/proc/<pid>/fdinfo` via the normal fd-projection mechanism.

---

## 10. Invariants

<!-- txdoc:IPC-V1-INVARIANTS-1 -->

**IPC-1. IPC objects are namespace-scoped.** Every SysV/POSIX-named IPC object is reachable only through its containing `IpcNamespace`. Cross-namespace access is not possible.

**IPC-2. Resource removal is namespace withdraw + waiter abort.** `IPC_RMID` (semctl/msgctl/shmctl) and `mq_unlink` withdraw the namespace binding and post `WakeHint::Abort { reason: AbortReason::Canceled }` to every waiter on every `WaitSource` owned by the object's payload. The script layer translates to the subsystem-specific errno (`EIDRM` for SysV).

**IPC-3. `SEM_UNDO` is per-process state.** The undo list lives on `ProcessPayload`; the process-exit step walks it before payload teardown, applying each undo against still-live arrays. Already-deleted arrays produce silent no-ops.

**IPC-4. Per-object ordering only.** IPC operations on different objects are not globally ordered. Operations on a single object linearize through that object's own commit primitives (single-sem CAS, atomic ring head/tail, etc.).

**IPC-5. `shmat` requires non-deleted backing.** A `shmat` after `IPC_RMID` returns `EIDRM`. The check is an atomic load on `ShmSegmentPayload.marked_for_deletion` in the upper-half observe phase.

**IPC-6. POSIX shm flows through tmpfs.** `shm_open` is `open(/dev/shm/...)`. The kernel does not maintain a separate POSIX-shm namespace.

**IPC-7. Compound predicates use the sequenced pattern.** `semop` (multi-op atomic check) and `msgrcv` (msgtyp < 0 multi-bucket check) build their `PreparedPredicate::Sequenced` against the object's `seq` counter; producers bump the counter before `WaitSource.notify()`. This is YIELD-2-compliant.

**IPC-8. `mq_notify` is a one-shot signal attachment.** The notification is governed by `SIGNAL_ATTACHMENTS_v1`; the one-shot guard is the existing publication-rule's "atomic with publication" property.

---

## 11. Implementation phases

<!-- txdoc:IPC-V1-PHASES-1 -->

### Phase IPC-1: shm (lowest difficulty)

Land:

```text
ShmSegmentIdentity / ShmSegmentPayload
IpcNamespace.sysv_shm table
step_shmget, step_shmat, step_shmdt, step_shmctl
/proc/sysvipc/shm projection
```

shm is the simplest because the only blocking operation is page-fault-on-attach, which is already handled by the existing VM fault path. No new `WaitSource`, no new predicate.

### Phase IPC-2: msg (medium difficulty)

Land:

```text
MsgQueueIdentity / MsgQueuePayload
per-type buckets + queue_seq counter
send_source / recv_source WaitSources
step_msgget, step_msgsnd, step_msgrcv (atomic + sequenced predicates), step_msgctl
/proc/sysvipc/msg projection
```

msgrcv with `msgtyp < 0` exercises the `Sequenced` predicate pattern; first real user beyond the runtime spec's examples.

### Phase IPC-3: sem (highest difficulty)

Land:

```text
SemArrayIdentity / SemArrayPayload
sem_changed_source WaitSource + sem_changed_seq
step_semget, step_semop (multi-op atomic / sequenced predicate), step_semctl
SEM_UNDO: ProcessPayload.sem_undos + exit-step walk
/proc/sysvipc/sem projection
```

Highest difficulty because of `SEM_UNDO`'s exit-step integration and the compound op-array atomicity. Both fit the existing primitives; the implementation is just larger.

### Phase IPC-4: POSIX wrappers

Land:

```text
PosixMqInstance fd-shaped wrapper
step_mq_open, step_mq_send, step_mq_receive, step_mq_notify, step_mq_unlink
step_mq_timedsend, step_mq_timedreceive (uses WaitProtocol.deadline)
POSIX sem named: /dev/shm tmpfs file + SemArrayPayload reuse
mq_notify SIGEV_SIGNAL attachment row
```

Mostly thin wrappers; the heavy lifting was done in IPC-3.

Total estimate across IPC-1 through IPC-4: **~4,000 LoC of subsystem code**, zero framework changes.

---

## 12. Non-goals

<!-- txdoc:IPC-V1-NEGATIVE-1 -->

This subsystem family must not do the following:

```text
Do not introduce a new YieldShape.
Do not introduce a new ExecutionScope.
Do not introduce a new AbortReason variant (use Canceled + script errno).
Do not introduce a new bus primitive.
Do not introduce a new RestrictionKind.
Do not implement SysV semaphores' deprecated obsolete-IPC-modes.
Do not maintain a global ordering across distinct IPC objects.
Do not run user-supplied filters under the WaitSource critical section (WAIT-2).
Do not implement SIGEV_THREAD (glibc concern, not kernel).
Do not implement POSIX sem anonymous-in-shared-memory (futex territory).
```

These exclusions exist to keep the subsystem within the closed-catalog discipline and to clarify the seam between IPC and futex/VM.

---

## 13. Summary

<!-- txdoc:IPC-V1-SUMMARY-1 -->

SysV and POSIX IPC fit v3 cleanly. Specifically:

- Each IPC kind is a typed subsystem with `Cap`-managed identity/payload and one or two `WaitSource`s.
- All blocking operations yield `OnWaitSource` with a `PreparedPredicate` — atomic for simple checks, sequenced for compound checks.
- `IPC_RMID` is namespace withdraw + waiter abort with `AbortReason::Canceled`; the script layer maps to `EIDRM`.
- `SEM_UNDO` rides the existing process-exit step.
- POSIX equivalents are thin fd-shaped wrappers over the SysV payloads.
- `/proc/sysvipc` is a standard projection.
- `mq_notify` is a standard signal-attachment row.

**Zero new closed-catalog members. Zero new framework cells.** The doc is the framework's canary: if a 30-syscall family with `SEM_UNDO`, `IPC_RMID`-while-waiting, per-type message filtering, and POSIX-fd-shaped overlays drops in without architecture review, the v3 vocabulary holds.
