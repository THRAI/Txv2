# PC / VM CoW implementation plan (revised)

Date: 2026-05-12
Status: planned (revision 2)
Supersedes: `2026-05-12-d15-vm-private-cow-plan.md`
Revision note: revision 1 placed `private_pages` on `AddressSpace`
keyed by absolute `UserPage` with eager-copy fork. **Both choices were
wrong as a final form.** Private state belongs to mapping identity
(`VmEntry`), keyed by offset-relative `VmPageOff`, with explicit
`Exclusive` / `SharedCow` state to support true lazy CoW. The
interim hotfix is not to be merged; this revision targets the final
architecture directly.

Grounded in `docs/design/03_memory-vm/VM_v1_2.md`
(`txdoc:VM-1-2-PUBLICATION-RULE`, `txdoc:VM-5-1-FAULT-HANDLER`,
`txdoc:VM-5-3-MUNMAP`, `txdoc:VM-5-6-FORK`,
`txdoc:VM-7-2-WRITE-FAULT-ON-MAP-PRIVATE`,
`txdoc:VM-7-3-PRIVATE-FRAMES-HAVE-NO-BACK-INDEX`) and
`docs/design/03_memory-vm/PAGE_BACKED_v1.md`
(`txdoc:PAGE-BACKED-3-1-STRUCTURE`,
`txdoc:PAGE-BACKED-5-1-MATERIALIZE`,
`txdoc:PAGE-BACKED-5-2-WRITE`,
`txdoc:PAGE-BACKED-7-2-COW-ON-WRITE`,
`txdoc:PAGE-BACKED-7-4-REFLINK-IS-NOT-A-PC-LEVEL-OPERATION`,
`txdoc:PAGE-BACKED-10-3-MAP-PRIVATE`).

## Ownership model

```text
AddressSpace : recipe coordination + range lock + pmap
VmEntry      : owns VM-private divergent pages (authoritative)
PageContainer: owns shared backing pages (authoritative)
Pmap         : owns derived PTE materialization (derived)
```

Two independent CoW paths. **They do not bridge.**

| Path | Owner | Authoritative state | Derived materialization |
|------|-------|--------------------|-------------------------|
| VM-private CoW (`MAP_PRIVATE`, fork CoW, `/dev/zero MAP_PRIVATE`) | `VmEntry` | `VmEntry.private` (per-mapping `PrivatePageSet`, keyed by `VmPageOff`) | pmap PTEs / TLB |
| PC shared-frame CoW (reflink) | FS + `PageContainer` | `PageContainer.pages` radix | pmap PTEs / TLB |

Hard rules:

1. No stacked PageContainers.
2. MAP_PRIVATE writes **never** install back into the source PC radix.
3. VM-private divergent pages belong to the mapping identity — keyed
   by `VmPageOff`, not absolute `UserPage`.
4. PC-level CoW is only for PC entries with `cache_ref > 1` due to
   reflink. FS code is the orchestrator; PC participates via
   `install_if_match`.
5. PTEs are always derived; install must revalidate the VmEntry recipe
   **and** the source identity (private frame or PC entry) before
   publishing.
6. `VmEntry.private` is **not a cache**. After a private write, its
   contents are unrecoverable from any other source. Losing it loses
   user-visible memory.

## Data model

### Types

```rust
// crates/tx-subsystems/src/vm/structure/private.rs (new)

pub struct VmPageOff(pub u64);            // page-offset within a VmEntry's range

pub struct PrivateFrame {
    pub frame: Cap<Frame>,
    pub state: PrivateFrameState,
}

pub enum PrivateFrameState {
    /// This mapping owns the frame exclusively; writable PTE OK.
    Exclusive,
    /// Frame is read-aliased between fork-related mappings.
    /// First writer on either side must allocate a new frame.
    SharedCow,
}

pub struct PrivatePageSet {
    pages: SpinMutex<BTreeMap<VmPageOff, PrivateFrame>>,
}

impl PrivatePageSet {
    pub fn lookup(&self, off: VmPageOff) -> Option<PrivateFrameSnapshot>;

    /// Linearization point for write-fault publication.
    pub fn install_if_absent(&self, off: VmPageOff, frame: PrivateFrame)
        -> Result<PrivateFrameSnapshot, PrivateFrame>;

    /// Linearization point for SharedCow → Exclusive transitions.
    /// `expected_identity` is the (frame Cap key, state) the caller copied from.
    pub fn replace_if_match(
        &self,
        off: VmPageOff,
        expected: PrivateFrameIdentity,
        new: PrivateFrame,
    ) -> Result<PrivateFrameSnapshot, ReplaceError>;

    /// Drop entries whose offsets fall in `range`. Used by munmap /
    /// mremap-shrink / MAP_FIXED-replace / exec.
    pub fn drain_range(&self, range: VmPageRange);

    /// Fork: mark all entries `SharedCow` and return a sibling set
    /// holding the same frames (cache_ref bumped per frame).
    pub fn fork_share(&self) -> Cap<PrivatePageSet>;

    /// VmEntry split: produce a fresh set holding only entries whose
    /// offsets fall in `target_range` (offsets rebased to the new
    /// VmEntry's range start).
    pub fn split(&self, target_range: VmPageRange, rebase_delta: u64)
        -> Cap<PrivatePageSet>;
}
```

### VmEntry attachment

```rust
pub struct VmEntry {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBacking,
    pub ufd_registration: Option<UfdRegistration>,
    /// Per-mapping private CoW page set. `None` until first private
    /// write fault populates it. Refcounted so VmEntry stays cheap
    /// to clone through the persistent recipe tree.
    pub private: Option<Cap<PrivatePageSet>>,
}
```

`Cap<PrivatePageSet>` clone is one atomic increment, so VmEntry stays
cheap-to-clone for the EBR persistent tree. Several VmEntry instances
(historical readers under epoch guard) may point at the same
`PrivatePageSet` — that's fine because interior mutability is behind
the `SpinMutex`, and re-published successors observe the same
authoritative state.

VmEntry `Eq`/`PartialEq` ignore `private` identity (compare by Cap
key, not deep). VmEntry's `split_for_*` clones `private` via
`PrivatePageSet::split` so each sub-entry owns the entries that fall
in its sub-range.

## Fault-path semantics

### Write fault on a private mapping

```text
1. Acquire Materializer reservation on the page range.
2. Re-observe recipe → VmEntry. Abort/retry if changed.
3. let off = vme.page_offset_of(fault_page);
4. Match vme.private.and_then(|p| p.lookup(off)):
   - Some(Exclusive { frame }):
       install_pte_if_recipe_matches(write=true, frame).
       done.
   - Some(SharedCow { frame: A }):
       new = frame_alloc_zeroed();
       copy_frame(new, A);
       set = vme.private_or_init();
       match set.replace_if_match(off, identity(A, SharedCow),
                                  PrivateFrame{new, Exclusive}):
         - Committed { winner: new }:
             install_pte_if_recipe_matches(write=true, new).
         - Lost { current }:
             drop(new); retry from step 4 with `current`.
   - None:
       src = materialize_from_backing(vme.backing, off, Read)?;
              // For PrivateAnon: zero frame.
              // For Page(pc): pc.materialize_anon(off, Read).
       new = frame_alloc_zeroed();
       copy_frame(new, src);
       set = vme.private_or_init();
       match set.install_if_absent(off, PrivateFrame{new, Exclusive}):
         - Committed { winner }:
             install_pte_if_recipe_matches(write=true, winner.frame).
         - Lost { current }:
             drop(new); retry from step 4 with `current`.
```

### Read fault on a private mapping

```text
1. Acquire Materializer reservation.
2. Re-observe recipe → VmEntry.
3. let off = vme.page_offset_of(fault_page);
4. If let Some(private) = vme.private.and_then(|p| p.lookup(off)):
     install_pte_if_recipe_matches(write=false, private.frame).
     // Same PTE perms regardless of Exclusive/SharedCow — RO is safe
     // and matches semantics (a read does not consume CoW credit).
     done.
5. Else: materialize_from_backing(vme.backing, off, Read)?;
     install_pte_if_recipe_matches(write=false, backing.frame).
```

This fixes the classic failure mode:
> private page exists, but fault path ignores it, falls back to
> backing PC / zero, user observes stale or zero data.

### PTE publication revalidation

`install_pte_if_recipe_matches` is the explicit linearization gate.
After any release/reacquire cycle (async I/O, lock yield) the install
path must:

```text
- re-observe the recipe at fault_addr;
- assert the observed VmEntry identity matches the one we copied from;
- assert the source frame's identity matches what we selected
  (PrivatePageSet entry still maps `off` to the same frame, OR the
  PC radix entry at `off` still maps to the same frame);
- only then call pmap.install(...) which bumps map_count / acquires MapPin.
```

Recipe is authoritative; selected source identity is the secondary
TOCTOU guard; PTE is the published derived result.

## Fork semantics

```text
fork_aspace(parent):
  acquire ExclusiveWriter on parent (full user range)
  clone recipes from parent into child
  for each parent VmEntry vme:
    if vme.private.is_some():
      // Mark all existing parent private frames SharedCow;
      // produce a sibling set for the child.
      child_set = vme.private.fork_share();
        // - bumps each frame's cache_ref by 1
        // - mutates parent.private entries' state to SharedCow
      child.vme.private = Some(child_set)
    // For private VmEntries with no private set yet (no prior write):
    //   no fork-time action; child's first read materializes from
    //   backing, child's first write CoWs into a fresh private entry.
  downgrade parent PTEs covering private SharedCow frames to read-only
  shootdown demoted parent PTEs and wait
  child has no PTEs yet — lazy install on fault
  release ExclusiveWriter
```

Key properties:

- **Lazy.** No frame is copied at fork time. Only frames the parent
  has already privately written carry a cache_ref bump.
- **Symmetric.** Parent and child both see `SharedCow`. Either side's
  first write triggers the CoW. Parent's writable PTEs are demoted at
  fork; child has none until first fault.
- **MAP_PRIVATE-not-yet-written pages cost zero.** They are not in
  parent's `PrivatePageSet`; child inherits the recipe, materializes
  from PC on its own demand, and writes CoW into its own
  `PrivatePageSet`. No fork-time work.

This is **true CoW**, replacing the eager-copy approximation in
revision 1 of the plan.

## Recipe mutations

| Operation | Action on `VmEntry.private` |
|-----------|------------------------------|
| `munmap(range)` | Drop affected VmEntries; their `Cap<PrivatePageSet>` drops (decrementing each frame's `cache_ref`). For partial overlap, split survivor inherits sliced `PrivatePageSet` via `split`. |
| `mprotect(range, prot)` | Split target sub-entry inherits sliced `PrivatePageSet` via `split`; PTE prot updates derive from new recipe. Private contents preserved. |
| `mremap` (move) | New VmEntry inherits the existing `Cap<PrivatePageSet>` (offsets are mapping-relative; no rekey needed). Old PTEs torn down; new PTEs install lazily. |
| `mremap` (resize) | Sliced PrivatePageSet for new range; entries outside dropped via `drain_range`. |
| `MAP_FIXED` replace | Withdraw old recipe in replaced range → `PrivatePageSet` drops with it. New recipe has `private: None`. |
| `exec` | Discard the recipe tree → all `PrivatePageSets` drop. |
| `fork` | See "Fork semantics" above. |

In every case the `Cap<PrivatePageSet>` lifetime is tied to the
VmEntry's recipe lifetime: as soon as the last recipe instance
referencing it is retired through EBR, the set drops and its frames
release their cache_refs.

## PC-level CoW (reflink) — already in tree

`crates/tx-subsystems/src/page_backed/reflink.rs` already implements:

- `install_shared_page(pc, page, source_ppn)` — install a shared
  entry, bumping source frame's `cache_ref`.
- `cow_replace_into_private(pc, page)` — CoW-replace a `cache_ref > 1`
  entry in one PC via `install_if_match`. Source PC untouched.

This stays **strictly separate** from the VM-private path. It only
fires on a `MAP_SHARED` write to a PC entry whose frame is shared with
another PC (via reflink). VM-private writes do not touch it.

E2E coverage lands alongside the `copy_file_range` / `FICLONE`
syscall surface, not as part of this slice.

## Phase plan

### Phase A — Final-shape VM-private CoW

**A1. Define `PrivatePageSet`, `PrivateFrame`, `PrivateFrameState`,
`VmPageOff`.** New file
`crates/tx-subsystems/src/vm/structure/private.rs`. Implement
`lookup` / `install_if_absent` / `replace_if_match` / `drain_range` /
`fork_share` / `split` with `SpinMutex<BTreeMap<...>>` interior. Cap
allocation via the standard `Zone<PrivatePageSet>` pattern.

**A2. Add `private: Option<Cap<PrivatePageSet>>` to `VmEntry`.**
Update `VmEntry::new` (default `None`), `with_*` builders,
`sub_entry` (call `PrivatePageSet::split` with rebase delta when
slicing). Audit every `derive(Clone, Debug, Eq, PartialEq)` site — Eq
must compare Cap key only (not deep contents).

**A3. Revert revision-1 artefacts.**
- Remove `AddressSpace.private_pages` and the
  `crate::vm::structure::address_space::PrivateFrame` shape.
- Revert the eager-copy block in `fork_aspace`
  ([execution.rs:108-149](../../../crates/tx-subsystems/src/vm/execution.rs))
  back to recipe-only clone before A4 reintroduces fork CoW work in
  the final shape.

**A4. Rewrite `fork_aspace` to lazy share-RO CoW.**
[execution.rs:93](../../../crates/tx-subsystems/src/vm/execution.rs):
- Clone recipes.
- For each parent private VmEntry with `private.is_some()`:
  call `child.vme.private = Some(parent.vme.private.fork_share())`.
- Walk parent's pmap for those VmEntries and downgrade writable PTEs
  to read-only (new `VmPmap::demote_writable_in_range`); shootdown
  via existing batch.
- Do not pre-install PTEs in the child.

**A5. Wire the fault path to consult `VmEntry.private`.**
[types.rs:572](../../../crates/tx-subsystems/src/vm/structure/types.rs)
and [types.rs:629](../../../crates/tx-subsystems/src/vm/structure/types.rs):
- New unified entrypoint that takes `&VmEntry` (already on the
  outcome) and resolves Read vs Write per the semantics in
  "Fault-path semantics" above.
- Materialize result still flows through
  `VmFaultMaterialization` + `publish_page_with_replacement`.
- The `install_if_recipe_matches` revalidation belongs in
  `publish_page_with_replacement` — assert (a) recipe-at-fault-addr
  still points at the same VmEntry identity, (b) selected source
  frame identity unchanged.

**A6. Hook recipe mutations.** For each of `munmap`, `mprotect`,
`mremap`, `MAP_FIXED`, `exec`, ensure the right `PrivatePageSet`
slice / drop happens. Most are automatic via VmEntry Cap drop; mremap
move needs to preserve the Cap across the new entry.

**A7. Verify.**
- `cargo check -p tx-subsystems` clean.
- New tests (under `vm/tests/private_cow.rs`):
  1. MAP_PRIVATE file write doesn't propagate to fd.
  2. MAP_PRIVATE file refault after write still sees private bytes.
  3. fork private stack: child first read sees parent bytes; writes
     diverge.
  4. parent refault after fork: parent sees its own (unchanged or
     post-write) bytes.
  5. munmap+mmap same VA: old private gone.
  6. mremap move: private follows mapping.
  7. mprotect: private preserved across permission change.
  8. concurrent write race: exactly one CoW winner per (entry, off).
  9. MAP_SHARED reflink write: PC-level CoW path fires, VM-private
     path does not.
  10. PTE stale-source: concurrent munmap aborts an in-flight
      install; no stale PTE survives.
- `cargo xtask shell-test --target rv64-qemu --script
  tools/shell-tests/busybox-prompt.txt` walks `true / echo / pwd /
  ls / / echo pipe-ok | cat / true && echo done`.

### Phase B — Retire diagnostics

Once Phase A is green:
- Remove `FAULT_SIGSEGV_{ADDR,ACCESS,HITS,PID}` from
  `tx_kernel::lib`.
- Remove `SYS_CLONE_PARENT_PC_{ENTRY,EXIT}` from `proc.rs` and the
  dump in `exec.rs`.
- Keep `trap-trace` feature.

### Phase C — PC reflink E2E (deferred)

Land with `copy_file_range` / `FICLONE` syscall surface; PC-level
infra already exists.

## Verification gates

- `cargo check -p tx-subsystems` after A1-A3.
- `cargo test -p tx-subsystems vm::` after A4-A6.
- `cargo test -p tx-subsystems vm::tests::private_cow` after A7.
- `cargo xtask shell-test --target rv64-qemu --script
  tools/shell-tests/busybox-prompt.txt` end-to-end.
- `cargo xtask progress validate` if any plan/handoff JSON touched.

## Why this is the final shape (not a hotfix)

- **Mapping identity vs VA.** `UserPage` on `AddressSpace` is
  fragile under munmap/mmap-same-VA, MAP_FIXED, mremap, splits,
  merges. `VmPageOff` on `VmEntry` survives all of them by tracking
  the recipe identity, not its current placement.
- **Authoritative, not cache.** Private contents cannot be
  reconstructed from any other source. The data model must reflect
  that.
- **True lazy CoW.** Eager copy violates fork's defining performance
  property and inflates memory footprint linearly with mapped pages.
- **Read-path correctness.** Without `Exclusive`/`SharedCow` and a
  read-path consult, post-fork-pre-write reads of an
  already-written-by-parent page would observe backing-PC or zero
  bytes instead of parent's private contents.
- **Concurrency linearization.** `install_if_absent` /
  `replace_if_match` on `PrivatePageSet` give the same CAS
  publication discipline as `install_if_match` on PC radix — two
  writers cannot both win, no refcount leaks.
- **Recipe mutation hygiene.** Tying private state to recipe
  identity means recipe operations (split/merge/withdraw/move)
  carry private state correctly by construction.

## Next step

A1 — define the `PrivatePageSet` module shape and its
`install_if_absent` / `replace_if_match` / `fork_share` / `split`
linearization helpers. Backing data structure is
`SpinMutex<BTreeMap<VmPageOff, PrivateFrame>>` to start; can migrate
to `PersistentRadix` later for read-fault concurrency without
changing the consumer surface.
