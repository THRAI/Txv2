use core::cell::UnsafeCell;
use core::mem::{align_of, size_of, MaybeUninit};
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use tx_hal::{
    BootInfo, BootstrapPmapInfo, MemoryRegionKind, MmioRegion, PhysAddr, PhysRange,
    PmapPermissions, PmapReserveKind, Ppn, TxPlatform, VirtAddr, VirtRange,
};

use crate::page_allocator::{
    claim_permanent_frame, claim_zero_frame, install_bitmap_allocator, install_frame_copier,
    install_frame_kernel_addr, AllocError, BitmapPageAllocator, FrameMeta,
};

pub const PAGE_SIZE_4K: usize = 4096;
const INIT_UNSTARTED: u8 = 0;
const INIT_RUNNING: u8 = 1;
const INIT_DONE: u8 = 2;
const MAX_BOOT_SPANS: usize = 32;

static INIT_STATE: AtomicU8 = AtomicU8::new(INIT_UNSTARTED);
static BOOT_ALLOCATOR: BootAllocatorCell =
    BootAllocatorCell(UnsafeCell::new(MaybeUninit::uninit()));

struct BootAllocatorCell(UnsafeCell<MaybeUninit<BitmapPageAllocator<'static>>>);

unsafe impl Sync for BootAllocatorCell {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootMemoryError {
    InvalidPageSize,
    RegionOverflow,
    TooManyRegions,
    NoRam,
    NoUsableMemory,
    LayoutOverflow,
    MetadataCarveFailed,
    DirectMapTooSmall,
    PmapExtensionFailed,
    InvalidMmioRegion,
    PmapMappingFailed,
    PmapPtAllocatorInstallFailed,
    PermanentAnchorFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootPageSpan {
    pub base: Ppn,
    pub count: usize,
}

impl BootPageSpan {
    const fn empty() -> Self {
        Self {
            base: Ppn(0),
            count: 0,
        }
    }

    fn end_ppn(self) -> Result<usize, BootMemoryError> {
        self.base
            .0
            .checked_add(self.count)
            .ok_or(BootMemoryError::RegionOverflow)
    }

    fn phys_range(self, page_size: usize) -> Result<PhysRange, BootMemoryError> {
        let start = self
            .base
            .0
            .checked_mul(page_size)
            .ok_or(BootMemoryError::RegionOverflow)?;
        let size = self
            .count
            .checked_mul(page_size)
            .ok_or(BootMemoryError::RegionOverflow)?;
        Ok(PhysRange {
            start: PhysAddr(start),
            size,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootMemoryPlan {
    pub base_ppn: Ppn,
    pub frame_count: usize,
    pub frame_meta: PhysRange,
    pub bitmap: PhysRange,
    pub metadata_storage: PhysRange,
    bitmap_words: usize,
    free_spans: [BootPageSpan; MAX_BOOT_SPANS],
    free_len: usize,
}

impl BootMemoryPlan {
    pub fn free_spans(&self) -> &[BootPageSpan] {
        &self.free_spans[..self.free_len]
    }

    fn bitmap_word_count(&self) -> usize {
        self.bitmap_words
    }
}

#[derive(Clone, Copy)]
struct SpanSet {
    spans: [BootPageSpan; MAX_BOOT_SPANS],
    len: usize,
}

impl SpanSet {
    const fn new() -> Self {
        Self {
            spans: [BootPageSpan::empty(); MAX_BOOT_SPANS],
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn as_slice(&self) -> &[BootPageSpan] {
        &self.spans[..self.len]
    }

    fn push(&mut self, span: BootPageSpan) -> Result<(), BootMemoryError> {
        if span.count == 0 {
            return Ok(());
        }
        if self.len == self.spans.len() {
            return Err(BootMemoryError::TooManyRegions);
        }

        self.spans[self.len] = span;
        self.len += 1;
        Ok(())
    }

    fn sort_and_merge(&mut self) -> Result<(), BootMemoryError> {
        let mut index = 1usize;
        while index < self.len {
            let mut cursor = index;
            while cursor > 0 && self.spans[cursor - 1].base.0 > self.spans[cursor].base.0 {
                self.spans.swap(cursor - 1, cursor);
                cursor -= 1;
            }
            index += 1;
        }

        if self.len <= 1 {
            return Ok(());
        }

        let mut merged = SpanSet::new();
        let mut current = self.spans[0];
        for index in 1..self.len {
            let next = self.spans[index];
            let current_end = current.end_ppn()?;
            let next_end = next.end_ppn()?;
            if next.base.0 <= current_end {
                current.count = current.count.max(next_end - current.base.0);
            } else {
                merged.push(current)?;
                current = next;
            }
        }
        merged.push(current)?;
        *self = merged;
        Ok(())
    }

    fn subtract_all(&mut self, reserved: &SpanSet) -> Result<(), BootMemoryError> {
        for index in 0..reserved.len {
            self.subtract_one(reserved.spans[index])?;
        }
        Ok(())
    }

    fn subtract_one(&mut self, reserved: BootPageSpan) -> Result<(), BootMemoryError> {
        if reserved.count == 0 || self.len == 0 {
            return Ok(());
        }

        let old = *self;
        let reserved_start = reserved.base.0;
        let reserved_end = reserved.end_ppn()?;
        *self = SpanSet::new();

        for index in 0..old.len {
            let span = old.spans[index];
            let span_start = span.base.0;
            let span_end = span.end_ppn()?;
            if span_end <= reserved_start || span_start >= reserved_end {
                self.push(span)?;
                continue;
            }

            if span_start < reserved_start {
                self.push(BootPageSpan {
                    base: Ppn(span_start),
                    count: reserved_start - span_start,
                })?;
            }

            if reserved_end < span_end {
                self.push(BootPageSpan {
                    base: Ppn(reserved_end),
                    count: span_end - reserved_end,
                })?;
            }
        }

        Ok(())
    }
}

struct MetadataLayout {
    frame_meta: PhysRange,
    bitmap: PhysRange,
    storage: PhysRange,
    bitmap_words: usize,
}

pub fn build_boot_memory_plan(
    boot_info: &BootInfo,
    pmap_info: &BootstrapPmapInfo,
    page_size: usize,
) -> Result<BootMemoryPlan, BootMemoryError> {
    validate_page_size(page_size)?;

    let mut ram = SpanSet::new();
    let mut usable = SpanSet::new();
    let mut reserved = SpanSet::new();

    for region in boot_info.memory_regions {
        if let Some(span) = covering_span(region.base, region.size, page_size)? {
            ram.push(span)?;
            if region.kind == MemoryRegionKind::Reserved {
                reserved.push(span)?;
            }
        }

        if region.kind == MemoryRegionKind::Usable {
            if let Some(span) = contained_span(region.base, region.size, page_size)? {
                usable.push(span)?;
            }
        }
    }

    ram.sort_and_merge()?;
    usable.sort_and_merge()?;
    reserved.sort_and_merge()?;

    if ram.is_empty() {
        return Err(BootMemoryError::NoRam);
    }
    if usable.is_empty() {
        return Err(BootMemoryError::NoUsableMemory);
    }

    push_reserved_range(&mut reserved, boot_info.kernel_image, page_size)?;
    if let Some(initrd) = boot_info.initrd {
        push_reserved_range(&mut reserved, initrd, page_size)?;
    }
    for range in pmap_info.reserved_page_tables {
        push_reserved_range(&mut reserved, *range, page_size)?;
    }
    reserved.sort_and_merge()?;

    let base_ppn = ram.as_slice()[0].base;
    let max_ppn = ram
        .as_slice()
        .iter()
        .try_fold(base_ppn.0, |max_ppn, span| {
            Ok::<_, BootMemoryError>(max_ppn.max(span.end_ppn()?))
        })?;
    let frame_count = max_ppn
        .checked_sub(base_ppn.0)
        .ok_or(BootMemoryError::RegionOverflow)?;

    let mut candidate_free = usable;
    candidate_free.subtract_all(&reserved)?;
    let layout = choose_metadata_layout(&candidate_free, frame_count, page_size)?;

    push_reserved_range(&mut reserved, layout.storage, page_size)?;
    reserved.sort_and_merge()?;

    let mut final_free = usable;
    final_free.subtract_all(&reserved)?;

    validate_direct_map(
        pmap_info.direct_map_base.0,
        pmap_info.direct_map,
        layout.storage,
    )?;
    for span in final_free.as_slice() {
        validate_direct_map(
            pmap_info.direct_map_base.0,
            pmap_info.direct_map,
            span.phys_range(page_size)?,
        )?;
    }

    let mut free_spans = [BootPageSpan::empty(); MAX_BOOT_SPANS];
    free_spans[..final_free.len].copy_from_slice(final_free.as_slice());

    Ok(BootMemoryPlan {
        base_ppn,
        frame_count,
        frame_meta: layout.frame_meta,
        bitmap: layout.bitmap,
        metadata_storage: layout.storage,
        bitmap_words: layout.bitmap_words,
        free_spans,
        free_len: final_free.len,
    })
}

pub fn init_from_hal<P: TxPlatform>() {
    let boot_info = P::boot_info();
    let mut pmap_info = expect_bootstrap_pmap_info::<P>();

    let direct_map_end = required_direct_map_end(boot_info, P::PAGE_SIZE)
        .expect("tx_substrate::init direct-map coverage calculation failed");
    if !direct_map_covers_phys_end(pmap_info, direct_map_end)
        .expect("tx_substrate::init direct-map coverage check failed")
    {
        P::extend_direct_map(direct_map_end)
            .map_err(|_| BootMemoryError::PmapExtensionFailed)
            .expect("tx_substrate::init direct-map extension failed");
        pmap_info = expect_bootstrap_pmap_info::<P>();
    }

    if pmap_info.direct_map_base != P::DIRECT_MAP_BASE {
        panic!("tx_substrate::init direct-map base mismatch");
    }
    map_platform_mmio::<P>().expect("tx_substrate::init MMIO mapping failed");

    let plan = build_boot_memory_plan(boot_info, pmap_info, P::PAGE_SIZE)
        .expect("tx_substrate::init boot-memory plan failed");

    INIT_STATE
        .compare_exchange(
            INIT_UNSTARTED,
            INIT_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .expect("tx_substrate::init called more than once");

    unsafe {
        install_allocator_from_plan::<P>(&plan)
            .expect("tx_substrate::init allocator install failed");
    }
    anchor_boot_permanent_frames(boot_info, pmap_info, &plan, P::PAGE_SIZE)
        .expect("tx_substrate::init permanent-frame anchors failed");
    claim_zero_frame().expect("tx_substrate::init zero-frame claim failed");
    P::install_pt_node_allocator(crate::page_allocator::reserve_page_table_node)
        .map_err(|_| BootMemoryError::PmapPtAllocatorInstallFailed)
        .expect("tx_substrate::init pmap PT-node allocator install failed");

    INIT_STATE.store(INIT_DONE, Ordering::Release);
}

fn anchor_boot_permanent_frames(
    boot_info: &BootInfo,
    pmap_info: &BootstrapPmapInfo,
    plan: &BootMemoryPlan,
    page_size: usize,
) -> Result<(), BootMemoryError> {
    claim_permanent_range(plan, plan.metadata_storage, page_size)?;
    claim_permanent_range(plan, boot_info.kernel_image, page_size)?;
    for range in pmap_info.reserved_page_tables {
        claim_permanent_range(plan, *range, page_size)?;
    }
    Ok(())
}

fn claim_permanent_range(
    plan: &BootMemoryPlan,
    range: PhysRange,
    page_size: usize,
) -> Result<(), BootMemoryError> {
    if range.size == 0 {
        return Ok(());
    }

    let start = align_down(range.start.0, page_size) / page_size;
    let end = align_up(range.end().0, page_size)? / page_size;
    let coverage_start = plan.base_ppn.0;
    let coverage_end = plan
        .base_ppn
        .0
        .checked_add(plan.frame_count)
        .ok_or(BootMemoryError::RegionOverflow)?;

    let mut ppn = start.max(coverage_start);
    let end = end.min(coverage_end);
    while ppn < end {
        match claim_permanent_frame(Ppn(ppn)) {
            Ok(_anchor) => {}
            Err(AllocError::InvalidRequest) => {}
            Err(_) => return Err(BootMemoryError::PermanentAnchorFailed),
        }
        ppn = ppn.checked_add(1).ok_or(BootMemoryError::RegionOverflow)?;
    }
    Ok(())
}

fn expect_bootstrap_pmap_info<P: TxPlatform>() -> &'static BootstrapPmapInfo {
    let Some(pmap_info) = P::bootstrap_pmap_info() else {
        panic!("tx_substrate::init requires bootstrap pmap info");
    };
    pmap_info
}

pub fn required_direct_map_end(
    boot_info: &BootInfo,
    page_size: usize,
) -> Result<PhysAddr, BootMemoryError> {
    validate_page_size(page_size)?;

    let mut end = 0usize;
    for region in boot_info.memory_regions {
        if let Some(span) = covering_span(region.base, region.size, page_size)? {
            end = end.max(
                span.end_ppn()?
                    .checked_mul(page_size)
                    .ok_or(BootMemoryError::RegionOverflow)?,
            );
        }
    }

    if end == 0 {
        return Err(BootMemoryError::NoRam);
    }
    Ok(PhysAddr(end))
}

fn direct_map_covers_phys_end(
    pmap_info: &BootstrapPmapInfo,
    phys_end: PhysAddr,
) -> Result<bool, BootMemoryError> {
    let direct_phys_start = pmap_info
        .direct_map
        .start
        .0
        .checked_sub(pmap_info.direct_map_base.0)
        .ok_or(BootMemoryError::DirectMapTooSmall)?;
    let direct_phys_end = direct_phys_start
        .checked_add(pmap_info.direct_map.size)
        .ok_or(BootMemoryError::RegionOverflow)?;

    Ok(phys_end.0 <= direct_phys_end)
}

fn map_platform_mmio<P: TxPlatform>() -> Result<(), BootMemoryError> {
    for region in P::platform_info().mmio_regions {
        map_mmio_region::<P>(*region)?;
    }
    Ok(())
}

fn map_mmio_region<P: TxPlatform>(region: MmioRegion) -> Result<(), BootMemoryError> {
    if region.phys.size == 0 {
        return Ok(());
    }
    if region.phys.size != region.virt.size {
        return Err(BootMemoryError::InvalidMmioRegion);
    }

    let phys_end = region
        .phys
        .start
        .0
        .checked_add(region.phys.size)
        .ok_or(BootMemoryError::RegionOverflow)?;
    let virt_end = region
        .virt
        .start
        .0
        .checked_add(region.virt.size)
        .ok_or(BootMemoryError::RegionOverflow)?;
    let phys_start = align_down(region.phys.start.0, PAGE_SIZE_4K);
    let virt_offset = region
        .phys
        .start
        .0
        .checked_sub(phys_start)
        .ok_or(BootMemoryError::InvalidMmioRegion)?;
    let virt_start = region
        .virt
        .start
        .0
        .checked_sub(virt_offset)
        .ok_or(BootMemoryError::InvalidMmioRegion)?;
    let phys_end = align_up(phys_end, PAGE_SIZE_4K)?;
    let virt_end = align_up(virt_end, PAGE_SIZE_4K)?;
    let mut phys = phys_start;
    let mut virt = virt_start;
    let mut remaining = phys_end
        .checked_sub(phys_start)
        .ok_or(BootMemoryError::InvalidMmioRegion)?;
    if virt_end
        .checked_sub(virt_start)
        .ok_or(BootMemoryError::InvalidMmioRegion)?
        != remaining
    {
        return Err(BootMemoryError::InvalidMmioRegion);
    }

    while remaining > 0 {
        let (kind, size) = choose_mmio_mapping(VirtAddr(virt), PhysAddr(phys), remaining)?;
        if let Some(reservation) = P::reserve_kernel_mapping(VirtAddr(virt), PhysAddr(phys), kind)
            .map_err(|_| BootMemoryError::PmapMappingFailed)?
        {
            P::commit_kernel_mapping(
                reservation,
                PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
            );
        }
        phys = phys
            .checked_add(size)
            .ok_or(BootMemoryError::RegionOverflow)?;
        virt = virt
            .checked_add(size)
            .ok_or(BootMemoryError::RegionOverflow)?;
        remaining -= size;
    }

    Ok(())
}

pub fn choose_mmio_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    remaining: usize,
) -> Result<(PmapReserveKind, usize), BootMemoryError> {
    if remaining >= 2 * 1024 * 1024
        && virt.0.is_multiple_of(2 * 1024 * 1024)
        && phys.0.is_multiple_of(2 * 1024 * 1024)
    {
        return Ok((PmapReserveKind::Superpage2M, 2 * 1024 * 1024));
    }
    if remaining >= PAGE_SIZE_4K
        && virt.0.is_multiple_of(PAGE_SIZE_4K)
        && phys.0.is_multiple_of(PAGE_SIZE_4K)
    {
        return Ok((PmapReserveKind::Page4K, PAGE_SIZE_4K));
    }
    Err(BootMemoryError::InvalidMmioRegion)
}

unsafe fn install_allocator_from_plan<P: TxPlatform>(
    plan: &BootMemoryPlan,
) -> Result<(), AllocError> {
    unsafe {
        zero_phys_range::<P>(plan.metadata_storage);

        let meta_ptr = direct_map_ptr::<P, FrameMeta>(plan.frame_meta.start);
        for index in 0..plan.frame_count {
            meta_ptr.add(index).write(FrameMeta::new());
        }
        let metas = core::slice::from_raw_parts(meta_ptr, plan.frame_count);
        for meta in metas {
            meta.mark_reserved();
        }

        let bitmap_ptr = direct_map_ptr::<P, AtomicU64>(plan.bitmap.start);
        for index in 0..plan.bitmap_word_count() {
            bitmap_ptr.add(index).write(AtomicU64::new(0));
        }
        let bitmap = core::slice::from_raw_parts(bitmap_ptr, plan.bitmap_word_count());

        let allocator =
            (*BOOT_ALLOCATOR.0.get()).write(BitmapPageAllocator::new_with_base_and_zeroer(
                metas,
                bitmap,
                plan.base_ppn,
                plan.frame_count,
                zero_frame_direct_map::<P>,
            ));

        for span in plan.free_spans() {
            for offset in 0..span.count {
                allocator.mark_free_for_boot(Ppn(span.base.0 + offset));
            }
        }

        install_bitmap_allocator(allocator)?;
        install_frame_copier(copy_frame_direct_map::<P>)?;
        install_frame_kernel_addr(frame_kernel_addr_direct_map::<P>)
    }
}

unsafe fn zero_phys_range<P: TxPlatform>(range: PhysRange) {
    unsafe {
        core::ptr::write_bytes(direct_map_ptr::<P, u8>(range.start), 0, range.size);
    }
}

unsafe fn zero_frame_direct_map<P: TxPlatform>(ppn: Ppn) {
    let phys = ppn
        .0
        .checked_mul(P::PAGE_SIZE)
        .expect("PPN physical address overflow");
    unsafe {
        core::ptr::write_bytes(direct_map_ptr::<P, u8>(PhysAddr(phys)), 0, P::PAGE_SIZE);
    }
}

unsafe fn copy_frame_direct_map<P: TxPlatform>(source: Ppn, dest: Ppn) {
    let source_phys = source
        .0
        .checked_mul(P::PAGE_SIZE)
        .expect("source PPN physical address overflow");
    let dest_phys = dest
        .0
        .checked_mul(P::PAGE_SIZE)
        .expect("dest PPN physical address overflow");
    unsafe {
        core::ptr::copy_nonoverlapping(
            direct_map_ptr::<P, u8>(PhysAddr(source_phys)),
            direct_map_ptr::<P, u8>(PhysAddr(dest_phys)),
            P::PAGE_SIZE,
        );
    }
}

unsafe fn frame_kernel_addr_direct_map<P: TxPlatform>(ppn: Ppn) -> *mut u8 {
    let phys = ppn
        .0
        .checked_mul(P::PAGE_SIZE)
        .expect("PPN physical address overflow");
    direct_map_ptr::<P, u8>(PhysAddr(phys))
}

fn direct_map_ptr<P: TxPlatform, T>(phys: PhysAddr) -> *mut T {
    P::DIRECT_MAP_BASE
        .0
        .checked_add(phys.0)
        .expect("direct-map virtual address overflow") as *mut T
}

fn validate_page_size(page_size: usize) -> Result<(), BootMemoryError> {
    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(BootMemoryError::InvalidPageSize);
    }
    Ok(())
}

fn choose_metadata_layout(
    free: &SpanSet,
    frame_count: usize,
    page_size: usize,
) -> Result<MetadataLayout, BootMemoryError> {
    let meta_bytes = frame_count
        .checked_mul(size_of::<FrameMeta>())
        .ok_or(BootMemoryError::LayoutOverflow)?;
    let bitmap_words = frame_count
        .checked_add(63)
        .ok_or(BootMemoryError::LayoutOverflow)?
        / 64;
    let bitmap_bytes = bitmap_words
        .checked_mul(size_of::<AtomicU64>())
        .ok_or(BootMemoryError::LayoutOverflow)?;

    for span in free.as_slice() {
        let range = span.phys_range(page_size)?;
        let span_start = range.start.0;
        let span_end = range
            .start
            .0
            .checked_add(range.size)
            .ok_or(BootMemoryError::RegionOverflow)?;
        let meta_start = align_up(span_start, align_of::<FrameMeta>())?;
        let meta_end = meta_start
            .checked_add(meta_bytes)
            .ok_or(BootMemoryError::LayoutOverflow)?;
        let bitmap_start = align_up(meta_end, align_of::<AtomicU64>())?;
        let bitmap_end = bitmap_start
            .checked_add(bitmap_bytes)
            .ok_or(BootMemoryError::LayoutOverflow)?;
        if bitmap_end > span_end {
            continue;
        }

        let storage_start = align_down(meta_start, page_size);
        let storage_end = align_up(bitmap_end, page_size)?;
        return Ok(MetadataLayout {
            frame_meta: PhysRange {
                start: PhysAddr(meta_start),
                size: meta_bytes,
            },
            bitmap: PhysRange {
                start: PhysAddr(bitmap_start),
                size: bitmap_bytes,
            },
            storage: PhysRange {
                start: PhysAddr(storage_start),
                size: storage_end - storage_start,
            },
            bitmap_words,
        });
    }

    Err(BootMemoryError::MetadataCarveFailed)
}

fn push_reserved_range(
    reserved: &mut SpanSet,
    range: PhysRange,
    page_size: usize,
) -> Result<(), BootMemoryError> {
    if let Some(span) = covering_span(range.start, range.size, page_size)? {
        reserved.push(span)?;
    }
    Ok(())
}

fn contained_span(
    start: PhysAddr,
    size: usize,
    page_size: usize,
) -> Result<Option<BootPageSpan>, BootMemoryError> {
    if size == 0 {
        return Ok(None);
    }

    let end = start
        .0
        .checked_add(size)
        .ok_or(BootMemoryError::RegionOverflow)?;
    let aligned_start = align_up(start.0, page_size)?;
    let aligned_end = align_down(end, page_size);
    ppn_span_from_bounds(aligned_start, aligned_end, page_size)
}

fn covering_span(
    start: PhysAddr,
    size: usize,
    page_size: usize,
) -> Result<Option<BootPageSpan>, BootMemoryError> {
    if size == 0 {
        return Ok(None);
    }

    let end = start
        .0
        .checked_add(size)
        .ok_or(BootMemoryError::RegionOverflow)?;
    let aligned_start = align_down(start.0, page_size);
    let aligned_end = align_up(end, page_size)?;
    ppn_span_from_bounds(aligned_start, aligned_end, page_size)
}

fn ppn_span_from_bounds(
    start: usize,
    end: usize,
    page_size: usize,
) -> Result<Option<BootPageSpan>, BootMemoryError> {
    if start >= end {
        return Ok(None);
    }

    Ok(Some(BootPageSpan {
        base: Ppn(start / page_size),
        count: (end - start) / page_size,
    }))
}

fn validate_direct_map(
    direct_map_base: usize,
    direct_map: VirtRange,
    phys: PhysRange,
) -> Result<(), BootMemoryError> {
    let Some(direct_phys_start) = direct_map.start.0.checked_sub(direct_map_base) else {
        return Err(BootMemoryError::DirectMapTooSmall);
    };
    let direct_phys_end = direct_phys_start
        .checked_add(direct_map.size)
        .ok_or(BootMemoryError::RegionOverflow)?;
    let phys_end = phys
        .start
        .0
        .checked_add(phys.size)
        .ok_or(BootMemoryError::RegionOverflow)?;

    if phys.start.0 < direct_phys_start || phys_end > direct_phys_end {
        return Err(BootMemoryError::DirectMapTooSmall);
    }

    Ok(())
}

fn align_down(value: usize, align: usize) -> usize {
    value - (value % align)
}

fn align_up(value: usize, align: usize) -> Result<usize, BootMemoryError> {
    let remainder = value % align;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(align - remainder)
            .ok_or(BootMemoryError::LayoutOverflow)
    }
}
