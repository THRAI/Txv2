use core::{marker::PhantomData, ptr::NonNull};

use tx_hal::{DmaDirection, DmaIf, PhysAddr, Ppn, TxPlatform};
use crate::adapter::step_engine::{
    page_allocator, BitmapPageAllocator, DmaPin, OwnedFrameRun, SpinMutex, ZeroPolicy,
};
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

    let owned = page_allocator::reserve_run(pages, 1, ZeroPolicy::Zeroed)
        .map_err(|_| ())?
        .commit();
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
        <P as DmaIf>::phys_to_dma(self.paddr).0 as usize
    }
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
            Err(()) => (0, NonNull::dangling()),
        }
    }

    unsafe fn dma_dealloc(paddr: VirtioPhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let phys = <P as DmaIf>::dma_to_phys(tx_hal::DmaAddr(paddr as u64));
        <P as DmaIf>::sync_for_cpu(phys, pages * PAGE_SIZE, DmaDirection::Bidirectional);
        let mut allocations = DMA_ALLOCATIONS.lock();
        let Some(index) = allocations.iter().position(|entry| {
            entry.dma_addr::<P>() == paddr && entry.vaddr == vaddr && entry.pages == pages
        }) else {
            return -1;
        };
        allocations.swap_remove(index);
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
        let allocation = match alloc_dma_pages::<P>(pages, direction) {
            Ok(allocation) => allocation,
            Err(()) => return 0,
        };

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
