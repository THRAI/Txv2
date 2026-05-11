# Decision D7: PR-10 userfaultfd — substrate readiness audit + plan

**Date:** 2026-05-11
**Status:** decided
**Worker:** W-O (research-only)
**Companion:**
- [PR-3 wake-substrate shape](2026-05-11-pr-3-wake-substrate-shape.md)
- [D4 — bus/mailbox layering](2026-05-11-d4-bus-mailbox-layering.md)
- [D6 — timer-wheel layering](2026-05-11-d6-timerwheel-layering.md)
- `docs/Txv3/05_DELEGATE_v1.md` §8.1 (userfaultfd worked example)
- `docs/Txv3/07_BLAST_RADIUS.md` §5.2 (PR-10 budget row), §6 risks
- `crates/tx-substrate/src/step_v3/agent.rs` (PR-7 runtime)
- `crates/tx-substrate/tests/v3_pr7_delegate_runtime.rs`
- `crates/tx-substrate/tests/v3_pr7b_mailbox_integration.rs`

---

## 1. What is userfaultfd?

Linux `userfaultfd(2)` is the kernel mechanism for **delegating
page-fault handling to user space**. A process opens a userfaultfd
(an `fd`), registers one or more VMA ranges against it
(`UFFDIO_REGISTER`), and from that point on any thread in that
process that touches an unmapped page in the registered range is
**parked by the kernel** while a fault message is delivered onto the
fd. A handler thread (usually inside the same process, but on a
different reactor task) reads the message, materialises the page
contents into a private buffer, and asks the kernel to install the
page with `UFFDIO_COPY { src, dst, len }` (or to install a zero page
with `UFFDIO_ZEROPAGE { dst, len }`, or — newer — to remap an
existing PTE with `UFFDIO_CONTINUE`). On the `UFFDIO_COPY` returning
success the kernel materialises the PTE and resumes the faulting
thread; if the handler thread exits without replying, the faulting
thread aborts (the man page says behaviour is "best effort" — Linux
typically returns the original SIGSEGV/SIGBUS path).

The structure is exactly the OnAgent yield shape rotated 90 degrees:
the kernel asks a userspace question, parks the asker, and resumes
on the answerer's reply.

## 2. Why userfaultfd is the OnAgent canary

The mapping from `05_DELEGATE_v1.md` concepts to userfaultfd
primitives is one-to-one:

| OnAgent concept | userfaultfd primitive |
|---|---|
| Script that yields `OnAgent` | The faulting thread (any thread in the registered range's process) — `fault_script` after it hits an unmapped page in a userfault-registered VMA |
| `Cap<DelegateEndpoint<Ufd>>` | The `Cap<UserfaultFd>` opened by `sys_userfaultfd(2)`, registered as the fault-delegate for one or more VMAs |
| `EndpointScope::Process(Cap<ProcessIdentity>)` | The ufd's owning process — when that process exits or the fd is closed, registered VMAs lose their delegate and outstanding faults transition to `AgentDied` |
| `DelegateRequest::Ufd(UfdRequest::PageFault { addr, kind })` | The `struct uffd_msg` the handler reads off the fd: address, access type (read/write/execute), thread id |
| `Cap<DelegateToken>` / `DelegateTokenId` | Per-fault token; the in-flight unit the registry tracks for that one fault. One token per parked faulting thread per fault address |
| `DelegateReply::Ufd(UfdReply::Copy { src, dst, len })` (or `ZeroPage`, `Continue`) | The arguments to `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_CONTINUE` ioctls written by the handler |
| `AgentTokenGuard` | The substrate-side RAII guard the faulting `fault_script` holds while parked; drop semantics route the abort to the faulting thread |
| `MailboxEvent::AgentReplied { token_id }` | Posted to the faulting thread's `TaskMailbox` when `UFFDIO_COPY` lands and `mark_replied` fires |
| `MailboxEvent::Abort { reason: AgentDied }` | Posted to the faulting thread's mailbox when the handler-side process exits or the ufd is closed — `mark_agent_died` fires |
| `MailboxEvent::Abort { reason: TimedOut }` | Linux's ufd has no built-in deadline (see §3.5); the substrate slot exists but `WaitProtocol.deadline` is intentionally `None` for ufd |
| `AgentCancelPolicy::BestEffort` | Userfaultfd has no cancel protocol — closing the fd is the only "cancellation". `BestEffort` + `TokenDropPolicy::CancelOnDrop` is the right pair |
| Resume → `apply_resume(WithReply(UfdReply::Copy { .. }))` | The reactor takes the reply payload, applies it under fresh epoch guard, materialises the page through the existing `materialize_pagebacked` path, and re-invokes `fault_script` |

The five Linux features `05_DELEGATE_v1.md` §1 lists differ in *what
they ask about*, not in the protocol. Ufd asks about a page fault;
that question has the smallest reply space (a `src` pointer + `len`),
zero `fd_injections`, and a tightly-scoped per-VMA registration —
exactly why §10 chose it as the first agent kind.

## 3. Substrate readiness audit

### 3.1 `DelegateRegistry` API surface (PR-7)

Grep evidence at `crates/tx-substrate/src/step_v3/agent.rs:480-696`:

| Method | Present | Sufficient for ufd? |
|---|---|---|
| `install_request(request, endpoint_marker, cancel, drop, mailbox)` | ✓ | **Yes.** Returns `AgentTokenGuard`, mints `DelegateTokenId`, stamps mailbox `Weak`. The `endpoint_marker: u64` placeholder serves the per-ufd grouping (see §3.3). |
| `mark_replied(id, reply)` | ✓ | **Yes.** `Pending → ReplyInstalling → Replied`, posts `MailboxEvent::AgentReplied`. The `reply: DelegateReply` parameter is the substrate placeholder shape; PR-10 must extend it (see §3.2). |
| `mark_canceled(id)` | ✓ | **Yes.** Drives the close-the-ufd-on-handler-exit path via `CancelOnDrop`. |
| `mark_agent_died(id)` | ✓ | **Yes.** Per-token endpoint-death entry. |
| `mark_timed_out(id)` | ✓ | Present but **unused for ufd** — Linux ufd has no protocol deadline. |
| `mark_endpoint_died(marker)` | ✓ | **Yes.** Walks every `endpoint_marker`-tagged token and transitions to `AgentDied`. This is the canonical ufd-close path: when the ufd fd is closed (or its owning process exits), the kernel calls `mark_endpoint_died(ufd_marker)` and every faulting thread parked on that ufd wakes with `Abort { reason: AgentDied }`. |
| `take_reply(id) -> Option<DelegateReply>` | ✓ | **Yes.** Consumed once on the resume path. |
| `state(id) -> Option<DelegateState>` | ✓ | Diagnostic only, sufficient. |

**Verdict:** the registry API is complete for ufd as a feature. PR-10
adds **no** new registry methods.

### 3.2 `DelegateReply::placeholder()` — must grow a real payload

Today's `DelegateReply` (agent.rs:144-153) is an empty unit type:

```rust
pub struct DelegateReply { _private: () }
impl DelegateReply { pub const fn placeholder() -> Self { ... } }
```

UFFDIO_COPY's payload is `{ src_kernel_addr: usize, dst_uaddr:
UserVirtAddr, len: usize, mode: UffdioCopyMode }`. The
substrate-side `DelegateReply` therefore needs to extend to either:

- (a) A closed sum keyed on endpoint kind:

  ```rust
  pub enum DelegateReply {
      Ufd(UfdReply),                // PR-10
      // future: Fuse(FuseReply), FanotifyPerm(FanotifyPermReply), ...
  }

  pub enum UfdReply {
      Copy { src: KernelVirtAddr, dst: UserVirtAddr, len: usize, mode: UffdioCopyMode },
      ZeroPage { dst: UserVirtAddr, len: usize },
      Continue { dst: UserVirtAddr, len: usize },
  }
  ```

  This matches `05_DELEGATE_v1.md` §5 verbatim.

- (b) A boxed `dyn Any`-style reply that subsystems downcast.
  Rejected — the spec's closed-catalog discipline (DELEGATE-2,
  ARCH-3) forbids open sums.

**Decision: extend `DelegateReply` to a closed sum in PR-10 phase 1**,
with the `Ufd` arm only. Future agent kinds add variants under
ARCH-3 review. The placeholder constructor is retained as
`DelegateReply::placeholder() -> Self` returning a never-matching
variant for state-machine unit tests that don't care about payload —
substrate-test friendliness, no functional cost.

This is a **substrate edit but not a substrate redesign**; the
registry's `mark_replied(id, reply: DelegateReply)` signature stays
unchanged.

### 3.3 `endpoint_marker: u64` — sufficient as a ufd handle

PR-7's `endpoint_marker: u64` (agent.rs:439, 676) is the per-ufd
discriminator. The natural mapping:

- Each `Cap<UserfaultFd>` (zone-allocated in `tx-subsystems`) carries
  a stable `ufd_id: u64` (the cap's slot id, or a separately-minted
  monotonic id).
- `install_request` is called with `endpoint_marker = ufd_id` for
  every page fault routed to that ufd.
- The ufd-close / process-exit path calls
  `registry.mark_endpoint_died(ufd_id)`, walking every in-flight
  fault on that ufd and transitioning each to `AgentDied`.

PR-10 **does not need** a real `Cap<DelegateEndpoint<Ufd>>` zone yet
— the `u64` marker is sufficient for the per-ufd grouping the
registry walks. The real endpoint cap-zone can land later (e.g.
PR-12 or alongside FUSE) without disturbing ufd. Per
`05_DELEGATE_v1.md` §3 the endpoint also drives the wait-source for
agent-side reads (`request_source: WaitSource`); that surface lives
on the `UserfaultFd` payload (a separate zone in `tx-subsystems`),
not in the substrate registry.

**Verdict:** the marker is sufficient; PR-10 ships a
`Cap<UserfaultFd>` in `tx-subsystems` whose `slot_id()` is the
marker.

### 3.4 `TaskMailbox` event routing (PR-7B)

`MailboxEvent::AgentReplied { token_id }` and `MailboxEvent::Abort
{ token_id, reason }` are already defined and posted from
`mark_replied` / `cas_terminal` (agent.rs:597-735). The PR-7B
integration tests (`v3_pr7b_mailbox_integration.rs` 1-453) pin:

- One mailbox can hold events for many tokens (test
  `one_mailbox_receives_events_from_many_tokens_in_order`)
- `mark_endpoint_died` posts an `Abort` to every bound mailbox
  (test `mark_endpoint_died_routes_abort_to_each_bound_mailbox`)
- Dead-task mailbox upgrade-failure is silently dropped
- `LateNoOp` does **not** post a second event (DTOK-2)
- `ActiveWait::matches` returns `false` for `AgentReplied` / `Abort`
  — these are not source-fired events; the driver must inspect them
  via a separate event-shape (see §4 gap-3)

**Verdict:** mailbox routing is **complete** for ufd. The only
unresolved bit is the driver-side **consumer**: today's
`ActiveWait` is wait-source-shaped, not agent-shaped. PR-10 needs a
small driver-side `await_agent_reply(token_id, mailbox) -> Result<
DelegateReply, AbortReason>` helper that polls the mailbox and
matches by `token_id`. This is **runtime code, not new substrate
surface** — the events are already posted; PR-10 just consumes
them.

### 3.5 `TimerWheel::install_delegate_timeout` — unused for ufd

`tx-substrate/src/wake/timer.rs` exposes `install_delegate_timeout
(deadline, DelegateTokenId) -> TimerGuard` (per D6 the wheel now
lives below the reactor; the file was relocated). Userfaultfd does
**not** use this: Linux ufd has no built-in deadline; a handler can
take as long as it wants, and signals are the only escape mechanism.

PR-10 therefore drives `WaitProtocol { deadline: None, .. }`. The
substrate machinery for deadline-on-OnAgent stays unexercised by
PR-10 but lands later with FUSE (per `05_DELEGATE_v1.md` §8.2). No
substrate work needed here.

**Verdict:** present but not needed. No gap.

### 3.6 Page-fault interception path — already in place

`crates/tx-subsystems/src/vm/execution.rs:153-203` defines
`AddressSpace::fault_script(VmFault) -> Result<PmapPublishOutcome,
VmFaultError>`, the async fault resolver. It is already invoked from
`crates/tx-kernel/src/thread_future.rs:391` on the
`UserspaceTrapInfo::PageFault(info)` arm:

```rust
UserspaceTrapInfo::PageFault(info) => {
    // ...
    match aspace.fault_script(fault).await { ... }
}
```

The existing loop pattern in `fault_script` yields `OnWaitSource` on
`RangeLock` `WouldBlock` and retries after wake. **PR-10 extends the
loop with a `userfault_check` step before the `require_fault_recipe`
call:** if the VMA has a registered ufd endpoint, the script
emits `Yield { shape: YieldShape::OnAgent { .. } }` instead of
calling `materialize_pagebacked`. On resume the reply payload tells
the script which page to install (the agent's `src` buffer for
`UFFDIO_COPY`, or a zero page for `UFFDIO_ZEROPAGE`).

The fault path **does not need a major redesign**. The required
edit is one new branch inside `fault_script` and one new field on
`VmEntry` (or its `VmBacking`) recording the registered ufd
endpoint.

**Verdict:** the entrypoint is in place. PR-10 inserts a branch.

### 3.7 File-descriptor model — sufficient

The fd table is `pub(crate) fds: SpinMutex<BTreeMap<u32, Cap<OpenFile>>>`
on `ProcessPayload` (`process/structure.rs:750`). Today's `OpenFile`
is rooted in VFS — it carries an `RNode` cap and `OpenFileFlags`.
Userfaultfd is a non-VFS fd kind.

Two options for how to land a `userfaultfd`-kind fd:

- **(a) Generic fd-kind tagged union on `OpenFile`.** Add an
  `OpenFileBacking` enum with `Vfs(RNode)`, `Pipe(...)`, `Anon(...)`.
  Userfaultfd lands as `OpenFileBacking::UserfaultFd(Cap<
  UserfaultFd>)`. This is the spec-aligned shape — `05_DELEGATE_v1.md`
  §3 says the endpoint is "exposed to userspace through a fd."
  Touches every existing OpenFile site.

- **(b) Parallel fd-table entry shape.** Treat the ufd's fd-table
  entry as a separate `Cap<UserfaultFd>` directly, bypassing
  `OpenFile`. Smaller blast radius but creates a second fd-cap
  shape the syscall surface (`close`, `dup`, `read`, `write`) must
  branch on. Inelegant and proliferates if FUSE / fanotify follow.

**Decision: (a).** Add a small `OpenFileBacking` enum and migrate
the existing VFS-only shape to `Vfs(RNode)`. Pipes are already
non-RNode (pipe.rs has its own per-file state) so this conversion
is in scope for PR-10's first phase — or, more conservatively, PR-10
**only** introduces the `UserfaultFd` variant alongside the existing
`Rnode`-only `OpenFile` shape, leaving pipe migration for a
follow-up. Both work; the second has a smaller PR-10 surface.

**Verdict:** workable. PR-10 phase 1 includes a small `OpenFile`
refactor to admit a `UserfaultFd` variant.

## 4. Gap list

Three discrete substrate / kernel deficits. None of them require a
major redesign; all are tractable inside PR-10 itself.

1. **`DelegateReply` is a unit-type placeholder.** Per §3.2, PR-10
   phase 1 extends it to a closed sum keyed on endpoint kind, with
   the `Ufd(UfdReply)` arm populated. Trivial substrate edit (~30
   LoC + test).

2. **No driver-side `await_agent_reply` helper.** Per §3.4, the
   PR-7B mailbox events are posted but no driver loop consumes them
   for `OnAgent` yields. PR-10 phase 4 lands a helper in
   `tx-reactor` (or `tx-scripts`) — `async fn await_agent_reply
   (mailbox: &TaskMailbox, token_id: DelegateTokenId) -> Result<
   DelegateReply, AbortReason>` — that polls the mailbox, filters by
   token id, and returns the appropriate variant. ~80 LoC including
   the test.

3. **`OpenFile` is RNode-only.** Per §3.7, PR-10 phase 1 adds a
   `UserfaultFd` variant either to a new `OpenFileBacking` enum or
   as a parallel fd-table shape. The recommended approach is the
   former, but PR-10 can ship the latter and defer the wider
   refactor.

**No substrate redesign required.** PR-10 is a *populate the
scaffold* PR, exactly as `07_BLAST_RADIUS.md` §10 anticipated.

## 5. Decision: GO

PR-10 is unblocked. Substrate is **ready** per the PR-7 + PR-7B
landings and the D4 / D6 layering moves that put `TaskMailbox`,
`WaitSource`, and `TimerWheel` below `tx-reactor`. The three gaps in
§4 are PR-10's own first three phases, not prerequisite PRs.

## 6. Implementation phase plan

Eight phases. Estimated total **5–7 working days** (slightly under
the `07_BLAST_RADIUS.md` §5.2 "5–10 days" budget because the
substrate is more populated than the v3-draft estimate assumed —
PR-7 landed the registry, PR-7B landed the mailbox routing, D6 has
brought `TimerWheel` down).

| Phase | Goal | Touches | Days |
|---|---|---|---|
| **P-10.0** | `OpenFile` admits a userfaultfd-kind fd-table entry. Add `OpenFileBacking { Vfs(...), UserfaultFd(Cap<UserfaultFd>) }` or sibling fd-table entry. Stub `Cap<UserfaultFd>` zone (empty payload). Wire `close` cleanup to call `registry.mark_endpoint_died(ufd_id)`. | `tx-subsystems/src/vfs/structure.rs`, new `tx-subsystems/src/userfaultfd/{mod,structure}.rs`, `tx-subsystems/src/process/structure.rs` (close path) | 1 |
| **P-10.1** | `DelegateReply` extension: closed sum with `Ufd(UfdReply { Copy / ZeroPage / Continue })` arm. Round-trip test through the registry. | `tx-substrate/src/step_v3/agent.rs`, `tx-substrate/tests/v3_pr7_delegate_runtime.rs` | 0.5 |
| **P-10.2** | `sys_userfaultfd(2)` syscall: allocates a `Cap<UserfaultFd>`, installs it in the fd table. Stub `UFFDIO_API` ioctl that just validates the api-handshake fields. | `tx-shims/src/linux_syscall/{vm,mod}.rs`, new `tx-shims/tests/userfaultfd.rs` | 0.5 |
| **P-10.3** | `UFFDIO_REGISTER` ioctl: attaches the ufd to a VMA range. Adds `VmBacking::PrivateAnonWithUfd { ufd_id: u64 }` (or a new field on `VmEntryFlags`) so the fault path can look up the endpoint. Plain `Cap` snapshot — no per-page tracking yet. | `tx-subsystems/src/vm/structure/types.rs`, the new userfaultfd module, the ioctl arm | 1 |
| **P-10.4** | **Fault-path interception (vm).** In `fault_script` insert a branch before `require_fault_recipe`: if the VMA carries a ufd, install a request via `DelegateRegistry::install_request(DelegateRequest::Ufd(...), ufd_id, BestEffort, CancelOnDrop, mailbox.weak())`, enqueue the token on the ufd's `request_source`, and await reply via the new `await_agent_reply` helper. On reply, materialise the page via `materialize_pagebacked` using the reply's `src`/`zero` content. | `tx-subsystems/src/vm/execution.rs` (the fault_script loop), `tx-reactor/src/...` (helper), the userfaultfd module | 1.5 |
| **P-10.5** | **`UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_CONTINUE` ioctls.** Agent thread calls these on the ufd fd; they call `registry.mark_replied(id, DelegateReply::Ufd(UfdReply::...))`. PR-7B already posts the mailbox event from `mark_replied`. Also: **read-fault-message syscall** — agent's `read(ufd_fd, &mut uffd_msg)` dequeues a `DelegateTokenId` + request payload from the ufd's request queue. | the userfaultfd module, the ioctl/read arms | 1 |
| **P-10.6** | **Faulting thread resume path** — verify end-to-end that on `UFFDIO_COPY` the bound `TaskMailbox` gets `AgentReplied`, the resume helper returns the reply, and the fault_script re-runs under fresh epoch guard to install the page. Stress-test the reply-vs-CancelOnDrop race (handler-thread exits while a fault is in flight → `mark_endpoint_died` → faulting thread aborts with the equivalent of SIGBUS). | new `tx-subsystems/tests/v3_userfaultfd_e2e.rs` | 1 |
| **P-10.7** | Docs + ADR cross-refs. Add a `userfaultfd` row to `05_DELEGATE_v1.md` §10 (migration), close out the userfaultfd "first agent kind" risk in `07_BLAST_RADIUS.md` §6, and write a landing-summary STATUS.md entry. | docs only | 0.5 |

Critical path: P-10.0 → P-10.1 → P-10.3 → P-10.4 → P-10.5 →
P-10.6. P-10.2 and P-10.7 are parallel-safe.

## 7. Risk register (PR-10-scoped)

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `OpenFile` refactor regresses VFS read paths | Low | Medium | Land the parallel-fd-shape variant first; do the broader OpenFileBacking enum refactor as a follow-up if scope balloons |
| Fault path needs to re-acquire `RangeLock` after the agent reply | Medium | Low | The existing `fault_script` loop already re-acquires on every iteration; ufd's branch lives inside the same loop |
| `mark_endpoint_died` races with `mark_replied` from a different thread | Low | Low | DTOK-3 pin tests already cover this race shape; the agent-died walk's CAS pattern is identical to the timer-fire race |
| Reply payload's `src` pointer mid-COPY is invalidated by handler exit | Medium | Medium | Resume revalidates under fresh epoch guard per A-15; the `src` pointer is consumed during the `materialize_pagebacked` call before the resume returns to userspace |
| Read-fault-message ordering vs. `mark_endpoint_died` walk | Low | Medium | `request_source: WaitSource` on the ufd is itself notified by the registry's transition; dead-endpoint walks post `Abort` and the agent's `read()` returns 0 (POSIX "fd is gone") |

The substrate-level race risks are all already covered by PR-7's
DTOK-1 / DTOK-2 / DTOK-3 tests. PR-10 risks are concentrated in
**the fault-path branch** (phase 4) and **the OpenFile shape**
(phase 0).

## 8. Estimated total

**5–7 working days for PR-10.** Cells of the table above add to 7.0
days; the lower bound assumes phases 0 and 1 can compress and
phase 7 runs in parallel with phase 6 verification.

This is at the lower end of the `07_BLAST_RADIUS.md` §5.2 budget of
5–10 days, reflecting that PR-7 + PR-7B + D6 have done more
scaffolding than the v3-draft estimate accounted for.

## 9. AIO (PR-11) readiness — differs

PR-11 (AIO worker) is **not** a parallel readiness story. AIO
exercises `ExecutionScope::OnBehalfOf<P>` (`06_EXECUTION_SCOPE_v1`),
not `OnAgent`. The substrate primitive AIO needs is the
**`OnBehalfOf` framework** — a kthread that borrows a process's
subject identity, runs steps under that subject, and routes its
results back. None of the wake/mailbox/timer/registry work covers
this; the relevant invariant table in `07_BLAST_RADIUS.md` §4 row J
flags `OnBehalfOf<P>` as "net new" and "defers cleanly until first
user (AIO/SQPOLL)."

Practically: PR-11's substrate audit will look very different from
this ADR. The mailbox is reusable (a kthread is just another reactor
task), but the subject-borrow protocol, restriction-stack propagation,
and result-routing back to the originating syscall are net-new
surface. Plan on a **separate D-series ADR** before PR-11 starts.

## 10. Relationship to existing ADRs

- **PR-3 wake-substrate shape ADR** — D7 confirms PR-3's
  `TaskMailbox` / `WaitSource` choice carries OnAgent correctly.
  PR-7B's `AgentReplied` / `Abort` extensions of `MailboxEvent` are
  the necessary specialisation; nothing in PR-3 needs revision.
- **D4 (bus/mailbox layering)** — PR-10 depends on D4: a
  fault-side `Weak<TaskMailbox>` for the parked thread is the wake
  target. D4's move-down is a hard prerequisite, already landed.
- **D6 (timer-wheel layering)** — PR-10 does **not** exercise the
  timer wheel (ufd has no deadline), so D6 is informationally
  relevant but not a blocker.
- **PR-7 / PR-7B landing entries in STATUS.md** — those landings
  produced the registry + mailbox routing this ADR audits. D7's
  "GO" verdict is the contractual closure of the "PR-10 readiness"
  question they left open.

## 11. Success criteria for PR-10

PR-10 is "done" when:

- A Rust integration test (`v3_userfaultfd_e2e.rs`) registers a ufd
  on a VMA, spawns a handler thread that reads fault messages and
  replies with `UFFDIO_COPY`, faults a page in another thread,
  observes the page contents, and verifies the registry shows the
  token as `Replied`.
- A second test registers a ufd, spawns a handler that exits
  without replying, faults a page in another thread, and observes
  the faulting thread aborting with `AbortReason::AgentDied`
  (translated to SIGBUS or `EOWNERDEAD` at the syscall surface,
  per `05_DELEGATE_v1.md` §7).
- The `sys_userfaultfd` / `UFFDIO_REGISTER` / `UFFDIO_COPY` /
  `UFFDIO_ZEROPAGE` ioctl surface is reachable from the Linux
  syscall shim with no compile-time gating.
- `05_DELEGATE_v1.md` §10 migration table is updated to reflect ufd
  as landed.
