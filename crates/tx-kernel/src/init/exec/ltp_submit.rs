//! Preliminary-stage LTP submit whitelist runner.

use super::{append_ltp_walk_env, is_ltp_case_token, LtpArgs, LOCAL_LTP_SKIP_SHELL_PATTERN};

fn oscomp_libc_root(libc: &str) -> &'static str {
    match libc {
        "glibc" => "/musl/glibc",
        _ => "/musl/musl",
    }
}

fn oscomp_group_label(base: &str, libc: &str) -> alloc::string::String {
    use alloc::format;

    match libc {
        "glibc" => format!("{base}-glibc"),
        _ => format!("{base}-musl"),
    }
}

const LTP_NO_SYNTH_SUMMARY_SHELL: &str = "";

const LTP_NORMALIZE_LINE_SHELL: &str = r#"ltp_line=${ltp_line%"$tx_cr"}; "#;

const LTP_PRINT_LINE_SHELL: &str = r#"case "$ltp_line" in *TPASS:*) prefix=${ltp_line%%TPASS:*}; suffix=${ltp_line#*TPASS:}; printf '%s\033[1;32mTPASS: \033[0m%s\n' "$prefix" "$suffix";; *TFAIL:*) prefix=${ltp_line%%TFAIL:*}; suffix=${ltp_line#*TFAIL:}; printf '%s\033[1;31mTFAIL: \033[0m%s\n' "$prefix" "$suffix";; *TBROK:*) prefix=${ltp_line%%TBROK:*}; suffix=${ltp_line#*TBROK:}; printf '%s\033[1;31mTBROK: \033[0m%s\n' "$prefix" "$suffix";; *TCONF:*) prefix=${ltp_line%%TCONF:*}; suffix=${ltp_line#*TCONF:}; printf '%s\033[1;33mTCONF: \033[0m%s\n' "$prefix" "$suffix";; *TWARN:*) prefix=${ltp_line%%TWARN:*}; suffix=${ltp_line#*TWARN:}; printf '%s\033[1;35mTWARN: \033[0m%s\n' "$prefix" "$suffix";; *) echo "$ltp_line";; esac; "#;

const LTP_CASE_WORKDIR: &str = "/tmp";

fn append_ltp_script_env(cmd: &mut alloc::string::String, libc: &str) {
    append_ltp_walk_env(cmd, oscomp_libc_root(libc));
}

fn append_ltp_workdir(cmd: &mut alloc::string::String, libc: &str) {
    use core::fmt::Write as _;
    let _ = write!(cmd, "; cd {}", oscomp_libc_root(libc));
}

pub(super) fn append_submit_ltp_runner(
    cmd: &mut alloc::string::String,
    libc: &str,
    args: &LtpArgs<'_>,
) {
    use core::fmt::Write as _;

    append_ltp_script_env(cmd, libc);
    append_ltp_workdir(cmd, libc);
    let group = oscomp_group_label("ltp", libc);

    let _ = write!(cmd, "; echo \"#### OS COMP TEST GROUP START {group} ####\"");
    append_ltp_submit_case_loop(cmd, libc, args);
    let _ = write!(cmd, "; echo \"#### OS COMP TEST GROUP END {group} ####\"");
}

pub(super) fn append_batch_submit_ltp_runner(cmd: &mut alloc::string::String, args: &LtpArgs<'_>) {
    append_batch_submit_ltp_runner_for_libc(cmd, "musl", args);
    append_batch_submit_ltp_runner_for_libc(cmd, "glibc", args);
}

pub(super) fn append_batch_submit_ltp_runner_for_libc(
    cmd: &mut alloc::string::String,
    libc: &str,
    args: &LtpArgs<'_>,
) {
    use core::fmt::Write as _;

    append_ltp_script_env(cmd, libc);
    append_ltp_workdir(cmd, libc);
    let group = oscomp_group_label("ltp", libc);

    let _ = write!(cmd, "; echo \"#### OS COMP TEST GROUP START {group} ####\"");
    append_ltp_submit_case_loop(cmd, libc, args);
    let _ = write!(cmd, "; echo \"#### OS COMP TEST GROUP END {group} ####\"");
}

fn append_ltp_submit_case_loop(cmd: &mut alloc::string::String, libc: &str, args: &LtpArgs<'_>) {
    append_ltp_case_loop(cmd, libc, ltp_submit_cases_for_arch(), args);
}

fn append_ltp_case_loop(
    cmd: &mut alloc::string::String,
    libc: &str,
    filter: &str,
    args: &LtpArgs<'_>,
) {
    append_ltp_case_loop_until(cmd, libc, filter, None, args);
}

fn append_ltp_case_loop_until(
    cmd: &mut alloc::string::String,
    libc: &str,
    filter: &str,
    stop_before_case: Option<&str>,
    args: &LtpArgs<'_>,
) {
    append_ltp_case_loop_until_excluding(cmd, libc, filter, stop_before_case, "", args);
}

fn append_ltp_case_loop_until_excluding(
    cmd: &mut alloc::string::String,
    libc: &str,
    filter: &str,
    stop_before_case: Option<&str>,
    excluded_cases: &str,
    args: &LtpArgs<'_>,
) {
    append_ltp_case_loop_until_excluding_extra(
        cmd,
        libc,
        filter,
        stop_before_case,
        excluded_cases,
        "",
        args,
    );
}

fn append_ltp_case_loop_until_excluding_extra(
    cmd: &mut alloc::string::String,
    libc: &str,
    filter: &str,
    stop_before_case: Option<&str>,
    excluded_cases: &str,
    extra_excluded_cases: &str,
    args: &LtpArgs<'_>,
) {
    use core::fmt::Write as _;

    if libc == "glibc" {
        append_ltp_case_loop_direct(
            cmd,
            libc,
            filter,
            stop_before_case,
            excluded_cases,
            extra_excluded_cases,
            args,
        );
        return;
    }

    let root = oscomp_libc_root(libc);
    let _ = write!(cmd, "; for case in");
    for case in filter.split('+') {
        let case = case.trim();
        if case.is_empty() || case.ends_with('_') {
            continue;
        }
        if stop_before_case == Some(case) {
            break;
        }
        if ltp_case_list_contains(excluded_cases, case)
            || ltp_case_list_contains(extra_excluded_cases, case)
        {
            continue;
        }
        if !is_ltp_case_token(case) {
            let _ = write!(cmd, "; echo \"SKIP LTP CASE {case} : invalid case token\"");
            continue;
        }
        let _ = write!(cmd, " {case}");
    }
    let skip_pattern = LOCAL_LTP_SKIP_SHELL_PATTERN;
    let runtime_assignment = args.shell_max_runtime_assignment("case");
    let _ = write!(
        cmd,
        "; do \
case \"$case\" in {skip_pattern}) echo \"SKIP LTP CASE $case : local skip\"; continue;; esac; \
ltp_label=\"$case\"; \
case \"$case\" in \
chdir01A) set -- {root}/ltp/testcases/bin/symlink01 -T chdir01; ltp_label='symlink01 -T chdir01';; \
chmod01A) set -- {root}/ltp/testcases/bin/symlink01 -T chmod01; ltp_label='symlink01 -T chmod01';; \
link01) set -- {root}/ltp/testcases/bin/symlink01 -T link01; ltp_label='symlink01 -T link01';; \
lstat01A) set -- {root}/ltp/testcases/bin/symlink01 -T lstat01; ltp_label='symlink01 -T lstat01';; \
lstat01A_64) set -- {root}/ltp/testcases/bin/symlink01 -T lstat01_64; ltp_label='symlink01 -T lstat01_64';; \
open01A) set -- {root}/ltp/testcases/bin/symlink01 -T open01; ltp_label='symlink01 -T open01';; \
readlink01A) set -- {root}/ltp/testcases/bin/symlink01 -T readlink01; ltp_label='symlink01 -T readlink01';; \
rename01A) set -- {root}/ltp/testcases/bin/symlink01 -T rename01; ltp_label='symlink01 -T rename01';; \
rmdir03A) set -- {root}/ltp/testcases/bin/symlink01 -T rmdir03; ltp_label='symlink01 -T rmdir03';; \
stat04) set -- {root}/ltp/testcases/bin/symlink01 -T stat04; ltp_label='symlink01 -T stat04';; \
stat04_64) set -- {root}/ltp/testcases/bin/symlink01 -T stat04_64; ltp_label='symlink01 -T stat04_64';; \
unlink01) set -- {root}/ltp/testcases/bin/symlink01 -T unlink01; ltp_label='symlink01 -T unlink01';; \
*) set -- \"{root}/ltp/testcases/bin/$case\";; \
esac; \
{runtime_assignment} \
echo \"RUN LTP CASE $case : $ltp_label\"; \
summary_seen=0; passed=0; failed=0; broken=0; skipped=0; warnings=0; \
tx_cr=$(printf '\\r'); \
{{ cd {workdir} || cd {root}; if [ -n \"$ltp_max_runtime\" ]; then PATH={root}/ltp/testcases/bin:{root}/ltp/bin:{root}/ltp/testscripts:{root}:/musl/musl:$PATH TMPDIR={workdir} LTPROOT={root}/ltp KCONFIG_PATH=/proc/config \"$@\" -I \"$ltp_max_runtime\"; else PATH={root}/ltp/testcases/bin:{root}/ltp/bin:{root}/ltp/testscripts:{root}:/musl/musl:$PATH TMPDIR={workdir} LTPROOT={root}/ltp KCONFIG_PATH=/proc/config \"$@\"; fi; echo \"__TX_LTP_CASE_RET__:$?\"; }} 2>&1 | while IFS= read -r ltp_line; do {normalize_line_shell}case \"$ltp_line\" in __TX_LTP_CASE_RET__:*) ret=${{ltp_line#__TX_LTP_CASE_RET__:}}; {summary_shell}exit \"$ret\";; esac; {print_line_shell}case \"$ltp_line\" in Summary:) summary_seen=1;; *TPASS*) passed=$((passed + 1));; *TFAIL*) failed=$((failed + 1));; *TBROK*) broken=$((broken + 1));; *TCONF*) skipped=$((skipped + 1));; *TWARN*) warnings=$((warnings + 1));; esac; done; \
ret=$?; \
if [ $ret = 0 ]; then echo \"PASS LTP CASE $case : $ret\"; fi; \
echo \"FAIL LTP CASE $case : $ret\"; \
done",
        summary_shell = LTP_NO_SYNTH_SUMMARY_SHELL,
        normalize_line_shell = LTP_NORMALIZE_LINE_SHELL,
        print_line_shell = LTP_PRINT_LINE_SHELL,
        workdir = LTP_CASE_WORKDIR,
    );
}

fn append_ltp_case_loop_direct(
    cmd: &mut alloc::string::String,
    libc: &str,
    filter: &str,
    stop_before_case: Option<&str>,
    excluded_cases: &str,
    extra_excluded_cases: &str,
    args: &LtpArgs<'_>,
) {
    use core::fmt::Write as _;

    let root = oscomp_libc_root(libc);
    let _ = write!(cmd, "; for case in");
    for case in filter.split('+') {
        let case = case.trim();
        if case.is_empty() || case.ends_with('_') {
            continue;
        }
        if stop_before_case == Some(case) {
            break;
        }
        if ltp_case_list_contains(excluded_cases, case)
            || ltp_case_list_contains(extra_excluded_cases, case)
        {
            continue;
        }
        if !is_ltp_case_token(case) {
            let _ = write!(cmd, "; echo \"SKIP LTP CASE {case} : invalid case token\"");
            continue;
        }
        let _ = write!(cmd, " {case}");
    }
    let skip_pattern = LOCAL_LTP_SKIP_SHELL_PATTERN;
    let runtime_assignment = args.shell_max_runtime_assignment("case");
    let _ = write!(
        cmd,
        "; do \
case \"$case\" in {skip_pattern}) echo \"SKIP LTP CASE $case : local skip\"; continue;; esac; \
ltp_label=\"$case\"; \
case \"$case\" in \
chdir01A) set -- {root}/ltp/testcases/bin/symlink01 -T chdir01; ltp_label='symlink01 -T chdir01';; \
chmod01A) set -- {root}/ltp/testcases/bin/symlink01 -T chmod01; ltp_label='symlink01 -T chmod01';; \
link01) set -- {root}/ltp/testcases/bin/symlink01 -T link01; ltp_label='symlink01 -T link01';; \
lstat01A) set -- {root}/ltp/testcases/bin/symlink01 -T lstat01; ltp_label='symlink01 -T lstat01';; \
lstat01A_64) set -- {root}/ltp/testcases/bin/symlink01 -T lstat01_64; ltp_label='symlink01 -T lstat01_64';; \
open01A) set -- {root}/ltp/testcases/bin/symlink01 -T open01; ltp_label='symlink01 -T open01';; \
readlink01A) set -- {root}/ltp/testcases/bin/symlink01 -T readlink01; ltp_label='symlink01 -T readlink01';; \
rename01A) set -- {root}/ltp/testcases/bin/symlink01 -T rename01; ltp_label='symlink01 -T rename01';; \
rmdir03A) set -- {root}/ltp/testcases/bin/symlink01 -T rmdir03; ltp_label='symlink01 -T rmdir03';; \
stat04) set -- {root}/ltp/testcases/bin/symlink01 -T stat04; ltp_label='symlink01 -T stat04';; \
stat04_64) set -- {root}/ltp/testcases/bin/symlink01 -T stat04_64; ltp_label='symlink01 -T stat04_64';; \
unlink01) set -- {root}/ltp/testcases/bin/symlink01 -T unlink01; ltp_label='symlink01 -T unlink01';; \
*) set -- \"{root}/ltp/testcases/bin/$case\";; \
esac; \
{runtime_assignment} \
echo \"RUN LTP CASE $case : $ltp_label\"; \
cd {workdir} || cd {root}; \
if [ -n \"$ltp_max_runtime\" ]; then TMPDIR={workdir} \"$@\" -I \"$ltp_max_runtime\"; else TMPDIR={workdir} \"$@\"; fi; \
ret=$?; \
if [ $ret = 0 ]; then echo \"PASS LTP CASE $case : $ret\"; else echo \"FAIL LTP CASE $case : $ret\"; fi; \
if [ $ret = 0 ]; then echo \"FAIL LTP CASE $case : $ret\"; fi; \
done",
        workdir = LTP_CASE_WORKDIR,
    );
}

fn ltp_case_list_contains(list: &str, needle: &str) -> bool {
    list.split('+').any(|case| case.trim() == needle)
}

// Final RV64 LTP submit whitelist. Keep this as the single RV source of truth.
#[allow(dead_code)]
const LTP_SUBMIT_RV_CASES: &str = "\
epoll_ctl03+splice07+access01+getpid01+waitpid01+pipe11+timer_settime02+clock_getres01+\
posix_fadvise03+posix_fadvise03_64+confstr01+timer_settime01+signal03+signal05+getitimer01+\
mq_timedsend01+signal04+name_to_handle_at01+mq_timedreceive01+chmod01+open11+semop02+ppoll01+\
llseek03+personality01+setitimer01+pathconf01+setregid03+select03+semctl07+shmctl02+getrlimit01+\
getrlimit03+readahead01+select02+mmap04+msgctl01+msgctl04+access02+getdents02+stat01+stat01_64+\
setreuid05+futex_wake03+msgrcv07+gettid02+clock_nanosleep01+readv01+clock_gettime02+link04+\
readlinkat01+setregid04+setresuid01+epoll_ctl02+epoll_wait06+lseek02+fpathconf01+getrandom03+\
name_to_handle_at02+open_by_handle_at01+fallocate03+preadv02+preadv02_64+preadv202+preadv202_64+\
writev07+semctl01+sched_setscheduler01+timer_delete01+fchmod01+mmap06+setreuid01+setreuid02+\
epoll_wait02+futex_wait05+poll02+pipe2_01+pwritev02+pwritev02_64+pwritev202+pwritev202_64+\
clock_nanosleep02+nanosleep01+times03+mknod01+open_by_handle_at02+readlink03+unlinkat01+capget01+\
setresgid02+futex_wake01+select01+dup202+fcntl02+fcntl02_64+fcntl05+fcntl05_64+posix_fadvise01+\
posix_fadvise01_64+posix_fadvise02+posix_fadvise02_64+posix_fadvise04+posix_fadvise04_64+preadv201+\
preadv201_64+pwritev201+pwritev201_64+writev01+sethostname02+mq_notify01+msgget02+semctl03+semget02+\
kcmp02+alarm02+timerfd02+chown05+creat01+creat08+fchmodat01+flock04+fstat02+fstat02_64+open10+\
readlinkat02+madvise01+setregid01+epoll_wait03+epoll_wait07+eventfd02+pwrite02+pwrite02_64+\
sendfile04+sendfile04_64+sync_file_range01+mq_open01+shmctl08+kcmp01+faccessat201+fchmodat02+\
statx03+truncate03+truncate03_64+unlink07+setegid01+setresuid02+setreuid03+eventfd01+futex_wait01+\
select04+dup201+dup203+dup204+fcntl13+fcntl13_64+fcntl30+fcntl30_64+ioctl_ns07+lseek01+readv02+\
sendfile03+sendfile03_64+msgrcv02+semctl09+semop01+shmat01+getpgid01+pidfd_send_signal02+\
sched_getaffinity01+sched_setaffinity01+getrandom01+getrandom02+clock_nanosleep04+timerfd_settime01+\
flock06+lstat02+lstat02_64+stat03+stat03_64+statx02+mlock01+mlock201+mlock202+munlock01+\
remap_file_pages02+capset01+setreuid04+epoll_ctl01+epoll_wait01+eventfd03+eventfd04+pselect02+\
pselect02_64+close01+dup07+dup3_02+fcntl29+fcntl29_64+pread02+pread02_64+preadv01+preadv01_64+\
pwritev01+pwritev01_64+read02+write05+mq_unlink01+msgctl12+semctl05+semget01+shmat02+shmget04+\
setns01+clone08+execve03+pidfd_getfd02+pidfd_open02+getrusage02+membarrier01+sigwait01+syscall01+\
alarm05+getitimer02+nanosleep04+setitimer02+timer_gettime01+timerfd01+timerfd_gettime01+chmod03+\
faccessat01+flock01+flock02+ftruncate03+ftruncate03_64+getcwd01+mlock02+mmap09+mremap06+setgid03+\
setresuid05+epoll_create01+epoll_create1_01+epoll_create1_02+eventfd05+eventfd2_01+eventfd2_02+\
eventfd2_03+futex_wait_bitset01+poll01+copy_file_range03+dup01+dup02+dup04+dup207+dup3_01+\
fcntl15_64+fcntl15+fcntl27+fcntl27_64+fsync03+llseek02+lseek07+pipe03+sendfile02+sendfile02_64+\
write02+write06+sethostname01+uname01+msgctl03+msgctl06+msgrcv01+semctl04+shmdt02+clone01+fork01+\
fork10+getpgid02+getpgrp01+getpid02+gettid01+setpgrp02+waitpid03+waitpid04+getrlimit02+getrusage01+\
sigaltstack02+getrandom05+memcmp01+memcpy01+alarm03+alarm06+alarm07+gettimeofday01+nanosleep02+\
time01+timer_getoverrun01+timerfd_create01+chown02+faccessat02+faccessat202+fstat03+fstat03_64+\
fstatfs02+fstatfs02_64+ftruncate01+ftruncate01_64+mknod02+open01+open08+open09+readlink01A+stat02+\
stat02_64+symlink04+truncate02+truncate02_64+unlink05+unlink08+madvise10+mlock05+munlockall01+\
capset04+getegid02+getegid02_16+geteuid01+getgid01+getgid03+getuid01+setgid01+setuid01+setreuid06+\
setreuid07+setuid03+setuid04+epoll_ctl04+epoll_ctl05+futex_cmp_requeue02+futex_wait02+futex_wait04+\
pselect03+pselect03_64+close02+dup03+dup05+dup06+dup205+dup206+fcntl03+fcntl03_64+fcntl04+\
fcntl04_64+fcntl08+fcntl08_64+fcntl12+fcntl12_64+fsync02+pipe01+pipe06+pipe08+pipe10+pipe14+pread01+\
pread01_64+pwrite01+pwrite01_64+pwrite03+pwrite03_64+pwrite04+pwrite04_64+read01+read04+write03+\
getdomainname01+uname02+uname04+gettimeofday02+timer_delete02+timer_settime03+times01+chmod07+\
chown01+creat03+creat05+fchdir01+fchdir02+fchmod02+fchmod03+fchmod04+fchmod05+flock03+getcwd03+\
mkdir05+open03+open04+readdir01+rmdir01+symlink02+umask01+madvise05+mlock03+mlock04+mlock203+mmap02+\
mmap08+mmap15+mmap17+mmap19+mmap20+mprotect05+munlock02+sbrk02+getpriority01+getpriority02+nice01+\
nice02+nice03+nice04+prctl01+prctl09+sched_get_priority_max01+sched_get_priority_max02+\
sched_get_priority_min01+sched_get_priority_min02+sched_rr_get_interval01+setpriority02+wait402+\
wait02+wait01+shmat04+sendfile08_64+sendfile08+sendfile06_64+sendfile06+sendfile05_64+sendfile05+\
semop04+semctl02+pidfd_open01+personality02+msgrcv08+msgget01+mknod09+kill06+getsid02+getsid01+\
getppid02+getppid01+fork08+fork07+fork03+exit02+setrlimit04+setrlimit05+clone07+clone06+clone05+\
clone03+access03+brk01+brk02+capget02+chdir04+chmod05+chown03+clone04+clone302+close_range02+\
creat04+epoll_create02+epoll_pwait02+epoll_pwait03+epoll_pwait05+epoll_wait04+execl01+execle01+\
execlp01+execv01+execve01+execve02+execve05+execve06+execvp01+fchown01+fchown02+fchown03+fchown05+\
fork04+getcpu01+getcwd02+geteuid02+gethostname01+getpagesize01+getrandom04+getuid03+\
inotify_init1_01+inotify_init1_02+ioprio_get01+ioprio_set03+io_uring01+kill03+kill05+link02+\
llseek01+madvise02+memfd_create02+memset01+mincore02+mincore03+mkdir04+msgctl02+msgsnd01+open02+\
open07+pathconf02+pause01+pidfd_getfd01+pidfd_open04+pipe02+pipe07+pipe13+pipe2_02+pipe2_04+prctl02+\
prctl03+prctl05+prctl08+rename09+rmdir03+rt_sigsuspend01+sbrk01+sched_getparam01+sched_getparam03+\
sched_getscheduler01+sched_getscheduler02+sched_rr_get_interval02+sched_rr_get_interval03+\
sched_setparam01+sched_setparam02+sched_setparam03+sched_setparam04+sched_setparam05+\
sched_setscheduler02+sched_setscheduler04+semop03+setegid02+setgid02+setgroups01+setgroups02+\
setpgid03+setregid02+setresgid03+setresuid03+setresuid04+setrlimit02+setrlimit03+shmctl07+shmdt01+\
sighold02+signal01+signal02+splice03+splice04+statfs02+statfs02_64+tee02+tgkill03+tkill02+unshare02+\
vmsplice02+waitid04+waitid05+waitid06+waitpid06+waitpid09+waitpid10+waitpid12+fanotify04+fanotify08+\
write01+clock_settime01+clock_settime02+settimeofday01+settimeofday02+stime01+stime02+socket01+\
getsockname01+setsockopt01+sendto02+accept01+accept03+setsockopt03+bind04+bind05+socketpair01+recvmsg01+getsockopt01+accept4_01+getpeername01+fcntl36_64+fcntl36+bind01+in6_01+socketpair02+socket02+sendmmsg02+sendmmsg01+send02+bind03+utsname04+utsname02+setsockopt02+setgroups03+semtest_2ns+utsname01+thp01+tgkill01+shmnstest+shmem_2nstest+shm_comm+setsockopt10+setsockopt04+sem_nstest+sem_comm+recvmsg03+recvmsg02+recvmmsg01+mqns_02+mqns_01+mmapstress04+mmapstress01+mesgq_nstest+getsockopt02+futex_wait03+fsx-linux+fork_procs+cve-2017-17052+bind02+\
fs_bind01.sh+fs_bind02.sh+fs_bind03.sh+fs_bind04.sh+fs_bind05.sh+fs_bind06.sh+fs_bind07.sh+\
fs_bind07-2.sh+fs_bind08.sh+fs_bind09.sh+fs_bind10.sh+fs_bind11.sh+fs_bind12.sh+fs_bind13.sh+\
fs_bind14.sh+fs_bind15.sh+fs_bind16.sh+fs_bind17.sh+fs_bind18.sh+fs_bind19.sh+fs_bind20.sh+\
fs_bind21.sh+fs_bind22.sh+fs_bind23.sh+fs_bind24.sh+\
fs_bind_rbind01.sh+fs_bind_rbind02.sh+fs_bind_rbind03.sh+fs_bind_rbind04.sh+fs_bind_rbind05.sh+\
fs_bind_rbind06.sh+fs_bind_rbind07.sh+fs_bind_rbind07-2.sh+fs_bind_rbind08.sh+fs_bind_rbind09.sh+\
fs_bind_rbind10.sh+fs_bind_rbind11.sh+fs_bind_rbind12.sh+fs_bind_rbind13.sh+fs_bind_rbind14.sh+\
fs_bind_rbind15.sh+fs_bind_rbind16.sh+fs_bind_rbind17.sh+fs_bind_rbind18.sh+fs_bind_rbind19.sh+\
fs_bind_rbind20.sh+fs_bind_rbind21.sh+fs_bind_rbind22.sh+fs_bind_rbind23.sh+fs_bind_rbind24.sh+\
fs_bind_rbind25.sh+fs_bind_rbind26.sh+fs_bind_rbind27.sh+fs_bind_rbind28.sh+fs_bind_rbind29.sh+\
fs_bind_rbind30.sh+fs_bind_rbind31.sh+fs_bind_rbind32.sh+fs_bind_rbind33.sh+fs_bind_rbind34.sh+\
fs_bind_rbind35.sh+fs_bind_rbind36.sh+fs_bind_rbind37.sh+fs_bind_rbind38.sh+fs_bind_rbind39.sh+\
fs_bind_move01.sh+fs_bind_move02.sh+fs_bind_move03.sh+fs_bind_move04.sh+fs_bind_move05.sh+\
fs_bind_move06.sh+fs_bind_move07.sh+fs_bind_move08.sh+fs_bind_move09.sh+fs_bind_move10.sh+\
fs_bind_move11.sh+fs_bind_move12.sh+fs_bind_move13.sh+fs_bind_move14.sh+fs_bind_move15.sh+\
fs_bind_move16.sh+fs_bind_move17.sh+fs_bind_move18.sh+fs_bind_move19.sh+fs_bind_move20.sh+\
fs_bind_move21.sh+fs_bind_move22.sh";

// Final LA64 LTP submit whitelist. Keep this as the single LA source of truth.
#[allow(dead_code)]
const LTP_SUBMIT_LA_CASES: &str = "\
epoll_ctl03+splice07+access01+getpid01+waitpid01+pipe11+timer_settime02+clock_getres01+\
posix_fadvise03+posix_fadvise03_64+confstr01+timer_settime01+signal03+signal05+getitimer01+\
mq_timedsend01+signal04+name_to_handle_at01+mq_timedreceive01+chmod01+open11+semop02+ppoll01+\
llseek03+personality01+setitimer01+pathconf01+setregid03+select03+semctl07+shmctl02+getrlimit01+\
getrlimit03+readahead01+select02+mmap04+msgctl01+msgctl04+access02+getdents02+stat01+stat01_64+\
setreuid05+futex_wake03+msgrcv07+clock_nanosleep01+readv01+clock_gettime02+link04+readlinkat01+\
setregid04+setresuid01+epoll_ctl02+epoll_wait06+lseek02+fpathconf01+getrandom03+name_to_handle_at02+\
open_by_handle_at01+fallocate03+preadv02+preadv02_64+preadv202+preadv202_64+writev07+semctl01+\
sched_setscheduler01+timer_delete01+fchmod01+mmap06+setreuid01+setreuid02+epoll_wait02+futex_wait05+\
poll02+pipe2_01+pwritev02+pwritev02_64+pwritev202+pwritev202_64+clock_nanosleep02+nanosleep01+\
times03+mknod01+open_by_handle_at02+readlink03+unlinkat01+capget01+setresgid02+futex_wake01+\
select01+dup202+fcntl02+fcntl02_64+fcntl05+fcntl05_64+posix_fadvise01+posix_fadvise01_64+\
posix_fadvise02+posix_fadvise02_64+posix_fadvise04+posix_fadvise04_64+preadv201+preadv201_64+\
pwritev201+pwritev201_64+writev01+sethostname02+msgget02+semctl03+semget02+kcmp02+alarm02+timerfd02+\
chown05+creat01+creat08+fchmodat01+flock04+fstat02+fstat02_64+open10+readlinkat02+madvise01+\
setregid01+epoll_wait03+epoll_wait07+eventfd02+pwrite02+pwrite02_64+sendfile04+sendfile04_64+\
sync_file_range01+mq_open01+shmctl08+kcmp01+faccessat201+fchmodat02+statx03+truncate03+\
truncate03_64+unlink07+setegid01+setresuid02+setreuid03+eventfd01+futex_wait01+select04+dup201+\
dup203+dup204+fcntl13+fcntl13_64+fcntl30+fcntl30_64+ioctl_ns07+lseek01+readv02+sendfile03+\
sendfile03_64+msgrcv02+semctl09+semop01+shmat01+getpgid01+pidfd_send_signal02+sched_getaffinity01+\
sched_setaffinity01+getrandom01+getrandom02+clock_nanosleep04+timerfd_settime01+flock06+lstat02+\
lstat02_64+stat03+stat03_64+statx02+mlock01+mlock201+mlock202+munlock01+remap_file_pages02+capset01+\
setreuid04+epoll_ctl01+epoll_wait01+eventfd03+eventfd04+pselect02+pselect02_64+close01+dup07+\
dup3_02+fcntl29+fcntl29_64+pread02+pread02_64+preadv01+preadv01_64+pwritev01+pwritev01_64+read02+\
write05+mq_unlink01+msgctl12+semctl05+semget01+shmat02+shmget04+setns01+clone08+execve03+\
pidfd_getfd02+pidfd_open02+getrusage02+membarrier01+sigwait01+syscall01+alarm05+getitimer02+\
nanosleep04+setitimer02+timer_gettime01+timerfd01+timerfd_gettime01+chmod03+faccessat01+flock01+\
flock02+ftruncate03+ftruncate03_64+getcwd01+mlock02+mmap09+mremap06+setgid03+setresuid05+\
epoll_create01+epoll_create1_01+epoll_create1_02+eventfd05+eventfd2_01+eventfd2_02+eventfd2_03+\
futex_wait_bitset01+poll01+copy_file_range03+dup01+dup02+dup04+dup207+dup3_01+fcntl15_64+fcntl15+\
fcntl27+fcntl27_64+fsync03+llseek02+lseek07+pipe03+sendfile02+sendfile02_64+write02+write06+\
sethostname01+uname01+msgctl03+msgctl06+msgrcv01+semctl04+shmdt02+clone01+fork01+fork10+getpgid02+\
getpgrp01+getpid02+gettid01+setpgrp02+waitpid03+waitpid04+getrlimit02+getrusage01+sigaltstack02+\
getrandom05+memcmp01+memcpy01+alarm03+alarm06+alarm07+gettimeofday01+nanosleep02+time01+\
timer_getoverrun01+timerfd_create01+chown02+faccessat02+faccessat202+fstat03+fstat03_64+fstatfs02+\
fstatfs02_64+ftruncate01+ftruncate01_64+mknod02+open01+open08+open09+readlink01+readlink01A+stat02+\
stat02_64+symlink04+truncate02+truncate02_64+unlink05+unlink08+madvise10+mlock05+munlockall01+\
capset04+getegid02+getegid02_16+geteuid01+getgid01+getgid03+getuid01+setgid01+setuid01+setreuid06+\
setreuid07+setuid03+setuid04+epoll_ctl04+epoll_ctl05+futex_cmp_requeue02+futex_wait02+futex_wait04+\
pselect03+pselect03_64+close02+dup03+dup05+dup06+dup205+dup206+fcntl03+fcntl03_64+fcntl04+\
fcntl04_64+fcntl08+fcntl08_64+fcntl12+fcntl12_64+fsync02+pipe01+pipe06+pipe08+pipe10+pipe14+pread01+\
pread01_64+pwrite01+pwrite01_64+pwrite03+pwrite03_64+pwrite04+pwrite04_64+read01+read04+write03+\
getdomainname01+uname02+uname04+gettimeofday02+timer_delete02+timer_settime03+times01+chmod07+\
chown01+creat03+creat05+fchdir01+fchdir02+fchmod02+fchmod03+fchmod04+fchmod05+flock03+getcwd03+\
mkdir05+open03+open04+readdir01+rmdir01+symlink02+umask01+madvise05+mlock03+mlock04+mlock203+mmap02+\
mmap08+mmap15+mmap17+mmap19+mmap20+mprotect05+munlock02+sbrk02+getpriority01+getpriority02+nice01+\
nice02+nice03+nice04+prctl01+prctl09+sched_get_priority_max01+sched_get_priority_max02+\
sched_get_priority_min01+sched_get_priority_min02+sched_rr_get_interval01+setpriority02+wait402+\
wait02+wait01+shmat04+sendfile08_64+sendfile08+sendfile06_64+sendfile06+sendfile05_64+sendfile05+\
semop04+semctl02+pidfd_open01+personality02+msgrcv08+msgget01+mknod09+kill06+getsid02+getsid01+\
getppid02+getppid01+fork08+fork07+fork03+exit02+setrlimit04+setrlimit05+clone07+clone06+clone05+\
clone03+access03+brk01+brk02+capget02+chdir04+chmod05+chown03+clone04+clone302+close_range02+\
creat04+epoll_create02+epoll_pwait02+epoll_pwait03+epoll_pwait05+epoll_wait04+execl01+execle01+\
execlp01+execv01+execve01+execve02+execve06+execvp01+exit_group01+fchown01+fchown02+fchown03+\
fchown05+getcpu01+getcwd02+geteuid02+gethostname01+getpagesize01+getrandom04+getuid03+\
inotify_init1_01+inotify_init1_02+ioprio_get01+ioprio_set03+io_uring01+kill03+kill05+link02+\
llseek01+madvise02+memfd_create02+memset01+mincore02+mincore03+mkdir04+msgctl02+msgsnd01+open02+\
open07+pathconf02+pause01+pidfd_getfd01+pidfd_open04+pipe07+pipe13+pipe2_02+pipe2_04+prctl02+\
prctl03+prctl05+prctl08+rename09+rmdir03+rt_sigsuspend01+sbrk01+sched_getparam01+sched_getparam03+\
sched_getscheduler01+sched_getscheduler02+sched_rr_get_interval02+sched_rr_get_interval03+\
sched_setparam01+sched_setparam02+sched_setparam03+sched_setparam04+sched_setparam05+\
sched_setscheduler02+semop03+setegid02+setgid02+setgroups01+setgroups02+setpgid03+setregid02+\
setresgid03+setresuid03+setresuid04+setrlimit02+setrlimit03+shmctl07+shmdt01+sighold02+signal01+\
signal02+splice03+splice04+statfs02+statfs02_64+tee02+unshare02+vmsplice02+waitid04+waitid05+\
waitid06+waitpid06+waitpid09+waitpid10+waitpid12+fanotify04+fanotify08+write01+clock_settime01+\
clock_settime02+settimeofday01+settimeofday02+stime01+stime02+socket01+getsockname01+setsockopt01+\
sendto02+accept01+accept03+setsockopt03+bind04+bind05+socketpair01+recvmsg01+getsockopt01+accept4_01+getpeername01+fcntl36_64+fcntl36+bind01+in6_01+socketpair02+socket02+sendmmsg02+sendmmsg01+send02+bind03+utsname04+utsname02+setsockopt02+setgroups03+semtest_2ns+utsname01+thp01+tgkill01+shmnstest+shmem_2nstest+shm_comm+setsockopt10+setsockopt04+sem_nstest+sem_comm+recvmsg03+recvmsg02+recvmmsg01+mqns_02+mqns_01+mmapstress04+mmapstress01+mesgq_nstest+getsockopt02+futex_wait03+fsx-linux+fork_procs+fcntl34_64+fcntl34+cve-2017-17052+bind02+\
fs_bind01.sh+fs_bind02.sh+fs_bind03.sh+fs_bind04.sh+fs_bind05.sh+fs_bind06.sh+fs_bind07.sh+\
fs_bind07-2.sh+fs_bind08.sh+fs_bind09.sh+fs_bind10.sh+fs_bind11.sh+fs_bind12.sh+fs_bind13.sh+\
fs_bind14.sh+fs_bind15.sh+fs_bind16.sh+fs_bind17.sh+fs_bind18.sh+fs_bind19.sh+fs_bind20.sh+\
fs_bind21.sh+fs_bind22.sh+fs_bind23.sh+fs_bind24.sh+\
fs_bind_rbind01.sh+fs_bind_rbind02.sh+fs_bind_rbind03.sh+fs_bind_rbind04.sh+fs_bind_rbind05.sh+\
fs_bind_rbind06.sh+fs_bind_rbind07.sh+fs_bind_rbind07-2.sh+fs_bind_rbind08.sh+fs_bind_rbind09.sh+\
fs_bind_rbind10.sh+fs_bind_rbind11.sh+fs_bind_rbind12.sh+fs_bind_rbind13.sh+fs_bind_rbind14.sh+\
fs_bind_rbind15.sh+fs_bind_rbind16.sh+fs_bind_rbind17.sh+fs_bind_rbind18.sh+fs_bind_rbind19.sh+\
fs_bind_rbind20.sh+fs_bind_rbind21.sh+fs_bind_rbind22.sh+fs_bind_rbind23.sh+fs_bind_rbind24.sh+\
fs_bind_rbind25.sh+fs_bind_rbind26.sh+fs_bind_rbind27.sh+fs_bind_rbind28.sh+fs_bind_rbind29.sh+\
fs_bind_rbind30.sh+fs_bind_rbind31.sh+fs_bind_rbind32.sh+fs_bind_rbind33.sh+fs_bind_rbind34.sh+\
fs_bind_rbind35.sh+fs_bind_rbind36.sh+fs_bind_rbind37.sh+fs_bind_rbind38.sh+fs_bind_rbind39.sh+\
fs_bind_move01.sh+fs_bind_move02.sh+fs_bind_move03.sh+fs_bind_move04.sh+fs_bind_move05.sh+\
fs_bind_move06.sh+fs_bind_move07.sh+fs_bind_move08.sh+fs_bind_move09.sh+fs_bind_move10.sh+\
fs_bind_move11.sh+fs_bind_move12.sh+fs_bind_move13.sh+fs_bind_move14.sh+fs_bind_move15.sh+\
fs_bind_move16.sh+fs_bind_move17.sh+fs_bind_move18.sh+fs_bind_move19.sh+fs_bind_move20.sh+\
fs_bind_move21.sh+fs_bind_move22.sh";

#[cfg(target_arch = "loongarch64")]
fn ltp_submit_cases_for_arch() -> &'static str {
    LTP_SUBMIT_LA_CASES
}

#[cfg(not(target_arch = "loongarch64"))]
fn ltp_submit_cases_for_arch() -> &'static str {
    LTP_SUBMIT_RV_CASES
}

// End generated LTP syscall batch case lists.
