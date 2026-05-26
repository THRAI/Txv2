//! Marker registry for structs copied across the kernel/userspace ABI.
//!
//! The entries here are intentionally small data descriptors: `tx-shims`
//! computes Rust `sizeof`/`alignof`/`offsetof`, while
//! `tools/check-kernel-user-layouts.py` extracts the matching C facts from
//! the pinned musl headers and compares the two.

use core::mem::{align_of, offset_of, size_of};

use super::ipc::{
    IpcPermLayout, MqAttrLayout, MsginfoLayout, MsqidDsLayout, SembufLayout, SemidDsLayout,
    SeminfoLayout, ShmInfoLayout, ShmidDsLayout, ShminfoLayout, SigeventPrefixLayout,
};
use super::{fs_basic, misc, signal, time};
use tx_subsystems::tty::structure::{Termios, Winsize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelUserField {
    pub rust: &'static str,
    pub musl: &'static str,
    pub offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelUserLayout {
    pub rust_type: &'static str,
    pub musl_header: &'static str,
    pub musl_type: &'static str,
    pub size: usize,
    pub align: usize,
    pub fields: &'static [KernelUserField],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelUserCandidate {
    pub name: &'static str,
    /// Non-empty when this candidate is backed by a local Rust `#[repr(C)]`
    /// struct that must carry `KernelToUserLayout`.
    pub rust_type: &'static str,
    /// `full`, `prefix`, `manual`, or `gap`.
    pub kind: &'static str,
    /// `checked`, `prefix`, `manual`, `deferred`, or `excluded`.
    pub status: &'static str,
    pub musl_header: &'static str,
    pub musl_type: &'static str,
    pub size: usize,
    pub align: usize,
    pub fields: &'static [KernelUserField],
    pub reason: &'static str,
}

/// Marker trait for `#[repr(C)]` layouts copied to or from userspace.
///
/// Implement this for a kernel-side struct when its byte image is intended to
/// match a musl-visible C struct or a registered prefix of one. The descriptor
/// is the single Rust-side input consumed by the layout detector.
pub trait KernelToUserLayout {
    const LAYOUT: KernelUserLayout;
}

macro_rules! marked_kernel_user_layout {
    (
        $ty:ty,
        header: $header:literal,
        musl: $musl_type:literal,
        fields: [$(($rust_field:ident, $musl_field:literal)),* $(,)?] $(,)?
    ) => {
        impl KernelToUserLayout for $ty {
            const LAYOUT: KernelUserLayout = KernelUserLayout {
                rust_type: stringify!($ty),
                musl_header: $header,
                musl_type: $musl_type,
                size: size_of::<$ty>(),
                align: align_of::<$ty>(),
                fields: &[
                    $(
                        KernelUserField {
                            rust: stringify!($rust_field),
                            musl: $musl_field,
                            offset: offset_of!($ty, $rust_field),
                        },
                    )*
                ],
            };
        }
    };
}

macro_rules! full_candidate {
    ($name:literal, $layout:expr) => {
        KernelUserCandidate {
            name: $name,
            rust_type: $layout.rust_type,
            kind: "full",
            status: "checked",
            musl_header: $layout.musl_header,
            musl_type: $layout.musl_type,
            size: $layout.size,
            align: $layout.align,
            fields: $layout.fields,
            reason: "",
        }
    };
}

macro_rules! deferred_candidate {
    ($name:literal, $header:literal, $musl_type:literal, $reason:literal $(,)?) => {
        KernelUserCandidate {
            name: $name,
            rust_type: "",
            kind: "gap",
            status: "deferred",
            musl_header: $header,
            musl_type: $musl_type,
            size: 0,
            align: 0,
            fields: &[],
            reason: $reason,
        }
    };
}

marked_kernel_user_layout!(
    IpcPermLayout,
    header: "sys/ipc.h",
    musl: "struct ipc_perm",
    fields: [
        (key, "key"),
        (uid, "uid"),
        (gid, "gid"),
        (cuid, "cuid"),
        (cgid, "cgid"),
        (mode, "mode"),
        (__seq, "seq"),
        (__pad1, "__pad1"),
        (__pad2, "__pad2"),
    ],
);

marked_kernel_user_layout!(
    MsqidDsLayout,
    header: "sys/msg.h",
    musl: "struct msqid_ds",
    fields: [
        (msg_perm, "msg_perm"),
        (msg_stime, "msg_stime"),
        (msg_rtime, "msg_rtime"),
        (msg_ctime, "msg_ctime"),
        (msg_cbytes, "msg_cbytes"),
        (msg_qnum, "msg_qnum"),
        (msg_qbytes, "msg_qbytes"),
        (msg_lspid, "msg_lspid"),
        (msg_lrpid, "msg_lrpid"),
        (__unused, "__unused"),
    ],
);

marked_kernel_user_layout!(
    MsginfoLayout,
    header: "sys/msg.h",
    musl: "struct msginfo",
    fields: [
        (msgpool, "msgpool"),
        (msgmap, "msgmap"),
        (msgmax, "msgmax"),
        (msgmnb, "msgmnb"),
        (msgmni, "msgmni"),
        (msgssz, "msgssz"),
        (msgtql, "msgtql"),
        (msgseg, "msgseg"),
    ],
);

marked_kernel_user_layout!(
    SemidDsLayout,
    header: "sys/sem.h",
    musl: "struct semid_ds",
    fields: [
        (sem_perm, "sem_perm"),
        (sem_otime, "sem_otime"),
        (sem_ctime, "sem_ctime"),
        (sem_nsems, "sem_nsems"),
        (__sem_nsems_pad, "__sem_nsems_pad"),
        (__unused3, "__unused3"),
        (__unused4, "__unused4"),
    ],
);

marked_kernel_user_layout!(
    SeminfoLayout,
    header: "sys/sem.h",
    musl: "struct seminfo",
    fields: [
        (semmap, "semmap"),
        (semmni, "semmni"),
        (semmns, "semmns"),
        (semmnu, "semmnu"),
        (semmsl, "semmsl"),
        (semopm, "semopm"),
        (semume, "semume"),
        (semusz, "semusz"),
        (semvmx, "semvmx"),
        (semaem, "semaem"),
    ],
);

marked_kernel_user_layout!(
    SembufLayout,
    header: "sys/sem.h",
    musl: "struct sembuf",
    fields: [
        (sem_num, "sem_num"),
        (sem_op, "sem_op"),
        (sem_flg, "sem_flg"),
    ],
);

marked_kernel_user_layout!(
    ShmidDsLayout,
    header: "sys/shm.h",
    musl: "struct shmid_ds",
    fields: [
        (shm_perm, "shm_perm"),
        (shm_segsz, "shm_segsz"),
        (shm_atime, "shm_atime"),
        (shm_dtime, "shm_dtime"),
        (shm_ctime, "shm_ctime"),
        (shm_cpid, "shm_cpid"),
        (shm_lpid, "shm_lpid"),
        (shm_nattch, "shm_nattch"),
        (__pad1, "__pad1"),
        (__pad2, "__pad2"),
    ],
);

marked_kernel_user_layout!(
    ShminfoLayout,
    header: "sys/shm.h",
    musl: "struct shminfo",
    fields: [
        (shmmax, "shmmax"),
        (shmmin, "shmmin"),
        (shmmni, "shmmni"),
        (shmseg, "shmseg"),
        (shmall, "shmall"),
        (__unused, "__unused"),
    ],
);

marked_kernel_user_layout!(
    ShmInfoLayout,
    header: "sys/shm.h",
    musl: "struct shm_info",
    fields: [
        (used_ids, "used_ids"),
        (shm_tot, "shm_tot"),
        (shm_rss, "shm_rss"),
        (shm_swp, "shm_swp"),
        (swap_attempts, "swap_attempts"),
        (swap_successes, "swap_successes"),
    ],
);

marked_kernel_user_layout!(
    MqAttrLayout,
    header: "mqueue.h",
    musl: "struct mq_attr",
    fields: [
        (mq_flags, "mq_flags"),
        (mq_maxmsg, "mq_maxmsg"),
        (mq_msgsize, "mq_msgsize"),
        (mq_curmsgs, "mq_curmsgs"),
        (__unused, "__unused"),
    ],
);

marked_kernel_user_layout!(
    SigeventPrefixLayout,
    header: "signal.h",
    musl: "struct sigevent",
    fields: [
        (sigval, "sigev_value"),
        (sigev_signo, "sigev_signo"),
        (sigev_notify, "sigev_notify"),
    ],
);

marked_kernel_user_layout!(
    Termios,
    header: "termios.h",
    musl: "struct termios",
    fields: [
        (c_iflag, "c_iflag"),
        (c_oflag, "c_oflag"),
        (c_cflag, "c_cflag"),
        (c_lflag, "c_lflag"),
        (c_line, "c_line"),
        (c_cc, "c_cc"),
    ],
);

marked_kernel_user_layout!(
    Winsize,
    header: "sys/ioctl.h",
    musl: "struct winsize",
    fields: [
        (ws_row, "ws_row"),
        (ws_col, "ws_col"),
        (ws_xpixel, "ws_xpixel"),
        (ws_ypixel, "ws_ypixel"),
    ],
);

pub const KERNEL_USER_LAYOUTS: &[KernelUserLayout] = &[
    time::layout_descriptors::TIMESPEC_LAYOUT,
    time::layout_descriptors::TIMEVAL_LAYOUT,
    time::layout_descriptors::ITIMERVAL_LAYOUT,
    time::layout_descriptors::ITIMERSPEC_LAYOUT,
    time::layout_descriptors::TMS_LAYOUT,
    time::layout_descriptors::TIMEX_LAYOUT,
    fs_basic::layout_descriptors::STAT_LAYOUT,
    fs_basic::layout_descriptors::STATX_TIMESTAMP_LAYOUT,
    fs_basic::layout_descriptors::STATX_LAYOUT,
    fs_basic::layout_descriptors::STATFS_LAYOUT,
    misc::layout_descriptors::UTSNAME_LAYOUT,
    misc::layout_descriptors::RLIMIT_LAYOUT,
    signal::layout_descriptors::SIGALTSTACK_LAYOUT,
    <IpcPermLayout as KernelToUserLayout>::LAYOUT,
    <MsqidDsLayout as KernelToUserLayout>::LAYOUT,
    <MsginfoLayout as KernelToUserLayout>::LAYOUT,
    <SemidDsLayout as KernelToUserLayout>::LAYOUT,
    <SeminfoLayout as KernelToUserLayout>::LAYOUT,
    <SembufLayout as KernelToUserLayout>::LAYOUT,
    <ShmidDsLayout as KernelToUserLayout>::LAYOUT,
    <ShminfoLayout as KernelToUserLayout>::LAYOUT,
    <ShmInfoLayout as KernelToUserLayout>::LAYOUT,
    <MqAttrLayout as KernelToUserLayout>::LAYOUT,
    <Winsize as KernelToUserLayout>::LAYOUT,
];

pub fn kernel_user_layouts() -> &'static [KernelUserLayout] {
    KERNEL_USER_LAYOUTS
}

pub const KERNEL_USER_LAYOUT_CANDIDATES: &[KernelUserCandidate] = &[
    full_candidate!("struct timespec", time::layout_descriptors::TIMESPEC_LAYOUT),
    full_candidate!("struct timeval", time::layout_descriptors::TIMEVAL_LAYOUT),
    full_candidate!(
        "struct itimerval",
        time::layout_descriptors::ITIMERVAL_LAYOUT
    ),
    full_candidate!(
        "struct itimerspec",
        time::layout_descriptors::ITIMERSPEC_LAYOUT
    ),
    full_candidate!("struct tms", time::layout_descriptors::TMS_LAYOUT),
    full_candidate!("struct timex", time::layout_descriptors::TIMEX_LAYOUT),
    full_candidate!("struct stat", fs_basic::layout_descriptors::STAT_LAYOUT),
    full_candidate!(
        "struct statx_timestamp",
        fs_basic::layout_descriptors::STATX_TIMESTAMP_LAYOUT
    ),
    full_candidate!("struct statx", fs_basic::layout_descriptors::STATX_LAYOUT),
    full_candidate!("struct statfs", fs_basic::layout_descriptors::STATFS_LAYOUT),
    KernelUserCandidate {
        name: "struct dirent/linux_dirent64",
        rust_type: fs_basic::layout_descriptors::LINUX_DIRENT64_HEADER_LAYOUT.rust_type,
        kind: "prefix",
        status: "prefix",
        musl_header: "dirent.h",
        musl_type: "struct dirent",
        size: 19,
        align: 0,
        fields: &[
            KernelUserField {
                rust: "d_ino",
                musl: "d_ino",
                offset: 0,
            },
            KernelUserField {
                rust: "d_off",
                musl: "d_off",
                offset: 8,
            },
            KernelUserField {
                rust: "d_reclen",
                musl: "d_reclen",
                offset: 16,
            },
            KernelUserField {
                rust: "d_type",
                musl: "d_type",
                offset: 18,
            },
            KernelUserField {
                rust: "d_name",
                musl: "d_name",
                offset: 19,
            },
        ],
        reason: "getdents64 emits the variable-length linux_dirent64 record prefix; musl's public dirent adds a fixed d_name[256] tail.",
    },
    full_candidate!("struct utsname", misc::layout_descriptors::UTSNAME_LAYOUT),
    full_candidate!("struct rlimit", misc::layout_descriptors::RLIMIT_LAYOUT),
    KernelUserCandidate {
        name: "struct rusage",
        rust_type: "",
        kind: "prefix",
        status: "prefix",
        musl_header: "sys/resource.h",
        musl_type: "struct rusage",
        size: 144,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "ru_utime",
                musl: "ru_utime",
                offset: 0,
            },
            KernelUserField {
                rust: "ru_stime",
                musl: "ru_stime",
                offset: 16,
            },
            KernelUserField {
                rust: "ru_maxrss",
                musl: "ru_maxrss",
                offset: 32,
            },
            KernelUserField {
                rust: "ru_nivcsw",
                musl: "ru_nivcsw",
                offset: 136,
            },
        ],
        reason: "wait4/getrusage use Linux's raw 18-long rusage image; musl's public struct rusage keeps an extra reserved tail.",
    },
    full_candidate!(
        "struct sigaltstack",
        signal::layout_descriptors::SIGALTSTACK_LAYOUT
    ),
    KernelUserCandidate {
        name: "sigset_t syscall mask prefix",
        rust_type: "",
        kind: "prefix",
        status: "prefix",
        musl_header: "signal.h",
        musl_type: "sigset_t",
        size: 8,
        align: 8,
        fields: &[KernelUserField {
            rust: "__bits",
            musl: "__bits",
            offset: 0,
        }],
        reason: "rt signal syscalls pass sigsetsize = _NSIG/8, so the kernel reads/writes the first 8 bytes of musl's 128-byte sigset_t.",
    },
    KernelUserCandidate {
        name: "kernel struct sigaction",
        rust_type: "",
        kind: "gap",
        status: "deferred",
        musl_header: "",
        musl_type: "",
        size: 0,
        align: 0,
        fields: &[],
        reason: "musl translates public struct sigaction through its internal k_sigaction header; the checker needs internal-header extraction before enforcing it.",
    },
    KernelUserCandidate {
        name: "siginfo_t",
        rust_type: "",
        kind: "manual",
        status: "manual",
        musl_header: "signal.h",
        musl_type: "siginfo_t",
        size: 128,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "si_signo",
                musl: "si_signo",
                offset: 0,
            },
            KernelUserField {
                rust: "si_errno",
                musl: "si_errno",
                offset: 4,
            },
            KernelUserField {
                rust: "si_code",
                musl: "si_code",
                offset: 8,
            },
        ],
        reason: "rt_sigtimedwait currently zero-fills the Linux-sized siginfo record; semantic population is tracked separately.",
    },
    KernelUserCandidate {
        name: "struct signalfd_siginfo",
        rust_type: "",
        kind: "manual",
        status: "manual",
        musl_header: "sys/signalfd.h",
        musl_type: "struct signalfd_siginfo",
        size: 128,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "ssi_signo",
                musl: "ssi_signo",
                offset: 0,
            },
            KernelUserField {
                rust: "ssi_code",
                musl: "ssi_code",
                offset: 8,
            },
            KernelUserField {
                rust: "ssi_pid",
                musl: "ssi_pid",
                offset: 12,
            },
            KernelUserField {
                rust: "ssi_int",
                musl: "ssi_int",
                offset: 20,
            },
            KernelUserField {
                rust: "ssi_ptr",
                musl: "ssi_ptr",
                offset: 48,
            },
        ],
        reason: "signalfd read serializes the common prefix plus POSIX timer ssi_int/ssi_ptr when siginfo is available.",
    },
    KernelUserCandidate {
        name: "struct termios TCGETS prefix",
        rust_type: <Termios as KernelToUserLayout>::LAYOUT.rust_type,
        kind: "prefix",
        status: "prefix",
        musl_header: "termios.h",
        musl_type: "struct termios",
        size: <Termios as KernelToUserLayout>::LAYOUT.size,
        align: <Termios as KernelToUserLayout>::LAYOUT.align,
        fields: <Termios as KernelToUserLayout>::LAYOUT.fields,
        reason: "TCGETS/TCSETS copy Linux's kernel termios prefix; musl's public struct extends the prefix with speed fields.",
    },
    full_candidate!("struct winsize", <Winsize as KernelToUserLayout>::LAYOUT),
    KernelUserCandidate {
        name: "struct epoll_event",
        rust_type: "",
        kind: "manual",
        status: "manual",
        musl_header: "sys/epoll.h",
        musl_type: "struct epoll_event",
        size: 16,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "events",
                musl: "events",
                offset: 0,
            },
            KernelUserField {
                rust: "data",
                musl: "data",
                offset: 8,
            },
        ],
        reason: "epoll events are manually serialized as events:u32 plus natural LP64 padding plus epoll_data_t.",
    },
    KernelUserCandidate {
        name: "struct iovec",
        rust_type: "",
        kind: "manual",
        status: "manual",
        musl_header: "sys/uio.h",
        musl_type: "struct iovec",
        size: 16,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "iov_base",
                musl: "iov_base",
                offset: 0,
            },
            KernelUserField {
                rust: "iov_len",
                musl: "iov_len",
                offset: 8,
            },
        ],
        reason: "readv/writev parse iovec entries from bytes instead of a Rust repr(C) struct.",
    },
    KernelUserCandidate {
        name: "struct pollfd",
        rust_type: "",
        kind: "manual",
        status: "manual",
        musl_header: "poll.h",
        musl_type: "struct pollfd",
        size: 8,
        align: 4,
        fields: &[
            KernelUserField {
                rust: "fd",
                musl: "fd",
                offset: 0,
            },
            KernelUserField {
                rust: "events",
                musl: "events",
                offset: 4,
            },
            KernelUserField {
                rust: "revents",
                musl: "revents",
                offset: 6,
            },
        ],
        reason: "ppoll parses and writes pollfd entries from bytes.",
    },
    KernelUserCandidate {
        name: "cpu_set_t sched_affinity mask prefix",
        rust_type: "",
        kind: "prefix",
        status: "prefix",
        musl_header: "sched.h",
        musl_type: "cpu_set_t",
        size: 8,
        align: 8,
        fields: &[KernelUserField {
            rust: "__bits0",
            musl: "__bits",
            offset: 0,
        }],
        reason: "sched_getaffinity/sched_setaffinity currently exchange the first u64 of musl's 128-byte cpu_set_t.",
    },
    deferred_candidate!(
        "struct sched_param",
        "sched.h",
        "struct sched_param",
        "sched_setscheduler is a success stub and musl's sched_getparam/sched_setparam wrappers return ENOSYS; enforce this layout when scheduler parameters are actually copied.",
    ),
    deferred_candidate!(
        "musl robust_list_head",
        "",
        "",
        "musl registers an internal three-word robust-list head; txKernel stores the pointer/length and best-effort reads fields on thread exit, but the checker needs internal-header extraction before enforcing it.",
    ),
    KernelUserCandidate {
        name: "struct sigevent timer_create prefix",
        rust_type: "",
        kind: "prefix",
        status: "prefix",
        musl_header: "signal.h",
        musl_type: "struct sigevent",
        size: 24,
        align: 8,
        fields: &[
            KernelUserField {
                rust: "sigev_value",
                musl: "sigev_value",
                offset: 0,
            },
            KernelUserField {
                rust: "sigev_signo",
                musl: "sigev_signo",
                offset: 8,
            },
            KernelUserField {
                rust: "sigev_notify",
                musl: "sigev_notify",
                offset: 12,
            },
            KernelUserField {
                rust: "sigev_notify_thread_id",
                musl: "sigev_notify_thread_id",
                offset: 16,
            },
        ],
        reason: "musl translates POSIX timer_create through a kernel ksigevent header; txKernel has no POSIX timer_create syscall yet, but the prefix is probed so future wiring cannot drift.",
    },
    deferred_candidate!(
        "struct flock",
        "fcntl.h",
        "struct flock",
        "fcntl locking commands currently return ENOSYS; enforce struct flock when F_GETLK/F_SETLK/F_SETLKW or OFD locks are implemented.",
    ),
    deferred_candidate!(
        "fd_set select bitset",
        "sys/select.h",
        "fd_set",
        "select/pselect are not dispatched yet; ppoll covers the current poll-family path through struct pollfd.",
    ),
    deferred_candidate!(
        "struct msghdr",
        "sys/socket.h",
        "struct msghdr",
        "socket sendmsg/recvmsg are not dispatched in this branch; add checked/manual coverage when socket syscalls land.",
    ),
    deferred_candidate!(
        "struct mmsghdr",
        "sys/socket.h",
        "struct mmsghdr",
        "sendmmsg/recvmmsg are not dispatched in this branch; recvmmsg also needs timespec timeout handling.",
    ),
    deferred_candidate!(
        "struct sockaddr",
        "sys/socket.h",
        "struct sockaddr",
        "socket address syscalls are not dispatched in this branch; enforce sockaddr-family records with the socket surface.",
    ),
    deferred_candidate!(
        "struct sysinfo",
        "sys/sysinfo.h",
        "struct sysinfo",
        "sysinfo(2) is not dispatched yet; add checked coverage when getloadavg/sysconf memory queries are wired.",
    ),
    full_candidate!("struct ipc_perm", <IpcPermLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct msqid_ds", <MsqidDsLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct msginfo", <MsginfoLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct semid_ds", <SemidDsLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct seminfo", <SeminfoLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct sembuf", <SembufLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct shmid_ds", <ShmidDsLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct shminfo", <ShminfoLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct shm_info", <ShmInfoLayout as KernelToUserLayout>::LAYOUT),
    full_candidate!("struct mq_attr", <MqAttrLayout as KernelToUserLayout>::LAYOUT),
    KernelUserCandidate {
        name: "struct sigevent mq_notify prefix",
        rust_type: <SigeventPrefixLayout as KernelToUserLayout>::LAYOUT.rust_type,
        kind: "prefix",
        status: "prefix",
        musl_header: "signal.h",
        musl_type: "struct sigevent",
        size: <SigeventPrefixLayout as KernelToUserLayout>::LAYOUT.size,
        align: <SigeventPrefixLayout as KernelToUserLayout>::LAYOUT.align,
        fields: <SigeventPrefixLayout as KernelToUserLayout>::LAYOUT.fields,
        reason: "mq_notify only consumes the stable sigevent header before the notification-specific union tail.",
    },
    KernelUserCandidate {
        name: "struct aiocb",
        rust_type: "",
        kind: "gap",
        status: "excluded",
        musl_header: "aio.h",
        musl_type: "struct aiocb",
        size: 0,
        align: 0,
        fields: &[],
        reason: "musl POSIX aio is thread-library state, not a kernel syscall ABI struct.",
    },
    KernelUserCandidate {
        name: "Linux AIO raw UAPI",
        rust_type: "",
        kind: "gap",
        status: "excluded",
        musl_header: "",
        musl_type: "",
        size: 0,
        align: 0,
        fields: &[],
        reason: "musl does not use Linux raw io_setup/io_submit for POSIX aio; txKernel's raw AIO scaffold intentionally diverges.",
    },
    KernelUserCandidate {
        name: "io_uring UAPI",
        rust_type: "",
        kind: "gap",
        status: "excluded",
        musl_header: "",
        musl_type: "",
        size: 0,
        align: 0,
        fields: &[],
        reason: "musl libc has no io_uring wrapper structs; io_uring compatibility is tracked as Linux UAPI work, not musl libc layout work.",
    },
    KernelUserCandidate {
        name: "userfaultfd UAPI",
        rust_type: "",
        kind: "gap",
        status: "excluded",
        musl_header: "",
        musl_type: "",
        size: 0,
        align: 0,
        fields: &[],
        reason: "userfaultfd ioctl records come from Linux UAPI headers that musl does not ship as libc structs.",
    },
];

pub fn kernel_user_layout_candidates() -> &'static [KernelUserCandidate] {
    KERNEL_USER_LAYOUT_CANDIDATES
}
