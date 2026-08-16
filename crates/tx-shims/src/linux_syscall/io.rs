//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::reactor_entry;
use crate::adapter::step_engine::Cap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_services::time::{timekeeper_clock, ClockRead, TimekeeperClock};
use tx_subsystems::vfs::FdReadyMask;

const PSELECT_READY_YIELD_INTERVAL: usize = 4;
const PSELECT_EMPTY_POLL_YIELD_INTERVAL: usize = 4;
static PSELECT_READY_RETURNS: AtomicUsize = AtomicUsize::new(0);
static PSELECT_EMPTY_POLL_RETURNS: AtomicUsize = AtomicUsize::new(0);
const EVENTFD_POLL_READABLE: u64 = 0x1;
const EVENTFD_POLL_WRITABLE: u64 = 0x2;
const TIMERFD_POLL_READABLE: u64 = 0x1;

fn raise_sigpipe(ctx: &SyscallCtx<'_>) {
    let _ = tx_subsystems::signal::step_kill_process_with_post(
        &ctx.process,
        tx_subsystems::signal::Signum::SIGPIPE,
        None,
        |mailbox, event| ctx.post_mailbox_event(mailbox, event),
    );
}

fn direct_mailbox_ref_post_with_hint(
    mailbox: &tx_substrate::wake::mailbox::TaskMailbox,
    event: tx_substrate::wake::mailbox::MailboxEvent,
    hint: tx_substrate::wake::mailbox::MailboxSchedulerHint,
) -> bool {
    mailbox.post_with_scheduler_hint(event, hint)
}

fn tty_readable_level(tty: &Cap<tx_subsystems::tty::structure::TtyIdentity>) -> bool {
    use tx_subsystems::tty::execution::TTY_READABLE;
    tty.input_readable.peek() & TTY_READABLE != 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticCharDevice {
    Null,
    Zero,
}

static STATIC_ZERO_READ_BUF: [u8; TTY_WRITE_MAX_INLINE] = [0u8; TTY_WRITE_MAX_INLINE];

fn static_char_device(file: &Cap<OpenFile>) -> Option<StaticCharDevice> {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload};

    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return None;
    };
    let RNodeBacking::StructBacked {
        payload: StructPayload::CharDevice(binding),
    } = rnode.backing()
    else {
        return None;
    };

    match (binding.name, binding.devt.major(), binding.devt.minor()) {
        ("null", 1, 3) => Some(StaticCharDevice::Null),
        ("zero", 1, 5) => Some(StaticCharDevice::Zero),
        _ => None,
    }
}

pub(super) fn dispatch_static_chardev_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    match req.nr {
        NR_READ => read_static_chardev_immediate(req.args, process, aspace),
        NR_WRITE => write_static_chardev_immediate(req.args, process),
        _ => None,
    }
}

pub(super) fn dispatch_static_chardev_cap_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
) -> Option<SyscallResult> {
    match req.nr {
        NR_WRITE => write_static_chardev_immediate(req.args, process),
        _ => None,
    }
}

fn write_static_chardev_immediate(
    args: [u64; 6],
    process: &Cap<ProcessIdentity>,
) -> Option<SyscallResult> {
    let fd = args[0] as i32;
    let len = args[2] as usize;
    if fd < 0 {
        return None;
    }

    let file = match resolve_fd(process, fd as u32) {
        Some(file) => file,
        None => return None,
    };
    if static_char_device(&file) != Some(StaticCharDevice::Null) {
        return None;
    }
    if !file.flags().write {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
    Some(SyscallResult::Return(len as i64))
}

fn read_static_chardev_immediate(
    args: [u64; 6],
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;
    if fd < 0 {
        return None;
    }

    let file = match resolve_fd(process, fd as u32) {
        Some(file) => file,
        None => return None,
    };
    if static_char_device(&file) != Some(StaticCharDevice::Zero) {
        return None;
    }
    if !file.flags().read {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
    if len == 0 {
        return Some(SyscallResult::Return(0));
    }
    if let Err(errno) = bootstrap_copy_to_user(aspace, buf_ptr as u64, &STATIC_ZERO_READ_BUF[..len])
    {
        return Some(SyscallResult::error_from(errno));
    }
    Some(SyscallResult::Return(len as i64))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PselectTimespecLayout {
    tv_sec: i64,
    tv_nsec: i64,
}

const FD_SETSIZE_MAX: u64 = 1024;
const FD_SET_WORD_BITS: u64 = 64;

fn read_pselect_timeout_ns(
    aspace: &Cap<AddressSpace>,
    timeout_ptr: u64,
) -> Result<Option<u64>, i32> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let ts =
        bootstrap_read_user::<PselectTimespecLayout>(aspace, timeout_ptr).map_err(errno_to_i32)?;
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(EINVAL_VALUE);
    }
    let ns = (ts.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|sec_ns| sec_ns.checked_add(ts.tv_nsec as u64))
        .ok_or(EINVAL_VALUE)?;
    Ok(Some(ns))
}

async fn read_pselect_timeout_ns_wait(
    aspace: &Cap<AddressSpace>,
    timeout_ptr: u64,
) -> Result<Option<u64>, i32> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let mut bytes = [0u8; core::mem::size_of::<PselectTimespecLayout>()];
    super::user_copy::bootstrap_copy_from_user_wait(aspace, &mut bytes, timeout_ptr)
        .await
        .map_err(errno_to_i32)?;
    let tv_sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let tv_nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 {
        return Err(EINVAL_VALUE);
    }
    let ns = (tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|sec_ns| sec_ns.checked_add(tv_nsec as u64))
        .ok_or(EINVAL_VALUE)?;
    Ok(Some(ns))
}

fn fdset_words(nfds: u64) -> usize {
    nfds.div_ceil(FD_SET_WORD_BITS) as usize
}

fn read_fdset(aspace: &Cap<AddressSpace>, ptr: u64, nfds: u64) -> Result<Option<Vec<u64>>, i32> {
    if ptr == 0 {
        return Ok(None);
    }
    let words = fdset_words(nfds);
    let mut set = Vec::with_capacity(words);
    for idx in 0..words {
        let word_ptr = ptr.wrapping_add((idx * core::mem::size_of::<u64>()) as u64);
        let word = bootstrap_read_user::<u64>(aspace, word_ptr).map_err(errno_to_i32)?;
        set.push(word);
    }
    Ok(Some(set))
}

fn write_fdset(aspace: &Cap<AddressSpace>, ptr: u64, set: Option<&[u64]>) -> Result<(), i32> {
    let Some(set) = set else {
        return Ok(());
    };
    for (idx, word) in set.iter().copied().enumerate() {
        let word_ptr = ptr.wrapping_add((idx * core::mem::size_of::<u64>()) as u64);
        bootstrap_write_user::<u64>(aspace, word_ptr, word).map_err(errno_to_i32)?;
    }
    Ok(())
}

fn fdset_has(set: Option<&[u64]>, fd: u64) -> bool {
    let Some(set) = set else {
        return false;
    };
    let word = (fd / FD_SET_WORD_BITS) as usize;
    let bit = fd % FD_SET_WORD_BITS;
    set.get(word)
        .map(|value| (value & (1u64 << bit)) != 0)
        .unwrap_or(false)
}

fn fdset_clear(set: Option<&mut [u64]>, fd: u64) {
    let Some(set) = set else {
        return;
    };
    let word = (fd / FD_SET_WORD_BITS) as usize;
    let bit = fd % FD_SET_WORD_BITS;
    if let Some(value) = set.get_mut(word) {
        *value &= !(1u64 << bit);
    }
}

fn timerfd_readable_level<P: tx_hal::TimeIf>(tfd: &tx_subsystems::timerfd::TimerFd) -> bool {
    let deadline = tfd.deadline_ns();
    tfd.expiration_count() > 0 || (deadline != 0 && P::read_ns() >= deadline)
}

fn include_timerfd_deadline(deadline_slot: &mut Option<u64>, deadline_ns: u64) {
    if deadline_ns == 0 {
        return;
    }
    *deadline_slot = Some(match *deadline_slot {
        Some(current) => core::cmp::min(current, deadline_ns),
        None => deadline_ns,
    });
}

fn pselect_fd_ready(
    file: &OpenFile,
    want_read: bool,
    want_write: bool,
    want_except: bool,
) -> (bool, bool, bool) {
    use tx_subsystems::{
        pipe::PipeSide,
        vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload},
    };

    let mut read_ready = false;
    let mut write_ready = false;
    let except_ready = false;

    if let Some(efd) = file.eventfd() {
        return (
            want_read && efd.counter() > 0,
            want_write && efd.counter() < tx_subsystems::eventfd::EVENTFD_MAX,
            false,
        );
    }

    if let OpenFileBacking::Rnode { rnode } = file.backing() {
        match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            } => {
                if want_read {
                    read_ready = tty_readable_level(tty);
                }
                if want_write {
                    write_ready = true;
                }
            }
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, side },
            } => match side {
                PipeSide::Reader => {
                    if want_read {
                        read_ready = payload.readable_level();
                    }
                }
                PipeSide::Writer => {
                    if want_write {
                        write_ready = payload.writable_level();
                    }
                }
            },
            _ => {
                read_ready = want_read;
                write_ready = want_write;
            }
        }
    }

    (read_ready, write_ready, want_except && except_ready)
}

/// Project the unified fd-ready mask into the `select(2)` read set.
/// Linux reports errors and peer shutdown as readable so the caller can
/// perform the final read and observe EOF/the pending socket error.
pub(super) fn select_fd_read_ready(want_read: bool, ready: FdReadyMask) -> bool {
    want_read
        && ready.intersects(
            FdReadyMask::READ | FdReadyMask::ERR | FdReadyMask::HUP | FdReadyMask::RDHUP,
        )
}

pub(super) fn select_fd_write_ready(want_write: bool, ready: FdReadyMask) -> bool {
    want_write && ready.intersects(FdReadyMask::WRITE | FdReadyMask::ERR)
}

/// Return the directions that still need wait registrations after a readiness
/// snapshot. Read and write remain independent so readiness in one direction
/// cannot suppress the other direction's wait registration.
pub(super) fn select_fd_blocked_directions(
    want_read: bool,
    want_write: bool,
    ready: FdReadyMask,
) -> (bool, bool) {
    (
        want_read && !select_fd_read_ready(true, ready),
        want_write && !select_fd_write_ready(true, ready),
    )
}

async fn sleep_timeout_ns<P: tx_hal::TimeIf>(ns: u64, ctx: &SyscallCtx<'_>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    if ns == 0 {
        return SyscallResult::Return(0);
    }
    let deadline_ns = timekeeper_clock::<P>()
        .monotonic_now_ns()
        .saturating_add(ns);
    sleep_until_deadline::<P>(deadline_ns, ns, ctx).await
}

fn read_ppoll_sigmask(
    ctx: &SyscallCtx<'_>,
    mask_ptr: u64,
    mask_size: u64,
) -> Result<Option<u64>, SyscallResult> {
    if mask_ptr == 0 {
        return Ok(None);
    }
    if mask_size < SIGSETSIZE_BYTES {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    match bootstrap_read_user::<u64>(&ctx.aspace, mask_ptr) {
        Ok(bits) => Ok(Some(bits)),
        Err(errno) => Err(SyscallResult::error_from(errno)),
    }
}

async fn read_ppoll_sigmask_wait(
    ctx: &SyscallCtx<'_>,
    mask_ptr: u64,
    mask_size: u64,
) -> Result<Option<u64>, SyscallResult> {
    if mask_ptr == 0 {
        return Ok(None);
    }
    if mask_size < SIGSETSIZE_BYTES {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    let mut bytes = [0u8; core::mem::size_of::<u64>()];
    match super::user_copy::bootstrap_copy_from_user_wait(&ctx.aspace, &mut bytes, mask_ptr).await {
        Ok(()) => Ok(Some(u64::from_le_bytes(bytes))),
        Err(errno) => Err(SyscallResult::error_from(errno)),
    }
}

fn set_thread_signal_mask(ctx: &SyscallCtx<'_>, mask_bits: u64) -> Result<u64, SyscallResult> {
    use tx_subsystems::{
        signal::SignalMask,
        thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange},
    };

    match step_sigprocmask(&ctx.thread, SigmaskHow::SetMask, SignalMask::new(mask_bits)) {
        SigprocmaskChange::Replaced { prev, .. } => Ok(prev.raw_bits()),
        SigprocmaskChange::ZombieIgnored => Err(SyscallResult::Error(ESRCH_VALUE)),
    }
}

fn clear_thread_pending_signals(ctx: &SyscallCtx<'_>, mask_bits: u64) {
    use tx_subsystems::signal::Signum;

    let Some(payload) = ctx.thread.payload_cap() else {
        return;
    };
    let mut cleared_any = false;
    for raw in 1..=Signum::MAX {
        let Some(sig) = Signum::new(raw) else {
            continue;
        };
        if mask_bits & sig.bit() != 0 {
            payload.pending().clear(sig);
            cleared_any = true;
        }
    }
    if cleared_any {
        tx_subsystems::signal::refresh_deliverable_signal_summary(&ctx.thread);
    }
}

fn restore_ppoll_sigmask(
    ctx: &SyscallCtx<'_>,
    saved_mask: Option<u64>,
    temporary_sigmask: Option<u64>,
    result: SyscallResult,
) -> SyscallResult {
    if let Some(bits) = saved_mask {
        if matches!(result, SyscallResult::Return(_)) {
            if let Some(temporary_bits) = temporary_sigmask {
                clear_thread_pending_signals(ctx, temporary_bits & !bits);
            }
        }
        let _ = set_thread_signal_mask(ctx, bits);
    }
    result
}

fn pselect_ready_return_should_yield() -> bool {
    PSELECT_READY_RETURNS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .is_multiple_of(PSELECT_READY_YIELD_INTERVAL)
}

fn pselect_empty_poll_should_yield() -> bool {
    PSELECT_EMPTY_POLL_RETURNS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .is_multiple_of(PSELECT_EMPTY_POLL_YIELD_INTERVAL)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PselectTimeout {
    Infinite,
    FiniteWait(u64),
    Poll,
}

fn pselect_timeout_policy<'a>(
    ctx: &SyscallCtx<'a>,
    timeout_ptr: u64,
) -> Result<PselectTimeout, i32> {
    if timeout_ptr == 0 {
        return Ok(PselectTimeout::Infinite);
    }

    let mut bytes = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, timeout_ptr).map_err(errno_to_i32)?;
    let sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(EINVAL_VALUE);
    }
    if sec == 0 && nsec == 0 {
        Ok(PselectTimeout::Poll)
    } else {
        let sec_ns = (sec as u64).saturating_mul(1_000_000_000);
        Ok(PselectTimeout::FiniteWait(
            sec_ns.saturating_add(nsec as u64),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PselectWaitWake {
    FdReady,
    TimedOut,
    /// The thread signal mailbox fired while parked. The caller checks for a
    /// deliverable pending signal and returns EINTR. Without this, ppoll/pselect
    /// could not be interrupted by a catchable signal — a process blocked in
    /// ppoll (e.g. hackbench workers polling their socketpairs) would never wake
    /// to process SIGTERM, so `kill` left them running and they leaked.
    Signalled,
}

fn push_unique_wait_token(
    tokens: &mut alloc::vec::Vec<tx_subsystems::execution::WaitToken>,
    token: tx_subsystems::execution::WaitToken,
) {
    if !tokens.contains(&token) {
        tokens.push(token);
    }
}

/// Park until one of `futures` is ready or a signal is posted. Returns `true`
/// if woken by the thread signal mailbox (caller then checks `select_next_signal`
/// and returns EINTR). Registering on the thread mailbox is what makes a blocked
/// ppoll/pselect interruptible — the fd futures each park on their own private
/// wait-source mailbox, so a posted signal would otherwise never wake the task.
async fn wait_on_any_token(
    mut futures: alloc::vec::Vec<wait_source::RegisteredWaitFuture>,
    mailbox: Option<&tx_substrate::wake::mailbox::TaskMailbox>,
) -> bool {
    core::future::poll_fn(|cx| {
        if let Some(mailbox) = mailbox {
            mailbox.register_waker(cx.waker().clone());
            let mut signalled = false;
            while let Some(event) = mailbox.poll() {
                if matches!(
                    event,
                    tx_substrate::wake::mailbox::MailboxEvent::SignalDelivered { .. }
                ) {
                    signalled = true;
                }
            }
            if mailbox.take_overflow() {
                // The concrete event was dropped. Re-scan fd readiness and
                // pending signals in the syscall's outer loop.
                return core::task::Poll::Ready(false);
            }
            if signalled {
                return core::task::Poll::Ready(true);
            }
        }
        for future in futures.iter_mut() {
            if core::future::Future::poll(core::pin::Pin::new(future), cx).is_ready() {
                return core::task::Poll::Ready(false);
            }
        }
        core::task::Poll::Pending
    })
    .await
}

async fn wait_on_any_token_or_pselect_deadline<P: tx_hal::TimeIf>(
    ctx: &SyscallCtx<'_>,
    mut fd_futures: alloc::vec::Vec<wait_source::RegisteredWaitFuture>,
    deadline_ns: u64,
    mailbox: Option<&tx_substrate::wake::mailbox::TaskMailbox>,
) -> PselectWaitWake
where
    TimekeeperClock<P>: ClockRead,
{
    if timekeeper_clock::<P>().monotonic_now_ns() >= deadline_ns {
        return PselectWaitWake::TimedOut;
    }
    let Some(mut timer_future) = super::deadline_timer(ctx, deadline_ns) else {
        return PselectWaitWake::TimedOut;
    };

    core::future::poll_fn(|cx| {
        if let Some(mailbox) = mailbox {
            mailbox.register_waker(cx.waker().clone());
            let mut signalled = false;
            while let Some(event) = mailbox.poll() {
                if matches!(
                    event,
                    tx_substrate::wake::mailbox::MailboxEvent::SignalDelivered { .. }
                ) {
                    signalled = true;
                }
            }
            if mailbox.take_overflow() {
                return core::task::Poll::Ready(PselectWaitWake::FdReady);
            }
            if signalled {
                return core::task::Poll::Ready(PselectWaitWake::Signalled);
            }
        }
        for future in fd_futures.iter_mut() {
            if core::future::Future::poll(core::pin::Pin::new(future), cx).is_ready() {
                return core::task::Poll::Ready(PselectWaitWake::FdReady);
            }
        }
        if core::future::Future::poll(core::pin::Pin::new(&mut timer_future), cx).is_ready() {
            return core::task::Poll::Ready(PselectWaitWake::TimedOut);
        }
        core::task::Poll::Pending
    })
    .await
}

async fn wait_until_pselect_deadline<P: tx_hal::TimeIf>(ctx: &SyscallCtx<'_>, deadline_ns: u64)
where
    TimekeeperClock<P>: ClockRead,
{
    if timekeeper_clock::<P>().monotonic_now_ns() >= deadline_ns {
        return;
    }
    let Some(timer_future) = super::deadline_timer(ctx, deadline_ns) else {
        return;
    };
    let _ = timer_future.await;
}

fn fdset_word_count(nfds: u64) -> u64 {
    nfds.div_ceil(64)
}

fn fdset_contains<'a>(ctx: &SyscallCtx<'a>, set_ptr: u64, fd: u64) -> Result<bool, i32> {
    if set_ptr == 0 {
        return Ok(false);
    }
    let word_idx = fd / 64;
    let bit = fd % 64;
    let mut bytes = [0u8; 8];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, set_ptr + word_idx * 8)
        .map_err(errno_to_i32)?;
    let word = u64::from_le_bytes(bytes);
    Ok(word & (1u64 << bit) != 0)
}

fn fdset_set(words: &mut [u64], fd: u64) {
    let word_idx = (fd / 64) as usize;
    let bit = fd % 64;
    words[word_idx] |= 1u64 << bit;
}

fn fdset_write<'a>(ctx: &SyscallCtx<'a>, set_ptr: u64, words: &[u64]) -> Result<(), i32> {
    if set_ptr == 0 {
        return Ok(());
    }
    for (idx, word) in words.iter().enumerate() {
        bootstrap_copy_to_user(&ctx.aspace, set_ptr + (idx as u64) * 8, &word.to_le_bytes())
            .map_err(errno_to_i32)?;
    }
    Ok(())
}

fn pselect_socket_write_ready(want_write: bool, mask: tx_subsystems::net::PollMask) -> bool {
    want_write
        && mask.intersects(tx_subsystems::net::PollMask::OUT | tx_subsystems::net::PollMask::ERR)
}

pub(super) fn pselect_socket_blocked_interests(
    want_read: bool,
    want_write: bool,
    mask: tx_subsystems::net::PollMask,
) -> (bool, bool) {
    (
        want_read && !pselect_socket_read_ready(true, mask),
        want_write && !pselect_socket_write_ready(true, mask),
    )
}

pub(super) fn pselect_socket_read_ready(
    want_read: bool,
    mask: tx_subsystems::net::PollMask,
) -> bool {
    want_read
        && mask.intersects(
            tx_subsystems::net::PollMask::IN
                | tx_subsystems::net::PollMask::ERR
                | tx_subsystems::net::PollMask::HUP
                | tx_subsystems::net::PollMask::RDHUP,
        )
}

/// `write(fd, buf, count)`.
///
/// Phase 2a restriction (per the trio plan §"Part 2 — Syscall table"
/// `write` row): the buffer is treated as kernel-side bytes, not a
/// user VA. `args[1]` is taken as a kernel pointer that already points
/// into kernel-readable memory (the test scaffolding allocates from
/// the test's stack/heap). General `copy_from_user` is out of scope
/// per §"Out of scope".
/// `writev(fd, iov, iovcnt)` — gather-write per `man 2 writev`.
///
/// musl's stdio (`fwrite` / `fputs` / etc.) uses `writev` rather than
/// `write` to flush its line-buffered stdio, so this is on busybox's
/// startup hot path: without it, every stdio write returns `-ENOSYS`,
/// busybox treats the negative return as a fatal error and crashes
/// while trying to print a diagnostic.
///
/// Implementation: walk the user's `struct iovec[iovcnt]`
/// (`[*const u8; 8]` + `usize`, 16 bytes per entry on RV64), copy
/// each entry into a kernel `iovec_local` and forward to `sys_write`.
/// Returns the cumulative byte count, with Linux's standard partial-
/// success policy: a short or failed write on entry N returns the
/// running total if `total > 0`, or the error from entry N otherwise.
pub(super) async fn sys_writev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    emit_debug_counter(b"debug.writev.enter", args[0] as i64);
    emit_debug_counter(b"debug.writev.iovcnt", iovcnt as i64);

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    let fd = args[0] as i32;
    let stdio_tty_file = if fd == 1 || fd == 2 {
        resolve_fd(&ctx.process, fd as u32).filter(|file| {
            matches!(
                file.rnode().backing(),
                tx_subsystems::vfs::RNodeBacking::StructBacked {
                    payload: tx_subsystems::vfs::StructPayload::Tty(_)
                }
            )
        })
    } else {
        None
    };
    if let Some(file) = stdio_tty_file {
        let mut combined = alloc::vec::Vec::new();
        for i in 0..iovcnt as u64 {
            let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
            let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
                return SyscallResult::error_from(errno);
            }
            let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
            if len == 0 {
                continue;
            }

            let old_len = combined.len();
            combined.resize(old_len + len as usize, 0);
            if let Err(errno) =
                bootstrap_copy_from_user(&ctx.aspace, &mut combined[old_len..], base)
            {
                return SyscallResult::error_from(errno);
            }
        }

        return sys_write_buffered(&file, &combined, ctx).await;
    }

    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let write_args = [args[0], base, len, 0, 0, 0];
        match sys_write(write_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                // Short write: stop here and return what we got. Linux
                // does the same — writev never silently combines past
                // a short write.
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
        // Yield the guard between iterations so the epoch can advance
        // and retired zone nodes can be reclaimed.  Without this, a
        // multi-element writev (common for musl's buffered stdio)
        // can exhaust the retired-node pool.
        drop(step_engine::guard());
    }
    SyscallResult::Return(total)
}

/// PageBacked-only synchronous `writev(2)` lane.
///
/// This is intentionally narrower than [`sys_writev`]: it claims only regular
/// PageBacked fds. A zero-progress `Yield` returns `None` so the async
/// `sys_writev` path owns the wait; a partial-progress `Yield` returns the
/// committed prefix as a Linux short write so that fallback cannot replay it.
/// Other fd kinds also return `None`.
pub(super) fn sys_writev_pagebacked_oneshot<'a>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let fd = args[0] as i32;
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    emit_debug_counter(b"debug.writev.pagebacked_oneshot.enter", fd as i64);
    emit_debug_counter(b"debug.writev.pagebacked_oneshot.iovcnt", iovcnt as i64);

    if !(0..=1024).contains(&iovcnt) {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    if iovcnt == 0 {
        return Some(SyscallResult::Return(0));
    }
    if fd < 0 {
        return Some(SyscallResult::Error(EBADF_VALUE));
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return Some(SyscallResult::Error(EBADF_VALUE)),
    };

    if file.posix_mq().is_some() || file.eventfd().is_some() {
        return None;
    }

    let pc = match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::PageBacked { pc } => pc.clone(),
            _ => return None,
        },
        _ => return None,
    };
    if !file.flags().write {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return Some(SyscallResult::Return(total));
            }
            return Some(SyscallResult::error_from(errno));
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap()) as usize;
        if len == 0 {
            continue;
        }

        emit_debug_counter(b"debug.write.enter", fd as i64);
        emit_debug_counter(b"debug.write.len", len as i64);
        emit_debug_counter(b"debug.write.pagebacked.len", len as i64);

        if let Some(range) = super::user_copy::covering_user_range(base, len) {
            use crate::adapter::step_engine::StepOutcome as V3;
            use tx_subsystems::vm::UserAccessKind;
            match ctx
                .aspace
                .reserve_user_range_for_access(range, UserAccessKind::Read)
            {
                V3::Done(()) => {}
                V3::Err(e) => {
                    if total > 0 {
                        return Some(SyscallResult::Return(total));
                    }
                    let errno: tx_subsystems::execution::Errno = e.into();
                    return Some(SyscallResult::error_from(errno));
                }
                V3::Yield { .. } | V3::Continue { .. } => {
                    if total > 0 {
                        return Some(SyscallResult::Return(total));
                    }
                    return None;
                }
            }
        }

        let guard = crate::adapter::step_engine::guard();
        // Select append EOF only once the iovec and its user range have been
        // validated. The offset-aware helper publishes the position only
        // after bytes move, preserving the old offset on zero-progress Yield.
        let start = if file.flags().append && total == 0 {
            pc.size_bytes()
        } else {
            file.offset()
        };
        match tx_subsystems::page_backed::step_write_from_user_at(
            &pc,
            &file,
            &ctx.aspace,
            tx_hal::UserPtr::<u8>::new(base as usize),
            len,
            start,
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(n) => {
                total += n as i64;
                if n < len {
                    return Some(SyscallResult::Return(total));
                }
            }
            tx_substrate::step::StepOutcome::Continue { progress } => {
                total += progress.bytes() as i64;
                return Some(SyscallResult::Return(total));
            }
            tx_substrate::step::StepOutcome::Err(e) => {
                if total > 0 {
                    return Some(SyscallResult::Return(total));
                }
                let errno: tx_subsystems::execution::Errno = e.into();
                if errno == tx_subsystems::execution::Errno::EPIPE {
                    let _ = tx_subsystems::signal::step_kill_process(
                        &ctx.process,
                        tx_subsystems::signal::Signum::SIGPIPE,
                        None,
                    );
                }
                return Some(SyscallResult::error_from(errno));
            }
            tx_substrate::step::StepOutcome::Yield { progress, .. } => {
                total += progress.bytes() as i64;
                if total > 0 {
                    return Some(SyscallResult::Return(total));
                }
                return None;
            }
        }
        drop(guard);
        drop(step_engine::guard());
    }

    Some(SyscallResult::Return(total))
}

pub(super) fn sys_writev_pagebacked_candidate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> bool {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let fd = args[0] as i32;
    if fd < 0 {
        return false;
    }
    let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
        return false;
    };
    if file.posix_mq().is_some() || file.eventfd().is_some() {
        return false;
    }
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => {
            matches!(rnode.backing(), RNodeBacking::PageBacked { .. })
        }
        _ => false,
    }
}

/// `readv(fd, iov, iovcnt)` — scatter-read counterpart of `sys_writev`.
pub(super) async fn sys_readv<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let read_args = [args[0], base, len, 0, 0, 0];
        match sys_read::<P>(read_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

/// `ppoll(fds, nfds, timeout_ptr, sigmask_ptr)`.
///
/// Polls socket, TTY, eventfd, timerfd, pipe and socketpair readiness
/// with level-triggered semantics. Socket wait interests stay split by
/// direction so a caller that asks for both `POLLIN` and `POLLOUT` can
/// wake on either side.
pub(super) async fn sys_ppoll<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    use tx_subsystems::{
        pipe::PipeSide,
        vfs::structure::{RNodeBacking, StructPayload},
    };

    let fds_ptr = args[0];
    let nfds = args[1];
    let timeout_ptr = args[2];
    let sigmask_ptr = args[3];
    let sigmask_size = args[4];
    let timeout_ns = match read_pselect_timeout_ns_wait(&ctx.aspace, timeout_ptr).await {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let temporary_sigmask = match read_ppoll_sigmask_wait(ctx, sigmask_ptr, sigmask_size).await {
        Ok(value) => value,
        Err(result) => return result,
    };

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let saved_mask = match temporary_sigmask {
        Some(bits) => match set_thread_signal_mask(ctx, bits) {
            Ok(prev) => Some(prev),
            Err(result) => return result,
        },
        None => None,
    };
    if nfds == 0 {
        let result = match timeout_ns {
            Some(ns) => sleep_timeout_ns::<P>(ns, ctx).await,
            None => loop {
                if tx_subsystems::signal::select_next_signal(&ctx.thread).is_some() {
                    break SyscallResult::Error(EINTR_VALUE);
                }

                use tx_scripts::drive;
                use tx_substrate::step::DriveMode;
                let now_ns = P::read_ns();
                let mut script_ctx = build_subject_script_ctx(ctx);
                let timer_registrar_handle = script_ctx.timer_registrar().cloned();
                let mailbox = ctx.mailbox.clone();
                let op = super::NanosleepOp {
                    nanos: 5_000_000,
                    deadline_ns: now_ns.saturating_add(5_000_000),
                    started: false,
                };
                match drive(
                    op,
                    &mut script_ctx,
                    DriveMode::Waiting,
                    mailbox.as_ref(),
                    None,
                    timer_registrar_handle.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(v3errno) => {
                        let errno: tx_subsystems::execution::Errno = v3errno.into();
                        if errno == tx_subsystems::execution::Errno::EINTR {
                            break SyscallResult::Error(EINTR_VALUE);
                        }
                        break SyscallResult::error_from(errno);
                    }
                }
            },
        };
        return restore_ppoll_sigmask(ctx, saved_mask, temporary_sigmask, result);
    }

    const POLLFD_BYTES: u64 = 8;
    const POLLIN: i16 = 0x0001;
    const POLLOUT: i16 = 0x0004;
    const POLLERR: i16 = 0x0008;
    const POLLHUP: i16 = 0x0010;
    const POLLNVAL: i16 = 0x0020;

    let timeout = match (timeout_ptr, timeout_ns) {
        (0, _) => PselectTimeout::Infinite,
        (_, Some(0)) => PselectTimeout::Poll,
        (_, Some(duration_ns)) => PselectTimeout::FiniteWait(duration_ns),
        (_, None) => PselectTimeout::Infinite,
    };
    let timeout_deadline_ns = match timeout {
        PselectTimeout::FiniteWait(duration_ns) => {
            Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(duration_ns))
        }
        PselectTimeout::Infinite | PselectTimeout::Poll => None,
    };

    let mut yielded_before_wait = false;
    let ready = loop {
        drive_loopback_pending();
        if let Some(deadline_ns) = timeout_deadline_ns {
            if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
                break 0;
            }
        }

        let mut ready: i64 = 0;
        let mut wait_tokens = alloc::vec::Vec::new();
        let mut effective_deadline_ns = timeout_deadline_ns;
        for i in 0..nfds {
            let ent_ptr = fds_ptr.wrapping_add(i * POLLFD_BYTES);
            let mut ent_bytes = [0u8; POLLFD_BYTES as usize];
            if let Err(errno) = super::user_copy::bootstrap_copy_from_user_wait(
                &ctx.aspace,
                &mut ent_bytes,
                ent_ptr,
            )
            .await
            {
                return restore_ppoll_sigmask(
                    ctx,
                    saved_mask,
                    temporary_sigmask,
                    SyscallResult::error_from(errno),
                );
            }
            let fd = i32::from_le_bytes(ent_bytes[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(ent_bytes[4..6].try_into().unwrap());
            let mut revents: i16 = 0;
            if fd >= 0 {
                if let Some(file) = resolve_fd(&ctx.process, fd as u32) {
                    let guard = tx_substrate::epoch::guard();
                    if let Some(tfd) = file.timerfd() {
                        if events & POLLIN != 0 {
                            if timerfd_readable_level::<P>(tfd) {
                                revents |= POLLIN;
                            } else {
                                include_timerfd_deadline(
                                    &mut effective_deadline_ns,
                                    tfd.deadline_ns(),
                                );
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        tfd.source_id(),
                                        TIMERFD_POLL_READABLE,
                                    ),
                                );
                            }
                        }
                    } else if let Some((rx, tx)) = file.socketpair_endpoint() {
                        if events & POLLIN != 0 {
                            if rx.readable_level() {
                                revents |= POLLIN;
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        rx.reader_source_id(),
                                        tx_subsystems::pipe::PIPE_READABLE,
                                    ),
                                );
                            }
                        }
                        if events & POLLOUT != 0 {
                            if tx.writable_level() {
                                revents |= POLLOUT;
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        tx.writer_source_id(),
                                        tx_subsystems::pipe::PIPE_WRITABLE,
                                    ),
                                );
                            }
                        }
                    } else if let Some(result) = socket_poll_mask_from_file(&file, &guard) {
                        match result {
                            Ok(mask) => {
                                let want_read = events & POLLIN != 0;
                                let want_write = events & POLLOUT != 0;
                                if want_read && mask.intersects(tx_subsystems::net::PollMask::IN) {
                                    revents |= POLLIN;
                                }
                                if want_write && mask.intersects(tx_subsystems::net::PollMask::OUT)
                                {
                                    revents |= POLLOUT;
                                }
                                if mask.intersects(tx_subsystems::net::PollMask::ERR) {
                                    revents |= POLLERR;
                                }
                                if mask.intersects(tx_subsystems::net::PollMask::HUP) {
                                    revents |= POLLHUP;
                                }
                                if timeout != PselectTimeout::Poll && revents == 0 {
                                    let (read_blocked, write_blocked) =
                                        pselect_socket_blocked_interests(
                                            want_read, want_write, mask,
                                        );
                                    if read_blocked {
                                        match socket_poll_wait_token_from_file(
                                            &file,
                                            tx_subsystems::net::PollMask::IN,
                                            &guard,
                                        ) {
                                            Some(Ok(Some(token))) => {
                                                push_unique_wait_token(&mut wait_tokens, token);
                                            }
                                            Some(Ok(None)) | None => {}
                                            Some(Err(errno)) => {
                                                return restore_ppoll_sigmask(
                                                    ctx,
                                                    saved_mask,
                                                    temporary_sigmask,
                                                    SyscallResult::Error(errno_to_i32(errno)),
                                                );
                                            }
                                        }
                                    }
                                    if write_blocked {
                                        match socket_poll_wait_token_from_file(
                                            &file,
                                            tx_subsystems::net::PollMask::OUT,
                                            &guard,
                                        ) {
                                            Some(Ok(Some(token))) => {
                                                push_unique_wait_token(&mut wait_tokens, token);
                                            }
                                            Some(Ok(None)) | None => {}
                                            Some(Err(errno)) => {
                                                return restore_ppoll_sigmask(
                                                    ctx,
                                                    saved_mask,
                                                    temporary_sigmask,
                                                    SyscallResult::Error(errno_to_i32(errno)),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            Err(errno) => {
                                return restore_ppoll_sigmask(
                                    ctx,
                                    saved_mask,
                                    temporary_sigmask,
                                    SyscallResult::Error(errno_to_i32(errno)),
                                );
                            }
                        }
                    } else {
                        let mut handled = false;
                        match file.rnode().backing() {
                            RNodeBacking::StructBacked {
                                payload: StructPayload::Tty(tty),
                            } => {
                                handled = true;
                                if events & POLLIN != 0 {
                                    if tty_readable_level(tty) {
                                        revents |= POLLIN;
                                    } else {
                                        push_unique_wait_token(
                                            &mut wait_tokens,
                                            tx_subsystems::execution::WaitToken::new(
                                                tty.wait_source_id(),
                                                POLLIN as u64,
                                            ),
                                        );
                                    }
                                }
                                if events & POLLOUT != 0 {
                                    revents |= POLLOUT;
                                }
                            }
                            RNodeBacking::StructBacked {
                                payload: StructPayload::Pipe { payload, side },
                            } => {
                                handled = true;
                                match side {
                                    PipeSide::Reader => {
                                        if events & POLLIN != 0 {
                                            if payload.readable_level() {
                                                revents |= POLLIN;
                                            } else {
                                                push_unique_wait_token(
                                                    &mut wait_tokens,
                                                    tx_subsystems::execution::WaitToken::new(
                                                        payload.reader_source_id(),
                                                        tx_subsystems::pipe::PIPE_READABLE,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    PipeSide::Writer => {
                                        if events & POLLOUT != 0 {
                                            if payload.writable_level() {
                                                revents |= POLLOUT;
                                            } else {
                                                push_unique_wait_token(
                                                    &mut wait_tokens,
                                                    tx_subsystems::execution::WaitToken::new(
                                                        payload.writer_source_id(),
                                                        tx_subsystems::pipe::PIPE_WRITABLE,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        if !handled {
                            if events & POLLIN != 0 {
                                revents |= POLLIN;
                            }
                            if events & POLLOUT != 0 {
                                revents |= POLLOUT;
                            }
                        }
                    }
                } else {
                    revents = POLLNVAL;
                }
            }
            if revents != 0 {
                ready += 1;
            }
            ent_bytes[6..8].copy_from_slice(&revents.to_le_bytes());
            if let Err(errno) =
                super::user_copy::bootstrap_copy_to_user_wait(&ctx.aspace, ent_ptr, &ent_bytes)
                    .await
            {
                return restore_ppoll_sigmask(
                    ctx,
                    saved_mask,
                    temporary_sigmask,
                    SyscallResult::error_from(errno),
                );
            }
        }

        if ready > 0 {
            break ready;
        }
        if timeout == PselectTimeout::Poll {
            break 0;
        }

        if !yielded_before_wait && !wait_tokens.is_empty() {
            yielded_before_wait = true;
            tx_reactor::yield_now().await;
            continue;
        }
        if wait_tokens.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(ctx, deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break 0;
        }

        let futures = wait_tokens
            .into_iter()
            .filter_map(wait_source::wait_on_token)
            .collect::<alloc::vec::Vec<_>>();
        if futures.is_empty() {
            if let Some(deadline_ns) = timeout_deadline_ns {
                wait_until_pselect_deadline::<P>(ctx, deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break 0;
        }
        if let Some(deadline_ns) = effective_deadline_ns {
            match wait_on_any_token_or_pselect_deadline::<P>(
                ctx,
                futures,
                deadline_ns,
                ctx.mailbox.as_deref(),
            )
            .await
            {
                PselectWaitWake::FdReady => {}
                PselectWaitWake::TimedOut => break 0,
                PselectWaitWake::Signalled => {
                    if tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread) {
                        return restore_ppoll_sigmask(
                            ctx,
                            saved_mask,
                            temporary_sigmask,
                            SyscallResult::Error(EINTR_VALUE),
                        );
                    }
                }
            }
        } else if wait_on_any_token(futures, ctx.mailbox.as_deref()).await
            && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
        {
            return restore_ppoll_sigmask(
                ctx,
                saved_mask,
                temporary_sigmask,
                SyscallResult::Error(EINTR_VALUE),
            );
        }
    };

    if ready == 0 && timeout == PselectTimeout::Poll && pselect_empty_poll_should_yield() {
        tx_reactor::yield_now().await;
    }

    restore_ppoll_sigmask(
        ctx,
        saved_mask,
        temporary_sigmask,
        SyscallResult::Return(ready),
    )
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)`.
///
/// musl implements `select(2)` on RV64 through the generic `pselect6`
/// syscall. This keeps the implementation close to `sys_ppoll`: sockets
/// use the network readiness projection, timerfd/eventfd/pipe/TTY are
/// level-polled, and socketpairs keep the current branch's pipe-backed
/// endpoint semantics.
pub(super) async fn sys_pselect6<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    use tx_subsystems::vfs::structure::{InodeKind, RNodeBacking, StructPayload};

    let nfds = args[0];
    let readfds = args[1];
    let writefds = args[2];
    let exceptfds = args[3];
    let timeout_ptr = args[4];

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    let timeout = match pselect_timeout_policy(ctx, timeout_ptr) {
        Ok(wait) => wait,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let timeout_deadline_ns = match timeout {
        PselectTimeout::FiniteWait(duration_ns) => {
            Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(duration_ns))
        }
        PselectTimeout::Infinite | PselectTimeout::Poll => None,
    };
    let word_count = fdset_word_count(nfds);
    let mut yielded_before_wait = false;
    let (read_ready, write_ready, except_ready, ready_count) = loop {
        drive_loopback_pending();
        if let Some(deadline_ns) = timeout_deadline_ns {
            if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
                break (
                    alloc::vec![0u64; word_count as usize],
                    alloc::vec![0u64; word_count as usize],
                    alloc::vec![0u64; word_count as usize],
                    0,
                );
            }
        }
        let mut read_ready = alloc::vec![0u64; word_count as usize];
        let mut write_ready = alloc::vec![0u64; word_count as usize];
        let mut except_ready = alloc::vec![0u64; word_count as usize];
        let mut ready_count: i64 = 0;
        let mut wait_tokens = alloc::vec::Vec::new();
        let mut effective_deadline_ns = timeout_deadline_ns;

        for fd in 0..nfds {
            let want_read = match fdset_contains(ctx, readfds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            let want_write = match fdset_contains(ctx, writefds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            let want_except = match fdset_contains(ctx, exceptfds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            if !want_read && !want_write && !want_except {
                continue;
            }
            let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
                return SyscallResult::Error(EBADF_VALUE);
            };

            let mut fd_ready = false;
            let guard = tx_substrate::epoch::guard();
            if let Some(efd) = file.eventfd() {
                if want_read {
                    if efd.counter() > 0 {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                efd.reader_source_id(),
                                EVENTFD_POLL_READABLE,
                            ),
                        );
                    }
                }
                if want_write {
                    if efd.counter() < tx_subsystems::eventfd::EVENTFD_MAX {
                        fdset_set(&mut write_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                efd.writer_source_id(),
                                EVENTFD_POLL_WRITABLE,
                            ),
                        );
                    }
                }
            } else if let Some(tfd) = file.timerfd() {
                if want_read {
                    if timerfd_readable_level::<P>(tfd) {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    } else {
                        include_timerfd_deadline(&mut effective_deadline_ns, tfd.deadline_ns());
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                tfd.source_id(),
                                TIMERFD_POLL_READABLE,
                            ),
                        );
                    }
                }
            } else if let Some((rx, tx)) = file.socketpair_endpoint() {
                if want_read {
                    if rx.readable_level() {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                rx.reader_source_id(),
                                tx_subsystems::pipe::PIPE_READABLE,
                            ),
                        );
                    }
                }
                if want_write {
                    if tx.writable_level() {
                        fdset_set(&mut write_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                tx.writer_source_id(),
                                tx_subsystems::pipe::PIPE_WRITABLE,
                            ),
                        );
                    }
                }
            } else if let Some(result) = socket_poll_mask_from_file(&file, &guard) {
                let mask = match result {
                    Ok(mask) => mask,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                if pselect_socket_read_ready(want_read, mask) {
                    fdset_set(&mut read_ready, fd);
                    fd_ready = true;
                }
                if pselect_socket_write_ready(want_write, mask) {
                    fdset_set(&mut write_ready, fd);
                    fd_ready = true;
                }
                if want_except && mask.intersects(tx_subsystems::net::PollMask::ERR) {
                    fdset_set(&mut except_ready, fd);
                    fd_ready = true;
                }
                let (read_blocked, write_blocked) =
                    pselect_socket_blocked_interests(want_read, want_write, mask);
                if read_blocked {
                    match socket_poll_wait_token_from_file(
                        &file,
                        tx_subsystems::net::PollMask::IN,
                        &guard,
                    ) {
                        Some(Ok(Some(token))) => {
                            push_unique_wait_token(&mut wait_tokens, token);
                        }
                        Some(Ok(None)) | None => {}
                        Some(Err(errno)) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                if write_blocked {
                    match socket_poll_wait_token_from_file(
                        &file,
                        tx_subsystems::net::PollMask::OUT,
                        &guard,
                    ) {
                        Some(Ok(Some(token))) => {
                            push_unique_wait_token(&mut wait_tokens, token);
                        }
                        Some(Ok(None)) | None => {}
                        Some(Err(errno)) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
            } else {
                if want_read {
                    let readable = match file.rnode().backing() {
                        RNodeBacking::StructBacked {
                            payload: StructPayload::Tty(tty),
                        } => {
                            use tx_subsystems::tty::execution::TTY_READABLE;
                            if tty.input_readable.peek() & TTY_READABLE != 0 {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        tty.wait_source_id(),
                                        TTY_READABLE,
                                    ),
                                );
                                false
                            }
                        }
                        RNodeBacking::StructBacked {
                            payload:
                                StructPayload::Pipe {
                                    payload,
                                    side: tx_subsystems::pipe::PipeSide::Reader,
                                },
                        } => {
                            if payload.readable_level() {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        payload.reader_source_id(),
                                        tx_subsystems::pipe::PIPE_READABLE,
                                    ),
                                );
                                false
                            }
                        }
                        // Regular files are always ready for reading in
                        // select/poll (POSIX); FIFOs/sockets/etc. are not.
                        RNodeBacking::PageBacked { .. } => {
                            matches!(file.rnode().meta().kind(), InodeKind::Regular)
                        }
                        _ => false,
                    };
                    if readable {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    }
                }
                if want_write {
                    let writable = match file.rnode().backing() {
                        RNodeBacking::StructBacked {
                            payload:
                                StructPayload::Pipe {
                                    payload,
                                    side: tx_subsystems::pipe::PipeSide::Writer,
                                },
                        } => {
                            if payload.writable_level() {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        payload.writer_source_id(),
                                        tx_subsystems::pipe::PIPE_WRITABLE,
                                    ),
                                );
                                false
                            }
                        }
                        // Regular files are always ready for writing in
                        // select/poll (POSIX).
                        RNodeBacking::PageBacked { .. } => {
                            matches!(file.rnode().meta().kind(), InodeKind::Regular)
                        }
                        _ => false,
                    };
                    if writable {
                        fdset_set(&mut write_ready, fd);
                        fd_ready = true;
                    }
                }
            }

            if fd_ready {
                ready_count += 1;
            }
        }

        if ready_count != 0 || timeout == PselectTimeout::Poll {
            break (read_ready, write_ready, except_ready, ready_count);
        }
        if !yielded_before_wait && !wait_tokens.is_empty() {
            yielded_before_wait = true;
            tx_reactor::yield_now().await;
            continue;
        }
        if wait_tokens.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(ctx, deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break (read_ready, write_ready, except_ready, ready_count);
        };
        let futures = wait_tokens
            .into_iter()
            .filter_map(wait_source::wait_on_token)
            .collect::<alloc::vec::Vec<_>>();
        if !futures.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                match wait_on_any_token_or_pselect_deadline::<P>(
                    ctx,
                    futures,
                    deadline_ns,
                    ctx.mailbox.as_deref(),
                )
                .await
                {
                    PselectWaitWake::FdReady => {}
                    PselectWaitWake::TimedOut => {
                        break (read_ready, write_ready, except_ready, ready_count);
                    }
                    PselectWaitWake::Signalled => {
                        if tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread) {
                            return SyscallResult::Error(EINTR_VALUE);
                        }
                    }
                }
            } else if wait_on_any_token(futures, ctx.mailbox.as_deref()).await
                && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
            {
                return SyscallResult::Error(EINTR_VALUE);
            }
        } else {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(ctx, deadline_ns).await;
            }
            break (read_ready, write_ready, except_ready, ready_count);
        }
    };

    if ready_count != 0 && pselect_ready_return_should_yield() {
        tx_reactor::yield_now().await;
    }
    if ready_count == 0 && timeout == PselectTimeout::Poll && pselect_empty_poll_should_yield() {
        tx_reactor::yield_now().await;
    }

    if let Err(errno) = fdset_write(ctx, readfds, &read_ready) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = fdset_write(ctx, writefds, &write_ready) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = fdset_write(ctx, exceptfds, &except_ready) {
        return SyscallResult::Error(errno);
    }

    SyscallResult::Return(ready_count)
}

/// PageBacked `write(2)` — direct user-buffer path.
///
/// Prefaults the user buffer through `reserve_user_range_for_access`,
/// then drives `OpenFileWriteFromUserOp` which copies bytes directly
/// from user pages to PC frames through the pmap, without kernel-buffer
/// staging (PAGE_BACKED_v1 §5.1 / §3 prefault discipline).
async fn sys_write_pagebacked<'a>(
    file: &Cap<tx_subsystems::vfs::structure::OpenFile>,
    buf_ptr: usize,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    emit_debug_counter(b"debug.write.pagebacked.len", len as i64);
    if len == 0 {
        return SyscallResult::Return(0);
    }
    emit_debug_counter(b"debug.write.pagebacked.phase", 0);

    // Prefault: eagerly materialise every user page and publish to
    // the pmap so the step loop below finds every page in the cache.
    // If any page is unmapped or has a prot mismatch, fail before
    // transferring any bytes.
    if let Some(range) = super::user_copy::covering_user_range(buf_ptr as u64, len) {
        use tx_subsystems::vm::UserAccessKind;
        emit_debug_counter(b"debug.write.pagebacked.phase", 1);
        match ctx
            .aspace
            .reserve_user_range_for_access_wait(range, UserAccessKind::Read)
            .await
        {
            Ok(()) => {
                emit_debug_counter(b"debug.write.pagebacked.phase", 2);
            }
            Err(errno) => {
                emit_debug_counter(b"debug.write.pagebacked.err", 1);
                return SyscallResult::error_from(errno);
            }
        }
    } else {
        emit_debug_counter(b"debug.write.pagebacked.phase", 2);
    }

    // Drive the write-from-user step loop.
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteFromUserOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileWriteFromUserOp {
        file,
        aspace: &ctx.aspace,
        src: tx_hal::UserPtr::<u8>::new(buf_ptr),
        len,
        cursor: 0,
    };
    emit_debug_counter(b"debug.write.pagebacked.phase", 3);
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(total) => {
            emit_debug_counter(b"debug.write.pagebacked.done", total as i64);
            emit_debug_counter(b"debug.write.pagebacked.phase", 4);
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            emit_debug_counter(b"debug.write.pagebacked.err", 3);
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            if errno == tx_subsystems::execution::Errno::EPIPE {
                let _ = tx_subsystems::signal::step_kill_process(
                    &ctx.process,
                    tx_subsystems::signal::Signum::SIGPIPE,
                    None,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

async fn sys_pipe_write_buffered<'a>(
    payload: &Cap<tx_subsystems::pipe::PipePayload>,
    bytes: &[u8],
    nonblocking: bool,
    packet_mode: bool,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::pipe::WriteWithHintPostOp;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let mode = if nonblocking || mailbox_arc.is_none() {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = WriteWithHintPostOp {
        payload,
        bytes,
        nonblocking,
        packet_mode,
        post: ctx
            .mailbox_ref_post_with_hint
            .unwrap_or(direct_mailbox_ref_post_with_hint),
    };

    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(total) => SyscallResult::Return(total as i64),
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            if errno == tx_subsystems::execution::Errno::EPIPE {
                raise_sigpipe(ctx);
            }
            SyscallResult::error_from(errno)
        }
    }
}

async fn sys_pipe_read_buffered<'a, P: tx_hal::ConsoleIf>(
    payload: &Cap<tx_subsystems::pipe::PipePayload>,
    fd: i32,
    lane: &str,
    buf_ptr: usize,
    len: usize,
    nonblocking: bool,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::pipe::ReadWithHintPostOp;

    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let mode = if nonblocking || mailbox_arc.is_none() {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = ReadWithHintPostOp {
        payload,
        out: &mut staging,
        nonblocking,
        post: ctx
            .mailbox_ref_post_with_hint
            .unwrap_or(direct_mailbox_ref_post_with_hint),
    };

    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(total) => {
            if total > 0 {
                if let Err(failure) = super::user_copy::bootstrap_copy_to_user_wait_diagnosed(
                    &ctx.aspace,
                    buf_ptr as u64,
                    &staging[..total],
                )
                .await
                {
                    if failure.errno == tx_subsystems::execution::Errno::EIO {
                        report_pipe_read_eio::<P>(
                            "copy-to-user",
                            lane,
                            fd,
                            payload,
                            buf_ptr,
                            len,
                            Some(total),
                            Some(failure),
                            ctx,
                        );
                    }
                    return SyscallResult::error_from(failure.errno);
                }
            }
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            if errno == tx_subsystems::execution::Errno::EIO {
                report_pipe_read_eio::<P>(
                    "pipe-drive",
                    lane,
                    fd,
                    payload,
                    buf_ptr,
                    len,
                    None,
                    None,
                    ctx,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

fn report_pipe_read_eio<P: tx_hal::ConsoleIf>(
    stage: &str,
    lane: &str,
    fd: i32,
    payload: &Cap<tx_subsystems::pipe::PipePayload>,
    buf_ptr: usize,
    len: usize,
    copied: Option<usize>,
    copy_failure: Option<super::user_copy::UserCopyWaitFailure>,
    ctx: &SyscallCtx<'_>,
) {
    use tx_subsystems::vm::{UserPage, UserVirtAddr, USER_PAGE_SIZE};

    let pipe = payload.diagnostic_snapshot();
    let pmap_stats = ctx.aspace.pmap().stats();
    let range_locks = ctx.aspace.range_lock().diagnostic_snapshot();
    let comm = ctx.process.comm();
    let comm_len = comm
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(comm.len());
    let comm = core::str::from_utf8(&comm[..comm_len]).unwrap_or("<non-utf8>");
    tx_hal::console_write_str::<P>(&alloc::format!(
        "txkernel:pipe-read-eio:stage={stage}:copy_failure={copy_failure:?}:lane={lane}:pid={}:tid={}:comm={comm}:fd={fd}:buf={buf_ptr:#x}:len={len}:copied={}:aspace={}:pipe_raw={:#x}:readers={}:writers={}:bytes={}:slots={}/{}:gift_slots={}:tmp_pages={}:write_avail={}:read_source={:#x}:write_source={:#x}:pmap_mapped={}:pmap_reservations={}:pmap_commits={}:pmap_rollbacks={}:pmap_shootdowns={}:range_active={}:range_pending={}:range_source={:#x}\n",
        ctx.process.pid.0,
        ctx.thread.tid.0,
        copied.map(|value| value as i64).unwrap_or(-1),
        ctx.aspace.futex_identity(),
        payload.raw(),
        pipe.readers,
        pipe.writers,
        pipe.buffered_bytes,
        pipe.occupied_slots,
        pipe.max_slots,
        pipe.reserved_gift_slots,
        pipe.temporary_pages,
        pipe.available_write_bytes,
        pipe.reader_wait_source_id,
        pipe.writer_wait_source_id,
        pmap_stats.mapped_pages,
        pmap_stats.reservations,
        pmap_stats.commits,
        pmap_stats.rollbacks,
        pmap_stats.shootdowns,
        range_locks.active,
        range_locks.pending_writers,
        range_locks.wait_source_id,
    ));

    if len == 0 {
        return;
    }
    let first_page_addr = buf_ptr & !(USER_PAGE_SIZE - 1);
    let end_addr = buf_ptr.saturating_add(len.saturating_sub(1));
    let last_page_addr = end_addr & !(USER_PAGE_SIZE - 1);
    let mut page_addr = first_page_addr;
    let mut emitted = 0usize;
    while page_addr <= last_page_addr && emitted < 4 {
        let recipe = ctx.aspace.lookup(UserVirtAddr(page_addr));
        let mapping = ctx
            .aspace
            .pmap()
            .lookup(UserPage(page_addr / USER_PAGE_SIZE));
        tx_hal::console_write_str::<P>(&alloc::format!(
            "txkernel:pipe-read-eio:vm-page={page_addr:#x}:recipe={recipe:?}:pmap={mapping:?}\n"
        ));
        emitted += 1;
        let Some(next) = page_addr.checked_add(USER_PAGE_SIZE) else {
            break;
        };
        page_addr = next;
    }
    if page_addr <= last_page_addr {
        tx_hal::console_write_str::<P>(&alloc::format!(
            "txkernel:pipe-read-eio:vm-pages-truncated:first={first_page_addr:#x}:last={last_page_addr:#x}:shown={emitted}\n"
        ));
    }
}

pub(super) async fn sys_write<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;
    emit_debug_counter(b"debug.write.enter", fd as i64);
    emit_debug_counter(b"debug.write.len", len as i64);

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }

    // Resolve fd → Cap<OpenFile> against the process payload's stub
    // fd table. Holding the payload guard across the lookup is fine —
    // the resulting Cap is independent and the lock is released before
    // any `.await`.
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // POSIX mq descriptors are not byte-stream fds. musl uses the
    // mq_* syscalls directly, but raw read/write on an mqd_t should
    // fail cleanly instead of falling into the VFS rnode path.
    if file.posix_mq().is_some() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // eventfd fds carry their own `write(2)` arm — add a 64-bit
    // value to the counter. Dispatch before any VFS-shaped rnode()
    // access because eventfd is a non-VFS OpenFile backing.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_write(&file, args[1], len, ctx).await;
    }
    // Netlink `write(fd, msg)` == `sendto(fd, msg, 0, NULL, 0)`: route it to the
    // netlink send path. The generic socket FileOps write (step_send_kernel_bytes)
    // has no netlink handling and blocks forever waiting for send readiness that
    // never fires — iproute2/busybox `ip` emit their RTM_GET* dump requests via
    // write(), so this hung `ip` (and thus all v6/v4 config) entirely.
    if let Ok(socket) = super::socket::socket_identity_from_file(&file) {
        if super::socket::is_netlink_socket_kind(socket.kind) {
            if !file.flags().write {
                return SyscallResult::Error(EBADF_VALUE);
            }
            let copy_len = core::cmp::min(len, SOCKET_IO_MAX_INLINE);
            let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; copy_len];
            if let Err(errno) = super::user_copy::bootstrap_copy_from_user_wait(
                &ctx.aspace,
                &mut bytes,
                buf_ptr as u64,
            )
            .await
            {
                return SyscallResult::error_from(errno);
            }
            return match super::socket::dispatch_netlink_send(ctx, &socket, &bytes) {
                Ok(sent) => SyscallResult::Return(sent as i64),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            };
        }
    }
    // P3-S3 (D13): sockets no longer detour to sys_sendto here —
    // `write(fd)` flows the generic VFS path into the socket FileOps arm
    // (`step_write` → `FileOps::write` ≡ `send(...,0)`), blocking via the
    // same v3 drive() loop as pipes. Loopback transfer progress rides the
    // delegate kick inside `step_send_kernel_bytes`.
    if let Some((_rx, tx)) = file.socketpair_endpoint() {
        if !file.flags().write {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
        if len == 0 {
            return SyscallResult::Return(0);
        }
        let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
        if let Err(errno) =
            super::user_copy::bootstrap_copy_from_user_wait(&ctx.aspace, &mut bytes, buf_ptr as u64)
                .await
        {
            return SyscallResult::error_from(errno);
        }
        return sys_pipe_write_buffered(&tx, &bytes, file.flags().nonblocking, false, ctx).await;
    }

    // POSIX: write(2) on a descriptor not opened for writing fails with
    // EBADF (the FMODE_WRITE check). Covers read-only regular files, the
    // read end of a pipe/FIFO, and read-only char/tty/null devices.
    // Special fds with their own write semantics (eventfd, socket,
    // socketpair, POSIX mq) are dispatched above and are opened RW.
    if !file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else if super::socket::socket_identity_from_file(&file).is_ok() {
        // Sockets are datagram/stream fds, not the TTY line discipline: a
        // UDP `read`/`write` must move the whole datagram in one call.
        // Truncating to `TTY_WRITE_MAX_INLINE` (4 KiB) shreds any datagram
        // >4 KiB into fragments (corrupting iperf3 UDP `-l 65495`); use the
        // socket I/O size (64 KiB), matching the sendto/recvfrom staging cap.
        core::cmp::min(len, SOCKET_IO_MAX_INLINE)
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

    if static_char_device(&file) == Some(StaticCharDevice::Null) {
        if !file.flags().write {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        return SyscallResult::Return(len as i64);
    }

    if let Some((pipe, tx_subsystems::pipe::PipeSide::Writer)) = file.pipe_endpoint() {
        if len == 0 {
            return SyscallResult::Return(0);
        }
        let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
        if let Err(errno) =
            super::user_copy::bootstrap_copy_from_user_wait(&ctx.aspace, &mut bytes, buf_ptr as u64)
                .await
        {
            return SyscallResult::error_from(errno);
        }
        return sys_pipe_write_buffered(
            &pipe,
            &bytes,
            file.flags().nonblocking,
            file.flags().packet,
            ctx,
        )
        .await;
    }

    // PageBacked files: direct user-buffer path (PAGE_BACKED_v1 §5.1).
    // Prefault the user buffer in the observe phase, then drive the
    // write through the pmap without kernel-buffer staging.
    if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        return sys_write_pagebacked(&file, buf_ptr, len, ctx).await;
    }

    // Pull the user buffer into kernel memory through the canonical
    // user-VA lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`, falling back to the kernel-pointer
    // deref the trio's earlier exemption used). The Vec is owned for
    // the duration of the step loop so the underlying user pages
    // can be re-mapped without affecting the byte stream we feed to
    // `step_write`.
    let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    if len > 0 {
        if let Err(errno) =
            super::user_copy::bootstrap_copy_from_user_wait(&ctx.aspace, &mut bytes, buf_ptr as u64)
                .await
        {
            return SyscallResult::error_from(errno);
        }
    }

    sys_write_buffered(&file, &bytes, ctx).await
}

async fn sys_write_buffered<'a>(
    file: &Cap<OpenFile>,
    bytes: &[u8],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    // The op acquires its own epoch guard inside `step()` per
    // STEP_MODEL_v2 §1; the syscall handler must not hold a guard
    // across `drive(...).await` (INVARIANTS_v5 YIELD-5 / EBR-7).
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileWriteOp {
        file,
        bytes,
        caller_netns: ctx.process.net_namespace(),
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(total) => SyscallResult::Return(total as i64),
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            // fd-ops Wave 3 — Q2 DECIDED 2026-05-07. SIGPIPE is
            // delivered to the calling process before returning
            // `-EPIPE` to userspace.
            if errno == tx_subsystems::execution::Errno::EPIPE {
                let _ = tx_subsystems::signal::step_kill_process(
                    &ctx.process,
                    tx_subsystems::signal::Signum::SIGPIPE,
                    None,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

fn emit_debug_counter(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

/// PageBacked `read(2)` — direct user-buffer path.
///
/// Prefaults the user buffer (Write access, since we're writing into
/// it), then drives `OpenFileReadToUserOp` which copies bytes directly
/// from PC frames to user pages through the pmap, without kernel-buffer
/// staging (PAGE_BACKED_v1 §5.1 / §3 prefault discipline).
async fn sys_read_pagebacked<'a, P: tx_hal::TimeIf>(
    file: &Cap<tx_subsystems::vfs::structure::OpenFile>,
    buf_ptr: usize,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if len == 0 {
        return SyscallResult::Return(0);
    }

    // Prefault: eagerly materialise every user page and publish to
    // the pmap so the step loop below finds every page in the cache.
    // Access is Write — we're writing data *into* the user buffer.
    if let Some(range) = super::user_copy::covering_user_range(buf_ptr as u64, len) {
        use tx_subsystems::vm::UserAccessKind;
        match ctx
            .aspace
            .reserve_user_range_for_access_wait(range, UserAccessKind::Write)
            .await
        {
            Ok(()) => {}
            Err(errno) => {
                return SyscallResult::error_from(errno);
            }
        }
    }

    // Drive the read-to-user step loop.
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileReadToUserOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileReadToUserOp {
        file,
        aspace: &ctx.aspace,
        dst: tx_hal::UserPtr::<u8>::new(buf_ptr),
        len,
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(total) => SyscallResult::Return(total as i64),
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            SyscallResult::error_from(errno)
        }
    }
}

/// `read(fd, buf, count)`.
///
/// Mirrors `sys_write`'s structure. For PageBacked files, delegates to
/// `sys_read_pagebacked` (direct user-buffer path, PAGE_BACKED_v1 §5.1);
/// for struct-backed files, uses kernel-buffer staging + copy_to_user.
pub(super) async fn sys_read<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Give queued loopback work one bounded chance before a bootstrap read.
    // The generic driver below uses Nonblocking mode when there is no mailbox,
    // so immediate FileOps results (including semantic errors) still win over
    // EAGAIN while a genuine Yield cannot park forever.
    if super::socket::socket_identity_from_file(&file).is_ok() && ctx.mailbox.is_none() {
        super::socket::drive_loopback_pending();
    }

    // POSIX mq descriptors are not byte-stream fds. musl uses the
    // mq_* syscalls directly, but raw read/write on an mqd_t should
    // fail cleanly instead of falling into the VFS rnode path.
    if file.posix_mq().is_some() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // PR-10 phase 5: userfaultfd fds carry their own `read(2)` arm
    // (drain a fault message off the pending queue, serialize 32-byte
    // `struct uffd_msg`). The VFS-shaped `OpenFile::step_read` returns
    // EINVAL for ufd backings, so dispatch here before any VFS-shaped
    // rnode() access.
    if file.ufd().is_some() {
        return super::userfaultfd::sys_ufd_read(&file, args[1], len, ctx).await;
    }
    // D9-D: signalfd fds carry their own `read(2)` arm (drain one
    // 128-byte `struct signalfd_siginfo` off the per-fd pending
    // queue). The VFS-shaped `OpenFile::rnode()` panics for the
    // signalfd backing, so this dispatch must run *before* any
    // generic VFS path.
    if file.signalfd().is_some() {
        return super::signalfd::sys_signalfd_read(&file, args[1], len, ctx).await;
    }
    // eventfd fds carry their own `read(2)` arm — drain the 64-bit
    // counter. Mirrors the ufd / signalfd dispatch shape.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_read(&file, args[1], len, ctx).await;
    }
    // timerfd fds carry their own `read(2)` arm — return the
    // expiration count as an 8-byte u64.
    if file.timerfd().is_some() {
        return super::timerfd::sys_timerfd_read::<P>(&file, args[1], len, ctx).await;
    }
    // P3-S3 (D13): sockets no longer detour to sys_recvfrom here —
    // `read(fd)` flows the generic VFS path into the socket FileOps arm
    // (`step_read` → `FileOps::read` ≡ `recv(...,0)`). The old
    // mailbox-less poll-gate is preserved because a bootstrap context
    // cannot block in the drive() loop.
    if super::socket::socket_identity_from_file(&file).is_ok() && ctx.mailbox.is_none() {
        let guard = crate::adapter::step_engine::guard();
        if let Some(Ok(mask)) = super::socket::socket_poll_mask_from_file(&file, &guard) {
            let readable = mask.intersects(
                tx_subsystems::net::PollMask::IN
                    | tx_subsystems::net::PollMask::ERR
                    | tx_subsystems::net::PollMask::HUP
                    | tx_subsystems::net::PollMask::RDHUP,
            );
            if !readable {
                return SyscallResult::Error(EAGAIN_VALUE);
            }
        }
    }
    if let Some((rx, _tx)) = file.socketpair_endpoint() {
        if !file.flags().read {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
        if len == 0 {
            return SyscallResult::Return(0);
        }
        return sys_pipe_read_buffered::<P>(
            &rx,
            fd,
            "socketpair",
            buf_ptr,
            len,
            file.flags().nonblocking,
            ctx,
        )
        .await;
    }

    // POSIX: read(2) on a descriptor not opened for reading fails with
    // EBADF (the FMODE_READ check). Covers write-only regular files, the
    // write end of a pipe/FIFO, and write-only char/tty devices. Special
    // fds with their own read semantics (eventfd, socket, socketpair,
    // POSIX mq) are dispatched above and are opened RW.
    if !file.flags().read {
        return SyscallResult::Error(EBADF_VALUE);
    }

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else if super::socket::socket_identity_from_file(&file).is_ok() {
        // Sockets are datagram/stream fds, not the TTY line discipline: a
        // UDP `read`/`write` must move the whole datagram in one call.
        // Truncating to `TTY_WRITE_MAX_INLINE` (4 KiB) shreds any datagram
        // >4 KiB into fragments (corrupting iperf3 UDP `-l 65495`); use the
        // socket I/O size (64 KiB), matching the sendto/recvfrom staging cap.
        core::cmp::min(len, SOCKET_IO_MAX_INLINE)
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }

    if static_char_device(&file) == Some(StaticCharDevice::Zero) {
        if !file.flags().read {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if let Err(errno) =
            bootstrap_copy_to_user(&ctx.aspace, buf_ptr as u64, &STATIC_ZERO_READ_BUF[..len])
        {
            return SyscallResult::error_from(errno);
        }
        return SyscallResult::Return(len as i64);
    }

    if let Some((pipe, tx_subsystems::pipe::PipeSide::Reader)) = file.pipe_endpoint() {
        return sys_pipe_read_buffered::<P>(
            &pipe,
            fd,
            "pipe",
            buf_ptr,
            len,
            file.flags().nonblocking,
            ctx,
        )
        .await;
    }

    // PageBacked files: direct user-buffer path (PAGE_BACKED_v1 §5.1).
    // Prefault the user buffer in the observe phase, then drive the
    // read through the pmap without kernel-buffer staging.
    if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        return sys_read_pagebacked::<P>(&file, buf_ptr, len, ctx).await;
    }

    // Non-canonical VMIN=0/VTIME>0 ("timed read", POSIX termios):
    // return 0 after VTIME deciseconds when no byte arrives. The TTY
    // step op itself has no deadline concept — it parks on the
    // readable wait source indefinitely — so the timeout is imposed
    // here by racing the drive() future against a timer, the same
    // pattern sys_pselect6 uses. Without this, vim's terminal-response
    // probes (write `ESC[6n`, timed-read the reply, give up on
    // timeout) hang forever on consoles that never answer.
    let vtime_deadline_ns: Option<u64> = if file.flags().nonblocking {
        None
    } else if let tx_subsystems::vfs::RNodeBacking::StructBacked {
        payload: tx_subsystems::vfs::StructPayload::Tty(tty),
    } = file.rnode().backing()
    {
        use tx_subsystems::tty::structure::termios::{ICANON, VMIN, VTIME};
        let termios = {
            let guard = crate::adapter::step_engine::guard();
            match tx_subsystems::tty::execution::step_ioctl_tcgets(tty, &guard) {
                tx_subsystems::execution::StepOutcome::Done(t) => Some(t),
                _ => None,
            }
        };
        termios.and_then(|t| {
            if t.c_lflag & ICANON == 0 && t.c_cc[VMIN] == 0 && t.c_cc[VTIME] != 0 {
                let vtime_ns = (t.c_cc[VTIME] as u64).saturating_mul(100_000_000);
                Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(vtime_ns))
            } else {
                None
            }
        })
    } else {
        None
    };

    // Read into a kernel-side staging buffer, then copy out through
    // the canonical user-VA lane (`bootstrap_copy_to_user` bridges
    // via `aspace.copy_to_user`, falling back to the kernel-pointer
    // dance the trio's earlier exemption used).
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];

    // Phase A.3: drive `OpenFile::step_read` via the v3 `drive()` loop
    // (per `docs/Txv3/03_STEP_MODEL_v2.md` §8). The internal cursor
    // in `OpenFileReadOp` tracks the fill position across successive
    // `step()` calls; after drive() returns, copy the accumulated
    // bytes to userspace in a single pass.

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileReadOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    // The op acquires its own epoch guard inside `step()` (STEP_MODEL_v2
    // §1, INVARIANTS_v5 YIELD-5 / EBR-7); no guard crosses `.await`.
    let mailbox_arc = script_ctx.mailbox().cloned();
    let mode = if file.flags().nonblocking || mailbox_arc.is_none() {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileReadOp {
        file: &file,
        out: &mut staging,
        caller_netns: ctx.process.net_namespace(),
        cursor: 0,
    };
    let drive_fut = drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    );
    // VTIME race: whichever completes first wins. A `None` from the
    // race means the timer expired with no byte — POSIX says return 0.
    let driven = if let Some(deadline_ns) = vtime_deadline_ns {
        let timer = super::deadline_timer(ctx, deadline_ns);
        match timer {
            Some(mut timer_fut) => {
                let mut drive_fut = core::pin::pin!(drive_fut);
                let raced = core::future::poll_fn(|cx| {
                    if let core::task::Poll::Ready(r) =
                        core::future::Future::poll(drive_fut.as_mut(), cx)
                    {
                        return core::task::Poll::Ready(Some(r));
                    }
                    if core::future::Future::poll(core::pin::Pin::new(&mut timer_fut), cx)
                        .is_ready()
                    {
                        return core::task::Poll::Ready(None);
                    }
                    core::task::Poll::Pending
                })
                .await;
                match raced {
                    Some(result) => result,
                    None => return SyscallResult::Return(0),
                }
            }
            None => drive_fut.await,
        }
    } else {
        drive_fut.await
    };
    match driven {
        Ok(total) => {
            if total > 0 {
                if let Err(errno) = super::user_copy::bootstrap_copy_to_user_wait(
                    &ctx.aspace,
                    buf_ptr as u64,
                    &staging[..total],
                )
                .await
                {
                    if errno == tx_subsystems::execution::Errno::EIO {
                        report_async_read_eio::<P>("copy-to-user", fd, buf_ptr, len);
                    }
                    return SyscallResult::error_from(errno);
                }
            }
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            if errno == tx_subsystems::execution::Errno::EIO {
                report_async_read_eio::<P>("file-op-drive", fd, buf_ptr, len);
            }
            SyscallResult::error_from(errno)
        }
    }
}

fn report_async_read_eio<P: tx_hal::ConsoleIf>(stage: &str, fd: i32, buf_ptr: usize, len: usize) {
    tx_hal::console_write_str::<P>(&alloc::format!(
        "txkernel:read-eio:stage={stage}:fd={fd}:buf={buf_ptr:#x}:len={len}\n"
    ));
}

/// `pread64(fd, buf, count, offset)`.
///
/// Minimal implementation for seekable in-kernel files: temporarily
/// snapshots the shared `OpenFile` offset, delegates to the existing
/// `read(2)` path, then restores the original offset so callers do
/// not observe a positioned read as a seek.
pub(super) async fn sys_pread64<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = args[3];
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, false) {
        return SyscallResult::Error(errno);
    }
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_read::<P>(args, ctx).await;
    file.set_offset(saved);
    result
}

pub(super) async fn sys_pwrite64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = args[3];
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, true) {
        return SyscallResult::Error(errno);
    }
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_write(args, ctx).await;
    file.set_offset(saved);
    result
}

fn positioned_io_check(file: &OpenFile, write: bool) -> Result<(), i32> {
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return Err(errno_to_i32(Errno::ESPIPE));
    };
    if matches!(
        rnode.backing(),
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { .. },
        } | RNodeBacking::StructBacked {
            payload: StructPayload::Tty(_),
        }
    ) {
        return Err(errno_to_i32(Errno::ESPIPE));
    }

    let flags = file.flags();
    if write {
        if !flags.write {
            return Err(EBADF_VALUE);
        }
    } else if !flags.read {
        return Err(EBADF_VALUE);
    }

    Ok(())
}

fn read_iovec_entry(ctx: &SyscallCtx<'_>, iov_ptr: u64, idx: u64) -> Result<(u64, u64), Errno> {
    const IOVEC_BYTES: u64 = 16;

    let ent_ptr = iov_ptr.wrapping_add(idx * IOVEC_BYTES);
    let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr)?;
    let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
    Ok((base, len))
}

pub(super) async fn sys_preadv<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    let mut offset = args[3];

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let (base, len) = match read_iovec_entry(ctx, iov_ptr, i) {
            Ok(entry) => entry,
            Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::error_from(errno);
            }
        };
        if len == 0 {
            continue;
        }
        if len > MAX_RW_COUNT {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }

        let read_args = [args[0], base, len, offset, 0, 0];
        match sys_pread64::<P>(read_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                let n = n as u64;
                offset = match offset.checked_add(n) {
                    Some(value) => value,
                    None => return SyscallResult::Error(EINVAL_VALUE),
                };
                if n < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

pub(super) async fn sys_pwritev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    let mut offset = args[3];

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let (base, len) = match read_iovec_entry(ctx, iov_ptr, i) {
            Ok(entry) => entry,
            Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::error_from(errno);
            }
        };
        if len == 0 {
            continue;
        }
        if len > MAX_RW_COUNT {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }

        let write_args = [args[0], base, len, offset, 0, 0];
        match sys_pwrite64(write_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                let n = n as u64;
                offset = match offset.checked_add(n) {
                    Some(value) => value,
                    None => return SyscallResult::Error(EINVAL_VALUE),
                };
                if n < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

fn preadv2_offset(args: [u64; 6]) -> Result<Option<u64>, i32> {
    let lo = args[3] as i64;
    let hi = args[4];
    if lo == -1 {
        return Ok(None);
    }
    if lo < 0 || hi != 0 {
        return Err(EINVAL_VALUE);
    }
    Ok(Some(args[3]))
}

pub(super) async fn sys_preadv2<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let flags = args[5] as i32;
    if flags != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    match preadv2_offset(args) {
        Ok(Some(offset)) => {
            let preadv_args = [args[0], args[1], args[2], offset, 0, 0];
            sys_preadv::<P>(preadv_args, ctx).await
        }
        Ok(None) => {
            let readv_args = [args[0], args[1], args[2], 0, 0, 0];
            sys_readv::<P>(readv_args, ctx).await
        }
        Err(errno) => SyscallResult::Error(errno),
    }
}

pub(super) async fn sys_pwritev2<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[5] as i32;
    if flags != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    match preadv2_offset(args) {
        Ok(Some(offset)) => {
            let pwritev_args = [args[0], args[1], args[2], offset, 0, 0];
            sys_pwritev(pwritev_args, ctx).await
        }
        Ok(None) => {
            let writev_args = [args[0], args[1], args[2], 0, 0, 0];
            sys_writev(writev_args, ctx).await
        }
        Err(errno) => SyscallResult::Error(errno),
    }
}

pub(super) fn sys_fadvise64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1];
    let len = args[2];
    let advice = args[3] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if (offset as i64) < 0 || (len as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !(0..=5).contains(&advice) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, false) {
        return SyscallResult::Error(errno);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_readahead<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    // readahead(2): EBADF if the descriptor is not open for reading
    // (e.g. an `O_PATH` or write-only fd).
    if !file.flags().read {
        return SyscallResult::Error(EBADF_VALUE);
    }
    // readahead(2): EINVAL unless the fd refers to a regular file (page
    // cache backing). Directories, symlinks, FIFOs/pipes, sockets, char
    // devices, and the anonymous fd kinds (eventfd, pidfd, …) all reject.
    let regular = match file.backing() {
        tx_subsystems::vfs::structure::OpenFileBacking::Rnode { rnode } => {
            matches!(
                rnode.backing(),
                tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
            )
        }
        _ => false,
    };
    if !regular {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_sync_file_range<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1] as i64;
    let nbytes = args[2] as i64;
    let flags = args[3] as u32;
    const SYNC_FILE_RANGE_KNOWN: u32 = 0x1 | 0x2 | 0x4;
    if fd < 0 || resolve_fd(&ctx.process, fd as u32).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 || nbytes < 0 || flags & !SYNC_FILE_RANGE_KNOWN != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
}

/// `sendfile64(out_fd, in_fd, offset, count)` — page-level copy from
/// one fd to another without an intermediate userspace buffer.
///
/// Linux generic ABI `__NR_sendfile64 = 71`.  Copies up to `count`
/// bytes from `in_fd` (must be a page-backed regular file) to
/// `out_fd`.  If `offset` is non-NULL, reads the input position from
/// `*offset` and writes the updated position back; the input file's
/// fd-level cursor is not touched in this case.  If `offset` is NULL,
/// the input file's fd cursor is used and advanced.
pub(super) async fn sys_sendfile64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    use tx_subsystems::page_backed::step_copy_file_range;
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let out_fd = args[0] as i32;
    let in_fd = args[1] as i32;
    let offset_ptr = args[2];
    let count = args[3] as usize;

    if out_fd < 0 || in_fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if count == 0 {
        return SyscallResult::Return(0);
    }

    let out_file = match resolve_fd(&ctx.process, out_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_file = match resolve_fd(&ctx.process, in_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if !in_file.flags().read || !out_file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }

    // Input must be a regular file backed by PageContainer.
    let in_rnode = match in_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let in_pc = match in_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Output must be page-backed (regular file); pipes/sockets are
    // deferred per PAGE_BACKED §9.1.
    let out_rnode = match out_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_pc = match out_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Determine input offset.
    let in_offset: u64;
    let needs_offset_writeback: bool;
    if offset_ptr != 0 {
        // Read the offset from userspace.
        let mut off_bytes = [0u8; 8];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut off_bytes, offset_ptr) {
            return SyscallResult::error_from(errno);
        }
        let signed_offset = i64::from_le_bytes(off_bytes);
        if signed_offset < 0 {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        in_offset = signed_offset as u64;
        needs_offset_writeback = true;
    } else {
        // Use the input file's current cursor.
        in_offset = in_file.offset();
        needs_offset_writeback = false;
    }

    // Output writes at the output file's current cursor.
    let out_offset = out_file.offset();

    // Single-shot page copy (non-blocking for v1).
    let guard = tx_substrate::epoch::guard();
    let outcome = step_copy_file_range(&in_pc, in_offset, &out_pc, out_offset, count, &guard);
    drop(guard);

    let transferred = match outcome {
        tx_substrate::step::StepOutcome::Done(n) => n,
        tx_substrate::step::StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e.into();
            return SyscallResult::error_from(errno);
        }
        // If the operation would block, return EAGAIN (sendfile is
        // non-blocking in this v1 implementation).
        _ => return SyscallResult::Error(EAGAIN_VALUE),
    };

    if transferred == 0 {
        return SyscallResult::Return(0);
    }

    // Advance the output file's cursor.
    out_file.advance_offset(transferred as u64);

    // Update the input offset in userspace if a pointer was given.
    if needs_offset_writeback {
        let new_off = in_offset + transferred as u64;
        let bytes = new_off.to_le_bytes();
        if let Err(_errno) = bootstrap_copy_to_user(&ctx.aspace, offset_ptr, &bytes) {
            // On partial success, Linux prefers to return the byte
            // count rather than the fault error.
            return SyscallResult::Return(transferred as i64);
        }
    } else {
        // Advance the input file's cursor when offset was NULL.
        in_file.advance_offset(transferred as u64);
    }

    SyscallResult::Return(transferred as i64)
}

pub(super) async fn sys_copy_file_range<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    use tx_subsystems::page_backed::step_copy_file_range;
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let in_fd = args[0] as i32;
    let off_in_ptr = args[1];
    let out_fd = args[2] as i32;
    let off_out_ptr = args[3];
    let len = args[4] as usize;
    let flags = args[5] as u32;

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if in_fd < 0 || out_fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }

    let in_file = match resolve_fd(&ctx.process, in_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, out_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if !in_file.flags().read || !out_file.flags().write || out_file.flags().append {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let in_rnode = match in_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_rnode = match out_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let in_pc = match in_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_pc = match out_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    let in_offset = if off_in_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_in_ptr) {
            Ok(offset) => offset,
            Err(errno) => return SyscallResult::error_from(errno),
        }
    } else {
        in_file.offset()
    };
    let out_offset = if off_out_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_out_ptr) {
            Ok(offset) => offset,
            Err(errno) => return SyscallResult::error_from(errno),
        }
    } else {
        out_file.offset()
    };

    let guard = tx_substrate::epoch::guard();
    let outcome = step_copy_file_range(&in_pc, in_offset, &out_pc, out_offset, len, &guard);
    drop(guard);

    let transferred = match outcome {
        tx_substrate::step::StepOutcome::Done(n) => n,
        tx_substrate::step::StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e.into();
            return SyscallResult::error_from(errno);
        }
        _ => return SyscallResult::Error(EAGAIN_VALUE),
    };

    if transferred > 0 {
        let new_in = in_offset + transferred as u64;
        let new_out = out_offset + transferred as u64;
        if off_in_ptr != 0 {
            if let Err(_errno) = bootstrap_write_user::<u64>(&ctx.aspace, off_in_ptr, new_in) {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            in_file.advance_offset(transferred as u64);
        }
        if off_out_ptr != 0 {
            if let Err(_errno) = bootstrap_write_user::<u64>(&ctx.aspace, off_out_ptr, new_out) {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            out_file.advance_offset(transferred as u64);
        }
    }

    SyscallResult::Return(transferred as i64)
}
