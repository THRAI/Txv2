use alloc::vec::Vec;

use crate::linux_syscall::KernelToUserLayout;
use tx_subsystems::tty::structure::Termios;

#[test]
fn marker_registry_exposes_current_full_musl_layouts() {
    let names: Vec<&str> = crate::linux_syscall::kernel_user_layouts()
        .iter()
        .map(|layout| layout.rust_type)
        .collect();

    for expected in [
        "TimespecLayout",
        "TimevalLayout",
        "TmsLayout",
        "StatLayout",
        "StatxTimestamp",
        "StatxLayout",
        "StatfsLayout",
        "UtsnameLayout",
        "RlimitLayout",
        "SigaltstackLayout",
        "IpcPermLayout",
        "MsqidDsLayout",
        "MsginfoLayout",
        "SemidDsLayout",
        "SeminfoLayout",
        "SembufLayout",
        "ShmidDsLayout",
        "ShminfoLayout",
        "ShmInfoLayout",
        "MqAttrLayout",
    ] {
        assert!(
            names.contains(&expected),
            "missing kernel-to-user musl layout marker for {expected}"
        );
    }
}

#[test]
fn prefix_repr_c_structs_have_marker_descriptors() {
    assert_eq!(<Termios as KernelToUserLayout>::LAYOUT.rust_type, "Termios");
    assert_eq!(
        <crate::linux_syscall::ipc::SigeventPrefixLayout as KernelToUserLayout>::LAYOUT.rust_type,
        "SigeventPrefixLayout"
    );
}

#[test]
fn candidate_registry_accounts_for_all_current_musl_surfaces() {
    let names: Vec<&str> = crate::linux_syscall::kernel_user_layout_candidates()
        .iter()
        .map(|candidate| candidate.name)
        .collect();

    for expected in [
        "struct timespec",
        "struct timeval",
        "struct tms",
        "struct stat",
        "struct statx_timestamp",
        "struct statx",
        "struct statfs",
        "struct dirent/linux_dirent64",
        "struct utsname",
        "struct rlimit",
        "struct rusage",
        "struct sigaltstack",
        "sigset_t syscall mask prefix",
        "kernel struct sigaction",
        "siginfo_t",
        "struct signalfd_siginfo",
        "struct termios TCGETS prefix",
        "struct winsize",
        "struct epoll_event",
        "struct iovec",
        "struct pollfd",
        "struct itimerspec",
        "cpu_set_t sched_affinity mask prefix",
        "struct sched_param",
        "musl robust_list_head",
        "struct itimerval",
        "struct sigevent timer_create prefix",
        "struct flock",
        "fd_set select bitset",
        "struct msghdr",
        "struct mmsghdr",
        "struct sockaddr",
        "struct timex",
        "struct sysinfo",
        "struct ipc_perm",
        "struct msqid_ds",
        "struct msginfo",
        "struct semid_ds",
        "struct seminfo",
        "struct sembuf",
        "struct shmid_ds",
        "struct shminfo",
        "struct shm_info",
        "struct mq_attr",
        "struct sigevent mq_notify prefix",
        "struct aiocb",
        "Linux AIO raw UAPI",
        "io_uring UAPI",
        "userfaultfd UAPI",
    ] {
        assert!(
            names.contains(&expected),
            "missing kernel-to-user musl candidate for {expected}"
        );
    }
}
