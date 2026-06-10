use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tx_hal::Ppn;

use super::{
    AllocError, AllocatorBackendKind, AllocatorDiagnostics, FrameMeta, FrameReservation,
    FrameRunReservation, FrameZeroer, PageAllocator, PermanentFrame, ZeroPolicy,
};

/// v1 global atomic-bitmap page allocator.
///
/// The backend covers a dense raw-PPN range `[base_ppn, base_ppn + total)`.
/// One metadata row and one bitmap bit exist for every covered PPN, including
/// holes and reserved ranges. Only bitmap bits explicitly populated during boot
/// are allocatable.
pub struct BitmapPageAllocator<'a> {
    metas: &'a [FrameMeta],
    bitmap: &'a [AtomicU64],
    base_ppn: Ppn,
    total: usize,
    free: AtomicUsize,
    hint: AtomicUsize,
    zero_frame: Option<FrameZeroer>,
}

impl<'a> BitmapPageAllocator<'a> {
    /// Create a bitmap backend without a zeroing hook.
    ///
    /// `ZeroPolicy::Zeroed` reservations will fail with
    /// `AllocError::ZeroScrubUnavailable` until `new_with_zeroer` is used by the
    /// boot handoff path.
    pub const fn new(metas: &'a [FrameMeta], bitmap: &'a [AtomicU64], total: usize) -> Self {
        Self::new_with_base(metas, bitmap, Ppn(0), total)
    }

    /// Create a dense bitmap backend covering raw PPNs starting at `base_ppn`.
    pub const fn new_with_base(
        metas: &'a [FrameMeta],
        bitmap: &'a [AtomicU64],
        base_ppn: Ppn,
        total: usize,
    ) -> Self {
        Self {
            metas,
            bitmap,
            base_ppn,
            total,
            free: AtomicUsize::new(0),
            hint: AtomicUsize::new(0),
            zero_frame: None,
        }
    }

    /// Create a bitmap backend with a direct-map zeroing hook.
    pub const fn new_with_zeroer(
        metas: &'a [FrameMeta],
        bitmap: &'a [AtomicU64],
        total: usize,
        zero_frame: FrameZeroer,
    ) -> Self {
        Self::new_with_base_and_zeroer(metas, bitmap, Ppn(0), total, zero_frame)
    }

    /// Create a dense bitmap backend with a direct-map zeroing hook.
    pub const fn new_with_base_and_zeroer(
        metas: &'a [FrameMeta],
        bitmap: &'a [AtomicU64],
        base_ppn: Ppn,
        total: usize,
        zero_frame: FrameZeroer,
    ) -> Self {
        Self {
            metas,
            bitmap,
            base_ppn,
            total,
            free: AtomicUsize::new(0),
            hint: AtomicUsize::new(0),
            zero_frame: Some(zero_frame),
        }
    }

    #[doc(hidden)]
    pub const fn new_for_test(
        metas: &'a [FrameMeta],
        bitmap: &'a [AtomicU64],
        total: usize,
    ) -> Self {
        Self::new(metas, bitmap, total)
    }

    #[doc(hidden)]
    pub const fn new_with_base_for_test(
        metas: &'a [FrameMeta],
        bitmap: &'a [AtomicU64],
        base_ppn: Ppn,
        total: usize,
    ) -> Self {
        Self::new_with_base(metas, bitmap, base_ppn, total)
    }

    #[doc(hidden)]
    /// Test helper: mark a frame as globally free.
    pub fn mark_free_for_test(&self, ppn: Ppn) {
        self.meta(ppn).force_free_state();
        let inserted = self.set_free_bit(ppn);
        if inserted {
            self.free.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[doc(hidden)]
    /// Test helper: remove a frame from the pool and set its reserved flag.
    pub fn mark_reserved_for_test(&self, ppn: Ppn) {
        let was_free = self.clear_free_bit(ppn);
        if was_free {
            self.free.fetch_sub(1, Ordering::AcqRel);
        }
        let meta = self.meta(ppn);
        meta.force_free_state();
        meta.mark_reserved();
    }

    #[doc(hidden)]
    /// Test helper: read the backend bitmap bit for a frame.
    pub fn is_free_for_test(&self, ppn: Ppn) -> bool {
        self.bit_is_set(ppn)
    }

    #[doc(hidden)]
    /// Boot helper: mark a frame inside allocator coverage as initially free.
    pub fn mark_free_for_boot(&self, ppn: Ppn) {
        let meta = self.meta(ppn);
        meta.force_free_state();
        meta.clear_reserved();
        let inserted = self.set_free_bit(ppn);
        if inserted {
            self.free.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Claim a free frame as a never-free permanent anchor.
    ///
    /// This is used for objects such as the zero frame or kernel metadata
    /// anchors. It removes the frame from the bitmap, sets `refcount = 1`, and
    /// marks the frame `reserved | direct_mapped`.
    pub fn claim_permanent_frame(&self, ppn: Ppn) -> Result<PermanentFrame<'_, Self>, AllocError> {
        let was_free = self.clear_free_bit(ppn);
        if was_free {
            self.free.fetch_sub(1, Ordering::AcqRel);
        }

        let meta = self.meta(ppn);
        if let Err(err) = meta.claim_permanent() {
            if was_free {
                self.set_free_bit(ppn);
                self.free.fetch_add(1, Ordering::AcqRel);
            }
            return Err(err);
        }

        meta.mark_reserved();
        meta.mark_direct_mapped();
        Ok(PermanentFrame::new(self, ppn))
    }

    fn meta(&self, ppn: Ppn) -> &'a FrameMeta {
        let index = self.dense_index(ppn);
        assert!(index < self.metas.len());
        &self.metas[index]
    }

    fn dense_index(&self, ppn: Ppn) -> usize {
        let index = ppn
            .0
            .checked_sub(self.base_ppn.0)
            .expect("PPN below bitmap allocator base");
        assert!(index < self.total);
        index
    }

    fn ppn_from_dense_index(&self, index: usize) -> Ppn {
        assert!(index < self.total);
        Ppn(self.base_ppn.0 + index)
    }

    fn word_count(&self) -> usize {
        self.bitmap.len()
    }

    fn bit_location(&self, ppn: Ppn) -> (usize, u64) {
        let index = self.dense_index(ppn);
        let word_index = index / 64;
        assert!(word_index < self.bitmap.len());
        (word_index, 1u64 << (index % 64))
    }

    fn bit_is_set(&self, ppn: Ppn) -> bool {
        let (word_index, bit) = self.bit_location(ppn);
        self.bitmap[word_index].load(Ordering::Acquire) & bit != 0
    }

    fn set_free_bit(&self, ppn: Ppn) -> bool {
        let (word_index, bit) = self.bit_location(ppn);
        let old = self.bitmap[word_index].fetch_or(bit, Ordering::AcqRel);
        old & bit == 0
    }

    fn clear_free_bit(&self, ppn: Ppn) -> bool {
        let (word_index, bit) = self.bit_location(ppn);
        loop {
            let old = self.bitmap[word_index].load(Ordering::Acquire);
            if old & bit == 0 {
                return false;
            }

            let new = old & !bit;
            match self.bitmap[word_index].compare_exchange_weak(
                old,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    fn reserve_single_ppn(&self) -> Result<Ppn, AllocError> {
        let word_count = self.word_count();
        if self.total == 0 || word_count == 0 {
            return Err(AllocError::Exhausted);
        }

        let start = self.hint.load(Ordering::Relaxed) % word_count;
        for offset in 0..word_count {
            let word_index = (start + offset) % word_count;
            let ppn = match self.take_first_free_from_word(word_index) {
                Some(ppn) => ppn,
                None => continue,
            };

            self.hint.store(word_index, Ordering::Relaxed);
            self.free.fetch_sub(1, Ordering::AcqRel);
            // The bitmap is the allocation linearization point. If boot data
            // left an impossible bitmap/meta combination behind, quarantine the
            // frame by keeping its bit clear and continue scanning.
            if self.meta(ppn).is_reserved() || self.meta(ppn).state() != 0 {
                continue;
            }
            return Ok(ppn);
        }

        Err(AllocError::Exhausted)
    }

    fn take_first_free_from_word(&self, word_index: usize) -> Option<Ppn> {
        loop {
            let old = self.bitmap[word_index].load(Ordering::Acquire);
            let masked = self.mask_word(word_index, old);
            if masked == 0 {
                return None;
            }

            let bit_index = masked.trailing_zeros() as usize;
            let bit = 1u64 << bit_index;
            let new = old & !bit;
            match self.bitmap[word_index].compare_exchange_weak(
                old,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(self.ppn_from_dense_index(word_index * 64 + bit_index)),
                Err(_) => continue,
            }
        }
    }

    fn mask_word(&self, word_index: usize, word: u64) -> u64 {
        let first_index = word_index * 64;
        if first_index >= self.total {
            return 0;
        }

        let valid_bits = self.total - first_index;
        if valid_bits >= 64 {
            word
        } else {
            word & ((1u64 << valid_bits) - 1)
        }
    }

    fn reserve_contiguous_ppns(&self, count: usize, align: usize) -> Result<Ppn, AllocError> {
        if count == 0 || align == 0 || count > self.total {
            return Err(AllocError::InvalidRequest);
        }

        let range_end = self
            .base_ppn
            .0
            .checked_add(self.total)
            .ok_or(AllocError::InvalidRequest)?;
        let range_base = align_up(self.base_ppn.0, align).ok_or(AllocError::InvalidRequest)?;

        // Next-fit with a free-bit fast-skip. First-fit from the range base
        // walks the whole allocated low-memory prefix on every call — once a
        // long-running boot fills low frames, each reserve_run(1, ..) (the
        // ext4 block-read path) degrades to an O(total) meta scan and a large
        // exec crawls for minutes. Resume near the word the singles/runs hint
        // points at instead, wrap around once, and skip allocated stretches
        // via the bitmap words rather than per-frame meta loads.
        let word_count = self.word_count();
        if self.total == 0 || word_count == 0 {
            return Err(AllocError::Exhausted);
        }
        let hint_word = self.hint.load(Ordering::Relaxed) % word_count;
        let hint_ppn = self.ppn_from_dense_index(hint_word * 64).0;
        let hint_base = align_up(hint_ppn.max(range_base), align)
            .unwrap_or(range_base)
            .min(range_end);

        // Pass 1 scans bases in [hint, range_end); pass 2 wraps to bases in
        // [range_base, hint). A run may always extend up to range_end — only
        // the candidate BASE is bounded per pass, so runs straddling the hint
        // stay reachable from pass 2.
        for (start, base_limit) in [(hint_base, range_end), (range_base, hint_base)] {
            let mut base = start;
            while base < base_limit
                && base.checked_add(count).is_some_and(|run_end| run_end <= range_end)
            {
                if !self.frame_bit_is_free(Ppn(base)) {
                    let Some(next_free) = self.next_free_ppn_at_or_after(base + 1, base_limit)
                    else {
                        break;
                    };
                    let Some(aligned) = align_up(next_free, align) else {
                        break;
                    };
                    base = aligned;
                    continue;
                }
                if self.try_reserve_run_at(Ppn(base), count) {
                    self.free.fetch_sub(count, Ordering::AcqRel);
                    self.hint
                        .store(self.dense_index(Ppn(base)) / 64, Ordering::Relaxed);
                    return Ok(Ppn(base));
                }

                base = match base.checked_add(align) {
                    Some(next) => next,
                    None => break,
                };
            }
        }

        Err(AllocError::Exhausted)
    }

    /// Whether `ppn`'s free bit is currently set (candidate for reservation).
    fn frame_bit_is_free(&self, ppn: Ppn) -> bool {
        let dense = self.dense_index(ppn);
        let word = self.bitmap[dense / 64].load(Ordering::Acquire);
        self.mask_word(dense / 64, word) & (1u64 << (dense % 64)) != 0
    }

    /// First ppn in `[from, end)` whose free bit is set, scanning whole bitmap
    /// words so fully-allocated stretches cost one load per 64 frames.
    fn next_free_ppn_at_or_after(&self, from: usize, end: usize) -> Option<usize> {
        if from >= end {
            return None;
        }
        let from_dense = self.dense_index(Ppn(from));
        let end_dense = from_dense + (end - from);
        let mut word_index = from_dense / 64;
        let mut bit_floor = from_dense % 64;
        while word_index * 64 < end_dense {
            let word = self.mask_word(word_index, self.bitmap[word_index].load(Ordering::Acquire));
            let word = if bit_floor == 0 {
                word
            } else {
                word & !((1u64 << bit_floor) - 1)
            };
            if word != 0 {
                let dense = word_index * 64 + word.trailing_zeros() as usize;
                if dense >= end_dense {
                    return None;
                }
                return Some(self.ppn_from_dense_index(dense).0);
            }
            word_index += 1;
            bit_floor = 0;
        }
        None
    }

    fn try_reserve_run_at(&self, base: Ppn, count: usize) -> bool {
        // Check metadata before touching bitmap bits so reserved holes do not
        // create a partial reservation that would need semantic rollback.
        for offset in 0..count {
            let meta = self.meta(Ppn(base.0 + offset));
            if meta.is_reserved() || meta.state() != 0 {
                return false;
            }
        }

        let mut claimed = 0usize;
        while claimed < count {
            let ppn = Ppn(base.0 + claimed);
            if self.clear_free_bit(ppn) {
                claimed += 1;
                continue;
            }

            // These bits were only candidate claims. `free` is decremented
            // after the whole run succeeds, so this rollback restores bits
            // without touching the diagnostic count.
            self.restore_candidate_run(base, claimed);
            return false;
        }

        true
    }

    fn restore_candidate_run(&self, base: Ppn, count: usize) {
        for offset in 0..count {
            self.set_free_bit(Ppn(base.0 + offset));
        }
    }

    fn return_claimed_run(&self, base: Ppn, count: usize) {
        for offset in 0..count {
            let ppn = Ppn(base.0 + offset);
            if self.set_free_bit(ppn) {
                self.free.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    fn return_to_free_pool(&self, ppn: Ppn) {
        let meta = self.meta(ppn);
        // Reserved frames are owned by a typed permanent/pmap path, never by the
        // normal allocator pool. A mistaken normal release is contained here.
        if meta.is_reserved() {
            return;
        }

        let inserted = self.set_free_bit(ppn);
        if inserted {
            self.free.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn release_when_zero(&self, ppn: Ppn, state_after: u32) {
        // Every role/owner drop funnels through this check. The final dropper is
        // the one that observes packed state zero and returns the frame.
        if state_after == 0 {
            self.return_to_free_pool(ppn);
        }
    }

    fn finish_zero_policy(&self, ppn: Ppn, policy: ZeroPolicy) -> Result<(), AllocError> {
        match policy {
            ZeroPolicy::UninitFullOverwrite => Ok(()),
            ZeroPolicy::Zeroed => {
                let Some(zero_frame) = self.zero_frame else {
                    return Err(AllocError::ZeroScrubUnavailable);
                };

                unsafe { zero_frame(ppn) };
                Ok(())
            }
        }
    }
}

impl PageAllocator for BitmapPageAllocator<'_> {
    fn reserve_frame(&self, policy: ZeroPolicy) -> Result<FrameReservation<'_, Self>, AllocError> {
        let ppn = self.reserve_single_ppn()?;
        if let Err(err) = self.finish_zero_policy(ppn, policy) {
            self.rollback_reserved(ppn);
            return Err(err);
        }
        Ok(FrameReservation::new(self, ppn))
    }

    fn reserve_run(
        &self,
        count: usize,
        align: usize,
        policy: ZeroPolicy,
    ) -> Result<FrameRunReservation<'_, Self>, AllocError> {
        let base = self.reserve_contiguous_ppns(count, align)?;
        for offset in 0..count {
            if let Err(err) = self.finish_zero_policy(Ppn(base.0 + offset), policy) {
                self.rollback_reserved_run(base, count);
                return Err(err);
            }
        }
        Ok(FrameRunReservation::new(self, base, count))
    }

    fn free_count(&self) -> usize {
        self.free.load(Ordering::Acquire)
    }

    fn total_count(&self) -> usize {
        self.total
    }

    fn backend_diagnostics(&self) -> AllocatorDiagnostics {
        AllocatorDiagnostics {
            backend: AllocatorBackendKind::Bitmap,
            base_ppn: self.base_ppn,
            total_count: self.total_count(),
            free_count: self.free_count(),
            scan_hint: self.hint.load(Ordering::Acquire),
        }
    }

    fn commit_reserved(&self, ppn: Ppn) {
        // Committing is the publish boundary: after this succeeds, the frame is
        // live and role counters may be acquired.
        self.meta(ppn)
            .claim_owned()
            .expect("reserved allocator frame must be claimable");
    }

    fn commit_reserved_run(&self, base: Ppn, count: usize) {
        let mut committed = 0usize;
        while committed < count {
            let ppn = Ppn(base.0 + committed);
            match self.meta(ppn).claim_owned() {
                Ok(()) => committed += 1,
                Err(_) => {
                    for offset in 0..committed {
                        let rollback_ppn = Ppn(base.0 + offset);
                        let state_after = self
                            .meta(rollback_ppn)
                            .decrement_refcount()
                            .expect("fresh run rollback owns refcount");
                        self.release_when_zero(rollback_ppn, state_after);
                    }
                    panic!("reserved allocator run must be claimable");
                }
            }
        }
    }

    fn rollback_reserved(&self, ppn: Ppn) {
        self.return_to_free_pool(ppn);
    }

    fn rollback_reserved_run(&self, base: Ppn, count: usize) {
        self.return_claimed_run(base, count);
    }

    fn acquire_map_pin(&self, ppn: Ppn) -> Result<(), AllocError> {
        self.meta(ppn).increment_map_count()
    }

    fn release_map_pin(&self, ppn: Ppn) {
        let state_after = self
            .meta(ppn)
            .decrement_map_count()
            .expect("map pin release must match an acquired map pin");
        self.release_when_zero(ppn, state_after);
    }

    fn acquire_cache_pin(&self, ppn: Ppn) -> Result<(), AllocError> {
        self.meta(ppn).increment_cache_ref()
    }

    fn release_cache_pin(&self, ppn: Ppn) {
        let state_after = self
            .meta(ppn)
            .decrement_cache_ref()
            .expect("cache pin release must match an acquired cache pin");
        self.release_when_zero(ppn, state_after);
    }

    fn acquire_dma_pin(&self, ppn: Ppn) -> Result<(), AllocError> {
        self.meta(ppn).increment_pin_count()
    }

    fn release_dma_pin(&self, ppn: Ppn) {
        let state_after = self
            .meta(ppn)
            .decrement_pin_count()
            .expect("DMA pin release must match an acquired DMA pin");
        self.release_when_zero(ppn, state_after);
    }

    fn acquire_gift_pin(&self, ppn: Ppn) -> Result<(), AllocError> {
        self.meta(ppn).increment_refcount()
    }

    fn release_gift_pin(&self, ppn: Ppn) {
        let state_after = self
            .meta(ppn)
            .decrement_refcount()
            .expect("gift pin release must match an acquired gift pin");
        self.release_when_zero(ppn, state_after);
    }

    fn release_owned(&self, ppn: Ppn) {
        let state_after = self
            .meta(ppn)
            .decrement_refcount()
            .expect("owned frame release must match an owned frame");
        self.release_when_zero(ppn, state_after);
    }

    fn adopt_page_table_frame(&self, ppn: Ppn) {
        // Page-table pages keep their refcount but move onto the pmap-only
        // teardown path by becoming reserved/direct-mapped.
        let meta = self.meta(ppn);
        meta.mark_reserved();
        meta.mark_direct_mapped();
    }

    fn adopt_permanent_frame(&self, ppn: Ppn) {
        // Permanent anchors keep their refcount for the kernel lifetime and
        // cannot be returned by the normal allocator path.
        let meta = self.meta(ppn);
        meta.mark_reserved();
        meta.mark_direct_mapped();
    }

    fn release_page_table_frame(&self, ppn: Ppn) {
        // Pmap teardown clears the reserved flags first so releasing the owned
        // refcount can return the frame if no other role counters remain.
        let meta = self.meta(ppn);
        meta.clear_reserved();
        meta.clear_direct_mapped();
        self.release_owned(ppn);
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}
