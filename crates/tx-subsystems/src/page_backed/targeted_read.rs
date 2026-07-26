//! Bounded targeted reads against a `PageContainer`'s direct-map view.
//!
//! Per `txdoc:PAGE-BACKED-3-PAGECONTAINER` and the cross-doc edit B1
//! (`txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS`), the ELF loader needs a way to
//! pull a small, kernel-side-bounded byte slice (the ELF header, the
//! program-header table) out of a file-backed `PageContainer` *before* an
//! `AddressSpace` exists for the new image. `step_read_to_user` and
//! `step_write_from_user` go through `AddressSpace::copy_*_user`; this
//! primitive is their kernel-buffer cousin.
//!
//! The function walks page-rounded chunks of `out`, materialises each
//! page on demand through the existing `PageContainer::materialize_page`
//! path, and `copy_nonoverlapping`s the bytes through the substrate's
//! `frame_kernel_addr` direct-map view. EOF before the buffer is full
//! returns `Errno::ENOEXEC` per the loader's short-read contract.

use super::*;
use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome};

/// Read exactly `out.len()` bytes from `pc` starting at byte offset
/// `off` into the kernel-side buffer `out`. Returns `Done(())` on
/// success.
///
/// - `Err(Errno::ENOEXEC)` on short read (request extends past
///   `pc.size_bytes()`), per the loader's targeted-read contract.
/// - `Err(Errno::EINVAL)` on offset overflow.
/// - `Err(Errno::EIO)` if the substrate's direct-map hook is not
///   installed for a materialised PPN.
/// - `Yield { OnWaitSource { .. } }` propagates from `materialize_page`
///   (e.g. an `FsPageBacking::fetch_page` block) carrying the bytes
///   read so far as `ByteProgress`.
///
/// Cites: `txdoc:PAGE-BACKED-3-PAGECONTAINER`, cross-doc edit B1 in
/// `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS`.
pub fn read_exact_at(
    pc: &PageContainer,
    off: u64,
    out: &mut [u8],
    guard: &Guard<'_>,
) -> StepOutcome<(), ByteProgress> {
    use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome as V3};
    if out.is_empty() {
        return V3::done(());
    }

    let len = out.len();
    let Some(end) = off.checked_add(len as u64) else {
        return V3::err(Errno::EINVAL.into());
    };

    // Short-read contract: EOF before fill is `ENOEXEC`. Mirrors the
    // loader's targeted-read errno mapping in
    // `txdoc:EXEC-8-9-ERRNO-MAPPING`.
    if end > pc.size_bytes() {
        return V3::err(Errno::ENOEXEC.into());
    }
    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    if end > capacity {
        return V3::err(Errno::EINVAL.into());
    }

    let mut advanced = 0usize;
    let mut offset = off;
    while advanced < len {
        let page_index = PageIndex::new(offset / crate::vm::USER_PAGE_SIZE as u64);
        let within_page = (offset % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(len - advanced, crate::vm::USER_PAGE_SIZE - within_page);

        // `materialize_page` is on v3
        // (`StepOutcome<MaterializedPage, NoProgress>`); translate per
        // outcome variant onto the v3 byte-progress return:
        // - v3 `Done(m)` → use the materialized frame.
        // - v3 `Continue { .. }` (NoProgress) → no frame; surface
        //   `Err(EAGAIN)` as a conservative collapse — page allocation
        //   rarely emits this for a one-shot loader read.
        // - v3 `Yield { OnWaitSource { c, i } }` → propagate as v3
        //   `Yield` carrying accumulated `ByteProgress`.
        // - v3 `Yield { OnAgent .. }` → `Err(EIO)`.
        // - v3 `Err(e)` → `Err(e)`.
        let materialized = match pc.materialize_page(page_index, MaterializeAccess::Read, guard) {
            V3::Done(m) => m,
            V3::Continue { .. } => return V3::err(Errno::EAGAIN.into()),
            V3::Yield { shape, .. } => {
                if let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                {
                    return crate::page_backed::notification::yield_on_wait_source(
                        ByteProgress::new(advanced),
                        carrier,
                        interests,
                    );
                }
                return V3::err(Errno::EIO.into());
            }
            V3::Err(errno) => return V3::err(errno),
        };

        let frame_base = match page_allocator::frame_kernel_addr(materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => return V3::err(Errno::EIO.into()),
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

    V3::done(())
}

/// Wait-capable targeted read used by asynchronous kernel loaders.
///
/// A `PageContainer` serialises first materialisation of a file page.  Under
/// SMP, another hart reading the same executable or interpreter can therefore
/// legitimately return `Yield(OnWaitSource)` while the owner fetches that
/// page.  This is an internal scheduling condition, not an `EIO`/`EBUSY`
/// condition visible to `execve(2)`.
///
/// Each attempt owns a fresh epoch guard and drops it before awaiting.  After
/// notification the complete bounded read is retried; this is safe because the
/// destination is a private kernel buffer and previously copied bytes are
/// simply overwritten with the same file contents.
pub async fn read_exact_at_wait(pc: &PageContainer, off: u64, out: &mut [u8]) -> Result<(), Errno> {
    use crate::execution::WaitToken;
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;

    loop {
        let outcome = {
            let guard = crate::page_backed::adapter::step_engine::guard();
            read_exact_at(pc, off, out, &guard)
        };

        match outcome {
            V3::Done(()) => return Ok(()),
            V3::Err(error) => return Err(error.into()),
            V3::Continue { .. } => {
                // `read_exact_at` currently collapses a materializer
                // Continue into EAGAIN, so this is only future-proofing for a
                // backend that adopts resumable local progress.
                continue;
            }
            V3::Yield { shape, .. } => {
                let Some((source, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    return Err(Errno::EIO);
                };
                let token = WaitToken::new(source, interests);
                if let Some(wait) = crate::wait_source::wait_on_token(token) {
                    let _ = wait.await;
                }
                // If the source disappeared before registration, its owner
                // has already completed or aborted publication.  Retry and
                // observe the resulting page-cache state.
            }
        }
    }
}
