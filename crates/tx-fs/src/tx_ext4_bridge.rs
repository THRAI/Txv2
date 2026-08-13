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
    borrow_current_guard, guard, page_allocator, SpinMutex, StepOutcome, ZeroPolicy,
};
use tx_ext4::planner::Ext4BlockGeometry;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::{Ext4FormatError, Result};
use tx_subsystems::device::{self, BlockDeviceHandle, BlockDeviceRegistration};
use tx_subsystems::execution::Errno;
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
/// The handle is the single source of device identity and bounds for the
/// image, its journal, and file-I/O runtime binder.
pub struct BlockDeviceImage {
    handle: BlockDeviceHandle,
    cache: SpinMutex<ReadBlockCache>,
}

impl BlockDeviceImage {
    pub fn new(handle: BlockDeviceHandle) -> Self {
        Self {
            handle,
            cache: SpinMutex::new(ReadBlockCache::new()),
        }
    }

    /// Compatibility constructor for callers mounting an entire device.
    pub fn whole(reg: &'static BlockDeviceRegistration) -> Self {
        Self::new(BlockDeviceHandle::whole(reg))
    }

    pub const fn handle(&self) -> BlockDeviceHandle {
        self.handle
    }

    fn sectors_per_ext4_block(&self) -> Option<u64> {
        self.handle.blocks_per_frame()
    }

    /// Bind this image's 4 KiB ext4 blocks to the handle's registered device.
    pub fn block_geometry(&self) -> Option<Ext4BlockGeometry> {
        let device = DeviceKey::new(self.handle.registration().devt.raw());
        self.sectors_per_ext4_block()
            .map(|sectors_per_block| Ext4BlockGeometry::new(device, sectors_per_block))
    }

    fn read_block_uncached(&self, block: u64, out: &mut Page4K) -> Result<()> {
        let blocks = self.read_blocks_uncached(block, 1)?;
        out.copy_from_slice(blocks[0].as_ref());
        Ok(())
    }

    fn read_blocks_uncached(&self, first_block: u64, count: usize) -> Result<Vec<Arc<Page4K>>> {
        if count == 0 {
            return Err(Ext4FormatError::InvalidInput);
        }
        let spb = self
            .sectors_per_ext4_block()
            .ok_or(Ext4FormatError::Unsupported)?;
        let lba = first_block
            .checked_mul(spb)
            .ok_or(Ext4FormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_run(count, 1, ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| Ext4FormatError::Truncated)?;
        let run = reservation.commit();
        let base = run.base();
        let mut frames = Vec::with_capacity(count);
        for index in 0..count {
            let mut ppn = base;
            ppn.0 = ppn
                .0
                .checked_add(index)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            frames.push(Frame::new(ppn));
        }

        // Borrow the caller's active guard when one is already held (e.g.
        // exec_script's VFS-lookup guard).  Creating a nested guard would
        // trigger the EBR no-nesting debug_assert even though the block device
        // ops ignore the guard parameter entirely.  Fall back to a fresh guard
        // when no guard is active.
        let guard = borrow_current_guard().unwrap_or_else(guard);
        let outcome = self.handle.read_blocks(lba, &mut frames, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return Err(Ext4FormatError::WouldBlock);
            }
            StepOutcome::Err(error) => return Err(map_device_error(error)),
        }

        let mut blocks = Vec::with_capacity(count);
        for frame in &frames {
            let src = page_allocator::frame_kernel_addr(frame.ppn())
                .map_err(|_| Ext4FormatError::Truncated)?;
            let src_nn = NonNull::new(src).ok_or(Ext4FormatError::Truncated)?;
            let mut page = [0u8; BLOCK_SIZE];
            // SAFETY: `src_nn` points to one frame in `run`, which remains
            // owned until every page has been copied into the adapter cache.
            unsafe {
                core::ptr::copy_nonoverlapping(src_nn.as_ptr(), page.as_mut_ptr(), BLOCK_SIZE);
            }
            blocks.push(Arc::new(page));
        }
        drop(run);
        Ok(blocks)
    }

    fn cache_blocks(&self, first_block: u64, blocks: &[Arc<Page4K>]) {
        let mut cache = self.cache.lock();
        for (index, page) in blocks.iter().enumerate() {
            let Some(block) = first_block.checked_add(index as u64) else {
                break;
            };
            cache.insert(block, Arc::clone(page));
        }
    }
}

impl BlockImage for BlockDeviceImage {
    fn total_blocks(&self) -> u64 {
        let Some(spb) = self.sectors_per_ext4_block() else {
            return 0;
        };
        self.handle.len_lba() / spb
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

    fn read_data_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
        let cached = { self.cache.lock().get(block) };
        if let Some(cached) = cached {
            out.copy_from_slice(cached.as_ref());
            return Ok(());
        }

        let remaining = self.total_blocks().saturating_sub(block);
        let count = usize::try_from(remaining.min(DATA_READ_AHEAD_BLOCKS as u64))
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        if count == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }

        match self.read_blocks_uncached(block, count) {
            Ok(blocks) => {
                out.copy_from_slice(blocks[0].as_ref());
                self.cache_blocks(block, &blocks);
                Ok(())
            }
            Err(_) if count > 1 => self.read_block(block, out),
            Err(error) => Err(error),
        }
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

        let guard = borrow_current_guard().unwrap_or_else(guard);
        let outcome = self
            .handle
            .write_blocks(lba, core::slice::from_ref(&frame), &guard);
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
            StepOutcome::Err(error) => Err(map_device_error(error)),
        }
    }

    fn barrier(&mut self) -> Result<()> {
        let guard = borrow_current_guard().unwrap_or_else(guard);
        let outcome = self.handle.barrier(&guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(Ext4FormatError::WouldBlock)
            }
            StepOutcome::Err(error) => Err(map_device_error(error)),
        }
    }

    fn invalidate_block(&mut self, block: u64) {
        self.cache.lock().invalidate(block);
    }

    fn invalidate_all(&mut self) {
        self.cache.lock().clear();
    }
}

fn map_device_error(error: Errno) -> Ext4FormatError {
    if error == Errno::EROFS {
        Ext4FormatError::ReadOnly
    } else if error == Errno::EINVAL {
        Ext4FormatError::InvalidInput
    } else {
        Ext4FormatError::Io
    }
}

const DATA_READ_AHEAD_BLOCKS: usize = 16;

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

    fn invalidate(&mut self, block: u64) {
        if let Some((last_used, _)) = self.entries.remove(&block) {
            self.lru.remove(&last_used);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devfs::adapter::step_engine::{page_allocator, NoProgress};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tx_subsystems::device::{
        BlockDevice, BlockDeviceHandle, BlockDeviceOps, BlockDeviceRegistration,
        BlockDurabilityCapabilities, DevT, PhysicalBlockNumber,
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

    #[test]
    fn read_block_cache_invalidates_replayed_home_block() {
        let mut cache = ReadBlockCache::new();
        cache.insert(7, Arc::new([0x5a; BLOCK_SIZE]));

        cache.invalidate(7);

        assert!(cache.get(7).is_none());
    }

    #[test]
    fn read_block_cache_clears_every_entry_after_checkpoint_settlement() {
        let mut cache = ReadBlockCache::new();
        cache.insert(7, Arc::new([0x5a; BLOCK_SIZE]));
        cache.insert(8, Arc::new([0xa5; BLOCK_SIZE]));

        cache.clear();

        assert!(cache.get(7).is_none());
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
            self.outcome()
        }

        fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
            BlockDurabilityCapabilities {
                fua: false,
                flush: true,
            }
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
    static YIELD_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 66),
        name: "ext4-yield-test",
        ops: &YIELD_DEVICE,
    };

    struct ErrorBlockDevice {
        error: Errno,
    }

    impl BlockDeviceOps for ErrorBlockDevice {
        fn read_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::Err(self.error)
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::Err(self.error)
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::Err(self.error)
        }

        fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
            BlockDurabilityCapabilities {
                fua: false,
                flush: true,
            }
        }
    }

    impl BlockDevice for ErrorBlockDevice {
        fn total_blocks(&self) -> u64 {
            64
        }

        fn block_size(&self) -> u32 {
            512
        }
    }

    static READ_ONLY_DEVICE: ErrorBlockDevice = ErrorBlockDevice {
        error: Errno::EROFS,
    };
    static IO_ERROR_DEVICE: ErrorBlockDevice = ErrorBlockDevice { error: Errno::EIO };
    static READ_ONLY_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 67),
        name: "ext4-read-only-test",
        ops: &READ_ONLY_DEVICE,
    };
    static IO_ERROR_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 68),
        name: "ext4-io-error-test",
        ops: &IO_ERROR_DEVICE,
    };

    struct RecordingBlockDevice {
        reads: AtomicUsize,
        last_frames: AtomicUsize,
        last_lba: core::sync::atomic::AtomicU64,
        barriers: AtomicUsize,
    }

    impl RecordingBlockDevice {
        const fn new() -> Self {
            Self {
                reads: AtomicUsize::new(0),
                last_frames: AtomicUsize::new(0),
                last_lba: core::sync::atomic::AtomicU64::new(u64::MAX),
                barriers: AtomicUsize::new(0),
            }
        }

        fn reset(&self) {
            self.reads.store(0, Ordering::Release);
            self.last_frames.store(0, Ordering::Release);
            self.last_lba.store(u64::MAX, Ordering::Release);
            self.barriers.store(0, Ordering::Release);
        }
    }

    impl BlockDeviceOps for RecordingBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            self.last_frames.store(target.len(), Ordering::Release);
            self.last_lba.store(block_id.as_u64(), Ordering::Release);
            let first_ext4_block = block_id.as_u64() / 8;
            for (index, frame) in target.iter().enumerate() {
                let ptr = page_allocator::frame_kernel_addr(frame.ppn())
                    .expect("recording device frame address");
                // SAFETY: the bridge reserved every target frame exclusively
                // for this full-overwrite device read.
                unsafe {
                    core::ptr::write_bytes(
                        ptr,
                        first_ext4_block.wrapping_add(index as u64) as u8,
                        BLOCK_SIZE,
                    );
                }
            }
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            self.barriers.fetch_add(1, Ordering::AcqRel);
            StepOutcome::done(())
        }

        fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
            BlockDurabilityCapabilities {
                fua: false,
                flush: true,
            }
        }
    }

    impl BlockDevice for RecordingBlockDevice {
        fn total_blocks(&self) -> u64 {
            1024
        }

        fn block_size(&self) -> u32 {
            512
        }
    }

    static RECORDING_DEVICE: RecordingBlockDevice = RecordingBlockDevice::new();
    static RECORDING_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 69),
        name: "ext4-recording-test",
        ops: &RECORDING_DEVICE,
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
            BlockDeviceImage::whole(&FILE_IO_REGISTRATION).read_block(0, &mut out),
            Err(Ext4FormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::whole(&YIELD_REGISTRATION).read_block(0, &mut out),
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
            BlockDeviceImage::whole(&FILE_IO_REGISTRATION).write_block(0, &data),
            Err(Ext4FormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::whole(&YIELD_REGISTRATION).write_block(0, &data),
            Err(Ext4FormatError::WouldBlock)
        );
    }

    #[test]
    fn ext4_bridge_delegates_barrier() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        RECORDING_DEVICE.reset();
        let mut image = BlockDeviceImage::whole(&RECORDING_REGISTRATION);

        assert_eq!(image.barrier(), Ok(()));
        assert_eq!(RECORDING_DEVICE.barriers.load(Ordering::Acquire), 1);
    }

    #[test]
    fn ext4_bridge_maps_retrying_barrier_to_would_block() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();

        let mut continuing = BlockDeviceImage::whole(&FILE_IO_REGISTRATION);
        assert_eq!(continuing.barrier(), Err(Ext4FormatError::WouldBlock));
        let mut yielding = BlockDeviceImage::whole(&YIELD_REGISTRATION);
        assert_eq!(yielding.barrier(), Err(Ext4FormatError::WouldBlock));
    }

    #[test]
    fn ext4_bridge_preserves_barrier_device_errors() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();

        let mut read_only = BlockDeviceImage::whole(&READ_ONLY_REGISTRATION);
        assert_eq!(read_only.barrier(), Err(Ext4FormatError::ReadOnly));
        let mut io_error = BlockDeviceImage::whole(&IO_ERROR_REGISTRATION);
        assert_eq!(io_error.barrier(), Err(Ext4FormatError::Io));
    }

    #[test]
    fn regular_data_read_populates_bounded_read_ahead_window() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        RECORDING_DEVICE.reset();
        let image = BlockDeviceImage::whole(&RECORDING_REGISTRATION);
        let mut out = [0u8; BLOCK_SIZE];

        image.read_data_block(4, &mut out).unwrap();
        assert_eq!(out, [4u8; BLOCK_SIZE]);
        assert_eq!(RECORDING_DEVICE.reads.load(Ordering::Acquire), 1);
        assert_eq!(
            RECORDING_DEVICE.last_frames.load(Ordering::Acquire),
            DATA_READ_AHEAD_BLOCKS
        );

        image.read_data_block(5, &mut out).unwrap();
        assert_eq!(out, [5u8; BLOCK_SIZE]);
        assert_eq!(
            RECORDING_DEVICE.reads.load(Ordering::Acquire),
            1,
            "the adjacent block should be served from read-ahead cache"
        );
    }

    #[test]
    fn partition_image_uses_handle_translation_capacity_and_tail_bounds() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        RECORDING_DEVICE.reset();
        let handle = BlockDeviceHandle::partition(&RECORDING_REGISTRATION, 33, 17)
            .expect("unaligned partition lies within parent");
        let image = BlockDeviceImage::new(handle);
        let mut out = [0u8; BLOCK_SIZE];

        assert_eq!(image.total_blocks(), 2);
        assert_eq!(image.handle().start_lba(), 33);
        image.read_block(1, &mut out).expect("last complete block");
        assert_eq!(RECORDING_DEVICE.last_lba.load(Ordering::Acquire), 41);
        assert_eq!(
            image.read_block(2, &mut out),
            Err(Ext4FormatError::InvalidInput),
            "the trailing single sector cannot expose a partial ext4 block"
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
        let image = BlockDeviceImage::new(
            BlockDeviceHandle::partition(&FILE_IO_REGISTRATION, 1, 63).expect("test partition"),
        );
        let binder = Ext4FileIoRuntimeBinder::new(image.handle());

        tx_ext4::mount::FilePageContainerBinder::bind_file_page_container(
            &binder,
            container.clone(),
        );

        let runtimes = tx_subsystems::device::page_container_file_io_service_runtimes_snapshot();
        assert_eq!(runtimes.len(), 1);
        let active_guard = guard();
        assert!(runtimes[0].is_live(&active_guard));
        assert_eq!(
            runtimes[0].handle().registration().devt,
            FILE_IO_REGISTRATION.devt
        );
        assert_eq!(runtimes[0].handle().start_lba(), 1);
        assert_eq!(runtimes[0].handle().len_lba(), 63);
        drop(active_guard);
        drop(container);
        let active_guard = guard();
        assert!(!runtimes[0].is_live(&active_guard));
        drop(active_guard);
        tx_subsystems::device::reset_page_container_file_io_service_registry_for_test();
    }
}
