//! Open-file readiness facade.
//!
//! This is the narrow fd-facing language above object-owned wait sources:
//! syscall shims translate Linux poll/epoll bits into [`FdReadyMask`], call
//! [`query_fd_ready`], then subscribe to the returned wait sources and re-query
//! after every wake.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::{BitAnd, BitOr, BitOrAssign};

use crate::eventfd::EVENTFD_MAX;
use crate::net::PollMask;
use crate::pipe::PipeSide;
use crate::vfs::adapter::step_engine::{Cap, Guard, InterestMask, StepOutcome, WaitSourceId};
use crate::vfs::adapter::wait_routing::{WaitEndpoint, WaitSource};
use crate::vfs::structure::{InodeKind, OpenFile, OpenFileBacking, RNodeBacking, StructPayload};

/// Readiness bits understood at the fd facade boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FdReadyMask(u32);

impl FdReadyMask {
    pub const READ: Self = Self(0x0001);
    pub const PRI: Self = Self(0x0002);
    pub const WRITE: Self = Self(0x0004);
    pub const ERR: Self = Self(0x0008);
    pub const HUP: Self = Self(0x0010);
    pub const RDHUP: Self = Self(0x2000);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(
            bits & (Self::READ.0
                | Self::PRI.0
                | Self::WRITE.0
                | Self::ERR.0
                | Self::HUP.0
                | Self::RDHUP.0),
        )
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

impl BitOr for FdReadyMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FdReadyMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for FdReadyMask {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// Wait-source subscription needed when the requested fd state is not ready.
#[derive(Clone)]
pub struct FdWait {
    pub source: WaitSourceId,
    pub interests: InterestMask,
    endpoint: Option<Arc<WaitSource>>,
}

impl FdWait {
    pub fn new(source: u64, interests: u64) -> Self {
        Self {
            source: WaitSourceId::new(source),
            interests: InterestMask::new(interests),
            endpoint: None,
        }
    }

    pub fn from_endpoint(endpoint: &impl WaitEndpoint, interests: u64) -> Self {
        Self {
            source: endpoint.source_id(),
            interests: InterestMask::new(interests),
            endpoint: Some(endpoint.source()),
        }
    }

    pub fn endpoint(&self) -> Option<&Arc<WaitSource>> {
        self.endpoint.as_ref()
    }
}

impl core::fmt::Debug for FdWait {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FdWait")
            .field("source", &self.source)
            .field("interests", &self.interests)
            .field("has_endpoint", &self.endpoint.is_some())
            .finish()
    }
}

impl PartialEq for FdWait {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.interests == other.interests
    }
}

impl Eq for FdWait {}

/// Inputs needed to observe fd readiness without importing Linux ABI details.
pub struct FdReadyQuery<'a> {
    pub file: &'a Cap<OpenFile>,
    pub interest: FdReadyMask,
    pub now_monotonic_ns: Option<u64>,
}

/// Level-readiness plus the object-owned sources that can wake a re-query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FdReadyReport {
    pub ready: FdReadyMask,
    pub waits: Vec<FdWait>,
    pub epoll_watchable: bool,
}

impl FdReadyReport {
    pub fn primary_wait_source(&self) -> WaitSourceId {
        self.waits
            .first()
            .map(|wait| wait.source)
            .unwrap_or_else(|| WaitSourceId::new(0))
    }

    pub fn primary_endpoint(&self) -> Option<Arc<WaitSource>> {
        self.waits.first().and_then(|wait| wait.endpoint().cloned())
    }

    fn watchable() -> Self {
        Self {
            ready: FdReadyMask::empty(),
            waits: Vec::new(),
            epoll_watchable: true,
        }
    }

    fn poll_only() -> Self {
        Self {
            ready: FdReadyMask::empty(),
            waits: Vec::new(),
            epoll_watchable: false,
        }
    }

    fn unsupported() -> Self {
        Self::default()
    }

    fn push_wait(&mut self, source: u64) {
        if source == 0 {
            return;
        }
        if self
            .waits
            .iter()
            .any(|wait| wait.source == WaitSourceId::new(source))
        {
            return;
        }
        self.waits.push(FdWait::new(source, u64::MAX));
    }

    fn push_endpoint(&mut self, endpoint: &impl WaitEndpoint) {
        let source = endpoint.source_id();
        if source.raw() == 0 {
            return;
        }
        if self.waits.iter().any(|wait| wait.source == source) {
            return;
        }
        self.waits.push(FdWait::from_endpoint(endpoint, u64::MAX));
    }
}

/// Query level readiness for an [`OpenFile`] and return the object-owned wait
/// sources that may make the requested state true later.
pub fn query_fd_ready(query: FdReadyQuery<'_>, guard: &Guard<'_>) -> FdReadyReport {
    if let Some(efd) = query.file.eventfd() {
        let mut report = FdReadyReport::watchable();
        if query.interest.intersects(FdReadyMask::READ) {
            if efd.counter() > 0 {
                report.ready |= FdReadyMask::READ;
            }
            report.push_endpoint(efd.reader_endpoint());
        }
        if query.interest.intersects(FdReadyMask::WRITE) {
            if efd.counter() < EVENTFD_MAX {
                report.ready |= FdReadyMask::WRITE;
            }
            report.push_endpoint(efd.writer_endpoint());
        }
        return report;
    }

    if let Some((payload, side)) = query.file.pipe_endpoint() {
        let mut report = FdReadyReport::watchable();
        match side {
            PipeSide::Reader => {
                if query
                    .interest
                    .intersects(FdReadyMask::READ | FdReadyMask::HUP)
                {
                    if payload.readable_level() {
                        report.ready |= query.interest & (FdReadyMask::READ | FdReadyMask::HUP);
                    }
                    report.push_endpoint(payload.reader_endpoint());
                }
            }
            PipeSide::Writer => {
                if query
                    .interest
                    .intersects(FdReadyMask::WRITE | FdReadyMask::ERR)
                {
                    if payload.writable_level() {
                        report.ready |= query.interest & (FdReadyMask::WRITE | FdReadyMask::ERR);
                    }
                    report.push_endpoint(payload.writer_endpoint());
                }
            }
        }
        return report;
    }

    if let Some((rx, tx)) = query.file.socketpair_endpoint() {
        let mut report = FdReadyReport::watchable();
        if query
            .interest
            .intersects(FdReadyMask::READ | FdReadyMask::HUP)
        {
            if rx.readable_level() {
                report.ready |= query.interest & (FdReadyMask::READ | FdReadyMask::HUP);
            }
            report.push_endpoint(rx.reader_endpoint());
        }
        if query
            .interest
            .intersects(FdReadyMask::WRITE | FdReadyMask::ERR)
        {
            if tx.writable_level() {
                report.ready |= query.interest & (FdReadyMask::WRITE | FdReadyMask::ERR);
            }
            report.push_endpoint(tx.writer_endpoint());
        }
        return report;
    }

    if let Some(tfd) = query.file.timerfd() {
        let mut report = FdReadyReport::watchable();
        if query.interest.intersects(FdReadyMask::READ) {
            if tfd.deadline_ns() != 0
                && (tfd.expiration_count() > 0
                    || query
                        .now_monotonic_ns
                        .is_some_and(|now_ns| tfd.remaining_value_ns(now_ns) == 0))
            {
                report.ready |= FdReadyMask::READ;
            }
            report.push_endpoint(tfd.read_endpoint());
        }
        return report;
    }

    if let Some(sfd) = query.file.signalfd() {
        let mut report = FdReadyReport::watchable();
        if query.interest.intersects(FdReadyMask::READ) {
            if sfd.pending_count() > 0 {
                report.ready |= FdReadyMask::READ;
            }
            report.push_endpoint(sfd.read_endpoint());
        }
        return report;
    }

    if let Some(ufd) = query.file.ufd() {
        let mut report = FdReadyReport::watchable();
        if query.interest.intersects(FdReadyMask::READ) {
            if ufd.pending_fault_count() > 0 {
                report.ready |= FdReadyMask::READ;
            }
            report.push_endpoint(ufd.read_endpoint());
        }
        return report;
    }

    if let Some(mq) = query.file.posix_mq() {
        let mut report = FdReadyReport::watchable();
        if let Ok(info) = crate::ipc::posix_mq::execution::step_mq_poll_info(mq) {
            if query.interest.intersects(FdReadyMask::READ) {
                if info.readable {
                    report.ready |= FdReadyMask::READ;
                }
                report.push_endpoint(&info.read_endpoint);
            }
            if query.interest.intersects(FdReadyMask::WRITE) {
                if info.writable {
                    report.ready |= FdReadyMask::WRITE;
                }
                report.push_endpoint(&info.write_endpoint);
            }
        }
        return report;
    }

    if let Some(ep) = query.file.epoll() {
        let mut report = FdReadyReport::watchable();
        if query.interest.intersects(FdReadyMask::READ) {
            if !ep.is_empty() {
                report.ready |= FdReadyMask::READ;
            }
            report.push_endpoint(ep.ready_endpoint());
        }
        return report;
    }

    if let Some(socket) = query.file.socket_identity() {
        let mut report = FdReadyReport::watchable();
        let socket_interest = fd_to_socket_poll_mask(query.interest);
        match crate::net::step_poll_ready(socket, guard) {
            StepOutcome::Done(mask) => {
                report.ready |= socket_to_fd_ready_mask(mask) & query.interest;
            }
            StepOutcome::Err(_) | StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {}
        }
        match crate::net::step_poll_wait_token(socket, socket_interest, guard) {
            StepOutcome::Done(Some(token)) => {
                report.push_wait(token.source_id());
            }
            StepOutcome::Done(None)
            | StepOutcome::Err(_)
            | StepOutcome::Continue { .. }
            | StepOutcome::Yield { .. } => {}
        }
        return report;
    }

    if let OpenFileBacking::Rnode { rnode } = query.file.backing() {
        match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            } => {
                let mut report = FdReadyReport::poll_only();
                if query.interest.intersects(FdReadyMask::READ) {
                    if crate::tty::execution::tty_read_would_complete(tty, guard) {
                        report.ready |= FdReadyMask::READ;
                    }
                    report.push_endpoint(tty.read_endpoint());
                }
                if query.interest.intersects(FdReadyMask::WRITE) {
                    report.ready |= FdReadyMask::WRITE;
                }
                return report;
            }
            RNodeBacking::PageBacked { .. } if rnode.meta().kind() == InodeKind::Regular => {
                let mut report = FdReadyReport::poll_only();
                report.ready |= query.interest & (FdReadyMask::READ | FdReadyMask::WRITE);
                return report;
            }
            _ => {}
        }
    }

    FdReadyReport::unsupported()
}

fn fd_to_socket_poll_mask(mask: FdReadyMask) -> PollMask {
    let mut poll = PollMask::empty();
    if mask.intersects(FdReadyMask::READ) {
        poll |= PollMask::IN;
    }
    if mask.intersects(FdReadyMask::PRI) {
        poll |= PollMask::PRI;
    }
    if mask.intersects(FdReadyMask::WRITE) {
        poll |= PollMask::OUT;
    }
    if mask.intersects(FdReadyMask::ERR) {
        poll |= PollMask::ERR;
    }
    if mask.intersects(FdReadyMask::HUP) {
        poll |= PollMask::HUP;
    }
    if mask.intersects(FdReadyMask::RDHUP) {
        poll |= PollMask::RDHUP;
    }
    poll
}

fn socket_to_fd_ready_mask(mask: PollMask) -> FdReadyMask {
    let mut ready = FdReadyMask::empty();
    if mask.intersects(PollMask::IN) {
        ready |= FdReadyMask::READ;
    }
    if mask.intersects(PollMask::PRI) {
        ready |= FdReadyMask::PRI;
    }
    if mask.intersects(PollMask::OUT) {
        ready |= FdReadyMask::WRITE;
    }
    if mask.intersects(PollMask::ERR) {
        ready |= FdReadyMask::ERR;
    }
    if mask.intersects(PollMask::HUP) {
        ready |= FdReadyMask::HUP;
    }
    if mask.intersects(PollMask::RDHUP) {
        ready |= FdReadyMask::RDHUP;
    }
    ready
}
