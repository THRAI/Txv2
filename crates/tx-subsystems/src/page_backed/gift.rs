use super::*;

impl PageContainer {
    pub fn install_user_gift_or_copy(
        &self,
        page: PageIndex,
        gift: crate::vm::UserPageGift,
    ) -> Result<bool, PageCacheError> {
        if matches!(self.kind(), PageContainerKind::Device { .. }) {
            return Err(PageCacheError::UnsupportedKind);
        }
        self.check_bounds(page)?;

        let ppn = gift.ppn();
        let cache_pin = page_allocator::acquire_cache_pin(ppn).map_err(PageCacheError::Alloc)?;
        let frame = CachedFrame {
            ppn,
            pin: PageCachePin::Allocated(cache_pin),
        };

        if self.install_resident_if_absent_published_with(page, frame, |slot| {
            slot.mark_dirty()
                .map_err(page_slot_completion_error_to_page_cache_error)
                .map(|_| ())
        })? {
            return Ok(true);
        }

        let current = self.lookup(page).ok_or(PageCacheError::MissingPage)?;
        page_allocator::copy_frame_contents(ppn, current).map_err(PageCacheError::Alloc)?;
        let mut state = self.state.lock();
        let observed = state
            .pages
            .lookup(page)
            .ok_or(PageCacheError::MissingPage)?;
        if observed != current {
            return Err(PageCacheError::MismatchedFrame { current: observed });
        }
        state
            .ensure_resident_page_slot(page, current)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        state
            .page_slots
            .get(&page)
            .expect("gift destination has a PageSlot")
            .mark_dirty()
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        Ok(false)
    }
}
