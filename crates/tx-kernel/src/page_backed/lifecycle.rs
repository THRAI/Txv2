use super::*;
use alloc::vec::Vec;

impl PageCacheIndex {
    fn withdraw_from(&mut self, first: PageIndex) {
        drop(self.pages.split_off(&first));
    }

    fn dirty_pages(&self) -> Vec<(PageIndex, Ppn)> {
        self.pages
            .iter()
            .filter_map(|(page, entry)| entry.marks.dirty.then_some((*page, entry.ppn)))
            .collect()
    }

    fn clear_dirty_if_match(&mut self, page: PageIndex, ppn: Ppn) {
        let Some(entry) = self.pages.get_mut(&page) else {
            return;
        };
        if entry.ppn == ppn {
            entry.marks.dirty = false;
            entry.marks.writeback = false;
        }
    }
}

impl PageContainer {
    fn withdraw_cached_pages_from(&self, first: PageIndex) {
        self.state.lock().pages.withdraw_from(first);
    }

    fn dirty_pages_snapshot(&self) -> Vec<(PageIndex, Ppn)> {
        self.state.lock().pages.dirty_pages()
    }

    fn clear_dirty_if_match(&self, page: PageIndex, ppn: Ppn) {
        self.state.lock().pages.clear_dirty_if_match(page, ppn);
    }
}

pub fn step_truncate(pc: &PageContainer, new_size: u64, guard: &Guard<'_>) -> StepOutcome<()> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if new_size > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .fs_page_backing
            .truncate(*fs_object_id, new_size, guard)
        {
            StepOutcome::Done(()) => false,
            StepOutcome::Advanced(()) => true,
            StepOutcome::Blocked(token) => return StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked((), token) => {
                return StepOutcome::AdvancedThenBlocked((), token);
            }
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
        },
        PageContainerKind::Anon { .. } => false,
        PageContainerKind::Device { .. } => unreachable!(),
    };

    let old_size = pc.size_bytes();
    pc.set_size_bytes(new_size);

    if new_size < old_size {
        let Some(first_drop) = first_page_after_size(new_size) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        pc.withdraw_cached_pages_from(first_drop);
        zero_partial_eof_tail(pc, new_size);
    }

    if fs_advanced {
        StepOutcome::Advanced(())
    } else {
        StepOutcome::Done(())
    }
}

/// Zero the bytes in the cached page containing the new EOF, from the
/// in-page byte offset of `new_size` up to the page end. After
/// truncate-shrink past a non-aligned size, the partial last page must not
/// expose stale post-EOF bytes when later grown back into. No-op when
/// `new_size` falls on a page boundary, when the EOF page is not currently
/// cached, or when the substrate kernel-address hook is not installed.
fn zero_partial_eof_tail(pc: &PageContainer, new_size: u64) {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    let within_page = (new_size % page_size) as usize;
    if within_page == 0 || new_size == 0 {
        return;
    }
    let page_index = PageIndex::new(new_size / page_size);
    let Some(ppn) = pc.lookup(page_index) else {
        return;
    };
    let Ok(frame_base) = page_allocator::frame_kernel_addr(ppn) else {
        return;
    };
    let tail_len = (page_size as usize) - within_page;
    unsafe {
        core::ptr::write_bytes(frame_base.add(within_page), 0, tail_len);
    }
}

pub fn step_fsync(pc: &PageContainer, guard: &Guard<'_>) -> StepOutcome<()> {
    let PageContainerKind::File {
        mount,
        fs_object_id,
    } = pc.kind()
    else {
        return StepOutcome::Done(());
    };

    let mut progressed = false;
    for (page, ppn) in pc.dirty_pages_snapshot() {
        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        match mount
            .fs_page_backing
            .flush_page(*fs_object_id, offset, &Frame::new(ppn), guard)
        {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                pc.clear_dirty_if_match(page, ppn);
                progressed = true;
            }
            StepOutcome::Blocked(token) => {
                return if progressed {
                    StepOutcome::AdvancedThenBlocked((), token)
                } else {
                    StepOutcome::Blocked(token)
                };
            }
            StepOutcome::AdvancedThenBlocked((), token) => {
                pc.clear_dirty_if_match(page, ppn);
                return StepOutcome::AdvancedThenBlocked((), token);
            }
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
        }
    }

    match mount.fs_page_backing.fsync(*fs_object_id, guard) {
        StepOutcome::Blocked(token) if progressed => StepOutcome::AdvancedThenBlocked((), token),
        other => other,
    }
}

/// Reserve space up to `new_size` for future writes per PAGE_BACKED §5.5.
///
/// - Device backings reject with `EINVAL` (the device aperture is fixed).
/// - `new_size` beyond the fixed `page_count` capacity rejects with
///   `EINVAL`; the capacity bound is preserved.
/// - `new_size <= pc.size_bytes()` is a `Done(())` no-op (fallocate cannot
///   shrink; that is the truncate path).
/// - File backings call `FsPageBacking::fallocate` first; on backing
///   success, `pc.size_bytes` is published. Pages are not materialized.
/// - Anon backings simply publish the new visible size; per spec this is
///   "mostly a hint".
pub fn step_fallocate(pc: &PageContainer, new_size: u64, guard: &Guard<'_>) -> StepOutcome<()> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if new_size > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    if new_size <= pc.size_bytes() {
        return StepOutcome::Done(());
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .fs_page_backing
            .fallocate(*fs_object_id, new_size, guard)
        {
            StepOutcome::Done(()) => false,
            StepOutcome::Advanced(()) => true,
            StepOutcome::Blocked(token) => return StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked((), token) => {
                return StepOutcome::AdvancedThenBlocked((), token);
            }
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
        },
        PageContainerKind::Anon { .. } => false,
        PageContainerKind::Device { .. } => unreachable!(),
    };

    pc.set_size_bytes(new_size);

    if fs_advanced {
        StepOutcome::Advanced(())
    } else {
        StepOutcome::Done(())
    }
}

fn first_page_after_size(size: u64) -> Option<PageIndex> {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    if size == 0 {
        return Some(PageIndex::new(0));
    }
    size.checked_add(page_size - 1)
        .map(|rounded| PageIndex::new(rounded / page_size))
}
