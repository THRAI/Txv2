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
//! **Lifecycle (shell-prompt roadmap Slice 1, revised 2026-05-20).**
//! The `incr_*` / `decr_*` hooks are owned by the process fd table,
//! not by `OpenFile::Drop`. Pipe EOF/EPIPE is fd-close-visible state:
//! a shell pipeline reader must observe EOF as soon as the last writer
//! fd is closed, without waiting for EBR to retire the shared
//! `OpenFile` slot. `step_pipe2` seeds one reader fd and one writer fd;
//! fd-table `dup` / `fork` increments, while `close` / `exec` /
//! process-exit drain decrements. The last-reader-close transition
//! fires the writer-side wait channel for SIGPIPE/EPIPE; the
//! last-writer-close transition fires the reader-side wait channel
//! for EOF.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub mod adapter;
pub mod notification;

use adapter::step_engine::{
    self, ByteProgress, Cap, NoProgress, OneShotStepOp, ScriptCtx, SpinMutex, StepOp, StepOutcome,
    SubjectIdentity, Zone, ZoneAllocated, ZoneError,
};
use adapter::wait_routing::{Channel, WaitSource};
pub use notification::{PIPE_READABLE, PIPE_WRITABLE};

use crate::execution::{Errno, Guard};
use crate::page_backed::PageLease;
use crate::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking, StructPayload,
    S_IFIFO,
};
/// Linux's `PIPE_BUF` per `man 7 pipe`. Atomic-write boundary.
pub const PIPE_BUF: usize = 4096;
pub const PIPE_DEF_BUFFERS: usize = 16;
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;

const PIPE_BUF_FLAG_CAN_MERGE: u32 = 0x10;
const PIPE_TMP_PAGES: usize = 2;

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
    ring: SpinMutex<PipeRing>,
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
enum PipeMode {
    ByteStream,
    #[allow(dead_code)]
    Notification,
}

#[derive(Debug)]
struct PipeRing {
    mode: PipeMode,
    bufs: VecDeque<PipeBuf>,
    tmp_pages: Vec<Vec<u8>>,
    max_usage: usize,
    ring_size: usize,
    bytes: usize,
}

#[derive(Debug)]
struct PipeBuf {
    storage: PipeStorage,
    offset: usize,
    len: usize,
    flags: u32,
}

#[derive(Debug)]
enum PipeStorage {
    AnonPage(Vec<u8>),
    PageBackedLease(PageLease),
}

impl PipeRing {
    fn new() -> Self {
        Self {
            mode: PipeMode::ByteStream,
            bufs: VecDeque::with_capacity(PIPE_DEF_BUFFERS),
            tmp_pages: Vec::new(),
            max_usage: PIPE_DEF_BUFFERS,
            ring_size: PIPE_DEF_BUFFERS,
            bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        debug_assert!(matches!(self.mode, PipeMode::ByteStream));
        self.bufs.is_empty()
    }

    fn is_full(&self) -> bool {
        self.bufs.len() >= self.max_usage
    }

    fn pipe_size_bytes(&self) -> usize {
        self.max_usage.saturating_mul(crate::vm::USER_PAGE_SIZE)
    }

    fn can_write_atomic(&self, len: usize) -> bool {
        self.available_write_capacity() >= len
    }

    fn available_write_capacity(&self) -> usize {
        let tail_merge = self.bufs.back().map(PipeBuf::merge_space).unwrap_or(0);
        let free_slots = self.max_usage.saturating_sub(self.bufs.len());
        tail_merge.saturating_add(free_slots.saturating_mul(crate::vm::USER_PAGE_SIZE))
    }

    fn occupied_slots(&self) -> usize {
        self.bufs.len()
    }

    fn set_pipe_size_bytes(&mut self, requested: usize) -> Result<usize, Errno> {
        let rounded = round_pipe_size(requested)?;
        if rounded > PIPE_MAX_SIZE {
            return Err(Errno::EPERM);
        }
        let slots = rounded / crate::vm::USER_PAGE_SIZE;
        if slots < self.occupied_slots() {
            return Err(Errno::EBUSY);
        }
        self.max_usage = slots;
        self.ring_size = slots;
        Ok(self.pipe_size_bytes())
    }

    fn drain_to_slice(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() {
            let Some(front) = self.bufs.front_mut() else {
                break;
            };
            let chunk = core::cmp::min(out.len() - copied, front.len);
            front.copy_to_slice(&mut out[copied..copied + chunk]);
            front.offset += chunk;
            front.len -= chunk;
            copied += chunk;
            self.bytes -= chunk;
            if front.len == 0 {
                let buf = self.bufs.pop_front().expect("front existed");
                self.release_storage(buf.storage);
            }
        }
        copied
    }

    fn fill_from_slice(&mut self, bytes: &[u8]) -> usize {
        let mut copied = 0usize;
        if let Some(tail) = self.bufs.back_mut() {
            let chunk = tail.append_merge(&bytes[copied..]);
            copied += chunk;
            self.bytes += chunk;
        }
        while copied < bytes.len() && !self.is_full() {
            let mut page = self.take_anon_page();
            let chunk = core::cmp::min(crate::vm::USER_PAGE_SIZE, bytes.len() - copied);
            page[..chunk].copy_from_slice(&bytes[copied..copied + chunk]);
            self.bufs.push_back(PipeBuf {
                storage: PipeStorage::AnonPage(page),
                offset: 0,
                len: chunk,
                flags: PIPE_BUF_FLAG_CAN_MERGE,
            });
            copied += chunk;
            self.bytes += chunk;
        }
        copied
    }

    fn copy_to_slice(&self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        for buf in &self.bufs {
            if copied == out.len() {
                break;
            }
            let chunk = core::cmp::min(out.len() - copied, buf.len);
            buf.copy_to_slice(&mut out[copied..copied + chunk]);
            copied += chunk;
        }
        copied
    }

    fn drain_to_ring(&mut self, dst: &mut PipeRing, len: usize) -> usize {
        let mut moved = 0usize;
        let mut scratch = alloc::vec![0u8; core::cmp::min(len, crate::vm::USER_PAGE_SIZE)];
        while moved < len && !self.is_empty() && !dst.is_full() {
            let target = core::cmp::min(scratch.len(), len - moved);
            let read = self.drain_to_slice(&mut scratch[..target]);
            if read == 0 {
                break;
            }
            let wrote = dst.fill_from_slice(&scratch[..read]);
            moved += wrote;
            if wrote < read {
                break;
            }
        }
        moved
    }

    fn tee_to_ring(&self, dst: &mut PipeRing, len: usize) -> usize {
        let mut duplicated = 0usize;
        for buf in &self.bufs {
            if duplicated >= len || dst.is_full() {
                break;
            }
            let chunk = core::cmp::min(buf.len, len - duplicated);
            if let Some(clone) = buf.clone_prefix(chunk) {
                dst.bytes += chunk;
                dst.bufs.push_back(clone);
                duplicated += chunk;
            } else {
                let mut scratch = alloc::vec![0u8; chunk];
                buf.copy_to_slice(&mut scratch);
                duplicated += dst.fill_from_slice(&scratch);
            }
        }
        duplicated
    }

    fn take_anon_page(&mut self) -> Vec<u8> {
        self.tmp_pages
            .pop()
            .unwrap_or_else(|| alloc::vec![0u8; crate::vm::USER_PAGE_SIZE])
    }

    fn release_storage(&mut self, storage: PipeStorage) {
        match storage {
            PipeStorage::AnonPage(mut page) if self.tmp_pages.len() < PIPE_TMP_PAGES => {
                page.fill(0);
                self.tmp_pages.push(page);
            }
            PipeStorage::AnonPage(_) => {}
            PipeStorage::PageBackedLease(_) => {}
        }
    }

    fn push_page_lease(
        &mut self,
        lease: PageLease,
        offset: usize,
        len: usize,
    ) -> Result<(), PageLease> {
        if self.is_full() {
            return Err(lease);
        }
        self.bytes += len;
        self.bufs.push_back(PipeBuf {
            storage: PipeStorage::PageBackedLease(lease),
            offset,
            len,
            flags: 0,
        });
        Ok(())
    }

    fn pop_front_lease(&mut self) -> Option<PipeBuf> {
        let front = self.bufs.front()?;
        if !matches!(front.storage, PipeStorage::PageBackedLease(_)) {
            return None;
        }
        let buf = self.bufs.pop_front()?;
        self.bytes -= buf.len;
        Some(buf)
    }
}

impl PipeBuf {
    fn merge_space(&self) -> usize {
        if self.flags & PIPE_BUF_FLAG_CAN_MERGE == 0 {
            return 0;
        }
        match &self.storage {
            PipeStorage::AnonPage(page) => page.len().saturating_sub(self.offset + self.len),
            PipeStorage::PageBackedLease(_) => 0,
        }
    }

    fn append_merge(&mut self, bytes: &[u8]) -> usize {
        let n = core::cmp::min(self.merge_space(), bytes.len());
        if n == 0 {
            return 0;
        }
        match &mut self.storage {
            PipeStorage::AnonPage(page) => {
                let start = self.offset + self.len;
                page[start..start + n].copy_from_slice(&bytes[..n]);
            }
            PipeStorage::PageBackedLease(_) => {}
        }
        self.len += n;
        n
    }

    fn copy_to_slice(&self, out: &mut [u8]) {
        match &self.storage {
            PipeStorage::AnonPage(page) => {
                out.copy_from_slice(&page[self.offset..self.offset + out.len()]);
            }
            PipeStorage::PageBackedLease(lease) => {
                if let Ok(base) = step_engine::page_allocator::frame_kernel_addr(lease.ppn()) {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            base.add(self.offset),
                            out.as_mut_ptr(),
                            out.len(),
                        );
                    }
                }
            }
        }
    }

    fn clone_prefix(&self, len: usize) -> Option<Self> {
        match &self.storage {
            PipeStorage::AnonPage(_) => None,
            PipeStorage::PageBackedLease(lease) => Some(Self {
                storage: PipeStorage::PageBackedLease(lease.retain().ok()?),
                offset: self.offset,
                len,
                flags: self.flags,
            }),
        }
    }
}

fn round_pipe_size(requested: usize) -> Result<usize, Errno> {
    let page_size = crate::vm::USER_PAGE_SIZE;
    let size = requested.max(page_size);
    let pages = size.checked_add(page_size - 1).ok_or(Errno::EINVAL)? / page_size;
    let slots = pages.checked_next_power_of_two().ok_or(Errno::EINVAL)?;
    slots.checked_mul(page_size).ok_or(Errno::EINVAL)
}

// === zone wiring ======================================================

static PIPE_PAYLOAD_ZONE: Zone<PipePayload> = Zone::const_new();

unsafe impl ZoneAllocated for PipePayload {
    fn zone() -> &'static Zone<Self> {
        &PIPE_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    step_engine::register_zone_for::<PipePayload>()?;
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
        let wait_points = notification::new_wait_points();

        Ok(Self {
            ring: SpinMutex::new(PipeRing::new()),
            reader_count: AtomicU32::new(1),
            writer_count: AtomicU32::new(1),
            reader_wait_channel: wait_points.reader_channel,
            reader_wait_source_id: wait_points.reader_source_id,
            writer_wait_channel: wait_points.writer_channel,
            writer_wait_source_id: wait_points.writer_source_id,
            reader_wait_source: wait_points.reader_source,
            writer_wait_source: wait_points.writer_source,
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

    pub fn pipe_size_bytes(&self) -> usize {
        self.ring.lock().pipe_size_bytes()
    }

    /// Level predicate for read-side polling. A pipe reader is
    /// readable when bytes are buffered, or when all writers are gone
    /// and a read would complete with EOF.
    pub fn readable_level(&self) -> bool {
        let has_bytes = !self.ring.lock().is_empty();
        has_bytes || self.writer_count.load(Ordering::Acquire) == 0
    }

    /// Level predicate for write-side polling. A pipe writer is
    /// writable when a write can make progress, or when all readers
    /// are gone and a write would complete immediately with EPIPE.
    pub fn writable_level(&self) -> bool {
        if self.reader_count.load(Ordering::Acquire) == 0 {
            return true;
        }
        self.ring.lock().available_write_capacity() > 0
    }

    pub fn set_pipe_size_bytes(&self, requested: usize) -> Result<usize, Errno> {
        let mut ring = self.ring.lock();
        let size = ring.set_pipe_size_bytes(requested)?;
        drop(ring);
        notification::notify_writable(&self.writer_wait_channel, &self.writer_wait_source);
        Ok(size)
    }

    /// A new fd now references this reader endpoint.
    pub(crate) fn incr_reader(&self) {
        self.reader_count.fetch_add(1, Ordering::AcqRel);
    }

    /// A new fd now references this writer endpoint.
    pub(crate) fn incr_writer(&self) {
        self.writer_count.fetch_add(1, Ordering::AcqRel);
    }

    /// Reader-end fd close. On the last-reader-close transition, fires
    /// the writer-side wait channel so any blocked writer observes the
    /// closed-reader state on its next iteration and surfaces `EPIPE`.
    ///
    /// `pub(crate)` because the only legitimate caller is
    /// process fd-table accounting.
    pub(crate) fn decr_reader(&self) {
        let Some(prev) = decrement_nonzero(&self.reader_count) else {
            return;
        };
        if prev == 1 {
            notification::notify_writable(&self.writer_wait_channel, &self.writer_wait_source);
        }
    }

    /// Writer-end fd close. Companion of `decr_reader`. On the
    /// last-writer-close transition, fires the reader-side wait channel
    /// so any blocked reader observes the closed-writer state on its
    /// next iteration and surfaces `Done(0)` (EOF).
    pub(crate) fn decr_writer(&self) {
        let Some(prev) = decrement_nonzero(&self.writer_count) else {
            return;
        };
        if prev == 1 {
            notification::notify_readable(&self.reader_wait_channel, &self.reader_wait_source);
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

fn decrement_nonzero(counter: &AtomicU32) -> Option<u32> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return None;
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(prev) => return Some(prev),
            Err(observed) => current = observed,
        }
    }
}

impl Drop for PipePayload {
    fn drop(&mut self) {
        notification::release_wait_points(self.reader_wait_source_id, self.writer_wait_source_id);
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // observe: N/A — pure allocation, no guard needed
    // upgrade: N/A
    // 1. Mint the shared payload + cap.
    // reserve: allocate zone slots for payload, reader, writer
    let payload_value = PipePayload::new().map_err(|_| Errno::ENOMEM)?;
    // commit — sign payload cap and mint reader/writer OpenFile caps
    let payload_cap = step_engine::sign(payload_value).map_err(|_| Errno::ENOMEM)?;

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

    // publish — N/A (pipes do not publish bus signals)
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
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // ① observe — empty out is a no-op
    if out.is_empty() {
        return step_engine::done_bytes(0);
    }
    // ② upgrade — N/A (PayloadCap already held by caller)
    // ③ reserve — acquire ring lock
    let mut ring = payload.ring.lock();
    if !ring.is_empty() {
        let copied = ring.drain_to_slice(out);
        drop(ring);
        // ④ commit — drain bytes into caller buffer, release lock
        // ⑤ publish — wake writers parked on space-available
        notification::notify_writable(&payload.writer_wait_channel, &payload.writer_wait_source);
        return step_engine::done_bytes(copied);
    }
    drop(ring);
    // Empty ring. EOF if all writers gone, otherwise block / EAGAIN.
    if payload.writer_count.load(Ordering::Acquire) == 0 {
        return step_engine::done_bytes(0);
    }
    if nonblocking {
        return step_engine::eagain();
    }
    // Yield: wait for readable — publish (N/A) precedes yield, ok per A-14
    notification::wait_until_readable(payload.reader_wait_source_id)
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
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // ① observe — empty bytes is a no-op
    if bytes.is_empty() {
        return step_engine::done_bytes(0);
    }
    // ① observe — check reader count for EPIPE
    if payload.reader_count.load(Ordering::Acquire) == 0 {
        return step_engine::epipe();
    }
    // ② upgrade — N/A (PayloadCap already held by caller)
    // ③ reserve — acquire ring lock
    let mut ring = payload.ring.lock();
    if bytes.len() <= PIPE_BUF && !ring.can_write_atomic(bytes.len()) {
        drop(ring);
        if nonblocking {
            return step_engine::eagain();
        }
        return notification::wait_until_writable(payload.writer_wait_source_id);
    }
    if !ring.is_full() || ring.available_write_capacity() > 0 {
        let copied = ring.fill_from_slice(bytes);
        // ④ commit — fill bytes into ring, release lock
        // ⑤ publish — wake readers parked on bytes-available
        if copied > 0 {
            drop(ring);
            notification::notify_readable(
                &payload.reader_wait_channel,
                &payload.reader_wait_source,
            );
            return step_engine::done_bytes(copied);
        }
    }
    drop(ring);
    if nonblocking {
        return step_engine::eagain();
    }
    // Yield: wait for writable — publish (N/A) precedes yield
    notification::wait_until_writable(payload.writer_wait_source_id)
}

/// Copy pipe bytes into `out` without consuming them.
pub fn step_peek(
    payload: &Cap<PipePayload>,
    out: &mut [u8],
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    if out.is_empty() {
        return step_engine::done_bytes(0);
    }
    let ring = payload.ring.lock();
    if !ring.is_empty() {
        return step_engine::done_bytes(ring.copy_to_slice(out));
    }
    drop(ring);
    if payload.writer_count.load(Ordering::Acquire) == 0 {
        return step_engine::done_bytes(0);
    }
    if nonblocking {
        return step_engine::eagain();
    }
    notification::wait_until_readable(payload.reader_wait_source_id)
}

pub fn step_push_page_lease(
    payload: &Cap<PipePayload>,
    lease: PageLease,
    offset: usize,
    len: usize,
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    if len == 0 {
        return step_engine::done_bytes(0);
    }
    if payload.reader_count.load(Ordering::Acquire) == 0 {
        return step_engine::epipe();
    }
    let mut ring = payload.ring.lock();
    match ring.push_page_lease(lease, offset, len) {
        Ok(()) => {
            drop(ring);
            notification::notify_readable(
                &payload.reader_wait_channel,
                &payload.reader_wait_source,
            );
            step_engine::done_bytes(len)
        }
        Err(_lease) if nonblocking => step_engine::eagain(),
        Err(_lease) => notification::wait_until_writable(payload.writer_wait_source_id),
    }
}

pub fn step_pop_page_lease(
    payload: &Cap<PipePayload>,
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<Option<(PageLease, usize, usize)>, ByteProgress> {
    let mut ring = payload.ring.lock();
    if let Some(buf) = ring.pop_front_lease() {
        drop(ring);
        notification::notify_writable(&payload.writer_wait_channel, &payload.writer_wait_source);
        return match buf.storage {
            PipeStorage::PageBackedLease(lease) => {
                StepOutcome::Done(Some((lease, buf.offset, buf.len)))
            }
            PipeStorage::AnonPage(_) => StepOutcome::Done(None),
        };
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

/// Move bytes from one pipe ring to another without staging through
/// userspace. This powers the v1 `splice(pipe, pipe)` syscall path.
pub fn step_splice_to_pipe(
    src: &Cap<PipePayload>,
    dst: &Cap<PipePayload>,
    len: usize,
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    if len == 0 {
        return step_engine::done_bytes(0);
    }
    if src == dst {
        return StepOutcome::Err(Errno::EINVAL.into());
    }
    if dst.reader_count.load(Ordering::Acquire) == 0 {
        return step_engine::epipe();
    }

    let copied = with_two_rings(src, dst, |src_ring, dst_ring| {
        if src_ring.is_empty() || dst_ring.is_full() {
            0
        } else {
            src_ring.drain_to_ring(dst_ring, len)
        }
    });
    if copied > 0 {
        notification::notify_writable(&src.writer_wait_channel, &src.writer_wait_source);
        notification::notify_readable(&dst.reader_wait_channel, &dst.reader_wait_source);
        return step_engine::done_bytes(copied);
    }

    if src.writer_count.load(Ordering::Acquire) == 0 {
        return step_engine::done_bytes(0);
    }
    if nonblocking {
        return step_engine::eagain();
    }
    notification::wait_until_readable(src.reader_wait_source_id)
}

/// Duplicate bytes from one pipe to another without consuming the
/// source. This is the semantic core of `tee(2)`.
pub fn step_tee_to_pipe(
    src: &Cap<PipePayload>,
    dst: &Cap<PipePayload>,
    len: usize,
    _guard: &Guard<'_>,
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    if len == 0 {
        return step_engine::done_bytes(0);
    }
    if src == dst {
        return StepOutcome::Err(Errno::EINVAL.into());
    }
    if dst.reader_count.load(Ordering::Acquire) == 0 {
        return step_engine::epipe();
    }

    let copied = with_two_rings(src, dst, |src_ring, dst_ring| {
        src_ring.tee_to_ring(dst_ring, len)
    });
    if copied > 0 {
        notification::notify_readable(&dst.reader_wait_channel, &dst.reader_wait_source);
        return step_engine::done_bytes(copied);
    }

    if src.writer_count.load(Ordering::Acquire) == 0 {
        return step_engine::done_bytes(0);
    }
    if nonblocking {
        return step_engine::eagain();
    }
    notification::wait_until_readable(src.reader_wait_source_id)
}

fn with_two_rings<R>(
    a: &Cap<PipePayload>,
    b: &Cap<PipePayload>,
    f: impl FnOnce(&mut PipeRing, &mut PipeRing) -> R,
) -> R {
    let a_first = a.key().raw() <= b.key().raw();
    if a_first {
        let mut a_ring = a.ring.lock();
        let mut b_ring = b.ring.lock();
        f(&mut a_ring, &mut b_ring)
    } else {
        let mut b_ring = b.ring.lock();
        let mut a_ring = a.ring.lock();
        f(&mut a_ring, &mut b_ring)
    }
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

impl<I: SubjectIdentity> StepOp<I> for Pipe2Op {
    type Output = (Cap<OpenFile>, Cap<OpenFile>);
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match step_pipe2(self.flags) {
            Ok(pair) => StepOutcome::Done(pair),
            Err(e) => StepOutcome::Err(e.into()),
        }
    }
}

impl OneShotStepOp for Pipe2Op {}
impl OneShotStepOp<crate::process::ProcessIdentity> for Pipe2Op {}

/// `StepOp` wrap of [`step_read`].
pub struct ReadOp<'a> {
    pub payload: &'a Cap<PipePayload>,
    pub out: &'a mut [u8],
    pub nonblocking: bool,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read(self.payload, self.out, &__guard, self.nonblocking)
    }
}

/// `StepOp` wrap of [`step_write`].
pub struct WriteOp<'a> {
    pub payload: &'a Cap<PipePayload>,
    pub bytes: &'a [u8],
    pub nonblocking: bool,
}

impl<'a, I: SubjectIdentity> StepOp<I> for WriteOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_write(self.payload, self.bytes, &__guard, self.nonblocking)
    }
}

#[cfg(test)]
mod tests {
    use super::adapter::step_engine::{guard, Errno as V3Errno, StepOutcome as V3Out, YieldShape};
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::structure::{RNodeBacking, StructPayload};
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
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
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            V3Out::Yield {
                progress: _,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
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
        // Simulate the last writer fd closing. Production paths drive
        // this from process fd-table accounting, not OpenFile Drop.
        payload.decr_writer();
        drop(writer);
        assert_eq!(payload.writer_count_snapshot(), 0);
        let mut buf = [0u8; 4];
        let guard = guard();
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
        let guard = guard();
        let write_outcome = step_write(&payload, b"hello", &guard, false);
        assert_eq!(write_outcome, V3Out::Done(5));
        let mut buf = [0u8; 8];
        let read_outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        assert_eq!(read_outcome, V3Out::Done(5));
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn pipe_default_capacity_is_sixteen_pages() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(payload.pipe_size_bytes(), 16 * crate::vm::USER_PAGE_SIZE);
    }

    #[test]
    fn pipe_resize_grows_rounds_and_rejects_busy_shrink() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(
            payload
                .set_pipe_size_bytes(crate::vm::USER_PAGE_SIZE + 1)
                .expect("grow rounds to two pages"),
            2 * crate::vm::USER_PAGE_SIZE,
        );

        let guard = guard();
        let two_pages = alloc::vec![b'x'; 2 * crate::vm::USER_PAGE_SIZE];
        assert_eq!(
            step_write(&payload, &two_pages, &guard, false),
            V3Out::Done(two_pages.len())
        );
        drop(guard);

        assert_eq!(
            payload.set_pipe_size_bytes(crate::vm::USER_PAGE_SIZE),
            Err(Errno::EBUSY),
        );
    }

    #[test]
    fn pipe_atomic_pipe_buf_write_fails_without_partial_publish_when_nonblocking() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        payload
            .set_pipe_size_bytes(crate::vm::USER_PAGE_SIZE)
            .expect("one-page pipe");

        let guard = guard();
        let almost_full = alloc::vec![b'a'; crate::vm::USER_PAGE_SIZE - 8];
        assert_eq!(
            step_write(&payload, &almost_full, &guard, false),
            V3Out::Done(almost_full.len())
        );
        let atomic = alloc::vec![b'b'; 16];
        assert_eq!(
            step_write(&payload, &atomic, &guard, true),
            V3Out::Err(V3Errno::EAGAIN)
        );

        let mut out = alloc::vec![0u8; crate::vm::USER_PAGE_SIZE];
        assert_eq!(
            step_read(&payload, &mut out, &guard, false),
            V3Out::Done(almost_full.len())
        );
        drop(guard);
        assert_eq!(&out[..almost_full.len()], &almost_full[..]);
    }

    #[test]
    fn pipe_step_write_to_full_ring_returns_blocked_when_reader_alive() {
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Fill the pipe exactly to its current byte capacity.
        let big = alloc::vec![b'x'; payload.pipe_size_bytes()];
        let guard = guard();
        let filled = step_write(&payload, &big, &guard, false);
        assert_eq!(filled, V3Out::Done(big.len()));
        // Next write blocks.
        let outcome = step_write(&payload, b"y", &guard, false);
        drop(guard);
        match outcome {
            V3Out::Yield {
                progress: _,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
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
        let big = alloc::vec![b'x'; payload.pipe_size_bytes()];
        let guard = guard();
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
        // Simulate the last reader fd closing through fd-table accounting.
        payload.decr_reader();
        drop(reader);
        assert_eq!(payload.reader_count_snapshot(), 0);
        let guard = guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EPIPE));
        drop(writer);
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
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, true);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EAGAIN));
    }

    // === shell-prompt roadmap Slice 1 — fd-table lifecycle ==============
    //
    // The tests below exercise the observable pipe-side state
    // transition driven by process fd-table close accounting. EOF/EPIPE
    // must be visible as soon as the last fd on a side closes; waiting
    // for EBR to retire an OpenFile would let shell pipelines hang.

    #[test]
    fn pipe_close_last_writer_fd_flips_writer_count() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(payload.writer_count_snapshot(), 1);
        payload.decr_writer();
        drop(writer);
        assert_eq!(payload.writer_count_snapshot(), 0);
        // Reader still alive; reader_count untouched.
        assert_eq!(payload.reader_count_snapshot(), 1);
    }

    #[test]
    fn pipe_close_last_reader_fd_flips_reader_count() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        assert_eq!(payload.reader_count_snapshot(), 1);
        payload.decr_reader();
        drop(reader);
        assert_eq!(payload.reader_count_snapshot(), 0);
        assert_eq!(payload.writer_count_snapshot(), 1);
        drop(writer);
    }

    #[test]
    fn pipe_close_last_reader_fd_makes_pending_writer_surface_epipe() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the reader side via fd-table accounting.
        payload.decr_reader();
        drop(reader);
        // A subsequent writer-side step must surface EPIPE; the
        // syscall arm in tx-shims pairs this with SIGPIPE delivery
        // before returning -EPIPE to userspace.
        let guard = guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Err(V3Errno::EPIPE));
        drop(writer);
    }

    #[test]
    fn pipe_close_last_writer_fd_makes_pending_reader_surface_eof() {
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the writer side via fd-table accounting.
        payload.decr_writer();
        drop(writer);
        // Reader on empty + writers closed must observe EOF (Done(0))
        // rather than parking forever.
        let mut buf = [0u8; 8];
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        assert_eq!(outcome, V3Out::Done(0));
        drop(reader);
    }

    // === step_v3 sibling-fn tests ========================================
    //
    // The tests below exercise the step_v3-shape sibling fns
    // `step_pipe2` / `step_read`. They pin the step_v3 outcome
    // catalog without crossing the tx-shims dispatch boundary.

    use super::adapter::step_engine::StepProgress;

    #[test]
    fn step_pipe2_returns_done_with_reader_writer_pair() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let outcome = step_pipe2(PipeFlags::default());
        match outcome {
            // publish: N/A — pipe creation doesn't publish signals
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
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let outcome = step_pipe2(PipeFlags {
            cloexec: true,
            nonblocking: true,
        });
        match outcome {
            // publish: N/A — pipe creation doesn't publish signals
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
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive so step_read sees writer_count > 0
        let guard = guard();
        // Seed with bytes via the (non-step_v3) write path.
        let _ = step_write(&payload, b"hello", &guard, false);
        let mut buf = [0u8; 8];
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Done(n) => {
                assert_eq!(n, 5);
                assert_eq!(&buf[..5], b"hello");
            }
            other => panic!("expected v3 Done(5), got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_buf_returns_done_zero() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let guard = guard();
        let mut empty: [u8; 0] = [];
        let outcome = step_read(&payload, &mut empty, &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for empty buf, got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_ring_nonblocking_returns_eagain() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, true);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected v3 Err(EAGAIN), got {other:?}"),
        }
    }

    #[test]
    fn step_read_empty_ring_blocking_yields_on_wait_source_with_empty_progress() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let mut buf = [0u8; 4];
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Yield {
                progress,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
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
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Close the writer side via fd-table accounting.
        payload.decr_writer();
        drop(writer);
        let mut buf = [0u8; 4];
        let guard = guard();
        let outcome = step_read(&payload, &mut buf, &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for EOF, got {other:?}"),
        }
        drop(reader);
    }

    // === wave-7 v3 cascade probe (W-pipe-step-write) =====================
    //
    // Mirrors the `step_read_*` tests above but for `step_write`.
    // The new variant pipe write encounters that read does not is the
    // `EPIPE` branch when all readers are gone.

    #[test]
    fn step_write_returns_done_for_partial_drain() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let _ = writer; // hold writer alive so reader_count > 0 path is irrelevant
        let guard = guard();
        let outcome = step_write(&payload, b"hello", &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Done(n) => {
                assert_eq!(n, 5);
            }
            other => panic!("expected v3 Done(5), got {other:?}"),
        }
    }

    #[test]
    fn step_write_returns_err_epipe_when_all_readers_gone() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Simulate last-reader close via fd-table accounting.
        payload.decr_reader();
        drop(reader);
        assert_eq!(payload.reader_count_snapshot(), 0);
        let guard = guard();
        let outcome = step_write(&payload, b"x", &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EPIPE) => {}
            other => panic!("expected v3 Err(EPIPE), got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn step_write_yields_on_wait_source_when_full_and_blocking() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        // Fill the pipe exactly to its current byte capacity.
        let big = alloc::vec![b'x'; payload.pipe_size_bytes()];
        let guard = guard();
        let filled = step_write(&payload, &big, &guard, false);
        assert_eq!(filled, V3Out::Done(big.len()));
        // Next write blocks → Yield::OnWaitSource with empty progress.
        let outcome = step_write(&payload, b"y", &guard, false);
        drop(guard);
        match outcome {
            StepOutcome::Yield {
                progress,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
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
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
        let payload = payload_of(&reader);
        let big = alloc::vec![b'x'; payload.pipe_size_bytes()];
        let guard = guard();
        let _ = step_write(&payload, &big, &guard, false);
        let outcome = step_write(&payload, b"y", &guard, true);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
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
    use super::adapter::step_engine::{
        guard, ByteProgress, Errno as V3Errno, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
        StepProgress, YieldShape,
    };
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::structure::{RNodeBacking, StructPayload};
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        // Quiesce any deferred drops from prior tests so reader/writer
        // counts read cleanly here.
        tx_test_support::drain_to_quiescence();
        guard
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
        let mut op = Pipe2Op {
            flags: PipeFlags::default(),
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
            flags: PipeFlags {
                cloexec: true,
                nonblocking: true,
            },
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
        let mut empty: [u8; 0] = [];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut empty,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            nonblocking: true,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
                        // Seed via the free fn (its own guard scope so the StepOp wrap
                        // below acquires its own per STEP_MODEL §1).
        {
            let guard = guard();
            let _ = step_write(&payload, b"hello", &guard, false);
        }
        let mut buf = [0u8; 8];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            payload: &payload,
            out: &mut buf,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
        match outcome {
            StepOutcome::Yield {
                progress,
                shape: YieldShape::OnWaitSource { source, interests },
            } => {
                assert!(
                    progress.is_empty(),
                    "blocked-empty read must carry empty ByteProgress"
                );
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
        let bytes: &[u8] = &[];
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
        let bytes: &[u8] = b"hello";
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
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
        payload.decr_reader();
        drop(reader);
        assert_eq!(payload.reader_count_snapshot(), 0);
        let bytes: &[u8] = b"x";
        let mut op = WriteOp {
            payload: &payload,
            bytes,
            nonblocking: false,
        };
        let outcome = op.step(&mut ScriptCtx::<ProcessIdentity>::new());
        match outcome {
            StepOutcome::Err(V3Errno::EPIPE) => {}
            other => panic!("expected Err(EPIPE), got {other:?}"),
        }
        drop(writer);
    }
}
