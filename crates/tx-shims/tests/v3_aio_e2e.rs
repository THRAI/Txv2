//! PR-11 phase 6 — AIO end-to-end canary.
//!
//! This is **the** canary for the `OnBehalfOf<P>` runtime. It exercises
//! the complete Linux-style AIO loop end-to-end against a real
//! page-backed (tmpfs-shaped) file installed in P's fd table:
//!
//!   1. **Setup.** Bootstrap a process; create a page-backed file (the
//!      kernel's tmpfs backing) with known content; install the file at
//!      a known fd in P's fd table; call `sys_io_setup(nr_events,
//!      ctxp)` to allocate the AIO context fd.
//!   2. **Submit.** Build a `struct iocb` (PREAD, fd = file fd, buf =
//!      leaked user-VA buffer, nbytes = N, offset = 0); call
//!      `sys_io_submit(ctx_fd, 1, &iocb_ptr)` and assert it returns 1.
//!   3. **Worker drives.** The spawned worker (from W-CC's
//!      `spawn_worker_for_context` + W-FF's real dispatcher) pumps
//!      iocbs under the `OnBehalfOf<P>` borrow. The real dispatch path
//!      resolves `aio_fildes` against P's fd table, seeks the file to
//!      `aio_offset`, and dispatches the read through `OpenFileReadOp`
//!      (which routes `RNodeBacking::PageBacked` through
//!      `page_backed::step_read_to_kernel` after W-KK's 2026-05-12 fix).
//!   4. **Completion.** `sys_io_getevents(ctx, 1, 1, events, NULL)`
//!      drains one event with the dispatcher's result; the event
//!      structure (32-byte LE wire layout, cookie echoed in `data` and
//!      `obj`) round-trips through P's address space.
//!   5. **Verify.** The completion event's `data` field echoes the
//!      iocb's `aio_data` cookie; the event lands at the user's
//!      `events` pointer with the documented wire shape; the AIO
//!      context's completion queue is drained.
//!   6. **Cleanup.** `sys_io_destroy(ctx_fd)` trips the worker's
//!      cooperative-cancel; subsequent ops against the AIO fd return
//!      `-EBADF`; a final poll of the worker resolves it to
//!      `Err(CooperativeCancel(OwnerRequested))`.
//!
//! If this canary works end-to-end, PR-11's structural framework is
//! validated as a whole unit:
//!
//! - PR-W (W-W) `with_on_behalf_of` borrow primitive,
//! - PR-Z (W-Z) `Cap<AioContext>` fd-table backing,
//! - PR-CC (W-CC) per-context worker spawn,
//! - PR-FF (W-FF) real iocb dispatch + completion queue + io_getevents
//!   + io_destroy.
//!
//! # PageBacked dispatch (closed 2026-05-12 by W-KK)
//!
//! `OpenFile::step_read` now routes `RNodeBacking::PageBacked` through
//! `page_backed::step_read_to_kernel` (a kernel-buffer cousin to
//! `step_read_to_user`) — see `crates/tx-subsystems/src/vfs/execution.rs`.
//! The symmetric `step_write` path is wired through
//! `step_write_from_kernel`. The canary's PREAD round-trip therefore
//! returns the bytes read and copies them into P's address space via
//! `bootstrap_copy_to_user`; the assertion below pins `res == user_len`
//! and byte-equality against the seeded file content.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §11
//!   (success criteria) + §7 row P-11.7.
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution
//!   scope) + §8.2 (AIO worker example).
//! - `docs/Txv3/07_BLAST_RADIUS.md` §5.2 PR-11 + §7 success criteria.

extern crate alloc;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, TimeIf, UserPtr, VirtAddr,
};
use tx_shims::adapter::reactor_entry::SyscallRequest;
use tx_shims::adapter::step_engine::{
    self as zone, page_allocator, CancelReason, Cap, OnBehalfOfAbort,
};
use tx_subsystems::aio::{reset_context_id_counter_for_test, AioWorkerFuture, IOCB_CMD_PREAD};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::page_backed::{
    step_write_from_kernel, AnonSwapPolicy, PageContainer, PageContainerKind,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

use tx_shims::linux_syscall::aio::{reset_worker_registry_for_test, take_worker_future_for_test};
use tx_shims::linux_syscall::numbers::{NR_IO_DESTROY, NR_IO_GETEVENTS, NR_IO_SETUP, NR_IO_SUBMIT};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_aio_io_getevents.rs / v3_aio_io_destroy.rs) --

struct StubPmap;

impl PlatformConfig for StubPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "shims-v3-test";
}

#[derive(Default)]
struct StubPmapState {
    next_root: usize,
}

static STUB_PMAP_STATE: LazyLock<Mutex<StubPmapState>> =
    LazyLock::new(|| Mutex::new(StubPmapState { next_root: 1 }));

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = STUB_PMAP_STATE.lock().expect("stub pmap lock");
        let id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * USER_PAGE_SIZE)),
            Asid(id as u16),
        ))
    }
    fn destroy_pmap_root(_root: PmapRoot) {}
    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }
    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}
    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }
    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }
}

impl EntropyIf for StubPmap {}
impl tx_hal::AuxvIf for StubPmap {}
impl tx_hal::ConsoleIf for StubPmap {
    fn write_bytes(_bytes: &[u8]) {}
}
impl tx_hal::SmpIf for StubPmap {}

impl TimeIf for StubPmap {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

// -------- Setup ----------------------------------------------------

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    // The tmpfs (page-backed) file writes need the zero-frame page
    // installed; idempotent on re-entry.
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for aio e2e: {error:?}"),
    }
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    reset_context_id_counter_for_test();
    reset_worker_registry_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    proc_cap
        .nth_thread(0)
        .expect("alive process has leader thread")
}

fn make_ctx(process: Cap<ProcessIdentity>, thread: Cap<ThreadIdentity>) -> SyscallCtx<'static> {
    let aspace = process.aspace_cap().expect("alive aspace");
    SyscallCtx::new(process, thread, aspace)
}

fn block_on<F: Future>(mut fut: F) -> F::Output {
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve in 1024 polls");
}

fn dispatch_call(ctx: &SyscallCtx<'_>, req: SyscallRequest) -> SyscallResult {
    block_on(dispatch::<StubPmap>(req, ctx))
}

// -------- tmpfs file fixture ------------------------------------------
//
// A `PageContainer` of `PageContainerKind::Anon` is the kernel's
// tmpfs backing — `RNodeBacking::PageBacked { pc }` wraps it for
// VFS-level access. The container's `size_bytes` is initialised to
// `page_count * USER_PAGE_SIZE` so the AIO PREAD path's
// `step_lseek(SEEK_SET, offset)` resolves to a valid offset before
// the dispatcher invokes the read path. See the "Known gap" note in
// the file header for why byte-level content verification is deferred
// to a follow-up.

/// Build a `Cap<OpenFile>` backed by a fresh anon `PageContainer` of
/// `page_count` pages with `OpenFileFlags::{read,write}`. The container's
/// size is `page_count * USER_PAGE_SIZE` (the constructor seeds
/// `size_bytes` to capacity).
fn make_tmpfs_open_file(page_count: u64) -> Cap<OpenFile> {
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
    .expect("page container cap");
    let rnode = {
        let raw = RNode::new(
            FsObjectId::new(0xA10_E2E),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc },
        );
        let res = zone::reserve_for::<RNode>().expect("rnode reservation");
        zone::sign_for(res, raw)
    };
    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("open file cap")
}

/// Seed the page-backed file at `file` with `content` at offset 0. Uses a
/// temporary `OpenFile` to drive `step_write_from_kernel` over the PC; this
/// resets the temp file's offset to zero before writing so the byte payload
/// lands at the start. The caller's `file` offset is then reset to 0 so the
/// AIO PREAD path reads from the seeded region.
fn seed_file_content(file: &Cap<OpenFile>, content: &[u8]) {
    let backing = file.rnode().backing().clone();
    let pc = match &backing {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => panic!("seed_file_content: expected PageBacked rnode"),
    };
    // Build a writer-side OpenFile over the same rnode for the seed pass.
    // Using a fresh OpenFile keeps `file`'s offset state untouched (the
    // PREAD dispatch will then drive `file.offset()` through `step_lseek`
    // before reading).
    let writer_rnode = {
        let raw = RNode::new(
            FsObjectId::new(0xA10_E2E + 1),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc: pc.clone() },
        );
        let res = zone::reserve_for::<RNode>().expect("writer rnode reservation");
        zone::sign_for(res, raw)
    };
    let writer = OpenFile::new_cap(
        writer_rnode,
        OpenFileFlags {
            read: false,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("writer open file cap");
    let guard = zone::guard();
    match step_write_from_kernel(&pc, &writer, content, &guard) {
        zone::StepOutcome::Done(n) => assert_eq!(
            n,
            content.len(),
            "seed: step_write_from_kernel wrote {n}/{} bytes",
            content.len()
        ),
        other => panic!("seed: step_write_from_kernel unexpected {other:?}"),
    }
    // Reset the file-under-test's offset; step_lseek will set it again
    // before the PREAD body runs.
    file.set_offset(0);
}

fn encode_iocb(
    aio_data: u64,
    aio_lio_opcode: u16,
    aio_fildes: u32,
    aio_buf: u64,
    aio_nbytes: u64,
    aio_offset: i64,
) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0..8].copy_from_slice(&aio_data.to_le_bytes());
    buf[16..18].copy_from_slice(&aio_lio_opcode.to_le_bytes());
    buf[20..24].copy_from_slice(&aio_fildes.to_le_bytes());
    buf[24..32].copy_from_slice(&aio_buf.to_le_bytes());
    buf[32..40].copy_from_slice(&aio_nbytes.to_le_bytes());
    buf[40..48].copy_from_slice(&aio_offset.to_le_bytes());
    buf
}

const USER_AIO_IOCBPP: usize = 0x5200_0000;
const USER_AIO_EVENTS: usize = 0x5200_2000;
const USER_AIO_BUFFER: usize = 0x5200_3000;

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn map_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, len: usize) {
    let len = align_up(len.max(1), USER_PAGE_SIZE);
    let range = UserRange::new_aligned(UserVirtAddr(uaddr), len).expect("aligned user range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(request).expect("mmap anon for aio e2e");
}

fn copy_to_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) {
    let guard = zone::guard();
    let copied = ctx
        .aspace
        .copy_to_user(UserPtr::<u8>::new(uaddr), bytes, &guard);
    drop(guard);
    assert_eq!(copied, zone::StepOutcome::Done(bytes.len()));
}

fn copy_from_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, out: &mut [u8]) {
    let guard = zone::guard();
    let copied = ctx
        .aspace
        .copy_from_user(out, UserPtr::<u8>::new(uaddr), &guard);
    drop(guard);
    assert_eq!(copied, zone::StepOutcome::Done(out.len()));
}

fn stage_iocb_array(ctx: &SyscallCtx<'_>, base: usize, iocbs: &[[u8; 64]]) -> u64 {
    let pointer_bytes_len = iocbs.len() * core::mem::size_of::<u64>();
    let iocb_base = base + USER_PAGE_SIZE;
    map_user_bytes(ctx, base, pointer_bytes_len);
    map_user_bytes(ctx, iocb_base, iocbs.len() * 64);

    let mut pointer_bytes = alloc::vec![0u8; pointer_bytes_len];
    for (i, iocb) in iocbs.iter().enumerate() {
        let iocb_addr = iocb_base + i * 64;
        pointer_bytes[i * 8..(i + 1) * 8].copy_from_slice(&(iocb_addr as u64).to_le_bytes());
        copy_to_user_bytes(ctx, iocb_addr, iocb);
    }
    copy_to_user_bytes(ctx, base, &pointer_bytes);
    base as u64
}

/// Heap-allocate a buffer large enough for `n` `struct io_event` records
/// (32 bytes each) in the process address space.
fn stage_events_buffer(ctx: &SyscallCtx<'_>, uaddr: usize, n: usize) -> u64 {
    map_user_bytes(ctx, uaddr, n * 32);
    uaddr as u64
}

fn stage_user_buffer(ctx: &SyscallCtx<'_>, uaddr: usize, len: usize) -> u64 {
    map_user_bytes(ctx, uaddr, len);
    uaddr as u64
}

fn pump_worker_until<F>(mut worker: AioWorkerFuture, mut done: F, budget: u32) -> AioWorkerFuture
where
    F: FnMut(&AioWorkerFuture) -> bool,
{
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    for _ in 0..budget {
        let _ = pinned.as_mut().poll(&mut cx);
        if done(&pinned) {
            return worker;
        }
    }
    panic!("pump_worker_until: condition not met within {budget} polls");
}

// -------- Tests ---------------------------------------------------

/// The big six-step canary: setup tmpfs file; AIO setup; submit
/// PREAD; pump worker; getevents; verify byte-equality; destroy.
///
/// If this passes, PR-11's structural framework (W-W with_on_behalf_of,
/// W-Z AioContext, W-CC worker spawn, W-FF real dispatch, io_getevents,
/// and io_destroy) is validated end-to-end as a unit, including the
/// PageBacked-read seam (W-KK 2026-05-12) that copies real bytes from
/// a tmpfs file into P's address space.
#[test]
fn aio_pread_e2e_round_trip_against_tmpfs_file() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // 1. Create a tmpfs (page-backed) file. The page-backed RNode
    //    represents an in-memory tmpfs inode; the AIO worker's
    //    dispatcher resolves `aio_fildes` to this OpenFile against
    //    the borrow's principal.
    let file = make_tmpfs_open_file(/* page_count */ 1);

    // Seed the file with a known, non-zero, structured byte pattern so
    // the PREAD canary can pin byte-equality against the dispatcher's
    // user-buffer copy (W-KK 2026-05-12: closes W-JJ's PageBacked-read
    // gap). The pattern is `i as u8` for `i in 0..32` — easy to read
    // in a hexdump and distinct from all-zero / all-one fills.
    let file_content: alloc::vec::Vec<u8> = (0u8..32).collect();
    seed_file_content(&file, &file_content);

    // Install the file at fd 7 in P's fd table. fd 7 is chosen
    // arbitrarily above the bootstrap reserved fds.
    let file_fd: u32 = 7;
    proc_cap.set_fd(file_fd, Some(file));

    // 2. io_setup → AIO context fd.
    let aio_fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("io_setup expected Return, got {other:?}"),
    };
    assert_ne!(aio_fd, file_fd, "aio_fd must not collide with file_fd");

    let aio = proc_cap
        .fd(aio_fd)
        .expect("aio fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();

    let worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed by io_setup");

    // 3. Build and submit the PREAD iocb. The user buffer and iocb
    //    array are mapped into the process address space so the
    //    dispatcher exercises the canonical user-VA copy path.
    let user_len: usize = 32;
    let user_buf_addr = stage_user_buffer(&ctx, USER_AIO_BUFFER, user_len);
    let cookie: u64 = 0xCAFE_FEED_DEAD_BEEF;
    let iocb = encode_iocb(
        cookie,
        IOCB_CMD_PREAD,
        file_fd,
        user_buf_addr,
        user_len as u64,
        0, /* offset */
    );
    let iocbpp = stage_iocb_array(&ctx, USER_AIO_IOCBPP, &[iocb]);

    let submit_r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [aio_fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(
        submit_r,
        SyscallResult::Return(1),
        "io_submit admits one iocb"
    );

    // 4. Pump the worker until one completion lands. Budget capped at
    //    1000 per the W-JJ task spec — the real path completes in well
    //    under 100 polls.
    let _worker = pump_worker_until(worker, |_| aio.completion_len() >= 1, 1000);
    assert_eq!(aio.completion_len(), 1, "exactly one completion landed");

    // 5. io_getevents → drain the completion.
    let events_ptr = stage_events_buffer(&ctx, USER_AIO_EVENTS, 2);
    // timeout = 1 (any non-zero) means "non-blocking" in this canary's
    // semantic; the completion has already landed so blocking-vs-not
    // does not matter functionally.
    let get_r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [aio_fd as u64, 1, 2, events_ptr, 1, 0]),
    );
    assert_eq!(
        get_r,
        SyscallResult::Return(1),
        "io_getevents returns exactly one event"
    );

    // 6. Verify the event payload structure. The load-bearing pins:
    //
    //    (a) `data == cookie`: the worker dequeued the iocb and posted
    //        the completion with the user's cookie echoed back. This
    //        is the proof that the worker ran under the OnBehalfOf<P>
    //        borrow and invoked the dispatcher.
    //    (b) `obj == cookie`: the placeholder `obj` field carries the
    //        cookie too (today's documented stand-in for the kernel
    //        iocb pointer).
    //    (c) `res == user_len`: the dispatcher read the full requested
    //        byte count from the seeded file content (W-KK closed the
    //        PageBacked seam 2026-05-12).
    //    (d) `res2 == 0` for non-vectored PREAD.
    let mut event_bytes = [0u8; 32];
    copy_from_user_bytes(&ctx, events_ptr as usize, &mut event_bytes);
    let data = u64::from_le_bytes(event_bytes[0..8].try_into().unwrap());
    let obj = u64::from_le_bytes(event_bytes[8..16].try_into().unwrap());
    let res = i64::from_le_bytes(event_bytes[16..24].try_into().unwrap());
    let res2 = i64::from_le_bytes(event_bytes[24..32].try_into().unwrap());
    assert_eq!(data, cookie, "event.data echoes the iocb's aio_data cookie");
    assert_eq!(obj, cookie, "event.obj placeholder echoes the cookie");
    assert_eq!(res2, 0, "event.res2 is 0 for non-vectored PREAD");
    assert_eq!(
        res, user_len as i64,
        "event.res must equal the requested byte count (PageBacked-read seam, W-KK)"
    );

    // Byte-equality: the leaked user-side buffer holds the bytes the
    // dispatcher copied into P's address space via
    // `bootstrap_copy_to_user`. Compare against the seeded file
    // content to pin the full PREAD round-trip.
    let mut user_buf_view = alloc::vec![0u8; user_len];
    copy_from_user_bytes(&ctx, user_buf_addr as usize, &mut user_buf_view);
    assert_eq!(
        &user_buf_view[..],
        &file_content[..],
        "user buffer must equal seeded file content after PREAD"
    );

    // 7. The completion queue has been drained.
    assert_eq!(
        aio.completion_len(),
        0,
        "completion queue drained by getevents"
    );

    // 8. io_destroy → cleanup; subsequent ops return -EBADF.
    let destroy_r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [aio_fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(
        destroy_r,
        SyscallResult::Return(0),
        "io_destroy returns 0 on the open AIO fd"
    );
    let post = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [aio_fd as u64, 0, 1, events_ptr, 1, 0]),
    );
    assert_eq!(
        post,
        SyscallResult::Error(9 /* EBADF */),
        "post-destroy io_getevents returns -EBADF"
    );
}

/// Companion canary: mid-flight `io_destroy` cancels the worker
/// cleanly. Submits a PREAD against a real tmpfs file, immediately
/// calls `io_destroy` before pumping the worker, then confirms the
/// worker future resolves to `CooperativeCancel(OwnerRequested)` on
/// its next poll (or terminates cleanly if it raced ahead and drained
/// the iocb before the destroy landed).
#[test]
fn aio_e2e_mid_flight_destroy_cancels_worker() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let file = make_tmpfs_open_file(1);
    let file_fd: u32 = 7;
    proc_cap.set_fd(file_fd, Some(file));

    let aio_fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("io_setup: {other:?}"),
    };

    let aio = proc_cap
        .fd(aio_fd)
        .expect("aio fd")
        .aio_context()
        .expect("aio_context")
        .clone();
    let mut worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed");

    let user_buf_addr = stage_user_buffer(&ctx, USER_AIO_BUFFER, 32);
    let iocb = encode_iocb(0xBEEF, IOCB_CMD_PREAD, file_fd, user_buf_addr, 32, 0);
    let iocbpp = stage_iocb_array(&ctx, USER_AIO_IOCBPP, &[iocb]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [aio_fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Pump once so the body enters and parks on the empty-queue yield
    // (after possibly draining the single iocb — see below).
    let waker = Waker::noop().clone();
    let mut poll_cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    let _ = pinned.as_mut().poll(&mut poll_cx);

    // Destroy mid-flight.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [aio_fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));

    // Worker terminates on its next yield with cooperative cancel.
    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut poll_cx) {
            assert!(
                matches!(
                    out,
                    Err(OnBehalfOfAbort::CooperativeCancel(
                        CancelReason::OwnerRequested
                    )) | Ok(_)
                ),
                "mid-flight destroy → worker terminates cleanly, got {out:?}"
            );
            return;
        }
    }
    panic!("worker did not terminate within 64 polls after mid-flight destroy");
}
