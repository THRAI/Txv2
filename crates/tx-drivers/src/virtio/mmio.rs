//! VirtIO MMIO block transport for RISC-V QEMU virt and similar boards.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::adapter::step_engine::{self as step_engine, NoProgress, StepOutcome};
use step_engine::page_allocator;
use step_engine::SpinMutex;
use tx_hal::{MmioRegion, PlatformInfoIf, TxPlatform};
use tx_subsystems::device::{
    BlockDevice, BlockDeviceOps, BlockDurabilityCapabilities, PhysicalBlockNumber,
};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::Frame;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use super::dma::TxVirtioHal;

const PAGE_SIZE: usize = 4096;
const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioMmioError {
    MissingMmioRegion(&'static str),
    NullHeader,
    Transport,
    Device,
}

pub struct VirtioMmioBlock<P: TxPlatform> {
    mmio_region_name: &'static str,
    inner: SpinMutex<Option<VirtIOBlk<TxVirtioHal<P>, MmioTransport<'static>>>>,
    initialized: AtomicBool,
    total_blocks: AtomicU64,
    block_size: AtomicU32,
    _platform: PhantomData<fn() -> P>,
}

// SAFETY: `MmioTransport<'static>` only contains pointers to a fixed hardware
// MMIO region whose mapping is `'static`. Concurrent access is mediated by the
// `SpinMutex`. The `Send` requirement only matters for cross-CPU ownership of
// the inner `Option<VirtIOBlk<...>>`, which is sound because the underlying
// MMIO addresses are valid on every hart.
unsafe impl<P: TxPlatform> Send for VirtioMmioBlock<P> {}
unsafe impl<P: TxPlatform> Sync for VirtioMmioBlock<P> {}

impl<P: TxPlatform> VirtioMmioBlock<P> {
    pub const fn new(mmio_region_name: &'static str) -> Self {
        Self {
            mmio_region_name,
            inner: SpinMutex::new(None),
            initialized: AtomicBool::new(false),
            total_blocks: AtomicU64::new(0),
            block_size: AtomicU32::new(VIRTIO_BLK_SECTOR_SIZE),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), VirtioMmioError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let region = mmio_region::<P>(self.mmio_region_name)?;
        let header = NonNull::new(region.virt.start.0 as *mut VirtIOHeader)
            .ok_or(VirtioMmioError::NullHeader)?;

        // SAFETY: `header` points to the `virtio0` MMIO region mapped at
        // direct-map VA during early boot (see board `boot_static.rs`).
        // The mapping is `'static` for the lifetime of the kernel.
        let transport = unsafe { MmioTransport::new(header, region.virt.size) }
            .map_err(|_| VirtioMmioError::Transport)?;

        let blk = VirtIOBlk::<TxVirtioHal<P>, MmioTransport<'static>>::new(transport)
            .map_err(|_| VirtioMmioError::Device)?;

        self.total_blocks.store(blk.capacity(), Ordering::Release);
        self.block_size
            .store(VIRTIO_BLK_SECTOR_SIZE, Ordering::Release);
        *self.inner.lock() = Some(blk);
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}

fn mmio_region<P: TxPlatform>(name: &'static str) -> Result<MmioRegion, VirtioMmioError> {
    <P as PlatformInfoIf>::platform_info()
        .mmio_regions
        .iter()
        .copied()
        .find(|region| region.name == name)
        .ok_or(VirtioMmioError::MissingMmioRegion(name))
}

impl<P: TxPlatform> BlockDeviceOps for VirtioMmioBlock<P> {
    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        for (idx, frame) in target.iter_mut().enumerate() {
            let Some(lba) = block_id
                .as_u64()
                .checked_add(idx as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = frame_slice_mut(*frame) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.read_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
        }
        StepOutcome::Done(())
    }

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        for (idx, frame) in source.iter().enumerate() {
            let Some(lba) = block_id
                .as_u64()
                .checked_add(idx as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = frame_slice(*frame) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.write_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
        }
        StepOutcome::Done(())
    }

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        if blk.flush().is_err() {
            return StepOutcome::Err(Errno::EIO.into());
        }
        StepOutcome::Done(())
    }

    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        // virtio-drivers exposes a durable flush but no FUA write option.
        BlockDurabilityCapabilities {
            fua: false,
            flush: true,
        }
    }
}

impl<P: TxPlatform> BlockDevice for VirtioMmioBlock<P> {
    fn total_blocks(&self) -> u64 {
        self.total_blocks.load(Ordering::Acquire)
    }

    fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Acquire)
    }
}

fn sectors_per_page(block_size: u32) -> u32 {
    if block_size == 0 || !PAGE_SIZE.is_multiple_of(block_size as usize) {
        return 0;
    }
    (PAGE_SIZE / block_size as usize) as u32
}

fn frame_slice_mut(frame: Frame) -> Option<&'static mut [u8]> {
    let ptr = page_allocator::frame_kernel_addr(frame.ppn()).ok()?;
    Some(unsafe { core::slice::from_raw_parts_mut(ptr, PAGE_SIZE) })
}

fn frame_slice(frame: Frame) -> Option<&'static [u8]> {
    let ptr = page_allocator::frame_kernel_addr(frame.ppn()).ok()?;
    Some(unsafe { core::slice::from_raw_parts(ptr, PAGE_SIZE) })
}
