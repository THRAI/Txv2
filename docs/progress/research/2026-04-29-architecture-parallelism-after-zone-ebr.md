---
date: 2026-04-29
topic: "Architecture parallelism after Zone/EBR"
status: complete
---

# Research: Architecture Parallelism After Zone/EBR

## Question

How much can txKernel implementation split according to the overall
architecture? Can semantic subsystems be implemented in parallel once Zone and
EBR are up?

## Conclusion

Yes, mostly. Zone/EBR is the main semantic-entity unlock, but the full
parallelism gate is wider: upper subsystems need the common substrate surface
named by the architecture, not only raw zone allocation.

The practical unlock is:

- `epoch::guard`
- `zone::reserve` / `zone::sign`
- `Weak<T>` / `IdentRef<'g, T>` / `Cap<T>` / `PayloadCap<T>`
- index reservation/commit
- observer-safe mutation primitives
- credit reservations for services
- RawQueue / RawPort / RawTrace bus carriers
- enough reactor/waker shell for waitable steps

Once those are stable, subsystem core work can fan out by owner: VM, Process,
ThreadRuntime, VFS, Mount, PageBacked, TTY, service subsystems, and filesystem
instances. Cross-subsystem scripts such as exec, fork/clone integration,
file I/O, first userspace, and full signal delivery remain integration gates.

## Coarse Roadmap Width

The current long-term `kernel_main` plan has a coarse maximum width of three
independent milestone nodes:

```text
level 0: baseline
level 1: coreinit
level 2: zone/epoch/bus foundation | trap shell
level 3: percpu/SMP gates | VM core | VFS/device/TTY init
level 4: user access | reactor/scheduler | pmap/shootdown
level 5: page-backed | process/thread runtime | SMP runtime
level 6: syscall/AST | exec loader
level 7: first userspace
level 8: runtime tests
```

This is the coarse graph. A finer graph can increase useful width inside the
semantic subsystem layers, but the first-userspace critical path still narrows
around VM, Process/ThreadRuntime, PageBacked, VFS/Mount, syscall dispatch, and
exec.

## Split Strategy

Use architecture homes as ownership boundaries:

- Foundation/HAL and pmap/trap work: platform and generic HAL scopes.
- Substrate: zone, epoch, index, mutation, credit, bus, pmap-facing helpers.
- Reactor/scheduler: task table, wait-adapt, wakers, idle, policy.
- Full semantic subsystems: each owns `structure/`, `checks/`, `execution/`,
  and `project.rs`.
- Services: cred, rlimit, time/trace/random-style service state.
- Filesystem instances: tmpfs, procfs, devfs, devpts, bdev-fs, tx-ext4.
- Scripts: cross-subsystem syscall sequencing; these integrate later.

Within each full subsystem, the best parallel split is:

1. `structure/` entity and index declarations.
2. `checks/` predicates, require functions, witnesses.
3. `execution/` step functions.
4. `project.rs` projections.
5. subsystem tests and conformance examples.

`structure/` plus type manifests should land before execution steps in the
same subsystem. `checks/` and `project.rs` can often proceed in parallel once
structure types stabilize.

## Likely Parallel Waves

### Wave A: Before Semantic Fanout

Low width: 1-3 workers.

- CoreInit spine.
- Zone/epoch/index/mutation/bus substrate.
- Trap shell and per-hart gates.

These define the interfaces everyone else consumes. Treat them as critical-path
work, with parallel read-only analysis and tests around them.

### Wave B: First Semantic Fanout

Medium-high width: 4-7 workers if write scopes are clean.

- VM AddressSpace / recipes / RangeLock.
- VFS DEntry/RNode/OpenFile core and walker checks.
- Mount identity/payload/namespace and mountpoint index.
- Process entity topology and frame slots.
- ThreadRuntime entity shape and payload state placement.
- Cred and rlimit service cores.
- Static device/devfs skeleton.

Some of these cannot become fully runnable yet, but their entity shapes,
manifests, structure/checks modules, and unit tests can proceed once the common
object model and substrate API are stable.

### Wave C: Runtime and Content Integration

Medium width: 3-5 workers.

- Reactor/scheduler.
- PageBacked and reclaim.
- TTY core, initially split from job-control integration.
- pmap/shootdown completion.
- user-access/fixups.
- filesystem-instance skeletons once `FsOps` / `FsPageBacking` are stable.

### Wave D: Cross-Subsystem Scripts

Low-medium width: 2-3 workers plus reviewers.

- syscall dispatch and AST loop.
- exec loader and detached AddressSpace construction.
- fork/clone/exit/wait integration.
- file I/O scripts spanning VFS/PageBacked/VM.
- signal delivery shim over Process/ThreadRuntime/Reactor/Trap.

These are narrower because they encode ordering, point-of-no-return placement,
and cross-owner contracts.

### Wave E: First Userspace and Runtime Tests

Low width: 1-2 workers.

- init process spawn.
- return-to-userspace loop.
- BusyBox / OSComp runtime sentinels.
- final integration and QEMU smoke/runtime verification.

## Practical Limit

Read-only design/research can run at 8+ lanes. Implementation should usually
start at 3-5 lanes after the common substrate lands, then narrow to 1-3 lanes
for integration scripts and runtime boot. The runner should split work by
architectural home and write scope, not by syscall name.

## Blockers

- Zone/epoch/index/mutation/bus substrate is not implemented yet.
- Reactor/task table is not implemented yet.
- Trap shell/user-return path is not implemented yet.
- VM AddressSpace/RangeLock is not implemented yet.

These blockers limit executable subsystem work, but do not prevent parallel
readiness audits, module skeleton plans, or structure/checks design work.

## Verification

Read active architecture docs and the long-term `kernel_main` plan. Used a
small topological levelization of the plan JSON to confirm coarse milestone
width. No code behavior changed.
