#!/usr/bin/env python3
"""List OSComp/LTP syscall batches for targeted local runs.

The source of truth is the current OSComp sdcard's
`/musl/ltp/runtest/syscalls` list, queried through
`tools/build-slim-sdcard.py --list-cases ltp-musl`.  Any temporary/cache
files live under `target/codex-tmp` so runs stay inside the Txv2 tree.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CACHE_DIR = ROOT / "target" / "codex-tmp"
CACHE_FILE = CACHE_DIR / "ltp-cases.txt"


def is_valid_case_name(case: str) -> bool:
    return bool(case) and not case.endswith("_")


SKIP_CASES: set[str] = {
    # Legacy epoll stress case. Slow on Txv2 and not useful while the
    # socket/network surface is intentionally out of scope. Tracked in
    # docs/ljs/ltp-network-deferred.md.
    "epoll01",
    # Defensive guard for malformed prefix artifacts such as `epoll_`.
    "epoll_",
    # Uses socket/socketpair paths and checkpoint synchronization; defer until
    # socket fd/readiness is implemented. Tracked in docs/ljs/ltp-network-deferred.md.
    "epoll_wait05",
    # CVE stress test with .max_runtime = 150s and 1,000,000 timerfd_settime
    # races. Keep it out of fast p0; run explicitly when timerfd stress is the
    # target.
    "timerfd_settime02",
}


SKIP_PREFIXES: set[str] = {
    # These cases depend on socketpair()/socket readiness. Keep them out of
    # the fast LTP batches until the socket/network module is a real target.
    # Tracked in docs/ljs/ltp-network-deferred.md.
    "epoll_pwait",
    # Network/socket groups are tracked separately and intentionally skipped
    # for the current LTP bringup pass.
    "accept",
    "accept4",
    "bind",
    "connect",
    "getpeername",
    "getsockname",
    "getsockopt",
    "listen",
    "recv",
    "recvfrom",
    "recvmsg",
    "recvmmsg",
    "send",
    "sendmmsg",
    "sendmsg",
    "sendto",
    "setsockopt",
    "socket",
    "socketcall",
    "socketpair",
}


BATCH_PREFIXES: dict[str, list[str]] = {
    "smoke": """
        abort confstr fmtmsg getcontext gethostbyname_r gethostid gethostname
        getpagesize getrandom mallinfo mallopt memcmp memcpy memset nftw
        pathconf fpathconf profil qmm realpath string switch sysconf syscall
        ulimit
    """.split(),
    "fd-io": """
        close close_range dup fcntl ioctl ioctl_loop ioctl_ns ioctl_sg sockioctl
        read write readv writev pread pwrite preadv pwritev llseek lseek pipe
        readahead posix_fadvise fsync fdatasync sync syncfs sync_file_range
        fallocate copy_file_range splice tee vmsplice sendfile
    """.split(),
    "vfs": """
        access faccessat chmod fchmod fchmodat chown fchown fchownat lchown
        chdir fchdir getcwd creat open openat open_by_handle_at name_to_handle_at
        stat lstat fstat fstatat statx statfs fstatfs statvfs getdents readdir
        readlink readlinkat truncate ftruncate utime utimes futimesat utimensat
        link linkat symlink symlinkat unlink unlinkat rmdir mkdir mkdirat mknod
        mknodat rename renameat flock getxattr lgetxattr fgetxattr listxattr
        llistxattr flistxattr setxattr fsetxattr removexattr lremovexattr
        fremovexattr umask prot_hsymlinks
    """.split(),
    "vm": """
        brk sbrk mmap munmap mprotect mremap madvise mincore mlock mlockall
        munlock munlockall memfd_create msync remap_file_pages mbind
        get_mempolicy set_mempolicy migrate_pages move_pages process_madvise
        dirtyc dirtypipe pkey
    """.split(),
    "process": """
        clone fork vfork execve execveat execl execle execlp execv execvp exit
        exit_group wait waitid waitpid getpid getppid gettid getpgid getpgrp
        getsid setpgid setpgrp setsid set_tid_address get_robust_list
        set_robust_list personality kcmp pidfd_open pidfd_send_signal
        pidfd_getfd process_vm_readv process_vm_writev
    """.split(),
    "cred": """
        capget capset getuid geteuid getgid getegid getresuid getresgid setuid
        setgid setreuid setregid setresuid setresgid setfsuid setfsgid setegid
        setgroups getgroups keyctl add_key request_key
    """.split(),
    "signal": """
        kill tkill tgkill rt_sigaction rt_sigprocmask rt_sigqueueinfo
        rt_sigsuspend rt_sigtimedwait rt_tgsigqueueinfo sigaction sigaltstack
        signal sigpending sigprocmask sigsuspend sigtimedwait sigwait
        sigwaitinfo sighold sigrelse signalfd sgetmask ssetmask pause
    """.split(),
    "time": """
        alarm adjtimex clock_adjtime clock_getres clock_gettime clock_nanosleep
        clock_settime getitimer setitimer gettimeofday settimeofday nanosleep
        time times timer_create timer_delete timer_getoverrun timer_gettime
        timer_settime timerfd timerfd_create timerfd_gettime timerfd_settime
        stime leapsec
    """.split(),
    "ipc": """
        semctl semget semop msgctl msgget msgrcv msgsnd msgstress shmctl shmget
        shmat shmdt mq_open mq_notify mq_timedreceive mq_timedsend mq_unlink
    """.split(),
    "event": """
        epoll epoll_create epoll_ctl epoll_wait epoll_pwait eventfd futex_wait
        futex_wake futex_cmp_requeue futex_wait_bitset futex_waitv inotify
        inotify_init fanotify poll ppoll pselect select userfaultfd
    """.split(),
    "net": """
        accept accept4 bind connect getpeername getsockname getsockopt listen
        recv recvfrom recvmsg recvmmsg send sendmmsg sendmsg sendto setsockopt
        socket socketcall socketpair
    """.split(),
    "sched": """
        getcpu getpriority setpriority getrlimit setrlimit getrusage nice prctl
        sched_get_priority_max sched_get_priority_min sched_getaffinity
        sched_setaffinity sched_getattr sched_setattr sched_getparam
        sched_setparam sched_getscheduler sched_setscheduler
        sched_rr_get_interval sched_yield ioprio_get ioprio_set membarrier
    """.split(),
    "mount": """
        mount umount chroot pivot_root unshare setns fsopen fsconfig fsmount
        fspick move_mount open_tree mount_setattr swapon swapoff acct
        init_module finit_module delete_module reboot vhangup
    """.split(),
    "heavy": """
        bpf_map bpf_prog perf_event_open ptrace quotactl sysctl sysfs sysinfo
        syslog ustat cacheflush arch_prctl ioperm iopl modify_ldt
        set_thread_area setdomainname getdomainname sethostname newuname uname
    """.split(),
    "aio": """
        io_setup io_destroy io_submit io_cancel io_getevents io_pgetevents
        io_uring
    """.split(),
    "p0": """
        epoll_ctl epoll_wait epoll_pwait eventfd poll ppoll select pselect
        futex_wait futex_wake alarm setitimer getitimer nanosleep
        clock_nanosleep timerfd timerfd_create timerfd_gettime timerfd_settime
    """.split(),
}

BATCH_ORDER = [
    "p0",
    "smoke",
    "fd-io",
    "vfs",
    "vm",
    "process",
    "cred",
    "signal",
    "time",
    "ipc",
    "event",
    "net",
    "sched",
    "mount",
    "heavy",
    "aio",
]


def case_prefix(case: str) -> str:
    match = re.match(r"([A-Za-z_]+?)(?:\d|_\d|$)", case)
    if match:
        return match.group(1).rstrip("_")
    return case


def refresh_cases() -> list[str]:
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["TMPDIR"] = str(CACHE_DIR)
    result = subprocess.run(
        [sys.executable, "tools/build-slim-sdcard.py", "--list-cases", "ltp-musl"],
        cwd=ROOT,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    CACHE_FILE.write_text(result.stdout)
    return parse_cases(result.stdout.splitlines())


def load_cases(refresh: bool) -> list[str]:
    if refresh or not CACHE_FILE.exists():
        return refresh_cases()
    return parse_cases(CACHE_FILE.read_text().splitlines())


def parse_cases(lines: list[str]) -> list[str]:
    cases = []
    for line in lines:
        stripped = line.strip()
        if not stripped or stripped.startswith("ltp-musl"):
            continue
        case = stripped.split()[0]
        if not is_valid_case_name(case):
            continue
        cases.append(case)
    return cases


def cases_for_batch(cases: list[str], batch: str) -> list[str]:
    if batch == "all":
        return cases
    try:
        prefixes = set(BATCH_PREFIXES[batch])
    except KeyError:
        known = ", ".join(BATCH_ORDER + ["all"])
        raise SystemExit(f"unknown LTP batch {batch!r}; known: {known}")
    return [
        case
        for case in cases
        if is_valid_case_name(case)
        and case_prefix(case) in prefixes
        and case not in SKIP_CASES
        and case_prefix(case) not in SKIP_PREFIXES
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description="List OSComp/LTP syscall batches")
    parser.add_argument("--batch", default=None, help="batch name to print")
    parser.add_argument("--csv", action="store_true", help="print comma-separated cases")
    parser.add_argument("--list", action="store_true", help="list batch names and counts")
    parser.add_argument("--refresh", action="store_true", help="refresh case cache from sdcard")
    args = parser.parse_args()

    cases = load_cases(args.refresh)

    if args.list:
        for name in BATCH_ORDER:
            selected = cases_for_batch(cases, name)
            print(f"{name:8s} {len(selected):4d}")
        print(f"{'all':8s} {len(cases):4d}")
        return 0

    batch = args.batch or "p0"
    selected = cases_for_batch(cases, batch)
    if args.csv:
        print(",".join(selected))
    else:
        for case in selected:
            print(case)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
