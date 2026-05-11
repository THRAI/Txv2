# ExecutionScope — `OnBehalfOf<P>` — v1

<!-- txdoc:TXV3-EXECUTION-SCOPE-V1 -->

**Status.** v1 (Txv3 refresh, 2026-05).
**Purpose.** Specify `ExecutionScope` as a script-context modifier orthogonal to `YieldShape`, and the `OnBehalfOf<P>` member that lets a kernel task execute scripts under a borrowed `Cap<ProcessIdentity>`. Coverage: io_uring SQPOLL, AIO workers, FUSE helper threads, network softirq.
**Audience.** Subsystem authors building long-lived in-kernel actors; reviewers evaluating proposed kthread-on-behalf-of designs.
**Companion documents.** `01_CONCEPTS_v5.md §12`, `02_INVARIANTS_v5.md (SCOPE-*)`, `04_SYSCALL_SHAPE_v1.md §3.2`.

---

## 1. Scope vs Yield

<!-- txdoc:SCOPE-V1-SHAPE-1 -->

The cleavage from `01_CONCEPTS_v5 §12`, restated:

> **YieldShape governs script pause/resume.** Point-shaped: the script halts at a point and is resumed when a condition resolves.
>
> **ExecutionScope governs script identity context.** Extent-shaped: the entire script runs under a particular `SubjectContext`.

The two compose orthogonally. A script running inside `OnBehalfOf<P>` may emit any YieldShape; a yield does not enter or leave a scope.

The cleavage matters because conflating them produces vocabulary creep. "OnBorrow" as a YieldShape is the wrong primitive: a borrow is not a thing the script *waits on*; it is the identity context the script *runs as*.

## 2. The closed catalog

<!-- txdoc:SCOPE-V1-CATALOG-1 -->

```rust
enum ExecutionScope {
    Thread,                              // default: identity from running thread's task
    OnBehalfOf(Cap<ProcessIdentity>),    // borrowed from another process
    // future closed-catalog members via ARCH-3
}
```

`Thread` is the implicit scope for native syscall scripts; `OnBehalfOf` is for kernel actors. Catalog extension requires architecture review.

## 3. The borrow primitive

<!-- txdoc:SCOPE-V1-PRIMITIVE-1 -->

```rust
async fn with_on_behalf_of<F, R>(
    owner: Cap<ProcessIdentity>,
    body: impl FnOnce(SubjectContext) -> F,
) -> Result<R, Errno>
where
    F: Future<Output = Result<R, Errno>>,
```

Inside the closure, `body` receives a `SubjectContext` constructed via `SubjectContext::borrowed(owner, ...)`. The borrow scope:

1. Holds `Cap<ProcessIdentity>` for the duration of the body's future, keeping identity addressable.
2. Subscribes to the borrowed process's `exit_source` (a `WaitSource`) as an abandonment source.
3. On scope exit (body completes or aborts), drops the cap, unsubscribes, releases any scope-held resources.

The body is an `async` closure: it can `.await` arbitrary subscripts, including `drive(...)` calls, including subscripts that yield `OnAgent`, `OnWaitSource`, or `OnTimer`. Yields inside the scope do not exit the scope; the borrow holds.

## 4. SubjectContext under borrow

<!-- txdoc:SCOPE-V1-SUBJECT-1 -->

```rust
SubjectContext::borrowed(owner: Cap<ProcessIdentity>, ...) -> SubjectContext
```

Returns a `SubjectContext` with:

- `process = owner.clone()`
- `thread = None`  (the kernel actor is not the borrowed process's thread)
- `authority = SubjectAuthority::derived_from(owner)`  — the borrowed process's cred + restrictions

SCOPE-4: subsystem authority lookups inside the scope resolve against the borrowed process. Files (resolved against `owner`'s file table), vm (against `owner`'s address space), cred (against `owner`'s credential), rlimit (charged against `owner`'s rlimits) all see the borrowed identity.

The borrowed authority is a snapshot at borrow time. If the borrowed process changes its authority (suid exec) during the scope's lifetime, the borrow is *not* automatically updated — the borrow scope's `SubjectContext.authority` is stable for the scope's duration. (Mid-scope authority updates are an open extension; v1 does not support them.)

## 5. Abandonment

<!-- txdoc:SCOPE-V1-ABANDONMENT-1 -->

SCOPE-3: scope abandonment is delivered through the existing Killable wait protocol.

When the borrowed process exits while a scope is active:

1. The `exit_source` subscription fires.
2. The script's drive loop, on its next yield resolution, observes a `Killed` outcome.
3. The script terminates with `Err(EOWNERDEAD)`.
4. All in-flight `OnAgent` tokens on the script reach `SENTINEL_DEAD` via their endpoints' standard cleanup.
5. The scope's drop releases the `Cap<ProcessIdentity>` and unsubscribes the `exit_source` watcher.

This means io_uring SQPOLL script abandonment, AIO worker abandonment, and FUSE helper abandonment all use the *same protocol* native syscall scripts use for SIGKILL. No new mechanism. `Killable` wait protocol is sufficient.

## 6. Resource scoping

<!-- txdoc:SCOPE-V1-RESOURCES-1 -->

SCOPE-5: resources held inside a scope must not outlive the scope.

This is an invariant about *what* a scope's body may carry across the scope's exit. The scope drops as part of body completion (or abort); any resources still held at that moment are dropped with it. Specifically:

- Fixed-buffer pins (e.g., io_uring `IORING_REGISTER_BUFFERS`) are typed as `OperationalEvidence` *bound to the scope's `Cap<ProcessIdentity>`*. Their drop is part of the scope's drop.
- In-flight delegation tokens issued inside the scope are bound to the scope's lifetime; on scope exit they reach `SENTINEL_DEAD`.
- Subscribed wait sources are unsubscribed on scope exit.

The corollary is that *long-lived* resources (e.g., io_uring's permanently-registered fixed buffers spanning many submitted ops) require a *long-lived* scope. SQPOLL kthread runs *one* OnBehalfOf scope for the entire lifetime of the io_uring instance, and individual SQE-handling sub-scripts run inside it. Each sub-script is its own short-lived script; the scope is the kthread's outer frame.

## 7. Nesting

<!-- txdoc:SCOPE-V1-NESTING-1 -->

SCOPE-6: a kernel task may enter at most one OnBehalfOf scope at a time.

Nesting is forbidden because:

- Authority composition under nested borrows is ill-defined. Whose `RLIMIT_NOFILE` charges? Whose `Credential` checks?
- Abandonment routing under nested borrows is fragile. If the inner-borrowed process exits, does the outer scope continue?
- The 1:1 mapping between kthread and borrowed identity matches the actual kernel-actor pattern in Linux (one io_uring SQPOLL kthread per uring; one kworker bound to one cgroup; etc.).

If a kernel task needs to act on behalf of *different* processes over time, it must explicitly *replace* its current scope (drop the old, enter a new). The replacement is a deliberate transition; the task acknowledges that all scope-held resources from the old borrow are released.

## 8. Worked uses

<!-- txdoc:SCOPE-V1-USES-1 -->

### 8.1 io_uring SQPOLL

<!-- txdoc:SCOPE-V1-IO-URING-1 -->

```rust
async fn sqpoll_kthread_main(uring: Cap<UringInstance>, owner: Cap<ProcessIdentity>) {
    with_on_behalf_of(owner, |ctx| async move {
        // Long-lived scope. Fixed buffers registered against this borrow.
        let registered_bufs = drive(
            uring::RegisterFixedBuffersOp::new(/* ... */),
            &mut ScriptCtx::wrap(ctx.clone()),
            DriveMode::Waiting,
        ).await?;

        loop {
            let sqe = uring.dequeue_sqe().await;
            // Each SQE handler is a sub-script under the same scope.
            let result = drive_sqe(&mut ScriptCtx::wrap(ctx.clone()), sqe, &registered_bufs).await;
            uring.post_cqe(result);
        }
    }).await;
}
```

The SQPOLL kthread enters one scope at startup; sub-scripts (each `drive_sqe`) run inside it. Fixed buffer registration produces operational evidence on the borrowed process's address space, scoped to the borrow.

When the io_uring instance is closed (or the owner exits), the scope drops; fixed buffers are unpinned; pending SQE work aborts with `EOWNERDEAD`.

### 8.2 AIO worker

<!-- txdoc:SCOPE-V1-AIO-1 -->

POSIX AIO (io_setup / io_submit / io_getevents) workers are kernel tasks that execute submitted I/O on behalf of the user task. Each AIO context has an associated `Cap<ProcessIdentity>`; the worker enters `OnBehalfOf` scope for the lifetime of one execution batch (or, for long-lived workers, for the lifetime of the AIO context).

**Landed in PR-11** as the `OnBehalfOf<P>` canary. Implementation: `crates/tx-subsystems/src/aio.rs` (the `AioContext` zone, the per-context worker future entered through `with_on_behalf_of` at `io_setup` time, the iocb submission queue + completion queue, and the `IocbDispatcher` callback that resolves `aio_fildes` against P's fd table under the borrow); `crates/tx-shims/src/linux_syscall/aio.rs` (the four syscall arms — `sys_io_setup`, `sys_io_submit`, `sys_io_getevents`, `sys_io_destroy`); `crates/tx-shims/tests/v3_aio_e2e.rs` (the end-to-end canary pinning the submit → dispatch → completion → getevents → destroy loop). The planning + readiness audit is recorded in `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md`.

### 8.3 FUSE helper

<!-- txdoc:SCOPE-V1-FUSE-HELPER-1 -->

FUSE filesystems running in userspace cause kernel-side helper threads (one per FUSE mount, typically) that pump request/reply traffic between the kernel and the daemon. The helper acts on behalf of the *script* whose syscall is being delegated, not on behalf of the FUSE daemon itself.

In v3's framework, this is mostly *not* a SCOPE primitive — the script that yielded `OnAgent` is awaited synchronously by its driver. The helper kthread, if any, exists to forward bytes on the FUSE protocol channel; it does not need a scope.

If a FUSE backend wants to perform background reclaim (writeback, attribute refresh) on its own schedule, *that* worker is `OnBehalfOf` the relevant FUSE mount's owning process.

### 8.4 Network softirq

<!-- txdoc:SCOPE-V1-NETWORK-1 -->

Per-CPU softirq tasks that process incoming packets do not act on behalf of any one process — they act on behalf of the kernel's networking subsystem itself. Their scope is `Thread` (or possibly a synthetic kernel-identity scope), not `OnBehalfOf<UserProcess>`.

When a packet is *delivered* to a user socket, that delivery is a publish into the socket's wait queue; the *user's* read script (running under its own thread's `Thread` scope) consumes it. No OnBehalfOf borrow is involved.

### 8.5 Per-cgroup writeback

<!-- txdoc:SCOPE-V1-WRITEBACK-1 -->

cgroup-v2 writeback workers act on behalf of the cgroup's owning subsystem. If a writeback target belongs to a particular memcg/blkio group, the writer is `OnBehalfOf` a representative process in that cgroup (or a synthetic per-cgroup identity, which is a future closed-catalog extension).

## 9. What this is not

<!-- txdoc:SCOPE-V1-NEGATIVE-1 -->

`OnBehalfOf` is *not*:

- a way for one process to *issue syscalls* on another's behalf. The borrowed identity does not get to run code; the kernel actor runs code under the borrowed authority. Process A cannot use `OnBehalfOf<B>` to read process B's files unless A is a kernel actor with the cap to do so.
- a privilege-escalation mechanism. Borrows are issued at endpoint-registration time (e.g., io_setup) and require the borrowed process's consent (typically by the registering process *being* the borrowed process).
- a substitute for setuid. The borrowed authority is whatever the borrowed process *currently has*; if you want a different authority, change the cred via cred service.
- nestable.

## 10. Substrate cost

<!-- txdoc:SCOPE-V1-SUBSTRATE-1 -->

| Component | Estimated LoC | Location |
|---|---|---|
| `with_on_behalf_of` async helper | ~150 | tx-scripts |
| `SubjectContext::borrowed` | ~50 | tx-subsystems (cred / process) |
| `borrow_count` field on `ProcessPayload` (so the borrowed process knows how many borrows are outstanding) | ~50 | tx-subsystems (process) |
| Exit-port subscription + Killable routing for the scope | ~150 | tx-scripts |
| Resource-scoping discipline (typed as scope-bound `OperationalEvidence`) | ~100 | tx-subsystems |
| **Total framework** | **~500** | |

Per-use-case work (uring registration, AIO context, FUSE helper) is additional and lands with the corresponding subsystem.

## 11. Open questions

<!-- txdoc:SCOPE-V1-OPEN-1 -->

11.1. **Authority drift.** v1 takes a snapshot of `SubjectAuthority` at borrow time and holds it stable for the scope's duration. Linux io_uring with `IORING_REGISTER_PERSONALITY` allows registering a *different* cred than the submitter's, used for subsequent operations. v3 may need a future mechanism to swap the borrow's authority; for now, this is deferred.

11.2. **Nested-borrow use cases.** SCOPE-6 forbids nesting. If a real use case for nesting emerges (none known), this rule is the first to revisit.

11.3. **Kernel-identity scope.** Some kernel actors (network softirq, idle loops) act on behalf of the kernel itself, not any process. v1 leaves this as `Thread` scope on a synthetic kernel thread. A future closed-catalog member `OnBehalfOf::KernelIdentity` is reserved.

11.4. **Per-cgroup identity.** As an alternative to "borrow a representative process," a future member `OnBehalfOf::Cgroup(Cap<Cgroup>)` may be admitted, with cgroup-derived authority. Out of scope for v1.

## 12. Migration order

<!-- txdoc:SCOPE-V1-MIGRATION-1 -->

Recommended landing order:

1. Framework: `with_on_behalf_of`, `SubjectContext::borrowed`, exit-port routing. ~500 LoC. **Landed in PR-11 phases 0–1** (W-W + W-Z); see `crates/tx-substrate/src/step_v3/borrow.rs` for `with_on_behalf_of`, `crates/tx-substrate/src/step_v3/subject_context.rs` for the `SubjectContext::borrowed` constructor, and `OnBehalfOfAbort` for the abandonment routing.
2. First use case: AIO worker (smallest surface; one borrow per AIO context). Validates the framework against a real use. **Landed in PR-11 phases 2–6** (W-Z `AioContext` zone, W-CC worker spawn, W-FF real dispatch + completion queue + `io_getevents` + `io_destroy`, W-JJ end-to-end canary). See `crates/tx-subsystems/src/aio.rs` for the subsystem, `crates/tx-shims/src/linux_syscall/aio.rs` for the syscall arms, and `crates/tx-shims/tests/v3_aio_e2e.rs` for the e2e canary. The full PR-11 plan + readiness audit is recorded in `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md`.
3. io_uring SQPOLL with the registered-buffer authority story.
4. cgroup-v2 writeback under per-cgroup identity (after the cgroup-identity catalog member lands).

Each step is independently shippable.
