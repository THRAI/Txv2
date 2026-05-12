# Decision D1: ScriptCtx / identity coupling — trait-bound subject identity

**Date:** 2026-05-11
**Status:** decided
**Companion:** [D2](2026-05-11-d2-waitsource-coexists-with-rawport.md), [D3](2026-05-11-d3-walker-async-carveout.md)

## Decision

Choose **B**: `step_v3` owns structural traits and algebra; `tx-subsystems`
owns concrete `ProcessIdentity` / `Credential` / `RestrictionStack`;
`tx-scripts` or `tx-kernel` defines production aliases such as
`KernelScriptCtx`. **`ScriptCtx` must not store an `epoch::Guard`**;
guards are step-local.

## Rule

Do not turn PR-9 into a process-subsystem relocation.

## Why not A (own)

Option A would move `process::ProcessIdentity` into `step_v3` /
`tx-substrate`. It sounds canonical if "identity is a primitive cell,"
but **the algebra crate should not own the concrete process object**.
Doing so drags process lifecycle, credentials, restrictions, exit
source, rlimits, and possibly thread identity toward the algebra
layer. That is not "v3 cleanup" — it is a process-subsystem
relocation, and out of scope for v3.

## Why not C (re-export)

C makes `step_v3` import `tx-subsystems`. The lower layer would know
the upper semantic owner. That makes future reuse of `StepOutcome`,
`YieldShape`, `StepProgress`, and driver code harder, and inverts
the layering rule.

## Why B (trait-bound)

B preserves the architecture:

```
step_v3       defines the contract
tx-subsystems implements the contract
tx-scripts    binds the concrete production context
```

## Recommended shape

```rust
// In step_v3:

pub trait SubjectIdentity: ZoneObject {
    type Credential: CredentialView;
    type Restrictions: RestrictionStackView;
    type ThreadIdentity;

    fn credential(&self) -> Cap<Self::Credential>;
    fn restrictions(&self) -> Cap<Self::Restrictions>;
    fn exit_source(&self) -> Option<WaitSourceId>;
}

pub trait CredentialView {
    // intentionally narrow
}

pub trait RestrictionStackView {
    // append-only restriction walk interface
}

pub struct SubjectContext<I: SubjectIdentity> {
    pub process: Cap<I>,
    pub thread: Option<Cap<I::ThreadIdentity>>,
    pub authority: SubjectAuthority<I>,
}

pub struct SubjectAuthority<I: SubjectIdentity> {
    pub cred: Cap<I::Credential>,
    pub restrictions: Cap<I::Restrictions>,
}
```

Production binds it once:

```rust
pub type KernelSubjectContext =
    step_v3::SubjectContext<tx_subsystems::process::ProcessIdentity>;

pub type KernelScriptCtx =
    step_v3::ScriptCtx<tx_subsystems::process::ProcessIdentity>;
```

Most step code sees `fn step(&mut self, ctx: &mut KernelScriptCtx)`
rather than generics everywhere.

## Important correction: guard is step-local, not ScriptCtx-held

`ScriptCtx` lives across async/yield boundaries. **An `epoch::Guard`
must not.** If `ScriptCtx` carried a guard, the no-guard-across-yield
invariant would be violated on the first `.await` after binding.

Better shape:

```rust
pub struct ScriptCtx<I: SubjectIdentity> {
    pub subject: SubjectContext<I>,
    pub deadline: Option<Deadline>,
    pub trace: TraceFrame,
    pub mailbox: Cap<TaskMailbox>,
    // no epoch::Guard here
}

pub struct StepCtx<'g, I: SubjectIdentity> {
    pub script: &'g mut ScriptCtx<I>,
    pub guard: &'g epoch::Guard,
}
```

Step bodies acquire the guard locally:

```rust
fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<...> {
    let guard = epoch::guard();
    let mut step_ctx = StepCtx {
        script: ctx,
        guard: &guard,
    };
    // require_* receives &step_ctx or &guard
}
```

This preserves the invariant:

```
ScriptCtx crosses awaits/yields.
epoch::Guard does not.
```

## What this unblocks

PR-9 can proceed with a concrete production alias
(`KernelScriptCtx = ScriptCtx<process::ProcessIdentity>`). The seven
canonical syscalls thread `&mut KernelScriptCtx` without waiting for
identity relocation. The 80 PR-2 wraps become useful because they
now receive a real subject context, but the process subsystem stays
where it is.

## Open work this enables (not in this ADR)

- Add `SubjectIdentity` / `CredentialView` / `RestrictionStackView`
  trait declarations to `step_v3`.
- Add `tx-subsystems` impl on `process::ProcessIdentity`.
- Define `tx-kernel::KernelScriptCtx` alias.
- Wire `StepCtx<'g, I>` step-local guard helper.
- Migrate the 7 canonical syscalls in PR-9.
