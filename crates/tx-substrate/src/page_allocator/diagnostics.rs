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
    /// Backend-local scan word hint, useful for debugging fragmentation.
    pub scan_hint: usize,
}
