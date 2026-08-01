use core::ptr::NonNull;

use tx_hal::CpuId;

use super::common::WireRetirement;
use crate::epoch::{EpochError, Guard};

/// Error returned while building or committing an owner-storage reclaim fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireOwnerReclaimError {
    WireAlreadyTerminal,
    GuardMismatch,
    Epoch(EpochError),
}

impl From<EpochError> for WireOwnerReclaimError {
    fn from(value: EpochError) -> Self {
        Self::Epoch(value)
    }
}

/// Summary returned after owner storage is queued for epoch-delayed reclaim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireOwnerReclamation {
    wires: usize,
    woken: usize,
    guard_epoch: u64,
    guard_cpu: CpuId,
}

impl WireOwnerReclamation {
    pub const fn wires(self) -> usize {
        self.wires
    }

    pub const fn woken(self) -> usize {
        self.woken
    }

    pub const fn guard_epoch(self) -> u64 {
        self.guard_epoch
    }

    pub const fn guard_cpu(self) -> CpuId {
        self.guard_cpu
    }
}

/// Proof that every embedded wire of an owner reached the terminal state
/// under one EBR guard before the containing storage is retired.
#[derive(Debug, Eq, PartialEq)]
pub struct WireOwnerRetireFence {
    wires: usize,
    woken: usize,
    guard_epoch: u64,
    guard_cpu: CpuId,
}

impl WireOwnerRetireFence {
    pub fn from_retirement(retirement: WireRetirement) -> Result<Self, WireOwnerReclaimError> {
        if !retirement.newly_terminal() {
            return Err(WireOwnerReclaimError::WireAlreadyTerminal);
        }
        Ok(Self {
            wires: 1,
            woken: retirement.woken(),
            guard_epoch: retirement.guard_epoch(),
            guard_cpu: retirement.guard_cpu(),
        })
    }

    pub fn include(&mut self, retirement: WireRetirement) -> Result<(), WireOwnerReclaimError> {
        if !retirement.newly_terminal() {
            return Err(WireOwnerReclaimError::WireAlreadyTerminal);
        }
        if retirement.guard_epoch() != self.guard_epoch || retirement.guard_cpu() != self.guard_cpu
        {
            return Err(WireOwnerReclaimError::GuardMismatch);
        }
        self.wires += 1;
        self.woken += retirement.woken();
        Ok(())
    }

    pub const fn wires(&self) -> usize {
        self.wires
    }

    pub const fn woken(&self) -> usize {
        self.woken
    }

    pub const fn guard_epoch(&self) -> u64 {
        self.guard_epoch
    }

    pub const fn guard_cpu(&self) -> CpuId {
        self.guard_cpu
    }

    /// Queue the containing storage through the active Crossbeam-style EBR.
    ///
    /// # Safety
    /// `owner` and `reclaim_fn` must describe the unique containing storage,
    /// every publishing wire must be represented by this consumed fence, and
    /// the storage must not be retired more than once.
    pub unsafe fn retire_owner_storage(
        self,
        owner: NonNull<u8>,
        reclaim_fn: unsafe fn(*mut u8),
    ) -> Result<WireOwnerReclamation, WireOwnerReclaimError> {
        unsafe {
            crate::epoch::retire_raw(owner.as_ptr(), reclaim_fn)?;
        }
        Ok(WireOwnerReclamation {
            wires: self.wires,
            woken: self.woken,
            guard_epoch: self.guard_epoch,
            guard_cpu: self.guard_cpu,
        })
    }
}

/// Complete manifest for an owner that embeds one or more bus wires.
///
/// # Safety
/// The implementation must retire every publishing wire exactly once and the
/// reclaim callback must match the storage passed to `retire_wire_owner`.
pub unsafe trait WireOwnerManifest: Sized + 'static {
    fn retire_embedded_wires(
        &self,
        guard: &Guard<'_>,
    ) -> Result<WireOwnerRetireFence, WireOwnerReclaimError>;

    unsafe fn reclaim_owner(owner: NonNull<Self>);
}

/// Terminal-drain an owner's wires, then enqueue its storage for EBR reclaim.
///
/// # Safety
/// `owner` must remain valid during wire retirement and must not already have
/// been retired.
pub unsafe fn retire_wire_owner<T: WireOwnerManifest>(
    owner: NonNull<T>,
    guard: &Guard<'_>,
) -> Result<WireOwnerReclamation, WireOwnerReclaimError> {
    let fence = unsafe { owner.as_ref() }.retire_embedded_wires(guard)?;
    unsafe { fence.retire_owner_storage(owner.cast(), reclaim_wire_owner::<T>) }
}

unsafe fn reclaim_wire_owner<T: WireOwnerManifest>(ptr: *mut u8) {
    let owner = unsafe { NonNull::new_unchecked(ptr.cast::<T>()) };
    unsafe { T::reclaim_owner(owner) }
}
