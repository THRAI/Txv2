//! Adapter from `tx_subsystems::device::BlockDevice` to `tx_ext4_format::BlockImage`.
//!
//! `Ext4Pager` needs a `BlockImage`: it reads/writes 4 KiB ext4 blocks. The
//! kernel block-device registry exposes `BlockDevice` / `BlockDeviceOps`:
//! sector-LBA addressed, page-frame DMA targets, EBR-guarded. This module
//! bridges the two by allocating a transient frame, issuing a DMA operation,
//! and copying between the frame and the caller's `[u8; 4096]` buffer.

use alloc::{sync::Arc, vec::Vec};
use core::ptr::NonNull;

use crate::devfs::adapter::step_engine::{
    borrow_current_guard, guard, page_allocator, SpinMutex, StepOutcome, ZeroPolicy,
};
use tx_ext4::planner::Ext4BlockGeometry;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::{Ext4FormatError, Result};
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
        // A runtime is one reactor task per inode. Small files cannot trigger
        // the page cache's fault readahead window, so registering them only
        // adds clone/exit/runqueue pressure to compiler workloads. They keep
        // the synchronous pager path; large files receive the parallel read
        // planner and bounded readahead service.
        if container.page_count() < tx_subsystems::page_backed::FILE_READAHEAD_TRIGGER_PAGES {
            return;
        }
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
        let blocks = self.read_blocks_uncached(block, 1)?;
        out.copy_from_slice(blocks[0].as_ref());
        Ok(())
    }

    fn read_blocks_uncached(&self, first_block: u64, count: usize) -> Result<Vec<Arc<Page4K>>> {
        if count == 0 {
            return Err(Ext4FormatError::OutOfBounds);
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
                // Keep the one-block path as a correctness fallback when a
                // contiguous multi-frame reservation is temporarily unavailable.
                self.read_block(block, out)
            }
            Err(error) => Err(error),
        }
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()> {
        self.write_blocks(block, core::slice::from_ref(&data))
    }

    fn write_blocks(&mut self, first_block: u64, data: &[&Page4K]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let spb = self
            .sectors_per_ext4_block()
            .ok_or(Ext4FormatError::Unsupported)?;
        let lba = first_block
            .checked_mul(spb)
            .ok_or(Ext4FormatError::OutOfBounds)?;

        let reservation =
            page_allocator::reserve_run(data.len(), 1, ZeroPolicy::UninitFullOverwrite)
                .map_err(|_| Ext4FormatError::Truncated)?;
        let run = reservation.commit();
        let base = run.base();
        let mut frames = Vec::with_capacity(data.len());
        for (index, page) in data.iter().enumerate() {
            let mut ppn = base;
            ppn.0 = ppn
                .0
                .checked_add(index)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            let dst =
                page_allocator::frame_kernel_addr(ppn).map_err(|_| Ext4FormatError::Truncated)?;
            let dst_nn = NonNull::new(dst).ok_or(Ext4FormatError::Truncated)?;
            // SAFETY: every destination is one distinct frame in the live
            // contiguous run and every source references one complete ext4
            // block for the duration of this synchronous submission.
            unsafe {
                core::ptr::copy_nonoverlapping(page.as_ptr(), dst_nn.as_ptr(), BLOCK_SIZE);
            }
            frames.push(Frame::new(ppn));
        }

        let guard = borrow_current_guard().unwrap_or_else(guard);
        let outcome = self
            .device
            .write_blocks(PhysicalBlockNumber::new(lba), &frames, &guard);
        drop(guard);
        drop(run);
        match outcome {
            StepOutcome::Done(()) => {
                let mut cache = self.cache.lock();
                for (index, page) in data.iter().enumerate() {
                    let block = first_block
                        .checked_add(index as u64)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    cache.insert(block, Arc::new(**page));
                }
                Ok(())
            }
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(Ext4FormatError::WouldBlock)
            }
            StepOutcome::Err(_) => Err(Ext4FormatError::Truncated),
        }
    }

    fn barrier(&mut self) -> Result<()> {
        let guard = borrow_current_guard().unwrap_or_else(guard);
        let outcome = self.device.barrier(&guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(Ext4FormatError::WouldBlock)
            }
            StepOutcome::Err(_) => Err(Ext4FormatError::Truncated),
        }
    }

    fn invalidate_block(&mut self, block: u64) {
        self.cache.lock().invalidate(block);
    }

    fn invalidate_all(&mut self) {
        self.cache.lock().clear();
    }
}

// 4096 × 4 KiB = 16 MiB. 128 entries (512 KiB) thrashed on every exec: the
// sdcard busybox alone is ~1.4 MiB (~350 ext4 blocks), so each shell command
// re-read most of the binary through virtio (~5-10 ms/block under TCG ≈
// seconds per command) — the dominant cost of every LTP shell test. The hot
// set (busybox + ash scripts + libc + common test binaries) fits in a few MiB.
const READ_BLOCK_CACHE_ENTRIES: usize = 4096;
const READ_BLOCK_CACHE_WAYS: usize = 8;
const READ_BLOCK_CACHE_SETS: usize = READ_BLOCK_CACHE_ENTRIES / READ_BLOCK_CACHE_WAYS;
const DATA_READ_AHEAD_BLOCKS: usize = 8;

struct ReadBlockCache {
    clock: u64,
    // Eight-way set associativity bounds every hit to eight probes. The old
    // pair of BTreeMaps performed a tree lookup, an LRU-tree removal and an
    // LRU-tree insertion on every cached 4 KiB read while holding one lock.
    entries: Vec<ReadBlockCacheEntry>,
}

struct ReadBlockCacheEntry {
    block: u64,
    last_used: u64,
    data: Option<Arc<Page4K>>,
}

impl ReadBlockCacheEntry {
    const fn empty() -> Self {
        Self {
            block: 0,
            last_used: 0,
            data: None,
        }
    }
}

impl ReadBlockCache {
    fn new() -> Self {
        Self {
            clock: 0,
            entries: (0..READ_BLOCK_CACHE_ENTRIES)
                .map(|_| ReadBlockCacheEntry::empty())
                .collect(),
        }
    }

    fn get(&mut self, block: u64) -> Option<Arc<Page4K>> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let index = read_block_cache_set(block).find(|index| {
            let entry = &self.entries[*index];
            entry.data.is_some() && entry.block == block
        })?;
        let entry = &mut self.entries[index];
        entry.last_used = clock;
        entry.data.clone()
    }

    fn insert(&mut self, block: u64, data: Arc<Page4K>) {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let range = read_block_cache_set(block);
        let victim = range
            .clone()
            .find(|index| {
                let entry = &self.entries[*index];
                entry.data.is_none() || entry.block == block
            })
            .unwrap_or_else(|| {
                range
                    .min_by_key(|index| self.entries[*index].last_used)
                    .unwrap_or(0)
            });
        self.entries[victim] = ReadBlockCacheEntry {
            block,
            last_used: clock,
            data: Some(data),
        };
    }

    fn invalidate(&mut self, block: u64) {
        for index in read_block_cache_set(block) {
            let entry = &mut self.entries[index];
            if entry.data.is_some() && entry.block == block {
                entry.data = None;
                return;
            }
        }
    }

    fn clear(&mut self) {
        for entry in &mut self.entries {
            entry.data = None;
        }
    }
}

fn read_block_cache_set(block: u64) -> core::ops::Range<usize> {
    let mixed = block.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (block >> 23);
    let first = mixed as usize % READ_BLOCK_CACHE_SETS * READ_BLOCK_CACHE_WAYS;
    first..first + READ_BLOCK_CACHE_WAYS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devfs::adapter::step_engine::{guard, page_allocator, NoProgress};
    use core::sync::atomic::{AtomicUsize, Ordering};
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

    struct ReadAheadBlockDevice;

    static READ_AHEAD_CALLS: AtomicUsize = AtomicUsize::new(0);
    static READ_AHEAD_FRAMES: AtomicUsize = AtomicUsize::new(0);
    static BATCH_WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BATCH_WRITE_FRAMES: AtomicUsize = AtomicUsize::new(0);
    static BATCH_WRITE_LBA: AtomicUsize = AtomicUsize::new(0);

    impl BlockDeviceOps for ReadAheadBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            READ_AHEAD_CALLS.fetch_add(1, Ordering::Relaxed);
            READ_AHEAD_FRAMES.store(target.len(), Ordering::Relaxed);
            for (index, frame) in target.iter().enumerate() {
                let address = page_allocator::frame_kernel_addr(frame.ppn())
                    .expect("read-ahead test frame mapping");
                let value = block_id.as_u64().wrapping_add((index * 8) as u64) as u8;
                // SAFETY: every frame belongs to the bridge's live contiguous
                // allocation and exposes one complete 4 KiB DMA destination.
                unsafe { core::ptr::write_bytes(address, value, BLOCK_SIZE) };
            }
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            BATCH_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
            BATCH_WRITE_FRAMES.store(source.len(), Ordering::Relaxed);
            BATCH_WRITE_LBA.store(block_id.as_u64() as usize, Ordering::Relaxed);
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl BlockDevice for ReadAheadBlockDevice {
        fn total_blocks(&self) -> u64 {
            1024
        }

        fn block_size(&self) -> u32 {
            512
        }
    }

    static READ_AHEAD_DEVICE: ReadAheadBlockDevice = ReadAheadBlockDevice;
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
    fn ext4_data_read_prefetches_eight_blocks_with_one_device_request() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        READ_AHEAD_CALLS.store(0, Ordering::Relaxed);
        READ_AHEAD_FRAMES.store(0, Ordering::Relaxed);
        let image = BlockDeviceImage::new(&READ_AHEAD_DEVICE);
        let mut first = [0u8; BLOCK_SIZE];
        let mut second = [0u8; BLOCK_SIZE];

        image
            .read_data_block(4, &mut first)
            .expect("first prefetched data block");
        image
            .read_data_block(5, &mut second)
            .expect("adjacent block cache hit");

        assert_eq!(READ_AHEAD_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(READ_AHEAD_FRAMES.load(Ordering::Relaxed), 8);
        assert_eq!(first[0], 32);
        assert_eq!(second[0], 40);
    }

    #[test]
    fn ext4_data_write_submits_one_contiguous_multi_frame_request() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        BATCH_WRITE_CALLS.store(0, Ordering::Relaxed);
        BATCH_WRITE_FRAMES.store(0, Ordering::Relaxed);
        BATCH_WRITE_LBA.store(0, Ordering::Relaxed);
        let mut image = BlockDeviceImage::new(&READ_AHEAD_DEVICE);
        let first = [0x31u8; BLOCK_SIZE];
        let second = [0x32u8; BLOCK_SIZE];

        image
            .write_blocks(4, &[&first, &second])
            .expect("contiguous ext4 data write");

        assert_eq!(BATCH_WRITE_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(BATCH_WRITE_FRAMES.load(Ordering::Relaxed), 2);
        assert_eq!(BATCH_WRITE_LBA.load(Ordering::Relaxed), 32);
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
            tx_subsystems::page_backed::FILE_READAHEAD_TRIGGER_PAGES,
        )
        .expect("page container cap");
        let binder = Ext4FileIoRuntimeBinder::new(BlockDeviceHandle::whole(&FILE_IO_REGISTRATION));

        tx_ext4::mount::FilePageContainerBinder::bind_file_page_container(
            &binder,
            container.clone(),
        );

        let runtimes = tx_subsystems::device::page_container_file_io_service_runtimes_snapshot();
        assert_eq!(runtimes.len(), 1);
        assert!(runtimes[0].is_live(&guard()));
        assert_eq!(
            runtimes[0].handle().registration().devt,
            FILE_IO_REGISTRATION.devt
        );
        drop(container);
        let stale = tx_subsystems::device::page_container_file_io_service_runtimes_snapshot();
        assert_eq!(stale.len(), 1);
        assert!(!stale[0].is_live(&guard()));
        tx_subsystems::device::reset_page_container_file_io_service_registry_for_test();
    }
}
