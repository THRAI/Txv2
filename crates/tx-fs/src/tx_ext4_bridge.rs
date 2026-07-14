//! Adapter from `tx_subsystems::device::BlockDevice` to `tx_ext4_format::BlockImage`.
//!
//! `Ext4Pager` needs a `BlockImage`: it reads/writes 4 KiB ext4 blocks. The
//! kernel block-device registry exposes `BlockDevice` / `BlockDeviceOps`:
//! sector-LBA addressed, page-frame DMA targets, EBR-guarded. This module
//! bridges the two by allocating a transient frame, issuing a DMA operation,
//! and copying between the frame and the caller's `[u8; 4096]` buffer.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::NonNull;

use crate::devfs::adapter::step_engine::{
    epoch, page_allocator, SpinMutex, StepOutcome, ZeroPolicy,
};
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::{Ext4FormatError, Result};
use tx_ext4::planner::Ext4BlockGeometry;
use tx_subsystems::device::{self, BlockDevice, BlockDeviceHandle, PhysicalBlockNumber};
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::page_backed::{Frame, PageContainer};

/// Concrete L5-to-L6 binder for one mounted ext4 block device.
///
/// Mount code supplies the registered handle explicitly.  The bridge never
/// derives a device identity from the image's `dyn BlockDevice`, because that
/// would make a partition or layered device indistinguishable from its parent.
#[derive(Clone, Copy, Debug)]
pub struct Ext4FileIoRuntimeBinder {
    handle: BlockDeviceHandle,
}

impl Ext4FileIoRuntimeBinder {
    pub const fn new(handle: BlockDeviceHandle) -> Self {
        Self { handle }
    }

    pub const fn handle(self) -> BlockDeviceHandle {
        self.handle
    }
}

impl tx_ext4::mount::FilePageContainerBinder for Ext4FileIoRuntimeBinder {
    fn bind_file_page_container(
        &self,
        container: tx_subsystems::adapter::step_engine::Cap<PageContainer>,
    ) {
        let _runtime = device::register_page_container_file_io_service(container, self.handle);
    }
}

/// A `BlockImage` that reads through a kernel block device.
///
/// The underlying `&'static dyn BlockDevice` is expected to outlive this
/// adapter — typically by leaking the device through
/// `Box::leak` at boot (see `crates/tx-kernel/src/devices.rs`).
pub struct BlockDeviceImage {
    device: &'static dyn BlockDevice,
    cache: SpinMutex<ReadBlockCache>,
}

impl BlockDeviceImage {
    pub fn new(device: &'static dyn BlockDevice) -> Self {
        Self {
            device,
            cache: SpinMutex::new(ReadBlockCache::new()),
        }
    }

    fn sectors_per_ext4_block(&self) -> Option<u64> {
        let sector = self.device.block_size() as u64;
        if sector == 0 || !(BLOCK_SIZE as u64).is_multiple_of(sector) {
            return None;
        }
        Some(BLOCK_SIZE as u64 / sector)
    }

    /// Bind this image's 4 KiB ext4 blocks to its registered L6 device key.
    /// The caller owns the key because `BlockDevice` deliberately exposes no
    /// registry identity and guessing one would misroute I/O.
    pub fn block_geometry(&self, device: DeviceKey) -> Option<Ext4BlockGeometry> {
        self.sectors_per_ext4_block()
            .map(|sectors_per_block| Ext4BlockGeometry::new(device, sectors_per_block))
    }

    fn read_block_uncached(&self, block: u64, out: &mut Page4K) -> Result<()> {
        let spb = self
            .sectors_per_ext4_block()
            .ok_or(Ext4FormatError::Unsupported)?;
        let lba = block.checked_mul(spb).ok_or(Ext4FormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| Ext4FormatError::Truncated)?;
        let run = reservation.commit();
        let ppn = run.base();
        let mut frame = Frame::new(ppn);

        // Borrow the caller's active guard when one is already held (e.g.
        // exec_script's VFS-lookup guard).  Creating a nested guard would
        // trigger the EBR no-nesting debug_assert even though the block device
        // ops ignore the guard parameter entirely.  Fall back to a fresh guard
        // when no guard is active.
        let guard = epoch::borrow_current_guard().unwrap_or_else(epoch::guard);
        let outcome = self.device.read_blocks(
            PhysicalBlockNumber::new(lba),
            core::slice::from_mut(&mut frame),
            &guard,
        );
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return Err(Ext4FormatError::WouldBlock);
            }
            StepOutcome::Err(_) => return Err(Ext4FormatError::Truncated),
        }

        let src = page_allocator::frame_kernel_addr(ppn).map_err(|_| Ext4FormatError::Truncated)?;
        let src_nn = NonNull::new(src).ok_or(Ext4FormatError::Truncated)?;
        // SAFETY: `src_nn` points to BLOCK_SIZE bytes of a frame we own
        // through `run`. `out` is a `&mut [u8; BLOCK_SIZE]`. Both regions
        // are valid for `BLOCK_SIZE` bytes and the kernel-VA mapping for
        // a freshly reserved frame does not alias `out`.
        unsafe {
            core::ptr::copy_nonoverlapping(src_nn.as_ptr(), out.as_mut_ptr(), BLOCK_SIZE);
        }
        drop(run);
        Ok(())
    }
}

impl BlockImage for BlockDeviceImage {
    fn total_blocks(&self) -> u64 {
        let Some(spb) = self.sectors_per_ext4_block() else {
            return 0;
        };
        self.device.total_blocks() / spb
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
        let cached = { self.cache.lock().get(block) };
        if let Some(cached) = cached {
            out.copy_from_slice(cached.as_ref());
            return Ok(());
        }
        self.read_block_uncached(block, out)?;
        let cached = Arc::new(*out);
        self.cache.lock().insert(block, cached);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()> {
        let spb = self
            .sectors_per_ext4_block()
            .ok_or(Ext4FormatError::Unsupported)?;
        let lba = block.checked_mul(spb).ok_or(Ext4FormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| Ext4FormatError::Truncated)?;
        let run = reservation.commit();
        let ppn = run.base();
        let frame = Frame::new(ppn);

        let dst = page_allocator::frame_kernel_addr(ppn).map_err(|_| Ext4FormatError::Truncated)?;
        let dst_nn = NonNull::new(dst).ok_or(Ext4FormatError::Truncated)?;
        // SAFETY: `dst_nn` is the kernel direct-map VA of the freshly
        // allocated frame we own through `run`. `data` is `&[u8; BLOCK_SIZE]`.
        // Both regions are valid for BLOCK_SIZE bytes and disjoint.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), dst_nn.as_ptr(), BLOCK_SIZE);
        }

        let guard = epoch::borrow_current_guard().unwrap_or_else(epoch::guard);
        let outcome = self.device.write_blocks(
            PhysicalBlockNumber::new(lba),
            core::slice::from_ref(&frame),
            &guard,
        );
        drop(guard);
        drop(run);
        match outcome {
            StepOutcome::Done(()) => {
                let cached = Arc::new(*data);
                self.cache.lock().insert(block, cached);
                Ok(())
            }
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(Ext4FormatError::WouldBlock)
            }
            StepOutcome::Err(_) => Err(Ext4FormatError::Truncated),
        }
    }
}

// 4096 × 4 KiB = 16 MiB. 128 entries (512 KiB) thrashed on every exec: the
// sdcard busybox alone is ~1.4 MiB (~350 ext4 blocks), so each shell command
// re-read most of the binary through virtio (~5-10 ms/block under TCG ≈
// seconds per command) — the dominant cost of every LTP shell test. The hot
// set (busybox + ash scripts + libc + common test binaries) fits in a few MiB.
const READ_BLOCK_CACHE_ENTRIES: usize = 4096;

struct ReadBlockCache {
    clock: u64,
    // block -> (last_used, data). O(log n) lookup; the previous Vec scan was
    // O(entries) per get and at 4096 entries × ~350 block reads per exec the
    // index walk itself dominated (every shell command pays one exec).
    entries: alloc::collections::BTreeMap<u64, (u64, Arc<Page4K>)>,
    // last_used -> block mirror for O(log n) LRU eviction. last_used values
    // are unique (clock strictly increases on every touch).
    lru: alloc::collections::BTreeMap<u64, u64>,
}

impl ReadBlockCache {
    fn new() -> Self {
        Self {
            clock: 0,
            entries: alloc::collections::BTreeMap::new(),
            lru: alloc::collections::BTreeMap::new(),
        }
    }

    fn get(&mut self, block: u64) -> Option<Arc<Page4K>> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let (last_used, data) = self.entries.get_mut(&block)?;
        self.lru.remove(last_used);
        *last_used = clock;
        self.lru.insert(clock, block);
        Some(data.clone())
    }

    fn insert(&mut self, block: u64, data: Arc<Page4K>) {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        if let Some((last_used, slot)) = self.entries.get_mut(&block) {
            self.lru.remove(last_used);
            *last_used = clock;
            *slot = data;
            self.lru.insert(clock, block);
            return;
        }
        if self.entries.len() >= READ_BLOCK_CACHE_ENTRIES {
            if let Some((&oldest, &victim_block)) = self.lru.iter().next() {
                self.lru.remove(&oldest);
                self.entries.remove(&victim_block);
            }
        }
        self.entries.insert(block, (clock, data));
        self.lru.insert(clock, block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devfs::adapter::step_engine::{page_allocator, NoProgress};
    use tx_subsystems::device::{
        BlockDeviceHandle, BlockDeviceOps, BlockDeviceRegistration, DevT, PhysicalBlockNumber,
    };
    use tx_subsystems::execution::Guard;

    #[test]
    fn read_block_cache_returns_shared_page_for_lock_free_copy() {
        let mut cache = ReadBlockCache::new();
        let mut block = [0u8; BLOCK_SIZE];
        block[0] = 0x5a;
        block[BLOCK_SIZE - 1] = 0xa5;

        let shared = alloc::sync::Arc::new(block);
        cache.insert(7, shared.clone());

        let cached = cache.get(7).expect("cache hit");
        assert!(alloc::sync::Arc::ptr_eq(&cached, &shared));
        assert_eq!(cached[0], 0x5a);
        assert_eq!(cached[BLOCK_SIZE - 1], 0xa5);
        assert!(cache.get(8).is_none());
    }

    #[derive(Clone, Copy)]
    enum BlockingMode {
        Continue,
        Yield,
    }

    struct BlockingBlockDevice {
        mode: BlockingMode,
    }

    impl BlockingBlockDevice {
        const fn new(mode: BlockingMode) -> Self {
            Self { mode }
        }

        fn outcome(&self) -> StepOutcome<(), NoProgress> {
            match self.mode {
                BlockingMode::Continue => StepOutcome::continue_with(NoProgress),
                BlockingMode::Yield => StepOutcome::yield_on_wait_source(NoProgress, 42, 0x1),
            }
        }
    }

    impl BlockDeviceOps for BlockingBlockDevice {
        fn read_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.outcome()
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.outcome()
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl BlockDevice for BlockingBlockDevice {
        fn total_blocks(&self) -> u64 {
            64
        }

        fn block_size(&self) -> u32 {
            512
        }
    }

    static CONTINUE_DEVICE: BlockingBlockDevice = BlockingBlockDevice::new(BlockingMode::Continue);
    static YIELD_DEVICE: BlockingBlockDevice = BlockingBlockDevice::new(BlockingMode::Yield);
    static FILE_IO_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 65),
        name: "ext4-test",
        ops: &CONTINUE_DEVICE,
    };

    fn init_bridge_test() {
        tx_test_support::init_host();
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for ext4 bridge tests: {error:?}"),
        }
    }

    #[test]
    fn ext4_bridge_maps_retrying_read_to_would_block() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        let mut out = [0u8; BLOCK_SIZE];

        assert_eq!(
            BlockDeviceImage::new(&CONTINUE_DEVICE).read_block(0, &mut out),
            Err(Ext4FormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::new(&YIELD_DEVICE).read_block(0, &mut out),
            Err(Ext4FormatError::WouldBlock)
        );
    }

    #[test]
    fn ext4_bridge_maps_retrying_write_to_would_block() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        let data = [0x5au8; BLOCK_SIZE];

        assert_eq!(
            BlockDeviceImage::new(&CONTINUE_DEVICE).write_block(0, &data),
            Err(Ext4FormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::new(&YIELD_DEVICE).write_block(0, &data),
            Err(Ext4FormatError::WouldBlock)
        );
    }

    #[test]
    fn ext4_file_io_runtime_binder_registers_supplied_block_handle() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        tx_subsystems::zones::register_all().expect("tx-subsystems zones");
        tx_subsystems::device::reset_page_container_file_io_service_registry_for_test();

        let container = tx_subsystems::page_backed::PageContainer::new_cap(
            tx_subsystems::page_backed::PageContainerKind::Anon {
                swap_policy: tx_subsystems::page_backed::AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");
        let binder = Ext4FileIoRuntimeBinder::new(BlockDeviceHandle::whole(&FILE_IO_REGISTRATION));

        tx_ext4::mount::FilePageContainerBinder::bind_file_page_container(&binder, container);

        let runtimes = tx_subsystems::device::page_container_file_io_service_runtimes_snapshot();
        assert_eq!(runtimes.len(), 1);
        assert_eq!(
            runtimes[0].handle().registration().devt,
            FILE_IO_REGISTRATION.devt
        );
        tx_subsystems::device::reset_page_container_file_io_service_registry_for_test();
    }
}
