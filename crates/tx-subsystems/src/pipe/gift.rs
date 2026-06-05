use super::*;

impl PipeRing {
    fn reserve_user_page_gift_slot(&mut self) -> bool {
        if self.is_full() {
            return false;
        }
        self.reserved_gift_slots += 1;
        true
    }

    fn release_user_page_gift_slot(&mut self) {
        if self.reserved_gift_slots > 0 {
            self.reserved_gift_slots -= 1;
        }
    }

    fn commit_reserved_user_page_gift_slot(
        &mut self,
        gift: crate::vm::UserPageGift,
        packet_mode: bool,
    ) -> Result<usize, crate::vm::UserPageGift> {
        if self.reserved_gift_slots == 0 {
            return Err(gift);
        }
        self.reserved_gift_slots -= 1;
        let len = gift.len();
        self.bytes += len;
        self.bufs.push_back(PipeBuf {
            storage: PipeStorage::UserPageGift(gift),
            offset: 0,
            len,
            flags: if packet_mode { PIPE_BUF_FLAG_PACKET } else { 0 },
        });
        Ok(len)
    }

    fn push_user_page_gift(
        &mut self,
        gift: crate::vm::UserPageGift,
        packet_mode: bool,
    ) -> Result<(), crate::vm::UserPageGift> {
        if self.is_full() {
            return Err(gift);
        }
        let len = gift.len();
        self.bytes += len;
        self.bufs.push_back(PipeBuf {
            storage: PipeStorage::UserPageGift(gift),
            offset: 0,
            len,
            flags: if packet_mode { PIPE_BUF_FLAG_PACKET } else { 0 },
        });
        Ok(())
    }

    fn pop_front_user_page_gift(&mut self) -> Option<PipeBuf> {
        let front = self.bufs.front()?;
        if !matches!(front.storage, PipeStorage::UserPageGift(_)) {
            return None;
        }
        if front.offset != 0 || front.len != crate::vm::USER_PAGE_SIZE {
            return None;
        }
        let buf = self.bufs.pop_front()?;
        self.bytes -= buf.len;
        Some(buf)
    }
}

pub struct UserPageGiftSlot {
    payload: Cap<PipePayload>,
    packet_mode: bool,
    active: bool,
}

impl UserPageGiftSlot {
    pub fn commit(
        mut self,
        gift: crate::vm::UserPageGift,
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        if gift.is_empty() {
            self.release();
            return step_engine::done_bytes(0);
        }
        let outcome = {
            let mut ring = self.payload.ring.lock();
            match ring.commit_reserved_user_page_gift_slot(gift, self.packet_mode) {
                Ok(len) => {
                    self.active = false;
                    StepOutcome::Done(len)
                }
                Err(_gift) => StepOutcome::Err(step_engine::Errno::EINVAL),
            }
        };
        if matches!(outcome, StepOutcome::Done(_)) {
            notification::notify_readable(
                &self.payload.reader_wait_channel,
                &self.payload.reader_wait_source,
            );
        }
        outcome
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        {
            let mut ring = self.payload.ring.lock();
            ring.release_user_page_gift_slot();
        }
        notification::notify_writable(
            &self.payload.writer_wait_channel,
            &self.payload.writer_wait_source,
        );
    }
}

impl Drop for UserPageGiftSlot {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn step_reserve_user_page_gift_slot(
    payload: &Cap<PipePayload>,
    _guard: &Guard<'_>,
    nonblocking: bool,
    packet_mode: bool,
) -> StepOutcome<UserPageGiftSlot, ByteProgress> {
    if payload.reader_count.load(Ordering::Acquire) == 0 {
        return StepOutcome::Err(step_engine::Errno::EPIPE);
    }
    let mut ring = payload.ring.lock();
    if ring.reserve_user_page_gift_slot() {
        return StepOutcome::Done(UserPageGiftSlot {
            payload: payload.clone(),
            packet_mode,
            active: true,
        });
    }
    drop(ring);
    if nonblocking {
        return StepOutcome::Err(step_engine::Errno::EAGAIN);
    }
    notification::yield_until_writable(payload.writer_wait_source_id)
}

pub fn step_push_user_page_gift(
    payload: &Cap<PipePayload>,
    gift: crate::vm::UserPageGift,
    _guard: &Guard<'_>,
    nonblocking: bool,
    packet_mode: bool,
) -> StepOutcome<usize, ByteProgress> {
    if gift.is_empty() {
        return step_engine::done_bytes(0);
    }
    if payload.reader_count.load(Ordering::Acquire) == 0 {
        return step_engine::epipe();
    }
    let len = gift.len();
    let mut ring = payload.ring.lock();
    match ring.push_user_page_gift(gift, packet_mode) {
        Ok(()) => {
            drop(ring);
            notification::notify_readable(
                &payload.reader_wait_channel,
                &payload.reader_wait_source,
            );
            step_engine::done_bytes(len)
        }
        Err(_gift) if nonblocking => step_engine::eagain(),
        Err(_gift) => notification::wait_until_writable(payload.writer_wait_source_id),
    }
}

pub fn step_pop_user_page_gift(
    payload: &Cap<PipePayload>,
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<Option<crate::vm::UserPageGift>, ByteProgress> {
    let mut ring = payload.ring.lock();
    if let Some(buf) = ring.pop_front_user_page_gift() {
        drop(ring);
        notification::notify_writable(&payload.writer_wait_channel, &payload.writer_wait_source);
        return match buf.storage {
            PipeStorage::UserPageGift(gift) => StepOutcome::Done(Some(gift)),
            PipeStorage::AnonPage(_) | PipeStorage::PageBackedLease(_) => StepOutcome::Done(None),
        };
    }
    if !ring.is_empty() {
        return StepOutcome::Done(None);
    }
    drop(ring);
    if payload.writer_count.load(Ordering::Acquire) == 0 {
        return StepOutcome::Done(None);
    }
    if nonblocking {
        return StepOutcome::Err(step_engine::Errno::EAGAIN);
    }
    notification::yield_until_readable(payload.reader_wait_source_id)
}
