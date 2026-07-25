# Chapter 11 — Projection: how VM state becomes `/proc/<pid>/maps`

Every chapter so far has been about *changing* the VM — mapping, faulting,
forking. This one is about *observing* it from the outside, safely. When a
debugger reads `/proc/1234/maps`, or `pmap` prints a process's address space, or
the OOM killer sizes a victim, they are reading a **projection** of the
authoritative VM state — a read-only view that must not leak a capability, must
not keep anything alive, and must not block a single VM operation. This is
`fs/proc/task_mmu.c` in Linux. In txKernel it is `vm/project.rs` feeding procfs.

It is also the cleanest demonstration of the address-as-coordinate boundary
(Chapter 2, Motivation 5): a projection hands out *facts about* mappings — start,
end, permissions — never *authority over* them.

## The projection home

```rust
// vm/project.rs:1
//! Read-only VM projections.
//!
//! Procfs/sysfs adapters should consume projection helpers from this module
//! rather than becoming owners of AddressSpace structure. Projection rows are
//! non-authoritative views: they expose stable VM facts without leaking caps or
//! granting backend authority.
```

The module exists so that `/proc` code never touches `AddressSpace` internals. It
offers exactly two functions and three plain-data types:

```rust
// vm/project.rs:13
pub struct AddressSpaceProjection {
    pub stats: AddressSpaceStats,
    pub mappings: Vec<VmMappingProjection>,
}
// vm/project.rs:19
pub struct VmMappingProjection {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBackingProjection,     // None | PrivateAnon | Page { offset }
}

// vm/project.rs:33
pub fn project_address_space(aspace: &AddressSpace) -> AddressSpaceProjection {
    AddressSpaceProjection {
        stats: aspace.stats(),
        mappings: aspace.recipes_snapshot().into_iter()
            .map(|entry| VmMappingProjection {
                range: entry.range, prot: entry.prot, flags: entry.flags,
                backing: project_backing(entry.backing_kind()),   // strips the cap!
            })
            .collect(),
    }
}
```

The key line is `project_backing(entry.backing_kind())`. A live `VmEntry` holds
`owners` caps — `VmCap<PageContainer>`, `VmCap<PrivatePageSet>` (Chapter 2). The
projection deliberately reduces the backing to `VmBackingProjection`, which
carries *only the offset for the `Page` case and nothing else*
(`project.rs:27`). The page container cap, the private set cap — none of it
survives into the projection. A consumer learns "this range is page-backed at
offset N"; it cannot reach the page cache, cannot pin a frame, cannot grant
anyone authority. The projection is facts, stripped of capabilities.

This is the same `Projected` mechanism the VFS series describes (`Projected
{ schema, key }` RNode backing): procfs files are not stored bytes, they are
*synthesized on read* from a projection of authoritative state. The VM is one of
the subsystems those synthetic files project.

## Reads go direct, and not through `checks`

Notice what `project_address_space` calls: `aspace.recipes_snapshot()`
(`address_space.rs:124`) — the lock-free, epoch-guarded snapshot from Chapter 3 —
and `aspace.stats()` for the cheap counters. It does **not** route through
`vm::checks`. That is deliberate and worth stating, because it is a common point
of confusion: the `require_*` predicates in `checks.rs` (Chapter 7) are
*mutation-gating* checks — they observe the binding to decide whether a fault or
map may proceed. Projection is not gating anything; it is a pure read. So it takes
the recipes snapshot directly, runs concurrently with every VM operation (the
snapshot is consistent under its epoch guard), and blocks nothing.

And it does **not** trust `stats` for the per-mapping data. `AddressSpaceStats`
(`recipe_count`, `vm_size`) is the non-authoritative, possibly-lagging cell from
Chapter 1 — fine for a rough `VmSize` number, wrong for the maps listing. So the
maps output is built from a *fresh recipe snapshot*, not from the cached stats.
Even observability re-reads the truth.

## Each maps line is one recipe

procfs turns the projection into the textual files. `render_maps`
(`tx-fs/src/procfs/read.rs:201`) is the core:

```rust
// tx-fs/src/procfs/read.rs:201  (condensed)
fn render_maps(pid: Pid) -> String {
    let mut entries = aspace.recipes_snapshot();      // ← the same snapshot
    entries.sort_by_key(|e| e.range.start());
    for entry in entries {
        // start-end  rwxp  offset  device  inode  [backing label]
        let perms = [entry.prot.read?'r':'-', entry.prot.write?'w':'-',
                     entry.prot.execute?'x':'-', entry.flags.shared?'s':'p'];
        let (offset, label) = match entry.backing_kind() {
            VmEntryBacking::PrivateAnon => (0, "[anon]"),
            VmEntryBacking::Page { offset } => (offset, /* resolve container kind */),
            VmEntryBacking::None => (0, "[none]"),
        };
        // format the line ...
    }
}
```

The thing to internalize: **one line of `/proc/<pid>/maps` is exactly one
recipe.** There is no separate VMA-vs-page-table reconciliation, no walking the
hardware tables — the recipes *are* the VMAs (Chapter 3), so projecting them is
the maps file. The `rwxp` column is the recipe's `Prot` plus the `shared` flag;
the offset is the `VmEntryBacking::Page` offset; the label is derived from the
`PageContainerKind` (a file path, `[anon]`, a device). Because coalescing
(Chapter 3) keeps adjacent compatible recipes merged, the maps file stays as
compact as Linux's.

`render_smaps` (`read.rs:260`) is the same snapshot with per-mapping detail
blocks. `render_status` (`read.rs:388`) produces `/proc/<pid>/status`: it sums the
`flags.locked` recipes for the `VmLck:` field (so the `mlock` flag from Chapter 10
becomes observable here), alongside `VmSize`/`VmData`. All three read the same
authoritative recipe snapshot; none mutates anything; none can block a fault.

## Why the cap-stripping matters

It would be easy — and wrong — to hand procfs a reference into the live
`AddressSpace` and let it read fields directly. That would couple the observer to
the structure, risk it pinning a `PageContainer` alive past the mapping, and blur
the boundary the whole subsystem defends. The projection's discipline — *snapshot
the recipes, strip the caps, return plain data* — keeps observation a strictly
lower-privilege operation than mutation. A `/proc` reader can see the shape of an
address space in full detail and still hold no power over it. That is the
identity/payload split applied to observation: you can look at the binding without
being given the payload.

## Where we go from here

You have now seen the VM from every side: the three structures, the split at
three levels, the recipes, the pmap, the hardware, the lock, the fault handler,
both backings, the inbound syscalls, and the outbound projection. Chapter 12 runs
one heap page through the entire stack — `mmap` to read fault to write fault to
`fork` to CoW break to `munmap` to frame-free to `exec` teardown — and watches all
three levels of the split move together, peeking at `/proc/self/maps` mid-flight.

## Source anchors

- Projection module: `crates/tx-subsystems/src/vm/project.rs:1`
- `AddressSpaceProjection` / `VmMappingProjection` / `VmBackingProjection`: same file, `:13, 19, 27`
- `project_address_space` (cap-stripping): same file, `:33`; `project_backing` `:53`
- `recipes_snapshot` / `stats` on AddressSpace: `crates/tx-subsystems/src/vm/structure/address_space.rs:124, 105`
- `render_maps` / `render_smaps` / `render_status`: `crates/tx-fs/src/procfs/read.rs:201, 260, 388`
- `AddressSpaceStats` (non-authoritative): `crates/tx-subsystems/src/vm/structure/types.rs:843`
- `vm::checks` are mutation-gating, not projection: `crates/tx-subsystems/src/vm/checks.rs` (Chapter 7)
- `Projected` RNode backing (the procfs mechanism): `docs/tutorials/vfs/03_fsops-and-backends.md`, `.../08_special-files-and-fd-zoo.md`
