use core::{marker::PhantomData, ptr::NonNull};

use crate::adapter::step_engine::{
    page_allocator, BitmapPageAllocator, DmaPin, DmaRunRequest, OwnedFrameRun, ZeroPolicy,
};
use tx_hal::{DmaAddr, DmaDirection, DmaDomain, DmaIf, DmaTranslation, PhysAddr, Ppn, TxPlatform};

const PAGE_SIZE: usize = 4096;

pub(super) struct DmaBuffer<P: TxPlatform> {
    paddr: PhysAddr,
    dma_addr: DmaAddr,
    vaddr: NonNull<u8>,
    len: usize,
    _owned: OwnedFrameRun<'static, BitmapPageAllocator<'static>>,
    _pins: alloc::vec::Vec<DmaPin<'static, BitmapPageAllocator<'static>>>,
    _platform: PhantomData<fn() -> P>,
}

unsafe impl<P: TxPlatform> Send for DmaBuffer<P> {}

impl<P: TxPlatform> DmaBuffer<P> {
    pub(super) fn allocate(domain: &'static DmaDomain, len: usize) -> Result<Self, DmaBufferError> {
        if len == 0 || len > domain.constraints.max_segment_len {
            return Err(DmaBufferError::InvalidLength);
        }
        if matches!(domain.translation, DmaTranslation::Managed { .. }) {
            return Err(DmaBufferError::ManagedTranslationUnsupported);
        }

        let pages = len.div_ceil(PAGE_SIZE);
        let page_align = domain.constraints.min_alignment.div_ceil(PAGE_SIZE).max(1);
        let owned = page_allocator::reserve_dma_run(
            domain,
            domain.constraints,
            DmaRunRequest {
                count: pages,
                align: page_align,
            },
            ZeroPolicy::Zeroed,
        )
        .map_err(|_| DmaBufferError::AllocationFailed)?
        .commit();
        let base = owned.base();
        let paddr = PhysAddr(base.0 * PAGE_SIZE);
        let vaddr = NonNull::new(
            page_allocator::frame_kernel_addr(base)
                .map_err(|_| DmaBufferError::KernelMappingMissing)?,
        )
        .ok_or(DmaBufferError::KernelMappingMissing)?;

        let mut pins = alloc::vec::Vec::with_capacity(pages);
        for offset in 0..pages {
            pins.push(
                page_allocator::acquire_dma_pin(Ppn(base.0 + offset))
                    .map_err(|_| DmaBufferError::PinFailed)?,
            );
        }

        let raw_dma = match domain.translation {
            DmaTranslation::Direct { offset } => (paddr.0 as u64)
                .checked_add_signed(offset)
                .ok_or(DmaBufferError::TranslationOverflow)?,
            DmaTranslation::Managed { .. } => unreachable!(),
        };
        let address_limit = if domain.constraints.dma_address_bits == u64::BITS as u8 {
            u64::MAX
        } else {
            (1u64 << domain.constraints.dma_address_bits) - 1
        };
        let last = raw_dma
            .checked_add((pages * PAGE_SIZE - 1) as u64)
            .ok_or(DmaBufferError::TranslationOverflow)?;
        if last > address_limit {
            return Err(DmaBufferError::AddressOutsideDomain);
        }

        <P as DmaIf>::sync_for_device(paddr, pages * PAGE_SIZE, DmaDirection::Bidirectional);
        Ok(Self {
            paddr,
            dma_addr: DmaAddr(raw_dma),
            vaddr,
            len: pages * PAGE_SIZE,
            _owned: owned,
            _pins: pins,
            _platform: PhantomData,
        })
    }

    pub(super) const fn len(&self) -> usize {
        self.len
    }

    pub(super) fn dma_addr_at(&self, offset: usize) -> Result<u64, DmaBufferError> {
        if offset >= self.len {
            return Err(DmaBufferError::OutOfBounds);
        }
        self.dma_addr
            .0
            .checked_add(offset as u64)
            .ok_or(DmaBufferError::TranslationOverflow)
    }

    pub(super) fn ptr_at(&self, offset: usize) -> Result<*mut u8, DmaBufferError> {
        if offset >= self.len {
            return Err(DmaBufferError::OutOfBounds);
        }
        Ok(unsafe { self.vaddr.as_ptr().add(offset) })
    }

    pub(super) fn sync_for_cpu(&self, offset: usize, len: usize, direction: DmaDirection) {
        debug_assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
        <P as DmaIf>::sync_for_cpu(PhysAddr(self.paddr.0 + offset), len, direction);
    }

    pub(super) fn sync_for_device(&self, offset: usize, len: usize, direction: DmaDirection) {
        debug_assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
        <P as DmaIf>::sync_for_device(PhysAddr(self.paddr.0 + offset), len, direction);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DmaBufferError {
    InvalidLength,
    ManagedTranslationUnsupported,
    AllocationFailed,
    KernelMappingMissing,
    PinFailed,
    TranslationOverflow,
    AddressOutsideDomain,
    OutOfBounds,
}
