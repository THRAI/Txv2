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
use tx_ext4::planner::Ext4BlockGeometry;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::{Ext4FormatError, Result};
use tx_subsystems::device::{self, BlockDevice, BlockDeviceHandle, PhysicalBlockNumber};
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::page_backed::{Frame, PageContainer};

/// Bind ext4 file-page containers to the exact registered block-device
/// handle. This preserves main's layered-device identity while leaving the
/// final-smp read-ahead implementation below intact.
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

    pub fn block_geometry(&self, device: DeviceKey) -> Option<Ext4BlockGeometry> {
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
        let guard = epoch::borrow_current_guard().unwrap_or_else(epoch::guard);
        let outcome = self
            .device
            .read_blocks(PhysicalBlockNumber::new(lba), &mut frames, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return Err(Ext4FormatError::WouldBlock);
            }
            StepOutcome::Err(_) => return Err(Ext4FormatError::Truncated),
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
            Err(_) if count > 1 => {
                // Fragmentation can make the bounded contiguous reservation
                // fail even though a single frame remains available.  Preserve
                // the original one-page behavior as the correctness fallback.
                self.read_block(block, out)
            }
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
const DATA_READ_AHEAD_BLOCKS: usize = 8;

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
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tx_subsystems::device::{BlockDeviceOps, PhysicalBlockNumber};
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

    struct RecordingBlockDevice {
        reads: AtomicUsize,
        last_frames: AtomicUsize,
    }

    impl RecordingBlockDevice {
        const fn new() -> Self {
            Self {
                reads: AtomicUsize::new(0),
                last_frames: AtomicUsize::new(0),
            }
        }

        fn reset(&self) {
            self.reads.store(0, Ordering::Release);
            self.last_frames.store(0, Ordering::Release);
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
            StepOutcome::done(())
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
    fn regular_data_read_populates_bounded_read_ahead_window() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        RECORDING_DEVICE.reset();
        let image = BlockDeviceImage::new(&RECORDING_DEVICE);
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
}
