use core::{marker::PhantomData, ptr::NonNull};

use crate::adapter::step_engine::{
    page_allocator, BitmapPageAllocator, DmaPin, DmaRunRequest, OwnedFrameRun, SpinMutex,
    ZeroPolicy,
};
use tx_hal::{DmaDirection, DmaDomain, DmaIf, DmaTranslation, PhysAddr, Ppn, TxPlatform};
use virtio_drivers::{BufferDirection, PhysAddr as VirtioPhysAddr};

const PAGE_SIZE: usize = virtio_drivers::PAGE_SIZE;

pub struct TxVirtioHal<P> {
    _platform: PhantomData<fn() -> P>,
}

struct DmaAllocation {
    paddr: PhysAddr,
    vaddr: NonNull<u8>,
    pages: usize,
    owned: OwnedFrameRun<'static, BitmapPageAllocator<'static>>,
    pins: alloc::vec::Vec<DmaPin<'static, BitmapPageAllocator<'static>>>,
    domain: Option<&'static DmaDomain>,
}

struct SharedBuffer {
    allocation: DmaAllocation,
    len: usize,
    direction: BufferDirection,
    original: NonNull<[u8]>,
}

// SAFETY: These tokens represent exclusive ownership of DMA pages recorded
// behind a global spin mutex. The underlying pages are not exposed as Rust
// references through the records themselves.
unsafe impl Send for DmaAllocation {}
unsafe impl Send for SharedBuffer {}

static DMA_ALLOCATIONS: SpinMutex<alloc::vec::Vec<DmaAllocation>> =
    SpinMutex::new(alloc::vec::Vec::new());
static SHARED_BUFFERS: SpinMutex<alloc::vec::Vec<SharedBuffer>> =
    SpinMutex::new(alloc::vec::Vec::new());
static CONFIGURED_DMA_DOMAIN: SpinMutex<Option<&'static DmaDomain>> = SpinMutex::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioDmaDomainError {
    ManagedTranslationUnsupported,
    ConflictingDomain,
}

/// Install the explicit DMA domain consumed by the statically bound VirtIO
/// devices. Reinstalling the same immutable graph record is harmless; mixing
/// different domains behind one `virtio_drivers::Hal` type is rejected.
pub fn configure_dma_domain(domain: &'static DmaDomain) -> Result<(), VirtioDmaDomainError> {
    if matches!(domain.translation, DmaTranslation::Managed { .. }) {
        return Err(VirtioDmaDomainError::ManagedTranslationUnsupported);
    }
    let mut configured = CONFIGURED_DMA_DOMAIN.lock();
    match *configured {
        Some(current) if current != domain => Err(VirtioDmaDomainError::ConflictingDomain),
        Some(_) => Ok(()),
        None => {
            *configured = Some(domain);
            Ok(())
        }
    }
}

fn virtio_direction(direction: BufferDirection) -> DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => DmaDirection::ToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

fn alloc_dma_pages<P: TxPlatform>(
    pages: usize,
    direction: BufferDirection,
) -> Result<DmaAllocation, ()> {
    if pages == 0 {
        return Err(());
    }

    let domain = *CONFIGURED_DMA_DOMAIN.lock();
    let owned = match domain {
        Some(domain) => {
            let page_align = domain.constraints.min_alignment.div_ceil(PAGE_SIZE).max(1);
            page_allocator::reserve_dma_run(
                domain,
                domain.constraints,
                DmaRunRequest {
                    count: pages,
                    align: page_align,
                },
                ZeroPolicy::Zeroed,
            )
            .map_err(|_| ())?
            .commit()
        }
        None => page_allocator::reserve_run(pages, 1, ZeroPolicy::Zeroed)
            .map_err(|_| ())?
            .commit(),
    };
    let base = owned.base();
    let paddr = PhysAddr(base.0 * PAGE_SIZE);
    let vaddr = page_allocator::frame_kernel_addr(base).map_err(|_| ())?;
    let vaddr = NonNull::new(vaddr).ok_or(())?;

    let mut pins = alloc::vec::Vec::with_capacity(pages);
    let mut offset = 0usize;
    while offset < pages {
        let frame = Ppn(base.0 + offset);
        let pin = page_allocator::acquire_dma_pin(frame).map_err(|_| ())?;
        pins.push(pin);
        offset += 1;
    }

    <P as DmaIf>::sync_for_device(paddr, pages * PAGE_SIZE, virtio_direction(direction));

    Ok(DmaAllocation {
        paddr,
        vaddr,
        pages,
        owned,
        pins,
        domain,
    })
}

unsafe fn copy_to_dma(src: NonNull<[u8]>, dst: NonNull<u8>, len: usize) {
    core::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst.as_ptr(), len);
}

unsafe fn copy_from_dma(src: NonNull<u8>, dst: NonNull<[u8]>, len: usize) {
    core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr() as *mut u8, len);
}

impl DmaAllocation {
    fn dma_addr<P: TxPlatform>(&self) -> VirtioPhysAddr {
        match self.domain.map(|domain| domain.translation) {
            Some(DmaTranslation::Direct { offset }) => self
                .paddr
                .0
                .checked_add_signed(offset as isize)
                .expect("validated direct DMA translation overflow"),
            Some(DmaTranslation::Managed { .. }) => {
                unreachable!("managed DMA domains are rejected during configuration")
            }
            None => <P as DmaIf>::phys_to_dma(self.paddr).0 as usize,
        }
    }
}

#[cold]
fn dma_allocation_failure(pages: usize, len: usize, operation: &str) -> ! {
    let diagnostics = page_allocator::backend_diagnostics().ok();
    let free = diagnostics.as_ref().map_or(0, |diag| diag.free_count);
    let max_run = diagnostics
        .as_ref()
        .map_or(0, |diag| diag.max_contiguous_free_run);
    panic!("virtio DMA {operation} failed: len={len} pages={pages} free={free} max_run={max_run}")
}

impl Drop for DmaAllocation {
    fn drop(&mut self) {
        let _ = self.owned.base();
        let _ = self.pins.len();
    }
}

unsafe impl<P: TxPlatform> virtio_drivers::Hal for TxVirtioHal<P> {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (VirtioPhysAddr, NonNull<u8>) {
        match alloc_dma_pages::<P>(pages, direction) {
            Ok(allocation) => {
                let paddr = allocation.dma_addr::<P>();
                let vaddr = allocation.vaddr;
                DMA_ALLOCATIONS.lock().push(allocation);
                (paddr, vaddr)
            }
            // The HAL has no error return. Publishing address zero makes the
            // device ignore an invalid descriptor while the caller spins for
            // a completion forever, so fail with allocator diagnostics.
            Err(()) => dma_allocation_failure(pages, pages.saturating_mul(PAGE_SIZE), "alloc"),
        }
    }

    unsafe fn dma_dealloc(paddr: VirtioPhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let mut allocations = DMA_ALLOCATIONS.lock();
        let Some(index) = allocations.iter().position(|entry| {
            entry.dma_addr::<P>() == paddr && entry.vaddr == vaddr && entry.pages == pages
        }) else {
            return -1;
        };
        let allocation = allocations.swap_remove(index);
        drop(allocations);
        <P as DmaIf>::sync_for_cpu(
            allocation.paddr,
            pages * PAGE_SIZE,
            DmaDirection::Bidirectional,
        );
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: VirtioPhysAddr, size: usize) -> NonNull<u8> {
        let end = paddr
            .checked_add(size)
            .expect("virtio mmio_phys_to_virt overflow");
        for region in <P as tx_hal::PlatformInfoIf>::platform_info().mmio_regions {
            let start = region.phys.start.0;
            let region_end = start + region.phys.size;
            if paddr >= start && end <= region_end {
                let offset = paddr - start;
                let vaddr = region.virt.start.0 + offset;
                return NonNull::new(vaddr as *mut u8).expect("virtio mmio region VA is null");
            }
        }
        panic!("virtio MMIO PA {paddr:#x}+{size:#x} is not in PlatformInfo.mmio_regions");
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> VirtioPhysAddr {
        let len = buffer.as_ref().len();
        if len == 0 {
            return 0;
        }
        let pages = len.div_ceil(PAGE_SIZE);
        let allocation = alloc_dma_pages::<P>(pages, direction)
            .unwrap_or_else(|()| dma_allocation_failure(pages, len, "share"));

        match direction {
            BufferDirection::DriverToDevice | BufferDirection::Both => {
                copy_to_dma(buffer, allocation.vaddr, len);
                <P as DmaIf>::sync_for_device(allocation.paddr, len, virtio_direction(direction));
            }
            BufferDirection::DeviceToDriver => {}
        }

        let paddr = allocation.dma_addr::<P>();
        let shared = SharedBuffer {
            allocation,
            len,
            direction,
            original: buffer,
        };
        SHARED_BUFFERS.lock().push(shared);
        paddr
    }

    unsafe fn unshare(paddr: VirtioPhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let mut shared = SHARED_BUFFERS.lock();
        let Some(index) = shared
            .iter()
            .position(|entry| entry.allocation.dma_addr::<P>() == paddr)
        else {
            return;
        };
        let entry = shared.swap_remove(index);
        let dir = entry.direction;
        let len = entry.len.min(buffer.as_ref().len());
        <P as DmaIf>::sync_for_cpu(entry.allocation.paddr, len, virtio_direction(direction));
        match dir {
            BufferDirection::DeviceToDriver | BufferDirection::Both => {
                copy_from_dma(entry.allocation.vaddr, entry.original, len);
            }
            BufferDirection::DriverToDevice => {}
        }
    }
}
