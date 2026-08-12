use super::*;

/// Install `source_ppn` into `pc`'s page cache at `page` as a *shared* entry.
///
/// PAGE_BACKED §7.1: reflink lets two PCs reference the same physical frame.
/// This helper takes a fresh `CachePin` on `source_ppn` (incrementing
/// `cache_ref` so the source is preserved) and inserts the entry via
/// `install_if_absent`. Caller is responsible for ensuring `source_ppn` is
/// already live (typically because another PC's page cache holds it).
///
/// `pc.kind()` must be a content-bearing variant (Anon or File). Device
/// backings reject because their PPNs are not refcount-managed by the page
/// allocator.
///
/// Returns `Err(PageCacheError::AlreadyPresent)` if `pc` already caches the
/// page; the caller is expected to use `cow_replace_into_private` for that
/// path.
pub fn install_shared_page(
    pc: &PageContainer,
    page: PageIndex,
    source_ppn: Ppn,
) -> Result<(), PageCacheError> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return Err(PageCacheError::UnsupportedKind);
    }
    let cache_pin = page_allocator::acquire_cache_pin(source_ppn).map_err(PageCacheError::Alloc)?;
    let frame = CachedFrame {
        ppn: source_ppn,
        pin: PageCachePin::Allocated(cache_pin),
    };
    if !pc.install_resident_if_absent_published(page, frame)? {
        return Err(PageCacheError::AlreadyPresent {
            current: pc.lookup(page).ok_or(PageCacheError::MissingPage)?,
        });
    }
    Ok(())
}

/// Replace `pc`'s currently cached frame at `page` with a fresh private
/// frame whose contents match the old shared frame.
///
/// PAGE_BACKED §7.2 CoW-on-write. Used by the fault handler when a write
/// targets a frame that other PCs share (e.g. via prior `install_shared_page`
/// from a reflink). Allocates a zeroed frame, copies the old frame's bytes
/// through the substrate `FrameCopier` hook, and swaps the page-cache entry
/// via `install_if_match` so concurrent CoW from another writer linearizes:
/// only one of them succeeds in replacing the entry, the other observes the
/// updated state and retries.
///
/// On the success path the old `CachePin` is dropped, so the source frame's
/// `cache_ref` decrements; if no other holder remains the substrate frees
/// the source frame.
///
/// Returns the new private PPN. Errors: `MissingPage` if `page` is not
/// cached; `MismatchedFrame { current }` if the page was replaced
/// concurrently between our lookup and the install_if_match (caller can
/// retry); `Alloc` for substrate allocation failures.
pub fn cow_replace_into_private(
    pc: &PageContainer,
    page: PageIndex,
) -> Result<Ppn, PageCacheError> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return Err(PageCacheError::UnsupportedKind);
    }
    let (existing_ppn, generation) = {
        let mut state = pc.state.lock();
        let existing_ppn = state
            .pages
            .lookup(page)
            .ok_or(PageCacheError::MissingPage)?;
        let snapshot = state
            .ensure_resident_page_slot(page, existing_ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        (existing_ppn, snapshot.generation)
    };

    let reservation =
        page_allocator::reserve_frame(ZeroPolicy::Zeroed).map_err(PageCacheError::Alloc)?;
    let new_frame = reservation.commit();
    let new_ppn = new_frame.ppn();
    page_allocator::copy_frame_contents(existing_ppn, new_ppn).map_err(PageCacheError::Alloc)?;
    let new_cache_pin = new_frame.try_cache_pin().map_err(PageCacheError::Alloc)?;
    drop(new_frame);

    let replacement = CachedFrame {
        ppn: new_ppn,
        pin: PageCachePin::Allocated(new_cache_pin),
    };
    pc.replace_resident_if_match_published(page, existing_ppn, generation, replacement)
}
