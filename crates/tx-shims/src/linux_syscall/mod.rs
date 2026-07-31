//! Linux syscall dispatch table — Phase 2a + 2b slice.
//!
//! Phase 2a deliverable per the Trio plan
//! (`docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md`
//! §"Phasing" item 2): `NR_WRITE`, `NR_EXIT`, `NR_EXIT_GROUP`,
//! `NR_GETPID`. Phase 2b (§"Phasing" item 4) extends with `NR_READ`,
//! `NR_BRK`, `NR_RT_SIGPROCMASK`, `NR_RT_SIGACTION`. Everything else
//! still returns `-ENOSYS`.
//!
//! ## Plan B writeback discipline
//!
//! Per the plan's "Cross-cutting risks #1" the dispatcher only writes
//! the syscall return into `ThreadPayload.pending_syscall_return`; the
//! userspace-entry shim drains the slot and writes it into the *fresh*
//! trap frame before `enter_userspace`. The `dispatch` function itself
//! returns a `SyscallResult` so the trap-shell-side wrapper (Phase 6
//! territory) can decide between writing to the payload slot
//! (`Return`/`Error`) and never re-entering userspace (`NoReturn`).
//!
//! ## Cap vs IdentRef
//!
//! Per `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` and the plan's
//! "Cross-cutting risks #2", every step that needs an epoch guard
//! takes a fresh `step_engine::guard()` *inside* the call
//! site. Guards never cross `.await`; `Cap<T>` does (it's
//! epoch-managed). This mirrors `vm::execution::fault_script`.
//!
//! ## Doc anchors
//!
//! - `txdoc:PROCESS-WHAT-THIS-DOCUMENT-PINS-1`,
//!   `txdoc:PROCESS-RELATIONSHIP-OTHER-SUBSYSTEMS-1`,
//!   `txdoc:PROCESS-STEP-THREAD-EXIT-1` (`PROCESS_v1` §7.3.1, §8.4)
//!   — exit ordering and the `step_thread_exit` → `step_process_exit`
//!   chain.
//! - `txdoc:THREAD-5-1-STATE-PLACEMENT` (`THREAD_RUNTIME_v1`) —
//!   per-thread state placement that this dispatcher consumes.
//! - `txdoc:VFS-CHECKS-WALKER-MODES-1` (`VFS_CHECKS_V2.1`) —
//!   `OpenFile::step_write` semantics.

#![allow(clippy::module_inception)]

extern crate alloc;

use crate::adapter::reactor_entry;
use alloc::sync::Arc;
use alloc::vec::Vec;

use reactor_entry::userspace::SyscallRequest;
use tx_hal::{AuxvIf, EntropyIf, IpiKind, PmapIf, SmpIf};
use tx_observe::SpanId;
use tx_scripts::process::exec::ExecScriptOp;
use tx_subsystems::cred::{
    Capability, CapabilitySet, CredChange, Gid, SetgidOp, SetregidOp, SetresgidOp, SetresuidOp,
    SetreuidOp, SetuidOp, Uid,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{
    AnonSwapPolicy, PageContainer, PageContainerKind, TruncateOp as FdTruncateOp,
};
use tx_subsystems::process::{
    process_by_pid, seed_child_leader_context, step_waitpid_nohang, ChdirOp, ChdirOutcome, CloseOp,
    Dup3Op, DupOp, ExitStatus, FcntlDupFdOp, FcntlFdOp, GetcwdOp, Pgid, Pid, ProcessIdentity,
    SetpgidOp, SetsidOp, WaitError, WaitTarget,
};
use tx_subsystems::reactor_submit;
use tx_subsystems::signal::{
    KillProcessWithPostOp, SaFlags, SigActionEntry, SigDisposition, SigDispositionChange,
    SigactionOp, SignalMask, Signum,
};
use tx_subsystems::thread_runtime::execution::{
    step_sigprocmask, step_sigprocmask_with_payload, SigmaskHow, SigprocmaskChange, SigprocmaskOp,
};
use tx_subsystems::thread_runtime::{
    step_thread_exit, ThreadExitOp, ThreadIdentity, ThreadKillWithPostOp, ThreadPayload,
};
use tx_subsystems::tty::execution::{
    step_ioctl_tcgets, step_ioctl_tcsets, step_ioctl_tiocgpgrp, step_ioctl_tiocgwinsz,
    step_ioctl_tiocnotty, step_ioctl_tiocsctty_for_process, step_ioctl_tiocspgrp,
    step_ioctl_tiocswinsz, IoctlCaller,
};
use tx_subsystems::tty::structure::{Termios, Winsize};
use tx_subsystems::vfs::composite::{
    AccessOp, ChmodOp, ChownOp, LstatOp, LstatxOp, MknodOp, NanosleepOp, RenameOp, StatOp, StatxOp,
    StatxResult,
};
use tx_subsystems::vfs::structure::{
    Credential, FsNotifyInstance, FsNotifyKind, InodeKind, InodeMeta, OpenFileBacking,
    OpenFileFlags, RNodeBacking, StructPayload, S_ISGID,
};
use tx_subsystems::vfs::{
    step_open, step_open_nofollow, step_walk, DEntry, FileFsyncOp, FlockOp, OpenFile,
    OpenFileGetFlOp, OpenFileSetFlOp, OpenNoFollowOp, OpenOp,
};
use tx_subsystems::vm::{
    AddressSpace, MadviseAdvice, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking,
    VmEntryFlags, VmMapError, VmMapRequest, VmRemapRequest, FULL_USER_V1_TOP, USER_PAGE_SIZE,
};
use tx_subsystems::wait_source;

use tx_services::time::{ClockRead, RealtimeControl, TimekeeperClock};

pub mod numbers;

mod cred;
use cred::*;
mod time;
pub use time::poll_due_itimers_with_post;
use time::*;
mod signal;
use signal::*;
mod ipc;
use ipc::*;
mod vm;
use vm::*;
pub mod io;
use io::*;
mod socket;
use socket::*;
pub mod fs_basic;
use fs_basic::*;
mod fs_path;
use fs_path::*;
mod fs_mounted;
use fs_mounted::*;
mod fs_resolve;
use fs_resolve::*;
mod fs_mut;
use fs_mut::*;
mod fs_handle;
use fs_handle::*;
pub mod proc;
use proc::*;
mod misc;
use misc::*;
mod userfaultfd;
use userfaultfd::*;
// `clone_op` is an unfinished StepOp-shaped refactor targeting an older
// API surface. `sys_execve` now drives the canonical
// `tx_scripts::process::exec::ExecScriptOp`.
// pub mod clone_op;
pub mod aio;
use aio::*;
pub mod io_uring;
use io_uring::*;
mod signalfd;
use crate::adapter::step_engine::{self as step_engine, Cap, PayloadCap};
use signalfd::*;
mod eventfd;
use eventfd::*;
mod timerfd;
use timerfd::*;
mod posix_timer;
pub use posix_timer::poll_due_posix_timers_with_post;
use posix_timer::*;
mod epoll;
use epoll::*;
mod kernel_object;
use kernel_object::*;
mod event_notify;
use event_notify::*;
mod splice;
use splice::*;

mod ctx;
pub use ctx::*;
mod result;
pub use result::*;
mod user_copy;
pub(super) use user_copy::*;
mod user_layout;
pub use user_layout::{
    kernel_user_layout_candidates, kernel_user_layouts, KernelToUserLayout, KernelUserCandidate,
    KernelUserField, KernelUserLayout,
};
mod helpers;
pub(super) use helpers::*;
mod wait;
pub(super) use wait::*;

pub use time::maybe_deliver_itimer_signal_with_post;

#[cfg(test)]
mod tests;

pub use numbers::{
    AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW,
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE, CLOCK_THREAD_CPUTIME_ID,
    CLONE_CHILD_CLEARTID, CLONE_CHILD_SETTID, CLONE_DETACHED, CLONE_FILES, CLONE_FS,
    CLONE_NEWCGROUP, CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWUSER, CLONE_NEWUTS, CLONE_PARENT,
    CLONE_PARENT_SETTID, CLONE_SETTLS, CLONE_SIGHAND, CLONE_SYSVSEM, CLONE_THREAD, CLONE_VFORK,
    CLONE_VM, CLOSE_RANGE_CLOEXEC, CLOSE_RANGE_UNSHARE, DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK,
    DT_REG, DT_SOCK, DT_UNKNOWN, FD_CLOEXEC, FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK,
    FUTEX_CMP_REQUEUE, FUTEX_LOCK_PI, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_TRYLOCK_PI,
    FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX_WAKE_OP,
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_GETLEASE, F_GETLK, F_GETPIPE_SZ, F_OFD_GETLK,
    F_OFD_SETLK, F_OFD_SETLKW, F_OK, F_SETFD, F_SETFL, F_SETLEASE, F_SETLK, F_SETLKW, F_SETPIPE_SZ,
    GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, MADV_DONTNEED, MADV_FREE, MADV_NORMAL, MADV_RANDOM,
    MADV_SEQUENTIAL, MADV_WILLNEED, MAP_ANONYMOUS, MAP_DENYWRITE, MAP_EXECUTABLE, MAP_FIXED,
    MAP_FIXED_NOREPLACE, MAP_GROWSDOWN, MAP_HUGETLB, MAP_LOCKED, MAP_NONBLOCK, MAP_NORESERVE,
    MAP_POPULATE, MAP_PRIVATE, MAP_SHARED, MAP_STACK, MAP_SYNC, MFD_ALLOW_SEALING, MFD_CLOEXEC,
    MFD_HUGETLB, MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE, NR_ACCEPT, NR_ACCEPT4, NR_BIND,
    NR_BPF, NR_BRK, NR_CHDIR, NR_CLOCK_GETRES, NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP,
    NR_CLOCK_SETTIME, NR_CLONE, NR_CLOSE, NR_CLOSE_RANGE, NR_CONNECT, NR_COPY_FILE_RANGE, NR_DUP,
    NR_DUP3, NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT, NR_EPOLL_PWAIT2, NR_EVENTFD2,
    NR_EXECVE, NR_EXIT, NR_EXIT_GROUP, NR_FACCESSAT, NR_FACCESSAT2, NR_FADVISE64_64, NR_FALLOCATE,
    NR_FANOTIFY_INIT, NR_FANOTIFY_MARK, NR_FCHDIR, NR_FCHMOD, NR_FCHMODAT, NR_FCHMODAT2, NR_FCHOWN,
    NR_FCHOWNAT, NR_FCNTL, NR_FDATASYNC, NR_FLOCK, NR_FSTAT, NR_FSTATFS, NR_FSYNC, NR_FTRUNCATE,
    NR_FUTEX, NR_GETCPU, NR_GETCWD, NR_GETDENTS64, NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETGROUPS,
    NR_GETITIMER, NR_GETPEERNAME, NR_GETPGID, NR_GETPID, NR_GETPPID, NR_GETPRIORITY, NR_GETRANDOM,
    NR_GETRESGID, NR_GETRESUID, NR_GETRLIMIT, NR_GETRUSAGE, NR_GETSID, NR_GETSOCKNAME,
    NR_GETSOCKOPT, NR_GETTID, NR_GETTIMEOFDAY, NR_GETUID, NR_GET_MEMPOLICY, NR_GET_ROBUST_LIST,
    NR_INOTIFY_ADD_WATCH, NR_INOTIFY_INIT1, NR_INOTIFY_RM_WATCH, NR_IOCTL, NR_IOPRIO_GET,
    NR_IOPRIO_SET, NR_IO_DESTROY, NR_IO_GETEVENTS, NR_IO_SETUP, NR_IO_SUBMIT, NR_IO_URING_ENTER,
    NR_IO_URING_SETUP, NR_KILL, NR_LINKAT, NR_LISTEN, NR_LSEEK, NR_MADVISE, NR_MEMFD_CREATE,
    NR_MEMFD_SECRET, NR_MKDIRAT, NR_MKNODAT, NR_MLOCK, NR_MMAP, NR_MOUNT, NR_MPROTECT,
    NR_MQ_GETSETATTR, NR_MQ_NOTIFY, NR_MQ_OPEN, NR_MQ_TIMEDRECEIVE, NR_MQ_TIMEDSEND, NR_MQ_UNLINK,
    NR_MREMAP, NR_MSGCTL, NR_MSGGET, NR_MSGRCV, NR_MSGSND, NR_MSYNC, NR_MUNLOCK, NR_MUNMAP,
    NR_NAME_TO_HANDLE_AT, NR_NANOSLEEP, NR_NEWFSTATAT, NR_OPENAT, NR_OPEN_BY_HANDLE_AT,
    NR_PERF_EVENT_OPEN, NR_PERSONALITY, NR_PIDFD_OPEN, NR_PIDFD_SEND_SIGNAL, NR_PIPE2, NR_PPOLL,
    NR_PRCTL, NR_PREAD64, NR_PREADV, NR_PREADV2, NR_PRLIMIT64, NR_PSELECT6, NR_PSELECT6_TIME64,
    NR_PWRITE64, NR_PWRITEV, NR_PWRITEV2, NR_READ, NR_READAHEAD, NR_READLINKAT, NR_READV,
    NR_RECVFROM, NR_RECVMMSG, NR_RECVMSG, NR_RENAMEAT2, NR_RESTART_SYSCALL, NR_RT_SIGACTION,
    NR_RT_SIGPENDING, NR_RT_SIGPROCMASK, NR_RT_SIGQUEUEINFO, NR_RT_SIGRETURN, NR_RT_SIGSUSPEND,
    NR_RT_SIGTIMEDWAIT, NR_SCHED_GETAFFINITY, NR_SCHED_GETATTR, NR_SCHED_GETPARAM,
    NR_SCHED_GETSCHEDULER, NR_SCHED_GET_PRIORITY_MAX, NR_SCHED_GET_PRIORITY_MIN,
    NR_SCHED_RR_GET_INTERVAL, NR_SCHED_SETAFFINITY, NR_SCHED_SETATTR, NR_SCHED_SETPARAM,
    NR_SCHED_SETSCHEDULER, NR_SCHED_YIELD, NR_SEMCTL, NR_SEMGET, NR_SEMOP, NR_SEMTIMEDOP,
    NR_SENDMMSG, NR_SENDMSG, NR_SENDTO, NR_SETGID, NR_SETITIMER, NR_SETNS, NR_SETPGID,
    NR_SETPRIORITY, NR_SETREGID, NR_SETRESGID, NR_SETRESUID, NR_SETREUID, NR_SETRLIMIT, NR_SETSID,
    NR_SETSOCKOPT, NR_SETTIMEOFDAY, NR_SETUID, NR_SET_ROBUST_LIST, NR_SET_TID_ADDRESS, NR_SHMAT,
    NR_SHMCTL, NR_SHMDT, NR_SHMGET, NR_SHUTDOWN, NR_SIGALTSTACK, NR_SIGNALFD4, NR_SOCKET,
    NR_SOCKETPAIR, NR_SPLICE, NR_STATFS, NR_STATX, NR_SYMLINKAT, NR_SYNC, NR_SYNCFS,
    NR_SYNC_FILE_RANGE, NR_SYSLOG, NR_TEE, NR_TGKILL, NR_TIMERFD_CREATE, NR_TIMERFD_GETTIME,
    NR_TIMERFD_SETTIME, NR_TIMER_CREATE, NR_TIMER_DELETE, NR_TIMER_GETOVERRUN, NR_TIMER_GETTIME,
    NR_TIMER_SETTIME, NR_TIMES, NR_TKILL, NR_TRUNCATE, NR_TX_OBSERVE_BEGIN,
    NR_TX_OBSERVE_TRACE_OFF, NR_TX_OBSERVE_TRACE_ON, NR_UMASK, NR_UMOUNT2, NR_UNAME, NR_UNLINKAT,
    NR_UNSHARE, NR_USERFAULTFD, NR_UTIMENSAT, NR_VMSPLICE, NR_WAIT4, NR_WRITE, NR_WRITEV,
    O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_EXCL, O_NOCTTY, O_NOFOLLOW,
    O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR, O_TMPFILE, O_TRUNC, O_WRONLY, PROT_EXEC, PROT_GROWSDOWN,
    PROT_GROWSUP, PROT_NONE, PROT_READ, PROT_WRITE, RENAME_EXCHANGE, RENAME_NOREPLACE,
    RENAME_WHITEOUT, RLIMIT_AS, RLIMIT_CORE, RLIMIT_CPU, RLIMIT_DATA, RLIMIT_FSIZE, RLIMIT_LOCKS,
    RLIMIT_MEMLOCK, RLIMIT_MSGQUEUE, RLIMIT_NICE, RLIMIT_NOFILE, RLIMIT_NPROC, RLIMIT_RSS,
    RLIMIT_RTPRIO, RLIMIT_RTTIME, RLIMIT_SIGPENDING, RLIMIT_STACK, RLIM_INFINITY, R_OK, SEEK_CUR,
    SEEK_END, SEEK_SET, SFD_CLOEXEC, SFD_NONBLOCK, SIGCHLD, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCGIFMTU,
    SIOCGIFTXQLEN, SIOCSIFFLAGS, SIOCSIFMTU, TCGETS, TCSETS, TCSETSF, TCSETSW,
    TFD_TIMER_ABSTIME_FLAG, TFD_TIMER_CANCEL_ON_SET_FLAG, TIMER_ABSTIME, TIMES_NS_PER_TICK,
    TIOCGPGRP, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP, TIOCSWINSZ, UTIME_NOW, UTIME_OMIT,
    WNOHANG, W_OK, X_OK, __O_TMPFILE,
};

pub use numbers::{
    MEMBARRIER_CMD_GLOBAL, MEMBARRIER_CMD_GLOBAL_EXPEDITED, MEMBARRIER_CMD_PRIVATE_EXPEDITED,
    MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, MEMBARRIER_CMD_QUERY,
    MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, MEMBARRIER_SUPPORTED_MASK, NR_MEMBARRIER,
    NR_SENDFILE64,
};

// Socket / network + non-net syscall constants
// (feature socket layer import; numbers already pulled above are omitted).
pub use numbers::{
    AF_INET, AF_INET6, AF_NETLINK, AF_PACKET, AF_UNIX, CLOCK_BOOTTIME_ALARM, CLOCK_REALTIME_ALARM,
    CLOCK_TAI, FSOPEN_CLOEXEC, FSPICK_CLOEXEC, FSPICK_EMPTY_PATH, FSPICK_NO_AUTOMOUNT,
    FSPICK_SYMLINK_NOFOLLOW, ICMP6_FILTER, IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_IP, IPPROTO_IPV6,
    IPPROTO_TCP, IPPROTO_UDP, IPPROTO_UDPLITE, IPT_SO_GET_ENTRIES, IPT_SO_GET_INFO,
    IPT_SO_SET_ADD_COUNTERS, IPT_SO_SET_REPLACE, IPV6_2292DSTOPTS, IPV6_2292HOPLIMIT,
    IPV6_2292HOPOPTS, IPV6_2292PKTINFO, IPV6_2292RTHDR, IPV6_ADDRFORM, IPV6_CHECKSUM,
    IPV6_HOPLIMIT, IPV6_PKTINFO, IPV6_RECVDSTOPTS, IPV6_RECVHOPLIMIT, IPV6_RECVHOPOPTS,
    IPV6_RECVPKTINFO, IPV6_RECVRTHDR, IPV6_RECVTCLASS, IPV6_TCLASS, IPV6_UNICAST_HOPS, IPV6_V6ONLY,
    IP_ADD_MEMBERSHIP, IP_DROP_MEMBERSHIP, IP_HDRINCL, IP_MULTICAST_IF, IP_MULTICAST_LOOP,
    IP_MULTICAST_TTL, IP_RECVERR, IP_TTL, ITIMER_REAL, MCAST_JOIN_GROUP, MCAST_LEAVE_GROUP,
    NETLINK_EXT_ACK, NETLINK_NETFILTER, NETLINK_ROUTE, NETLINK_XFRM, NR_CAPGET, NR_CAPSET,
    NR_FADVISE64, NR_FSOPEN, NR_FSPICK, NR_KCMP, NR_MINCORE, NR_MLOCK2, NR_MLOCKALL, NR_MUNLOCKALL,
    NR_OPEN_TREE, NR_PIDFD_GETFD, NR_REMAP_FILE_PAGES, NR_SETGROUPS, NR_SETHOSTNAME, NR_SIGNALFD,
    OPEN_TREE_CLOEXEC, OPEN_TREE_CLONE, PACKET_RESERVE, PACKET_RX_RING, PACKET_VERSION,
    PACKET_VNET_HDR, SCTP_ASSOCINFO, SCTP_AUTOCLOSE, SCTP_DEFAULT_SEND_PARAM,
    SCTP_DELAYED_ACK_TIME, SCTP_DISABLE_FRAGMENTS, SCTP_EVENTS, SCTP_GET_LOCAL_ADDRS,
    SCTP_GET_PEER_ADDRS, SCTP_GET_PEER_ADDR_INFO, SCTP_INITMSG, SCTP_MAXSEG, SCTP_PEER_ADDR_PARAMS,
    SCTP_PRIMARY_ADDR, SCTP_RTOINFO, SCTP_SOCKOPT_BINDX_ADD, SCTP_SOCKOPT_BINDX_REM,
    SCTP_SOCKOPT_CONNECTX3, SCTP_SOCKOPT_PEELOFF, SCTP_STATUS, SIOCADDRT, SIOCDARP, SIOCDELRT,
    SIOCGIFADDR, SIOCGIFBRDADDR, SIOCGIFCONF, SIOCGIFHWADDR, SIOCGIFNAME, SIOCGIFNETMASK, SIOCSARP,
    SIOCSIFADDR, SIOCSIFBRDADDR, SIOCSIFNETMASK, SOL_IPV6, SOL_NETLINK, SOL_PACKET, SOL_RAW,
    SOL_SCTP, SOL_SOCKET, SOL_TLS, SO_BINDTODEVICE, SO_BROADCAST, SO_DONTROUTE, SO_ERROR,
    SO_KEEPALIVE, SO_LINGER, SO_NO_CHECK, SO_OOBINLINE, SO_PEERCRED, SO_RCVBUF, SO_RCVTIMEO,
    SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF, SO_SNDBUFFORCE, SO_SNDTIMEO, SO_TYPE, TCP_CONGESTION,
    TCP_INFO, TCP_MAXSEG, TCP_NODELAY, TCP_ULP, TLS_TX, TPACKET_V1, TPACKET_V2, TPACKET_V3,
};
/// Maximum number of input bytes the Phase 2a `write` syscall accepts
/// in a single call. The dispatcher copies `[buf_ptr, buf_ptr+len)` into
/// a kernel-side stack-bounded slice (via `from_raw_parts`); higher-level
/// `copy_from_user` machinery is deferred per the trio plan §"Out of
/// scope". 4 KiB matches a single page; values above that should batch
/// across multiple write calls until the userspace-VA copy lane lands.
pub const TTY_WRITE_MAX_INLINE: usize = 4096;

/// Maximum bytes staged per inline socket `read(2)`/`write(2)` step (re-homed
/// from the pre-rebase net tree; the rebase dropped it with the socket I/O lane).
pub const SOCKET_IO_MAX_INLINE: usize = 64 * 1024;

/// Maximum path-name length accepted by `execve(2)` (Linux's
/// `PATH_MAX`). Mirrors the `TTY_WRITE_MAX_INLINE = 4096` discipline
/// for inline buffer copies. A longer path returns `-ENAMETOOLONG`
/// per the Phase 6 plan.
///
/// Could be lifted to platform `PATH_MAX` (typically 4096 across
/// Linux ABIs, so this is already at the canonical ceiling).
pub const EXECVE_PATH_MAX: usize = 4096;

/// Maximum total argv + envp byte budget per `execve(2)` call.
///
/// Linux's `ARG_MAX` is 128 KiB but the Phase 6 plan caps the inline
/// buffer at 8 KiB to keep the same discipline as the `write` /
/// `sigaction` arms. Overflow returns `-E2BIG`. Could be lifted to
/// 128 KiB now that the user-VA `copy_from_user` lane has landed.
pub const EXECVE_ARG_MAX_INLINE: usize = 8192;

/// Maximum number of pointer slots walked through `argv` / `envp`
/// before we give up. The Phase 6 plan caps at 256; in practice the
/// total-byte cap (`EXECVE_ARG_MAX_INLINE`) bounds well below this.
pub const EXECVE_VEC_MAX: usize = 256;

/// Linux generic ABI errno value for "function not implemented" (`ENOSYS`).
/// Used as the `-ENOSYS` magnitude returned from `dispatch` for every
/// syscall number not handled by Phase 2a / 2b.
pub(super) const ENOSYS_VALUE: i32 = 38;
/// Linux generic ABI errno value for "operation not supported" (`EOPNOTSUPP`).
/// Used when a syscall surface exists but the requested object/clock flavor is
/// outside txKernel's current emulation contract.
pub(super) const EOPNOTSUPP_VALUE: i32 = 95;
pub(super) const ENODEV_VALUE: i32 = 19;
/// Linux generic ABI errno value for "no such device or address" (`ENXIO`).
/// Used by interface-index lookup helpers when an ifindex has no backing link.
pub(super) const ENXIO_VALUE: i32 = 6;
/// Linux generic ABI errno value for "bad file descriptor" (`EBADF`).
pub(super) const EBADF_VALUE: i32 = 9;
/// Linux generic ABI errno value for "too many open files" (`EMFILE`).
/// Kept in sync with the soft `RLIMIT_NOFILE` value reported by
/// `prlimit64`.
pub(super) const EMFILE_VALUE: i32 = 24;
/// Linux generic ABI errno value for "bad address" (`EFAULT`).
/// Used by Slice 4's time syscalls when a required user pointer is
/// null, and by every `bootstrap_*` user-VA bridge for invalid user
/// addresses (the canonical `aspace.copy_*_user` lane already
/// surfaces `Errno::EFAULT`; the dispatcher translates it here).
pub(super) const EFAULT_VALUE: i32 = 14;
/// Linux generic ABI errno value for "argument list too long" (`E2BIG`).
/// Used when a syscall argument violates a Phase 2a slice bound (e.g.
/// `write(len > TTY_WRITE_MAX_INLINE)`).
pub(super) const E2BIG_VALUE: i32 = 7;
/// Linux generic ABI errno value for "filename too long" (`ENAMETOOLONG`).
/// Used by Phase 6's `execve(path)` arm when the path overflows
/// `EXECVE_PATH_MAX`.
pub(super) const ENAMETOOLONG_VALUE: i32 = 36;
/// Linux generic ABI errno value for "invalid argument" (`EINVAL`).
/// Used by Phase 2b's `rt_sigprocmask` / `rt_sigaction` for the
/// `sigsetsize != 8` rejection per `SIGNAL_v1` §3 / §15.1, and for
/// any signum out of the 1..=64 range.
pub(super) const EINVAL_VALUE: i32 = 22;
/// Linux generic ABI errno value for "no such process" (`ESRCH`).
/// Used by `rt_sigprocmask` / `rt_sigaction` when the target thread /
/// process is a zombie (no payload to install state on).
pub(super) const ESRCH_VALUE: i32 = 3;
/// Linux generic ABI errno value for "operation not permitted" (`EPERM`).
/// Used by `setpgid` / `setsid` when the caller is not allowed to
/// perform the requested process-group / session change (Wave 2's
/// day-1 surface only supports the self-pid / self-pgid form;
/// cross-process and join-existing-pgid map to `-EPERM`).
pub(super) const EPERM_VALUE: i32 = 1;
/// Linux generic ABI errno value for "out of memory" (`ENOMEM`).
/// Used by `setpgid` / `setsid` when zone allocation fails minting a
/// fresh `ProcessGroup` / `Session`.
pub(super) const ENOMEM_VALUE: i32 = 12;
/// Linux generic ABI errno value for "resource temporarily
/// unavailable" (`EAGAIN`). Reserved for `sys_clone` to surface
/// retriable allocator failures from `step_fork`'s VM-side clone path
/// (`fork_aspace`'s `WouldBlock`); current `step_fork` only surfaces
/// `Zone(_)` / `ParentZombie`, but EAGAIN is the canonical Linux
/// errno for fork's transient-failure case.
pub(super) const EAGAIN_VALUE: i32 = 11;
/// Linux generic ABI errno value for "connection refused" (`ECONNREFUSED`).
pub(super) const ECONNREFUSED_VALUE: i32 = 111;
/// Linux generic ABI errno value for "no child processes" (`ECHILD`).
/// Used by `sys_wait4` when the caller has no children matching the
/// requested selector (Wave 3 of the fork/clone/wait4 slice).
pub(super) const ECHILD_VALUE: i32 = 10;
/// Linux generic ABI errno value for "permission denied" (`EACCES`).
/// Used by Wave 4 Part 4's file-mode arms (`fchmodat`, `fchownat`,
/// `faccessat`, `faccessat2`) when the DAC predicate denies the
/// requested permission bits.
pub(super) const EACCES_VALUE: i32 = 13;
/// Linux generic ABI errno value for "read-only file system"
/// (`EROFS`). Used by `fchmodat` / `fchownat` against devfs (which
/// returns `Errno::EROFS` from `chmod_inode` / `chown_inode` per the
/// Wave 3 slice's projection-only contract).
pub(super) const EROFS_VALUE: i32 = 30;
/// Linux generic ABI errno value for "I/O error" (`EIO`). Used as the
/// fall-through magnitude for `StepOutcome::Yield { shape:
/// YieldShape::OnWaitSource { .. } }` shapes the file-mode arms
/// cannot produce today (chmod/chown/access never block in
/// tmpfs/devfs); matches `errno_to_i32`'s `Errno::EIO` row.
pub(super) const EIO_VALUE: i32 = 5;
/// Soft `RLIMIT_NOFILE` value exported by `prlimit64`.
pub(super) const RLIMIT_NOFILE_CUR: u32 = 1024;

pub(super) fn next_fd_below_nofile(
    process: &Cap<ProcessIdentity>,
    min: u32,
) -> Result<u32, SyscallResult> {
    if min >= RLIMIT_NOFILE_CUR {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    let fd = process.next_fd_above(min);
    if fd >= RLIMIT_NOFILE_CUR {
        return Err(SyscallResult::Error(EMFILE_VALUE));
    }
    Ok(fd)
}

pub(super) fn next_stdio_fd_below_nofile(
    process: &Cap<ProcessIdentity>,
) -> Result<u32, SyscallResult> {
    match next_fd_below_nofile(process, 0) {
        Err(SyscallResult::Error(EINVAL_VALUE)) => Err(SyscallResult::Error(EMFILE_VALUE)),
        other => other,
    }
}

/// Required sigsetsize per Linux RV64 generic ABI: 8 bytes (a single
/// `u64` bitset matching `tx_subsystems::signal::SignalMask`'s
/// internal representation). `rt_sigprocmask` / `rt_sigaction`
/// reject any other value with `-EINVAL`.
pub(super) const SIGSETSIZE_BYTES: u64 = 8;
/// Minimum alternate signal stack size (Linux: MINSIGSTKSZ = 2048).
pub(super) const MINSIGSTKSZ: u64 = 2048;
/// Size of the kernel `struct sigaction` exchanged via `rt_sigaction`
/// on the RV64/LA64 generic ABI.
///
/// Both architectures include `asm-generic/signal.h` without defining
/// `SA_RESTORER`, so the kernel ABI contains exactly three 64-bit words:
///
/// ```text
/// struct sigaction {
///     __sighandler_t  sa_handler;   // 8B
///     unsigned long   sa_flags;     // 8B
///     sigset_t        sa_mask;      // 8B  (single u64 bitset, sigsetsize=8)
/// };
/// ```
///
/// In particular this is **not** libc's public 152-byte `struct sigaction`.
/// Glibc translates that public object to this 24-byte kernel image before
/// issuing syscall 134. Writing a fourth word here corrupts the wrapper's
/// stack and makes concurrent signal delivery fail nondeterministically.
pub(super) const SIGACTION_BYTES: usize = 24;

/// Per-syscall context resolved by the trap-shell wrapper: the calling
/// process / thread, the bound address space, and the bookkeeping the
/// dispatch table needs to act without knowing the wrapper's shape.
///
/// Phase 2a only consumes `process` (for `getpid` / `exit_group` and
/// fd-table lookup) and `thread` (for `exit`). `aspace` is wired into
/// the surface today so the Phase 2b additions (`brk`, `read`) can
/// land without a context-shape break; the field is intentionally
/// unused by the four current arms.
///
/// Dispatch a Phase 2a syscall.
///
/// This is the single entry point that maps a `SyscallRequest` to a
/// concrete `step_*` call. The function is `async` because some arms
/// (notably `NR_WRITE`) loop on `StepOutcome::Yield { shape:
/// YieldShape::OnWaitSource { .. } }` and `.await` the wait-source
/// release per `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`. The four currently
/// implemented arms return synchronously today; the `async` shape
/// stays so Phase 2b's additions (`read`, `brk`) can return
/// `SyscallResult::Return` after one or more `.await` points without
/// changing the surface.
pub async fn dispatch<'a, P>(req: SyscallRequest, ctx: &SyscallCtx<'a>) -> SyscallResult
where
    P: PmapIf + EntropyIf + AuxvIf + SmpIf + tx_hal::ConsoleIf,
    TimekeeperClock<P>: ClockRead + RealtimeControl,
{
    // L0 boundary span — `08_OBSERVATION_v1.md` §6 HOOKS-1.
    // Always opened before the inner dispatch; closed after with the
    // result-shaped syscall exit payload. A `SpanId::NONE` short-circuit
    // ensures we never emit a mismatched span-end when no emitter is installed
    // (test contexts, boards with `ObserverIf` default).
    //
    // Parent-span linkage (L0 → L2 → L4) lives in a per-hart static
    // installed via [`tx_observe::set_current_parent_span`] so the L2
    // record in `tx_scripts::drive` picks it up implicitly without
    // every syscall arm having to thread it through `ScriptCtx`.
    //
    // The threshold-based observation dump trigger is handled one level
    // up in `tx_kernel::thread_future::run_thread` so the dispatch
    // signature stays free of `ConsoleIf + PowerIf` bounds that would
    // ripple into every test-stub platform.
    let l0_span = emit_syscall_enter(&req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    // Deliver an expired ITIMER_REAL on the generic syscall boundary (Linux
    // delivers a fired alarm on the next return-to-userspace from any syscall).
    // Without this, alarm-armed tight loops that never reach a socket wait
    // (netperf UDP_STREAM/TCP_STREAM `send` bursts) never see SIGALRM and hang.
    time::poll_itimer_real_on_syscall_boundary::<P>(ctx);
    let result = dispatch_inner::<P>(req, ctx).await;
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    result
}

/// Narrow unboxed lane for the pthread create/join hot path.
///
/// `thread_future::run_thread` uses this before falling back to the broad
/// dispatcher so cloned thread-task futures do not carry the full generic
/// syscall dispatch state machine. Keep this list restricted to syscalls that
/// are both frequent in pthread lifecycle loops and already implemented by
/// compact direct arms here.
pub fn dispatch_pthread_hot_oneshot(
    req: SyscallRequest,
    ctx: &SyscallCtx<'_>,
) -> Option<SyscallResult> {
    match req.nr {
        NR_FUTEX | NR_RT_SIGPROCMASK | NR_EXIT | NR_SET_TID_ADDRESS => {}
        _ => return None,
    }

    let l0_span = emit_syscall_enter(&req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let result = match req.nr {
        NR_FUTEX => sys_futex_oneshot(req.args, ctx)?,
        NR_RT_SIGPROCMASK => sys_rt_sigprocmask(req.args, ctx),
        NR_EXIT => sys_exit(req.args, ctx),
        NR_SET_TID_ADDRESS => sys_set_tid_address(req.args, ctx),
        _ => unreachable!("pthread hot dispatch prefilter covers all arms"),
    };
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// One-shot syscall lane for `CLONE_THREAD`.
///
/// Process fork can contend on the parent VM and therefore belongs to the
/// boxed async dispatcher. Keeping only pthread creation here prevents the
/// large wait-capable fork state machine from inflating every thread task.
pub fn dispatch_clone_oneshot<P: PmapIf>(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    if req.nr != NR_CLONE {
        return None;
    }
    let flags = req.args[0];
    if (flags & CLONE_THREAD) == 0 {
        return None;
    }
    // Net/mount-namespace clones take the async fork path (namespace creation +
    // SYS_ADMIN check); sys_clone_oneshot returns None for them, so this fast
    // path must defer rather than `.expect(...)` a Some.
    if (flags & (numbers::CLONE_NEWNET | numbers::CLONE_NEWNS)) != 0 {
        return None;
    }

    let l0_span = emit_syscall_enter(req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let ctx = SyscallCtx::new(process.clone(), thread.clone(), aspace.clone());
    let result = sys_clone_oneshot::<P>(req.args, &ctx)
        .expect("dispatch_clone_oneshot prefilters process-fork async clone");
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// One-shot lane for `exit(2)` from an already-resolved thread identity.
///
/// `exit` never returns to userspace and does not need a Linux syscall context:
/// `step_thread_exit` resolves the owning process and `clear_child_tid` state
/// through the thread identity. Keeping this before `SyscallCtx` construction
/// trims the pthread child teardown path without changing the no-return
/// contract.
pub fn dispatch_thread_exit_oneshot(
    req: &SyscallRequest,
    thread: &Cap<ThreadIdentity>,
) -> Option<SyscallResult> {
    if req.nr != NR_EXIT {
        return None;
    }
    let l0_span = emit_syscall_enter(req);
    let result = match step_thread_exit(thread.clone(), req.args[0] as i32) {
        tx_subsystems::thread_runtime::ThreadExitOutcome::Completed => SyscallResult::NoReturn,
        tx_subsystems::thread_runtime::ThreadExitOutcome::Retry => {
            SyscallResult::Error(EAGAIN_VALUE)
        }
    };
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Fast dispatch for immediate syscalls that can be answered from the
/// already-resolved thread/process identity before building a full
/// [`SyscallCtx`].
///
/// This preserves the normal L0 syscall observe span. It is deliberately
/// narrow: only add syscalls whose implementation does not need an address
/// space, credential snapshot, mailbox, timer wheel, delegate registry, or
/// subject context.
pub fn dispatch_cap_only_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
) -> Option<SyscallResult> {
    let result = match req.nr {
        NR_GETPPID => SyscallResult::Return(process.parent_pid().0 as i64),
        _ => dispatch_static_chardev_cap_immediate(req, process)?,
    };
    let l0_span = emit_syscall_enter(req);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Fast dispatch for immediate syscalls that need the already-resolved
/// process plus address space, but still do not need a full [`SyscallCtx`].
///
/// Keep this lane narrow. It is for non-blocking operations whose complete
/// semantics can be expressed from fd table state plus direct user copy.
pub fn dispatch_process_aspace_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    let result = dispatch_static_chardev_immediate(req, process, aspace)?;
    let l0_span = emit_syscall_enter(req);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Fast one-shot lane for syscalls that need only the current thread and
/// address space, avoiding full [`SyscallCtx`] construction.
pub fn dispatch_thread_aspace_oneshot(
    req: &SyscallRequest,
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    if req.nr != NR_RT_SIGPROCMASK {
        return None;
    }
    let l0_span = emit_syscall_enter(req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let result = sys_rt_sigprocmask_thread_aspace(req.args, thread, aspace);
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Fast one-shot lane for syscalls that need the current thread payload and
/// address space, avoiding both full [`SyscallCtx`] construction and reopening
/// `thread.payload` after the thread future has already resolved it.
pub fn dispatch_thread_payload_aspace_oneshot(
    req: &SyscallRequest,
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    if req.nr != NR_RT_SIGPROCMASK {
        return None;
    }
    let l0_span = emit_syscall_enter(req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let result = sys_rt_sigprocmask_thread_payload_aspace(req.args, thread, payload, aspace);
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Fast one-shot lane for anonymous private `mmap` that can commit without
/// parking on the VM range lock.
pub fn dispatch_vm_try_oneshot(
    req: &SyscallRequest,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    if req.nr != NR_MMAP {
        return None;
    }
    let l0_span = emit_syscall_enter(req);
    let result = sys_mmap_private_anon_try(req.args, aspace)?;
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Direct trap-resume lane for strictly synchronous syscalls.
///
/// These arms can complete inside the platform trap handler without resolving
/// the userspace-run wait. The caller is responsible for proving any
/// syscall-specific safety preconditions, such as signal quiescence for
/// `rt_sigprocmask`.
pub fn dispatch_direct_trap_oneshot<P>(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult>
where
    TimekeeperClock<P>: ClockRead,
{
    match req.nr {
        NR_GETPPID => dispatch_cap_only_immediate(req, process),
        // Pure queries / clock reads: never yield, never touch
        // VFS/VM/reactor state. They reuse the same Lane-1 immediate
        // handlers the generic dispatcher runs; serving them from the
        // trap shell skips the full run_thread reactor round-trip
        // (~576us measured end-to-end for getpid under TCG). The trap
        // shell only routes here when no signal is pending (see
        // `direct_syscall_preconditions`), so AST delivery timing is
        // unchanged.
        NR_GETPID | NR_GETTID | NR_GETUID | NR_GETEUID | NR_GETGID | NR_GETEGID
        | NR_CLOCK_GETTIME | NR_GETTIMEOFDAY => {
            let l0_span = emit_syscall_enter(req);
            let ctx = SyscallCtx::new(process.clone(), thread.clone(), aspace.clone());
            let result = match req.nr {
                NR_GETPID => sys_getpid(&ctx),
                NR_GETTID => sys_gettid(&ctx),
                nr if nr == NR_GETUID => sys_getuid(&ctx),
                nr if nr == NR_GETEUID => sys_geteuid(&ctx),
                nr if nr == NR_GETGID => sys_getgid(&ctx),
                nr if nr == NR_GETEGID => sys_getegid(&ctx),
                nr if nr == NR_CLOCK_GETTIME => sys_clock_gettime::<P>(req.args, &ctx),
                nr if nr == NR_GETTIMEOFDAY => sys_gettimeofday::<P>(req.args, &ctx),
                _ => unreachable!("direct query prefilter covers all arms"),
            };
            emit_syscall_exit(l0_span, &result);
            Some(result)
        }
        NR_FUTEX => {
            let l0_span = emit_syscall_enter(req);
            let prev = tx_observe::set_current_parent_span(l0_span);
            let ctx = SyscallCtx::new(process.clone(), thread.clone(), aspace.clone());
            let result = sys_futex_oneshot_with_wake_hint(
                req.args,
                &ctx,
                tx_substrate::wake::MailboxSchedulerHint::WakeHandoff,
            )?;
            tx_observe::set_current_parent_span(prev);
            emit_syscall_exit(l0_span, &result);
            Some(result)
        }
        NR_RT_SIGPROCMASK => dispatch_thread_aspace_oneshot(req, thread, aspace),
        NR_SET_TID_ADDRESS => {
            let l0_span = emit_syscall_enter(req);
            let prev = tx_observe::set_current_parent_span(l0_span);
            let ctx = SyscallCtx::new(process.clone(), thread.clone(), aspace.clone());
            let result = sys_set_tid_address(req.args, &ctx);
            tx_observe::set_current_parent_span(prev);
            emit_syscall_exit(l0_span, &result);
            Some(result)
        }
        _ => None,
    }
}

/// Direct trap-resume lane variant for call sites that already hold the
/// current userspace thread payload.
pub fn dispatch_direct_trap_payload_oneshot<P>(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult>
where
    TimekeeperClock<P>: ClockRead,
{
    match req.nr {
        NR_RT_SIGPROCMASK => dispatch_thread_payload_aspace_oneshot(req, thread, payload, aspace),
        _ => dispatch_direct_trap_oneshot::<P>(req, process, thread, aspace),
    }
}

/// Narrow async lane for VM lifecycle syscalls that are hot in pthread stack
/// setup/teardown.
///
/// This keeps `mmap` / `mprotect` / `munmap` out of the broad generic async
/// dispatcher while preserving their existing RangeLock wait/retry semantics.
/// The lane still emits the normal L0 syscall span; it only shrinks the future
/// allocated by `thread_future::run_thread` before that span opens.
pub async fn dispatch_vm_hot(req: SyscallRequest, ctx: &SyscallCtx<'_>) -> Option<SyscallResult> {
    match req.nr {
        NR_MMAP | NR_MUNMAP | NR_MPROTECT => {}
        _ => return None,
    }

    let l0_span = emit_syscall_enter(&req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let result = match req.nr {
        NR_MMAP => sys_mmap(req.args, ctx).await,
        NR_MUNMAP => sys_munmap(req.args, ctx).await,
        NR_MPROTECT => sys_mprotect(req.args, ctx).await,
        _ => unreachable!("VM hot dispatch prefilter covers all arms"),
    };
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

/// Narrow async lane for `writev(2)` hot in musl stdio.
///
/// This preserves `sys_writev` semantics and the normal L0 syscall observe
/// span, while keeping repeated buffered stdio writes out of the broad generic
/// dispatch future carried by `thread_future::run_thread`.
pub async fn dispatch_writev_hot(
    req: SyscallRequest,
    ctx: &SyscallCtx<'_>,
) -> Option<SyscallResult> {
    emit_writev_hot_trace(b"debug.writev.hot.enter", req.nr as i64);
    if req.nr != NR_WRITEV {
        emit_writev_hot_trace(b"debug.writev.hot.reject", req.nr as i64);
        return None;
    }

    emit_writev_hot_trace(b"debug.writev.hot.before_enter", req.nr as i64);
    let l0_span = emit_syscall_enter(&req);
    emit_writev_hot_trace(b"debug.writev.hot.after_enter", req.nr as i64);
    let prev = tx_observe::set_current_parent_span(l0_span);
    emit_writev_hot_trace(b"debug.writev.hot.after_parent", req.nr as i64);
    let result = sys_writev(req.args, ctx).await;
    emit_writev_hot_trace(b"debug.writev.hot.after_body", req.nr as i64);
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    emit_writev_hot_trace(b"debug.writev.hot.after_exit", req.nr as i64);
    Some(result)
}

fn emit_writev_hot_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

/// Synchronous `writev(2)` lane for PageBacked files.
///
/// This handles the libcbench tmpfile shape without allocating and polling the
/// broad async `sys_writev` future. It is deliberately a narrow prefilter:
/// non-PageBacked fds return `None` and continue through the existing async
/// dispatcher. PageBacked writes that unexpectedly need to yield surface
/// `EAGAIN`; the current PageBacked user-buffer path materializes
/// synchronously, so that is a defensive future-backend branch rather than the
/// libcbench path.
pub fn dispatch_writev_pagebacked_oneshot(
    req: &SyscallRequest,
    ctx: &SyscallCtx<'_>,
) -> Option<SyscallResult> {
    if req.nr != NR_WRITEV || !sys_writev_pagebacked_candidate(req.args, ctx) {
        return None;
    }

    let l0_span = emit_syscall_enter(req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    emit_writev_hot_trace(b"debug.writev.pagebacked_dispatch.enter", req.nr as i64);
    let result =
        sys_writev_pagebacked_oneshot(req.args, ctx).unwrap_or(SyscallResult::Error(EAGAIN_VALUE));
    emit_writev_hot_trace(b"debug.writev.pagebacked_dispatch.after", req.nr as i64);
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    Some(result)
}

async fn dispatch_inner<'a, P>(req: SyscallRequest, ctx: &SyscallCtx<'a>) -> SyscallResult
where
    P: PmapIf + EntropyIf + AuxvIf + SmpIf + tx_hal::ConsoleIf,
    TimekeeperClock<P>: ClockRead + RealtimeControl,
{
    // ── Lane 1: ImmediateSyscall (pure ABI queries, never yield) ──
    // Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md §6.1`: these syscalls
    // do not call drive(), do not enter StepOp, do not construct
    // YieldShape, and do not access VFS/VM/reactor/timer.
    match req.nr {
        NR_GETPID => return sys_getpid(ctx),
        NR_GETTID => return sys_gettid(ctx),
        nr if nr == NR_GETPPID => return sys_getppid(ctx),
        nr if nr == NR_GETPGID => return sys_getpgid(req.args, ctx),
        nr if nr == NR_GETSID => return sys_getsid(req.args, ctx),
        nr if nr == NR_KCMP => return sys_kcmp(req.args, ctx),
        nr if nr == NR_PIDFD_GETFD => return sys_pidfd_getfd(req.args, ctx),
        nr if nr == NR_GETUID => return sys_getuid(ctx),
        nr if nr == NR_GETEUID => return sys_geteuid(ctx),
        nr if nr == NR_GETGID => return sys_getgid(ctx),
        nr if nr == NR_GETEGID => return sys_getegid(ctx),
        nr if nr == NR_GETGROUPS => return sys_getgroups(req.args),
        nr if nr == NR_GETRESUID => return sys_getresuid(req.args, ctx),
        nr if nr == NR_GETRESGID => return sys_getresgid(req.args, ctx),
        nr if nr == NR_GETPRIORITY => return sys_getpriority(req.args, ctx),
        nr if nr == NR_SETPRIORITY => return sys_setpriority(req.args, ctx),
        nr if nr == NR_TIMES => return sys_times::<P>(req.args, ctx),
        nr if nr == NR_GETTIMEOFDAY => return sys_gettimeofday::<P>(req.args, ctx),
        nr if nr == NR_GETITIMER => return sys_getitimer::<P>(req.args, ctx),
        nr if nr == NR_SETITIMER => return sys_setitimer::<P>(req.args, ctx),
        nr if nr == NR_SETTIMEOFDAY => return sys_settimeofday::<P>(req.args, ctx),
        nr if nr == NR_UMASK => return sys_umask(req.args, ctx),
        nr if nr == NR_UNAME => return sys_uname::<P>(req.args, ctx),
        nr if nr == NR_SETHOSTNAME => return sys_sethostname(req.args, ctx),
        nr if nr == NR_PRLIMIT64 => return sys_prlimit64(req.args, ctx),
        nr if nr == NR_GETRLIMIT => return sys_getrlimit(req.args, ctx),
        nr if nr == NR_SETRLIMIT => return sys_setrlimit(req.args, ctx),
        nr if nr == NR_GETRUSAGE => return sys_getrusage(req.args, ctx),
        nr if nr == NR_RT_SIGRETURN => return sys_rt_sigreturn(ctx),
        nr if nr == NR_SCHED_GETATTR => return sys_sched_getattr(req.args, ctx),
        nr if nr == NR_SCHED_SETATTR => return sys_sched_setattr(req.args, ctx),
        nr if nr == NR_SCHED_GETAFFINITY => return sys_sched_getaffinity(req.args, ctx),
        nr if nr == NR_SCHED_SETAFFINITY => return sys_sched_setaffinity(req.args, ctx),
        nr if nr == NR_SCHED_SETSCHEDULER => return sys_sched_setscheduler(req.args, ctx),
        nr if nr == NR_SCHED_SETPARAM => return sys_sched_setparam(req.args, ctx),
        nr if nr == NR_SCHED_GETSCHEDULER => return sys_sched_getscheduler(req.args, ctx),
        nr if nr == NR_SCHED_GETPARAM => return sys_sched_getparam(req.args, ctx),
        nr if nr == NR_GET_MEMPOLICY => return sys_get_mempolicy(req.args, ctx),
        nr if nr == NR_SCHED_YIELD => return sys_sched_yield(),
        nr if nr == NR_SCHED_GET_PRIORITY_MAX => return sys_sched_get_priority_max(req.args),
        nr if nr == NR_SCHED_GET_PRIORITY_MIN => return sys_sched_get_priority_min(req.args),
        nr if nr == NR_SCHED_RR_GET_INTERVAL => return sys_sched_rr_get_interval(req.args, ctx),
        nr if nr == NR_PRCTL => return sys_prctl(req.args, ctx),
        nr if nr == NR_SET_TID_ADDRESS => return sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => return sys_set_robust_list(req.args, ctx),
        nr if nr == NR_GET_ROBUST_LIST => return sys_get_robust_list(req.args, ctx),
        nr if nr == NR_RESTART_SYSCALL => return SyscallResult::Error(ENOSYS_VALUE),
        nr if nr == NR_MADVISE => return sys_madvise(req.args, ctx),
        nr if nr == NR_MLOCK => return sys_mlock(req.args, ctx).await,
        nr if nr == NR_MUNLOCK => return sys_munlock(req.args, ctx).await,
        nr if nr == NR_MLOCKALL => return sys_mlockall(req.args, ctx).await,
        nr if nr == NR_MUNLOCKALL => return sys_munlockall(req.args, ctx).await,
        nr if nr == NR_MINCORE => return sys_mincore(req.args, ctx),
        nr if nr == NR_REMAP_FILE_PAGES => return sys_remap_file_pages(req.args),
        nr if nr == NR_MLOCK2 => return sys_mlock2(req.args, ctx).await,
        nr if nr == NR_UTIMENSAT => return sys_utimensat::<P>(req.args, ctx),
        nr if nr == NR_SHMGET => return sys_shmget(req.args, ctx),
        nr if nr == NR_SHMDT => return sys_shmdt(req.args, ctx).await,
        nr if nr == NR_MSGGET => return sys_msgget(req.args, ctx),
        nr if nr == NR_MSGSND => return sys_msgsnd(req.args, ctx),
        nr if nr == NR_MSGRCV => return sys_msgrcv(req.args, ctx),
        nr if nr == NR_IOPRIO_GET => return sys_ioprio_get(req.args, ctx),
        nr if nr == NR_IOPRIO_SET => return sys_ioprio_set(req.args, ctx),
        nr if nr == NR_SEMGET => return sys_semget(req.args, ctx),
        nr if nr == NR_MQ_OPEN => return sys_mq_open(req.args, ctx),
        nr if nr == NR_MQ_UNLINK => return sys_mq_unlink(req.args, ctx),
        nr if nr == NR_MQ_GETSETATTR => return sys_mq_getsetattr(req.args, ctx),
        nr if nr == NR_MQ_NOTIFY => return sys_mq_notify(req.args, ctx),
        nr if nr == NR_MEMBARRIER => return sys_membarrier::<P>(&req.args),
        nr if nr == NR_TX_OBSERVE_BEGIN => return sys_tx_observe_begin(req.args),
        nr if nr == NR_TX_OBSERVE_TRACE_ON => return sys_tx_observe_trace_on(),
        nr if nr == NR_TX_OBSERVE_TRACE_OFF => return sys_tx_observe_trace_off(),
        _ => {} // fall through to script lanes
    }

    // ── Lanes 2+3: Script-based (OneShotStepOp + Full async drive) ──
    match req.nr {
        nr if nr == NR_GETRANDOM => sys_getrandom(req.args, ctx).await,
        nr if nr == NR_WRITE => sys_write(req.args, ctx).await,
        nr if nr == NR_WRITEV => sys_writev(req.args, ctx).await,
        nr if nr == NR_READ => sys_read::<P>(req.args, ctx).await,
        nr if nr == NR_READV => sys_readv::<P>(req.args, ctx).await,
        nr if nr == NR_MQ_TIMEDSEND => sys_mq_timedsend(req.args, ctx).await,
        nr if nr == NR_MQ_TIMEDRECEIVE => sys_mq_timedreceive(req.args, ctx).await,
        nr if nr == NR_PREAD64 => sys_pread64::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITE64 => sys_pwrite64(req.args, ctx).await,
        nr if nr == NR_PREADV => sys_preadv::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITEV => sys_pwritev(req.args, ctx).await,
        nr if nr == NR_PREADV2 => sys_preadv2::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITEV2 => sys_pwritev2(req.args, ctx).await,
        nr if nr == NR_FADVISE64_64 || nr == NR_FADVISE64 => sys_fadvise64(req.args, ctx),
        nr if nr == NR_READAHEAD => sys_readahead(req.args, ctx),
        nr if nr == NR_SYNC_FILE_RANGE => sys_sync_file_range(req.args, ctx),
        nr if nr == NR_COPY_FILE_RANGE => sys_copy_file_range(req.args, ctx).await,
        nr if nr == NR_VMSPLICE => sys_vmsplice(req.args, ctx).await,
        nr if nr == NR_SPLICE => sys_splice::<P>(req.args, ctx).await,
        nr if nr == NR_TEE => sys_tee(req.args, ctx),
        nr if nr == NR_SOCKET => sys_socket(req.args, ctx),
        nr if nr == NR_SOCKETPAIR => sys_socketpair(req.args, ctx).await,
        nr if nr == NR_BIND => sys_bind(req.args, ctx),
        nr if nr == NR_GETSOCKNAME => sys_getsockname(req.args, ctx),
        nr if nr == NR_SETSOCKOPT => sys_setsockopt(req.args, ctx),
        nr if nr == NR_SENDTO => sys_sendto(req.args, ctx).await,
        nr if nr == NR_RECVFROM => sys_recvfrom::<P>(req.args, ctx).await,
        nr if nr == NR_LISTEN => sys_listen(req.args, ctx),
        nr if nr == NR_CONNECT => sys_connect(req.args, ctx).await,
        nr if nr == NR_ACCEPT => sys_accept::<P>(req.args, ctx).await,
        nr if nr == NR_ACCEPT4 => sys_accept4::<P>(req.args, ctx).await,
        nr if nr == NR_GETPEERNAME => sys_getpeername(req.args, ctx),
        nr if nr == NR_GETSOCKOPT => sys_getsockopt(req.args, ctx),
        nr if nr == NR_SHUTDOWN => sys_shutdown(req.args, ctx),
        nr if nr == NR_SENDMSG => sys_sendmsg(req.args, ctx).await,
        nr if nr == NR_RECVMSG => sys_recvmsg::<P>(req.args, ctx).await,
        nr if nr == NR_SENDMMSG => sys_sendmmsg(req.args, ctx).await,
        nr if nr == NR_RECVMMSG => sys_recvmmsg::<P>(req.args, ctx).await,
        nr if nr == NR_SETITIMER => time::sys_setitimer::<P>(req.args, ctx),
        nr if nr == NR_GETITIMER => time::sys_getitimer::<P>(req.args, ctx),
        nr if nr == NR_SENDFILE64 => sys_sendfile64(req.args, ctx).await,
        nr if nr == NR_PPOLL => sys_ppoll::<P>(req.args, ctx).await,
        nr if nr == NR_PSELECT6 => sys_pselect6::<P>(req.args, ctx).await,
        nr if nr == NR_PSELECT6_TIME64 => sys_pselect6::<P>(req.args, ctx).await,
        nr if nr == NR_EXIT => sys_exit(req.args, ctx),
        nr if nr == NR_EXIT_GROUP => sys_exit_group(req.args, ctx),
        nr if nr == NR_BRK => sys_brk(req.args, ctx).await,
        nr if nr == NR_RT_SIGPROCMASK => sys_rt_sigprocmask(req.args, ctx),
        nr if nr == NR_RT_SIGACTION => sys_rt_sigaction(req.args, ctx),
        nr if nr == NR_RT_SIGPENDING => sys_rt_sigpending(req.args, ctx),
        nr if nr == NR_RT_SIGSUSPEND => return sys_rt_sigsuspend::<P>(req.args, ctx).await,
        nr if nr == NR_RT_SIGQUEUEINFO => sys_rt_sigqueueinfo(req.args, ctx),
        nr if nr == NR_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait::<P>(req.args, ctx).await,
        nr if nr == NR_SIGALTSTACK => sys_sigaltstack(req.args, ctx),
        nr if nr == NR_CAPGET => sys_capget(req.args, ctx),
        nr if nr == NR_CAPSET => sys_capset(req.args, ctx),
        nr if nr == NR_PIDFD_OPEN => sys_pidfd_open(req.args, ctx),
        nr if nr == NR_PIDFD_SEND_SIGNAL => sys_pidfd_send_signal(req.args, ctx),
        nr if nr == NR_FCNTL => sys_fcntl(req.args, ctx),
        nr if nr == NR_SHMCTL => sys_shmctl(req.args, ctx),
        nr if nr == NR_MSGCTL => sys_msgctl(req.args, ctx),
        nr if nr == NR_SEMOP => sys_semop(req.args, ctx).await,
        nr if nr == NR_SEMTIMEDOP => sys_semtimedop::<P>(req.args, ctx).await,
        nr if nr == NR_SEMCTL => sys_semctl(req.args, ctx),
        nr if nr == NR_SHMAT => sys_shmat(req.args, ctx).await,
        nr if nr == NR_EXECVE => sys_execve::<P>(req.args, ctx).await,
        nr if nr == NR_CLONE => sys_clone::<P>(req.args, ctx).await,
        nr if nr == NR_UNSHARE => sys_unshare(req.args, ctx),
        nr if nr == NR_SETNS => sys_setns(req.args, ctx),
        nr if nr == NR_WAIT4 => sys_wait4::<P>(req.args, ctx).await,
        nr if nr == NR_SETPGID => sys_setpgid(req.args, ctx),
        nr if nr == NR_SETSID => sys_setsid(ctx),
        nr if nr == NR_SET_TID_ADDRESS => sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => sys_set_robust_list(req.args, ctx),
        nr if nr == NR_GET_ROBUST_LIST => sys_get_robust_list(req.args, ctx),
        nr if nr == NR_GETCPU => sys_getcpu(req.args, ctx),
        nr if nr == NR_PERSONALITY => sys_personality(req.args, ctx),
        // Wave 2 of the DAC + setuid slice — Part 3 (cred-mutation /
        // cred-reading arms). Each wraps a Wave 1 `cred::step_*`
        // helper through the new `ctx.cred()` accessor.
        nr if nr == NR_SETUID => sys_setuid(req.args, ctx),
        nr if nr == NR_SETGID => sys_setgid(req.args, ctx),
        nr if nr == NR_SETGROUPS => sys_setgroups(req.args, ctx),
        nr if nr == NR_SETREUID => sys_setreuid(req.args, ctx),
        nr if nr == NR_SETREGID => sys_setregid(req.args, ctx),
        nr if nr == NR_SETRESUID => sys_setresuid(req.args, ctx),
        nr if nr == NR_SETRESGID => sys_setresgid(req.args, ctx),
        // Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall
        // arms. Each wraps the FsOps surface Wave 3 Part 2 landed
        // (`chmod_inode` / `chown_inode`) plus a walker-side `access(2)`
        // predicate over the inode meta.
        nr if nr == NR_FCHMOD => sys_fchmod(req.args[0] as u32, req.args[1] as u32, ctx),
        nr if nr == NR_FCHMODAT => sys_fchmodat::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as u32,
            req.args[3] as i32,
            ctx,
        ),
        nr if nr == NR_FCHMODAT2 => sys_fchmodat::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as u32,
            req.args[3] as i32,
            ctx,
        ),
        nr if nr == NR_FCHOWN => sys_fchown(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2] as u32,
            ctx,
        ),
        nr if nr == NR_FCHOWNAT => sys_fchownat::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as u32,
            req.args[3] as u32,
            req.args[4] as i32,
            ctx,
        ),
        nr if nr == NR_FACCESSAT => {
            sys_faccessat::<P>(req.args[0] as i32, req.args[1], req.args[2] as i32, ctx)
        }
        nr if nr == NR_FACCESSAT2 => sys_faccessat2::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as i32,
            req.args[3] as i32,
            ctx,
        ),
        // Wave 2 of the fd-ops slice — fd-management arms
        // (`openat` / `close` / `dup` / `dup3`). `sys_openat` needs
        // `<P>` because the walker's `resolve_path_at` is generic over
        // `PmapIf`; the others operate purely on the fd table and the
        // `Cap<OpenFile>` slot it carries.
        nr if nr == NR_OPENAT => {
            sys_openat::<P>(
                req.args[0] as i32,
                req.args[1],
                req.args[2] as u32,
                req.args[3] as u32,
                ctx,
            )
            .await
        }
        nr if nr == NR_CLOSE => sys_close(req.args[0] as u32, ctx),
        nr if nr == NR_CLOSE_RANGE => sys_close_range(req.args, ctx),
        nr if nr == NR_DUP => sys_dup(req.args[0] as u32, ctx),
        nr if nr == NR_DUP3 => sys_dup3(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2] as u32,
            ctx,
        ),
        // fd-ops Wave 3 — anonymous pipe.
        nr if nr == NR_PIPE2 => sys_pipe2(req.args[0], req.args[1] as u32, ctx).await,
        // fd-ops Wave 4 — `lseek(2)`. Non-async; pure offset compute
        // through `OpenFile::step_lseek`. ESPIPE for non-seekable
        // backings (TTY / chardev / pipe), EISDIR for directories,
        // EINVAL for negative result / overflow / unknown whence.
        nr if nr == NR_LSEEK => sys_lseek(
            req.args[0] as u32,
            req.args[1] as i64,
            req.args[2] as u32,
            ctx,
        ),
        // Slice 2 of the shell-prompt roadmap — VM syscalls. mmap /
        // munmap / mprotect / mremap / madvise use StepOp wrappers
        // (VmMapOp / VmUnmapOp etc.) that yield on RangeLock
        // conflicts; the drive loop parks on WaitSource and retries.
        nr if nr == NR_MMAP => sys_mmap(req.args, ctx).await,
        nr if nr == NR_MUNMAP => sys_munmap(req.args, ctx).await,
        nr if nr == NR_MPROTECT => sys_mprotect(req.args, ctx).await,
        nr if nr == NR_MREMAP => sys_mremap(req.args, ctx).await,
        nr if nr == NR_MSYNC => sys_msync(req.args, ctx).await,
        // Slice 3 of the shell-prompt roadmap — `futex(2)`. v1 honours
        // wait/wake, bitset wait/wake, requeue/cmp-requeue, wake-op,
        // and best-effort PI lock/trylock/unlock selectors. The
        // private/realtime flag bits are recognised at the syscall
        // layer and mapped onto the current single-host wait model.
        nr if nr == NR_FUTEX => sys_futex::<P>(req.args, ctx).await,
        // Slice 4 of the shell-prompt roadmap — time syscalls. The
        // four POSIX clock ids alias to the platform monotonic clock
        // for v1 (CLOCK_REALTIME has no boot-time RTC offset yet;
        // CPU-time clocks have no per-process accounting yet —
        // documented at the constant declarations in `numbers.rs`).
        // `nanosleep` / `clock_nanosleep` park the task on the reactor
        // timer queue for real-duration sleeps; zero-duration and
        // past-deadline cases short-circuit immediately.
        nr if nr == NR_CLOCK_GETTIME => sys_clock_gettime::<P>(req.args, ctx),
        nr if nr == NR_CLOCK_SETTIME => sys_clock_settime::<P>(req.args, ctx),
        nr if nr == NR_CLOCK_GETRES => sys_clock_getres(req.args, ctx),
        nr if nr == NR_NANOSLEEP => sys_nanosleep::<P>(req.args, ctx).await,
        nr if nr == NR_CLOCK_NANOSLEEP => sys_clock_nanosleep::<P>(req.args, ctx).await,
        // Slice 5 of the shell-prompt roadmap — `ioctl(2)` + TTY
        // routing. Without this, musl's `isatty(STDIN_FILENO)` check
        // returns false, the shell starts in non-interactive mode, no
        // prompt is printed. Pure plumbing — all eight TTY ioctl
        // step functions exist; the arm decodes `request` and
        // dispatches. Non-TTY fds and unknown request codes return
        // `-ENOTTY` per Linux's `man ioctl_tty`.
        nr if nr == NR_IOCTL => sys_ioctl::<P>(req.args, ctx),
        // Slice 6 of the shell-prompt roadmap — stat family
        // (`fstat` / `newfstatat` / `getdents64` / `getcwd` / `chdir`
        // / `umask`). `fchdir` returns `-ENOSYS` (carryover; OpenFile
        // has no DEntry hint to install via step_chdir).
        nr if nr == NR_FSTAT => sys_fstat::<P>(req.args, ctx),
        nr if nr == NR_NEWFSTATAT => sys_newfstatat::<P>(req.args, ctx).await,
        nr if nr == NR_GETCWD => sys_getcwd(req.args, ctx),
        nr if nr == NR_CHDIR => sys_chdir(req.args, ctx).await,
        nr if nr == NR_FCHDIR => sys_fchdir::<P>(req.args, ctx).await,
        nr if nr == NR_STATFS => sys_statfs::<P>(req.args, ctx).await,
        nr if nr == NR_FSTATFS => sys_fstatfs::<P>(req.args, ctx).await,
        nr if nr == NR_SYNC => sys_sync::<P>(req.args, ctx).await,
        nr if nr == NR_SYNCFS => sys_syncfs::<P>(req.args, ctx).await,
        nr if nr == NR_FSYNC => sys_fsync::<P>(req.args, ctx).await,
        nr if nr == NR_FDATASYNC => sys_fdatasync::<P>(req.args, ctx).await,
        nr if nr == NR_FLOCK => sys_flock::<P>(req.args, ctx).await,
        // The new mount API is not exposed until its complete fd lifecycle
        // (`fsconfig` -> `fsmount` -> `move_mount`) is implemented.  Returning
        // ENOSYS consistently is important: util-linux then falls back to the
        // legacy mount(2) path instead of retaining a half-backed MountApi fd
        // that can reach ordinary VFS-only rnode dispatch.
        nr if nr == NR_OPEN_TREE || nr == NR_FSOPEN || nr == NR_FSPICK => {
            SyscallResult::Error(ENOSYS_VALUE)
        }
        nr if nr == NR_MOUNT => sys_mount::<P>(req.args, ctx).await,
        nr if nr == NR_UMOUNT2 => sys_umount2::<P>(req.args, ctx).await,
        nr if nr == NR_MKNODAT => sys_mknodat::<P>(req.args, ctx).await,
        nr if nr == NR_GETDENTS64 => sys_getdents64(req.args, ctx).await,
        nr if nr == NR_STATX => sys_statx::<P>(req.args, ctx).await,
        // Slice 7 of the shell-prompt roadmap — fcntl extension +
        // day-1 misc syscalls. None individually heavy; each unblocks
        // a specific shell-startup path.
        nr if nr == NR_KILL => sys_kill(req.args, ctx),
        nr if nr == NR_TKILL => sys_tkill(req.args, ctx),
        nr if nr == NR_TGKILL => sys_tgkill(req.args, ctx),
        // rt_sigreturn is handled in the immediate lane above. The
        // syscall layer restores the parked pre-handler context and
        // returns the SigreturnRestored control-flow marker; live trap
        // frame writeback is completed by the kernel syscall-return
        // path.
        // Slice 8 of the shell-prompt roadmap — file-mutation syscalls.
        // Each arm wraps an in-tree `FsOps::*` step body
        // (`mkdir`/`rmdir`/`unlink`/`rename`/`link`/`symlink`/
        // `read_link`) plus, for the truncate pair, the
        // `page_backed::lifecycle::step_truncate` body. The walker
        // resolves the target path(s); the syscall arm dispatches
        // through `fs_ops_for_dentry` against the parent's dentry to
        // find the in-scope FS surface. `utimensat` is deferred
        // (`-ENOSYS`); `RENAME_EXCHANGE` / `RENAME_WHITEOUT` are
        // recognised flag bits but unsupported. See
        // `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md`
        // Slice 8.
        nr if nr == NR_MKDIRAT => sys_mkdirat(req.args, ctx).await,
        nr if nr == NR_UNLINKAT => sys_unlinkat::<P>(req.args, ctx).await,
        nr if nr == NR_SYMLINKAT => sys_symlinkat(req.args, ctx).await,
        nr if nr == NR_LINKAT => sys_linkat(req.args, ctx).await,
        nr if nr == NR_TRUNCATE => sys_truncate(req.args, ctx).await,
        nr if nr == NR_FTRUNCATE => sys_ftruncate(req.args, ctx).await,
        nr if nr == NR_FALLOCATE => sys_fallocate(req.args, ctx).await,
        nr if nr == NR_READLINKAT => sys_readlinkat(req.args, ctx).await,
        nr if nr == NR_RENAMEAT2 => sys_renameat2(req.args, ctx).await,
        // syslog(2) / klogctl — kernel ring-buffer read/control.
        // Stubbed: type 2 (READ) returns 0 bytes so `dmesg(1)` exits 0.
        nr if nr == NR_SYSLOG => sys_syslog(req.args, ctx),
        nr if nr == NR_PERF_EVENT_OPEN => sys_perf_event_open(req.args, ctx),
        nr if nr == NR_MEMFD_CREATE => sys_memfd_create(req.args, ctx),
        nr if nr == NR_BPF => sys_bpf(req.args, ctx),
        nr if nr == NR_MEMFD_SECRET => sys_memfd_secret(req.args, ctx),
        // PR-10 phase 2 — `userfaultfd(2)` scaffold. Mints a fresh
        // `Cap<UserfaultFd>` (W-Q phase 0 zone), wraps in an
        // `OpenFile` with `OpenFileBacking::Ufd`, installs in the fd
        // table, returns the fd. The companion `UFFDIO_API` ioctl
        // handshake is dispatched from `sys_ioctl` when the resolved
        // fd carries an `OpenFileBacking::Ufd` (see
        // `super::userfaultfd::step_uffdio_api`). Later phases
        // (P-10.3 / .4 / .5) land `UFFDIO_REGISTER` / fault
        // interception / reply ioctls.
        nr if nr == NR_USERFAULTFD => sys_userfaultfd(req.args[0] as u32, ctx),
        // PR-11 phase 1 — `io_setup(2)` scaffold. Mints a fresh
        // `Cap<AioContext>` (W-Z phase 1 zone), wraps in an
        // `OpenFile` with `OpenFileBacking::AioContext`, installs in
        // the fd table, returns the fd. Diverges from Linux which
        // writes a pointer-shape into the `aio_context_t *`
        // out-parameter; per D8 §4.1 we return a real fd and
        // userspace bridges. Phases 2–4 land `io_submit` (worker +
        // `with_on_behalf_of` borrow), `io_getevents`, and
        // `io_destroy`.
        nr if nr == NR_IO_SETUP => sys_io_setup(req.args[0] as u32, req.args[1], ctx),
        // PR-11 phase 2 — `io_submit(2)` dispatch. Resolves the AIO
        // fd, copies each iocb in, validates + pushes onto the
        // context's submit queue, and returns the count admitted.
        // The per-context worker future is the one
        // `sys_io_setup` spawned + stashed for phase 2's deferred-
        // pump model.
        nr if nr == NR_IO_SUBMIT => sys_io_submit(req.args, ctx),
        // PR-11 phase 4 — `io_getevents(2)` dispatch. Drains up to
        // `nr` completions from the AIO context's completion queue;
        // blocks on the `events_available` carrier until `min_nr` is
        // satisfied when `timeout == NULL`. Serialises each drained
        // event into a 32-byte `struct io_event` and writes through
        // P's address space.
        nr if nr == NR_IO_GETEVENTS => sys_io_getevents(req.args, ctx).await,
        // PR-11 phase 5 — `io_destroy(2)` dispatch. Trips the
        // worker's abort signal (cooperative cancel), drops the
        // worker future, removes the fd-table entry. Mirrors
        // `sys_close(2)` on the AIO fd plus the worker teardown.
        nr if nr == NR_IO_DESTROY => sys_io_destroy(req.args[0] as u32, ctx),
        // Future PR-12 phase 0 — `io_uring_setup(2)` scaffold (second
        // `OnBehalfOf<P>` canary). Mints a fresh `Cap<IoUring>`
        // (W-LL phase 0 zone), wraps in an `OpenFile` with
        // `OpenFileBacking::IoUring`, spawns the SQPOLL kthread via
        // `with_on_behalf_of`, installs the fd, returns the fd.
        // Diverges from Linux which writes ring offsets into
        // `*params`; per the scaffold scope the in-kernel `VecDeque`
        // ring doesn't yet need user-mmaps. Phase 1 will land the
        // user-mmapped ring + real SQE dispatch.
        nr if nr == NR_IO_URING_SETUP => sys_io_uring_setup(req.args[0] as u32, req.args[1], ctx),
        // PR-12 scaffold follow-up — `io_uring_enter(2)`. Resolves the
        // io_uring fd, drains the in-kernel SQ scaffold into CQEs, and
        // returns the submitted count. The real user-mmapped ring parser
        // remains a future extension.
        nr if nr == NR_IO_URING_ENTER => sys_io_uring_enter(req.args, ctx),
        // D9-D — `signalfd4(fd, &mask, sizemask, flags)` dispatch.
        // `fd == -1` mints a fresh signalfd cap and installs it at
        // the lowest free fd; `fd >= 0` updates the mask on an
        // existing signalfd. Returns the fd. The companion
        // signalfd-shaped read(2) arm lives in sys_read after the
        // ufd discriminator.
        nr if nr == NR_SIGNALFD => sys_signalfd(req.args, ctx),
        nr if nr == NR_SIGNALFD4 => sys_signalfd4(
            req.args[0] as i32,
            req.args[1],
            req.args[2],
            req.args[3] as u32,
            ctx,
        ),
        // eventfd2(init_val, flags) — mints an eventfd.
        nr if nr == NR_EVENTFD2 => sys_eventfd2(req.args[0], req.args[1] as u32, ctx),
        // timerfd_create(clockid, flags) — mints a timerfd.
        nr if nr == NR_TIMERFD_CREATE => {
            sys_timerfd_create(req.args[0] as u32, req.args[1] as u32, ctx)
        }
        // timerfd_settime(fd, flags, new_value, old_value).
        nr if nr == NR_TIMERFD_SETTIME => sys_timerfd_settime::<P>(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2],
            req.args[3],
            ctx,
        ),
        // timerfd_gettime(fd, curr_value).
        nr if nr == NR_TIMERFD_GETTIME => {
            sys_timerfd_gettime::<P>(req.args[0] as u32, req.args[1], ctx)
        }
        // POSIX timer syscalls.
        nr if nr == NR_TIMER_CREATE => {
            sys_timer_create(req.args[0] as u32, req.args[1], req.args[2], ctx)
        }
        nr if nr == NR_TIMER_DELETE => sys_timer_delete(req.args[0] as u32, ctx),
        nr if nr == NR_TIMER_GETOVERRUN => sys_timer_getoverrun(req.args[0] as u32, ctx),
        nr if nr == NR_TIMER_GETTIME => {
            sys_timer_gettime::<P>(req.args[0] as u32, req.args[1], ctx)
        }
        nr if nr == NR_TIMER_SETTIME => sys_timer_settime::<P>(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2],
            req.args[3],
            ctx,
        ),
        // epoll_create1 / epoll_ctl / epoll_pwait — generic Linux
        // numbers used by musl on RV64 and LoongArch64. musl's
        // epoll_wait wrapper calls epoll_pwait with a null mask on
        // these targets.
        nr if nr == NR_EPOLL_CREATE1 => sys_epoll_create1(req.args[0] as u32, ctx),
        nr if nr == NR_EPOLL_CTL => sys_epoll_ctl(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2] as u32,
            req.args[3],
            ctx,
        ),
        nr if nr == NR_EPOLL_PWAIT => {
            sys_epoll_wait::<P>(
                req.args[0] as u32,
                req.args[1],
                req.args[2] as u32,
                req.args[3] as i32,
                ctx,
            )
            .await
        }
        nr if nr == NR_EPOLL_PWAIT2 => {
            sys_epoll_pwait2::<P>(
                req.args[0] as u32,
                req.args[1],
                req.args[2] as u32,
                req.args[3],
                ctx,
            )
            .await
        }
        nr if nr == NR_INOTIFY_INIT1 => sys_inotify_init1(req.args[0] as u32, ctx),
        nr if nr == NR_INOTIFY_ADD_WATCH => sys_inotify_add_watch(req.args, ctx),
        nr if nr == NR_INOTIFY_RM_WATCH => sys_inotify_rm_watch(req.args, ctx),
        nr if nr == NR_FANOTIFY_INIT => sys_fanotify_init(req.args, ctx),
        nr if nr == NR_FANOTIFY_MARK => sys_fanotify_mark(req.args, ctx),
        nr if nr == NR_NAME_TO_HANDLE_AT => sys_name_to_handle_at::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2],
            req.args[3],
            req.args[4] as u32,
            ctx,
        ),
        nr if nr == NR_OPEN_BY_HANDLE_AT => {
            sys_open_by_handle_at(req.args[0] as i32, req.args[1], req.args[2] as u32, ctx)
        }
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

// ---------------------------------------------------------------------------
// membarrier(2)
// ---------------------------------------------------------------------------

/// `membarrier(cmd, flags, cpu_id)` — issue memory-ordering barriers
/// across all online harts.
///
/// Lane 1 (Immediate): never yields, never enters StepOp.
///
/// Supported commands:
/// - `MEMBARRIER_CMD_QUERY` (0) — returns a bitmask of supported commands.
/// - `MEMBARRIER_CMD_GLOBAL` (1), `_EXPEDITED` (1<<1) — broadcast a
///   [`IpiKind::Membarrier`] to every online hart and busy-wait for all
///   acks. On the receiving hart the IPI handler executes a
///   `core::sync::atomic::fence(SeqCst)` so that all prior stores are
///   globally visible.
/// - `MEMBARRIER_CMD_PRIVATE_EXPEDITED` (1<<3) — same as GLOBAL in a
///   single-address-space kernel.
/// - `MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE` (1<<5) — private
///   expedited plus instruction-fetch barrier (`fence.i` on RV64).
/// - `MEMBARRIER_CMD_REGISTER_*` — registration is a no-op; always
///   returns 0.
/// `flags` and `cpu_id` are currently ignored (must be 0).
fn sys_membarrier<P: SmpIf + CacheIf + TimeIf>(args: &[u64; 6]) -> SyscallResult {
    let cmd = args[0];
    let flags = args[1] as u32;

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => SyscallResult::Return(MEMBARRIER_SUPPORTED_MASK as i64),

        MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
            // The issuing hart participates locally; only remote harts need
            // an IPI and acknowledgement. Sending an SBI software interrupt
            // to the current hart while omitting its software pending bit
            // leaves SSIP permanently asserted and livelocks the trap entry.
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            if cmd == MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE {
                P::fence_i_all();
            }

            let targets = P::online_cpus().without(P::current_cpu_id());
            if targets.is_empty() {
                return SyscallResult::Return(0);
            }
            P::clear_ipi_ack_cpus(IpiKind::Membarrier, targets);
            P::broadcast_ipi(targets, IpiKind::Membarrier);
            // Synchronous wait: spin until every target hart has
            // executed the barrier and acked. The IPI handler on the
            // target hart runs `fence(SeqCst)` + ack before
            // returning to its interrupt context.
            const MEMBARRIER_ACK_TIMEOUT_NS: u64 = 2_000_000_000;
            let deadline = P::read_ns().saturating_add(MEMBARRIER_ACK_TIMEOUT_NS);
            loop {
                let acked = P::ipi_ack_cpus(IpiKind::Membarrier);
                if (acked.bits() & targets.bits()) == targets.bits() {
                    break;
                }
                if P::read_ns() >= deadline {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                core::hint::spin_loop();
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            SyscallResult::Return(0)
        }

        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            // Registration is a no-op in this kernel.
            SyscallResult::Return(0)
        }

        _ => SyscallResult::Error(EINVAL_VALUE),
    }
}

/// Translate the subsystem-shared `Errno` enum into the Linux RV64
/// generic ABI errno number used in `-errno` returns.
///
/// Phase 2a covers only the errnos `OpenFile::step_write` /
/// `tty::execution::step_write` / `CharDeviceOps::write` can
/// produce. Anything outside that set falls back to `EIO`; future
/// phases extend the table in lockstep with the syscall arms.
pub(super) fn errno_to_i32(errno: Errno) -> i32 {
    match errno {
        Errno::E2BIG => 7,
        Errno::EACCES => 13,
        Errno::EADDRINUSE => 98,
        Errno::EADDRNOTAVAIL => 99,
        Errno::EAFNOSUPPORT => 97,
        Errno::EAGAIN => EAGAIN_VALUE,
        Errno::EALREADY => 114,
        Errno::EBADF => EBADF_VALUE,
        Errno::EBUSY => 16,
        Errno::ECANCELED => 125,
        Errno::ECONNREFUSED => 111,
        Errno::EDESTADDRREQ => 89,
        Errno::EDQUOT => 122,
        Errno::EEXIST => 17,
        Errno::EFBIG => 27,
        Errno::EIDRM => 43,
        Errno::ELIBBAD => 80,
        Errno::EFAULT => 14,
        Errno::EINVAL => 22,
        Errno::EINPROGRESS => 115,
        Errno::EIO => 5,
        Errno::EISCONN => 106,
        Errno::EISDIR => 21,
        Errno::ELOOP => 40,
        Errno::ENAMETOOLONG => 36,
        Errno::ENODEV => 19,
        Errno::ENOEXEC => 8,
        Errno::EMSGSIZE => 90,
        Errno::ENOMEM => 12,
        Errno::ENOENT => 2,
        Errno::ENOPROTOOPT => 92,
        Errno::ENOSYS => ENOSYS_VALUE,
        Errno::ENOTCONN => 107,
        Errno::ENOTDIR => 20,
        Errno::ENOTEMPTY => 39,
        Errno::ENOTTY => 25,
        Errno::ENOTSOCK => 88,
        Errno::EOPNOTSUPP => EOPNOTSUPP_VALUE,
        Errno::EPERM => 1,
        Errno::EPIPE => 32,
        Errno::EPROTONOSUPPORT => 93,
        Errno::ERANGE => 34,
        Errno::EROFS => 30,
        Errno::ESPIPE => 29,
        Errno::ESRCH => 3,
        Errno::ESTALE => 116,
        Errno::ETIMEDOUT => 110,
        Errno::EINTR => 4,
        Errno::EMLINK => 31,
        Errno::ESOCKTNOSUPPORT => 94,
    }
}

// ---------------------------------------------------------------------------
// L0 observation hooks for the syscall U/K boundary.
//
// Per `docs/Txv3/08_OBSERVATION_v1.md` §6 OBS-V1-HOOKS-1: emit a
// `SpanBegin(SyscallEnter)` at dispatch entry and a matching
// `SpanEnd(SyscallExit)` at dispatch exit. The functions are no-ops
// when no emitter is installed (test contexts, boards without an
// `ObserverIf` impl).
//
// OBS-2 compliance: raw register-shaped args are emitted as opaque `u64`
// `ArgValue` continuations. No `UserPtr<T>` deref.
// ---------------------------------------------------------------------------

#[inline]
fn emit_syscall_enter(req: &SyscallRequest) -> SpanId {
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    // Linux RV64 = 0 today; LoongArch64 will use 1 once its shim lands.
    // Threading the per-board ABI through the call chain is OBS follow-up
    // work; emitting 0 is correct for the only board currently emitting.
    em.syscall_enter(req.nr as u32, 0, &req.args)
}

#[inline]
fn emit_syscall_exit(span: SpanId, result: &SyscallResult) {
    if span == SpanId::NONE {
        return;
    }
    let Some(em) = tx_observe::current() else {
        return;
    };
    // result_kind: 0=Ok, 1=Err, 2=Restart, 3=Fatal, 4=NoReturn (per
    // OBSERVATION_SERIALIZATION_v0 §8.1). `ExecCommitted` and
    // `SigreturnRestored` and `SigreturnContextRestored` are
    // kernel-internal control-flow markers that never surface as a
    // userspace return value; classify them as NoReturn for the trace so
    // the daemon's syscall slice closes cleanly even though no `a0` write
    // occurs.
    let (ret, errno, result_kind) = match result {
        SyscallResult::Return(v) => (*v, 0, 0u8),
        SyscallResult::CloneReturn { value, .. } => (*value, 0, 0u8),
        SyscallResult::Error(e) => (0, *e, 1u8),
        SyscallResult::NoReturn => (0, 0, 4u8),
        SyscallResult::ExecCommitted => (0, 0, 4u8),
        SyscallResult::SigreturnRestored => (0, 0, 4u8),
        SyscallResult::SigreturnContextRestored => (0, 0, 4u8),
    };
    em.syscall_exit(span, ret, errno, result_kind);
}

/// Linux generic ABI errno value for "no such file or directory"
/// (`ENOENT`). Used by `sys_openat` when the walker reports the file
/// is missing and `O_CREAT` is unset.
pub(super) const ENOENT_VALUE: i32 = 2;
/// Linux generic ABI errno value for "file exists" (`EEXIST`). Used by
/// `sys_openat` when `O_CREAT | O_EXCL` is set and the file already
/// exists.
pub(super) const EEXIST_VALUE: i32 = 17;
/// Linux generic ABI errno value for "is a directory" (`EISDIR`).
/// Used by `sys_openat` when `O_TRUNC` is requested against a
/// directory inode.
pub(super) const EISDIR_VALUE: i32 = 21;
/// Linux generic ABI errno value for "not a directory" (`ENOTDIR`).
/// Used by Slice 6's `sys_chdir` when the resolved path is not a
/// directory and by `sys_getdents64` for a non-directory fd.
pub(super) const ENOTDIR_VALUE: i32 = 20;
/// Linux generic ABI errno value for "interrupted system call" (`EINTR`).
/// Used by `sys_rt_sigsuspend`.
pub(super) const EINTR_VALUE: i32 = 4;
/// Linux generic ABI errno value for "result out of range" (`ERANGE`).
/// Used by Slice 6's `sys_getcwd` when the user buffer is too small
/// for the rendered cwd path (NUL terminator inclusive).
pub(super) const ERANGE_VALUE: i32 = 34;
