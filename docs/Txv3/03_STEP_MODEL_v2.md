# The Step Model — v2

<!-- txdoc:TXV3-STEP-MODEL-V2 -->

**Status.** v2 (Txv3 refresh, 2026-05).
**Supersedes.** `02_execution/STEP_MODEL_v1.md`. v2 retains the five-stage in-step discipline, the bounded-step rule, and the witness/cap discipline; refactors the outcome algebra from five variants to four variants plus a closed `YieldShape` catalog; introduces the `StepOp` and `StepProgress` traits; promotes `DriverMode` to a closed catalog with explicit accept/handle semantics.
**Companion documents.** `01_CONCEPTS_v5.md`, `02_INVARIANTS_v5.md`, `04_SYSCALL_SHAPE_v1.md`, `05_DELEGATE_v1.md`.

---

## 1. The primitive

<!-- txdoc:STEP-V2-PRIMITIVE-1 -->

A **step** is a synchronous, bounded, outcome-returning unit of work that a subsystem exposes for dispatch-layer invocation. Each step:

- Acquires its own epoch guard on entry and releases it on return.
- Consults predicates through `require_*`, producing witnesses scoped to the step.
- May upgrade witness observations to retention evidence for mutation.
- May mutate subsystem state via substrate primitives.
- May publish transitions via bus primitives.
- Returns exactly one outcome from the closed four-variant algebra.

Steps are the subsystem's execution surface. Drivers in the dispatch layer invoke steps and compose their outcomes; reactors run the driver's composed future; but neither driver nor reactor calls into subsystem state directly — only step functions do.

## 2. The outcome algebra

<!-- txdoc:STEP-V2-OUTCOME-ALGEBRA-1 -->

```rust
enum StepOutcome<T, P> {
    Continue { progress: P },
    Yield    { progress: P, shape: YieldShape },
    Done(T),
    Err(Errno),
}
```

Four variants. The two non-terminal variants share a `progress: P` field; whether progress is non-empty is the operational distinction between "the step did something observable" and "the step did nothing."

| Variant | Meaning | Driver action |
|---|---|---|
| `Continue { progress }` | The step made progress (possibly empty) and may make more without waiting. | Accumulate `progress`; re-invoke step. |
| `Yield { progress, shape }` | The step made progress (possibly empty) and now needs `shape` to resolve. | Accumulate `progress`; invoke `mode.handle(shape, ctx)`; on Resume, re-invoke step. |
| `Done(T)` | The operation terminates with value `T`. | Return `T`. |
| `Err(Errno)` | The operation terminates with error. | Return errno. |

### 2.1 Why four variants

<!-- txdoc:STEP-V2-WHY-FOUR-1 -->

The v1 algebra had five variants because progress and yielding were inlined into the variant set: `Advanced` (continue with progress), `Blocked` (yield with no progress), `AdvancedThenBlocked` (yield with progress), `Done`, `Err`. The variant explosion grew when a new yield primitive was added (Delegate would have added `Delegated` and `AdvancedThenDelegated`).

The v2 factoring orthogonalizes:

- **Progress axis** — the `progress: P` field on `Continue` and `Yield`. `P::EMPTY` is the no-progress case.
- **Continuation axis** — `Continue` (loop), `Yield(shape)` (yield through closed shape), `Done` / `Err` (terminate).
- **Shape axis** — closed `YieldShape` catalog.

New yield primitives plug into the YieldShape catalog (closed-catalog extension under ARCH-3); the StepOutcome stays at four variants.

The match-exhaustiveness property is preserved: every driver and yield-adapt site handles four outcomes, and within `Yield` handles all current YieldShape members. Adding a YieldShape is a compile-time error at every `match` that does not handle it.

### 2.2 Progress is typed

<!-- txdoc:STEP-V2-PROGRESS-TYPED-1 -->

```rust
trait StepProgress: Sized {
    const EMPTY: Self;
    fn is_empty(&self) -> bool;
    fn extend(&mut self, other: Self);
}
```

`StepProgress` is a monoid: `(Self, EMPTY, extend)` is associative with `EMPTY` as identity. `extend` is in-place to keep the driver's accumulator hot-loop allocation-free.

Concrete impls (closed-list-by-convention; per-subsystem additions are normal):

| Type | Use |
|---|---|
| `NoProgress` | one-shots: open, mkdir, fork, dup, close, mmap-reservation |
| `ByteProgress(usize)` | byte-moving ops: read, write, splice, sendfile, copy_file_range |
| `PageProgress { pages: u32 }` | page-moving ops: fault materialization, mlock-population, mmap-population |
| `EntryProgress { count: u32, cursor: DirCursor }` | enumeration ops: getdents |
| `IoVecProgress { iovecs_complete: u32, partial_bytes_in_current: usize }` | scatter/gather ops: readv, writev, preadv, pwritev |

The driver accumulates progress across `Continue` and `Yield` returns via `extend`. The accumulated progress is consumed at `Done(T)` to synthesize the syscall return.

### 2.3 The closed YieldShape catalog

<!-- txdoc:STEP-V2-YIELD-SHAPE-1 -->

```rust
enum YieldShape {
    OnCarrier {
        carrier: WakeCarrier,
        interests: InterestConditions,
    },
    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        request: DelegateRequest,
        token: Cap<DelegateToken>,
        deadline: Deadline,
        cancel: CancelPolicy,
    },
    // deferred catalog members:
    // OnEdge { subscription: Cap<EdgeSubscription>, interests: EdgeInterests },
    // OnHandoff { owned: Cap<OwnedSlot<T>>, priority: PriorityHint },
}
```

Each member's substrate cost, resume protocol, and abandonment semantics are stated in its specifying doc.

| Member | Substrate cost | Resume protocol | Abandonment |
|---|---|---|---|
| `OnCarrier` | zero (existing `RawQueue`/`RawPort`) | wait_event(carrier, condition, protocol) | wait outcome `Killed`/`Interrupted`/`TimedOut` |
| `OnAgent` | `DelegateToken` zone, endpoint zone, per-endpoint request bus carrier, RLIMIT_DELEGATE | await reply on token; verify token live; re-require | token → `SENTINEL_DEAD` on cancel/timeout/agent-death |
| `OnEdge` (deferred) | per-subscription edge-state slot, overflow mark | await edge fire; consume from subscription | subscription drop |
| `OnHandoff` (deferred) | `Owned<T>` ref strength, priority-donation lattice, owner-CAS primitive | await ownership transfer; resume holds `Owned<T>` | `EOWNERDEAD` |

Catalog extension is governed by ARCH-3.

## 3. The StepOp trait

<!-- txdoc:STEP-V2-STEP-OP-1 -->

```rust
trait StepOp {
    type Output;
    type Progress: StepProgress;
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress>;
}
```

A typed `StepOp` impl is the unit of subsystem work. The associated types make progress and output type-safe per-operation: `vfs::ReadOp` is `StepOp<Output = (), Progress = ByteProgress>` (the Output is `()` because the byte count flows through Progress and the script's `Done` synthesis pulls it out); `pipe::OpenOp` is `StepOp<Output = Fd, Progress = NoProgress>`.

Each step impl owns its private resume state in `&mut self`. The driver constructs the op, calls `step` repeatedly until `Done` / `Err`, and accumulates progress externally.

## 4. The five-stage in-step discipline (preserved from v1)

<!-- txdoc:STEP-V2-FIVE-STAGE-1 -->

When a step mutates state, it follows the v1 discipline unchanged:

```
1. Observe   — require produces witnesses under epoch guard
2. Upgrade   — IdentRef → Cap / PayloadCap / typed contribution
3. Reserve   — linear substrate reservations acquire slots/credit/ids
4. Commit    — substrate commit primitives install/remove/modify bindings
5. Publish   — bus primitives fire signal attachments for declared transitions
```

After commit, rollback is not part of the model. Errors detected after commit surface on the *next* step invocation as `Err`, not retroactively (anti-pattern A-9).

**Yield-aware addition (new in v2):** if a step would yield with non-empty progress, the publish step (5) runs *before* the yield is constructed. YIELD-4 enforces this: `Yield { progress, shape }` with non-empty progress requires that the progress's commit-and-publish has linearized before the yield's own reserve-phase begins.

## 5. The driver and DriverMode

<!-- txdoc:STEP-V2-DRIVER-1 -->

```rust
fn drive<S: StepOp>(
    mut op: S,
    ctx: &mut ScriptCtx,
    mode: DriveMode,
) -> impl Future<Output = Result<S::Output, Errno>>
```

The driver is the subsystem-agnostic loop that interprets `StepOutcome`:

```rust
async fn drive<S: StepOp>(mut op: S, ctx: &mut ScriptCtx, mode: DriveMode)
    -> Result<S::Output, Errno>
{
    let mut accumulated = S::Progress::EMPTY;
    loop {
        match op.step(ctx) {
            Continue { progress } => {
                accumulated.extend(progress);
                // loop
            }
            Yield { progress, shape } => {
                accumulated.extend(progress);
                match mode.handle(shape, &accumulated, ctx).await {
                    ResumeOutcome::Retry => { /* loop */ }
                    ResumeOutcome::Translate(Translation::EAGAIN) => return Err(EAGAIN),
                    ResumeOutcome::Translate(Translation::PartialReturn) =>
                        return Ok(accumulated.into_output()),  // POSIX partial
                    ResumeOutcome::Translate(Translation::UnsupportedShape) =>
                        return Err(EOPNOTSUPP),
                    ResumeOutcome::Failed(e) => return Err(e),
                }
            }
            Done(t) => return Ok(t),
            Err(e) => return Err(e),
        }
    }
}
```

### 5.1 The closed DriverMode catalog

<!-- txdoc:STEP-V2-DRIVER-MODE-1 -->

```rust
enum DriveMode {
    Nonblocking,
    Waiting,
    Selecting,
}

enum AcceptOutcome {
    Resolve,
    Translate(Translation),
}

enum Translation {
    EAGAIN,            // nonblocking, no progress
    PartialReturn,     // nonblocking, progress > EMPTY
    UnsupportedShape,  // hard rejection: EOPNOTSUPP
}

impl DriveMode {
    fn classify(&self, shape: &YieldShape, progress_empty: bool) -> AcceptOutcome {
        match (self, shape) {
            (Nonblocking, _) if progress_empty => Translate(Translation::EAGAIN),
            (Nonblocking, _) => Translate(Translation::PartialReturn),
            (Waiting, OnCarrier { .. }) => Resolve,
            (Waiting, OnAgent   { .. }) => Resolve,
            (Selecting, OnCarrier { .. }) => Resolve,  // register-only, never wait
            (Selecting, OnAgent { .. }) => Translate(Translation::UnsupportedShape),
            // future variants: same pattern
        }
    }
}
```

`Selecting` does not invoke `await`; it registers on the carrier and returns the readiness mask, leaving step invocation to the caller's epoll loop.

When `OnAgent` is added to a mode's accept set, the mode's `handle` implementation must define how it awaits the token reply, what protocol family applies (Killable, KillableTimeout, etc.), and how abandonment is reported. See `05_DELEGATE_v1`.

### 5.2 Selecting mode and OnAgent

<!-- txdoc:STEP-V2-SELECTING-AGENT-1 -->

`Selecting` does not currently accept `OnAgent` because epoll-shape registration on a delegated yield has no well-defined semantic: the step has not yet run, so no token has been issued. Future use cases (e.g., epolling a ufd endpoint to know when faults are pending) operate on the *agent's read-side fd*, not on the script's yield. They are a separate selecting target.

## 6. One-step operations (preserved from v1)

<!-- txdoc:STEP-V2-ONE-STEP-1 -->

Operations with no progressive behavior — `open`, `unlink`, `mkdir`, `rmdir`, `rename`, `fork`, `dup`, `close`, `mmap` (reservation, not population), `setsockopt` — are `StepOp`s with `Progress = NoProgress`, terminating in `Done` or `Err` on first call.

### 6.1 Example: open

```rust
struct OpenOp { path: PathBuf, flags: OpenFlags, mode: Mode, resume: Option<OpenResume> }

impl StepOp for OpenOp {
    type Output = Fd;
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<Fd, NoProgress> {
        let guard = epoch::guard();
        // observe
        let path_w   = require_path_resolvable(&self.path, ctx.cwd, &guard)?;
        let parent_w = require_parent_exists(path_w.clone(), &guard)?;
        let lookup_w = require_lookup(parent_w.clone(), self.path.last(), self.flags, &guard)?;
        let perm_w   = require_open_perm(lookup_w.clone(), self.flags, &ctx.subject, &guard)?;
        // upgrade
        let target_cap = upgrade_cap(lookup_w.target())?;
        // reserve
        let openfile_slot = zone::reserve::<OpenFile>()?;
        let fd_slot = ctx.subject_files()?.reserve_slot()?;
        let openfile = OpenFile::new(target_cap, self.flags);
        // commit
        zone::sign(openfile_slot, openfile);
        index::commit(ctx.subject_files()?, fd_slot, Cap::from_slot(openfile_slot));
        // publish — none for plain open
        Done(Fd(fd_slot.as_raw()))
    }
}
```

Yields possible only in upper-half resolution (dcache miss → `OnCarrier`; seccomp-trap → `OnAgent`). The lower-half `OpenOp` shown here is one-step.

### 6.2 Example: fork (preserved from v1, four-variant rephrasing)

Fork remains a single one-step `StepOp` despite its complexity (multiple zone signs and index commits in one step). Substrate commit primitives provide observer-safety — concurrent walkers see either no child or the fully-formed child, never an intermediate state. See v1 §5.3 for the worked example; the v2 difference is only that the outcome is `Done(Pid)` directly, with no intermediate `Advanced` state.

## 7. Multi-step operations

<!-- txdoc:STEP-V2-MULTI-STEP-1 -->

Operations with progressive behavior — `read`, `write`, `pread`, `pwrite`, `splice`, `sendfile`, `copy_file_range`, `getdents`, `readv`, `writev` — are `StepOp`s returning `Continue { progress }` or `Yield { progress, shape }` across multiple invocations.

### 7.1 Example: pipe read

```rust
struct PipeReadOp {
    pipe: Cap<Pipe>,
    buf: UserBufDesc,
    resume: Option<PipeReadResume>,
}

impl StepOp for PipeReadOp {
    type Output = ();             // total moves through Progress
    type Progress = ByteProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), ByteProgress> {
        let guard = epoch::guard();
        let pipe_w = require_pipe_readable(&self.pipe, &guard)?;
        let buf_w  = require_user_buf_writable(&self.buf, &ctx.subject, &guard)?;

        let pipe = pipe_w.ident();
        let avail = pipe.ring.available_read();
        let writers_closed = pipe.writers_closed();

        if avail == 0 {
            return if writers_closed {
                Done(())  // EOF; partial progress in driver's accumulator
            } else {
                Yield {
                    progress: ByteProgress::EMPTY,
                    shape: YieldShape::OnCarrier {
                        carrier: pipe.read_wq,
                        interests: ReadInterests::HasData | ReadInterests::Broken,
                    },
                }
            };
        }

        let _pipe_cap = upgrade_cap(pipe_w)?;
        let n = avail.min(self.buf.remaining());
        pipe.ring.pop_into(&mut self.buf.slice_mut(n));
        pipe.ring.advance_read(n);
        pipe.write_wq.fire(WriteReadiness::Space);

        if self.buf.remaining() == 0 {
            return Done(());
        }
        if pipe.ring.available_read() == 0 && !writers_closed {
            return Yield {
                progress: ByteProgress(n),
                shape: YieldShape::OnCarrier {
                    carrier: pipe.read_wq,
                    interests: ReadInterests::HasData | ReadInterests::Broken,
                },
            };
        }
        Continue { progress: ByteProgress(n) }
    }
}
```

`Continue { progress: ByteProgress(n) }` covers v1's `Advanced(n)`; `Yield { progress: ByteProgress::EMPTY, shape: OnCarrier{..} }` covers v1's `Blocked(..)`; `Yield { progress: ByteProgress(n), shape: OnCarrier{..} }` covers v1's `AdvancedThenBlocked(n, ..)`.

### 7.2 Example: FUSE read (delegated)

```rust
impl StepOp for FuseReadOp {
    type Output = ();
    type Progress = ByteProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), ByteProgress> {
        // observe + upgrade as usual; the fuse RNode's pagecache may have data
        // ...
        if let Some(cached) = self.cache_lookup(&guard) {
            return Continue { progress: cached.bytes };
        }
        // need to ask the fuse daemon
        let token = zone::reserve::<DelegateToken>()?;
        let req = FuseRequest::Read { ino: self.ino, offset: self.off, size: self.size };
        Yield {
            progress: ByteProgress::EMPTY,
            shape: YieldShape::OnAgent {
                endpoint: self.fuse_endpoint.clone(),
                request: DelegateRequest::Fuse(req),
                token: Cap::from_slot(token),
                deadline: Deadline::from(ctx.fuse_timeout),
                cancel: CancelPolicy::BestEffort,
            },
        }
    }
}
```

The two yield shapes (carrier and agent) are uniform from the script's perspective; the driver mode's `handle` does the right thing for each.

## 8. Subsystem module layout for steps (preserved from v1)

<!-- txdoc:STEP-V2-LAYOUT-1 -->

```
execution/pipe/
    op_read.rs         // pipe::PipeReadOp
    op_write.rs        // pipe::PipeWriteOp
    op_open.rs         // pipe::PipeOpenOp
    op_close.rs        // pipe::PipeCloseOp
    resume.rs          // PipeReadResume, PipeWriteResume types
    mod.rs             // re-exports
```

The v1 file naming `step_read.rs` is preserved as a convention; the v2 difference is that each file defines a `StepOp` impl rather than a free function.

## 9. Witness scope (preserved from v1, extended for yield boundary)

<!-- txdoc:STEP-V2-WITNESS-SCOPE-1 -->

Witness rules (WIT-3 / WIT-4) are strict and v5 adds WIT-5 / WIT-6 for the yield boundary. In practice:

- Witnesses are local within the `step` function body.
- Witnesses must not be stored in `&mut self`-fields of the `StepOp`, returned in `StepOutcome`, passed to other threads, or held across `.await` points.
- Reservation guards are subject to the same yield-boundary prohibition (YIELD-8): commit or roll back before yielding.
- Cross-step continuation carries `Cap<T>` / `OperationalEvidence` only.
- Cross-yield continuation (across `OnCarrier`, `OnAgent`, etc.) carries `'static` retention only — no witness, no reservation guard, no `IdentRef`.

Resume after yield re-acquires guard and re-runs `require_*` (anti-TOCTOU re-check, PRED-7 + WIT-5).

## 10. Anti-patterns

<!-- txdoc:STEP-V2-ANTI-PATTERNS-1 -->

The v1 anti-pattern catalog is preserved; v2 adds three new anti-patterns and updates the wording of those that referenced the five-variant outcome.

| ID | Anti-pattern | Violates | Fix |
|---|---|---|---|
| A-1 | Returning a witness in `StepOutcome`. | WIT-3, WIT-4 | Extract `Cap<T>` from witness; store in `&mut self` resume state. |
| A-2 | Storing a witness in `&mut self` or a static. | WIT-4 | Treat witness as strictly stack-local. |
| A-3 | Calling `.await` inside `step()`. | STEP-2 | Return `Yield { shape: ... }`; let driver compose. |
| A-4 | Driver inspecting subsystem state. | DISP-2 | Move check into op's observe stage. |
| A-5 | Firing a signal before the corresponding mutation. | SIG-4 | Always fire after `substrate::*_commit` returns. |
| A-6 | Firing a signal from outside publish stage. | SIG-6 | Signals only from publish stage. |
| A-7 | Direct field mutation bypassing substrate primitives. | STEP-4 | Mutations go through `substrate::index::*` / `zone::sign`. |
| A-8 | Make progress, then mutate further, then return Yield without the earlier progress. | STEP-3, STEP-4, STEP-8 | Return `Yield { progress, shape }` carrying both. |
| A-9 | Returning `Err` after substrate commits succeeded. | STEP-3, STEP-8 | Error detection precedes commit. Errors after commit surface on next step. |
| A-10 | Carrying a witness across Yield-then-resume. | WIT-3, WIT-5 | Stash `Cap<T>` in resume state; downgrade + re-require on resume. |
| A-11 | Acting on values read or computed during a previous step without re-verifying under current guard. | STEP-7 | Re-derive state-sensitive decisions from `require_*` under current guard. |
| A-12 | Driver inspecting subsystem state through a "convenient" side channel. | DISP-2, DISP-6 | Encode the decision as a `StepOutcome`. |
| **A-13** | **Carrying a reservation guard across a Yield boundary.** | **YIELD-8, WIT-6** | **Commit or rollback before yield. No "carry, commit on resume."** |
| **A-14** | **Yielding with non-empty `progress` whose commit-and-publish has not yet linearized.** | **YIELD-4, STEP-4** | **Run publish (stage 5) before constructing the Yield.** |
| **A-15** | **Treating yield resolution (wake / agent reply / edge fire) as truth without re-require.** | **WIT-5, PRED-7** | **Re-acquire guard; re-run `require_*`; resume's outcome is not authority.** |

## 11. Open questions (preserved from v1)

<!-- txdoc:STEP-V2-OPEN-QUESTIONS-1 -->

11.1. **Bounded-work granularity.** Per-subsystem convention; canonical bounds in subsystem `execution/` docs.

11.2. **Resume state versioning.** Out of scope for v2; relevant if checkpoint/restart is added.

11.3. **Step-local tracing.** Tracepoints fire during publish; in-step observability beyond that is deferred.

11.4. **Selecting on OnAgent.** Currently rejected (`UnsupportedShape`); revisit if a use case lands.

## 12. v2 changelog

<!-- txdoc:STEP-V2-CHANGELOG-1 -->

| Aspect | v1 | v2 |
|---|---|---|
| Outcome variants | 5 (Advanced, Blocked, AdvancedThenBlocked, Done, Err) | 4 (Continue, Yield, Done, Err) |
| Yield shape | implicit in Blocked / AdvancedThenBlocked | explicit closed `YieldShape` catalog |
| Step container | free function | typed `StepOp` trait with associated Output / Progress |
| Progress | `usize`-shaped | `StepProgress` trait with monoid laws and per-op concrete impls |
| Driver modes | three modes, ad-hoc | closed catalog with `DriveMode::classify` returning `AcceptOutcome` |
| Wait-adapt phase class | "wait-adapt" | "yield-adapt" (consumes any closed YieldShape) |
| Anti-patterns | A-1 through A-12 | + A-13 (reservation across yield), A-14 (uncommitted progress), A-15 (resolution-as-truth) |

The migration is mechanical at every site: see `07_BLAST_RADIUS` for the count.
