//! RangeLock coordination state.
//!
//! This is the bounded VM_v1 interval-reservation set. It preserves the
//! declared overlap semantics and writer preference while leaving the final
//! persistent/concurrent interval index for a later substrate fit.
//!
//! Each `RangeLock` owns a `tx_reactor::wait::Channel` registered with
//! `wait_source`. On every release the channel fires the
//! `RANGE_LOCK_RELEASE_MASK` bit so async script wrappers can convert a
//! `WouldBlock` outcome into an awaitable wait via `WouldBlock::wait_token`.

use tx_reactor::wait::{Channel, Mask};
use tx_substrate::step_v3::{
    InterestMask, NoProgress, StepOutcome as V3StepOutcome, WaitSourceId, YieldShape,
};
use tx_substrate::SpinMutex;

use crate::execution::WaitToken;
use crate::wait_source;

use super::UserRange;

/// Channel mask bit fired whenever a RangeLock acquirer releases its
/// reservation. Async waiters subscribe to this bit and re-acquire on wake.
pub const RANGE_LOCK_RELEASE_MASK: u64 = 0x1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    ExclusiveWriter,
    Materializer,
}

pub enum AcquireResult<'a> {
    Acquired(RangeGuard<'a>),
    WouldBlock(WouldBlock<'a>),
}

pub enum AcquirePairResult<'a> {
    Acquired(RangeGuardPair<'a>),
    WouldBlock(WouldBlock<'a>),
}

pub struct WouldBlock<'a> {
    lock: &'a RangeLock,
    pending_writer: Option<PendingWriter<'a>>,
}

impl<'a> WouldBlock<'a> {
    pub fn pending_writer(self) -> Option<PendingWriter<'a>> {
        self.pending_writer
    }

    pub fn has_pending_writer(&self) -> bool {
        self.pending_writer.is_some()
    }

    /// `WaitToken` whose source resolves to the underlying `RangeLock`'s
    /// release channel. Async wrappers feed this into
    /// `wait_source::wait_on_token` to await the next release before
    /// retrying their try-acquire.
    pub fn wait_token(&self) -> WaitToken {
        WaitToken::new(self.lock.wait_source_id, RANGE_LOCK_RELEASE_MASK)
    }
}

pub struct RangeGuard<'a> {
    lock: &'a RangeLock,
    id: u64,
    range: UserRange,
}

impl Drop for RangeGuard<'_> {
    fn drop(&mut self) {
        self.lock.release_active(self.id, self.range);
    }
}

pub struct RangeGuardPair<'a> {
    first: RangeGuard<'a>,
    second: RangeGuard<'a>,
}

impl<'a> RangeGuardPair<'a> {
    pub const fn first(&self) -> &RangeGuard<'a> {
        &self.first
    }

    pub const fn second(&self) -> &RangeGuard<'a> {
        &self.second
    }
}

pub struct PendingWriter<'a> {
    lock: &'a RangeLock,
    id: u64,
    range: UserRange,
}

impl<'a> PendingWriter<'a> {
    pub fn try_acquire(self) -> AcquireResult<'a> {
        self.lock.acquire_pending_writer(self)
    }
}

impl Drop for PendingWriter<'_> {
    fn drop(&mut self) {
        self.lock.release_pending_writer(self.id, self.range);
    }
}

/// Simple bounded interval-reservation set for VM_v1 coordination.
///
/// VM_v1_2 leaves the internal RangeLock data structure implementation-defined.
/// This type preserves the declared overlap semantics and writer preference,
/// but it is not the final optimized segment tree/concurrent interval index.
pub struct RangeLock {
    state: SpinMutex<RangeLockState>,
    wait_channel: Channel,
    wait_source_id: u64,
}

impl RangeLock {
    pub fn new() -> Self {
        let wait_channel = Channel::new();
        let wait_source_id = wait_source::register_wait_channel(wait_channel.clone());
        Self {
            state: SpinMutex::new(RangeLockState::new()),
            wait_channel,
            wait_source_id,
        }
    }

    /// Carrier id under which this `RangeLock`'s release channel is
    /// registered with the global wait-source resolver. Exposed for async
    /// wrappers that hand-build their own `WaitToken` values.
    pub fn wait_source_id(&self) -> u64 {
        self.wait_source_id
    }

    /// Canonical step-shaped acquire per VM_v1_2 §3.1. `Done` = the
    /// reservation is held; `Yield { OnWaitSource }` = the caller should
    /// await the carrier+interest pair and retry. The internal
    /// `PendingWriter` slot used for writer-preference is not exposed
    /// here; production scripts (which drop the rich `WouldBlock`
    /// carrier immediately on blocking anyway) compose against this
    /// surface. Tests that probe the rich pending-writer machinery use
    /// `acquire_step_rich`.
    ///
    /// Returns a step_v3 outcome with `NoProgress` (one-shot acquire,
    /// no byte/page accumulator). Only `Done` and `Yield { OnWaitSource }`
    /// are produced; `Continue` / `Yield { OnAgent }` / `Err` cannot
    /// occur by construction.
    pub fn acquire_step(
        &self,
        range: UserRange,
        mode: LockMode,
    ) -> V3StepOutcome<RangeGuard<'_>, NoProgress> {
        match self.acquire_step_rich(range, mode) {
            AcquireResult::Acquired(guard) => V3StepOutcome::Done(guard),
            AcquireResult::WouldBlock(blocked) => {
                let token = blocked.wait_token();
                V3StepOutcome::Yield {
                    progress: NoProgress,
                    shape: YieldShape::OnWaitSource {
                        source: WaitSourceId::new(token.source_id()),
                        interests: InterestMask::new(token.interest()),
                    },
                }
            }
        }
    }

    /// Canonical step-shaped pair-acquire per VM_v1_2 §3.1.
    pub fn acquire_pair_step(
        &self,
        a: (UserRange, LockMode),
        b: (UserRange, LockMode),
    ) -> V3StepOutcome<RangeGuardPair<'_>, NoProgress> {
        match self.acquire_pair_step_rich(a, b) {
            AcquirePairResult::Acquired(pair) => V3StepOutcome::Done(pair),
            AcquirePairResult::WouldBlock(blocked) => {
                let token = blocked.wait_token();
                V3StepOutcome::Yield {
                    progress: NoProgress,
                    shape: YieldShape::OnWaitSource {
                        source: WaitSourceId::new(token.source_id()),
                        interests: InterestMask::new(token.interest()),
                    },
                }
            }
        }
    }

    /// Rich variant exposing the `AcquireResult` carrier so callers can
    /// inspect (and, for `ExclusiveWriter`, retain) the internal
    /// `PendingWriter` slot used to implement writer-preference. Spec-
    /// shaped callers should prefer [`Self::acquire_step`]; this
    /// variant exists for the writer-preference test machinery and
    /// future producers that want to retain a pending-writer slot
    /// across an await.
    pub fn acquire_step_rich(&self, range: UserRange, mode: LockMode) -> AcquireResult<'_> {
        let mut state = self.state.lock();
        match mode {
            LockMode::Materializer => {
                if state.materializer_blocked(range) {
                    AcquireResult::WouldBlock(WouldBlock {
                        lock: self,
                        pending_writer: None,
                    })
                } else {
                    state.acquire_active(self, range, mode)
                }
            }
            LockMode::ExclusiveWriter => {
                if state.writer_blocked(range, None) {
                    state.acquire_pending_writer(self, range)
                } else {
                    state.acquire_active(self, range, mode)
                }
            }
        }
    }

    /// Rich variant of [`Self::acquire_pair_step`].
    pub fn acquire_pair_step_rich(
        &self,
        a: (UserRange, LockMode),
        b: (UserRange, LockMode),
    ) -> AcquirePairResult<'_> {
        let mut state = self.state.lock();
        if state.pair_blocked(a, b) {
            return AcquirePairResult::WouldBlock(WouldBlock {
                lock: self,
                pending_writer: None,
            });
        }

        let first = state.reserve_active_slot(a.0, a.1);
        let second = state.reserve_active_slot(b.0, b.1);
        match (first, second) {
            (Some(first_id), Some(second_id)) => AcquirePairResult::Acquired(RangeGuardPair {
                first: RangeGuard {
                    lock: self,
                    id: first_id,
                    range: a.0,
                },
                second: RangeGuard {
                    lock: self,
                    id: second_id,
                    range: b.0,
                },
            }),
            (Some(first_id), None) => {
                state.release_active(first_id, a.0);
                AcquirePairResult::WouldBlock(WouldBlock {
                    lock: self,
                    pending_writer: None,
                })
            }
            _ => AcquirePairResult::WouldBlock(WouldBlock {
                lock: self,
                pending_writer: None,
            }),
        }
    }

    fn acquire_pending_writer<'a>(&'a self, mut pending: PendingWriter<'a>) -> AcquireResult<'a> {
        let mut state = self.state.lock();
        let range = pending.range;
        if state.writer_blocked(range, Some(pending.id)) {
            return AcquireResult::WouldBlock(WouldBlock {
                lock: self,
                pending_writer: Some(pending),
            });
        }
        state.release_pending_writer(pending.id, range);
        pending.id = 0;
        core::mem::forget(pending);
        state.acquire_active(self, range, LockMode::ExclusiveWriter)
    }

    fn release_active(&self, id: u64, range: UserRange) {
        self.state.lock().release_active(id, range);
        self.wait_channel
            .fire(Mask::from_bits(RANGE_LOCK_RELEASE_MASK));
    }

    fn release_pending_writer(&self, id: u64, range: UserRange) {
        if id != 0 {
            self.state.lock().release_pending_writer(id, range);
            self.wait_channel
                .fire(Mask::from_bits(RANGE_LOCK_RELEASE_MASK));
        }
    }
}

impl Default for RangeLock {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RangeLock {
    fn drop(&mut self) {
        wait_source::release_wait_channel(self.wait_source_id);
    }
}

const MAX_ACTIVE_RANGES: usize = 16;
const MAX_PENDING_WRITERS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reservation {
    id: u64,
    range: UserRange,
    mode: LockMode,
}

struct RangeLockState {
    active: ReservationIntervalTree<MAX_ACTIVE_RANGES>,
    pending_writers: ReservationIntervalTree<MAX_PENDING_WRITERS>,
    next_id: u64,
}

impl RangeLockState {
    const fn new() -> Self {
        Self {
            active: ReservationIntervalTree::new(),
            pending_writers: ReservationIntervalTree::new(),
            next_id: 1,
        }
    }

    fn materializer_blocked(&self, range: UserRange) -> bool {
        self.active.any_overlap_where(range, |reservation| {
            reservation.mode == LockMode::ExclusiveWriter
        }) || self.pending_writers.any_overlap_where(range, |_| true)
    }

    fn writer_blocked(&self, range: UserRange, own_pending_id: Option<u64>) -> bool {
        if self.active.any_overlap_where(range, |_| true) {
            return true;
        }

        match self.pending_writers.min_overlap_id_where(range, |_| true) {
            Some(first_pending_id) => match own_pending_id {
                Some(own_id) => first_pending_id < own_id,
                None => true,
            },
            None => false,
        }
    }

    fn pair_blocked(&self, a: (UserRange, LockMode), b: (UserRange, LockMode)) -> bool {
        self.single_blocked_by_pair(a, b) || self.single_blocked_by_pair(b, a)
    }

    fn single_blocked_by_pair(
        &self,
        current: (UserRange, LockMode),
        other: (UserRange, LockMode),
    ) -> bool {
        match current.1 {
            LockMode::Materializer => {
                self.materializer_blocked(current.0)
                    || (other.1 == LockMode::ExclusiveWriter && current.0.overlaps(other.0))
            }
            LockMode::ExclusiveWriter => {
                self.writer_blocked(current.0, None) || current.0.overlaps(other.0)
            }
        }
    }

    fn acquire_active<'a>(
        &mut self,
        lock: &'a RangeLock,
        range: UserRange,
        mode: LockMode,
    ) -> AcquireResult<'a> {
        match self.reserve_active_slot(range, mode) {
            Some(id) => AcquireResult::Acquired(RangeGuard { lock, id, range }),
            None => AcquireResult::WouldBlock(WouldBlock {
                lock,
                pending_writer: None,
            }),
        }
    }

    fn reserve_active_slot(&mut self, range: UserRange, mode: LockMode) -> Option<u64> {
        let id = self.take_id();
        if self.active.insert(Reservation { id, range, mode }) {
            Some(id)
        } else {
            None
        }
    }

    fn acquire_pending_writer<'a>(
        &mut self,
        lock: &'a RangeLock,
        range: UserRange,
    ) -> AcquireResult<'a> {
        let id = self.take_id();
        if !self.pending_writers.insert(Reservation {
            id,
            range,
            mode: LockMode::ExclusiveWriter,
        }) {
            return AcquireResult::WouldBlock(WouldBlock {
                lock,
                pending_writer: None,
            });
        }
        AcquireResult::WouldBlock(WouldBlock {
            lock,
            pending_writer: Some(PendingWriter { lock, id, range }),
        })
    }

    fn release_active(&mut self, id: u64, range: UserRange) {
        self.active.remove(id, range);
    }

    fn release_pending_writer(&mut self, id: u64, range: UserRange) {
        self.pending_writers.remove(id, range);
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationKeyOrdering {
    Less,
    Equal,
    Greater,
}

#[derive(Clone, Copy)]
struct ReservationNode {
    reservation: Option<Reservation>,
    left: Option<usize>,
    right: Option<usize>,
    height: i8,
    max_end: usize,
}

impl ReservationNode {
    const fn empty() -> Self {
        Self {
            reservation: None,
            left: None,
            right: None,
            height: 0,
            max_end: 0,
        }
    }

    fn new(reservation: Reservation) -> Self {
        Self {
            reservation: Some(reservation),
            left: None,
            right: None,
            height: 1,
            max_end: reservation.range.end().as_usize(),
        }
    }
}

struct ReservationIntervalTree<const N: usize> {
    root: Option<usize>,
    nodes: [ReservationNode; N],
}

impl<const N: usize> ReservationIntervalTree<N> {
    const fn new() -> Self {
        Self {
            root: None,
            nodes: [const { ReservationNode::empty() }; N],
        }
    }

    fn insert(&mut self, reservation: Reservation) -> bool {
        let Some(slot) = self
            .nodes
            .iter()
            .position(|node| node.reservation.is_none())
        else {
            return false;
        };

        self.nodes[slot] = ReservationNode::new(reservation);
        self.root = Some(match self.root {
            Some(root) => self.insert_node(root, slot),
            None => slot,
        });
        true
    }

    fn remove(&mut self, id: u64, range: UserRange) -> Option<Reservation> {
        let mut removed = None;
        self.root = self.remove_node(self.root, id, range, &mut removed);
        removed
    }

    fn any_overlap_where(&self, range: UserRange, predicate: impl Fn(Reservation) -> bool) -> bool {
        self.any_overlap_from(self.root, range, &predicate)
    }

    fn min_overlap_id_where(
        &self,
        range: UserRange,
        predicate: impl Fn(Reservation) -> bool,
    ) -> Option<u64> {
        self.min_overlap_id_from(self.root, range, &predicate)
    }

    fn insert_node(&mut self, root: usize, inserted: usize) -> usize {
        let inserted_reservation = self.nodes[inserted]
            .reservation
            .expect("inserted node is populated");
        let root_reservation = self.nodes[root]
            .reservation
            .expect("root node is populated");

        match compare_reservation_key(
            inserted_reservation.id,
            inserted_reservation.range,
            root_reservation.id,
            root_reservation.range,
        ) {
            ReservationKeyOrdering::Less => {
                self.nodes[root].left = Some(match self.nodes[root].left {
                    Some(left) => self.insert_node(left, inserted),
                    None => inserted,
                });
            }
            ReservationKeyOrdering::Equal | ReservationKeyOrdering::Greater => {
                self.nodes[root].right = Some(match self.nodes[root].right {
                    Some(right) => self.insert_node(right, inserted),
                    None => inserted,
                });
            }
        }

        self.rebalance(root)
    }

    fn remove_node(
        &mut self,
        root: Option<usize>,
        id: u64,
        range: UserRange,
        removed: &mut Option<Reservation>,
    ) -> Option<usize> {
        let root = root?;
        let root_reservation = self.nodes[root]
            .reservation
            .expect("root node is populated");

        match compare_reservation_key(id, range, root_reservation.id, root_reservation.range) {
            ReservationKeyOrdering::Less => {
                self.nodes[root].left = self.remove_node(self.nodes[root].left, id, range, removed);
            }
            ReservationKeyOrdering::Greater => {
                self.nodes[root].right =
                    self.remove_node(self.nodes[root].right, id, range, removed);
            }
            ReservationKeyOrdering::Equal => {
                *removed = Some(root_reservation);
                return self.remove_root(root);
            }
        }

        Some(self.rebalance(root))
    }

    fn remove_root(&mut self, root: usize) -> Option<usize> {
        match (self.nodes[root].left, self.nodes[root].right) {
            (None, None) => {
                self.nodes[root] = ReservationNode::empty();
                None
            }
            (Some(child), None) | (None, Some(child)) => {
                self.nodes[root] = ReservationNode::empty();
                Some(child)
            }
            (Some(_), Some(right)) => {
                let successor = self.min_node(right);
                let successor_reservation = self.nodes[successor]
                    .reservation
                    .expect("successor node is populated");
                let mut discarded = None;
                self.nodes[root].reservation = Some(successor_reservation);
                self.nodes[root].right = self.remove_node(
                    self.nodes[root].right,
                    successor_reservation.id,
                    successor_reservation.range,
                    &mut discarded,
                );
                Some(self.rebalance(root))
            }
        }
    }

    fn min_node(&self, root: usize) -> usize {
        let mut cursor = root;
        while let Some(left) = self.nodes[cursor].left {
            cursor = left;
        }
        cursor
    }

    fn any_overlap_from(
        &self,
        root: Option<usize>,
        range: UserRange,
        predicate: &impl Fn(Reservation) -> bool,
    ) -> bool {
        let Some(root) = root else {
            return false;
        };
        let node = &self.nodes[root];
        let reservation = node.reservation.expect("tree node is populated");

        if self.subtree_may_overlap(node.left, range)
            && self.any_overlap_from(node.left, range, predicate)
        {
            return true;
        }

        if reservation.range.overlaps(range) && predicate(reservation) {
            return true;
        }

        if reservation.range.start().as_usize() < range.end().as_usize() {
            return self.any_overlap_from(node.right, range, predicate);
        }

        false
    }

    fn min_overlap_id_from(
        &self,
        root: Option<usize>,
        range: UserRange,
        predicate: &impl Fn(Reservation) -> bool,
    ) -> Option<u64> {
        let root = root?;
        let node = &self.nodes[root];
        let reservation = node.reservation.expect("tree node is populated");
        let mut best = None;

        if self.subtree_may_overlap(node.left, range) {
            best = min_option_id(best, self.min_overlap_id_from(node.left, range, predicate));
        }

        if reservation.range.overlaps(range) && predicate(reservation) {
            best = min_option_id(best, Some(reservation.id));
        }

        if reservation.range.start().as_usize() < range.end().as_usize() {
            best = min_option_id(best, self.min_overlap_id_from(node.right, range, predicate));
        }

        best
    }

    fn subtree_may_overlap(&self, root: Option<usize>, range: UserRange) -> bool {
        root.is_some_and(|root| self.nodes[root].max_end > range.start().as_usize())
    }

    fn rebalance(&mut self, root: usize) -> usize {
        self.recompute(root);
        let balance = self.balance_factor(root);

        if balance > 1 {
            let left = self.nodes[root]
                .left
                .expect("left-heavy node has left child");
            if self.balance_factor(left) < 0 {
                self.nodes[root].left = Some(self.rotate_left(left));
            }
            return self.rotate_right(root);
        }

        if balance < -1 {
            let right = self.nodes[root]
                .right
                .expect("right-heavy node has right child");
            if self.balance_factor(right) > 0 {
                self.nodes[root].right = Some(self.rotate_right(right));
            }
            return self.rotate_left(root);
        }

        root
    }

    fn rotate_left(&mut self, root: usize) -> usize {
        let new_root = self.nodes[root].right.expect("rotate_left requires right");
        let transferred = self.nodes[new_root].left;
        self.nodes[new_root].left = Some(root);
        self.nodes[root].right = transferred;
        self.recompute(root);
        self.recompute(new_root);
        new_root
    }

    fn rotate_right(&mut self, root: usize) -> usize {
        let new_root = self.nodes[root].left.expect("rotate_right requires left");
        let transferred = self.nodes[new_root].right;
        self.nodes[new_root].right = Some(root);
        self.nodes[root].left = transferred;
        self.recompute(root);
        self.recompute(new_root);
        new_root
    }

    fn recompute(&mut self, index: usize) {
        let reservation = self.nodes[index]
            .reservation
            .expect("tree node is populated");
        let left = self.nodes[index].left;
        let right = self.nodes[index].right;
        self.nodes[index].height = 1 + self.node_height(left).max(self.node_height(right));
        self.nodes[index].max_end = reservation
            .range
            .end()
            .as_usize()
            .max(self.node_max_end(left))
            .max(self.node_max_end(right));
    }

    fn balance_factor(&self, index: usize) -> i8 {
        self.node_height(self.nodes[index].left) - self.node_height(self.nodes[index].right)
    }

    fn node_height(&self, index: Option<usize>) -> i8 {
        index.map_or(0, |index| self.nodes[index].height)
    }

    fn node_max_end(&self, index: Option<usize>) -> usize {
        index.map_or(0, |index| self.nodes[index].max_end)
    }
}

fn compare_reservation_key(
    a_id: u64,
    a_range: UserRange,
    b_id: u64,
    b_range: UserRange,
) -> ReservationKeyOrdering {
    match a_range.start().as_usize().cmp(&b_range.start().as_usize()) {
        core::cmp::Ordering::Less => ReservationKeyOrdering::Less,
        core::cmp::Ordering::Greater => ReservationKeyOrdering::Greater,
        core::cmp::Ordering::Equal => match a_id.cmp(&b_id) {
            core::cmp::Ordering::Less => ReservationKeyOrdering::Less,
            core::cmp::Ordering::Equal => ReservationKeyOrdering::Equal,
            core::cmp::Ordering::Greater => ReservationKeyOrdering::Greater,
        },
    }
}

fn min_option_id(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
