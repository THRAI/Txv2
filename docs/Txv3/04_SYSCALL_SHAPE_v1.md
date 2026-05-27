# Syscall Shape — Upper/Lower Split — v1

<!-- txdoc:TXV3-SYSCALL-SHAPE-V1 -->

**Status.** v1 (Txv3 refresh, 2026-05).
**Purpose.** Codify the upper/lower split that organizes every syscall script into an *identity-side* upper half and a *payload-side* lower half. Worked examples cover the full cross-product of native vs borrowed identity, restriction-stack walks, subject mutation, and delegated payload work.
**Audience.** Script authors writing syscall implementations; reviewers evaluating script structure; shim authors translating Linux syscalls.
**Companion documents.** `01_CONCEPTS_v5.md §10`, `02_INVARIANTS_v5.md (SUBJ-*, SCRIPT-V5-*)`, `03_STEP_MODEL_v2.md`, `05_DELEGATE_v1.md`, `06_EXECUTION_SCOPE_v1.md`.

---

## 1. The split

<!-- txdoc:SYSCALL-V1-SPLIT-1 -->

A syscall script decomposes along the same axis as the kernel's identity/payload bifurcation (BIF-2):

| Half | Concerns | Composes |
|---|---|---|
| **Upper half** | identity, signifier, authority, restriction stack | typed `StepOp`s over `&SubjectContext`: cred check, fd/path/pid resolution, seccomp filter, LSM hook, restriction-stack walk |
| **Lower half** | payload transition | typed `StepOp`s over object payload: vfs / pipe / net / vm / proc / signal step functions |

Both halves use `StepOp` / `StepOutcome` / `YieldShape` / `drive` identically. The split is about *concerns*, not *mechanism*. Both halves can yield. The upper half terminates with a resolved `Cap<T>` on the lower half's input object, a typed errno, or a yield.

This split is not a layer in the codebase; it is a discipline a script author follows. A script may be all-upper-half (`getpid`), all-lower-half within a wrapper that sets up subject (rare), or canonically both (most syscalls).

## 2. The SubjectContext

<!-- txdoc:SYSCALL-V1-SUBJECT-1 -->

```rust
struct SubjectContext {
    process: Cap<ProcessIdentity>,
    thread: Option<Cap<ThreadIdentity>>,
    authority: SubjectAuthority,
}

struct SubjectAuthority {
    cred: Cap<Credential>,
    restrictions: Cap<RestrictionStack>,   // append-only
}

impl SubjectContext {
    fn from_thread(task: &ThreadTask) -> Self { /* native syscall entry */ }
    fn borrowed(owner: Cap<ProcessIdentity>, restrictions: ...) -> Self { /* OnBehalfOf */ }
}
```

There is no `current_subject_context()` accessor (SUBJ-1). Helpers receive `&SubjectContext` explicitly. Native syscall trampolines materialize a context from the running thread's task; `OnBehalfOf` scopes (see `06_EXECUTION_SCOPE_v1`) materialize one from a borrowed identity.

### 2.1 The script's ScriptCtx

<!-- txdoc:SYSCALL-V1-SCRIPT-CTX-1 -->

The script frame's mutable state — what individual `StepOp::step` calls receive — is `ScriptCtx`:

```rust
struct ScriptCtx<'a> {
    subject: SubjectContext,
    deadline: Option<Deadline>,
    trace: TraceFrame,
    // ... per-script frame data
}
```

`ScriptCtx ⊃ SubjectContext`. The driver constructs `ScriptCtx` at script entry and threads `&mut ScriptCtx` through every `step` call. Authority lookups (cred, fd table, vm, rlimit) reach through `ctx.subject`.

## 3. Worked examples

<!-- txdoc:SYSCALL-V1-EXAMPLES-1 -->

The five canonical examples cover the cross-product of native vs borrowed, simple vs restricted, immutable subject vs mutating, and carrier vs agent yielding.

### 3.1 Native syscall, simple lower-half: sys_read

<!-- txdoc:SYSCALL-V1-SYS-READ-1 -->

```rust
async fn sys_read(fd: RawFd, buf: UserPtr, len: usize) -> Result<usize, Errno> {
    // Subject established at the trampoline.
    let mut ctx = ScriptCtx::for_thread(current_task());

    // Upper half: signifier resolution under the subject.
    let of = fd_resolve(&mut ctx, fd, Access::Read).await?;
    let ubuf = UserBufDesc { ptr: buf, len };

    // Lower half: typed StepOp on the payload.
    let op = vfs::ReadOp::new(of, ubuf, len);
    let bytes = drive(op, &mut ctx, DriveMode::Waiting).await?;
    Ok(bytes)
}
```

`fd_resolve` is itself a `drive(...)` over a small upper-half `StepOp` (`fd::ResolveOp`) that walks the fd table, checks the open mode, and may yield on dcache miss or seccomp-trap.

### 3.2 Borrowed syscall: drive_sqe_read (io_uring shape)

<!-- txdoc:SYSCALL-V1-SQE-READ-1 -->

```rust
async fn drive_sqe_read(owner: Cap<ProcessIdentity>, sqe: ReadSqe) -> Result<usize, Errno> {
    with_on_behalf_of(owner, |mut ctx| async move {
        // The lower half is the same as native sys_read.
        // The difference is the SubjectContext: borrowed, not from current thread.
        let of = fd_resolve(&mut ctx, sqe.fd, Access::Read).await?;
        let ubuf = UserBufDesc { ptr: sqe.buf, len: sqe.len };
        let op = vfs::ReadOp::new(of, ubuf, sqe.len);
        drive(op, &mut ctx, DriveMode::Waiting).await
    }).await
}
```

The lower-half `vfs::ReadOp` is *identical* between sys_read and drive_sqe_read. All authority work flows through `&ctx.subject`. RLIMIT charges, cred checks, and fd-table lookups all resolve against the borrowed identity. See `06_EXECUTION_SCOPE_v1`.

### 3.3 Restricted upper-half: sys_open with seccomp + LSM

<!-- txdoc:SYSCALL-V1-SYS-OPEN-1 -->

```rust
async fn sys_open(path: UserPath, flags: OpenFlags, mode: Mode) -> Result<RawFd, Errno> {
    let mut ctx = ScriptCtx::for_thread(current_task());

    // Upper half: restriction-stack walk before signifier resolution.
    let restriction_outcome = drive(
        cred::WalkRestrictionsOp::new(SyscallId::Open, &SyscallArgs::Open { /* args */ }),
        &mut ctx,
        DriveMode::Waiting,  // may yield OnAgent for seccomp-trap or fanotify-perm
    ).await?;
    match restriction_outcome {
        RestrictionResult::Allow => {}
        RestrictionResult::Deny(e) => return Err(e),
        // Trap is already handled inside the WalkRestrictionsOp via OnAgent yield.
    }

    // Upper half continued: path resolution.
    let path_buf = path.copy_in(&ctx.subject)?;
    let resolved = drive(
        vfs::PathResolveOp::new(path_buf, ctx.cwd(), flags, mode),
        &mut ctx,
        DriveMode::Waiting,
    ).await?;

    // Lower half: install the OpenFile and the fd binding.
    let op = vfs::OpenOp::new(resolved, flags, mode);
    let fd = drive(op, &mut ctx, DriveMode::Waiting).await?;
    Ok(fd.as_raw())
}
```

The restriction-stack walk is upper-half because it is identity-and-authority work. A seccomp-trap restriction issues a `YieldShape::OnAgent` to the registered tracer; the tracer's `DelegateReply` mutates the syscall outcome (continue / route to different syscall / inject signal / kill). LSM stacking layers on top of seccomp via the same restriction stack.

### 3.4 Subject-mutating: sys_execve

<!-- txdoc:SYSCALL-V1-SYS-EXECVE-1 -->

```rust
async fn sys_execve(path: UserPath, argv: UserVec, envp: UserVec) -> Result<!, Errno> {
    let mut ctx = ScriptCtx::for_thread(current_task());

    // Upper half: pre-PoNR. SubjectContext is the *current* one.
    let path_buf = path.copy_in(&ctx.subject)?;
    let target = drive(
        vfs::PathResolveOp::new(path_buf, ctx.cwd(), OpenFlags::EXEC, Mode::default()),
        &mut ctx,
        DriveMode::Waiting,
    ).await?;
    let argv_kv = drive(exec::CopyArgvOp::new(argv, envp), &mut ctx, DriveMode::Waiting).await?;
    let new_creds = drive(
        cred::ExecPrivilegeOp::new(target.clone(), &ctx.subject),
        &mut ctx,
        DriveMode::Waiting,
    ).await?;  // may compute suid, ambient caps, securebits, no_new_privs

    // Point of no return crosses here: subject authority replacement (SUBJ-3).
    let post_ponr_ctx_authority = new_creds.into_authority();

    // Lower half (post-PoNR, infallible from here):
    drive(vm::TearDownOldAddressSpaceOp::new(), &mut ctx, DriveMode::Waiting).await?;
    let new_as = drive(
        vm::BuildNewAddressSpaceOp::new(target, argv_kv),
        &mut ctx,
        DriveMode::Waiting,
    ).await?;

    // SubjectContext authority replacement: linearization point.
    ctx.subject.replace_authority(post_ponr_ctx_authority);

    drive(thread::ResetForExecOp::new(new_as), &mut ctx, DriveMode::Waiting).await?;
    drive(signal::ResetForExecOp::new(), &mut ctx, DriveMode::Waiting).await?;

    // The script terminates by transferring control to userspace at the new entry.
    // Done(()) is conventionally written as Done(!) for execve.
    Ok(/* unreachable, returns to user */)
}
```

The execve script demonstrates that `SubjectContext.authority` is *replaceable* mid-script (SUBJ-3). The replacement is a publication boundary — observers see old-or-new authority, never an intermediate. Lower-half steps after the replacement see the new authority.

### 3.5 Delegated payload: sys_read on a FUSE inode

<!-- txdoc:SYSCALL-V1-FUSE-READ-1 -->

```rust
// Outer skeleton identical to §3.1. The difference materializes inside vfs::ReadOp,
// which dispatches by the resolved RNode's backing kind:

impl StepOp for vfs::ReadOp {
    type Output = ();
    type Progress = ByteProgress;

    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), ByteProgress> {
        match self.of.rnode().backing_kind() {
            BackingKind::PageCache => self.step_page_cache(ctx),
            BackingKind::Fuse => self.step_fuse(ctx),       // Yields OnAgent
            BackingKind::Pipe => self.step_pipe(ctx),       // Yields OnWaitSource
            BackingKind::Tty  => self.step_tty(ctx),
            // ...
        }
    }
}
```

The driver loop and the script wrapper do not change. The yield shape produced by the inner step is what differs, and the driver mode handles each shape per its protocol. From the script's POV, "the read is a read"; the kernel's choice of yield primitive is encapsulated.

## 4. Cross-cutting properties

<!-- txdoc:SYSCALL-V1-PROPERTIES-1 -->

### 4.1 Subject is constant within a script except via SUBJ-3

The script's `ctx.subject.process` and `ctx.subject.thread` do not change for the script's duration. `ctx.subject.authority` may change once or twice via cred-service transition commits (suid exec; setuid family). Anything else is a SCRIPT-* violation.

### 4.2 The lower half is identity-agnostic

A lower-half `StepOp` does not encode whose authority it runs under. It receives `&mut ScriptCtx` and uses `ctx.subject` for whatever it needs (cred check, fd lookup, vm access, rlimit charge). This is what lets `vfs::ReadOp` work identically for native sys_read and OnBehalfOf sqe_read.

### 4.3 Both halves yield identically

A dcache miss in upper-half `vfs::PathResolveOp` returns `Yield { shape: OnWaitSource { source: dcache.fill_source.id(), ... } }`. A FUSE read in lower-half `vfs::ReadOp` returns `Yield { shape: OnAgent { endpoint: ..., request: ..., ... } }`. The driver mode handles both. There is no special "upper-half-only" or "lower-half-only" yield shape.

### 4.4 The upper-half terminates the lower-half input

The upper half's job is to produce a `Cap<T>` (or operational evidence) on the object the lower half operates on, plus any auxiliary capabilities (user buffer descriptors, argv vectors). The lower half's `StepOp` constructor consumes those. There is no shared mutable state between the halves except `&mut ScriptCtx`.

### 4.5 Restrictions are upper-half observe phases

Seccomp BPF filters, Landlock rules, and LSM hook stacks are all members of `SubjectAuthority::restrictions`. Their evaluation is part of the upper-half observe stage (typically the first upper-half step). A trap outcome (seccomp `SECCOMP_RET_TRACE` or LSM userspace mediation) becomes an `OnAgent` yield from inside the upper-half `StepOp`. The script does not see the trap explicitly — the upper-half `StepOp` yields, the tracer's reply mutates the outcome, and either the script continues or returns the tracer-decided result.

## 5. The trampoline

<!-- txdoc:SYSCALL-V1-TRAMPOLINE-1 -->

The shim layer (Linux syscall trampoline) is responsible for:

1. Materializing a `SubjectContext` from the trapped thread's task.
2. Constructing a `ScriptCtx` wrapping the subject plus deadline / trace.
3. Calling the canonical script function (`sys_read`, `sys_open`, etc.).
4. Translating the script's `Result<T, Errno>` into the userspace ABI.

For OnBehalfOf cases (io_uring SQPOLL, AIO worker), the trampoline is replaced by a kthread loop that materializes the SubjectContext via `with_on_behalf_of` and dispatches to the same script functions.

## 6. Dispatch lanes

<!-- txdoc:SYSCALL-V1-DISPATCH-LANES-1 -->

A syscall enters one of three dispatch lanes after the trampoline materializes the `SubjectContext`. The lanes differ in whether they enter `StepOp` / `drive`, and whether they may yield.

| Lane | Enters StepOp? | Enters drive? | May yield? | Typical syscalls |
|---|---|---|---|---|
| **ImmediateSyscall** | no | no | no | getpid, getuid, umask, times |
| **OneShotStepOp** | yes | drive_oneshot only | no | setuid, sigaction, setsid, chdir, close |
| **Full async script** | yes | async drive | yes | read, write, open, futex_wait, poll |

### 6.1 ImmediateSyscall

An `ImmediateSyscall` is a syscall whose entire call chain is statically non-yielding. It does not:

- call `drive()` or `drive_oneshot()`;
- construct a `StepOutcome` or `YieldShape`;
- enter VFS path resolution or VM materialization;
- register on a `WaitSource` or `DelegateEndpoint`;
- acquire a guard that crosses the return boundary.

It *may* acquire short-lived guards for reading subject/process state; the guard must be dropped before return and no guard handle may escape the call.

```rust
/// Marker trait for syscalls that never yield.
trait ImmediateSyscall: sealed::Sealed {
    type Output;

    fn call(ctx: &ImmediateCtx, args: SyscallArgs)
        -> Result<Self::Output, Errno>;
}
```

`ImmediateCtx` is narrower than `ScriptCtx` — it carries `&SubjectContext`, `&ThreadTask`, and `&TrapFrameView`, but no VFS, VM, reactor, timer, or mailbox handle.

Immediate syscalls in the current surface (17 of 90 wired):

```
getpid / getppid / getpgrp / getpgid / getsid
getuid / geteuid / getgid / getegid
getresuid / getresgid
times / gettimeofday / umask / prlimit64 / uname
rt_sigreturn
```

### 6.2 OneShotStepOp

A `OneShotStepOp` is a `StepOp<Progress = NoProgress>` whose first `step()` invocation returns `Done(T)` or `Err(Errno)`. It never returns `Continue` or `Yield`.

```rust
/// A StepOp that terminates on first invocation.
trait OneShotStepOp:
    StepOp<Progress = NoProgress> + sealed::Sealed
{}

/// Synchronous drive for one-shot ops.
fn drive_oneshot<O: OneShotStepOp>(
    op: &mut O,
    ctx: &mut ScriptCtx,
) -> Result<O::Output, Errno> {
    match op.step(ctx) {
        StepOutcome::Done(v) => Ok(v),
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            kernel_bug!("OneShotStepOp violated contract")
        }
    }
}
```

If a `OneShotStepOp` yields, it is a kernel invariant violation (STEP-11), not a user-visible `EAGAIN`. This is stronger than the `Nonblocking` driver mode, which *translates* unexpected yields to `EAGAIN`.

One-shot syscalls have semantic transitions (observe → upgrade → reserve → commit → publish) but never block. They are the middle lane between pure ABI queries and full async scripts.

One-shot syscalls in the current surface (43 of 90 wired):

```
Credential:    setuid, setgid, setreuid, setregid, setresuid, setresgid
Process:       setsid, setpgid, set_tid_address, set_robust_list
Signal:        sigaction, sigprocmask, kill, tkill, tgkill
Exit:          exit, exit_group
Fd table:      close, dup, dup3, fcntl, pipe2
Directory:     chdir, mkdirat, unlinkat, symlinkat, linkat, renameat2
File attr:     fchmodat, fchownat, utimensat, faccessat, faccessat2
Stat:          newfstatat, fstat, statx
Other:         ioctl (sync variants), lseek, readlinkat, madvise, signalfd4 (create)
```

### 6.3 Full async script

A full async script may `Continue`, `Yield` (any `YieldShape`), and requires the full driver loop with `DriverMode`, `ActiveWait`, `WaitProtocol`, and `apply_resume`. These syscalls enter `drive(op, ctx, mode).await`.

Full async syscalls in the current surface (29 of 90 wired):

```
IO:            read, write, readv, writev
Path:          openat
Poll:          ppoll
Directory:     getdents64
Futex:         futex (wait+wake composite)
Timer:         nanosleep, clock_nanosleep, clock_gettime
VM:            brk, mmap, munmap, mprotect, mremap, msync
Process:       clone, execve, wait4
AIO:           io_setup, io_submit, io_getevents, io_destroy
Other:         io_uring_setup, io_uring_enter, userfaultfd, getrandom
```

### 6.4 Design rationale

The three lanes exist because forcing all syscalls through `async drive` would impose unnecessary cost on the 60% of wired syscalls that never yield (17 immediate + 43 one-shot). It would also obscure the architectural distinction between:

- **ABI queries** — pure reads of kernel-side process/credential/time state.
- **Semantic transitions** — mutations with observe→commit→publish discipline but no blocking.
- **Progressive operations** — data transfer or blocking ops that may yield to VFS/VM/timer/wait sources.

The `ImmediateSyscall` trait and `OneShotStepOp` trait are the type-level markers that enable lint enforcement: a function implementing `ImmediateSyscall` must not call `drive`; a function implementing `OneShotStepOp` must not return `Yield`. These are checked by `SCRIPT-V5-4` and `STEP-11` respectively.

## 7. Migration note

<!-- txdoc:SYSCALL-V1-MIGRATION-1 -->

Existing v4 scripts that thread an implicit `current_thread()`-shaped context become v5 scripts by:

1. Replacing the implicit access with an explicit `&SubjectContext` parameter at every helper boundary.
2. Adding the `ScriptCtx::for_thread(current_task())` line at the trampoline entry.
3. Renaming `ThreadContext` to `SubjectContext` if the existing code used it.
4. Removing any `current_subject_context()` / `current_cred()` / `current_thread()` global accessors (they violate SUBJ-1).

The blast radius for this migration in mainline is zero (no existing types reference `SubjectContext` / `ThreadContext` / `ScriptCtx`); see `07_BLAST_RADIUS.md` for the count.
