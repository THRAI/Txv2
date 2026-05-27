//! FatFsInstance: wraps FatPager with locking, mount-pin registration,
//! and FsObjectId ↔ cluster conversions.
//!
//! Mirrors `tx-ext4::read_backend::Ext4FsInstance`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::step_engine::{Cap, PayloadCap, SpinMutex};
use alloc::sync::Arc;
use tx_fat_format::pager::{BlockImage, DirEntryLite, FatFormatError, FatPager};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{MountPayload, MountPayloadPin};
use tx_subsystems::vfs::structure::{DirCursor, FsObjectId, InodeMeta, Timespec};

/// FAT root directory has no cluster (FAT12/16) or cluster 0 as sentinel.
/// We use this cluster number for the FAT12/16 root dir in FsObjectId.
pub(crate) const FAT_ROOT_CLUSTER_SENTINEL: u32 = 0xFFFF_FFFF;

/// Capacity of the dirent cache.
const CACHE_CAPACITY: usize = 64;

/// Compact metadata cached after lookup/readdir so `load_inode_meta`
/// can return accurate size, timestamps, and mode.
///
/// For non-root entries, `parent_cluster` and `entry_index` identify
/// the 32-byte dirent slot in the parent directory so that
/// `serialize_inode_meta` and `truncate` can update the on-disk
/// directory entry.  The root directory's cached entry has
/// `parent_cluster == FAT_ROOT_CLUSTER_SENTINEL`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedDirent {
    pub first_cluster: u32,
    pub attr: u8,
    pub size: u32,
    pub write_date: u16,
    pub write_time: u16,
    /// Parent directory cluster (FAT_ROOT_CLUSTER_SENTINEL for root).
    pub parent_cluster: u32,
    /// Index of this entry in the parent directory's dirent list.
    pub entry_index: u32,
}

/// Simple linear-probe cache mapping `FsObjectId → CachedDirent`.
pub(crate) struct LookupCache {
    slots: [(FsObjectId, CachedDirent); CACHE_CAPACITY],
    // Generation counter so we can distinguish stale slots from
    // fresh ones without needing an `Option` discriminant.
    gen: u64,
}

impl LookupCache {
    const fn new() -> Self {
        Self {
            slots: [(
                FsObjectId::new(0),
                CachedDirent {
                    first_cluster: 0,
                    attr: 0,
                    size: 0,
                    write_date: 0,
                    write_time: 0,
                    parent_cluster: FAT_ROOT_CLUSTER_SENTINEL,
                    entry_index: 0,
                },
            ); CACHE_CAPACITY],
            gen: 0,
        }
    }

    pub(crate) fn insert(
        &mut self,
        key: FsObjectId,
        entry: &DirEntryLite,
        parent_cluster: u32,
        entry_index: u32,
    ) {
        // Increment generation so every slot has a unique tombstone.
        self.gen = self.gen.wrapping_add(1);
        // When cache overflows, generation wrapping means all slots
        // look "old"; that's fine — we just overwrite starting at 0.
        let idx = (key.as_u64() as usize) % CACHE_CAPACITY;
        self.slots[idx] = (
            key,
            CachedDirent {
                first_cluster: entry.first_cluster,
                attr: entry.attr,
                size: entry.size,
                write_date: entry.write_date,
                write_time: entry.write_time,
                parent_cluster,
                entry_index,
            },
        );
    }

    pub(crate) fn get(&self, key: FsObjectId) -> Option<CachedDirent> {
        let idx = (key.as_u64() as usize) % CACHE_CAPACITY;
        let (cached_key, dirent) = &self.slots[idx];
        if *cached_key == key {
            // We don't compare generations because a matching key
            // means the slot was written for this key.  Overlap from
            // gen-wraparound is astronomically unlikely and would
            // only cause a stale-but-harmless cache hit.
            Some(*dirent)
        } else {
            None
        }
    }
}

pub(crate) struct FatFsInstance<I> {
    pager: FatPagerCell<I>,
    pub(crate) mount_pin: SpinMutex<Option<MountPayloadPin>>,
    read_only: AtomicBool,
    /// Cache of recently looked-up directory entries so that
    /// `load_inode_meta` can return accurate metadata without
    /// re-reading the parent directory.
    pub(crate) dirent_cache: SpinMutex<LookupCache>,
}

impl<I: BlockImage> FatFsInstance<I> {
    pub(crate) fn open(image: I, read_only: bool) -> Result<Arc<Self>, Errno> {
        let pager = FatPager::open(image).map_err(map_format_error)?;
        Ok(Arc::new(Self {
            pager: FatPagerCell::new(pager),
            mount_pin: SpinMutex::new(None),
            read_only: AtomicBool::new(read_only),
            dirent_cache: SpinMutex::new(LookupCache::new()),
        }))
    }

    pub(crate) fn bind_mount_payload(&self, payload: &Cap<MountPayload>) {
        let payload = PayloadCap::from_cap(payload.clone());
        *self.mount_pin.lock() = Some(MountPayloadPin::acquire(&payload));
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    pub(crate) fn with_pager<T>(
        &self,
        f: impl FnOnce(&mut FatPager<I>) -> core::result::Result<T, FatFormatError>,
    ) -> core::result::Result<T, Errno> {
        let mut pager = self.pager.lock();
        f(&mut pager).map_err(map_format_error)
    }
}

// Spin-locked FatPager cell — same pattern as Ext4PagerCell.
struct FatPagerCell<I> {
    locked: AtomicBool,
    pager: UnsafeCell<FatPager<I>>,
}

unsafe impl<I: Send> Sync for FatPagerCell<I> {}

impl<I> FatPagerCell<I> {
    fn new(pager: FatPager<I>) -> Self {
        Self {
            locked: AtomicBool::new(false),
            pager: UnsafeCell::new(pager),
        }
    }

    fn lock(&self) -> FatPagerGuard<'_, I> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        FatPagerGuard { cell: self }
    }
}

struct FatPagerGuard<'a, I> {
    cell: &'a FatPagerCell<I>,
}

impl<I> Deref for FatPagerGuard<'_, I> {
    type Target = FatPager<I>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.cell.pager.get() }
    }
}

impl<I> DerefMut for FatPagerGuard<'_, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.cell.pager.get() }
    }
}

impl<I> Drop for FatPagerGuard<'_, I> {
    fn drop(&mut self) {
        self.cell.locked.store(false, Ordering::Release);
    }
}

// ====================================================================
// FsObjectId encoding
// ====================================================================

/// Encode a (cluster, entry_offset) pair into an FsObjectId.
///
/// High 32 bits = cluster number (FAT_ROOT_CLUSTER_SENTINEL for FAT12/16 root).
/// Low 32 bits = entry offset within the directory cluster (for lookup disambiguation).
pub(crate) fn fs_object_id(cluster: u32, entry_offset: u32) -> FsObjectId {
    let combined: u64 = ((cluster as u64) << 32) | (entry_offset as u64);
    FsObjectId::new(combined)
}

/// Decode the cluster number from an FsObjectId.
pub(crate) fn cluster_from_fs_id(fs_id: FsObjectId) -> u32 {
    (fs_id.as_u64() >> 32) as u32
}

/// Whether this FsObjectId represents the FAT12/16 root directory.
pub(crate) fn is_fat_root(fs_id: FsObjectId) -> bool {
    cluster_from_fs_id(fs_id) == FAT_ROOT_CLUSTER_SENTINEL
}

// ====================================================================
// InodeMeta mapping
// ====================================================================

/// Map a `CachedDirent` into an `InodeMeta`.
pub(crate) fn map_cached_meta(dirent: &CachedDirent) -> InodeMeta {
    use tx_fat_format::ondisk::{decode_date, decode_time, ATTR_DIRECTORY};

    let is_dir = dirent.attr & ATTR_DIRECTORY != 0;
    let mode = if is_dir {
        0o555 | 0o040000
    } else {
        0o444 | 0o100000
    };
    let (year, month, day) = decode_date(dirent.write_date);
    let (hours, minutes, seconds) = decode_time(dirent.write_time);
    let mtime_sec = date_to_unix(year, month, day, hours, minutes, seconds);

    InodeMeta {
        mode,
        uid: 0,
        gid: 0,
        size: dirent.size as u64,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec {
            sec: mtime_sec,
            nsec: 0,
        },
        ctime: Timespec {
            sec: mtime_sec,
            nsec: 0,
        },
        nlinks: 1,
        blocks: (dirent.size as u64).div_ceil(512),
        flags: 0,
    }
}

/// Convert a FAT date/time to Unix epoch seconds.
fn date_to_unix(year: u16, month: u8, day: u8, hours: u8, minutes: u8, seconds: u8) -> i64 {
    // Days in months for a regular year
    const DAYS_IN_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    fn is_leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
    }

    let y = year as i64;
    let m = month as i64;
    let d = day as i64;

    // Days from 1970 to year-01-01
    let mut days: i64 = 0;
    for yr in 1970..y {
        days += if is_leap(yr) { 366 } else { 365 };
    }

    // Days from year-01-01 to year-month-01
    for mon in 1..m {
        let mut dim = DAYS_IN_MONTH[(mon - 1) as usize];
        if mon == 2 && is_leap(y) {
            dim += 1;
        }
        days += dim;
    }

    days += d - 1;

    days * 86400 + (hours as i64) * 3600 + (minutes as i64) * 60 + (seconds as i64)
}

// ====================================================================
// DirCursor encoding
// ====================================================================

/// DirCursor encodes (cluster, entry_index) for `readdir` resumption.
pub(crate) fn cursor_from_cluster_index(cluster: u32, index: usize) -> DirCursor {
    let mut raw = [0u8; 16];
    raw[0..4].copy_from_slice(&cluster.to_le_bytes());
    raw[4..12].copy_from_slice(&(index as u64).to_le_bytes());
    DirCursor(raw)
}

// ====================================================================
// Error mapping
// ====================================================================

pub(crate) fn map_format_error(err: FatFormatError) -> Errno {
    match err {
        FatFormatError::BadBPB(_) | FatFormatError::Corrupt | FatFormatError::IO => Errno::EIO,
        FatFormatError::OutOfBounds => Errno::EIO,
        FatFormatError::Unsupported => Errno::ENOSYS,
        FatFormatError::FileTooLarge => Errno::EIO,
        FatFormatError::NotFound => Errno::ENOENT,
    }
}

// ====================================================================
// Unix timestamp → FAT date/time
// ====================================================================

/// Convert a Unix epoch second to a FAT date (u16).
pub(crate) fn unix_to_fat_date(unix_sec: i64) -> u16 {
    if unix_sec <= 0 {
        return 0x21; // 1980-01-01: year=0, month=1, day=1 → (0<<9)|(1<<5)|1
    }

    // Days since 1970-01-01
    let days = unix_sec / 86400;
    let mut year: i64 = 1970;
    let mut remaining = days;

    fn is_leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
    }

    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if remaining < year_days {
            break;
        }
        remaining -= year_days;
        year += 1;
    }

    if year < 1980 {
        return 0x21; // clamp to 1980-01-01
    }
    if year > 2107 {
        return 0xFF9F; // clamp to 2107-12-31: year=127, month=12, day=31
    }

    const DAYS_IN_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month: i64 = 1;
    for (i, &dim) in DAYS_IN_MONTH.iter().enumerate() {
        let mut dim = dim;
        if i == 1 && is_leap(year) {
            dim += 1;
        }
        if remaining < dim {
            month = i as i64 + 1;
            break;
        }
        remaining -= dim;
    }

    let day = remaining + 1;
    ((year - 1980) as u16) << 9 | (month as u16) << 5 | (day as u16)
}

/// Convert a Unix epoch second to a FAT time (u16, 2-second resolution).
pub(crate) fn unix_to_fat_time(unix_sec: i64) -> u16 {
    if unix_sec < 0 {
        return 0;
    }
    let seconds_of_day = unix_sec % 86400;
    let hours = (seconds_of_day / 3600) as u16;
    let minutes = ((seconds_of_day % 3600) / 60) as u16;
    let seconds = ((seconds_of_day % 60) / 2) as u16; // 2-second granularity
    hours << 11 | minutes << 5 | seconds
}
