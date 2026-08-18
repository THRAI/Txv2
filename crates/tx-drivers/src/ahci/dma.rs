use core::{marker::PhantomData, ptr::NonNull};

use crate::adapter::step_engine::{
    page_allocator, BitmapPageAllocator, DmaPin, DmaRunRequest, OwnedFrameRun, ZeroPolicy,
};
use tx_hal::{
    DmaAddr, DmaConstraints, DmaDirection, DmaDomain, DmaIf, DmaTranslation, PhysAddr, Ppn,
    TxPlatform,
};

const PAGE_SIZE: usize = 4096;
const AHCI_DMA_ADDRESS_BITS: u8 = 32;
pub(super) struct DmaWorkspace<P: TxPlatform> {
    paddr: PhysAddr,
    dma_addr: DmaAddr,
    vaddr: NonNull<u8>,
    len: usize,
    _pins: alloc::vec::Vec<DmaPin<'static, BitmapPageAllocator<'static>>>,
    _owned: OwnedFrameRun<'static, BitmapPageAllocator<'static>>,
    _platform: PhantomData<fn() -> P>,
}

// SAFETY: the allocation and every pin are uniquely owned by this token. Raw
// access is serialized by the containing AHCI controller's spin mutex.
unsafe impl<P: TxPlatform> Send for DmaWorkspace<P> {}

impl<P: TxPlatform> DmaWorkspace<P> {
    pub(super) fn allocate(
        domain: &'static DmaDomain,
        len: usize,
    ) -> Result<Self, DmaWorkspaceError> {
        if len == 0 || matches!(domain.translation, DmaTranslation::Managed { .. }) {
            return Err(DmaWorkspaceError::InvalidDomain);
        }

        let pages = len.div_ceil(PAGE_SIZE);
        let effective = DmaConstraints {
            dma_address_bits: domain
                .constraints
                .dma_address_bits
                .min(AHCI_DMA_ADDRESS_BITS),
            min_alignment: domain.constraints.min_alignment.max(PAGE_SIZE),
            segment_boundary: domain.constraints.segment_boundary,
            max_segment_len: domain.constraints.max_segment_len,
            max_segments: domain.constraints.max_segments,
        };
        if pages
            .checked_mul(PAGE_SIZE)
            .is_none_or(|bytes| bytes > effective.max_segment_len)
        {
            return Err(DmaWorkspaceError::InvalidDomain);
        }
        let page_align = effective.min_alignment.div_ceil(PAGE_SIZE).max(1);
        let owned = page_allocator::reserve_dma_run(
            domain,
            effective,
            DmaRunRequest {
                count: pages,
                align: page_align,
            },
            ZeroPolicy::UninitFullOverwrite,
        )
        .map_err(|_| DmaWorkspaceError::AllocationFailed)?
        .commit();
        let base = owned.base();
        let paddr = PhysAddr(base.0 * PAGE_SIZE);
        let vaddr = NonNull::new(
            page_allocator::frame_kernel_addr(base)
                .map_err(|_| DmaWorkspaceError::KernelMappingMissing)?,
        )
        .ok_or(DmaWorkspaceError::KernelMappingMissing)?;

        let mut pins = alloc::vec::Vec::with_capacity(pages);
        for offset in 0..pages {
            pins.push(
                page_allocator::acquire_dma_pin(Ppn(base.0 + offset))
                    .map_err(|_| DmaWorkspaceError::PinFailed)?,
            );
        }

        let raw_dma = match domain.translation {
            DmaTranslation::Direct { offset } => (paddr.0 as u64)
                .checked_add_signed(offset)
                .ok_or(DmaWorkspaceError::TranslationOverflow)?,
            DmaTranslation::Managed { .. } => unreachable!(),
        };
        let byte_len = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(DmaWorkspaceError::TranslationOverflow)?;
        let last = raw_dma
            .checked_add((byte_len - 1) as u64)
            .ok_or(DmaWorkspaceError::TranslationOverflow)?;
        if last > u32::MAX as u64 {
            return Err(DmaWorkspaceError::AddressOutside32Bit);
        }

        let kernel = <P as tx_hal::BootInfoIf>::boot_info().kernel_image;
        let end = paddr
            .0
            .checked_add(byte_len)
            .ok_or(DmaWorkspaceError::TranslationOverflow)?;
        let kernel_end = kernel
            .start
            .0
            .checked_add(kernel.size)
            .ok_or(DmaWorkspaceError::TranslationOverflow)?;
        let overlaps_kernel = paddr.0 < kernel_end && kernel.start.0 < end;
        if overlaps_kernel {
            return Err(DmaWorkspaceError::KernelImageOverlap);
        }

        <P as DmaIf>::sync_for_device(paddr, byte_len, DmaDirection::Bidirectional);
        Ok(Self {
            paddr,
            dma_addr: DmaAddr(raw_dma),
            vaddr,
            len: byte_len,
            _pins: pins,
            _owned: owned,
            _platform: PhantomData,
        })
    }

    pub(super) fn ptr_at(&self, offset: usize) -> Result<*mut u8, DmaWorkspaceError> {
        if offset >= self.len {
            return Err(DmaWorkspaceError::OutOfBounds);
        }
        Ok(unsafe { self.vaddr.as_ptr().add(offset) })
    }

    pub(super) fn dma_addr_at(&self, offset: usize) -> Result<u64, DmaWorkspaceError> {
        if offset >= self.len {
            return Err(DmaWorkspaceError::OutOfBounds);
        }
        self.dma_addr
            .0
            .checked_add(offset as u64)
            .ok_or(DmaWorkspaceError::TranslationOverflow)
    }

    pub(super) fn clear(&self, offset: usize, len: usize) -> Result<(), DmaWorkspaceError> {
        self.check_range(offset, len)?;
        unsafe {
            core::ptr::write_bytes(self.vaddr.as_ptr().add(offset), 0, len);
        }
        Ok(())
    }

    pub(super) fn sync_for_device(
        &self,
        offset: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Result<(), DmaWorkspaceError> {
        self.check_range(offset, len)?;
        <P as DmaIf>::sync_for_device(PhysAddr(self.paddr.0 + offset), len, direction);
        Ok(())
    }

    pub(super) fn sync_for_cpu(
        &self,
        offset: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Result<(), DmaWorkspaceError> {
        self.check_range(offset, len)?;
        <P as DmaIf>::sync_for_cpu(PhysAddr(self.paddr.0 + offset), len, direction);
        Ok(())
    }

    fn check_range(&self, offset: usize, len: usize) -> Result<(), DmaWorkspaceError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(DmaWorkspaceError::OutOfBounds);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DmaWorkspaceError {
    InvalidDomain,
    AllocationFailed,
    KernelMappingMissing,
    PinFailed,
    TranslationOverflow,
    AddressOutside32Bit,
    KernelImageOverlap,
    OutOfBounds,
}
