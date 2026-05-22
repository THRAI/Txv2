//! VM StepOp adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
//!
//! Each `*Op` struct wraps one VM mutation operation (mmap, munmap,
//! mprotect, mremap, brk, msync) and implements [`StepOp`]. The
//! `step()` method encapsulates a single attempt:
//!
//! - `Done(output)` — operation completed.
//! - `Yield { OnWaitSource { source, interests } }` — RangeLock
//!   blocked; the caller should await the range-lock release channel
//!   and retry.
//! - `Err(errno)` — terminal error (translated from [`VmMapError`]).
//!
//! The free `try_*` methods on [`AddressSpace`] remain the source of
//! truth; these wrappers are the canonical non-async entrypoint for
//! reactors and shim layers that drive via `tx_scripts::drive`.

use crate::vm::adapter::step_engine::{
    self, Errno, InterestMask, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    WaitSourceId, YieldShape,
};
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmRemapOutcome, VmRemapRequest,
    RANGE_LOCK_RELEASE_MASK,
};

/// Translate a [`VmMapError`] into a substrate [`Errno`] for
/// [`StepOutcome::Err`].
fn vmmap_error_to_errno(error: VmMapError) -> Errno {
    match error {
        VmMapError::AlreadyMapped => Errno::EEXIST,
        VmMapError::InvalidRange => Errno::EINVAL,
        VmMapError::MissingMapping => Errno::EINVAL,
        VmMapError::NoFreeRange => Errno::ENOMEM,
        VmMapError::WouldBlock => Errno::EAGAIN,
        VmMapError::BackingOffsetOverflow => Errno::EINVAL,
        VmMapError::Pmap(_) => Errno::EIO,
        VmMapError::Private(_) => Errno::ENOMEM,
    }
}

/// Build the canonical RangeLock wait-source Yield for WouldBlock.
fn range_lock_blocked<O>(aspace: &AddressSpace) -> StepOutcome<O, NoProgress> {
    let source = WaitSourceId::new(aspace.range_lock().wait_source_id());
    let interests = InterestMask::new(RANGE_LOCK_RELEASE_MASK);
    StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource { source, interests },
    }
}

// ---------------------------------------------------------------------------
// VmMapOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::try_mmap`].
///
/// On RangeLock conflict returns
/// `Yield { OnWaitSource { range-lock-release } }` so the reactor parks
/// and retries.
pub struct VmMapOp<'a> {
    pub aspace: &'a AddressSpace,
    pub request: VmMapRequest,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmMapOp<'a> {
    type Output = VmMapOutcome;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_mmap(self.request.clone()) {
            Ok(outcome) => StepOutcome::Done(outcome),
            Err(VmMapError::WouldBlock) => {
                let source = WaitSourceId::new(self.aspace.range_lock().wait_source_id());
                let interests = InterestMask::new(RANGE_LOCK_RELEASE_MASK);
                StepOutcome::Yield {
                    progress: NoProgress,
                    shape: YieldShape::OnWaitSource { source, interests },
                }
            }
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// VmUnmapOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::try_munmap`].
pub struct VmUnmapOp<'a> {
    pub aspace: &'a AddressSpace,
    pub range: UserRange,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmUnmapOp<'a> {
    type Output = VmMapCommit;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_munmap(self.range) {
            Ok(commit) => StepOutcome::Done(commit),
            Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// VmProtectOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::try_mprotect`].
pub struct VmProtectOp<'a> {
    pub aspace: &'a AddressSpace,
    pub range: UserRange,
    pub prot: Prot,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmProtectOp<'a> {
    type Output = VmMapCommit;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_mprotect(self.range, self.prot) {
            Ok(commit) => StepOutcome::Done(commit),
            Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// VmRemapOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::try_mremap`].
pub struct VmRemapOp<'a> {
    pub aspace: &'a AddressSpace,
    pub request: VmRemapRequest,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmRemapOp<'a> {
    type Output = VmRemapOutcome;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_mremap(self.request) {
            Ok(outcome) => StepOutcome::Done(outcome),
            Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// VmBrkOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of the brk script.
///
/// Brk is a composite operation that may internally call mmap_script
/// (for grow) or munmap_script (for shrink). Those sub-operations also
/// need to yield on RangeLock conflicts, so this StepOp implements
/// its own retry loop with an inner state machine.
///
/// The state machine transitions:
///   Init -> try one-shot brk
///   GrowBlocked / ShrinkBlocked -> retry the sub-op after yield
///   Done / Err -> terminal
pub struct VmBrkOp<'a> {
    pub aspace: &'a AddressSpace,
    pub brk_base: UserVirtAddr,
    pub current_brk: UserVirtAddr,
    pub requested_brk: UserVirtAddr,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmBrkOp<'a> {
    type Output = UserVirtAddr;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        use crate::vm::UserRange;

        if self.requested_brk.0 < self.brk_base.0 {
            return StepOutcome::Err(Errno::EINVAL);
        }
        if self.requested_brk.0 == self.current_brk.0 {
            return StepOutcome::Done(self.current_brk);
        }

        if self.requested_brk.0 > self.current_brk.0 {
            // Grow: mmap the new pages.
            let Some(old_committed) =
                crate::vm::execution::checked_page_align_up(self.current_brk.0)
            else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            let Some(new_committed) =
                crate::vm::execution::checked_page_align_up(self.requested_brk.0)
            else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            if new_committed <= old_committed {
                return StepOutcome::Done(self.requested_brk);
            }
            let range = match UserRange::new_aligned(
                UserVirtAddr(old_committed),
                new_committed - old_committed,
            ) {
                Ok(r) => r,
                Err(_) => return StepOutcome::Err(Errno::EINVAL),
            };
            let request = VmMapRequest::fixed(
                range,
                MapPlacement::RequireFree,
                Prot::READ_WRITE,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            );
            match self.aspace.try_mmap(request) {
                Ok(_outcome) => StepOutcome::Done(self.requested_brk),
                Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
                Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
            }
        } else {
            // Shrink: munmap the excess pages.
            let Some(old_committed) =
                crate::vm::execution::checked_page_align_up(self.current_brk.0)
            else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            let Some(new_committed) =
                crate::vm::execution::checked_page_align_up(self.requested_brk.0)
            else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            if new_committed >= old_committed {
                return StepOutcome::Done(self.requested_brk);
            }
            let range = match UserRange::new_aligned(
                UserVirtAddr(new_committed),
                old_committed - new_committed,
            ) {
                Ok(r) => r,
                Err(_) => return StepOutcome::Err(Errno::EINVAL),
            };
            match self.aspace.try_munmap(range) {
                Ok(_commit) => StepOutcome::Done(self.requested_brk),
                Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
                Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VmMlockOp / VmMunlockOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::try_mlock`] with `locked = true`.
pub struct VmMlockOp<'a> {
    pub aspace: &'a AddressSpace,
    pub range: UserRange,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmMlockOp<'a> {
    type Output = VmMapCommit;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_mlock(self.range, true) {
            Ok(commit) => StepOutcome::Done(commit),
            Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

/// `StepOp` wrap of [`AddressSpace::try_mlock`] with `locked = false`.
pub struct VmMunlockOp<'a> {
    pub aspace: &'a AddressSpace,
    pub range: UserRange,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmMunlockOp<'a> {
    type Output = VmMapCommit;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.aspace.try_mlock(self.range, false) {
            Ok(commit) => StepOutcome::Done(commit),
            Err(VmMapError::WouldBlock) => range_lock_blocked(self.aspace),
            Err(error) => StepOutcome::Err(vmmap_error_to_errno(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// VmMsyncOp
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`AddressSpace::msync`].
///
/// `AddressSpace::msync` returns `StepOutcome<(), PageProgress>` because
/// it may call `step_fsync` internally on file-backed pages. This wrapper
/// converts `PageProgress` to `NoProgress`, mapping `Continue { progress }`
/// to `Continue { progress: NoProgress }` (discarding the intermediate
/// progress value, which is acceptable because msync's progress is
/// boolean: "more pages to sync").
pub struct VmMsyncOp<'a> {
    pub aspace: &'a AddressSpace,
    pub range: UserRange,
}

impl<'a, I: SubjectIdentity> StepOp<I> for VmMsyncOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        match self.aspace.msync(self.range, &guard) {
            V3::Done(()) => V3::Done(()),
            V3::Err(e) => V3::Err(e),
            V3::Continue { .. } => V3::Continue {
                progress: NoProgress,
            },
            V3::Yield { shape, .. } => V3::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}
