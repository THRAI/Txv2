//! VM-owned user-page gift value vocabulary.
//!
//! This module defines VM-owned value, eligibility, and one-shot execution
//! surface for `vmsplice(SPLICE_F_GIFT)`.

use alloc::vec::Vec;

use tx_hal::Ppn;

use crate::execution::WaitToken;
use crate::page_backed::PageContainerKind;
use crate::vm::adapter::step_engine::page_allocator::{
    self, AllocError, BitmapPageAllocator, GiftPin,
};
use crate::vm::adapter::step_engine::{self, Cap, Errno, NoProgress, StepOutcome};

use super::{
    AccessMode, AddressSpace, LockMode, PrivateFrameIdentity, PrivateFrameState, PrivatePageError,
    UserRange, UserVirtAddr, VmEntry, VmEntryBacking, VmFault, VmFaultError,
    VmFaultMaterializationStep, VmPmapError, USER_PAGE_SIZE,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPageGiftFreeze {
    DetachedPrivate,
    DemotedCow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserGiftFallbackReason {
    RangeOutsideEntry,
    NotFullPage,
    SharedMapping,
    MissingWritePermission,
    MissingBacking,
    DeviceMapping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserGiftEligibility {
    Giftable { freeze: UserPageGiftFreeze },
    CopyFallback(UserGiftFallbackReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserGiftIovError {
    ZeroLength,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPageGiftError {
    Iov(UserGiftIovError),
    Fault(VmFaultError),
    Blocked(WaitToken),
    Private(PrivatePageError),
    Alloc(AllocError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserGiftIovPlan {
    total_bytes: usize,
    gift_range: Option<UserRange>,
    copy_prefix_bytes: usize,
    copy_suffix_bytes: usize,
}

impl UserGiftIovPlan {
    pub fn new(start: UserVirtAddr, len: usize) -> Result<Self, UserGiftIovError> {
        if len == 0 {
            return Err(UserGiftIovError::ZeroLength);
        }
        let end = start
            .as_usize()
            .checked_add(len)
            .ok_or(UserGiftIovError::Overflow)?;
        let gift_start = align_up(start.as_usize(), USER_PAGE_SIZE)
            .ok_or(UserGiftIovError::Overflow)?
            .min(end);
        let gift_end = align_down(end, USER_PAGE_SIZE);
        let (gift_range, copy_prefix_bytes, copy_suffix_bytes) = if gift_start < gift_end {
            let gift_range =
                UserRange::new_aligned(UserVirtAddr(gift_start), gift_end - gift_start)
                    .map_err(|_| UserGiftIovError::Overflow)?;
            let copy_prefix_bytes = gift_start.saturating_sub(start.as_usize()).min(len);
            let copy_suffix_bytes = end.saturating_sub(gift_end).min(len - copy_prefix_bytes);
            (Some(gift_range), copy_prefix_bytes, copy_suffix_bytes)
        } else {
            (None, len, 0)
        };
        Ok(Self {
            total_bytes: len,
            gift_range,
            copy_prefix_bytes,
            copy_suffix_bytes,
        })
    }

    pub const fn total_bytes(self) -> usize {
        self.total_bytes
    }

    pub const fn gift_range(self) -> Option<UserRange> {
        self.gift_range
    }

    pub const fn copy_prefix_bytes(self) -> usize {
        self.copy_prefix_bytes
    }

    pub const fn copy_suffix_bytes(self) -> usize {
        self.copy_suffix_bytes
    }

    pub const fn copy_bytes(self) -> usize {
        self.copy_prefix_bytes + self.copy_suffix_bytes
    }
}

#[derive(Clone, Debug)]
pub struct UserPageGiftSource {
    aspace: Cap<AddressSpace>,
    range: UserRange,
}

impl UserPageGiftSource {
    pub fn new(aspace: Cap<AddressSpace>, range: UserRange) -> Self {
        Self { aspace, range }
    }

    pub const fn aspace(&self) -> &Cap<AddressSpace> {
        &self.aspace
    }

    pub const fn range(&self) -> UserRange {
        self.range
    }
}

#[derive(Debug)]
pub struct UserPageGift {
    ppn: Ppn,
    source: UserPageGiftSource,
    #[allow(dead_code)]
    pin: GiftPin<'static, BitmapPageAllocator<'static>>,
    freeze: UserPageGiftFreeze,
}

// UserPageGift is linear transfer evidence for an already-frozen user page.
// Moving the token between pipe descriptors across harts moves only ownership
// of the retained-frame contribution; it does not grant shared mutable access
// to VM recipe state or frame metadata.
unsafe impl Send for UserPageGift {}
unsafe impl Sync for UserPageGift {}

impl UserPageGift {
    pub fn new_for_vm(
        ppn: Ppn,
        source: UserPageGiftSource,
        pin: GiftPin<'static, BitmapPageAllocator<'static>>,
        freeze: UserPageGiftFreeze,
    ) -> Self {
        debug_assert_eq!(pin.ppn(), ppn);
        Self {
            ppn,
            source,
            pin,
            freeze,
        }
    }

    pub const fn ppn(&self) -> Ppn {
        self.ppn
    }

    pub const fn source(&self) -> &UserPageGiftSource {
        &self.source
    }

    pub const fn range(&self) -> UserRange {
        self.source.range()
    }

    pub const fn len(&self) -> usize {
        self.source.range().len()
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub const fn freeze(&self) -> UserPageGiftFreeze {
        self.freeze
    }
}

#[derive(Debug)]
pub struct GiftBatch {
    gifts: Vec<UserPageGift>,
    bytes: usize,
}

impl GiftBatch {
    pub fn new(gifts: Vec<UserPageGift>, bytes: usize) -> Self {
        Self { gifts, bytes }
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn gift_count(&self) -> usize {
        self.gifts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes == 0 && self.gifts.is_empty()
    }

    pub fn into_gifts(self) -> Vec<UserPageGift> {
        self.gifts
    }
}

pub type UserPageGiftStep = StepOutcome<GiftBatch, NoProgress>;

impl AddressSpace {
    pub fn gift_user_pages_step(
        &self,
        source_aspace: Cap<AddressSpace>,
        range: UserRange,
    ) -> UserPageGiftStep {
        let plan = match UserGiftIovPlan::new(range.start(), range.len()) {
            Ok(plan) => plan,
            Err(error) => {
                return StepOutcome::Err(gift_error_to_errno(UserPageGiftError::Iov(error)));
            }
        };
        let Some(gift_range) = plan.gift_range() else {
            return StepOutcome::Done(GiftBatch::new(Vec::new(), 0));
        };
        if gift_range.page_count() != 1 {
            return StepOutcome::Done(GiftBatch::new(Vec::new(), 0));
        }

        let _guard = match self
            .range_lock()
            .acquire_step(gift_range, LockMode::ExclusiveWriter)
        {
            StepOutcome::Done(guard) => guard,
            StepOutcome::Yield { .. } => {
                return crate::vm::notification::range_lock_blocked(
                    self.range_lock().release_endpoint(),
                );
            }
            _ => unreachable!("RangeLock acquire_step only returns Done or Yield"),
        };

        let mut gifts = Vec::new();
        for page in gift_range.iter_pages() {
            let page_range = match UserRange::from_pages(page, 1) {
                Ok(range) => range,
                Err(error) => {
                    return StepOutcome::Err(gift_error_to_errno(UserPageGiftError::Fault(
                        VmFaultError::Range(error),
                    )));
                }
            };
            let gift = match self.gift_one_page(&source_aspace, page_range) {
                Ok(Some(gift)) => gift,
                Ok(None) => return StepOutcome::Done(GiftBatch::new(Vec::new(), 0)),
                Err(UserPageGiftError::Blocked(token)) => {
                    return crate::vm::notification::yield_wait_token(NoProgress, token);
                }
                Err(error) => return StepOutcome::Err(gift_error_to_errno(error)),
            };
            gifts.push(gift);
        }

        let bytes = gifts.len().saturating_mul(USER_PAGE_SIZE);
        StepOutcome::Done(GiftBatch::new(gifts, bytes))
    }

    fn gift_one_page(
        &self,
        source_aspace: &Cap<AddressSpace>,
        page_range: UserRange,
    ) -> Result<Option<UserPageGift>, UserPageGiftError> {
        let outcome = crate::vm::checks::require_fault_recipe(
            self,
            VmFault::new(page_range.start(), AccessMode::Write),
        )
        .map_err(UserPageGiftError::Fault)?;
        let eligibility = classify_user_gift_page(&outcome.entry, page_range);
        let UserGiftEligibility::Giftable { freeze } = eligibility else {
            return Ok(None);
        };

        let guard = step_engine::guard();
        let materialization = match outcome.materialize_pagebacked_step(&guard) {
            VmFaultMaterializationStep::Done(materialization) => materialization,
            VmFaultMaterializationStep::Blocked(token) => {
                return Err(UserPageGiftError::Blocked(token));
            }
            VmFaultMaterializationStep::Err(error) => return Err(UserPageGiftError::Fault(error)),
        };
        drop(guard);
        let ppn = materialization.page.ppn;
        let page = page_range.start().containing_page();
        let private_off = outcome
            .private_page_off()
            .map_err(UserPageGiftError::Fault)?;
        let private_identity = outcome
            .entry
            .private()
            .and_then(|set| set.lookup(private_off));
        if let Some(snapshot) = private_identity {
            if snapshot.ppn != ppn {
                return Err(UserPageGiftError::Fault(VmFaultError::StaleRecipe));
            }
        }

        let pin = page_allocator::acquire_gift_pin(ppn).map_err(UserPageGiftError::Alloc)?;
        match freeze {
            UserPageGiftFreeze::DetachedPrivate => {
                let Some(set) = outcome.entry.private() else {
                    return Err(UserPageGiftError::Fault(VmFaultError::BackingMismatch));
                };
                let Some(snapshot) = private_identity else {
                    return Err(UserPageGiftError::Private(PrivatePageError::Missing));
                };
                self.pmap
                    .remove_page_for_gift(page, ppn)
                    .map_err(|error| UserPageGiftError::Fault(VmFaultError::Pmap(error)))?;
                set.take_if_match(
                    private_off,
                    PrivateFrameIdentity {
                        ppn: snapshot.ppn,
                        state: PrivateFrameState::Exclusive,
                    },
                )
                .map_err(UserPageGiftError::Private)?;
            }
            UserPageGiftFreeze::DemotedCow => {
                if let (Some(set), Some(snapshot)) = (outcome.entry.private(), private_identity) {
                    set.demote_if_match(
                        private_off,
                        PrivateFrameIdentity {
                            ppn: snapshot.ppn,
                            state: PrivateFrameState::Exclusive,
                        },
                    )
                    .map_err(UserPageGiftError::Private)?;
                }
                self.pmap
                    .protect_range(page_range, outcome.entry.prot.without_write())
                    .map_err(|error| UserPageGiftError::Fault(VmFaultError::Pmap(error)))?;
            }
        }
        drop(materialization);

        Ok(Some(UserPageGift::new_for_vm(
            ppn,
            UserPageGiftSource::new(source_aspace.clone(), page_range),
            pin,
            freeze,
        )))
    }
}

pub fn classify_user_gift_page(entry: &VmEntry, page_range: UserRange) -> UserGiftEligibility {
    if !entry.range.contains_range(page_range) {
        return UserGiftEligibility::CopyFallback(UserGiftFallbackReason::RangeOutsideEntry);
    }
    if page_range.len() != USER_PAGE_SIZE {
        return UserGiftEligibility::CopyFallback(UserGiftFallbackReason::NotFullPage);
    }
    if entry.flags.shared {
        return UserGiftEligibility::CopyFallback(UserGiftFallbackReason::SharedMapping);
    }
    if !entry.prot.permits(super::AccessMode::Write) {
        return UserGiftEligibility::CopyFallback(UserGiftFallbackReason::MissingWritePermission);
    }

    match entry.backing_kind() {
        VmEntryBacking::PrivateAnon => UserGiftEligibility::Giftable {
            freeze: UserPageGiftFreeze::DetachedPrivate,
        },
        VmEntryBacking::Page { .. } => {
            let Some((pc, _)) = entry.page_backing() else {
                return UserGiftEligibility::CopyFallback(UserGiftFallbackReason::MissingBacking);
            };
            if matches!(pc.kind(), PageContainerKind::Device { .. }) {
                UserGiftEligibility::CopyFallback(UserGiftFallbackReason::DeviceMapping)
            } else {
                UserGiftEligibility::Giftable {
                    freeze: UserPageGiftFreeze::DemotedCow,
                }
            }
        }
        VmEntryBacking::None => {
            UserGiftEligibility::CopyFallback(UserGiftFallbackReason::MissingBacking)
        }
    }
}

fn gift_error_to_errno(error: UserPageGiftError) -> Errno {
    match error {
        UserPageGiftError::Iov(UserGiftIovError::ZeroLength) => Errno::EINVAL,
        UserPageGiftError::Iov(UserGiftIovError::Overflow) => Errno::EFAULT,
        UserPageGiftError::Fault(error) => vm_fault_error_to_errno(error),
        UserPageGiftError::Blocked(_) => Errno::EAGAIN,
        UserPageGiftError::Private(PrivatePageError::Zone(_)) => Errno::ENOMEM,
        UserPageGiftError::Private(
            PrivatePageError::Conflict { .. } | PrivatePageError::Missing,
        ) => Errno::EAGAIN,
        UserPageGiftError::Alloc(AllocError::Exhausted) => Errno::ENOMEM,
        UserPageGiftError::Alloc(
            AllocError::InvalidRequest
            | AllocError::CounterOverflow
            | AllocError::CounterUnderflow
            | AllocError::DoubleFree,
        ) => Errno::EINVAL,
        UserPageGiftError::Alloc(
            AllocError::NotInitialized
            | AllocError::AlreadyInstalled
            | AllocError::ZeroScrubUnavailable
            | AllocError::FrameCopyUnavailable
            | AllocError::FrameKernelAddrUnavailable
            | AllocError::ReservedFrame,
        ) => Errno::EIO,
    }
}

const fn vm_fault_error_to_errno(error: VmFaultError) -> Errno {
    match error {
        VmFaultError::Range(_) => Errno::EFAULT,
        VmFaultError::NoRecipe => Errno::EFAULT,
        VmFaultError::ProtectionViolation => Errno::EFAULT,
        VmFaultError::WouldBlock => Errno::EAGAIN,
        VmFaultError::BackingMismatch
        | VmFaultError::BackingOffsetOverflow
        | VmFaultError::PageBeyondSize
        | VmFaultError::StaleRecipe => Errno::EINVAL,
        VmFaultError::PageCache(_) | VmFaultError::SpecialUnavailable => Errno::EIO,
        VmFaultError::Pmap(VmPmapError::Zone(_)) => Errno::ENOMEM,
        VmFaultError::Pmap(
            VmPmapError::Pmap(_)
            | VmPmapError::MissingReservation
            | VmPmapError::AlreadyMappedDrift
            | VmPmapError::MappingMismatch,
        ) => Errno::EIO,
    }
}

const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

const fn align_up(value: usize, align: usize) -> Option<usize> {
    let remainder = value & (align - 1);
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}
