//! `epoll_create1(2)`, `epoll_ctl(2)`, `epoll_wait(2)` — Phase B.1c.
//!
//! Wires the [`crate::linux_syscall::mod`] dispatch table to the
//! [`crate::epoll`] subsystem.

use alloc::sync::Arc;

use crate::adapter::reactor_entry::WaitSource;
use crate::adapter::step_engine::{InterestMask, StepOutcome, WaitSourceId};
use crate::linux_syscall::{
    bootstrap_copy_from_user, bootstrap_copy_to_user, errno_to_i32, SyscallCtx, SyscallResult,
    EBADF_VALUE, EFAULT_VALUE, EINVAL_VALUE, EMFILE_VALUE, ENOMEM_VALUE, ENOSYS_VALUE,
};
use tx_services::time::{timekeeper_clock, ClockRead, TimekeeperClock};
use tx_subsystems::{
    epoll,
    vfs::{
        query_fd_ready,
        structure::{OpenFile, OpenFileBacking, OpenFileFlags, RNodeBacking, StructPayload},
        FdReadyMask, FdReadyQuery, FdReadyReport,
    },
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
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;

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

fn epoll_to_fd_ready_mask(interests: u32) -> FdReadyMask {
    let mut mask = FdReadyMask::empty();
    if (interests & EPOLLIN) != 0 {
        mask |= FdReadyMask::READ;
    }
    if (interests & EPOLLOUT) != 0 {
        mask |= FdReadyMask::WRITE;
    }
    if (interests & EPOLLERR) != 0 {
        mask |= FdReadyMask::ERR;
    }
    if (interests & EPOLLHUP) != 0 {
        mask |= FdReadyMask::HUP;
    }
    mask
}

fn fd_ready_report_for_epoll(
    file: &tx_subsystems::adapter::step_engine::Cap<OpenFile>,
    interests: u32,
    now_monotonic_ns: Option<u64>,
) -> FdReadyReport {
    let guard = crate::adapter::step_engine::guard();
    query_fd_ready(FdReadyQuery {
        file,
        interest: epoll_to_fd_ready_mask(interests),
        now_monotonic_ns,
    }, &guard)
}

fn rtc_wait_source(
    file: &tx_subsystems::adapter::step_engine::Cap<OpenFile>,
    interests: u32,
) -> Option<WaitSourceId> {
    if (interests & EPOLLIN) == 0 {
        return None;
    }
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::CharDevice(binding),
            } if binding.ops.rtc_ops().is_some() => {
                Some(WaitSourceId::new(tx_fs::devfs::rtc_event_source_id()))
            }
            _ => None,
        },
        _ => None,
    }
}

fn rtc_ready_mask(
    file: &tx_subsystems::adapter::step_engine::Cap<OpenFile>,
    interests: u32,
) -> u32 {
    if (interests & EPOLLIN) == 0 {
        return 0;
    }
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::CharDevice(binding),
            } => {
                let Some(rtc_ops) = binding.ops.rtc_ops() else {
                    return 0;
                };
                let guard = crate::adapter::step_engine::guard();
                if rtc_ops
                    .poll_events(&guard)
                    .is_ok_and(|mask| !mask.is_empty())
                {
                    EPOLLIN
                } else {
                    0
                }
            }
            _ => 0,
        },
        _ => 0,
    }
}

fn epoll_wait_source(
    file: &tx_subsystems::adapter::step_engine::Cap<OpenFile>,
    interests: u32,
) -> (WaitSourceId, Option<Arc<WaitSource>>) {
    let report = fd_ready_report_for_epoll(file, interests, None);
    let source = report.primary_wait_source();
    if source.raw() != 0 {
        return (source, report.primary_endpoint());
    }
    (
        rtc_wait_source(file, interests).unwrap_or_else(|| WaitSourceId::new(0)),
        None,
    )
}

fn supports_epoll(file: &tx_subsystems::adapter::step_engine::Cap<OpenFile>) -> bool {
    fd_ready_report_for_epoll(file, EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP, None).epoll_watchable
        || rtc_wait_source(file, EPOLLIN).is_some()
}

fn ready_mask_for_entry<P>(
    entry: &epoll::EpollEntry,
    target: &tx_subsystems::adapter::step_engine::Cap<OpenFile>,
) -> u32
where
    TimekeeperClock<P>: ClockRead,
{
    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
    let ready = fd_ready_report_for_epoll(target, entry.interests, Some(now_ns))
        .ready
        .bits();
    ready | rtc_ready_mask(target, entry.interests)
}

fn collect_ready_events<P>(
    ctx: &SyscallCtx<'_>,
    ep_cap: &tx_subsystems::adapter::step_engine::Cap<epoll::Epoll>,
    maxevents: usize,
) -> alloc::vec::Vec<UserEpollEvent>
where
    TimekeeperClock<P>: ClockRead,
{
    let entries = ep_cap.entries_snapshot();
    let mut ready = alloc::vec::Vec::new();
    for entry in entries.iter() {
        if ready.len() >= maxevents {
            break;
        }
        let Some(target) = super::resolve_fd(&ctx.process, entry.fd) else {
            continue;
        };
        let ready_mask = ready_mask_for_entry::<P>(entry, &target);
        let deliver = if entry.disabled || ready_mask == 0 {
            false
        } else if (entry.interests & EPOLLET) != 0 {
            (ready_mask & !entry.last_ready) != 0
        } else {
            true
        };
        let disable_after_delivery = deliver && (entry.interests & EPOLLONESHOT) != 0;
        let _ = epoll::step_epoll_note_ready(ep_cap, entry.fd, ready_mask, disable_after_delivery);
        if deliver {
            ready.push(UserEpollEvent {
                events: ready_mask,
                data: entry.data,
            });
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
) -> alloc::vec::Vec<(WaitSourceId, InterestMask, Option<Arc<WaitSource>>)> {
    let mut sources = alloc::vec::Vec::new();
    for entry in entries {
        if entry.source.raw() == 0 {
            continue;
        }
        if sources.iter().any(|(source, _, _)| *source == entry.source) {
            continue;
        }
        sources.push((
            entry.source,
            InterestMask::new(u64::MAX),
            entry.endpoint.clone(),
        ));
    }
    sources
}

async fn wait_for_epoll_wake(
    ctx: &SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask, Option<Arc<WaitSource>>)],
    deadline_ns: Option<u64>,
) -> bool {
    super::await_any_wait_source(ctx, sources, deadline_ns).await
}

fn epoll_timeout_ms_deadline<P>(timeout_ms: i32) -> Option<u64>
where
    TimekeeperClock<P>: ClockRead,
{
    if timeout_ms < 0 {
        return None;
    }
    let timeout_ns = (timeout_ms as u64).saturating_mul(1_000_000);
    Some(
        timekeeper_clock::<P>()
            .monotonic_now_ns()
            .saturating_add(timeout_ns),
    )
}

fn epoll_timeout_timespec_deadline<P>(
    timeout_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<Option<u64>, SyscallResult>
where
    TimekeeperClock<P>: ClockRead,
{
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let Some(timeout_ns) = super::time::read_timespec_at(&ctx.aspace, timeout_ptr) else {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    };
    Ok(Some(
        timekeeper_clock::<P>()
            .monotonic_now_ns()
            .saturating_add(timeout_ns),
    ))
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
    let Some(fd) = ctx.process.install_new_fd(of, cloexec) else {
        return SyscallResult::Error(EMFILE_VALUE);
    };

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

    let target_of = super::resolve_fd(&ctx.process, fd);
    if op != EPOLL_CTL_DEL {
        match target_of.as_ref() {
            Some(target) if supports_epoll(target) => {}
            Some(_) => {
                return SyscallResult::Error(errno_to_i32(tx_subsystems::execution::Errno::EPERM));
            }
            None => return SyscallResult::Error(EBADF_VALUE),
        }
    }

    // Call the appropriate step function.
    let ep_ref = &*ep_cap;
    let (source, endpoint) = target_of
        .as_ref()
        .map(|target| epoll_wait_source(target, event.events))
        .unwrap_or_else(|| (WaitSourceId::new(0), None));
    let target_epoll = if op != EPOLL_CTL_DEL {
        target_of
            .as_ref()
            .and_then(|target| target.epoll().cloned())
    } else {
        None
    };
    let outcome = match op {
        EPOLL_CTL_ADD => epoll::step_epoll_ctl_add(
            ep_ref,
            fd,
            event.events,
            event.data,
            source,
            endpoint,
            target_epoll,
        ),
        EPOLL_CTL_MOD => epoll::step_epoll_ctl_mod(
            ep_ref,
            fd,
            event.events,
            event.data,
            source,
            endpoint,
            target_epoll,
        ),
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

pub(super) async fn sys_epoll_wait<P>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    timeout: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let deadline_ns = epoll_timeout_ms_deadline::<P>(timeout);
    sys_epoll_wait_until::<P>(epfd, events_ptr, maxevents, deadline_ns, ctx).await
}

pub(super) async fn sys_epoll_pwait2<P>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    timeout_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let deadline_ns = match epoll_timeout_timespec_deadline::<P>(timeout_ptr, ctx) {
        Ok(deadline) => deadline,
        Err(result) => return result,
    };
    sys_epoll_wait_until::<P>(epfd, events_ptr, maxevents, deadline_ns, ctx).await
}

async fn sys_epoll_wait_until<P>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    deadline_ns: Option<u64>,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
        let ready = collect_ready_events::<P>(ctx, &ep_cap, maxevents as usize);
        if !ready.is_empty() {
            return copy_ready_events(ctx, events_ptr, &ready);
        }

        if let Some(deadline) = deadline_ns {
            if timekeeper_clock::<P>().monotonic_now_ns() >= deadline {
                return SyscallResult::Return(0);
            }
        }

        let entries = ep_cap.entries_snapshot();
        let sources = wait_sources_for_entries(&entries);
        if ctx.mailbox.is_none() || sources.is_empty() {
            return SyscallResult::Return(0);
        }
        if !wait_for_epoll_wake(ctx, &sources, deadline_ns).await {
            return SyscallResult::Return(0);
        }
    }
}
