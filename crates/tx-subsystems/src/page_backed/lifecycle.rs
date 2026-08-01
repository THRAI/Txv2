use super::*;
use crate::page_backed::adapter::step_engine::{
    self as step_engine, PageProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use alloc::vec::Vec;

enum SynchronousWritebackAdmission {
    Generation(PageGeneration),
    Legacy,
    Busy,
}

impl PageCacheIndex {
    fn withdraw_from(&mut self, first: PageIndex) {
        self.erase_from(first);
    }

    fn dirty_pages(&self) -> Vec<(PageIndex, Ppn)> {
        if !self.marked(PageCacheMark::Dirty) {
            return Vec::new();
        }
        self.collect_marked(PageCacheMark::Dirty)
            .into_iter()
            .map(|(page, entry)| (page, entry.ppn))
            .collect()
    }

    fn clear_dirty_if_match(&mut self, page: PageIndex, ppn: Ppn) {
        let Some(entry) = self.load(page) else {
            return;
        };
        if entry.ppn == ppn {
            let _ = self.clear_mark(page, PageCacheMark::Dirty);
            let _ = self.clear_mark(page, PageCacheMark::Writeback);
        }
    }
}

impl PageContainer {
    fn withdraw_cached_pages_from(&self, first: PageIndex) {
        let notify_ready: Vec<notification::PageReadyNotifier> = {
            let mut state = self.state.lock();
            state.pages.withdraw_from(first);
            state
                .in_flight_file_pages
                .split_off(&first)
                .into_iter()
                .filter_map(|(page, fetch)| {
                    Self::retire_file_page_fetch_wait(&mut state, page, fetch)
                })
                .collect()
        };
        for notifier in notify_ready {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
    }

    fn dirty_pages_snapshot(&self) -> Vec<(PageIndex, Ppn)> {
        self.state.lock().pages.dirty_pages()
    }

    fn clear_dirty_if_match(&self, page: PageIndex, ppn: Ppn) {
        self.state.lock().pages.clear_dirty_if_match(page, ppn);
    }

    fn begin_synchronous_writeback(
        &self,
        page: PageIndex,
        ppn: Ppn,
    ) -> SynchronousWritebackAdmission {
        let mut state = self.state.lock();
        let Some(snapshot) = state.file_page_slots.get(&page).map(PageSlot::snapshot) else {
            return SynchronousWritebackAdmission::Legacy;
        };
        let slot_ppn = match snapshot.state {
            PageSlotState::Dirty { ppn } | PageSlotState::Resident { ppn } => ppn,
            PageSlotState::Empty
            | PageSlotState::Fetching
            | PageSlotState::Writeback { .. }
            | PageSlotState::Error { .. } => return SynchronousWritebackAdmission::Busy,
        };
        if slot_ppn != ppn {
            return SynchronousWritebackAdmission::Busy;
        }
        if state
            .pages
            .set_mark(page, PageCacheMark::Writeback)
            .is_err()
        {
            return SynchronousWritebackAdmission::Busy;
        }
        let Some(writeback) = state
            .file_page_slots
            .get(&page)
            .and_then(|slot| slot.begin_writeback().ok())
        else {
            let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
            return SynchronousWritebackAdmission::Busy;
        };
        SynchronousWritebackAdmission::Generation(writeback.generation)
    }

    fn complete_synchronous_writeback(
        &self,
        page: PageIndex,
        generation: PageGeneration,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let slot = state.file_page_slots.get(&page).ok_or(Errno::EIO)?;
        let snapshot = slot
            .complete_writeback(generation, Ok(()))
            .map_err(|_| Errno::EIO)?;
        let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
        if matches!(snapshot.state, PageSlotState::Resident { .. }) {
            let _ = state.pages.clear_mark(page, PageCacheMark::Dirty);
        }
        Ok(())
    }

    fn abort_synchronous_writeback(&self, page: PageIndex, generation: PageGeneration) {
        let mut state = self.state.lock();
        if let Some(slot) = state.file_page_slots.get(&page) {
            let _ = slot.abort_writeback(generation);
        }
        let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
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

fn first_page_after_size(size: u64) -> Option<PageIndex> {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    if size == 0 {
        return Some(PageIndex::new(0));
    }
    size.checked_add(page_size - 1)
        .map(|rounded| PageIndex::new(rounded / page_size))
}

pub fn step_fsync(pc: &PageContainer, guard: &Guard<'_>) -> StepOutcome<(), PageProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;

    let PageContainerKind::File {
        mount,
        fs_object_id,
    } = pc.kind()
    else {
        return V3::done(());
    };

    let mut pages_so_far: u32 = 0;
    for (page, ppn) in pc.dirty_pages_snapshot() {
        let admission = pc.begin_synchronous_writeback(page, ppn);
        if matches!(admission, SynchronousWritebackAdmission::Busy) {
            let progress = if pages_so_far == 0 {
                PageProgress::EMPTY
            } else {
                PageProgress::new(pages_so_far)
            };
            return V3::continue_with(progress);
        }
        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            if let SynchronousWritebackAdmission::Generation(generation) = admission {
                pc.abort_synchronous_writeback(page, generation);
            }
            return V3::err(Errno::EINVAL.into());
        };
        match mount.payload().fs_page_backing.flush_page(
            *fs_object_id,
            offset,
            &Frame::new(ppn),
            guard,
        ) {
            V3::Done(()) => {
                match admission {
                    SynchronousWritebackAdmission::Generation(generation) => {
                        if let Err(errno) = pc.complete_synchronous_writeback(page, generation) {
                            return V3::err(errno.into());
                        }
                    }
                    SynchronousWritebackAdmission::Legacy => {
                        pc.clear_dirty_if_match(page, ppn);
                    }
                    SynchronousWritebackAdmission::Busy => unreachable!(),
                }
                pages_so_far = pages_so_far.saturating_add(1);
            }
            V3::Continue { progress: _ } => {
                if let SynchronousWritebackAdmission::Generation(generation) = admission {
                    pc.abort_synchronous_writeback(page, generation);
                }
                let progress = if pages_so_far == 0 {
                    PageProgress::EMPTY
                } else {
                    PageProgress::new(pages_so_far)
                };
                return V3::continue_with(progress);
            }
            V3::Yield { progress: _, shape } => {
                if let SynchronousWritebackAdmission::Generation(generation) = admission {
                    pc.abort_synchronous_writeback(page, generation);
                }
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    return V3::err(step_engine::Errno::EIO);
                };
                let progress = if pages_so_far == 0 {
                    PageProgress::EMPTY
                } else {
                    PageProgress::new(pages_so_far)
                };
                return crate::page_backed::notification::yield_on_wait_source(
                    progress, carrier, interests,
                );
            }
            V3::Err(v3_errno) => {
                if let SynchronousWritebackAdmission::Generation(generation) = admission {
                    pc.abort_synchronous_writeback(page, generation);
                }
                return V3::err(v3_errno);
            }
        }
    }

    // Persist the logical size once data blocks are written back:
    // `flush_page` writes data only, so without this a fresh reopen
    // sees the inode's stale (create-time) size and reads zero bytes.
    if pages_so_far > 0 {
        let size = pc.size_bytes();
        match mount
            .payload()
            .fs_page_backing
            .truncate(*fs_object_id, size, guard)
        {
            V3::Done(()) => {
                stamp_write_times(mount, *fs_object_id, guard);
            }
            V3::Continue { progress: _ } => return V3::continue_with(PageProgress::EMPTY),
            V3::Yield { progress: _, shape } => {
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    return V3::err(step_engine::Errno::EIO);
                };
                return crate::page_backed::notification::yield_on_wait_source(
                    PageProgress::EMPTY,
                    carrier,
                    interests,
                );
            }
            V3::Err(v3_errno) => return V3::err(v3_errno),
        }
    }

    match mount
        .payload()
        .fs_page_backing
        .fsync_file(*fs_object_id, guard)
    {
        V3::Done(()) => V3::done(()),
        V3::Continue { progress: _ } => {
            let progress = if pages_so_far == 0 {
                PageProgress::EMPTY
            } else {
                PageProgress::new(pages_so_far)
            };
            V3::continue_with(progress)
        }
        V3::Yield { progress: _, shape } => {
            let Some((carrier, interests)) =
                crate::page_backed::notification::wait_source_parts(&shape)
            else {
                return V3::err(step_engine::Errno::EIO);
            };
            let progress = if pages_so_far == 0 {
                PageProgress::EMPTY
            } else {
                PageProgress::new(pages_so_far)
            };
            crate::page_backed::notification::yield_on_wait_source(progress, carrier, interests)
        }
        V3::Err(v3_errno) => V3::err(v3_errno),
    }
}

/// Stamp mtime/ctime on the FS inode after a successful writeback so
/// metadata-based change detection (git's stat cache trusts size+mtime)
/// observes shell-redirect and applet writes. Close/fd-release is the
/// only flush point (no background writeback daemon), so stamping here
/// covers every writer. Best-effort: hosts without an installed
/// wall-clock source (unit tests) and filesystems without
/// `serialize_inode_meta` skip silently — the data flush above already
/// succeeded.
fn stamp_write_times(
    mount: &crate::mount::MountPayloadPin,
    fs_object_id: FsObjectId,
    guard: &Guard<'_>,
) {
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
    let Some(now_ns) = tx_services::time::realtime_now_ns_hooked() else {
        return;
    };
    let ts = crate::vfs::structure::Timespec::new(
        (now_ns / 1_000_000_000) as i64,
        (now_ns % 1_000_000_000) as i32,
    );
    let mut meta = match mount.payload().fs_ops.load_inode_meta(fs_object_id, guard) {
        V3::Done(meta) => meta,
        _ => return,
    };
    meta.mtime = ts;
    meta.ctime = ts;
    let _ = mount
        .payload()
        .fs_ops
        .serialize_inode_meta(fs_object_id, &meta, guard);
}

/// `step_truncate` — v3 outcome shape over `PageProgress`.
///
/// Same body and semantics as [`step_truncate`], translated to a v3
/// [`StepOutcome`]:
///
/// - `Device` / new_size > capacity → `Err(EINVAL)`
/// - fs `Done(())` then post-fs work → `Done(())`
/// - fs `Advanced(())` then post-fs work → `Continue { progress:
///   PageProgress::EMPTY }` (rerun, no page count to expose — see note)
/// - fs `Blocked(token)` → `Yield { progress: PageProgress::EMPTY, … }`
/// - fs `AdvancedThenBlocked((), token)` → `Yield { progress:
///   PageProgress::EMPTY, … }` (see note)
/// - fs `Err(e)` → `Err(e)`
///
/// `FsPageBacking::truncate` returns `T = ()`, so there is no per-step
/// page count to thread through; both yield/continue cases use
/// `PageProgress::EMPTY`. If interim page-step accounting for truncate
/// is ever needed, extend `FsPageBacking::truncate` to expose a
/// `pages` count or track it externally at call sites.
pub fn step_truncate(
    pc: &PageContainer,
    new_size: u64,
    guard: &Guard<'_>,
) -> StepOutcome<(), PageProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;

    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }

    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    if new_size > capacity {
        return V3::err(Errno::EINVAL.into());
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing
            .truncate(*fs_object_id, new_size, guard)
        {
            V3::Done(()) => false,
            V3::Continue { progress: _ } => true,
            V3::Yield { progress: _, shape } => {
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    return V3::err(step_engine::Errno::EIO);
                };
                return crate::page_backed::notification::yield_on_wait_source(
                    PageProgress::EMPTY,
                    carrier,
                    interests,
                );
            }
            V3::Err(v3_errno) => return V3::err(v3_errno),
        },
        PageContainerKind::Anon { .. } => false,
        // Device PC is rejected above; fall through safely.
        PageContainerKind::Device { .. } => false,
    };

    let old_size = pc.size_bytes();
    pc.set_size_bytes(new_size);

    if new_size < old_size {
        let Some(first_drop) = first_page_after_size(new_size) else {
            return V3::err(Errno::EINVAL.into());
        };
        pc.withdraw_cached_pages_from(first_drop);
        zero_partial_eof_tail(pc, new_size);
    }

    if fs_advanced {
        V3::continue_with(PageProgress::EMPTY)
    } else {
        V3::done(())
    }
}

pub fn step_fallocate(
    pc: &PageContainer,
    new_size: u64,
    guard: &Guard<'_>,
) -> StepOutcome<(), PageProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;

    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }

    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    if new_size > capacity {
        return V3::err(Errno::EINVAL.into());
    }

    if new_size <= pc.size_bytes() {
        return V3::done(());
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing
            .fallocate(*fs_object_id, new_size, guard)
        {
            V3::Done(()) => false,
            V3::Continue { progress: _ } => true,
            V3::Yield { progress: _, shape } => {
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    return V3::err(step_engine::Errno::EIO);
                };
                return crate::page_backed::notification::yield_on_wait_source(
                    PageProgress::EMPTY,
                    carrier,
                    interests,
                );
            }
            V3::Err(v3_errno) => return V3::err(v3_errno),
        },
        PageContainerKind::Anon { .. } => false,
        // Device PC is rejected above; fall through safely.
        PageContainerKind::Device { .. } => false,
    };

    pc.set_size_bytes(new_size);

    if fs_advanced {
        V3::continue_with(PageProgress::EMPTY)
    } else {
        V3::done(())
    }
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 pilot)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs by reference under a single lifetime `'a` and
// delegates from `step()` to the corresponding free fn above — semantics are
// unchanged. The free fns remain the source of truth; callers can migrate to
// the `*Op` types incrementally.

/// `StepOp` wrap of [`step_fsync`].
#[allow(dead_code)]// txdoc:pr2-step-op-scaffold
pub struct FsyncOp<'a> {
    pub pc: &'a PageContainer,
    state: FileFsyncState,
}

impl<'a> FsyncOp<'a> {
    #[allow(dead_code)] // constructed by syscall-side StepOp migration next
    pub const fn new(pc: &'a PageContainer) -> Self {
        Self {
            pc,
            state: FileFsyncState::new(),
        }
    }
}

impl<'a, I: SubjectIdentity> StepOp<I> for FsyncOp<'a> {
    type Output = ();
    type Progress = PageProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        use crate::page_backed::adapter::step_engine::StepOutcome as V3;

        let PageContainerKind::File { mount, .. } = self.pc.kind() else {
            return V3::done(());
        };
        if mount.payload().backend_planner().is_none() {
            let guard = step_engine::guard();
            return step_fsync(self.pc, &guard);
        }

        match self.state.advance(self.pc) {
            Err(errno) => V3::err(errno.into()),
            Ok(None) => self.state.pending_outcome(PageProgress::EMPTY),
            Ok(Some(Ok(()))) => V3::done(()),
            Ok(Some(Err(errno))) => V3::err(errno.into()),
        }
    }
}

/// `StepOp` wrap of [`step_truncate`].
///
/// Each `step()` call acquires its own epoch guard per STEP_MODEL_v2
/// §1; the op stays `Send` (REACTOR_v0, INVARIANTS_v5 EBR-7).
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct TruncateOp<'a> {
    pub pc: &'a PageContainer,
    pub new_size: u64,
}

impl<'a, I: SubjectIdentity> StepOp<I> for TruncateOp<'a> {
    type Output = ();
    type Progress = PageProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let guard = step_engine::guard();
        step_truncate(self.pc, self.new_size, &guard)
    }
}

/// `StepOp` wrap of [`step_fallocate`].
#[allow(dead_code)]// txdoc:pr2-step-op-scaffold
pub struct FallocateOp<'a> {
    pub pc: &'a PageContainer,
    pub new_size: u64,
}

impl<'a, I: SubjectIdentity> StepOp<I> for FallocateOp<'a> {
    type Output = ();
    type Progress = PageProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_fallocate(self.pc, self.new_size, &__guard)
    }
}

#[cfg(test)]
mod v3_tests {
    use super::*;
    use crate::execution::{Errno as V4Errno, WaitToken};
    use crate::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
    use crate::page_backed::{
        allocate_cached_frame, AnonSwapPolicy, CachedFrame, PageContainer, PageContainerKind,
        PageIndex,
    };
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta};
    use alloc::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use step_engine::page_allocator;
    use step_engine::{
        Errno as V3Errno, NoProgress, PageProgress, StepOutcome as V3Outcome, YieldShape,
    };
    use step_engine::{InterestMask, WaitSourceId};

    fn setup_host_substrate() {
        tx_test_support::init_host();
        crate::zones::register_all().expect("kernel zones");
        match step_engine::page_allocator::claim_zero_frame() {
            Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for v3 lifecycle tests: {error:?}"),
        }
    }

    struct LifecycleFs {
        flushes: AtomicUsize,
        fsyncs: AtomicUsize,
        last_object: AtomicU64,
        last_offset: AtomicU64,
        last_truncate_size: AtomicU64,
        block_flush_after: Option<usize>,
        truncate_outcome: StepOutcome<(), NoProgress>,
    }

    impl LifecycleFs {
        fn new() -> Self {
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            Self {
                flushes: AtomicUsize::new(0),
                fsyncs: AtomicUsize::new(0),
                last_object: AtomicU64::new(0),
                last_offset: AtomicU64::new(0),
                last_truncate_size: AtomicU64::new(0),
                block_flush_after: None,
                truncate_outcome: V3::done(()),
            }
        }

        fn blocking_after(first_done_count: usize) -> Self {
            Self {
                block_flush_after: Some(first_done_count),
                ..Self::new()
            }
        }

        fn failing_truncate(errno: V4Errno) -> Self {
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            Self {
                truncate_outcome: V3::err(errno.into()),
                ..Self::new()
            }
        }

        fn blocking_truncate(token: WaitToken) -> Self {
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            Self {
                truncate_outcome: V3::yield_on_wait_source(
                    NoProgress,
                    token.source_id(),
                    token.interest(),
                ),
                ..Self::new()
            }
        }

        fn advancing_truncate() -> Self {
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            Self {
                truncate_outcome: V3::continue_with(NoProgress),
                ..Self::new()
            }
        }
    }

    // Trait impls so the inner-mod LifecycleFs satisfies the
    // `FsOps` / `FsPageBacking` fields on `MountPayload`. Anything
    // that would go through `Blocked(_)` upgrades to `Err(EAGAIN)`
    // (this surface has no `NoProgress`-Blocked variant).
    impl crate::vfs::FsOps for LifecycleFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> V3Outcome<FsObjectId, NoProgress> {
            V3Outcome::err(V3Errno::ENOSYS)
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<InodeMeta, NoProgress> {
            V3Outcome::done(InodeMeta::new(InodeKind::Regular, 0o100644))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> V3Outcome<Option<(DirEntry, DirCursor)>, NoProgress> {
            V3Outcome::done(None)
        }

        fn destroy_inode(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::done(())
        }
    }

    impl crate::page_backed::FsPageBacking for LifecycleFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> V3Outcome<Frame, NoProgress> {
            V3Outcome::done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            ))
        }

        fn flush_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            let flush = self.flushes.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_offset.store(offset, Ordering::Release);
            if self.block_flush_after == Some(flush) {
                V3Outcome::yield_on_wait_source(NoProgress, 13, 0x55)
            } else {
                V3Outcome::done(())
            }
        }

        fn truncate(
            &self,
            fs_object_id: FsObjectId,
            new_size: u64,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_truncate_size.store(new_size, Ordering::Release);
            self.truncate_outcome
        }

        fn fsync_file(
            &self,
            fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            self.fsyncs.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            V3Outcome::done(())
        }
    }

    fn file_page_container(fs: Arc<LifecycleFs>, fs_object_id: FsObjectId) -> PageContainer {
        let mount = MountPayload::new_cap(
            fs.clone(),
            fs,
            None,
            DevId::new(8),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");
        PageContainer::new(
            PageContainerKind::File {
                mount: MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
                fs_object_id,
            },
            4,
        )
    }

    fn cached_frame_for_test() -> CachedFrame {
        setup_host_substrate();
        allocate_cached_frame().expect("cached frame")
    }

    // ------- step_fsync -------------------------------------------------

    #[test]
    fn fsync_v3_anon_returns_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
    }

    #[test]
    fn fsync_v3_no_dirty_pages_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::new());
        let pc = file_page_container(fs.clone(), FsObjectId::new(91));
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
        assert_eq!(fs.flushes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn fsync_v3_flushes_clean_pages_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::new());
        let pc = file_page_container(fs.clone(), FsObjectId::new(92));
        for page in [0u64, 1, 2] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
        assert_eq!(fs.flushes.load(Ordering::Acquire), 3);
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
    }

    #[test]
    fn fsync_v3_blocked_first_page_yields_with_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::blocking_after(0));
        let pc = file_page_container(fs.clone(), FsObjectId::new(93));
        for page in [0u64, 1] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(
            step_fsync(&pc, &guard),
            V3Outcome::Yield {
                progress: PageProgress::EMPTY,
                shape: YieldShape::OnWaitSource {
                    source: WaitSourceId::new(13),
                    interests: InterestMask::new(0x55),
                },
            }
        );
        // No clear-dirty happened on the blocked page.
        assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    }

    #[test]
    fn fsync_v3_blocked_after_progress_yields_with_pages_so_far() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::blocking_after(1));
        let pc = file_page_container(fs.clone(), FsObjectId::new(94));
        for page in [0u64, 1] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(
            step_fsync(&pc, &guard),
            V3Outcome::Yield {
                progress: PageProgress::new(1),
                shape: YieldShape::OnWaitSource {
                    source: WaitSourceId::new(13),
                    interests: InterestMask::new(0x55),
                },
            }
        );
        // The first flush did clear-dirty; the second blocked one didn't.
        assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
        assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 0);
    }

    // ------- step_truncate ---------------------------------------------

    #[test]
    fn truncate_v3_anon_shrink_done_and_withdraws_pages() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            4,
        );
        for page in 0..4 {
            pc.state
                .lock()
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
        }
        assert_eq!(
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::done(())
        );
        assert!(pc.lookup(PageIndex::new(0)).is_some());
        assert_eq!(pc.lookup(PageIndex::new(1)), None);
    }

    #[test]
    fn truncate_v3_device_returns_einval() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let device = PageContainer::new(
            PageContainerKind::Device {
                base_ppn: Ppn(0xface_3000),
                page_count: 1,
            },
            1,
        );
        assert_eq!(
            step_truncate(&device, 0, &guard),
            V3Outcome::err(V3Errno::EINVAL)
        );
    }

    #[test]
    fn truncate_v3_grow_past_capacity_returns_einval() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        assert_eq!(
            step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::err(V3Errno::EINVAL)
        );
    }

    #[test]
    fn truncate_v3_fs_err_propagates_unchanged_state() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::failing_truncate(V4Errno::EROFS));
        let pc = file_page_container(fs.clone(), FsObjectId::new(95));
        let original_size = pc.size_bytes();
        assert_eq!(
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::err(V3Errno::EROFS)
        );
        assert_eq!(pc.size_bytes(), original_size);
    }

    #[test]
    fn truncate_v3_fs_blocked_yields_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::blocking_truncate(WaitToken::new(7, 0x11)));
        let pc = file_page_container(fs.clone(), FsObjectId::new(96));
        let original_size = pc.size_bytes();
        assert_eq!(
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::Yield {
                progress: PageProgress::EMPTY,
                shape: YieldShape::OnWaitSource {
                    source: WaitSourceId::new(7),
                    interests: InterestMask::new(0x11),
                },
            }
        );
        // Size stays unpublished on a yield (post-fs work didn't run).
        assert_eq!(pc.size_bytes(), original_size);
    }

    #[test]
    fn truncate_v3_fs_advanced_returns_continue_with_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let fs = Arc::new(LifecycleFs::advancing_truncate());
        let pc = file_page_container(fs.clone(), FsObjectId::new(97));
        // Pick a shrink so the post-fs work runs (withdraw + zero-tail).
        pc.set_size_bytes(2 * crate::vm::USER_PAGE_SIZE as u64);
        let new_size = crate::vm::USER_PAGE_SIZE as u64;
        assert_eq!(
            step_truncate(&pc, new_size, &guard),
            V3Outcome::continue_with(PageProgress::EMPTY)
        );
        // Post-fs work *did* run, so size is published.
        assert_eq!(pc.size_bytes(), new_size);
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 pilot smoke tests for the `StepOp` wraps.
    //!
    //! Each test builds the `*Op` adapter, drives it through a single
    //! `.step(&mut ctx)` call, and pins the outcome variant. Compile-checks
    //! `impl StepOp` correctness; the heavy-lifting semantics tests live in
    //! the free-fn suite in `v3_tests` above.
    use super::*;
    use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
    use crate::test_support::EPOCH_TEST_LOCK;
    use step_engine::{PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome as V3Outcome};

    fn setup() {
        tx_test_support::init_host();
        crate::zones::register_all().expect("kernel zones");
        match step_engine::page_allocator::claim_zero_frame() {
            Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for step_op_wraps tests: {error:?}"),
        }
    }

    fn anon_pc(pages: u64) -> PageContainer {
        PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            pages,
        )
    }

    #[test]
    fn fsync_op_anon_returns_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup();
        let pc = anon_pc(1);
        let mut op = FsyncOp::new(&pc);
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Outcome::done(()));
    }

    #[test]
    fn truncate_op_anon_shrink_returns_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup();
        let pc = { anon_pc(4) };
        let new_size = crate::vm::USER_PAGE_SIZE as u64;
        // No outer epoch guard — `TruncateOp::step` acquires its own,
        // per STEP_MODEL_v2 §1.
        let mut op = TruncateOp { pc: &pc, new_size };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Outcome::done(()));
    }

    #[test]
    fn fallocate_op_anon_grow_returns_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup();
        let pc = anon_pc(4);
        // pc.size_bytes() starts at 0 for a fresh Anon container, so growing
        // to one page exercises the fs_advanced = false → Done(()) arm.
        let new_size = crate::vm::USER_PAGE_SIZE as u64;
        let mut op = FallocateOp { pc: &pc, new_size };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Outcome::done(()));
    }
}
