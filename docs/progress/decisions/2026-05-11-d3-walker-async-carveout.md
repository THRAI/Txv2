# Decision D3: Walker async surface — explicit carve-out from StepOp

**Date:** 2026-05-11
**Status:** decided
**Companion:** [D1](2026-05-11-d1-scriptctx-trait-bound-identity.md), [D2](2026-05-11-d2-waitsource-coexists-with-rawport.md)

## Decision

Choose **C**: `vfs::walker::step_walk` and `step_open` remain
**async script-level resolution functions**, not `StepOp` impls.
This is an **explicit carve-out**, not an accidental leftover.

## Rule

Do not turn PR-2 into a path-walker state-machine rewrite.

## Why not A (convert walker to sync StepOp)

A is architecturally clean:

```
PathWalkOp::step()
  -> Continue
  -> Yield
  -> apply_resume
  -> Continue
  -> Done
```

But path walking is not a simple leaf operation. It includes:

```
component iteration
dcache lookup
mount traversal
symlink expansion
EAGAIN / retry cycles
permission checks
open terminal-mode differences
possibly delegation / permission agents
recursion-like behavior
```

Rewriting `walk_inner_v3` into a hand-authored synchronous state
machine is real work. It is **not** required to unblock v3
foundation. Forcing A now turns PR-2 into "rewrite Linux path
resolution as a state machine" — the wrong scope.

## Why not B (AsyncStepOp trait variant)

An `AsyncStepOp` trait would weaken the core `StepOp` rule:

```
StepOp::step is synchronous and bounded.
Yield is explicit.
No hidden await inside step.
```

Once `AsyncStepOp` exists, future authors will use it for
convenience. The distinction between "yield as explicit runtime
contract" and "await hidden inside an operation" blurs.

That is especially bad for the lost-wake and witness-scope rules.
The whole point of `StepOp::step` being sync is that no witness,
guard, or reservation can accidentally cross an `.await`.

**Reject B** unless a later feature proves async step bodies are
necessary.

## Why C (carve-out)

The right classification is structural:

```
script layer:
  async composition
  path walker
  fd resolver
  restriction-stack walk
  syscall sequencing

StepOp layer:
  bounded synchronous semantic transitions
  pipe read chunk
  page materialization attempt
  dentry mutation commit
  fd table install
  rnode operation
```

Path walking sits in the script layer because it composes many
smaller observations and can drive yields internally. Treating it
as a `StepOp` would force a synchronous state machine where the
natural shape is async composition.

## Invariant: `WALKER-CARVEOUT-1`

`vfs::walker::{step_walk, step_open}` are script-level async
resolution functions. They are not `StepOp` implementations in PR-2.

They must still obey the same external yield safety rules:

- no `epoch::Guard` across `.await`
- no witness across `.await`
- no reservation guard across `.await`
- resume revalidates path state

## Status of the walker functions today

- `pub async fn vfs::walker::step_walk(...)` — script-level resolver.
- `pub async fn vfs::walker::step_open(...)` — script-level resolver.
- PR-2 wave 3 worker Q1 attempted to wrap them; identified them as
  async-typed and skipped per this carve-out (then-implicit, now
  explicit).

## Future migration path

C does not prevent A later. If the walker stabilizes, introduce:

```rust
struct PathResolveOp {
    state: WalkState,
    resume: Option<WalkResume>,
}

impl StepOp for PathResolveOp {
    type Output = ResolvedPath;
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx)
        -> StepOutcome<ResolvedPath, NoProgress>;
}
```

This should be a **dedicated VFS PR**, not part of PR-2.

## What this unblocks

PR-2 can be declared honestly complete for the production surface:

```
The two unwrapped walker functions (step_walk, step_open) are
intentional script-level async resolvers, not forgotten StepOps.
```
