//! Bounded targeted reads against a `PageContainer`'s direct-map view.
//!
//! Per `txdoc:PAGE-BACKED-3-PAGECONTAINER` and the cross-doc edit B1
//! (`txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS`), the ELF loader needs a way to
//! pull a small, kernel-side-bounded byte slice (the ELF header, the
//! program-header table) out of a file-backed `PageContainer` *before* an
//! `AddressSpace` exists for the new image. `step_read_to_user` and
//! `step_write_from_user` go through `UserAccessIf`; this primitive is
//! their kernel-buffer cousin.
//!
//! The function walks page-rounded chunks of `out`, materialises each
//! page on demand through the existing `PageContainer::materialize_page`
//! path, and `copy_nonoverlapping`s the bytes through the substrate's
//! `frame_kernel_addr` direct-map view. EOF before the buffer is full
//! returns `Errno::ENOEXEC` per the loader's short-read contract.

use super::*;

/// Read exactly `out.len()` bytes from `pc` starting at byte offset
/// `off` into the kernel-side buffer `out`. Returns `Done(())` on
/// success.
///
/// - `Err(Errno::ENOEXEC)` on short read (request extends past
///   `pc.size_bytes()`), per the loader's targeted-read contract.
/// - `Err(Errno::EINVAL)` on offset overflow.
/// - `Err(Errno::EIO)` if the substrate's direct-map hook is not
///   installed for a materialised PPN.
/// - `Blocked` / `AdvancedThenBlocked` propagate from
///   `materialize_page` (e.g. an `FsPageBacking::fetch_page` block).
///
/// Cites: `txdoc:PAGE-BACKED-3-PAGECONTAINER`, cross-doc edit B1 in
/// `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS`.
pub fn read_exact_at(
    pc: &PageContainer,
    off: u64,
    out: &mut [u8],
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    if out.is_empty() {
        return StepOutcome::Done(());
    }

    let len = out.len();
    let Some(end) = off.checked_add(len as u64) else {
        return StepOutcome::Err(Errno::EINVAL);
    };

    // Short-read contract: EOF before fill is `ENOEXEC`. Mirrors the
    // loader's targeted-read errno mapping in
    // `txdoc:EXEC-8-9-ERRNO-MAPPING`.
    if end > pc.size_bytes() {
        return StepOutcome::Err(Errno::ENOEXEC);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if end > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let mut advanced = 0usize;
    let mut offset = off;
    while advanced < len {
        let page_index = PageIndex::new(offset / crate::vm::USER_PAGE_SIZE as u64);
        let within_page = (offset % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(len - advanced, crate::vm::USER_PAGE_SIZE - within_page);

        let materialized = match pc.materialize_page(page_index, MaterializeAccess::Read, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            StepOutcome::Blocked(token) => return StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked(_, token) => return StepOutcome::Blocked(token),
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
        };

        let frame_base = match page_allocator::frame_kernel_addr(materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => return StepOutcome::Err(Errno::EIO),
        };
        // SAFETY: `frame_base` is the kernel direct-map view of an
        // installed page; we hold the materialisation pin via
        // `materialized.map_pin` for the duration of the copy.
        // `within_page + chunk <= USER_PAGE_SIZE` by construction; the
        // destination slice covers exactly `chunk` bytes starting at
        // `advanced`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_base.add(within_page),
                out.as_mut_ptr().add(advanced),
                chunk,
            );
        }

        advanced += chunk;
        offset += chunk as u64;
    }

    StepOutcome::Done(())
}
