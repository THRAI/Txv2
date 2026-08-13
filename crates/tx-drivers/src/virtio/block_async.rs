//! Transport-neutral bookkeeping for non-blocking VirtIO block requests.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use tx_subsystems::device::BlockAsyncCompletion;
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::Frame;
use virtio_drivers::device::blk::{BlkReq, BlkResp};

/// Bound each descriptor even when L6 merged a long contiguous run. This is
/// also the largest physically-contiguous bounce allocation requested by one
/// data descriptor on platforms which cannot DMA from the direct map.
pub(super) const MAX_DMA_CHUNK_PAGES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AsyncBlockOp {
    Read,
    Write,
}

pub(super) struct PendingChunk {
    pub(super) lba: usize,
    pub(super) frames: Vec<Frame>,
}

pub(super) struct InFlightChunk {
    pub(super) cookie: u64,
    pub(super) op: AsyncBlockOp,
    pub(super) chunk: PendingChunk,
    pub(super) request: Box<BlkReq>,
    pub(super) response: Box<BlkResp>,
}

struct PendingLogicalRequest {
    op: AsyncBlockOp,
    queued: VecDeque<PendingChunk>,
    in_flight: usize,
    result: Result<(), Errno>,
}

#[derive(Default)]
pub(super) struct AsyncBlockState {
    logical: BTreeMap<u64, PendingLogicalRequest>,
    pub(super) in_flight: BTreeMap<u16, InFlightChunk>,
    ready: VecDeque<BlockAsyncCompletion>,
}

impl AsyncBlockState {
    pub(super) const fn new() -> Self {
        Self {
            logical: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            ready: VecDeque::new(),
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.logical.is_empty()
    }

    pub(super) fn stash_ready(
        &mut self,
        completions: impl IntoIterator<Item = BlockAsyncCompletion>,
    ) {
        self.ready.extend(completions);
    }

    pub(super) fn take_ready(&mut self, budget: usize) -> Vec<BlockAsyncCompletion> {
        let count = budget.min(self.ready.len());
        self.ready.drain(..count).collect()
    }

    pub(super) fn enqueue(
        &mut self,
        cookie: u64,
        op: AsyncBlockOp,
        first_lba: u64,
        sectors_per_page: u32,
        frames: &[Frame],
    ) -> Result<(), Errno> {
        if frames.is_empty() || sectors_per_page == 0 || self.logical.contains_key(&cookie) {
            return Err(Errno::EINVAL);
        }

        let mut chunks = VecDeque::new();
        let mut first = 0usize;
        while first < frames.len() {
            let mut end = first + 1;
            while end < frames.len()
                && end - first < MAX_DMA_CHUNK_PAGES
                && frames[end].ppn().0 == frames[end - 1].ppn().0.saturating_add(1)
            {
                end += 1;
            }
            let sector_offset = (first as u64)
                .checked_mul(sectors_per_page as u64)
                .ok_or(Errno::EINVAL)?;
            let lba = first_lba
                .checked_add(sector_offset)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(Errno::EINVAL)?;
            chunks.push_back(PendingChunk {
                lba,
                frames: frames[first..end].to_vec(),
            });
            first = end;
        }

        self.logical.insert(
            cookie,
            PendingLogicalRequest {
                op,
                queued: chunks,
                in_flight: 0,
                result: Ok(()),
            },
        );
        Ok(())
    }

    pub(super) fn next_chunk(&mut self) -> Option<(u64, AsyncBlockOp, PendingChunk)> {
        self.logical.iter_mut().find_map(|(cookie, request)| {
            if request.result.is_ok() {
                request
                    .queued
                    .pop_front()
                    .map(|chunk| (*cookie, request.op, chunk))
            } else {
                None
            }
        })
    }

    pub(super) fn requeue_front(&mut self, cookie: u64, chunk: PendingChunk) {
        if let Some(request) = self.logical.get_mut(&cookie) {
            request.queued.push_front(chunk);
        }
    }

    pub(super) fn submitted(&mut self, token: u16, part: InFlightChunk) -> Result<(), Errno> {
        let Some(request) = self.logical.get_mut(&part.cookie) else {
            return Err(Errno::EIO);
        };
        if self.in_flight.contains_key(&token) {
            return Err(Errno::EIO);
        }
        request.in_flight = request.in_flight.saturating_add(1);
        self.in_flight.insert(token, part);
        Ok(())
    }

    pub(super) fn fail_submission(
        &mut self,
        cookie: u64,
        error: Errno,
    ) -> Option<BlockAsyncCompletion> {
        let request = self.logical.get_mut(&cookie)?;
        request.queued.clear();
        request.result = Err(error);
        self.finish_if_terminal(cookie)
    }

    pub(super) fn finish_part(
        &mut self,
        cookie: u64,
        result: Result<(), Errno>,
    ) -> Option<BlockAsyncCompletion> {
        let request = self.logical.get_mut(&cookie)?;
        request.in_flight = request.in_flight.saturating_sub(1);
        if result.is_err() && request.result.is_ok() {
            request.result = result;
            request.queued.clear();
        }
        self.finish_if_terminal(cookie)
    }

    fn finish_if_terminal(&mut self, cookie: u64) -> Option<BlockAsyncCompletion> {
        let request = self.logical.get(&cookie)?;
        if request.in_flight != 0 || !request.queued.is_empty() {
            return None;
        }
        let result = request.result;
        self.logical.remove(&cookie);
        Some(BlockAsyncCompletion::new(cookie, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::Ppn;

    #[test]
    fn enqueue_splits_noncontiguous_and_long_runs() {
        let mut state = AsyncBlockState::new();
        let mut frames = (0..MAX_DMA_CHUNK_PAGES + 3)
            .map(|offset| Frame::new(Ppn(100 + offset)))
            .collect::<Vec<_>>();
        frames.push(Frame::new(Ppn(1000)));
        state
            .enqueue(8, AsyncBlockOp::Write, 200, 8, &frames)
            .unwrap();

        let (_, _, first) = state.next_chunk().unwrap();
        assert_eq!(first.frames.len(), MAX_DMA_CHUNK_PAGES);
        let (_, _, second) = state.next_chunk().unwrap();
        assert_eq!(second.frames.len(), 3);
        assert_eq!(second.lba, 200 + (MAX_DMA_CHUNK_PAGES * 8));
        let (_, _, third) = state.next_chunk().unwrap();
        assert_eq!(third.frames.len(), 1);
    }

    #[test]
    fn logical_completion_waits_for_all_submitted_chunks() {
        let mut state = AsyncBlockState::new();
        let frames = [Frame::new(Ppn(10)), Frame::new(Ppn(20))];
        state.enqueue(9, AsyncBlockOp::Read, 0, 8, &frames).unwrap();

        for token in [1, 2] {
            let (cookie, op, chunk) = state.next_chunk().unwrap();
            state
                .submitted(
                    token,
                    InFlightChunk {
                        cookie,
                        op,
                        chunk,
                        request: Box::new(BlkReq::default()),
                        response: Box::new(BlkResp::default()),
                    },
                )
                .unwrap();
        }
        assert!(state.finish_part(9, Ok(())).is_none());
        assert_eq!(
            state.finish_part(9, Ok(())),
            Some(BlockAsyncCompletion::new(9, Ok(())))
        );
    }

    #[test]
    fn failed_part_waits_for_other_inflight_dma() {
        let mut state = AsyncBlockState::new();
        let frames = [
            Frame::new(Ppn(10)),
            Frame::new(Ppn(20)),
            Frame::new(Ppn(30)),
        ];
        state
            .enqueue(11, AsyncBlockOp::Read, 0, 8, &frames)
            .unwrap();
        for token in [1, 2] {
            let (cookie, op, chunk) = state.next_chunk().unwrap();
            state
                .submitted(
                    token,
                    InFlightChunk {
                        cookie,
                        op,
                        chunk,
                        request: Box::new(BlkReq::default()),
                        response: Box::new(BlkResp::default()),
                    },
                )
                .unwrap();
        }

        let failed = state.in_flight.remove(&1).unwrap();
        assert_eq!(failed.cookie, 11);
        assert!(state.finish_part(11, Err(Errno::EIO)).is_none());
        let remaining = state.in_flight.remove(&2).unwrap();
        assert_eq!(remaining.cookie, 11);
        assert_eq!(
            state.finish_part(11, Ok(())),
            Some(BlockAsyncCompletion::new(11, Err(Errno::EIO)))
        );
    }
}
