//! `epoll_create1(2)`, `epoll_ctl(2)`, `epoll_wait(2)` — Phase B.1c.
//!
//! Wires the [`crate::linux_syscall::mod`] dispatch table to the
//! [`crate::epoll`] subsystem.

use crate::linux_syscall::{
    bootstrap_read_user, bootstrap_write_user, errno_to_i32, EBADF_VALUE, EFAULT_VALUE,
    EINVAL_VALUE, ENOENT_VALUE, ENOMEM_VALUE, ENOSYS_VALUE, SyscallCtx, SyscallResult,
};
use tx_subsystems::{
    epoll,
    execution::{self, Guard, StepOp},
    vfs::{
        self,
        structure::{OpenFile, OpenFileBacking, OpenFileFlags},
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

/// Size of `struct epoll_event` — two u64s: events (u32) + data (u64).
const EPOLL_EVENT_SIZE: usize = 12;

/// Max events returned in one `epoll_wait` call.
const MAX_EVENTS: usize = 1024;

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
    let fd = match tx_subsystems::process::execution::install_fd_cap(
        &ctx.process,
        of,
        cloexec,
        None, // no specific target fd — kernel picks
    ) {
        Ok(fd) => fd,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
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
    let interests: u32 = if op != EPOLL_CTL_DEL {
        if event_ptr == 0 {
            return SyscallResult::Error(EFAULT_VALUE);
        }
        // epoll_event.events is the first 4 bytes.
        match bootstrap_read_user::<u32>(&ctx.aspace, event_ptr) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        0
    };

    // Resolve epfd → Cap<OpenFile> → Epoll.
    let ep_of = match vfs::resolve_fd(&ctx.process, epfd) {
        Some(of) => of,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let ep_cap = match ep_of.epoll() {
        Some(cap) => cap.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE), // not an epoll fd
    };

    // Call the appropriate step function.
    let _guard = execution::guard();
    let ep_ref = ep_cap.as_ref();
    // Phase B.2: resolve the monitored fd's WaitSourceId.
    // Placeholder — Phase B.3 will look up the fd's bus wire.
    let source = tx_subsystems::adapter::step_engine::WaitSourceId::new(0);
    let outcome = match op {
        EPOLL_CTL_ADD | EPOLL_CTL_MOD => {
            epoll::step_epoll_ctl_add(ep_ref, fd, interests, source)
        }
        EPOLL_CTL_DEL => epoll::step_epoll_ctl_del(ep_ref, fd),
        _ => unreachable!(),
    };

    match outcome {
        execution::StepOutcome::Done(()) => SyscallResult::Return(0),
        execution::StepOutcome::Err(e) => SyscallResult::Error(errno_to_i32(e.into())),
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

// ---------------------------------------------------------------------------
// sys_epoll_wait
// ---------------------------------------------------------------------------

pub(super) fn sys_epoll_wait(
    epfd: u32,
    events_ptr: u64,
    maxevents: u32,
    _timeout: u32, // ignored in PoC — always blocks indefinitely
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if events_ptr == 0 || maxevents == 0 || maxevents as usize > MAX_EVENTS {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Resolve epfd → Cap<OpenFile> → Epoll.
    let ep_of = match vfs::resolve_fd(&ctx.process, epfd) {
        Some(of) => of,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let ep_cap = match ep_of.epoll() {
        Some(cap) => cap.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // PoC: call step_epoll_wait and convert to Linux format.
    let _guard = execution::guard();
    let ep_ref = ep_cap.as_ref();
    let outcome = epoll::step_epoll_wait(ep_ref, &_guard);

    match outcome {
        execution::StepOutcome::Done(count) => {
            // Write `count` epoll_event entries (events=0, data=0)
            // back to userspace. Real implementation fills actual
            // event data.
            let nbytes = count * EPOLL_EVENT_SIZE;
            if nbytes > 0 {
                // Zero out the userspace buffer.
                let zeros = alloc::vec![0u8; nbytes];
                if let Err(errno) = bootstrap_write_user(
                    &ctx.aspace,
                    events_ptr,
                    &zeros,
                ) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
            }
            SyscallResult::Return(count as i64)
        }
        execution::StepOutcome::Err(e) => SyscallResult::Error(errno_to_i32(e.into())),
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}
