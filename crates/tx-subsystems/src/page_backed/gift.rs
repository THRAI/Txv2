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

        let mut state = self.state.lock();
        match state.pages.install_if_absent(page, frame) {
            Ok(()) => {
                state.pages.mark_dirty(page)?;
                Ok(true)
            }
            Err(PageCacheError::AlreadyPresent { current }) => {
                drop(state);
                page_allocator::copy_frame_contents(ppn, current).map_err(PageCacheError::Alloc)?;
                let mut state = self.state.lock();
                state.pages.mark_dirty(page)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}
