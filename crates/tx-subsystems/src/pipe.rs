//! POSIX-style `pipe2(2)` primitive — anonymous unidirectional byte ring.
//!
//! Spec: `docs/design/05_filesystem/VFS_CHECKS_V2.1.md` (read/write
//! routing), `man 2 pipe2`.
//!
//! **Blocking model — Q2 DECIDED 2026-05-07** (fd-ops Wave 3): matches
//! Linux exactly.
//! - Reader on empty ring: `Blocked` if blocking, `EAGAIN` if
//!   `O_NONBLOCK`. On all-writers-closed empty ring: returns `Done(0)`
//!   (EOF).
//! - Writer on full ring: `Blocked` if blocking, `EAGAIN` if
//!   `O_NONBLOCK`. On all-readers-closed: returns `EPIPE`. The syscall
//!   arm (`tx-shims::sys_write`) is responsible for delivering SIGPIPE
//!   before returning `-EPIPE` to userspace — the pipe module itself
//!   has no `Cap<ProcessIdentity>` and so cannot fan out the signal.
//!
//! The payload allocates *two* `Channel`s registered with the global
//! `wait_source` resolver: one fired when bytes become available
//! (wakes blocked readers) and one fired when space becomes available
//! (wakes blocked writers). This mirrors the TTY identity's
//! single-channel pattern in `tty::structure::TtyIdentity`, generalised
//! to a reader-side and a writer-side carrier.
//!
//! Side bookkeeping. A single `Cap<PipePayload>` backs *two* `OpenFile`
//! shapes — the reader-end and the writer-end — distinguished by the
//! `PipeSide` discriminator on the OpenFile's `RNode` backing
//! (`StructPayload::Pipe { side, .. }`). The shared `Cap<PipePayload>`
//! refcount cannot tell us how many readers vs writers remain, so the
//! payload carries explicit `reader_count` / `writer_count`
//! `AtomicU32`s.
//!
//! **Lifecycle (shell-prompt roadmap Slice 1, 2026-05-07).** The
//! `decr_reader` / `decr_writer` hooks are called from
//! `vfs::structure::OpenFile`'s `Drop` impl, fired exactly once per
//! `Cap<OpenFile>` chain when EBR retires the slot. `Cap` clone
//! shares the same `OpenFile` (the count tracks distinct OpenFiles,
//! not fds), so `dup`/`fork`/`dup3`-replace need no explicit
//! increment. `step_pipe2` sets each count to 1 — one reader
//! OpenFile, one writer OpenFile — and the count strictly
//! decreases from there via Drop. The last-reader-close transition
//! fires the writer-side wait channel for SIGPIPE/EPIPE; the
//! last-writer-close transition fires the reader-side wait channel
//! for EOF.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tx_reactor::wait::{Channel, Mask};
use tx_substrate::step_v3::{InterestMask, WaitSourceId};
use tx_substrate::wake::WaitSource;
use tx_substrate::zone::{self, Cap, ZoneAllocated, ZoneError};
use tx_substrate::SpinMutex;

use crate::execution::{Errno, Guard};
use crate::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking, StructPayload,
    S_IFIFO,
};
use crate::wait_source;

/// Linux's `PIPE_BUF` per `man 7 pipe`. Atomic-write boundary; we
/// reuse the same value as the ring capacity for simplicity (Linux
/// uses 4096 bytes too on most architectures).
pub const PIPE_BUF: usize = 4096;

/// Carrier interest mask: bytes are available to read.
pub const PIPE_READABLE: u64 = 0x1;
/// Carrier interest mask: space is available to write.
pub const PIPE_WRITABLE: u64 = 0x2;

/// Reader vs writer end of a pipe. Threads through the synthetic
/// `RNode` backing each `OpenFile` so `vfs::execution::step_read` /
/// `step_write` can dispatch without an extra OpenFile field. Two
/// distinct RNodes are minted per pipe (one per side); both share
/// the underlying `Cap<PipePayload>` clone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeSide {
    Reader,
    Writer,
}

/// Flags surface for `pipe2(2)`. Wave 3 honours `O_CLOEXEC` and
/// `O_NONBLOCK`; `O_DIRECT` (packet-mode pipes) returns `ENOSYS` from
/// the syscall arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeFlags {
    pub cloexec: bool,
    pub nonblocking: bool,
}

/// Anonymous-pipe payload. Carries the byte ring, per-side reference
/// counts, and the two wait sources.
///
/// **PR-3D-1 coexistence** (D2/D4 ADRs). Each side carries **two**
/// parallel wake-publication points:
///
/// 1. The legacy `Channel` (`reader_wait_channel` /
///    `writer_wait_channel`) — backed by `RawPort`+`Waker`. Consumed
///    by the existing `wait_source` resolver and any caller that
///    `lookup_wait_channel`s the source id and awaits via
///    `WaitFuture`. **Stays in place** until PR-3D-4 retires the
///    legacy resolver path.
/// 2. The new `Arc<WaitSource>` (`reader_wait_source` /
///    `writer_wait_source`) — backed by `TaskMailbox`. Consumed by
///    v3 callers that own a `TaskMailbox` and register via
///    `WaitSource::prepare(...).install_if(...)`. Tested in
///    `crates/tx-subsystems/tests/v3_pipe_waitsource.rs`.
///
/// Both paths fire on every state transition (`step_read`,
/// `step_write`, `decr_reader`, `decr_writer`). The `WaitSourceId`
/// stamped into `step_read` / `step_write`'s `YieldShape::OnWaitSource`
/// is the same `u64` the legacy `wait_source` resolver returned, so
/// the two paths share an id namespace and a v3 caller's
/// `WaitSourceId.raw()` round-trips cleanly to the right side's
/// `WaitSource`.
///
/// Single zone slot per pipe; the reader and writer ends share one
/// `Cap<PipePayload>`.
pub struct PipePayload {
    ring: SpinMutex<RingBuffer>,
    reader_count: AtomicU32,
    writer_count: AtomicU32,
    reader_wait_channel: Channel,
    reader_wait_source_id: u64,
    writer_wait_channel: Channel,
    writer_wait_source_id: u64,
    /// PR-3D-1 new path. Fired alongside `reader_wait_channel` on
    /// every transition that makes the reader side wake-relevant
    /// (bytes-available, writer-closed-EOF).
    reader_wait_source: Arc<WaitSource>,
    /// PR-3D-1 new path. Fired alongside `writer_wait_channel` on
    /// every transition that makes the writer side wake-relevant
    /// (space-available, reader-closed-EPIPE).
    writer_wait_source: Arc<WaitSource>,
}

impl core::fmt::Debug for PipePayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PipePayload")
            .field("reader_count", &self.reader_count.load(Ordering::Acquire))
            .field("writer_count", &self.writer_count.load(Ordering::Acquire))
            .field("reader_wait_source_id", &self.reader_wait_source_id)
            .field("writer_wait_source_id", &self.writer_wait_source_id)
            .finish()
    }
}

#[derive(Debug)]
struct RingBuffer {
    bytes: Vec<u8>,
    head: usize,
    tail: usize,
    /// Number of bytes currently held; redundant with head/tail but
    /// disambiguates `head == tail` between empty and full.
    len: usize,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            bytes: alloc::vec![0u8; PIPE_BUF],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == PIPE_BUF
    }

    /// Drain up to `out.len()` bytes. Returns the number of bytes
    /// copied (≤ `min(self.len, out.len())`).
    fn drain_to_slice(&mut self, out: &mut [u8]) -> usize {
        let n = core::cmp::min(self.len, out.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.bytes[self.head];
            self.head = (self.head + 1) % PIPE_BUF;
        }
        self.len -= n;
        n
    }

    /// Push up to `bytes.len()` bytes. Returns the number copied
    /// (≤ `min(PIPE_BUF - self.len, bytes.len())`).
    fn fill_from_slice(&mut self, bytes: &[u8]) -> usize {
        let space = PIPE_BUF - self.len;
        let n = core::cmp::min(space, bytes.len());
        for &b in bytes.iter().take(n) {
            self.bytes[self.tail] = b;
            self.tail = (self.tail + 1) % PIPE_BUF;
        }
        self.len += n;
        n
    }
}

// === zone wiring ======================================================

static PIPE_PAYLOAD_ZONE: zone::Zone<PipePayload> = zone::Zone::const_new();

unsafe impl ZoneAllocated for PipePayload {
    fn zone() -> &'static zone::Zone<Self> {
        &PIPE_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    zone::register_zone_for::<PipePayload>()?;
    Ok(())
}

// === payload + step impls =============================================

impl PipePayload {
    /// Construct a fresh payload with one reader-end and one
    /// writer-end already accounted for — `step_pipe2` mints exactly
    /// one fd of each side. Fails iff the wait-source registry is
    /// out of memory (currently impossible on the in-tree
    /// `BTreeMap`-backed registry, but the Result surface keeps the
    /// signature aligned with future bounded carrier slabs).
    pub fn new() -> Result<Self, ZoneError> {
        let reader_wait_channel = Channel::new();
        let reader_wait_source_id =
            wait_source::register_wait_channel(reader_wait_channel.clone());
        let writer_wait_channel = Channel::new();
        let writer_wait_source_id =
            wait_source::register_wait_channel(writer_wait_channel.clone());

        // PR-3D-1 (D2/D4 coexistence). Per-side `WaitSource`s share
        // the legacy registry's id namespace so a v3 caller using the
        // `WaitSourceId` stamped into `YieldShape::OnWaitSource` lands
        // on the right side here.
        let reader_wait_source =
            Arc::new(WaitSource::new(WaitSourceId::new(reader_wait_source_id)));
        let writer_wait_source =
            Arc::new(WaitSource::new(WaitSourceId::new(writer_wait_source_id)));

        Ok(Self {
            ring: SpinMutex::new(RingBuffer::new()),
            reader_count: AtomicU32::new(1),
            writer_count: AtomicU32::new(1),
            reader_wait_channel,
            reader_wait_source_id,
            writer_wait_channel,
            writer_wait_source_id,
            reader_wait_source,
            writer_wait_source,
        })
    }

    /// Carrier id paired with the reader-side `Channel`. `step_read`
    /// embeds this in any `Blocked(WaitToken)` it returns; the
    /// channel fires when bytes land in the ring (on
    /// `step_write`-side push).
    pub fn reader_source_id(&self) -> u64 {
        self.reader_wait_source_id
    }

    /// Carrier id paired with the writer-side `Channel`. `step_write`
    /// embeds this in any `Blocked(WaitToken)` it returns; the
    /// channel fires when space frees up in the ring (on
    /// `step_read`-side drain) and on last-reader-close to surface
    /// SIGPIPE/EPIPE to blocked writers.
    pub fn writer_source_id(&self) -> u64 {
        self.writer_wait_source_id
    }

    /// PR-3D-1: reader-side `WaitSource` for the new mailbox-based
    /// wake path. Returned as `&Arc<WaitSource>` so callers can clone
    /// and hold the source across the wait window — pipe's
    /// `Drop` releases its own clone independently of any consumer's.
    ///
    /// `WaitSource::id()` matches [`Self::reader_source_id`].
    pub fn reader_wait_source(&self) -> &Arc<WaitSource> {
        &self.reader_wait_source
    }

    /// PR-3D-1: writer-side `WaitSource`. See [`Self::reader_wait_source`].
    /// `WaitSource::id()` matches [`Self::writer_source_id`].
    pub fn writer_wait_source(&self) -> &Arc<WaitSource> {
        &self.writer_wait_source
    }

    /// Reader-end ref drop. Called from `Drop for OpenFile` when the
    /// last `Cap<OpenFile>` referencing the reader side is released
    /// and EBR retires the slot. On the last-reader-close transition,
    /// fires the writer-side wait channel so any blocked writer
    /// observes the closed-reader state on its next iteration and
    /// surfaces `EPIPE`.
    ///
    /// `pub(crate)` because the only legitimate caller is
    /// `vfs::structure::OpenFile`'s Drop impl. `Cap` clone semantics
    /// (one OpenFile shared by `dup` / `fork`-cloned fds) mean
    /// `incr_reader` is never needed — the count tracks distinct
    /// OpenFiles, not fds, and Cap clone doesn't create a new
    /// OpenFile.
    pub(crate) fn decr_reader(&self) {
        let prev = self.reader_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Legacy path (D2 coexistence): wake any `Waker`-based waiter.
            self.writer_wait_channel
                .fire(Mask::from_bits(PIPE_WRITABLE));
            // PR-3D-1 new path: post `MailboxEvent::SourceFired` to
            // any v3 caller that registered against the writer
            // source. Blocked writers will re-observe and surface
            // EPIPE on the next step (reader_count == 0).
            self.writer_wait_source
                .notify(InterestMask::new(PIPE_WRITABLE));
        }
    }

    /// Writer-end ref drop. Companion of `decr_reader`. On the
    /// last-writer-close transition, fires the reader-side wait
    /// channel so any blocked reader observes the closed-writer
    /// state on its next iteration and surfaces `Done(0)` (EOF).
    pub(crate) fn decr_writer(&self) {
        let prev = self.writer_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Legacy path (D2 coexistence).
            self.reader_wait_channel
                .fire(Mask::from_bits(PIPE_READABLE));
            // PR-3D-1 new path. Blocked readers will re-observe and
            // surface EOF (Done(0)) on the next step.
            self.reader_wait_source
                .notify(InterestMask::new(PIPE_READABLE));
        }
    }

    /// Snapshot of the reader count. Test-only use today.
    #[cfg(test)]
    fn reader_count_snapshot(&self) -> u32 {
        self.reader_count.load(Ordering::Acquire)
    }

    /// Snapshot of the writer count. Test-only use today.
    #[cfg(test)]
    fn writer_count_snapshot(&self) -> u32 {
        self.writer_count.load(Ordering::Acquire)
    }
}

impl Drop for PipePayload {
    fn drop(&mut self) {
        wait_source::release_wait_channel(self.reader_wait_source_id);
        wait_source::release_wait_channel(self.writer_wait_source_id);
    }
}

// === step_pipe2 =======================================================

/// Synthetic `FsObjectId` namespace for anonymous pipes. Linux's
/// kernel allocates an inode out of `pipefs`, an internal mount that
/// is never visible to userspace. Per-side ids are minted from a
/// monotonically-increasing counter rooted at this base so that no
/// real on-disk inode space collides with pipe ids.
const PIPE_FS_OBJECT_ID_BASE: u64 = 0xFFFF_0000_0000_0000;
static NEXT_PIPE_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(PIPE_FS_OBJECT_ID_BASE);

fn allocate_pipe_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_PIPE_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}

/// Build a (reader, writer) `OpenFile` pair sharing a single
/// `Cap<PipePayload>`. Each side is backed by its own synthetic
/// `RNode` whose `RNodeBacking::StructBacked` carries
/// `StructPayload::Pipe { payload, side }`. The walker's
/// containing-mount slot stays `None` — pipes don't belong to a
/// publicly-visible mount.
///
/// On success returns `(reader_cap, writer_cap)`; the caller installs
/// each cap at the lowest unused fd via `process.allocate_fd()` /
/// `process.install_fd(fd, cap)` and applies `O_CLOEXEC` per the
/// flags.
pub fn step_pipe2(flags: PipeFlags) -> Result<(Cap<OpenFile>, Cap<OpenFile>), Errno> {
    // 1. Mint the shared payload + cap.
    let payload_value = PipePayload::new().map_err(|_| Errno::ENOMEM)?;
    let payload_reservation = zone::reserve_for::<PipePayload>().map_err(|_| Errno::ENOMEM)?;
    let payload_cap: Cap<PipePayload> = zone::sign_for(payload_reservation, payload_value);

    // 2. Build per-side RNodes. Each carries
    //    `StructPayload::Pipe { payload, side }` so dispatch in
    //    `vfs::execution::step_read/step_write` matches on the
    //    StructPayload variant.
    //
    //    Mode is `S_IFIFO | 0o600` per Linux's `pipe(7)` semantic
    //    (inode kind = FIFO, owner-only rw).
    let pipe_mode: u16 = S_IFIFO | 0o600;

    let reader_meta = InodeMeta::new(InodeKind::Fifo, pipe_mode);
    let writer_meta = InodeMeta::new(InodeKind::Fifo, pipe_mode);

    let reader_rnode = RNode::new_cap(
        allocate_pipe_fs_object_id(),
        reader_meta,
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe {
                payload: payload_cap.clone(),
                side: PipeSide::Reader,
            },
        },
    )
    .map_err(|_| Errno::ENOMEM)?;

    let writer_rnode = RNode::new_cap(
        allocate_pipe_fs_object_id(),
        writer_meta,
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe {
                payload: payload_cap,
                side: PipeSide::Writer,
            },
        },
    )
    .map_err(|_| Errno::ENOMEM)?;

    // 3. Build per-side OpenFiles. Reader is read-only, writer is
    //    write-only — the wrong-side dispatch in
    //    `vfs::execution::step_read/step_write` is also guarded by
    //    `OpenFileFlags.read` / `.write`.
    let reader_flags = OpenFileFlags {
        read: true,
        write: false,
        append: false,
        cloexec: flags.cloexec,
        nonblocking: flags.nonblocking,
    };
    let writer_flags = OpenFileFlags {
        read: false,
        write: true,
        append: false,
        cloexec: flags.cloexec,
        nonblocking: flags.nonblocking,
    };

    let reader_open = OpenFile::new_cap(reader_rnode, reader_flags).map_err(|_| Errno::ENOMEM)?;
    let writer_open = OpenFile::new_cap(writer_rnode, writer_flags).map_err(|_| Errno::ENOMEM)?;

    Ok((reader_open, writer_open))
}

// === step_read / step_write ==============================================

/// `read(pipe_fd, buf, len)`.
///
/// - empty `out` → `Done(0)`
/// - non-empty ring → `Done(copied)` (single-step: copies what fits)
/// - empty ring + writers closed → `Done(0)` (EOF)
/// - empty ring + writers alive + nonblocking → `Err(EAGAIN)`
/// - empty ring + writers alive + blocking → `Yield` on reader carrier
pub fn step_read(
    payload: &Cap<PipePayload>,
    out: &mut [u8],
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    if out.is_empty() {
        return tx_substrate::step_v3::StepOutcome::done(0);
    }
    let mut ring = payload.ring.lock();
    if !ring.is_empty() {
        let copied = ring.drain_to_slice(out);
        drop(ring);
        // Wake any writer parked on space-available — both paths
        // (D2 coexistence): legacy `Channel` waker AND the new
        // `WaitSource` mailbox path.
        payload
            .writer_wait_channel
            .fire(Mask::from_bits(PIPE_WRITABLE));
        payload
            .writer_wait_source
            .notify(InterestMask::new(PIPE_WRITABLE));
        return tx_substrate::step_v3::StepOutcome::done(copied);
    }
    drop(ring);
    // Empty ring. EOF if all writers gone, otherwise block / EAGAIN.
    if payload.writer_count.load(Ordering::Acquire) == 0 {
        return tx_substrate::step_v3::StepOutcome::done(0);
    }
    if nonblocking {
        return tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN);
    }
    tx_substrate::step_v3::StepOutcome::yield_on_wait_source(
        tx_substrate::step_v3::ByteProgress::EMPTY,
        payload.reader_wait_source_id,
        PIPE_READABLE,
    )
}

/// `write(pipe_fd, buf, len)`.
///
/// - empty `bytes` → `Done(0)`
/// - all readers closed → `Err(EPIPE)` (SIGPIPE delivery is the caller's
///   responsibility — the pipe module has no process Cap)
/// - non-full ring → `Done(copied)` (single-step per wave-6 finding)
/// - full + nonblocking → `Err(EAGAIN)`
/// - full + blocking → `Yield` on writer carrier
pub fn step_write(
    payload: &Cap<PipePayload>,
    bytes: &[u8],
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    if bytes.is_empty() {
        return tx_substrate::step_v3::StepOutcome::done(0);
    }
    if payload.reader_count.load(Ordering::Acquire) == 0 {
        return tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EPIPE);
    }
    let mut ring = payload.ring.lock();
    if !ring.is_full() {
        let copied = ring.fill_from_slice(bytes);
        drop(ring);
        // Wake any reader parked on bytes-available — both paths
        // (D2 coexistence).
        payload
            .reader_wait_channel
            .fire(Mask::from_bits(PIPE_READABLE));
        payload
            .reader_wait_source
            .notify(InterestMask::new(PIPE_READABLE));
        return tx_substrate::step_v3::StepOutcome::done(copied);
    }
    drop(ring);
    if nonblocking {
        return tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN);
    }
    tx_substrate::step_v3::StepOutcome::yield_on_wait_source(
        tx_substrate::step_v3::ByteProgress::EMPTY,
        payload.writer_wait_source_id,
        PIPE_WRITABLE,
    )
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 2)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs (Cap by value, slice + guard by reference under
// a single lifetime `'a`) and delegates from `step()` to the corresponding
// free fn above. The free fns remain the source of truth; callers can migrate
// to the `*Op` types incrementally.
//
// `step_pipe2` returns `Result<_, Errno>` rather than `StepOutcome`, so its
// wrap lifts the result via `StepOutcome::Done` / `StepOutcome::Err`, mirroring
// the cred mutators.

/// `StepOp` wrap of [`step_pipe2`]. No guard arg, so no lifetime needed.
pub struct Pipe2Op {
    pub flags: PipeFlags,
}

impl<I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for Pipe2Op
{
    type Output = (Cap<OpenFile>, Cap<OpenFile>);
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        match step_pipe2(self.flags) {
            Ok(pair) => tx_substrate::step_v3::StepOutcome::Done(pair),
            Err(e) => tx_substrate::step_v3::StepOutcome::Err(e.into()),
        }
    }
}

/// `StepOp` wrap of [`step_read`].
pub struct ReadOp<'a> {
    pub payload: &'a Cap<PipePayload>,
    pub out: &'a mut [u8],
    pub guard: &'a Guard<'a>,
    pub nonblocking: bool,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ReadOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_read(self.payload, self.out, self.guard, self.nonblocking)
    }
}

/// `StepOp` wrap of [`step_write`].
pub struct WriteOp<'a> {
    pub payload: &'a Cap<PipePayload>,
    pub bytes: &'a [u8],
    pub guard: &'a Guard<'a>,
    pub nonblocking: bool,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for WriteOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_write(self.payload, self.bytes, self.guard, self.nonblocking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Out, YieldShape};
    use tx_substrate::testing::init_host_for_test_once;

    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::structure::{RNodeBacking, StructPayload};
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        drain_to_quiescence();
        guard
    }

    /// Loop `drain_with_budget(usize::MAX)` until two consecutive
    /// drains reclaim zero nodes. Each individual drain advances the
    /// global epoch at most once and only reclaims nodes whose
    /// retirement epoch is at least 2 behind the current epoch, so a
    /// single call is insufficient to flush a freshly-dropped
    /// `Cap<OpenFile>` chain (the OpenFile drop queues its inner
    /// `Cap<RNode>` / `Cap<PipePayload>` retirements, which need
    /// further advances). This helper hides that bookkeeping behind
    /// a single call so the lifecycle assertions read cleanly.
    fn drain_to_quiescence() {
        let mut quiet = 0u32;
        while quiet < 2 {
            let stats = tx_substrate::epoch::drain_with_budget(usize::MAX);
            if stats.reclaimed == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
    }

    /// Pull the shared `Cap<PipePayload>` back out of an OpenFile so
    /// tests can poke at counters / call step_read/step_write directly.
    fn payload_of(openfile: &Cap<OpenFile>) -> Cap<PipePayload> {
        match openfile.rnode().backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, .. },
            } => payload.clone(),
            other => panic!("expected StructPayload::Pipe, got {other:?}"),
        }
    }

    fn side_of(openfile: &Cap<OpenFile>) -> PipeSide {
        match openfile.rnode().backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { side, .. },
            } => *side,
            other => panic!("expected StructPayload::Pipe, got {other:?}"),
        }
    }

    #[test]
    fn pipe_step_pipe2_returns_distinct_reader_and_writer_caps() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        assert_eq!(side_of(&reader), PipeSide::Reader);
        assert_eq!(side_of(&writer), PipeSide::Writer);
        assert!(reader.flags().read);
        assert!(!reader.flags().write);
        assert!(writer.flags().write);
        assert!(!writer.flags().read);
        // Both OpenFiles share the same underlying payload identity.
        let payload_a = payload_of(&reader);
        let payload_b = payload_of(&writer);
        assert_eq!(payload_a.reader_count_snapshot(), 1);
        assert_eq!(payload_a.writer_count_snapshot(), 1);
        assert_eq!(payload_a.reader_source_id(), payload_b.reader_source_id());
        assert_eq!(payload_a.writer_source_id(), payload_b.writer_source_id());
    }

    #[test]
    fn pipe_step_read_on_empty_returns_blocked_when_writer_alive() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            V3Out::Yield {
                progress: _,
                shape: YieldShape::OnWaitSource { source: carrier, interests },
            } => {
                assert_eq!(carrier.raw(), payload.reader_source_id());
                assert_eq!(interests.raw(), PIPE_READABLE);
            }
            other => panic!("expected Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn pipe_step_read_on_empty_returns_done_zero_when_writer_closed() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Simulate the last writer closing through the production path:
        // dropping the writer's `Cap<OpenFile>` releases the last retain,
        // EBR retires the slot, and `Drop for OpenFile` calls
        // `decr_writer` exactly once. `drain_with_budget` forces the
        // reclamation callback to run synchronously inside the test.
        drop(writer);
        drain_to_quiescence();
        assert_eq!(payload.writer_count_snapshot(), 0);
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Done(0));
    }

    #[test]
    fn pipe_step_write_then_read_roundtrip_returns_bytes() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // payload pulled via reader; same identity as writer's payload.
        let _ = writer;
        let guard = tx_substrate::epoch::guard();
        let write_outcome = step_write(&payload, b"hello", &guard, false);
        assert_eq!(write_outcome, V3Out::Done(5));
        let mut buf = [0u8; 8];
        let read_outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        assert_eq!(read_outcome, V3Out::Done(5));
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn pipe_step_write_to_full_ring_returns_blocked_when_reader_alive() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Fill the ring exactly to PIPE_BUF.
        let big = alloc::vec![b'x'; PIPE_BUF];
        let guard = tx_substrate::epoch::guard();
        let filled = step_write(&payload, &big, &guard, false);
        assert_eq!(filled, V3Out::Done(PIPE_BUF));
        // Next write blocks.
        let outcome = step_write(&payload, b"y", &guard, false);
        drop(guard);
        match outcome {
            V3Out::Yield {
                progress: _,
                shape: YieldShape::OnWaitSource { source: carrier, interests },
            } => {
                assert_eq!(carrier.raw(), payload.writer_source_id());
                assert_eq!(interests.raw(), PIPE_WRITABLE);
            }
            other => panic!("expected Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn pipe_step_write_to_full_ring_returns_eagain_when_nonblocking() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let big = alloc::vec![b'x'; PIPE_BUF];
        let guard = tx_substrate::epoch::guard();
        let _ = step_write(&payload, &big, &guard, false);
        let outcome = step_write(&payload, b"y", &guard, true);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EAGAIN));
    }

    #[test]
    fn pipe_step_write_with_no_readers_returns_epipe() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Production-path simulate of last-reader-close: drop the
        // reader's `Cap<OpenFile>`. `Drop for OpenFile` fires from
        // EBR reclamation and calls `decr_reader` exactly once.
        drop(reader);
        drain_to_quiescence();
        assert_eq!(payload.reader_count_snapshot(), 0);
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EPIPE));
        // Hold writer to PIPE_BUF lifetime so its drop happens after
        // the assertion (and naturally drives writer_count to 0 too).
        drop(writer);
        drain_to_quiescence();
    }

    #[test]
    fn pipe_step_pipe2_honors_cloexec_flag() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags {
            cloexec: true,
            nonblocking: false,
        })
        .expect("step_pipe2");
        assert!(reader.flags().cloexec);
        assert!(writer.flags().cloexec);
        assert!(!reader.flags().nonblocking);
        assert!(!writer.flags().nonblocking);
    }

    #[test]
    fn pipe_step_pipe2_honors_nonblocking_flag() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags {
            cloexec: false,
            nonblocking: true,
        })
        .expect("step_pipe2");
        assert!(reader.flags().nonblocking);
        assert!(writer.flags().nonblocking);
        assert!(!reader.flags().cloexec);
        assert!(!writer.flags().cloexec);
    }

    #[test]
    fn pipe_step_read_on_empty_returns_eagain_when_nonblocking() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, true);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EAGAIN));
    }

    // === shell-prompt roadmap Slice 1 — Drop-driven lifecycle ===========
    //
    // The four tests below exercise the production close path: dropping
    // the last `Cap<OpenFile>` for a side fires `Drop for OpenFile`
    // through EBR reclamation, which in turn calls `decr_reader` /
    // `decr_writer` exactly once. Each `drain_with_budget(usize::MAX)`
    // forces the reclamation callback to run synchronously inside the
    // test instead of being deferred to whatever future drain happens
    // to flush the queue.

    #[test]
    fn pipe_drop_last_writer_cap_flips_writer_count_via_open_file_drop() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(payload.writer_count_snapshot(), 1);
        drop(writer);
        drain_to_quiescence();
        assert_eq!(payload.writer_count_snapshot(), 0);
        // Reader still alive; reader_count untouched.
        assert_eq!(payload.reader_count_snapshot(), 1);
    }

    #[test]
    fn pipe_drop_last_reader_cap_flips_reader_count_via_open_file_drop() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(payload.reader_count_snapshot(), 1);
        drop(reader);
        drain_to_quiescence();
        assert_eq!(payload.reader_count_snapshot(), 0);
        assert_eq!(payload.writer_count_snapshot(), 1);
        drop(writer);
        drain_to_quiescence();
    }

    #[test]
    fn pipe_drop_last_reader_cap_makes_pending_writer_surface_epipe() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the reader side via the production path.
        drop(reader);
        drain_to_quiescence();
        // A subsequent writer-side step must surface EPIPE; the
        // syscall arm in tx-shims pairs this with SIGPIPE delivery
        // before returning -EPIPE to userspace.
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EPIPE));
        drop(writer);
        drain_to_quiescence();
    }

    #[test]
    fn pipe_drop_last_writer_cap_makes_pending_reader_surface_eof() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the writer side via the production path.
        drop(writer);
        drain_to_quiescence();
        // Reader on empty + writers closed must observe EOF (Done(0))
        // rather than parking forever.
        let mut buf = [0u8; 8];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Done(0));
        drop(reader);
        drain_to_quiescence();
    }

    // === step_v3 sibling-fn tests ========================================
    //
    // The tests below exercise the step_v3-shape sibling fns
    // `step_pipe2` / `step_read`. They pin the step_v3 outcome
    // catalog without crossing the tx-shims dispatch boundary.

    use tx_substrate::step_v3::StepProgress;

    #[test]
    fn step_pipe2_returns_done_with_reader_writer_pair() {
        let _setup = setup();
        let outcome = step_pipe2(PipeFlags::default());
        match outcome {
            Ok((reader, writer)) => {
                assert_eq!(side_of(&reader), PipeSide::Reader);
                assert_eq!(side_of(&writer), PipeSide::Writer);
                assert!(reader.flags().read);
                assert!(writer.flags().write);
            }
            Err(e) => panic!("expected Ok((reader, writer)), got Err({e:?})"),
        }
    }

    #[test]
    fn step_pipe2_honors_cloexec_and_nonblocking() {
        let _setup = setup();
        let outcome = step_pipe2(PipeFlags {
            cloexec: true,
            nonblocking: true,
        });
        match outcome {
            Ok((reader, writer)) => {
                assert!(reader.flags().cloexec);
                assert!(reader.flags().nonblocking);
                assert!(writer.flags().cloexec);
                assert!(writer.flags().nonblocking);
            }
            Err(e) => panic!("expected Ok(_), got Err({e:?})"),
        }
    }

    #[test]
    fn step_read_drains_ring_returns_done_byte_count() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive so step_read sees writer_count > 0
        let guard = tx_substrate::epoch::guard();
        // Seed with bytes via the (non-step_v3) write path.
        let _ = step_write(&payload, b"hello", &guard, false);
        let mut buf = [0u8; 8];
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(n) => {
                assert_eq!(n, 5);
                assert_eq!(&buf[..5], b"hello");
            }
            other => panic!("expected v3 Done(5), got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_buf_returns_done_zero() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = tx_substrate::epoch::guard();
        let mut empty: [u8; 0] = [];
        let outcome = step_read(&payload, &mut empty, &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for empty buf, got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_ring_nonblocking_returns_eagain() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, true);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EAGAIN) => {}
            other => panic!("expected v3 Err(EAGAIN), got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_ring_blocking_yields_on_wait_source_with_empty_progress() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Yield {
                progress,
                shape: tx_substrate::step_v3::YieldShape::OnWaitSource { source: carrier, interests },
            } => {
                assert!(
                    progress.is_empty(),
                    "blocked-empty read must carry empty ByteProgress",
                );
                assert_eq!(carrier.raw(), payload.reader_source_id());
                assert_eq!(interests.raw(), PIPE_READABLE);
            }
            other => panic!("expected v3 Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_ring_writer_closed_returns_done_zero_eof() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the writer side via the production path.
        drop(writer);
        drain_to_quiescence();
        let mut buf = [0u8; 4];
        let guard = tx_substrate::epoch::guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for EOF, got {other:?}"),
        }
        drop(reader);
        drain_to_quiescence();
    }

    // === wave-7 v3 cascade probe (W-pipe-step-write) =====================
    //
    // Mirrors the `step_read_*` tests above but for `step_write`.
    // The new variant pipe write encounters that read does not is the
    // `EPIPE` branch when all readers are gone.

    #[test]
    fn step_write_returns_done_for_partial_drain() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive so reader_count > 0 path is irrelevant
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&payload, b"hello", &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(n) => {
                assert_eq!(n, 5);
            }
            other => panic!("expected v3 Done(5), got {other:?}"),
        }
    }

    #[test]
    fn step_write_returns_err_epipe_when_all_readers_gone() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Production-path simulate of last-reader-close.
        drop(reader);
        drain_to_quiescence();
        assert_eq!(payload.reader_count_snapshot(), 0);
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EPIPE) => {}
            other => panic!("expected v3 Err(EPIPE), got {other:?}"),
        }
        drop(writer);
        drain_to_quiescence();
    }

    #[test]
    fn step_write_yields_on_wait_source_when_full_and_blocking() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Fill the ring exactly to PIPE_BUF.
        let big = alloc::vec![b'x'; PIPE_BUF];
        let guard = tx_substrate::epoch::guard();
        let filled = step_write(&payload, &big, &guard, false);
        assert_eq!(filled, V3Out::Done(PIPE_BUF));
        // Next write blocks → Yield::OnWaitSource with empty progress.
        let outcome = step_write(&payload, b"y", &guard, false);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Yield {
                progress,
                shape: tx_substrate::step_v3::YieldShape::OnWaitSource { source: carrier, interests },
            } => {
                assert!(
                    progress.is_empty(),
                    "blocked-full write must carry empty ByteProgress",
                );
                assert_eq!(carrier.raw(), payload.writer_source_id());
                assert_eq!(interests.raw(), PIPE_WRITABLE);
            }
            other => panic!("expected v3 Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn step_write_returns_eagain_when_full_and_nonblocking() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let big = alloc::vec![b'x'; PIPE_BUF];
        let guard = tx_substrate::epoch::guard();
        let _ = step_write(&payload, &big, &guard, false);
        let outcome = step_write(&payload, b"y", &guard, true);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EAGAIN) => {}
            other => panic!("expected v3 Err(EAGAIN), got {other:?}"),
        }
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave 2 StepOp wrap tests. Each test exercises one wrap
    //! against fixtures already used by the file's existing tests,
    //! confirming the wrap delegates to the corresponding free fn and
    //! the outcome shape is preserved. The free-fn tests above remain
    //! the source of truth for the semantic surface; these tests pin
    //! the wrap layer.
    use super::*;
    use tx_substrate::step_v3::{
        ByteProgress, Errno as V3Errno, ScriptCtx, StepOp, StepOutcome, StepProgress, YieldShape,
    };
    use tx_substrate::testing::init_host_for_test_once;

    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::structure::{RNodeBacking, StructPayload};
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        // Quiesce any deferred drops from prior tests so reader/writer
        // counts read cleanly here.
        let mut quiet = 0u32;
        while quiet < 2 {
            let stats = tx_substrate::epoch::drain_with_budget(usize::MAX);
            if stats.reclaimed == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
        guard
    }

    fn drain_to_quiescence() {
        let mut quiet = 0u32;
        while quiet < 2 {
            let stats = tx_substrate::epoch::drain_with_budget(usize::MAX);
            if stats.reclaimed == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
    }

    fn payload_of(openfile: &Cap<OpenFile>) -> Cap<PipePayload> {
        match openfile.rnode().backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, .. },
            } => payload.clone(),
            other => panic!("expected StructPayload::Pipe, got {other:?}"),
        }
    }

    #[test]
    fn pipe2_op_default_flags_returns_done_with_reader_writer_pair() {
        let _setup = setup();
        let mut op = Pipe2Op { flags: PipeFlags::default() };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        match outcome {
            StepOutcome::Done((reader, writer)) => {
                assert!(reader.flags().read);
                assert!(writer.flags().write);
                assert!(!reader.flags().cloexec);
                assert!(!writer.flags().nonblocking);
            }
            other => panic!("expected Done((reader, writer)), got {other:?}"),
        }
    }

    #[test]
    fn pipe2_op_honors_cloexec_and_nonblocking_flags() {
        let _setup = setup();
        let mut op = Pipe2Op {
            flags: PipeFlags { cloexec: true, nonblocking: true },
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        match outcome {
            StepOutcome::Done((reader, writer)) => {
                assert!(reader.flags().cloexec);
                assert!(reader.flags().nonblocking);
                assert!(writer.flags().cloexec);
                assert!(writer.flags().nonblocking);
            }
            other => panic!("expected Done((reader, writer)), got {other:?}"),
        }
    }

    #[test]
    fn read_op_empty_buf_returns_done_zero() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = tx_substrate::epoch::guard();
        let mut empty: [u8; 0] = [];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut empty,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Done(0) => {}
            other => panic!("expected Done(0), got {other:?}"),
        }
    }

    #[test]
    fn read_op_empty_ring_nonblocking_returns_eagain() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = tx_substrate::epoch::guard();
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            guard: &guard,
            nonblocking: true,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected Err(EAGAIN), got {other:?}"),
        }
    }

    #[test]
    fn read_op_drains_ring_returns_done_byte_count() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive so step_read sees writer_count > 0
        let guard = tx_substrate::epoch::guard();
        // Seed via the free fn.
        let _ = step_write(&payload, b"hello", &guard, false);
        let mut buf = [0u8; 8];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Done(n) => {
                assert_eq!(n, 5);
                assert_eq!(&buf[..5], b"hello");
            }
            other => panic!("expected Done(5), got {other:?}"),
        }
    }

    #[test]
    fn read_op_empty_ring_blocking_yields_on_wait_source() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = tx_substrate::epoch::guard();
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Yield {
                progress,
                shape: YieldShape::OnWaitSource { source, interests },
            } => {
                assert!(progress.is_empty(), "blocked-empty read must carry empty ByteProgress");
                assert_eq!(source.raw(), payload.reader_source_id());
                assert_eq!(interests.raw(), PIPE_READABLE);
                let _ = ByteProgress::EMPTY; // exercise the type
            }
            other => panic!("expected Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn write_op_empty_bytes_returns_done_zero() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = tx_substrate::epoch::guard();
        let bytes: &[u8] = &[];
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Done(0) => {}
            other => panic!("expected Done(0), got {other:?}"),
        }
    }

    #[test]
    fn write_op_partial_drain_returns_done_byte_count() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive
        let _ = reader; // hold reader alive
        let guard = tx_substrate::epoch::guard();
        let bytes: &[u8] = b"hello";
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Done(n) => assert_eq!(n, 5),
            other => panic!("expected Done(5), got {other:?}"),
        }
    }

    #[test]
    fn write_op_no_readers_returns_epipe() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        drop(reader);
        drain_to_quiescence();
        assert_eq!(payload.reader_count_snapshot(), 0);
        let guard = tx_substrate::epoch::guard();
        let bytes: &[u8] = b"x";
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            guard: &guard,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new());
        drop(op);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EPIPE) => {}
            other => panic!("expected Err(EPIPE), got {other:?}"),
        }
        drop(writer);
        drain_to_quiescence();
    }
}
