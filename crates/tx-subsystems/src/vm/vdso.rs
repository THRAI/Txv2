//! VM mapping contract for the permanent vDSO image and VVAR frame.

use crate::page_backed::MaterializedPagePin;
use crate::vm::adapter::step_engine::page_allocator;
use crate::vm::{
    AddressSpace, MapPlacement, MapReserveResult, Prot, UserRange, UserVirtAddr, VmBacking,
    VmEntry, VmEntryFlags, VmMapError, VmSpecialBacking, FULL_USER_V1_TOP, USER_PAGE_SIZE,
};
use tx_time::vdso::VvarData;

pub const VDSO_RESERVATION_WINDOW: usize = 16 * 1024 * 1024;
const _: () = assert!(core::mem::size_of::<VvarData>() <= USER_PAGE_SIZE);

/// The process-local interval reserved for VVAR and the immutable vDSO image.
///
/// VM owns the placement policy. Exec uses this value while choosing its main
/// image, interpreter, and stack layout, then hands the same value back to the
/// mapper for special-page publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VdsoLayout {
    window: UserRange,
}

impl VdsoLayout {
    pub fn for_user_top(user_top: UserVirtAddr) -> Result<Self, VmMapError> {
        let ceiling = user_top.as_usize().min(FULL_USER_V1_TOP);
        let window_start = ceiling
            .checked_sub(VDSO_RESERVATION_WINDOW)
            .ok_or(VmMapError::InvalidRange)?;
        let window = UserRange::new_aligned(UserVirtAddr(window_start), VDSO_RESERVATION_WINDOW)
            .map_err(|_| VmMapError::InvalidRange)?;
        Ok(Self { window })
    }

    pub const fn window(self) -> UserRange {
        self.window
    }
}

/// User locations of a fully installed vDSO/VVAR mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VdsoMapping {
    pub vdso_base: UserVirtAddr,
    pub vdso_size: usize,
    pub vvar_base: UserVirtAddr,
}

/// Return the mapped userspace address of the vDSO signal restorer.
///
/// The result is available only after the full special mapping exists. This
/// keeps signal delivery from manufacturing a code pointer from an image
/// offset when exec deliberately selected the syscall-only vDSO fallback.
pub fn vdso_rt_sigreturn_addr(aspace: &AddressSpace) -> Option<UserVirtAddr> {
    if !tx_vdso::VDSO_AVAILABLE || tx_vdso::VDSO_RT_SIGRETURN_OFFSET >= tx_vdso::VDSO_IMAGE_SIZE {
        return None;
    }

    let vdso = aspace
        .recipes_snapshot()
        .into_iter()
        .find(|entry| entry.special_backing() == Some(VmSpecialBacking::VdsoText))?;
    let address = vdso
        .range
        .start()
        .as_usize()
        .checked_add(tx_vdso::VDSO_RT_SIGRETURN_OFFSET)?;
    let address = UserVirtAddr::new(address);
    (aspace.lookup(address)?.special_backing() == Some(VmSpecialBacking::VdsoText))
        .then_some(address)
}

/// Map the boot-initialised permanent frames, or return `None` when no target
/// vDSO image exists. `Some` is returned only after every recipe and PTE is
/// installed; failed installation tears down the whole reservation.
pub fn map_vdso_into_aspace(
    aspace: &AddressSpace,
    layout: VdsoLayout,
) -> Result<Option<VdsoMapping>, VmMapError> {
    if !crate::vdso::vdso_available() {
        return Ok(None);
    }
    let image = crate::vdso::kernel_vdso();
    map_frames(aspace, image.frames, crate::vdso::vvar_ppn(), layout).map(Some)
}

fn map_frames(
    aspace: &AddressSpace,
    vdso_frames: &[tx_hal::Ppn],
    vvar_ppn: tx_hal::Ppn,
    layout: VdsoLayout,
) -> Result<VdsoMapping, VmMapError> {
    if vdso_frames.is_empty() {
        return Err(VmMapError::InvalidRange);
    }
    let pages = vdso_frames
        .len()
        .checked_add(1)
        .ok_or(VmMapError::InvalidRange)?;
    let bytes = pages
        .checked_mul(USER_PAGE_SIZE)
        .ok_or(VmMapError::InvalidRange)?;
    if bytes > VDSO_RESERVATION_WINDOW {
        return Err(VmMapError::NoFreeRange);
    }
    let whole = aspace
        .find_free_range(layout.window(), pages)
        .ok_or(VmMapError::NoFreeRange)?;
    let vvar_base = whole.start();
    let vdso_base = UserVirtAddr(
        vvar_base
            .as_usize()
            .checked_add_signed(-tx_vdso::VVAR_DELTA)
            .ok_or(VmMapError::InvalidRange)?,
    );
    let vvar =
        UserRange::new_aligned(vvar_base, USER_PAGE_SIZE).map_err(|_| VmMapError::InvalidRange)?;
    let vdso = UserRange::new_aligned(vdso_base, vdso_frames.len() * USER_PAGE_SIZE)
        .map_err(|_| VmMapError::InvalidRange)?;

    if let Err(error) = commit_recipe(aspace, vvar, Prot::READ, VmSpecialBacking::Vvar)
        .and_then(|_| commit_recipe(aspace, vdso, Prot::READ_EXECUTE, VmSpecialBacking::VdsoText))
        .and_then(|_| publish(aspace, vvar_base, vvar_ppn, Prot::READ))
        .and_then(|_| {
            for (index, ppn) in vdso_frames.iter().copied().enumerate() {
                publish(
                    aspace,
                    UserVirtAddr(vdso_base.as_usize() + index * USER_PAGE_SIZE),
                    ppn,
                    Prot::READ_EXECUTE,
                )?;
            }
            Ok(())
        })
    {
        let _ = aspace.try_munmap(whole);
        return Err(error);
    }

    Ok(VdsoMapping {
        vdso_base,
        vdso_size: vdso.len(),
        vvar_base,
    })
}

fn commit_recipe(
    aspace: &AddressSpace,
    range: UserRange,
    prot: Prot,
    special: VmSpecialBacking,
) -> Result<(), VmMapError> {
    match aspace.reserve_map(
        VmEntry::new(
            range,
            prot,
            VmEntryFlags::SHARED,
            VmBacking::Special(special),
        ),
        MapPlacement::RequireFree,
    ) {
        MapReserveResult::Reserved(reservation) => reservation.commit().map(|_| ()),
        MapReserveResult::Blocked(_) => Err(VmMapError::WouldBlock),
        MapReserveResult::Err(error) => Err(error),
    }
}

fn publish(
    aspace: &AddressSpace,
    addr: UserVirtAddr,
    ppn: tx_hal::Ppn,
    prot: Prot,
) -> Result<(), VmMapError> {
    let pin = page_allocator::acquire_map_pin(ppn).map_err(|_| VmMapError::InvalidRange)?;
    aspace
        .pmap
        .publish_page(
            addr.containing_page(),
            ppn,
            prot,
            MaterializedPagePin::Allocated(pin),
        )
        .map(|_| ())
        .map_err(VmMapError::Pmap)
}

#[cfg(test)]
pub(crate) fn map_frames_for_test(
    aspace: &AddressSpace,
    vdso_frames: &[tx_hal::Ppn],
    vvar_ppn: tx_hal::Ppn,
    layout: VdsoLayout,
) -> Result<VdsoMapping, VmMapError> {
    map_frames(aspace, vdso_frames, vvar_ppn, layout)
}
