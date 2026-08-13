# Chapter 10 — VM system calls: the tx-shims seam

Everything so far has been the VM subsystem proper (`tx-subsystems::vm`). But a
user program does not call `fault_script` or `try_mmap`; it executes the `mmap`
instruction-trap with six raw register arguments. Something has to decode those
arguments, translate Linux's flag bits into txKernel's typed vocabulary, run the
right VM operation — possibly blocking — and map the result back to a Linux errno.
That seam lives in a *different crate*, `tx-shims`, and it has a consistent shape
worth learning once.

This is the syscall table → `do_mmap` layer in Linux. Here it is
`tx-shims/src/linux_syscall/vm.rs`.

## Where the syscalls live

Every VM memory-management call has a handler in
`tx-shims/src/linux_syscall/vm.rs`:

```
sys_brk        :107      sys_mincore    :763      sys_mlock      :562
sys_mmap       :223      sys_mprotect   :823      sys_munlock    :610
sys_munmap     :504      sys_mremap     :894      sys_mlockall   :677
sys_madvise   :1008      sys_msync     :1060      sys_mlock2     :754
```

`userfaultfd` and its `UFFDIO_*` ioctls live next door in
`linux_syscall/userfaultfd.rs`. Dispatch from the raw syscall number happens in
`linux_syscall/mod.rs`: a fast **hot lane** `dispatch_vm_hot` (`mod.rs:792`)
inlines the three hottest calls (`NR_MMAP`, `NR_MUNMAP`, `NR_MPROTECT`) ahead of
the general match, with the full table following (`brk` at `:1005`, the rest
around `:1129`).

## The two-layer pattern: decode → fast path → drive async → errno

Read one handler and you have read them all. `mprotect` is the cleanest:

```rust
// tx-shims/src/linux_syscall/vm.rs:823  (condensed)
pub(super) async fn sys_mprotect(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    // (1) DECODE the raw register arguments
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];
    if length_in == 0 { return SyscallResult::Error(EINVAL_VALUE); }
    let length = length_in.checked_next_multiple_of(USER_PAGE_SIZE)
        .ok_or(EINVAL)?;
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // (2) TRANSLATE Linux flag bits → txKernel's typed Prot
    if prot_bits & !PROT_RECOGNISED != 0 { return Error(EINVAL); }   // reject unknown bits
    let prot = Prot::new(prot_bits & PROT_READ  != 0,
                         prot_bits & PROT_WRITE != 0,
                         prot_bits & PROT_EXEC  != 0);
    let range = UserRange::new_aligned(UserVirtAddr::new(addr as usize), length)?;

    // (3) FAST PATH: try the synchronous attempt first
    match ctx.aspace.try_mprotect(range, prot) {
        Ok(_) => return SyscallResult::Return(0),                    // done, no yield needed
        Err(VmMapError::WouldBlock) => {}                            // contended → fall through
        Err(error) => return SyscallResult::Error(vmmap_error_to_i32(error)),
    }

    // (4) SLOW PATH: drive the async script to completion through the reactor
    let op = VmProtectOp { aspace: &ctx.aspace, range, prot };
    let mut script_ctx = build_subject_script_ctx(ctx);
    match drive(op, &mut script_ctx, DriveMode::Waiting, /* mailbox, registry, timer */ ..).await {
        Ok(_commit) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}
```

Four steps, every VM syscall:

1. **Decode** the `[u64; 6]` register array into typed locals, with the POSIX
   validation (zero length, alignment, recognized flags). This is the boundary
   where untyped user input is checked before it reaches the subsystem.
2. **Translate** Linux's bit conventions into txKernel's typed vocabulary —
   `PROT_*` → `Prot`, `MAP_*` → `VmEntryFlags`/`MapPlacement`, the address+length
   → a `UserRange`. After this step there are no raw bits; the subsystem only ever
   sees `Prot`, `UserRange`, `VmMapRequest`. This is Chapter 2's Motivation 5 at
   the boundary: the address is a coordinate, validated and typed before it means
   anything.
3. **Fast path.** Call the synchronous `try_*` form. Most VM operations do not
   actually need to block — the `RangeLock` is usually uncontended and there is no
   I/O — so `try_mprotect` returns `Ok` and the syscall is done with no reactor
   round-trip. A real error returns immediately, mapped to errno. Only
   `WouldBlock` (the `RangeLock` was contended) falls through.
4. **Slow path.** Wrap the operation in its step-op (`VmProtectOp`, `VmMapOp`,
   `VmUnmapOp`, …, from `vm/step_ops.rs`) and `drive` it through the reactor in
   `DriveMode::Waiting` — the same async machinery the fault handler uses (the
   reactor-and-threads series covers `drive`). On completion, map `Ok`/`Err` to
   the syscall result.

The fast-path/slow-path split is why the common case is cheap: the elaborate
async-retry-with-reservation-drop story from Chapters 6–7 only engages under
actual contention. `vmmap_error_to_i32` and `Errno` conversion are the single
errno-mapping chokepoint, so the subsystem speaks `VmMapError`/`VmFaultError`
semantics and the shim alone knows the Linux numbers.

## The gallery

The other handlers are variations on the same skeleton, each calling the matching
operation from Chapters 6–9:

- **`mmap`** (`:223`) is the most involved decode: resolve the target range
  (`MAP_FIXED` → a fixed `MapPlacement::FixedReplace`; otherwise `find_free_range`
  → `RequireFree`), resolve the backing (`MAP_ANONYMOUS` → `PrivateAnon`;
  otherwise resolve the `fd` to a `Cap<PageContainer>`), build a `VmMapRequest`,
  then the same fast/slow drive. Returns the chosen address.
- **`munmap`** (`:504`) → `VmUnmapOp` → recipe withdraw + `teardown_range`.
- **`mremap`** (`:894`) → `VmRemapOp`, using the `RangeLock` pair-acquire for a
  move.
- **`brk`** (`:107`) is modeled as anonymous `mmap`/`munmap` of the heap delta —
  grow maps `[old, new)`, shrink unmaps `[new, old)`.
- **`madvise`** (`:1008`) — `MADV_DONTNEED`/`MADV_FREE` is a range-scoped pmap
  teardown that *keeps the recipe* (a mini-munmap at the materialization layer
  only); most other advice is a no-op under no-swap.
- **`msync`** (`:1060`) → the `msync` writeback step from Chapter 9.
- **`mincore`** (`:763`) → `pmap.walk_range`, reporting per-page residency; pure
  observation, no reservation.
- **`mlock`/`mlock2`/`mlockall`** (`:562`/`:754`/`:677`) — under no-swap, pages
  are already pinned, so these set the recipe's `locked` flag for observability
  (`/proc` will show it, Chapter 11) and otherwise succeed without kernel work.

## exec and ELF loading

`exec` is where an address space is *built from scratch* rather than edited. The
loader (Process-side) produces an `ImagePlan` and hands it to the VM:

```rust
// vm/scripts.rs:212
pub fn build_aspace_from_image<P: PmapIf>(image_plan: &ImagePlan, ...) -> Result<...>;

// vm/scripts.rs:77 / :102
pub struct ImagePlan  { entry, stack_top, load_segments: Vec<LoadSegment>, bss_extension, ... }
pub struct LoadSegment{ vaddr, memsz, filesz, file_offset, flags, backing: Cap<PageContainer> }
```

`build_aspace_from_image` installs one recipe per ELF `PT_LOAD` segment —
file-backed (`VmBacking::Page` onto the executable's `PageContainer`) for the
file portion, with an anonymous (`PrivateAnon`) tail for any BSS where `memsz >
filesz` — plus a stack region anchored at `USER_STACK_TOP_DEFAULT` (`scripts.rs:52`)
with an `USER_STACK_INITIAL_RESERVATION` (`:63`) reservation. Note it installs
*recipes*, not pages: the new program demand-faults its code and data in on first
touch, exactly the lazy materialization of Chapter 7. The initial stack image
(argv/envp/auxv) is written by `populate_detached_user_range` (`scripts.rs:341`),
which faults and copies into the not-yet-active address space page by page,
acquiring a fresh guard per page per the cross-async-wait discipline.

The teardown side, `exec`'s replacement of the old image, is `exec_aspace`
(`execution.rs`, after `fork_aspace`): it tears down every PTE in the old address
space. Its caller invariant is that exec's prologue already reduced the thread
group to one, so there are no concurrent VM operations — the teardown takes an
uncontended `ExclusiveWriter` over the whole range.

## Two advanced consumers

- **Page gifting (`vmsplice`).** `gift_user_pages_step` (`vm/gift.rs:228`) moves
  whole user pages into a pipe without copying, by *transferring frame ownership*.
  `classify_user_gift_page` (`gift.rs:371`) decides eligibility: a private
  anonymous page becomes `UserPageGiftFreeze::DetachedPrivate` (removed from the
  `PrivatePageSet` via `take_if_match`, Chapter 8); a private file page becomes
  `DemotedCow`. Ineligible pages fall back to a byte copy. The gifted page keeps a
  retained-frame pin so it stays live in transit, and the user's writable PTE is
  revoked so a later write refaults onto a fresh CoW copy — the binding/
  materialization split letting a page change owners safely.
- **userfaultfd.** The syscall (`userfaultfd.rs:137`) creates a `UserfaultFd` cap;
  `UFFDIO_REGISTER` tags VMAs with the `ufd_registration` the fault handler reads
  (Chapter 7); `UFFDIO_COPY`/`UFFDIO_ZEROPAGE` supply pages for outstanding
  faults. This is the user-space-paging surface (live migration, lazy restore)
  built entirely on top of the fault handler's one dispatch branch.

## Where we go from here

You have the full inbound path: a trap with six registers becomes a decoded,
typed, validated operation, tried synchronously and driven asynchronously only
when contended, with one errno-mapping seam — plus the exec/gift/uffd surfaces on
top. Chapter 11 turns the other way: the *outbound* path, how the VM's
authoritative state is projected read-only into `/proc/<pid>/maps` without leaking
a single capability.

## Source anchors

- VM syscall handlers: `crates/tx-shims/src/linux_syscall/vm.rs` (`sys_mmap:223`, `sys_munmap:504`, `sys_mprotect:823`, `sys_mremap:894`, `sys_brk:107`, `sys_madvise:1008`, `sys_msync:1060`, `sys_mincore:763`, `sys_mlock*:562/677/754`)
- Dispatch hot lane: `crates/tx-shims/src/linux_syscall/mod.rs:792`; full table `:1005, 1129`
- Errno mapping (`vmmap_error_to_i32`): `crates/tx-shims/src/linux_syscall/vm.rs`
- Step-ops (`VmMapOp`/`VmProtectOp`/…): `crates/tx-subsystems/src/vm/step_ops.rs`
- exec/ELF (`build_aspace_from_image`, `ImagePlan`, `LoadSegment`): `crates/tx-subsystems/src/vm/scripts.rs:212, 77, 102`; stack consts `:52, 63`; `populate_detached_user_range:341`
- Page gifting (`gift_user_pages_step`, `classify_user_gift_page`): `crates/tx-subsystems/src/vm/gift.rs:228, 371`
- userfaultfd syscall + ioctls: `crates/tx-shims/src/linux_syscall/userfaultfd.rs:137`
- `drive` / `DriveMode`: reactor-and-threads tutorial; `tx-scripts`
