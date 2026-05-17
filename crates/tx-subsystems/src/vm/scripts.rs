//! VM scripts that build and populate detached `Cap<AddressSpace>`s for
//! the exec path.
//!
//! These primitives are the V1 and V2 cross-doc obligations from
//! `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS` (research note labels). They
//! prepare a fully reversible address space — no live PTEs, no
//! thread-visible side effects — that the exec script's phase 6
//! atomically swaps onto a `ProcessIdentity`. Drop the returned `Cap`
//! before phase 6 to abort exec without observable footprint.
//!
//! Sibling Phase 4 (`tx-scripts/src/process/exec/loader.rs`) populates
//! the `ImagePlan` from a parsed `goblin::elf::Elf`. This module owns
//! the kernel-side translation: each `LoadSegment` lands as one or two
//! `VmEntry` recipes (file-backed prefix + optional anonymous BSS
//! tail), and `populate_detached_user_range` writes the loader-built
//! initial userspace stack image into the detached aspace via the
//! direct-map view of freshly materialised pages.
//!
//! Per `txdoc:VM-1-AUTHORITATIVE-BINDINGS-AND-MATERIALIZATIONS-IN-VM`
//! every page of the resulting aspace is recipe-only at the time the
//! caller sees the `Cap`. Faulting after the phase-6 swap drives the
//! existing `fault_script` lane (file-backed segments lazy-load
//! through `PageContainer::materialize_page`; BSS tail / stack zero
//! through the existing `PrivateAnon` recipe). V2 is *not* on the
//! demand-fault path — it is the rare case where the kernel needs to
//! pre-populate userspace bytes (the stack image) before any thread
//! has run in the new aspace.

use alloc::vec::Vec;

use step_engine::page_allocator;
use step_engine::Cap;
use tx_hal::PmapIf;

use crate::execution::Errno;
use crate::page_backed::PageContainer;
use crate::vm::adapter::step_engine::{self as step_engine, ByteProgress, StepOutcome};
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry, VmEntryFlags,
    VmFault, VmFaultError, VmMapError, VmPmapError, USER_PAGE_SIZE,
};

/// Default initial top of the userspace stack for v1 static binaries.
///
/// Per `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK` the loader
/// places argv/envp/auxv just below `stack_top`. The slice picks
/// `0x4000_0000` (1 GiB) — well clear of typical RV64 LOAD vaddrs
/// (around 0x10000) and the bootstrap brk (`0x6000_0000`). The
/// concrete number is staged here so the loader and the host smoke
/// test both reference the same constant; revisit when the per-platform
/// VA layout is finalised.
pub const USER_STACK_TOP_DEFAULT: u64 = 0x4000_0000;

/// Default initial reservation for the userspace stack region. Sized
/// at 16 KiB (4 pages) per `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`.
/// Stack growth via a future `expand_stack` script is out of scope for
/// the initial slice.
pub const USER_STACK_INITIAL_RESERVATION: u64 = 16 * 1024;

/// Loader's parsed view of the ELF image, in kernel-owned shape.
///
/// Phase 1A ships a placeholder shape large enough for V1 / V2 to be
/// implemented and tested; sibling Phase 4 (`tx-scripts/.../loader.rs`)
/// extends or refines this without touching the V1 / V2 internals.
/// Per the plan's "Cross-doc supporting edits" the goblin dependency
/// stays inside `tx-scripts`; `tx-subsystems::vm::scripts` only sees
/// kernel-side shapes.
///
/// Cites: `txdoc:EXEC-8-1-LOADER-PUBLIC-TYPES`,
///        `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`.
#[derive(Clone, Debug)]
pub struct ImagePlan {
    /// User-virtual entry point (`e_entry`).
    pub entry: u64,
    /// Top of the initial userspace stack (page-aligned).
    pub stack_top: u64,
    /// LOAD segments derived from the ELF program-header walk.
    pub load_segments: Vec<LoadSegment>,
    /// Optional BSS tail extension when a LOAD segment's `mem_size`
    /// exceeds its `file_size` and the tail extends past the
    /// page-rounded end of the file-backed prefix.
    pub bss_extension: Option<BssTail>,
    /// Whether PT_GNU_STACK with PF_X was found in the ELF.
    /// When true, the stack region gets `PROT_EXEC`.
    pub executable_stack: bool,
}

/// One LOAD segment from the ELF image.
///
/// `vaddr`/`memsz`/`filesz`/`file_offset` map onto the program-header
/// fields (`p_vaddr`, `p_memsz`, `p_filesz`, `p_offset`). `flags`
/// captures `p_flags` after the loader's W^X validation per
/// `txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION`. `backing` is the
/// kernel-side `Cap<PageContainer>` cloned from the open file's RNode
/// (`RNodeBacking::PageBacked { pc }`).
#[derive(Clone, Debug)]
pub struct LoadSegment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub file_offset: u64,
    pub flags: SegmentFlags,
    pub backing: Cap<PageContainer>,
}

/// Final-segment BSS tail: the page-rounded extent past the file-backed
/// prefix that needs an anonymous-private recipe so demand-faulting
/// produces zero-filled pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BssTail {
    pub vaddr: u64,
    pub size: u64,
}

/// Parsed `p_flags` after the loader's W^X / non-zero-prot validation.
///
/// Slice scope per `txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION`: a LOAD
/// segment with both `write == true` and `execute == true` is rejected
/// at parse time. The loader normalises `read = true` for any executable
/// or writable segment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SegmentFlags {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl SegmentFlags {
    pub const fn to_prot(self) -> Prot {
        Prot::new(self.read, self.write, self.execute)
    }
}

/// Errors from `build_aspace_from_image` and
/// `populate_detached_user_range`. Maps onto the loader's
/// `txdoc:EXEC-8-9-ERRNO-MAPPING` table; the exec script translates
/// these to the Linux ABI errnos at the syscall boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptError {
    /// Loader supplied an invalid `ImagePlan` (overlapping segments,
    /// unaligned vaddrs, zero-page reservation). Maps to ENOEXEC.
    InvalidImage,
    /// VM-side map / unmap returned an error.
    Map(VmMapError),
    /// Pmap construction failed during `AddressSpace::new`.
    Pmap(VmPmapError),
    /// Range-lock contention while populating the detached aspace.
    /// V2 callers run before any thread is in the aspace, so this
    /// should never appear in production; surface it for tests that
    /// exercise the contention path.
    WouldBlock,
    /// Out-of-bounds populate target — `vaddr` falls outside every
    /// recipe of the detached aspace.
    Efault,
    /// Anonymous-private materialisation failed (out of pages).
    OutOfMemory,
    /// Direct-map hook unavailable for a materialised PPN. Maps to
    /// EIO at the syscall boundary.
    Io,
}

impl From<VmMapError> for ScriptError {
    fn from(e: VmMapError) -> Self {
        Self::Map(e)
    }
}

impl From<VmPmapError> for ScriptError {
    fn from(e: VmPmapError) -> Self {
        Self::Pmap(e)
    }
}

/// Build a fresh detached `Cap<AddressSpace>` seeded by the image plan.
///
/// The returned `Cap` has only recipe rows installed: every page of
/// every LOAD segment is recipe-only, demand-faulting via the existing
/// `fault_script` lane after the exec script's phase 6 swap. Drop the
/// `Cap` before phase 6 to abort exec without observable side effects.
///
/// For each `LoadSegment`:
/// - File-backed prefix (length = page-rounded `filesz`):
///   `VmBacking::Page { pc: segment.backing, offset: file_page_offset }`,
///   prot from `segment.flags`.
/// - BSS tail (when `memsz > filesz` extends past the page-rounded
///   prefix end): `VmBacking::PrivateAnon`, prot from `segment.flags`.
///
/// Plus a stack `VmEntry` (anonymous private, top-down,
/// `USER_STACK_INITIAL_RESERVATION`) anchored at
/// `[stack_top - reservation, stack_top)`.
///
/// `image_plan.bss_extension` is folded into the per-segment BSS tail
/// computation; if the loader has already emitted a final-segment BSS
/// tail in `load_segments` it is used directly. The redundant
/// `bss_extension` slot is retained so sibling Phase 4 can populate it
/// without changing this signature.
///
/// Cites: `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`,
///        `txdoc:VM-1-AUTHORITATIVE-BINDINGS-AND-MATERIALIZATIONS-IN-VM`.
///
/// Caller-discipline note: the function acquires fresh epoch guards
/// internally (via the existing `reserve_map` / `commit_reserved_map`
/// lane). Per `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`, callers must
/// not hold a guard at the call site. Sibling Phase 5's `exec_script`
/// already obeys this by acquiring fresh guards inside the script's
/// step-call sites.
pub fn build_aspace_from_image<P: PmapIf>(
    image_plan: &ImagePlan,
) -> Result<Cap<AddressSpace>, ScriptError> {
    let aspace = AddressSpace::new_cap_for_platform::<P>()?;

    // Defensive: reject the trivially-bad `ImagePlan` shapes that the
    // loader's validation should have caught. The full validation
    // table lives in Part 4's parser per `txdoc:EXEC-8-5`; this is
    // belt-and-braces.
    if image_plan
        .stack_top
        .checked_sub(USER_STACK_INITIAL_RESERVATION)
        .is_none()
    {
        return Err(ScriptError::InvalidImage);
    }
    if !image_plan.stack_top.is_multiple_of(USER_PAGE_SIZE as u64) {
        return Err(ScriptError::InvalidImage);
    }

    // Register LOAD-segment recipes. Each segment yields one (file-backed)
    // recipe and optionally a second (BSS-tail private-anon) recipe.
    //
    // **BSS handling note (2026-05-08).** `register_load_segment` already
    // handles per-segment BSS extension by splitting at page boundaries:
    // file-backed range up to `file_end_rounded`, then anon range from
    // there to `mem_end_rounded`. The loader's `image_plan.bss_extension`
    // tracks the same BSS at byte granularity (raw `vaddr + filesz`,
    // not page-aligned), which we'd have to re-derive into page bounds
    // and then de-dup against the per-segment anon range. The cleaner
    // shape is to ignore the loader's bss_extension here and rely on
    // the per-segment split. This also removes a class of bugs where
    // the loader's raw BSS bounds collide with `align_range`'s
    // page-alignment requirement on real binaries (e.g. busybox's
    // seg[1] BSS at 0x10a341 fails `align_range`).
    //
    // The `bss_extension` field stays on `ImagePlan` for downstream
    // consumers (auxv, debug tooling) that care about the
    // byte-granularity range; only the redundant register-recipe pass
    // is removed.
    for segment in &image_plan.load_segments {
        register_load_segment(&aspace, segment)?;
    }

    // Stack: anonymous private, page-aligned, anchored to `stack_top`.
    let stack_start = image_plan.stack_top - USER_STACK_INITIAL_RESERVATION;
    let stack_range = align_range(stack_start, USER_STACK_INITIAL_RESERVATION)
        .ok_or(ScriptError::InvalidImage)?;
    let stack_entry = VmEntry::new(
        stack_range,
        if image_plan.executable_stack {
            Prot::new(true, true, true)
        } else {
            Prot::READ_WRITE
        },
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    register_recipe(&aspace, stack_entry)?;

    Ok(aspace)
}

/// Populate a kernel-side `bytes` slice into the detached aspace at
/// user-virtual `vaddr`.
///
/// Used by the exec script to write the initial userspace stack image
/// (argc/argv/envp/auxv as composed by Part 3's `build_initial_user_stack`)
/// into the detached aspace before phase 6's atomic swap. LOAD segments
/// are *not* on this path — they materialise lazily through
/// `fault_script` post-swap.
///
/// Walks page-rounded chunks of `bytes`. For each page:
/// 1. Looks up the covering recipe in the detached aspace.
/// 2. Materialises an anonymous private page via the existing
///    `materialize_pagebacked` lane.
/// 3. Copies bytes through `frame_kernel_addr`'s direct-map view.
/// 4. Publishes the materialised page into the aspace's pmap so the
///    first user access finds it.
///
/// Errors:
/// - `Errno::EFAULT` — `vaddr` is outside any recipe of the aspace.
/// - `Errno::EINVAL` — recipe is read-only or the `vaddr + bytes.len()`
///   arithmetic overflows.
/// - `Errno::ENOMEM` — anonymous page allocation failed.
/// - `Errno::EIO` — direct-map hook unavailable for a materialised PPN.
///
/// Cites: `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`,
///        `txdoc:VM-1-AUTHORITATIVE-BINDINGS-AND-MATERIALIZATIONS-IN-VM`.
///
/// Caller-discipline note: callers must not hold an epoch guard at the
/// call site — the function takes fresh guards internally (the
/// `aspace.lookup` / `resolve_fault` / `publish_fault_materialization`
/// lane each acquire their own). Per
/// `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` sibling Phase 5's
/// `exec_script` obeys this by acquiring fresh guards inside its
/// step-call sites; this function continues the pattern.
pub async fn populate_detached_user_range(
    aspace: &Cap<AddressSpace>,
    vaddr: u64,
    bytes: &[u8],
) -> StepOutcome<(), ByteProgress> {
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
    if bytes.is_empty() {
        return V3::done(());
    }

    // Bounds check.
    let Some(end) = (vaddr as usize).checked_add(bytes.len()) else {
        return V3::err(Errno::EINVAL.into());
    };
    let _ = end;

    let mut written = 0usize;
    let mut cursor = vaddr as usize;
    while written < bytes.len() {
        let within_page = cursor % USER_PAGE_SIZE;
        let chunk = core::cmp::min(bytes.len() - written, USER_PAGE_SIZE - within_page);

        let addr = UserVirtAddr(cursor);
        // `aspace.lookup` acquires its own epoch guard internally. We
        // intentionally do not nest a guard from `_guard` per the EBR
        // discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`); the
        // `_guard` parameter is reserved for sibling Phase 4 / 5
        // consumers that need a guard-scoped lookup later.
        let entry = match aspace.lookup(addr) {
            Some(entry) => entry,
            None => return V3::err(Errno::EFAULT.into()),
        };
        if !entry.prot.write {
            return V3::err(Errno::EINVAL.into());
        }

        // Materialise the page through the canonical fault-resolution
        // path. `aspace.fault_script` would also work, but reservation-
        // and pmap-publish-bookkeeping is heavier than this path needs:
        // we already know the recipe row is fresh (no concurrent
        // mutators of a detached aspace) and we want the kernel-side
        // direct-map view, not a pmap publish + user-side fault retry.
        // Use `resolve_fault` + `materialize_pagebacked` directly so
        // the materialised `MaterializedPage` is visible to us; then
        // publish it so the first user fault after the phase-6 swap
        // finds the mapping ready.
        let outcome = match aspace.resolve_fault(VmFault::new(addr, crate::vm::AccessMode::Write)) {
            Ok(outcome) => outcome,
            Err(VmFaultError::WouldBlock) => return V3::err(Errno::EBUSY.into()),
            Err(VmFaultError::ProtectionViolation) => return V3::err(Errno::EINVAL.into()),
            Err(VmFaultError::NoRecipe) => return V3::err(Errno::EFAULT.into()),
            Err(_) => return V3::err(Errno::EIO.into()),
        };
        let materialized = match outcome.materialize_pagebacked() {
            Ok(m) => m,
            Err(_) => return V3::err(Errno::ENOMEM.into()),
        };

        let frame_base = match page_allocator::frame_kernel_addr(materialized.page.ppn) {
            Ok(ptr) => ptr,
            Err(_) => return V3::err(Errno::EIO.into()),
        };
        // SAFETY: `frame_base` is the kernel direct-map view of a
        // freshly materialised anonymous page; we hold the pin via
        // `materialized.page.map_pin` for the duration of the copy.
        // `within_page + chunk <= USER_PAGE_SIZE` by construction.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(written),
                frame_base.add(within_page),
                chunk,
            );
        }

        // Publish the materialisation so the first userspace access
        // post-swap finds the page already mapped (no refault needed).
        // V2 is the only caller that pre-publishes in a detached
        // aspace; LOAD segments stay recipe-only and refault.
        if let Err(error) = aspace.publish_fault_materialization(outcome, materialized) {
            // `WouldBlock` cannot happen on a detached aspace (no
            // concurrent thread reservation contender); a `Pmap` /
            // `StaleRecipe` error here is a programmer error worth
            // surfacing.
            return V3::err(map_publish_error(error).into());
        }

        written += chunk;
        cursor += chunk;
    }

    V3::done(())
}

/// Register a single LOAD segment as recipes on the aspace.
///
/// Splits the segment into a file-backed prefix (the bytes the loader
/// will demand-fault from `Cap<PageContainer>`) and an optional
/// anonymous-private BSS tail (`memsz > filesz`).
fn register_load_segment(
    aspace: &Cap<AddressSpace>,
    segment: &LoadSegment,
) -> Result<(), ScriptError> {
    let prot = segment.flags.to_prot();
    let page_size = USER_PAGE_SIZE as u64;

    // ELF p_vaddr / p_offset must be congruent mod page_size per the
    // loader's validation; the page delta is the in-page offset of the
    // segment's start within the first mapped page.
    let page_delta = segment.vaddr % page_size;
    let map_start = segment.vaddr - page_delta;
    let file_page_offset = segment
        .file_offset
        .checked_sub(page_delta)
        .ok_or(ScriptError::InvalidImage)?;

    if segment.memsz == 0 {
        return Ok(());
    }

    let file_end = segment
        .vaddr
        .checked_add(segment.filesz)
        .ok_or(ScriptError::InvalidImage)?;
    let mem_end = segment
        .vaddr
        .checked_add(segment.memsz)
        .ok_or(ScriptError::InvalidImage)?;

    let file_end_rounded = round_up(file_end, page_size).ok_or(ScriptError::InvalidImage)?;
    let mem_end_rounded = round_up(mem_end, page_size).ok_or(ScriptError::InvalidImage)?;

    // Per the ELF spec, bytes in the LAST file-backed page that lie
    // beyond `vaddr + filesz` must read as zero **when the segment
    // has a BSS extension** (memsz > filesz). The previous shape
    // mapped the partial last file-backed page directly through the
    // PageContainer, which exposed whatever bytes happened to live
    // past `filesz` in the file (typically section-header-string
    // fragments for static binaries — busybox.musl crashed
    // dereferencing ".got\0ata" via gp-relative loads into BSS).
    //
    // Fix: when `memsz > filesz` and `filesz` is not page-aligned,
    // round the file-backed range DOWN to a page boundary so the
    // partial last page lands in the anon range. `exec_script` then
    // eagerly populates that page's file-content prefix from the
    // PageContainer; the rest of the page stays zero (fresh anon
    // allocation).
    //
    // For segments with `memsz == filesz` (no BSS), keep the
    // original `file_end_rounded` boundary: the partial-last-page
    // tail past `filesz` is not in any LOAD-segment vaddr range, so
    // userspace doesn't observe it, and writing through populate
    // would fail EINVAL on a non-writable segment (e.g. .text+RX).
    let has_bss_extension = mem_end > file_end;
    let file_part_end = if has_bss_extension {
        let file_end_floor = file_end & !(page_size - 1);
        file_end_floor.max(map_start)
    } else {
        file_end_rounded.min(mem_end_rounded)
    };

    if file_part_end > map_start {
        let len = file_part_end - map_start;
        let range = UserRange::new_aligned(UserVirtAddr(map_start as usize), len as usize)
            .map_err(|_| ScriptError::InvalidImage)?;
        let entry = VmEntry::new(
            range,
            prot,
            VmEntryFlags::PRIVATE,
            VmBacking::Page {
                pc: segment.backing.clone(),
                offset: file_page_offset,
            },
        );
        register_recipe(aspace, entry)?;
    }

    if mem_end_rounded > file_part_end {
        let len = mem_end_rounded - file_part_end;
        let range = UserRange::new_aligned(UserVirtAddr(file_part_end as usize), len as usize)
            .map_err(|_| ScriptError::InvalidImage)?;
        let entry = VmEntry::new(range, prot, VmEntryFlags::PRIVATE, VmBacking::PrivateAnon);
        register_recipe(aspace, entry)?;
    }

    Ok(())
}

/// Insert a fresh recipe row into the detached aspace.
///
/// Reuses the existing `reserve_map` / commit lane that `try_mmap`
/// builds on. The detached aspace has no concurrent mutators (no
/// thread is in it yet), so the reservation always succeeds in one
/// step.
fn register_recipe(aspace: &Cap<AddressSpace>, entry: VmEntry) -> Result<(), ScriptError> {
    match aspace.reserve_map(entry, MapPlacement::RequireFree) {
        crate::vm::MapReserveResult::Reserved(reservation) => {
            reservation.commit().map_err(ScriptError::Map)?;
            Ok(())
        }
        crate::vm::MapReserveResult::Blocked(_) => Err(ScriptError::WouldBlock),
        crate::vm::MapReserveResult::Err(error) => Err(ScriptError::Map(error)),
    }
}

fn round_up(value: u64, align: u64) -> Option<u64> {
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

pub fn align_range(start: u64, len: u64) -> Option<UserRange> {
    let page_size = USER_PAGE_SIZE as u64;
    if !start.is_multiple_of(page_size) {
        return None;
    }
    let aligned_len = round_up(len, page_size)?;
    if aligned_len == 0 {
        return None;
    }
    UserRange::new_aligned(
        UserVirtAddr(usize::try_from(start).ok()?),
        usize::try_from(aligned_len).ok()?,
    )
    .ok()
}

const fn map_publish_error(error: VmFaultError) -> Errno {
    match error {
        VmFaultError::WouldBlock => Errno::EBUSY,
        VmFaultError::NoRecipe | VmFaultError::StaleRecipe => Errno::EFAULT,
        VmFaultError::ProtectionViolation => Errno::EINVAL,
        VmFaultError::PageBeyondSize | VmFaultError::BackingMismatch => Errno::EFAULT,
        VmFaultError::BackingOffsetOverflow | VmFaultError::Range(_) => Errno::EINVAL,
        VmFaultError::PageCache(_) | VmFaultError::Pmap(_) => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_backed::{AnonSwapPolicy, PageContainerKind};
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vm::adapter::step_engine::StepOutcome as V3StepOutcome;
    use crate::vm::{UserPage, USER_PAGE_SIZE};
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        crate::zones::register_all().expect("kernel zones");
        match step_engine::page_allocator::claim_zero_frame() {
            Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for vm::scripts tests: {error:?}"),
        }
        // No global drain here; nested guards are forbidden so callers
        // that already hold an epoch guard would crash.
        lock
    }

    fn anon_pc(pages: u64) -> Cap<PageContainer> {
        PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            pages,
        )
        .expect("page container cap")
    }

    fn rwx_flags(read: bool, write: bool, execute: bool) -> SegmentFlags {
        SegmentFlags {
            read,
            write,
            execute,
        }
    }

    fn run_async<F: core::future::Future>(future: F) -> F::Output {
        use core::ptr::null;
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        const NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(null(), &NOOP_VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(null(), &NOOP_VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut pinned = Box::pin(future);
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("populate_detached_user_range yielded unexpectedly"),
        }
    }

    #[test]
    fn build_aspace_from_image_returns_empty_aspace_with_recipe_table() {
        let _g = setup();
        let plan = ImagePlan {
            entry: 0x1_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: Vec::new(),
            bss_extension: None,
            executable_stack: false,
        };

        let aspace =
            build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build empty aspace");

        // Only the stack range is registered.
        let recipes = aspace.recipes_snapshot();
        assert_eq!(recipes.len(), 1);
        let stack = &recipes[0];
        assert_eq!(stack.flags, VmEntryFlags::PRIVATE);
        assert_eq!(stack.prot, Prot::READ_WRITE);
        assert!(matches!(stack.backing, VmBacking::PrivateAnon));
        assert_eq!(
            stack.range.start().as_usize() as u64,
            USER_STACK_TOP_DEFAULT - USER_STACK_INITIAL_RESERVATION
        );
        assert_eq!(stack.range.end().as_usize() as u64, USER_STACK_TOP_DEFAULT);
    }

    #[test]
    fn build_aspace_from_image_emits_recipes_per_load_segment() {
        let _g = setup();
        let pc = anon_pc(8);
        let segment = LoadSegment {
            vaddr: 0x1_0000,
            memsz: 2 * USER_PAGE_SIZE as u64,
            filesz: 2 * USER_PAGE_SIZE as u64,
            file_offset: 0,
            flags: rwx_flags(true, false, true),
            backing: pc.clone(),
        };
        let plan = ImagePlan {
            entry: 0x1_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: vec![segment],
            bss_extension: None,
            executable_stack: false,
        };

        let aspace = build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build aspace");

        let recipes = aspace.recipes_snapshot();
        // 1 LOAD file-backed + 1 stack.
        assert_eq!(recipes.len(), 2);
        let load = recipes
            .iter()
            .find(|e| matches!(e.backing, VmBacking::Page { .. }))
            .expect("file-backed recipe");
        assert_eq!(load.range.start().as_usize() as u64, 0x1_0000);
        assert_eq!(
            load.range.end().as_usize() as u64,
            0x1_0000 + 2 * USER_PAGE_SIZE as u64
        );
        assert_eq!(load.prot, Prot::READ_EXECUTE);
    }

    #[test]
    fn build_aspace_from_image_emits_bss_tail_for_memsz_gt_filesz() {
        let _g = setup();
        let pc = anon_pc(8);
        let segment = LoadSegment {
            vaddr: 0x2_0000,
            memsz: 3 * USER_PAGE_SIZE as u64,
            filesz: USER_PAGE_SIZE as u64,
            file_offset: 0,
            flags: rwx_flags(true, true, false),
            backing: pc.clone(),
        };
        let plan = ImagePlan {
            entry: 0x2_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: vec![segment],
            bss_extension: None,
            executable_stack: false,
        };

        let aspace = build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build aspace");

        let recipes = aspace.recipes_snapshot();
        // file-backed prefix + BSS tail + stack.
        assert_eq!(recipes.len(), 3);
        let bss = recipes
            .iter()
            .find(|e| {
                matches!(e.backing, VmBacking::PrivateAnon)
                    && e.range.start().as_usize() as u64 == 0x2_0000 + USER_PAGE_SIZE as u64
            })
            .expect("BSS-tail recipe");
        assert_eq!(
            bss.range.end().as_usize() as u64,
            0x2_0000 + 3 * USER_PAGE_SIZE as u64
        );
    }

    #[test]
    fn populate_detached_user_range_anonymous_writes_bytes_correctly() {
        let _g = setup();
        let plan = ImagePlan {
            entry: 0x1_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: Vec::new(),
            bss_extension: None,
            executable_stack: false,
        };
        let aspace = build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build aspace");

        // Write a 200-byte payload near the stack top.
        let payload: alloc::vec::Vec<u8> = (0u8..200).collect();
        let dst = USER_STACK_TOP_DEFAULT - 4096;

        let outcome = run_async(populate_detached_user_range(&aspace, dst, &payload));
        assert_eq!(outcome, V3StepOutcome::Done(()));

        // Verify the page now resolves through pmap (publication
        // succeeded) — the pmap snapshot reports the mapping for the
        // populated page.
        let page = UserPage((dst as usize) / USER_PAGE_SIZE);
        assert!(aspace.pmap().lookup(page).is_some());
    }

    #[test]
    fn populate_detached_user_range_efault_outside_recipes() {
        let _g = setup();
        let plan = ImagePlan {
            entry: 0x1_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: Vec::new(),
            bss_extension: None,
            executable_stack: false,
        };
        let aspace = build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build aspace");

        // Bytes at 0x100 are not covered by any recipe (only the stack
        // range near `USER_STACK_TOP_DEFAULT` is registered).
        let bytes = [0x55u8; 16];
        let outcome = run_async(populate_detached_user_range(&aspace, 0x100, &bytes));
        assert_eq!(outcome, V3StepOutcome::Err(Errno::EFAULT.into()));
    }

    #[test]
    fn populate_detached_user_range_round_trip_reads_via_direct_map() {
        let _g = setup();
        let plan = ImagePlan {
            entry: 0x1_0000,
            stack_top: USER_STACK_TOP_DEFAULT,
            load_segments: Vec::new(),
            bss_extension: None,
            executable_stack: false,
        };
        let aspace = build_aspace_from_image::<crate::vm::TestPmap>(&plan).expect("build aspace");

        let payload: Vec<u8> = (0..(USER_PAGE_SIZE * 2 + 17))
            .map(|i| (i & 0xff) as u8)
            .collect();
        let dst = USER_STACK_TOP_DEFAULT - (USER_STACK_INITIAL_RESERVATION);
        let outcome = run_async(populate_detached_user_range(&aspace, dst, &payload));
        assert_eq!(outcome, V3StepOutcome::Done(()));

        // Pmap-look-up the first populated page and verify the bytes
        // through the kernel direct map.
        let page = UserPage((dst as usize) / USER_PAGE_SIZE);
        let snapshot = aspace.pmap().lookup(page).expect("pmap mapping");
        let frame_base = page_allocator::frame_kernel_addr(snapshot.ppn).expect("direct map");
        let observed: &[u8] = unsafe { core::slice::from_raw_parts(frame_base, USER_PAGE_SIZE) };
        assert_eq!(&observed[..USER_PAGE_SIZE], &payload[..USER_PAGE_SIZE]);
    }
}
