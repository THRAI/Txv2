# Decision: PR-1.6 — keep `FsOps`, stop calling it v4

**Date:** 2026-05-11
**Status:** decided

## Decision

Do not delete `FsOps` in PR-1.6. Keep it as the live VFS / filesystem
operation vtable. Treat it as canonical v3 if its methods return v3
`StepOutcome` / `V3Outcome`. Do not migrate production dispatch away
from `Arc<dyn FsOps>` in this PR.

## Why this is the right call

The wave-9h-ζ author treated "delete the v4 trait" as the goal. That
is the **wrong invariant.**

The v3 migration requires **StepOutcome shape unification**, not
trait-identity unification. If `FsOps` now returns the v3 outcome
shape, then it is already a v3 trait in substance. The old name is
not enough reason to redesign the filesystem dispatch layer.

`FsOps` has real production dispatch pressure:

```
shim layer
  -> Arc<dyn FsOps>
  -> fs_basic.rs / fs_mut.rs / fs_path.rs
  -> ~30 call sites
```

Deleting it forces a bigger design decision:

- What replaces `Arc<dyn FsOps>`?
  - enum dispatch?
  - per-filesystem concrete typing?
  - unified `StepOp` provider?
  - vtable over operation objects?
  - backend-specific associated types?

That is not a mechanical v3 migration. That is a filesystem dispatch
redesign — out of scope for v3.

## What to do with `FsPageBacking`

Keep `FsPageBacking`, but do not make it a replacement for `FsOps`.
The split is:

| Trait | Role |
|---|---|
| **`FsOps`** | filesystem / VFS operation surface; object-safe; dynamically dispatched from VFS/shim code; open, lookup, mkdir, unlink, rename, readdir, metadata, etc. |
| **`FsPageBacking`** | page-cache / pager-facing backing surface; readpage, writepage, populate, dirty/writeback, maybe allocation hooks |

`FsPageBacking` does **not** absorb all filesystem operations merely
because page-cache-backed filesystems implement both.

## Handling the wave-9h-ζ self-recursion case

The dangerous pattern:

```rust
impl FsPageBacking for Devfs {
    fn fallocate(...) -> V3Outcome {
        Self::fallocate(self, ...)
    }
}
```

`Self::fallocate` resolves to the trait method being defined when the
trait is in scope, producing recursion or a type mismatch.

**The fix is not** "rename 60 inherent helpers with `_v4_inner_*`."
That makes implementation vocabulary serve a Rust resolution accident.

**Instead**, in order of preference:

1. Use explicit UFCS where the intention is "bridge to the `FsOps`
   implementation":

   ```rust
   <Devfs as FsOps>::fallocate(self, ...)
   ```

2. For genuinely shared logic, add a small private core helper:

   ```rust
   impl Devfs {
       fn fallocate_core(...) -> V3Outcome { ... }
   }

   impl FsOps for Devfs {
       fn fallocate(...) -> V3Outcome { self.fallocate_core(...) }
   }

   impl FsPageBacking for Devfs {
       fn fallocate(...) -> V3Outcome { self.fallocate_core(...) }
   }
   ```

Do **not** globally rename every helper to dodge UFCS. Only
shared-overlap methods need a core helper.

## PR-1.6 scope

1. Keep `FsOps`.
2. Declare `FsOps` v3-canonical once it returns `V3Outcome`.
3. Keep `Arc<dyn FsOps>` dispatch.
4. Keep `FsPageBacking` as page-cache-specific.
5. Use explicit UFCS where `FsPageBacking` bridges to `FsOps`.
6. Add narrow `*_core` helpers only where logic is genuinely shared.
7. Remove "delete `FsOps`" from PR-1.6 goals.
8. Rename later only if the name itself confuses reviewers.

A rename is optional. If clarity matters later:

- `FsOps` → `FsBackendOps` *or*
- `FsOps` → `VfsBackendOps`

Not in PR-1.6 unless the diff is trivial. The important thing is to
**stop calling it "v4"** just because it predates the v3 vocabulary.

## Net effect

PR-1.6 changes from a "language workaround" PR into a **doc and
naming cleanup** PR. The wave-9h-ζ blocker dissolves because the goal
that produced it was the wrong goal.
