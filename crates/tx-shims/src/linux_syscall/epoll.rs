//! `epoll_create1(2)`, `epoll_ctl(2)`, `epoll_wait(2)` — Phase B.1c.
//!
//! Wires the [`crate::linux_syscall::mod`] dispatch table to the
//! [`crate::epoll`] subsystem.

use crate::adapter::step_engine::{guard, StepOutcome, WaitSourceId};
use crate::linux_syscall::{
    bootstrap_copy_from_user, bootstrap_copy_to_user, SyscallCtx, SyscallResult, EBADF_VALUE,
    EFAULT_VALUE, EINVAL_VALUE, ENOMEM_VALUE, ENOSYS_VALUE,
};
use tx_hal::TimeIf;
use tx_subsystems::{
    epoll,
    vfs::structure::{OpenFile, OpenFileFlags},
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

fn epoll_wait_source(file: &OpenFile, interests: u32) -> WaitSourceId {
    if let Some(efd) = file.eventfd() {
        let source = if (interests & EPOLLIN) != 0 {
            efd.reader_source_id()
        } else if (interests & EPOLLOUT) != 0 {
            efd.writer_source_id()
        } else {
            0
        };
        return WaitSourceId::new(source);
    }

    if let Some(tfd) = file.timerfd() {
        let source = if (interests & EPOLLIN) != 0 {
            tfd.source_id()
        } else {
            0
        };
        return WaitSourceId::new(source);
    }

    if let Some(sfd) = file.signalfd() {
        let source = if (interests & EPOLLIN) != 0 {
            sfd.wait_source_id()
        } else {
            0
        };
        return WaitSourceId::new(source);
    }

    if let Some(ufd) = file.ufd() {
        let source = if (interests & EPOLLIN) != 0 {
            ufd.wait_source_id()
        } else {
            0
        };
        return WaitSourceId::new(source);
    }

    WaitSourceId::new(0)
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
    }

    (ready != 0).then_some(UserEpollEvent {
        events: ready,
        data: entry.data,
    })
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
    let fd = match super::next_stdio_fd_below_nofile(&ctx.process) {
        Ok(fd) => fd,
        Err(result) => return result,
    };
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

    // Call the appropriate step function.
    let _guard = guard();
    let ep_ref = &*ep_cap;
    let source = epoll_wait_source(&target_of, event.events);
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

pub(super) fn sys_epoll_wait<P: TimeIf>(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    timeout: i32,
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

    let entries = ep_cap.entries_snapshot();
    let mut ready = alloc::vec::Vec::new();
    for entry in entries.iter() {
        if ready.len() >= maxevents as usize {
            break;
        }
        let Some(target) = super::resolve_fd(&ctx.process, entry.fd) else {
            continue;
        };
        if let Some(event) = ready_events_for_entry::<P>(entry, &target) {
            ready.push(event);
        }
    }

    if !ready.is_empty() {
        let mut bytes = alloc::vec![0u8; ready.len() * EPOLL_EVENT_SIZE];
        for (index, event) in ready.iter().enumerate() {
            let start = index * EPOLL_EVENT_SIZE;
            bytes[start..start + EPOLL_EVENT_SIZE].copy_from_slice(&event.to_bytes());
        }
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, events_ptr, &bytes) {
            return SyscallResult::error_from(errno);
        }
        return SyscallResult::Return(ready.len() as i64);
    }

    if timeout == 0 {
        return SyscallResult::Return(0);
    }

    let _guard = guard();
    let ep_ref = &*ep_cap;
    let outcome = epoll::step_epoll_wait(ep_ref, &_guard);

    match outcome {
        StepOutcome::Done(_) => SyscallResult::Error(ENOSYS_VALUE),
        StepOutcome::Err(e) => SyscallResult::error_from(e.into()),
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}
