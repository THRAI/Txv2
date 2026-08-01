use tx_hal::Ppn;

/// Installed allocator backend kind reported by diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocatorBackendKind {
    /// Global atomic bitmap backend.
    Bitmap,
}

/// Snapshot of allocator counts and backend-local hints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorDiagnostics {
    /// Concrete backend currently installed.
    pub backend: AllocatorBackendKind,
    /// First raw PPN covered by the dense backend metadata/bitmap arrays.
    pub base_ppn: Ppn,
    /// Number of PPN rows covered by the backend.
    pub total_count: usize,
    /// Number of frames currently available to the allocator.
    pub free_count: usize,
    /// Longest currently free contiguous PPN run observed by diagnostics.
    pub max_contiguous_free_run: usize,
    /// Backend-local scan word hint, useful for debugging fragmentation.
    pub scan_hint: usize,
}

/// A best-effort snapshot of why frames are not in the allocator free bitmap.
///
/// The `*_pages` role counts overlap: a frame mapped into a page table and
/// retained by the page cache contributes to both `mapped_pages` and
/// `cached_pages`.  `mixed_role_pages` makes that overlap explicit.  The
/// `*_refs` fields sum the packed contributor counters and are useful for
/// finding a small number of frames with unexpectedly large reference counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameRoleDiagnostics {
    /// Frames whose allocator bitmap bit is currently set.
    pub free_pages: usize,
    /// Frames excluded from normal allocation by the reserved flag.
    pub reserved_pages: usize,
    /// Reserved frames whose packed liveness state is non-zero.
    pub reserved_live_pages: usize,
    /// Non-free, non-reserved frames with an all-zero liveness state.
    ///
    /// A very small value can be an in-flight `FrameReservation`.  A large
    /// steady value means a reservation/rollback leak or inconsistent bitmap.
    pub unowned_unavailable_pages: usize,
    /// Frames with at least one generic owner/retainer reference.
    pub owned_pages: usize,
    /// Frames with at least one PTE/map contributor.
    pub mapped_pages: usize,
    /// Frames retained by at least one page-cache contributor.
    pub cached_pages: usize,
    /// Frames retained by at least one DMA/long-term pin.
    pub pinned_pages: usize,
    /// Frames with more than one non-zero contributor class.
    pub mixed_role_pages: usize,
    /// Sum of generic owner/retainer counters across all frames.
    pub owner_refs: usize,
    /// Sum of PTE/map counters across all frames.
    pub map_refs: usize,
    /// Sum of page-cache counters across all frames.
    pub cache_refs: usize,
    /// Sum of DMA/long-term-pin counters across all frames.
    pub pin_refs: usize,
}
