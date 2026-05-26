//! `epoll_create1(2)`, `epoll_ctl(2)`, `epoll_wait(2)` — Phase B.1c.
//!
//! Wires the [`crate::linux_syscall::mod`] dispatch table to the
//! [`crate::epoll`] subsystem.

use crate::adapter::step_engine::{guard, InterestMask, StepOutcome, WaitSourceId};
use crate::linux_syscall::{
    bootstrap_copy_from_user, bootstrap_copy_to_user, SyscallCtx, SyscallResult, EBADF_VALUE,
    EFAULT_VALUE, EINVAL_VALUE, ENOMEM_VALUE, ENOSYS_VALUE, EPERM_VALUE,
};
use tx_hal::TimeIf;
use tx_subsystems::{
    epoll,
    pipe::PipeSide,
    vfs::structure::{OpenFile, OpenFileFlags, RNodeBacking, StructPayload},
};

// ---------------------------------------------------------------------------
// Linux constants
// ---------------------------------------------------------------------------

const EPOLL_CLOEXEC: u32 = 0o2000000;

// epoll_ctl operations
const EPOLL_CTL_ADD: u32 = 1;
const EPOLL_CTL_DEL: u32 = 2;
const EPOLL_CTL_MOD: u32 = 3;

/// Size of `struct epoll_event` on generic LP64 Linux targets.
///
/// musl only packs this struct on x86_64; RV64 and LoongArch64 use the
/// natural layout: `events` at offset 0, four bytes of padding, then
/// `epoll_data_t` at offset 8.
const EPOLL_EVENT_SIZE: usize = 16;

/// Max events returned in one `epoll_wait` call.
const MAX_EVENTS: usize = 1024;

const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLL_MAX_NEST_DEPTH: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UserEpollEvent {
    events: u32,
    data: u64,
}

impl UserEpollEvent {
    fn to_bytes(self) -> [u8; EPOLL_EVENT_SIZE] {
        let mut bytes = [0u8; EPOLL_EVENT_SIZE];
        bytes[0..4].copy_from_slice(&self.events.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.data.to_le_bytes());
        bytes
    }
}

fn read_user_epoll_event(
    ctx: &SyscallCtx<'_>,
    event_ptr: u64,
) -> Result<UserEpollEvent, SyscallResult> {
    let mut bytes = [0u8; EPOLL_EVENT_SIZE];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, event_ptr)
        .map_err(SyscallResult::error_from)?;
    Ok(UserEpollEvent {
        events: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        data: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
    })
}

fn epoll_wait_source(file: &OpenFile, interests: u32) -> Option<WaitSourceId> {
    if let Some(efd) = file.eventfd() {
        let source = if (interests & EPOLLIN) != 0 {
            efd.reader_source_id()
        } else if (interests & EPOLLOUT) != 0 {
            efd.writer_source_id()
        } else {
            0
        };
        return Some(WaitSourceId::new(source));
    }

    if let Some(tfd) = file.timerfd() {
        let source = if (interests & EPOLLIN) != 0 {
            tfd.source_id()
        } else {
            0
        };
        return Some(WaitSourceId::new(source));
    }

    if let Some(sfd) = file.signalfd() {
        let source = if (interests & EPOLLIN) != 0 {
            sfd.wait_source_id()
        } else {
            0
        };
        return Some(WaitSourceId::new(source));
    }

    if let Some(ufd) = file.ufd() {
        let source = if (interests & EPOLLIN) != 0 {
            ufd.wait_source_id()
        } else {
            0
        };
        return Some(WaitSourceId::new(source));
    }

    if let Some(mq) = file.posix_mq() {
        let source = match tx_subsystems::ipc::posix_mq::execution::step_mq_poll_info(mq) {
            Ok(info) if (interests & EPOLLIN) != 0 => info.read_source_id,
            Ok(info) if (interests & EPOLLOUT) != 0 => info.write_source_id,
            _ => 0,
        };
        return Some(WaitSourceId::new(source));
    }

    if let Some(ep) = file.epoll() {
        return Some(ep.wait_source_id());
    }

    if let tx_subsystems::vfs::structure::OpenFileBacking::Rnode { rnode } = file.backing() {
        match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, side },
            } => {
                let source = match (*side, interests & (EPOLLIN | EPOLLOUT)) {
                    (PipeSide::Reader, mask) if (mask & EPOLLIN) != 0 => payload.reader_source_id(),
                    (PipeSide::Writer, mask) if (mask & EPOLLOUT) != 0 => {
                        payload.writer_source_id()
                    }
                    _ => 0,
                };
                return Some(WaitSourceId::new(source));
            }
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            } => {
                let source = if (interests & EPOLLIN) != 0 {
                    tty.wait_source_id()
                } else {
                    0
                };
                return Some(WaitSourceId::new(source));
            }
            _ => {}
        }
    }

    None
}

fn epoll_nested_depth(
    ctx: &SyscallCtx<'_>,
    ep: &epoll::Epoll,
    seen: &mut alloc::vec::Vec<u64>,
) -> Option<usize> {
    if seen.contains(&ep.epoll_id()) {
        return None;
    }
    seen.push(ep.epoll_id());
    let mut max_child_depth = 0usize;
    for entry in ep.entries_snapshot() {
        let Some(of) = super::resolve_fd(&ctx.process, entry.fd) else {
            continue;
        };
        let Some(child) = of.epoll() else {
            continue;
        };
        let child_depth = epoll_nested_depth(ctx, child, seen)?.saturating_add(1);
        max_child_depth = max_child_depth.max(child_depth);
    }
    let _ = seen.pop();
    Some(max_child_depth)
}

fn epoll_add_would_exceed_depth(
    ctx: &SyscallCtx<'_>,
    ep: &epoll::Epoll,
    target: &epoll::Epoll,
) -> bool {
    let mut seen = alloc::vec![ep.epoll_id()];
    let Some(target_depth) = epoll_nested_depth(ctx, target, &mut seen) else {
        return true;
    };
    target_depth.saturating_add(1) >= EPOLL_MAX_NEST_DEPTH
}

fn ready_events_for_entry<P: TimeIf>(
    entry: &epoll::EpollEntry,
    target: &OpenFile,
) -> Option<UserEpollEvent> {
    let mut ready = 0u32;

    if let Some(efd) = target.eventfd() {
        if (entry.interests & EPOLLIN) != 0 && efd.counter() > 0 {
            ready |= EPOLLIN;
        }
        if (entry.interests & EPOLLOUT) != 0 && efd.counter() < tx_subsystems::eventfd::EVENTFD_MAX
        {
            ready |= EPOLLOUT;
        }
    } else if let Some(tfd) = target.timerfd() {
        if (entry.interests & EPOLLIN) != 0 && tfd.deadline_ns() != 0 {
            let now_ns = P::read_ns();
            if tfd.expiration_count() > 0 || tfd.remaining_value_ns(now_ns) == 0 {
                ready |= EPOLLIN;
            }
        }
    } else if let Some(sfd) = target.signalfd() {
        if (entry.interests & EPOLLIN) != 0 && sfd.pending_count() > 0 {
            ready |= EPOLLIN;
        }
    } else if let Some(ufd) = target.ufd() {
        if (entry.interests & EPOLLIN) != 0 && ufd.pending_fault_count() > 0 {
            ready |= EPOLLIN;
        }
    } else if let Some(mq) = target.posix_mq() {
        if let Ok(info) = tx_subsystems::ipc::posix_mq::execution::step_mq_poll_info(mq) {
            if (entry.interests & EPOLLIN) != 0 && info.readable {
                ready |= EPOLLIN;
            }
            if (entry.interests & EPOLLOUT) != 0 && info.writable {
                ready |= EPOLLOUT;
            }
        }
    }

    (ready != 0).then_some(UserEpollEvent {
        events: ready,
        data: entry.data,
    })
}

fn collect_ready_events<P: TimeIf>(
    ctx: &SyscallCtx<'_>,
    entries: &[epoll::EpollEntry],
    maxevents: usize,
) -> alloc::vec::Vec<UserEpollEvent> {
    let mut ready = alloc::vec::Vec::new();
    for entry in entries.iter() {
        if ready.len() >= maxevents {
            break;
        }
        let Some(target) = super::resolve_fd(&ctx.process, entry.fd) else {
            continue;
        };
        if let Some(event) = ready_events_for_entry::<P>(entry, &target) {
            ready.push(event);
        }
    }
    ready
}

fn copy_ready_events(
    ctx: &SyscallCtx<'_>,
    events_ptr: u64,
    ready: &[UserEpollEvent],
) -> SyscallResult {
    let mut bytes = alloc::vec![0u8; ready.len() * EPOLL_EVENT_SIZE];
    for (index, event) in ready.iter().enumerate() {
        let start = index * EPOLL_EVENT_SIZE;
        bytes[start..start + EPOLL_EVENT_SIZE].copy_from_slice(&event.to_bytes());
    }
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, events_ptr, &bytes) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(ready.len() as i64)
}

fn wait_sources_for_entries(
    entries: &[epoll::EpollEntry],
) -> alloc::vec::Vec<(WaitSourceId, InterestMask)> {
    let mut sources = alloc::vec::Vec::new();
    for entry in entries {
        if entry.source.raw() == 0 {
            continue;
        }
        if sources.iter().any(|(source, _)| *source == entry.source) {
            continue;
        }
        sources.push((entry.source, InterestMask::new(u64::MAX)));
    }
    sources
}

async fn wait_for_epoll_wake<P: TimeIf>(
    ctx: &SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask)],
    deadline_ns: Option<u64>,
) -> bool {
    let source_future = super::await_any_wait_source(ctx, sources);
    let mut source_future = core::pin::pin!(source_future);
    let process_timer_deadline = ctx.process.next_process_timer_deadline_ns();
    let Some((wake_deadline_ns, wake_reason)) =
        earliest_epoll_wake_deadline(deadline_ns, process_timer_deadline)
    else {
        return source_future.as_mut().await;
    };
    let Some(timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(wake_deadline_ns) else {
        return false;
    };
    let mut timer_future = core::pin::pin!(timer_future);

    use core::future::{poll_fn, Future};
    use core::task::Poll;

    poll_fn(|cx| {
        if source_future.as_mut().poll(cx).is_ready() {
            Poll::Ready(true)
        } else if timer_future.as_mut().poll(cx).is_ready() {
            if wake_reason == EpollWakeDeadlineReason::ProcessTimer {
                let now_ns = P::read_ns().max(wake_deadline_ns);
                super::time::poll_expired_process_timers_at(ctx, now_ns);
            }
            Poll::Ready(false)
        } else {
            Poll::Pending
        }
    })
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EpollWakeDeadlineReason {
    Timeout,
    ProcessTimer,
}

fn earliest_epoll_wake_deadline(
    timeout_deadline: Option<u64>,
    process_timer_deadline: Option<u64>,
) -> Option<(u64, EpollWakeDeadlineReason)> {
    match (timeout_deadline, process_timer_deadline) {
        (Some(timeout), Some(timer)) if timer <= timeout => {
            Some((timer, EpollWakeDeadlineReason::ProcessTimer))
        }
        (Some(timeout), _) => Some((timeout, EpollWakeDeadlineReason::Timeout)),
        (None, Some(timer)) => Some((timer, EpollWakeDeadlineReason::ProcessTimer)),
        (None, None) => None,
    }
}

fn epoll_timeout_ms_deadline<P: TimeIf>(timeout_ms: i32) -> Option<u64> {
    if timeout_ms < 0 {
        return None;
    }
    let timeout_ns = (timeout_ms as u64).saturating_mul(1_000_000);
    Some(P::read_ns().saturating_add(timeout_ns))
}

fn epoll_timeout_timespec_deadline<P: TimeIf>(
    timeout_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<Option<u64>, SyscallResult> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let Some(timeout_ns) = super::time::read_timespec_at(&ctx.aspace, timeout_ptr) else {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    };
    Ok(Some(P::read_ns().saturating_add(timeout_ns)))
}

// ---------------------------------------------------------------------------
// sys_epoll_create1
// ---------------------------------------------------------------------------

pub(super) fn sys_epoll_create1(flags: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let recognised = EPOLL_CLOEXEC;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let cloexec = (flags & EPOLL_CLOEXEC) != 0;

    // Allocate a fresh Epoll identity.
    let ep = match tx_subsystems::adapter::step_engine::sign(epoll::Epoll::new()) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Wrap in an OpenFile.
    let open_file_flags = OpenFileFlags {
        read: true,
        write: false,
        nonblocking: false,
        append: false,
        ..Default::default()
    };
    let of = match OpenFile::new_epoll_cap(ep, open_file_flags) {
        Ok(of) => of,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Install as an fd in the calling process.
    let fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(fd, of);
    if cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

// ---------------------------------------------------------------------------
// sys_epoll_ctl
// ---------------------------------------------------------------------------

pub(super) fn sys_epoll_ctl(
    epfd: u32,
    op: u32,
    fd: u32,
    event_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    // Validate op.
    if op != EPOLL_CTL_ADD && op != EPOLL_CTL_DEL && op != EPOLL_CTL_MOD {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Read epoll_event from userspace (ADD and MOD).
    let event = if op != EPOLL_CTL_DEL {
        if event_ptr == 0 {
            return SyscallResult::Error(EFAULT_VALUE);
        }
        match read_user_epoll_event(ctx, event_ptr) {
            Ok(event) => event,
            Err(result) => return result,
        }
    } else {
        UserEpollEvent { events: 0, data: 0 }
    };

    // Resolve epfd → Cap<OpenFile> → Epoll.
    let ep_of = match super::resolve_fd(&ctx.process, epfd) {
        Some(of) => of,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let ep_cap = match ep_of.epoll() {
        Some(cap) => cap.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE), // not an epoll fd
    };

    if epfd == fd {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let target_of = match super::resolve_fd(&ctx.process, fd) {
        Some(of) => of,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    let source = if op == EPOLL_CTL_DEL {
        WaitSourceId::new(0)
    } else {
        match epoll_wait_source(&target_of, event.events) {
            Some(source) => source,
            None => return SyscallResult::Error(EPERM_VALUE),
        }
    };
    if op == EPOLL_CTL_ADD {
        if let Some(target_ep) = target_of.epoll() {
            if epoll_add_would_exceed_depth(ctx, &ep_cap, target_ep) {
                return SyscallResult::Error(EINVAL_VALUE);
            }
        }
    }

    // Call the appropriate step function.
    let _guard = guard();
    let ep_ref = &*ep_cap;
    let outcome = match op {
        EPOLL_CTL_ADD => epoll::step_epoll_ctl_add(ep_ref, fd, event.events, event.data, source),
        EPOLL_CTL_MOD => epoll::step_epoll_ctl_mod(ep_ref, fd, event.events, event.data, source),
        EPOLL_CTL_DEL => epoll::step_epoll_ctl_del(ep_ref, fd),
        _ => unreachable!(),
    };

    match outcome {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(e) => SyscallResult::error_from(e.into()),
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

// ---------------------------------------------------------------------------
// sys_epoll_wait
// ---------------------------------------------------------------------------

pub(super) async fn sys_epoll_wait<P: TimeIf>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    timeout: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let deadline_ns = epoll_timeout_ms_deadline::<P>(timeout);
    sys_epoll_wait_until::<P>(epfd, events_ptr, maxevents, deadline_ns, ctx).await
}

pub(super) async fn sys_epoll_pwait2<P: TimeIf>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    timeout_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let deadline_ns = match epoll_timeout_timespec_deadline::<P>(timeout_ptr, ctx) {
        Ok(deadline) => deadline,
        Err(result) => return result,
    };
    sys_epoll_wait_until::<P>(epfd, events_ptr, maxevents, deadline_ns, ctx).await
}

async fn sys_epoll_wait_until<P: TimeIf>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    deadline_ns: Option<u64>,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if maxevents == 0 || maxevents as usize > MAX_EVENTS {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if events_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Resolve epfd → Cap<OpenFile> → Epoll.
    let ep_of = match super::resolve_fd(&ctx.process, epfd) {
        Some(of) => of,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let ep_cap = match ep_of.epoll() {
        Some(cap) => cap.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    loop {
        let entries = ep_cap.entries_snapshot();
        let ready = collect_ready_events::<P>(ctx, &entries, maxevents as usize);
        if !ready.is_empty() {
            return copy_ready_events(ctx, events_ptr, &ready);
        }

        if let Some(deadline) = deadline_ns {
            if P::read_ns() >= deadline {
                return SyscallResult::Return(0);
            }
        }

        let sources = wait_sources_for_entries(&entries);
        if ctx.mailbox.is_none() || sources.is_empty() {
            return SyscallResult::Return(0);
        }
        if !wait_for_epoll_wake::<P>(ctx, &sources, deadline_ns).await {
            return SyscallResult::Return(0);
        }
    }
}
