//! L4-owned direct-I/O user-buffer leases.
//!
//! The caller must fault and publish the user range before entering this
//! constructor. This module only validates the published pmap snapshot and
//! holds long-term DMA role evidence until the buffer is dropped.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::io_manager::block::BioVec;
use crate::vm::{AccessMode, AddressSpace, USER_PAGE_SIZE, UserAccessKind, UserRange};
use tx_hal::UserPtr;
use tx_substrate::page_allocator::{self, DmaPin};

use super::{PageRange, RangeReservation};

static NEXT_DIRECT_IO_LEASE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoBufferError {
    InvalidRange,
    NotMaterialized,
    PermissionDenied,
    DmaPin(page_allocator::AllocError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoOperation {
    Read,
    Write,
}

/// Immutable L4 description handed to the filesystem planner after admission.
///
/// The corresponding [`DirectIoBuffer`] remains private to `PageContainer` so
/// its DMA pins cannot be dropped before terminal completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectIoSubmission {
    lease: crate::fs_iface::IoDataLeaseId,
    range: PageRange,
    operation: DirectIoOperation,
    source: Option<crate::fs_iface::IoDataSource>,
    target: Option<crate::fs_iface::IoDataTarget>,
}

impl DirectIoSubmission {
    pub(crate) fn read(range: PageRange, buffer: &DirectIoBuffer) -> Self {
        Self {
            lease: buffer.lease_id(),
            range,
            operation: DirectIoOperation::Read,
            source: None,
            target: Some(buffer.as_target()),
        }
    }

    pub(crate) fn write(range: PageRange, buffer: &DirectIoBuffer) -> Self {
        Self {
            lease: buffer.lease_id(),
            range,
            operation: DirectIoOperation::Write,
            source: Some(buffer.as_source()),
            target: None,
        }
    }

    pub const fn lease_id(&self) -> crate::fs_iface::IoDataLeaseId {
        self.lease
    }

    pub const fn range(&self) -> PageRange {
        self.range
    }

    pub const fn operation(&self) -> DirectIoOperation {
        self.operation
    }

    pub fn source(&self) -> Option<&crate::fs_iface::IoDataSource> {
        self.source.as_ref()
    }

    pub fn target(&self) -> Option<&crate::fs_iface::IoDataTarget> {
        self.target.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoCompletion {
    Read,
    Write { invalidated: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectIoInFlightState {
    Admitted,
    Planning,
    Queued,
}

#[derive(Debug)]
pub(crate) struct DirectIoInFlight {
    pub(crate) reservation: RangeReservation,
    pub(crate) buffer: DirectIoBuffer,
    pub(crate) operation: DirectIoOperation,
    pub(crate) state: DirectIoInFlightState,
}

/// A pinned user buffer that can cross the L4 -> L5 -> L6 async boundary.
///
/// `vecs` contains physical frame keys and byte ranges only; it does not
/// expose a user pointer to filesystem or driver code. The DMA pins keep the
/// corresponding frames live until this lease is dropped.
#[derive(Debug)]
pub struct DirectIoBuffer {
    range: UserRange,
    len: usize,
    lease: crate::fs_iface::IoDataLeaseId,
    vecs: Vec<BioVec>,
    pins: Vec<DmaPin<'static, page_allocator::BitmapPageAllocator<'static>>>,
}

// DmaPin's allocator reference is the process-global installed allocator. The
// token contains no borrowed address-space state, and moving this aggregate is
// the intended ownership transfer across the async submission boundary.
unsafe impl Send for DirectIoBuffer {}
unsafe impl Sync for DirectIoBuffer {}

impl DirectIoBuffer {
    pub fn pin(
        aspace: &AddressSpace,
        user: UserPtr<u8>,
        len: usize,
        access: UserAccessKind,
    ) -> Result<Self, DirectIoBufferError> {
        let (range, first_offset) = user_range(user, len)?;
        let snapshots = aspace.pmap().walk_range(range);
        if snapshots.len() != range.page_count() {
            return Err(DirectIoBufferError::NotMaterialized);
        }
        let required = match access {
            UserAccessKind::Read => AccessMode::Read,
            UserAccessKind::Write => AccessMode::Write,
        };
        if snapshots
            .iter()
            .any(|(_, snapshot)| !snapshot.prot.permits(required))
        {
            return Err(DirectIoBufferError::PermissionDenied);
        }

        let mut pins = Vec::with_capacity(snapshots.len());
        for (_, snapshot) in &snapshots {
            match page_allocator::acquire_dma_pin(snapshot.ppn) {
                Ok(pin) => pins.push(pin),
                Err(error) => return Err(DirectIoBufferError::DmaPin(error)),
            }
        }

        let mut vecs = Vec::with_capacity(snapshots.len());
        let mut remaining = len;
        for (index, (_, snapshot)) in snapshots.iter().enumerate() {
            let offset = if index == 0 { first_offset } else { 0 };
            let available = USER_PAGE_SIZE - offset;
            let segment_len = core::cmp::min(remaining, available);
            vecs.push(BioVec::new(
                snapshot.ppn.0 as u64,
                offset as u32,
                segment_len as u32,
            ));
            remaining -= segment_len;
        }
        debug_assert_eq!(remaining, 0);

        Ok(Self {
            range,
            len,
            lease: crate::fs_iface::IoDataLeaseId::new(
                NEXT_DIRECT_IO_LEASE.fetch_add(1, Ordering::Relaxed).max(1),
            ),
            vecs,
            pins,
        })
    }

    pub const fn range(&self) -> UserRange {
        self.range
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub fn vecs(&self) -> &[BioVec] {
        &self.vecs
    }

    pub const fn lease_id(&self) -> crate::fs_iface::IoDataLeaseId {
        self.lease
    }

    pub fn pin_count(&self) -> usize {
        self.pins.len()
    }

    pub fn as_source(&self) -> crate::fs_iface::IoDataSource {
        crate::fs_iface::IoDataSource::direct(self.lease, self.vecs.clone())
    }

    pub fn as_target(&self) -> crate::fs_iface::IoDataTarget {
        crate::fs_iface::IoDataTarget::direct(self.lease, self.vecs.clone())
    }
}

fn user_range(user: UserPtr<u8>, len: usize) -> Result<(UserRange, usize), DirectIoBufferError> {
    if user.addr() == 0 || len == 0 {
        return Err(DirectIoBufferError::InvalidRange);
    }
    let end = user
        .addr()
        .checked_add(len)
        .ok_or(DirectIoBufferError::InvalidRange)?;
    let start = user.addr() & !(USER_PAGE_SIZE - 1);
    let page_end = end
        .checked_add(USER_PAGE_SIZE - 1)
        .map(|value| value & !(USER_PAGE_SIZE - 1))
        .ok_or(DirectIoBufferError::InvalidRange)?;
    let range = UserRange::new_aligned(
        crate::vm::UserVirtAddr::new(start),
        page_end
            .checked_sub(start)
            .ok_or(DirectIoBufferError::InvalidRange)?,
    )
    .map_err(|_| DirectIoBufferError::InvalidRange)?;
    Ok((range, user.addr() - start))
}
