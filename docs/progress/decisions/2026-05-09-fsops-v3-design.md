# Decision: FsOps v3 trait migration via parallel `FsOps`

**Date:** 2026-05-09

## Decision

- The `FsOps` filesystem-backend trait migrates to v3 step outcomes via a **parallel `FsOps` trait** declared alongside the existing `FsOps` in `crates/tx-subsystems/src/vfs/execution.rs`. Each of the eight `impl FsOps for X` blocks across the workspace grows a sibling `impl FsOps for X` block; existing v4 callers stay on `FsOps` unchanged, and v3 callers opt into `FsOps` per call site. A final wholesale cascade replaces `FsOps` with `FsOps` once every impl + caller is dual-routed.
- Wave 8 (this slice) declares the trait and lands one prototype impl (`LifecycleFs`) + five inline tests. Wave 9 fans out to the remaining seven impls. The same pattern applies to the sibling `FsPageBacking` trait, designed in this doc but **not** prototyped here — `FsPageBacking` lands first in wave 9 because it shares mount-payload coupling with `FsOps` (the walker dual-routes through `MountPayload { fs_ops, fs_page_backing }`).

## Trait shape

```rust
pub trait FsOps: Send + Sync + 'static {
    fn lookup(&self, parent, name, guard)
        -> step_v3::StepOutcome<FsObjectId, NoProgress>;
    fn load_inode_meta(&self, fs_object_id, guard)
        -> step_v3::StepOutcome<InodeMeta, NoProgress>;
    fn serialize_inode_meta(&self, fs_object_id, meta, guard)
        -> step_v3::StepOutcome<(), NoProgress>;
    fn create_inode(&self, parent, name, mode, cred, guard)
        -> step_v3::StepOutcome<(FsObjectId, InodeMeta), NoProgress>;
    fn unlink/rename/link/rmdir(...)
        -> step_v3::StepOutcome<(), NoProgress>;
    fn mkdir/symlink(...)
        -> step_v3::StepOutcome<(FsObjectId, InodeMeta), NoProgress>;
    fn readdir(&self, fs_object_id, cursor, guard)
        -> step_v3::StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress>;
    fn destroy_inode(&self, fs_object_id, guard)
        -> step_v3::StepOutcome<(), NoProgress>;
    // Defaulted to ENOSYS, parity with FsOps:
    fn read_link(&self, fs_object_id, guard)
        -> step_v3::StepOutcome<Box<[u8]>, NoProgress> { ... }
    fn materialise_rnode(&self, fs_object_id, meta, guard)
        -> step_v3::StepOutcome<Cap<RNode>, NoProgress> { ... }
    fn step_chmod(&self, fs_object_id, new_mode, cred, guard)
        -> step_v3::StepOutcome<(), NoProgress> { ... }
    fn step_chown(&self, fs_object_id, new_uid, new_gid, cred, guard)
        -> step_v3::StepOutcome<(), NoProgress> { ... }
}
```

All 13 methods carry `NoProgress`. The Rust signature lives in `crates/tx-subsystems/src/vfs/execution.rs` after the existing `FsOps` block.

## Per-method progress-type table

| Method | Progress | Rationale |
|---|---|---|
| `lookup` | `NoProgress` | One-shot identity-side query: parent + name → child id. No accumulator. |
| `load_inode_meta` | `NoProgress` | Point load. |
| `serialize_inode_meta` | `NoProgress` | Point store. |
| `create_inode` | `NoProgress` | Single inode allocation; the FS may internally do partial work but the trait surface is one inode. |
| `unlink`/`link`/`rmdir`/`rename` | `NoProgress` | Single dentry mutation. |
| `mkdir`/`symlink` | `NoProgress` | Single inode + dentry. |
| `readdir` | `NoProgress` | One entry per call. The cursor is a method **input**, not progress; multi-entry composition lives at the caller (re-call with returned cursor). If a future `readdir_batch` lands carrying multiple entries per call, it would carry `EntryProgress`. |
| `destroy_inode` | `NoProgress` | Single inode tear-down. |
| `read_link` | `NoProgress` | Returns full target bytes in one call. |
| `materialise_rnode` | `NoProgress` | Returns one `Cap<RNode>`. |
| `step_chmod`/`step_chown` | `NoProgress` | Per-inode metadata mutation. |

`PageProgress` does **not** appear on `FsOps`. The page-counting accumulators live one trait over, on `FsPageBacking{,V3}`, where ops like `flush_page` genuinely move pages and the caller (e.g. `step_fsync_v3`) tallies pages across a loop.

## Context

- Wave-7's `step_truncate_v3`/`step_fsync_v3` cascade probe (free-fn step migration) closed cleanly: free fns can pick a per-call-site `Advanced(t) → Continue/Yield+progress` mapping with full caller knowledge. Wave-6's W-mount probe surfaced the inverse: `mount.rs` has zero free-fn step targets — its outcome surface IS `FsOps`/`FsPageBacking`. Trait migration is a **different** problem from free-fn migration.
- A trait method has no caller context. The `Advanced(t)` mapping (`Continue { progress }` vs `Yield { progress, … }` vs `Done(t)`) is per-call-site ambiguous when applied at the trait boundary, because:
  - Different impls may treat `Advanced` differently (an FS that streams out bytes vs an FS that fakes "made progress" as a hint).
  - Different callers may want the same impl behaviour mapped differently (a tight `step_fsync` loop counts `Advanced` as one page; a one-shot mount doesn't care).
- Mechanical translation of every `StepOutcome<T>` to a v3 outcome at the trait surface forces the impls (not the callers) to commit to a `Progress` type per method. `NoProgress` for the entire `FsOps` surface is the right answer because none of the 13 methods aggregate sub-operations within one call.

## Consequences

- **Wave 9 fan-out (seven impls):** `Tmpfs`, `Devfs`, `Ext4FsInstance`, `DevptsInstance`, `TestFs`, `LifecycleFs` (done in wave 8), `ExecTestFs`, `ExecveTestFs` each grow a sibling `impl FsOps for X` block next to their existing `impl FsOps for X`. Most are mechanical: the `LifecycleFs` prototype is a pure error-and-`Done(())` skeleton (no progress decisions to make), so wave 9's risk is concentrated in the impls that do real work — `Tmpfs` and `Ext4FsInstance`. Recommendation: do `Tmpfs` + one test fixture (`TestFs`) first as a learning sub-wave, then fan out the remaining five in parallel.
- **`FsPageBacking` design (wave 9 first slice):** the sibling page-backing trait has 5 methods (`fetch_page`, `flush_page`, `truncate`, `fsync`, `fallocate`). Same `NoProgress` answer for all five — the trait surface is per-page or per-fs-internal-op, not aggregating. Page-count accounting stays at the caller (e.g. `step_fsync_v3` already tracks `pages_so_far`). `MountOutput` already holds `Arc<dyn FsPageBacking>` separately from `Arc<dyn FsOps>`, so the two traits migrate independently — no joint trait-object coupling.
- **Walker dual-dispatch:** `walker.rs:247` types `current_fs_ops: Option<Arc<dyn FsOps>>` and routes lookups/materialisations through it. The cleanest path is **new walker entry points using `FsOps`** rather than an adapter (`&dyn FsOps` → `&dyn FsOps`). An adapter would re-introduce the per-call-site `Advanced` ambiguity at the adapter layer, which is exactly the problem `FsOps` exists to dissolve. The walker grows `step_walk` / `step_open` siblings (free-fn cascade pattern from wave 4/6/7); when every impl dual-routes, walker callers swap, then `step_walk` deletes alongside `FsOps`.
- **Final cascade still required:** the parallel-trait approach defers the wholesale rename, it doesn't eliminate it. Once every impl + every caller is on `FsOps`, a final wave deletes `FsOps`, renames `FsOps → FsOps`, and the workspace is single-trait again. This wave is large but mechanical (no logic decisions left).
- **Trait-object lifetime contracts unchanged:** both traits are `Send + Sync + 'static`. `Arc<dyn FsOps>` and `Arc<dyn FsOps>` coexist without trouble in `MountPayload` — wave 9 grows a sibling `fs_ops: Arc<dyn FsOps>` on `MountPayload`, populated by the same backend whose `mount_root` constructor returns both.

## Cross-trait coupling found

- `MountOutput { fs_ops, fs_page_backing }` couples both traits at construction time. Each backend (`tmpfs::mount_root`, `ext4::mount_root`, …) builds both objects from the same internal state. Wave 9 needs to ship both `FsOps` and `FsPageBacking` together so backends can dual-route in one PR rather than having to add `fs_ops` first and `fs_page_backing` second across all eight backends.
- `materialise_rnode` returns `Cap<RNode>` whose `RNodeBacking::PageBacked { pc }` variant references a `PageContainer` whose `mount` field already references the `MountPayload` (which holds the page-backing trait object). No type-level v3↔v4 leak — the cap is opaque at the trait boundary.
- The walker calls `fs_ops.lookup(...)` and `mount_payload.fs_page_backing.fetch_page(...)` independently per step. No method on `FsOps` consumes a type produced by `FsPageBacking` or vice versa. The two trait migrations are **decoupled at the surface** even though both backends are built together.

## Alternatives Considered

- **(a) Default-method shim on `FsOps`** — add `fn lookup_v3(...) -> step_v3::StepOutcome<...>` with a default impl that calls `lookup` and converts the v4 outcome. Rejected: v4 `Advanced(t)` is per-call-site ambiguous (Continue vs Done vs Yield), and the default body has no caller context. Every impl that returns `Advanced(t)` from any method would have to override the v3 default, which defeats the point of a default. Worse, the conversion direction is backwards: v4-impl-first defeats the "v3 is ground truth" disposition the migration plan commits to.
- **(b) Wrapper free fns** — `lookup_v3<F: FsOps>(fs: &F, ...) -> step_v3::StepOutcome<...>` that call the trait method and convert the outcome. Same `Advanced` ambiguity as (a) at the wrapper layer. Additionally, free wrappers can't be installed via `Arc<dyn FsOps>`-shaped storage in `MountPayload`, so callers would have to thread a different shape through. Strictly worse than (a).
- **(c) Flip `FsOps` in place** — change the return types directly; cascade to all 8 impls + walker callers in one PR. Considered in the v3-migration plan §4 as the "wholesale" strategy. Rejected for this wave: the blast radius is ~50 method bodies across 8 impls + ~20 walker call sites + every trait-object construction site. With wave-7's lessons fresh, the right move is to validate the trait shape on one impl + one caller (this wave 8) and then either do the wholesale flip OR continue with parallel until coverage hits 100%. Parallel trait keeps both options open — wholesale flip in (c) does not.

## Open Questions

- **`fetch_page` returns a `Frame` — is `PageProgress` the right v3 type, or `NoProgress`?** Current design picks `NoProgress` because the trait surface is "fetch one page" (the caller asked for one specific page; partial progress within a single page fetch is meaningless). But page-cache lifecycle ops drive a loop over pages from outside, so the per-call accumulator is genuinely empty. Confirmed `NoProgress` for wave 9 unless an impl surfaces a counter-example.
- **Should `readdir_batch` (multi-entry per call) land at the same time as `FsOps`?** Adding it later means a second trait migration; adding it now means designing a `EntryProgress`-carrying method without a caller. Defer to wave 9: ship the 13-method `FsOps` mirror first, add `readdir_batch` only when there's a caller that benefits.
- **Should the wholesale-cascade wave delete `FsOps` and rename `FsOps → FsOps`, or keep the `V3` suffix as the new spelling?** Code-style guess: drop the suffix. The `_v3` naming is a migration scaffold; once the migration completes the kernel is single-version again.
- **Walker call-site `dyn FsOps` → `dyn FsOps` cutover order:** does the walker grow `step_walk` first (freezing `step_walk` against `FsOps`), or do the impls grow `FsOps` first (so `step_walk` has impls to test against)? Recommended order: impls first (wave 9a), walker entry points second (wave 9b). The wave-8 prototype validates the impl side; wave 9a is the impl fan-out; wave 9b adds `step_walk` once impl coverage is 8/8.

## Verification

- `cargo build --workspace --tests` — clean.
- `cargo test -p tx-subsystems --lib fsopsv3 -- --test-threads=1` — 5/5 passed.
- Wave-baseline test count: `1287 → 1292` (+5 new v3 outcome-shape tests on `LifecycleFs`).
