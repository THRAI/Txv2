use core::ptr::NonNull;

use tx_hal::CpuId;

use super::common::WireRetirement;
use crate::epoch::{EpochError, Guard};

/// Error returned while building or committing an owner-storage reclaim fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireOwnerReclaimError {
    /// A wire retirement record came from a wire that was already terminal.
    WireAlreadyTerminal,
    /// All wire retirements for one owner must be performed under one guard.
    GuardMismatch,
    /// The epoch domain rejected the physical owner-storage retirement.
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

/// Fence proving that an owner's embedded bus wires were terminal-drained.
///
/// The fence is built from [`WireRetirement`] records produced by
/// `retire(..., &epoch::Guard)` or `retire_silently(&epoch::Guard)`. It is the
/// bridge between bus-level terminal/drain handshakes and owner-level EBR:
/// once all embedded wires have contributed a retirement record, the owning
/// subsystem can enqueue the containing storage for epoch-delayed reclaim.
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

    /// Queue the containing owner storage for epoch-delayed reclaim.
    ///
    /// # Safety
    ///
    /// `owner` must name the storage containing the wires represented by this
    /// fence, and `reclaim_fn` must be valid for exactly one call with that
    /// pointer after the EBR delay window. The caller must include every
    /// embedded wire that can publish wakes from the owner before calling this
    /// method, and must not enqueue the same owner storage more than once.
    /// This method consumes the fence so one fence cannot be reused for a
    /// second enqueue.
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

/// Typed manifest for owner storage that embeds bus wires.
///
/// Implement this on the containing zone/device owner type, not on the wire
/// fields. The owner knows the complete embedded-wire set and the matching
/// typed reclaim callback for its storage.
///
/// # Safety
///
/// `retire_embedded_wires` must include every embedded wire that can publish a
/// wake from this owner, and it must retire each of those wires exactly once
/// under the supplied guard. `reclaim_owner` must be valid for exactly one call
/// after the EBR delay window for storage previously passed to
/// [`retire_wire_owner`].
pub unsafe trait WireOwnerManifest: Sized + 'static {
    fn retire_embedded_wires(
        &self,
        guard: &Guard<'_>,
    ) -> Result<WireOwnerRetireFence, WireOwnerReclaimError>;

    /// Reclaim one owner storage instance after the EBR delay window.
    ///
    /// # Safety
    ///
    /// `owner` must be the same storage instance previously passed to
    /// [`retire_wire_owner`] for this owner type. The implementation must be
    /// valid for exactly one call and must not publish new references to the
    /// retired storage.
    unsafe fn reclaim_owner(owner: NonNull<Self>);
}

/// Retire a typed owner after its manifest terminal-drains embedded wires.
///
/// This is the typed owner hook over [`WireOwnerRetireFence`]. The manifest
/// provides the complete wire list and typed reclaim callback; the helper keeps
/// the final epoch callback type-correct instead of passing an erased reclaim
/// function at each call site.
///
/// # Safety
///
/// `owner` must be valid for shared access while `retire_embedded_wires` runs,
/// must not be retired twice, and must name storage whose physical reuse is
/// controlled by `T::reclaim_owner`.
pub unsafe fn retire_wire_owner<T: WireOwnerManifest>(
    owner: NonNull<T>,
    guard: &Guard<'_>,
) -> Result<WireOwnerReclamation, WireOwnerReclaimError> {
    let fence = unsafe { owner.as_ref() }.retire_embedded_wires(guard)?;
    unsafe { fence.retire_owner_storage(owner.cast(), reclaim_wire_owner::<T>) }
}

unsafe fn reclaim_wire_owner<T: WireOwnerManifest>(ptr: *mut u8) {
    let owner = unsafe { NonNull::new_unchecked(ptr.cast::<T>()) };
    unsafe {
        T::reclaim_owner(owner);
    }
}
