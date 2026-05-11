# Delegate — `YieldShape::OnAgent` — v1

<!-- txdoc:TXV3-DELEGATE-V1 -->

**Status.** v1 (Txv3 refresh, 2026-05).
**Purpose.** Specify the `OnAgent` yield shape: cap-owned `DelegateEndpoint`, token-as-cap, typed request/reply, cancellation/timeout/abandonment, and the universal coverage of userspace-agent-mediated kernel work (userfaultfd, FUSE, fanotify-perm, ptrace).
**Audience.** Subsystem authors implementing agent-mediated features; reviewers evaluating proposed delegations.
**Companion documents.** `01_CONCEPTS_v5.md §2.4, §12`, `02_INVARIANTS_v5.md (DELEGATE-*, YIELD-*)`, `03_STEP_MODEL_v2.md §2.3, §7.2`.

---

## 1. Why a primitive

<!-- txdoc:DELEGATE-V1-WHY-1 -->

Linux has multiple features where a kernel syscall must block pending a reply from a userspace agent in a different process:

- **userfaultfd** — VM faults route to a user-registered handler.
- **FUSE** — VFS operations on a mounted FUSE filesystem are answered by the FUSE daemon.
- **fanotify FAN_OPEN_PERM / FAN_ACCESS_PERM** — open/access syscalls block pending a security daemon's permit.
- **ptrace syscall stops** — tracee's syscall-entry / syscall-exit gates wait on the tracer's decision.
- **seccomp `SECCOMP_RET_TRACE`** — trap delivered to the registered tracer; tracer mutates the result.
- **LSM userspace mediation** — generalized form of fanotify-perm.

Each is a structurally identical pattern: kernel reaches a point where it cannot decide locally; it asks a userspace agent; the agent's reply determines what the kernel does next. The five Linux features differ in *what they ask about* (page fault, file op, permission check, syscall args), not in *the protocol*.

The `OnAgent` yield shape unifies them. One primitive, four (and counting) features.

## 2. Authority direction

<!-- txdoc:DELEGATE-V1-AUTHORITY-1 -->

**DELEGATE-1: a delegate endpoint reverses the normal authority direction. The agent answers a kernel question.**

A delegation does *not*:

- transfer process-identity authority to the agent;
- authorize the agent to execute syscalls on the script's behalf;
- charge any of the agent's resources to the script (or vice versa).

What the agent's reply *can* contain — capability injections (FUSE returning a backing fd), register mutations (ptrace), or page-content (userfaultfd) — is reified through the receiving step's normal upper-half check against the script's existing `SubjectContext`. The agent's authority is what allowed the endpoint to be registered in the first place; after registration, no further authority crossing happens.

If a feature requires the kernel to act *on behalf of* a process (io_uring SQPOLL, AIO worker), that is `ExecutionScope::OnBehalfOf`, not delegation. See `06_EXECUTION_SCOPE_v1`.

## 3. Endpoint

<!-- txdoc:DELEGATE-V1-ENDPOINT-1 -->

```rust
struct DelegateEndpoint<K: EndpointKind> {
    scope: EndpointScope,
    request_queue: MpscQueue<Cap<DelegateToken>>,    // tokens, not envelopes
    request_source: WaitSource,                       // agents wait on this
    in_flight: TokenLedger,                           // bounded by RLIMIT_DELEGATE
    // ...
}

enum EndpointScope {
    Thread(Cap<ThreadIdentity>),    // ptrace per-thread
    Process(Cap<ProcessIdentity>),  // FUSE, userfaultfd, fanotify
}

enum EndpointKind {
    Ufd,
    Fuse,
    FanotifyPerm,
    Ptrace,
    LsmMediated,
    // closed catalog; new kinds via ARCH-3
}
```

An endpoint is a Cap-owned object exposed to userspace through a fd. Standard fd semantics drive cleanup: when the fd is closed (or its owning process exits), the `Cap<DelegateEndpoint>` reaches `SENTINEL_DEAD` and all outstanding tokens are transitioned to `AgentDied` (via `mark_agent_died`) so that their bound script waiters wake with `Aborted(AgentDied)` (DELEGATE-3, DTOK-2).

The endpoint's type parameter `K` constrains the legal `DelegateRequest` and `DelegateReply` shapes. A `Cap<DelegateEndpoint<Ufd>>` cannot receive `Fuse` requests; the type system rejects the mismatch. This kills covert channels between endpoint kinds (DELEGATE-2).

The endpoint's request queue holds **`Cap<DelegateToken>`**, not request envelopes. The request payload lives on the token (see §4). Agents waiting for new requests subscribe to `request_source` (a `WaitSource`); each `enqueue_request` call ends with `request_source.notify(HasRequest)`.

### 3.1 EndpointScope

<!-- txdoc:DELEGATE-V1-SCOPE-1 -->

`Thread`-scoped endpoints attach to a particular thread; `Process`-scoped attach to a process. Most cases are `Process` (FUSE, userfaultfd, fanotify). Ptrace requires `Thread` scope because tracer/tracee relationships are per-thread.

The endpoint's scope determines whose `exit_source` is subscribed for abandonment. A `Process`-scoped endpoint dies when its owning process exits; a `Thread`-scoped endpoint dies when its owning thread exits. On endpoint death, the runtime walks `in_flight` and calls `mark_agent_died` on each token.

## 4. Token

<!-- txdoc:DELEGATE-V1-TOKEN-1 -->

```rust
struct DelegateToken {
    id: DelegateTokenId,
    state: AtomicDelegateState,
    request: OnceCell<DelegateRequest>,    // installed by install_request before enqueue
    reply: OnceCell<DelegateReply>,        // populated only in ReplyInstalling phase
    waiter: AtomicOption<Weak<TaskMailbox>>,
    generation: AtomicU64,
}

enum DelegateState {
    Pending,
    ReplyInstalling,
    Replied,
    Canceled,
    AgentDied,
    TimedOut,
}
```

A `Cap<DelegateToken>` is the script's resume credential. Token state — *not* slot lifecycle — is the linearization point of the delegation:

- The state machine has CAS-only transitions: `Pending → ReplyInstalling → Replied`, or `Pending → Canceled / AgentDied / TimedOut`. The first transition wins; later attempts return `LateReply` and are dropped.
- `ReplyInstalling` is the single-writer phase that owns the `reply` slot; only after the reply is fully installed does the state transition to `Replied`.
- A reply against any non-`Pending` state is rejected as late and has no observable effect on the script.

Slot lifecycle (substrate) and logical lifecycle (state machine) are independent (DELEGATE-3): the token may reach `Replied` while the slot is still pinned by other holders; the slot reaches `SENTINEL_DEAD` only when no `Cap` retains it.

The token zone is its own zone (`DelegateToken`), with bounded slot count per endpoint. Reservation in the script's reserve-phase, sign at publish (the `Yield` outcome's reserve→commit→publish sequence), drop at resume / timeout / cancel / agent death.

DELEGATE-5: per-endpoint in-flight tokens are bounded by `RLIMIT_DELEGATE` (or by borrowed `RLIMIT_NOFILE` charge), preventing a misbehaving script-side process from saturating the token zone.

### 4.1 Request placement

The request envelope lives **on the token**, not in the endpoint queue. The script-side flow is:

```
1. step constructs YieldShape::OnAgent { endpoint, request, token, cancel }
2. prepare_active_wait calls token.install_request(request)
3. prepare_active_wait calls endpoint.enqueue_request(token.clone())
4. agent dequeues token from endpoint, reads token.request()
5. agent computes, calls token.reply(reply)
6. token state CASes Pending → ReplyInstalling → Replied
```

The endpoint queue is therefore `MpscQueue<Cap<DelegateToken>>`, not `MpscQueue<DelegateRequest>`. Concept-layer `YieldShape::OnAgent` carries `request` for clarity; runtime `prepare_active_wait` moves it onto the token before enqueue.

## 5. Request and reply

<!-- txdoc:DELEGATE-V1-REQUEST-REPLY-1 -->

```rust
enum DelegateRequest {
    Ufd(UfdRequest),
    Fuse(FuseRequest),
    FanotifyPerm(FanotifyPermRequest),
    Ptrace(PtraceRequest),
    LsmMediated(LsmRequest),
}

enum DelegateReply {
    Ufd(UfdReply),
    Fuse(FuseReply),
    FanotifyPerm(FanotifyPermReply),
    Ptrace(PtraceReply),
    LsmMediated(LsmReply),
}
```

Each endpoint kind has its own request and reply types. The reply is a closed sum:

```rust
struct ReplyEnvelope<K: EndpointKind> {
    result: K::Result,
    fd_injections: Vec<InjectedFd>,    // bounded by injection limit per endpoint kind
    continuation: Continuation,
}

enum Continuation {
    Final,
    Partial { next_token: Cap<DelegateToken>, accumulated: Progress },
    Streamed { stream_handle: Cap<DelegateStream> },
}
```

### 5.1 fd_injections — capability transfer

<!-- txdoc:DELEGATE-V1-FD-INJECTIONS-1 -->

DELEGATE-4: each `InjectedFd` is checked at the receiving step's resume-side `require_*` for:

1. **Authority of the agent to inject this fd.** The endpoint's type constrains what cap classes are legal injections — a `Ufd` endpoint's reply may inject zero fds; a `Fuse` endpoint's reply may inject backing-file fds whose RNode is rooted within the FUSE filesystem instance.
2. **Cred check at receive.** The script's `SubjectContext.authority` must permit access to the injected resource. The agent does not get to launder authority.
3. **`RLIMIT_NOFILE` reservation in the script's resume reserve-phase.** Reservation precedes the agent's reply being applied; an over-quota receiver fails before commit.

Failure on any of these → `Agent::Refused` outcome (or a fresh errno class for the specific failure mode).

### 5.2 continuation — typed

<!-- txdoc:DELEGATE-V1-CONTINUATION-1 -->

DELEGATE-9 makes continuation a closed sum:

| Variant | Use |
|---|---|
| `Final` | The reply is complete; resume the script. |
| `Partial { next_token, accumulated }` | The agent gave a partial answer (e.g., FUSE READ short-read; `UFFDIO_COPY` partial-page). The kernel resumes with `accumulated` progress and a fresh token for the remainder. |
| `Streamed { stream_handle }` | The agent supplies a stream of events through a stream handle (fanotify subscription); per-event token accounting is replaced by stream-handle linearization. |

`continuation` cannot be free-form; the structure of the kernel's resume protocol is fixed.

## 6. Cancellation

<!-- txdoc:DELEGATE-V1-CANCELLATION-1 -->

Cancellation has two orthogonal axes. The two are independent because they live at different layers:

### 6.1 `AgentCancelPolicy` — what the kernel tells the agent

DELEGATE-6: closed `AgentCancelPolicy`:

```rust
enum AgentCancelPolicy {
    BestEffort,       // notify endpoint, give up after a grace
    Synchronous,      // block on agent ack of cancellation
    Detached,         // fire-and-forget, only for idempotent agent ops
}
```

Carried in `YieldShape::OnAgent::cancel`. Determines the agent-side protocol when the kernel cancels a delegated request:

- `BestEffort`: post a `CANCEL` notification on the endpoint's request carrier; transition the token to `Canceled` after grace; the agent's eventual reply is dropped.
- `Synchronous`: post `CANCEL` and wait on a per-token cancellation-ack carrier; the script's cancellation path itself yields until the agent acknowledges. The token state still CASes `Pending → Canceled` immediately; the kernel just additionally waits for the agent's ack before releasing scope-owned resources. No new token state is needed.
- `Detached`: drop the token immediately; agent's reply is dropped on arrival. Only safe when the agent's pending work has no observable side effects on the kernel beyond the (now-dropped) reply.

The default for most yield sites is `BestEffort`. `Synchronous` is needed when the agent holds resources whose release is required for forward progress (e.g., a file lock the agent acquired pending its reply).

### 6.2 `TokenDropPolicy` — what `ActiveWait` drop does

```rust
enum TokenDropPolicy {
    CancelOnDrop,     // drop ⇒ token.cancel(Abandoned); waiter unbound
    Abandon,          // drop ⇒ unbind waiter only; agent reply lands on dead waiter
    // reserved (not in R2):
    // KeepAlive,     // drop ⇒ unbind only; preserve token for another consumer
}
```

Held internally by `AgentTokenGuard` (the `YieldRegistration` for `OnAgent`). Consumed only at `ActiveWait` drop time; the runtime never reads it during normal resume.

`CancelOnDrop + Synchronous` is meaningful: on drop, cancel the agent's request *and* wait for ack. The two policies compose orthogonally — `AgentCancelPolicy` decides the protocol the agent sees; `TokenDropPolicy` decides whether `ActiveWait` drop initiates that protocol.

`KeepAlive` is reserved but not implemented; landing it requires a concrete consumer (a token's reply being passed to a different script frame).

## 7. Resume protocol

<!-- txdoc:DELEGATE-V1-RESUME-1 -->

When the agent writes a reply, the kernel:

1. CAS the token state `Pending → ReplyInstalling`. Failure → reply rejected as `LateReply`; the previous transition (Canceled/AgentDied/TimedOut) wins.
2. Validate the reply envelope's structural shape against the token's endpoint kind. Mismatch → state CAS rolls forward to a terminal kind (e.g., `AgentDied`); reply rejected.
3. Install reply payload in the `OnceCell<DelegateReply>` slot. The `ReplyInstalling` state is the single-writer phase that owns the slot.
4. Store state `Replied`; post `WakeHint::AgentReplied` to the bound waiter.
5. Driver wakes, validates generation, classifies the token state (`Replied` → `Ready(WithReply(reply))`), takes the reply.
6. Driver acquires fresh epoch guard, calls `op.apply_resume(WithReply(reply))` to stash the reply in the StepOp's `&mut self`, then re-invokes `step()`.
7. The next `step()` runs `require_*` from scratch under fresh guard (WIT-5, anti-pattern A-15). The wake/reply itself is not truth; the predicate re-evaluation establishes truth.

Abandonment (`Canceled`, `AgentDied`, `TimedOut`) flows through the same path: each transitions the token state via CAS, the bound waiter receives `WakeHint::Abort{reason}`, and the driver returns the corresponding errno (`EINTR` for Canceled, `EOWNERDEAD` for AgentDied, `ETIMEDOUT` for TimedOut). The reply-vs-abandonment race is resolved by the token state machine, not by mailbox order: whichever transition CAS wins first determines the outcome.

## 8. Worked uses

<!-- txdoc:DELEGATE-V1-USES-1 -->

### 8.1 userfaultfd

<!-- txdoc:DELEGATE-V1-UFD-1 -->

The userfaultfd registration installs a `Cap<DelegateEndpoint<Ufd>>` as the fault delegate for one or more VMAs. The fault script (running in the faulting process) yields:

```rust
Yield {
    progress: PageProgress::EMPTY,
    shape: YieldShape::OnAgent {
        endpoint: vma.ufd_endpoint.clone(),
        request: DelegateRequest::Ufd(UfdRequest::PageFault {
            addr: fault_addr,
            kind: fault_kind,
        }),
        token: token_cap,
        cancel: AgentCancelPolicy::BestEffort,
    },
}
// Caller drives with WaitProtocol { deadline: Some(ctx.ufd_timeout), .. }
```

The agent process holds the ufd-fd, reads requests, replies with `UFFDIO_COPY { src, dst }`, `UFFDIO_CONTINUE { ... }`, or `UFFDIO_ZEROPAGE`. The reply's `result` carries the page contents (or copy-source). Resume revalidates the recipe BTree under fresh guard and materializes the PTE; if munmap raced, resume sees the recipe gone and returns `Err(EFAULT)`. The deadline is enforced by a driver-installed `TimerGuard` with `TimerRole::DelegateTimeout { token }`; on timer expiry it CASes the token state `Pending → TimedOut`, racing the agent's reply CAS by token state.

### 8.2 FUSE

<!-- txdoc:DELEGATE-V1-FUSE-1 -->

A FUSE mount creates RNodes whose backing is `FuseDelegate` with `endpoint` = the mounting daemon's `Cap<DelegateEndpoint<Fuse>>`. Every VFS step on those RNodes (READ, WRITE, READDIR, GETATTR, OPEN, ...) yields `OnAgent` with the relevant `FuseRequest`. Replies carry inode metadata, page content, attributes; `fd_injections` allow the daemon to hand back open backing fds for OPEN replies.

The deadline is per-mount, attached via `WaitProtocol.deadline` at the driver call site; default is "long" because FUSE filesystems are not adversarial-by-assumption (compared to userfaultfd, which often is).

### 8.3 fanotify FAN_OPEN_PERM

<!-- txdoc:DELEGATE-V1-FANOTIFY-1 -->

The fanotify watch's permission-event registration installs a `Cap<DelegateEndpoint<FanotifyPerm>>` as the open-permission delegate for a path tree. The upper-half `vfs::PathResolveOp` checks the watch list during its observe phase; if a permission event matches, it yields:

```rust
Yield {
    progress: NoProgress::EMPTY,
    shape: YieldShape::OnAgent {
        endpoint: watch.fanotify_endpoint.clone(),
        request: DelegateRequest::FanotifyPerm(FanotifyPermRequest::OpenPerm {
            path: path_buf, flags, mode,
        }),
        token: token_cap,
        cancel: AgentCancelPolicy::BestEffort,
    },
}
// Caller drives with WaitProtocol { deadline: Some(watch.deadline), .. }
```

The daemon replies with `Allow` or `Deny`; resume continues path resolution or returns `Err(EACCES)`.

### 8.4 ptrace syscall stop

<!-- txdoc:DELEGATE-V1-PTRACE-1 -->

A traced process's syscall script has a Gate phase class step at entry and at exit. Each gate checks for a registered `Cap<DelegateEndpoint<Ptrace>>` on the tracee's `ThreadIdentity` (`Thread`-scoped endpoint). On match:

```rust
Yield {
    progress: NoProgress::EMPTY,
    shape: YieldShape::OnAgent {
        endpoint: tracer_endpoint,
        request: DelegateRequest::Ptrace(PtraceRequest::SyscallEntry {
            regs, syscall_num, args,
        }),
        token: token_cap,
        cancel: AgentCancelPolicy::Synchronous,
    },
}
// Caller drives with WaitProtocol { deadline: None, .. } — ptrace stops are
// unbounded by Linux convention; signal-driven abort is the only escape.
```

The tracer reads the request, optionally mutates registers, and replies with `Continue`, `RouteSyscall(new_num, new_args)`, `InjectSignal(sig)`, or `Detach`. The tracee's resume applies the reply *under the tracee's SubjectContext* — the reply's content (register values, etc.) is reply data, not authority transfer (DELEGATE-1, SUBJ-5).

This factoring lifts ptrace from "structural break in the v4 framework" to "natural fit in the v5 framework." The third intercept point that ptrace seemed to need does not exist; the syscall-entry / syscall-exit gates are existing Gate-class phases that emit `OnAgent` like any other yield site.

## 9. Substrate cost

<!-- txdoc:DELEGATE-V1-SUBSTRATE-1 -->

Adding `OnAgent` to the YieldShape catalog requires the following substrate primitives (YIELD-6):

| Component | Estimated LoC | Location |
|---|---|---|
| `DelegateToken` zone (with bounded slots, generation tag) | ~500 | tx-substrate (new zone) |
| `DelegateEndpoint<K>` cap-zone | ~500 | tx-subsystems (new endpoint catalog) |
| Per-endpoint request RawPort wiring | ~200 | tx-reactor (existing bus extension) |
| `RLIMIT_DELEGATE` accounting | ~150 | tx-services (rlimit service extension) |
| Driver-mode `handle` impl for OnAgent | ~150 | tx-scripts (driver impl) |
| Resume-side reply validation + injection check | ~200 | tx-scripts (drive primitive) |
| Cancellation path (per-policy) | ~300 | tx-scripts (drive primitive) |
| **Total** | **~2,000** | |

Per-feature-kind work (Ufd, Fuse, FanotifyPerm, Ptrace, LsmMediated) is *additional* and lands as the corresponding subsystem matures. The framework cost above is one-time.

## 10. Migration order

<!-- txdoc:DELEGATE-V1-MIGRATION-1 -->

Recommended landing order:

1. Substrate token zone and endpoint cap-zone, with no kind populated yet. Compile-only changes in `tx-substrate` and `tx-subsystems`. ~1,200 LoC.
2. Driver-mode `Waiting::handle` for OnAgent, with the closed reply-validation path. Tested with a synthetic kind. ~700 LoC.
3. First real kind: **userfaultfd** (smallest reply space, smallest covert-channel surface). ~1,500 LoC including the ufd subsystem itself.
4. **FUSE** as the canary for `fd_injections` (largest reply space, most authority-laundering risk). ~3,000 LoC including the FUSE backend itself.
5. **fanotify FAN_OPEN_PERM** alongside the seccomp restriction-stack work (both share `RestrictionStack` plumbing).
6. **ptrace** last; benefits from prior endpoint and reply-validation work.

Each step is independently shippable: features land one kind at a time without disturbing the framework.
