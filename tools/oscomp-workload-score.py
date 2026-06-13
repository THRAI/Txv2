#!/usr/bin/env python3
"""按内核能力覆盖面和测试宽度量化 OSComp 测试工作量。

这个脚本故意保持成一个小而可审计的模型，而不是依赖运行 trace。
它回答的问题是：如果每个测试套件都完整实现，相互之间需要多少内核能力面。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable


@dataclass(frozen=True)
class Capability:
    key: str
    label: str
    weight: float
    examples: str


@dataclass(frozen=True)
class Suite:
    key: str
    label: str
    surfaces: tuple[str, ...]
    strengths: dict[str, float]
    breadth: float
    note: str


@dataclass(frozen=True)
class LtpFunctionalTopic:
    key: str
    label: str
    cases: int
    capability_factor: float
    note: str


@dataclass(frozen=True)
class LtpStressGroup:
    key: str
    label: str
    cases: int
    weight: float
    note: str


CAPABILITIES: tuple[Capability, ...] = (
    Capability(
        "proc_basic",
        "进程生命周期",
        6,
        "fork, clone, execve, exit, wait, waitpid, pid/session basics",
    ),
    Capability(
        "loader_tls",
        "ELF/libc/TLS/动态加载",
        8,
        "ELF argv/env/auxv, dynamic linker, TLS, pthread startup",
    ),
    Capability(
        "fd_io",
        "fd 与字节 I/O",
        4,
        "open, close, read, write, dup, lseek, readv/writev",
    ),
    Capability(
        "vfs_basic",
        "基础 VFS 与目录",
        5,
        "stat, getdents, mkdir, unlink, rename, cwd, directory traversal",
    ),
    Capability(
        "vfs_deep",
        "深入 VFS 语义",
        9,
        "permissions, symlink edge cases, xattr, fcntl/flock locks, statx",
    ),
    Capability(
        "mount_fs",
        "挂载与文件系统管理",
        10,
        "mount, umount, chroot, readonly/bind semantics, new mount API",
    ),
    Capability(
        "vm_basic",
        "基础虚拟内存",
        6,
        "brk, mmap, munmap, page faults, file mappings",
    ),
    Capability(
        "vm_deep",
        "高级虚拟内存",
        11,
        "mprotect, mremap, madvise, mincore, mlock, hugepage, NUMA, userfaultfd",
    ),
    Capability(
        "time_basic",
        "基础时间接口",
        4,
        "gettimeofday, clock_gettime, nanosleep, times",
    ),
    Capability(
        "time_advanced",
        "定时器与 timerfd",
        7,
        "POSIX timers, timerfd, setitimer/getitimer, clock_nanosleep",
    ),
    Capability(
        "signal_basic",
        "基础信号处理",
        5,
        "kill, signal, sigaction, masks, signal delivery",
    ),
    Capability(
        "signal_rt",
        "实时信号语义",
        8,
        "rt_sigaction, rt_sigqueueinfo, sigtimedwait, sigaltstack, signalfd",
    ),
    Capability(
        "ipc_pipe_futex",
        "pipe/futex/事件就绪",
        7,
        "pipe, poll/select/epoll, futex, eventfd, readiness wakeups",
    ),
    Capability(
        "ipc_sysv_posix",
        "SysV/POSIX IPC",
        9,
        "msgget/msgrcv/msgsnd, semget/semop, shm*, POSIX mq",
    ),
    Capability(
        "sched_resource",
        "调度器与资源限制",
        7,
        "sched_yield, priorities, affinity, rlimit, rusage, prctl",
    ),
    Capability(
        "cred_security",
        "凭据与安全",
        10,
        "uid/gid, setuid, groups, capabilities, keyrings, DAC checks",
    ),
    Capability(
        "proc_dev_pseudo",
        "伪文件系统与设备",
        5,
        "/proc, /sys, devfs, tty, rtc/random, uname/sysinfo",
    ),
    Capability(
        "network_basic",
        "基础 socket API",
        10,
        "socket, bind, listen, accept, connect, send/recv, socketpair",
    ),
    Capability(
        "network_deep",
        "网络协议栈",
        13,
        "TCP/UDP behavior, options, IPv6, SCTP/DCCP, NFS/RPC, IPsec stress",
    ),
    Capability(
        "namespace_cgroup",
        "namespace 与 cgroup",
        13,
        "clone/unshare/setns namespaces, cgroup v1/v2 controllers",
    ),
    Capability(
        "trace_debug",
        "观测与调试接口",
        12,
        "ptrace, perf_event_open, bpf, ftrace, kcmp, process_vm",
    ),
    Capability(
        "aio_uring",
        "AIO and io_uring",
        12,
        "libaio syscalls, io_uring_setup/enter/register",
    ),
    Capability(
        "stress_perf",
        "压力/性能鲁棒性",
        6,
        "long-running benchmark, fuzz, crash, throughput, latency, contention",
    ),
)


def s(*items: str) -> tuple[str, ...]:
    return tuple(items)


SUITES: tuple[Suite, ...] = (
    Suite(
        "basic",
        "basic",
        s(
            "brk, chdir, clone, close, dup, dup2, execve, exit, fork",
            "fstat, getcwd, getdents, getpid, getppid, gettimeofday",
            "mkdir, mmap, munmap, mount, umount, open, openat",
            "pipe, read, write, sleep/nanosleep, times, uname, unlink",
            "wait, waitpid, sched_yield",
        ),
        {
            "proc_basic": 1.5,
            "fd_io": 1.5,
            "vfs_basic": 1.5,
            "mount_fs": 0.7,
            "vm_basic": 1.0,
            "time_basic": 1.0,
            "ipc_pipe_futex": 0.8,
            "sched_resource": 0.5,
            "proc_dev_pseudo": 0.5,
        },
        1.00,
        "小型 syscall 冒烟测试：覆盖不少核心调用，但主要是正常路径。",
    ),
    Suite(
        "busybox",
        "busybox",
        s(
            "ash/sh, fork/exec/wait, pipe and redirection",
            "touch, cat, cut, od, head, tail, hexdump, md5sum, sort, uniq",
            "stat, strings, wc, test -f, more, rm, mkdir, mv, rmdir, grep, cp, find",
            "date, df, dmesg, du, uname, uptime, ps, pwd, free, hwclock, kill, sleep",
            "/proc, /dev, tty, rtc/random-like device surface",
        ),
        {
            "proc_basic": 1.5,
            "fd_io": 2.0,
            "vfs_basic": 2.0,
            "vfs_deep": 0.8,
            "time_basic": 0.8,
            "signal_basic": 0.5,
            "ipc_pipe_futex": 1.0,
            "proc_dev_pseudo": 1.5,
            "stress_perf": 0.3,
        },
        1.30,
        "应用集成测试：覆盖 shell、VFS、proc/dev 与管道组合。",
    ),
    Suite(
        "lua",
        "lua",
        s(
            "Lua 解释器进程镜像",
            "date.lua, file_io.lua, random.lua, remove.lua, sort.lua, strings.lua",
            "通过 brk/mmap 触发分配器；文件 open/read/write/unlink；time/random",
        ),
        {
            "loader_tls": 0.5,
            "fd_io": 1.0,
            "vfs_basic": 0.8,
            "vm_basic": 0.7,
            "time_basic": 0.5,
            "proc_dev_pseudo": 0.2,
        },
        0.55,
        "解释器冒烟测试；有集成价值，但内核能力面较窄。",
    ),
    Suite(
        "libctest",
        "libctest",
        s(
            "220 个静态/动态 libc 测试",
            "argv/env, fdopen/stdio, clock_gettime/time, stat/statvfs, lseek_large",
            "pthread_cancel/cond/tsd/tls, dlopen, socket, sigprocmask, rlimit",
            "大量 string/printf/regex/locale 测试主要在 libc 内部完成",
        ),
        {
            "proc_basic": 0.8,
            "loader_tls": 2.5,
            "fd_io": 1.5,
            "vfs_basic": 1.0,
            "vm_basic": 1.5,
            "time_basic": 1.2,
            "signal_basic": 0.7,
            "ipc_pipe_futex": 1.5,
            "sched_resource": 0.8,
            "network_basic": 0.5,
        },
        1.80,
        "大型 libc ABI 检查；内核工作主要集中在 loader、pthread/futex、VM、fd、time。",
    ),
    Suite(
        "libcbench",
        "libcbench",
        s(
            "malloc sparse/bubble/tiny/big/thread stress",
            "pthread create/join, stdio putc/getc, string/regex benchmarks",
        ),
        {
            "loader_tls": 0.7,
            "vm_basic": 1.5,
            "ipc_pipe_futex": 0.8,
            "sched_resource": 0.7,
            "fd_io": 0.4,
            "stress_perf": 1.5,
        },
        0.85,
        "偏性能的 libc 路径；范围较窄，但能压 VM/线程相关路径。",
    ),
    Suite(
        "lmbench",
        "lmbench",
        s(
            "lat_syscall/read/write/stat/fstat/open-close",
            "select on 100 fds, signal install/handler overhead",
            "pipe latency/bandwidth, fork+exit, fork+execve, shell exec",
            "file bandwidth, pagefault, mmap read, context switch",
        ),
        {
            "proc_basic": 2.0,
            "fd_io": 1.8,
            "vfs_basic": 1.0,
            "vm_basic": 1.5,
            "time_basic": 0.5,
            "signal_basic": 1.0,
            "ipc_pipe_futex": 1.5,
            "sched_resource": 1.3,
            "stress_perf": 2.0,
        },
        1.20,
        "集中测试进程、fd、pipe、VM、调度器的延迟/吞吐。",
    ),
    Suite(
        "iozone",
        "iozone",
        s(
            "write/read, rewrite/reread, random read/write, reverse/stride read",
            "fwrite/fread, pwrite/pread, pwritev/preadv",
            "multi-process 4 worker file I/O throughput",
        ),
        {
            "proc_basic": 0.8,
            "fd_io": 2.5,
            "vfs_basic": 1.5,
            "vfs_deep": 0.8,
            "vm_basic": 0.7,
            "sched_resource": 0.5,
            "stress_perf": 2.2,
        },
        1.35,
        "较深入的文件 I/O 性能测试，但集中在一个子系统族。",
    ),
    Suite(
        "netperf",
        "netperf",
        s(
            "UDP_STREAM, TCP_STREAM, UDP_RR, TCP_RR, TCP_CRR",
            "netserver lifecycle plus socket options and localhost TCP/UDP",
        ),
        {
            "proc_basic": 0.8,
            "fd_io": 0.5,
            "network_basic": 2.2,
            "network_deep": 1.5,
            "ipc_pipe_futex": 0.5,
            "sched_resource": 0.5,
            "stress_perf": 1.5,
        },
        1.15,
        "网络栈集成/性能测试；case 数少，但缺网络时实现代价很高。",
    ),
    Suite(
        "iperf",
        "iperf",
        s(
            "基础 UDP/TCP、并行 UDP/TCP、反向 UDP/TCP",
            "throughput, retransmit/reporting, socket buffer/options",
        ),
        {
            "proc_basic": 0.6,
            "fd_io": 0.5,
            "network_basic": 2.0,
            "network_deep": 1.7,
            "sched_resource": 0.6,
            "stress_perf": 1.8,
        },
        1.15,
        "网络吞吐压力测试，流量形态比 netperf 略宽。",
    ),
    Suite(
        "cyclictest",
        "cyclictest",
        s(
            "NO_STRESS 与 STRESS 场景，1/8 个周期线程的延迟测试",
            "hackbench 进程压力，以及 timer/scheduler 延迟报告",
        ),
        {
            "proc_basic": 1.0,
            "time_basic": 0.8,
            "time_advanced": 1.0,
            "sched_resource": 2.0,
            "ipc_pipe_futex": 0.5,
            "stress_perf": 1.8,
        },
        1.00,
        "压力下的调度器/定时器延迟；范围窄，但语义重要。",
    ),
    Suite(
        "ltp",
        "完整 LTP",
        s(
            "67 个 runtest 文件共 4056 条 entry；2691 条功能测试，1365 条 perf/stress/fuzz",
            "ltp_view.pdf 给出的功能难度分布：L1 497，L2 1198，L3 688，L4 308",
            "覆盖 syscalls 以及 fs/mm/ipc/sched/net/cgroup/namespace/trace/security 等套件",
            "同时测试正常路径、errno、坏地址、权限、竞争、并发、清理等语义",
        ),
        {
            "proc_basic": 3.0,
            "loader_tls": 1.5,
            "fd_io": 3.0,
            "vfs_basic": 3.0,
            "vfs_deep": 3.0,
            "mount_fs": 2.8,
            "vm_basic": 3.0,
            "vm_deep": 3.0,
            "time_basic": 3.0,
            "time_advanced": 3.0,
            "signal_basic": 3.0,
            "signal_rt": 3.0,
            "ipc_pipe_futex": 3.0,
            "ipc_sysv_posix": 3.0,
            "sched_resource": 3.0,
            "cred_security": 3.0,
            "proc_dev_pseudo": 2.5,
            "network_basic": 3.0,
            "network_deep": 3.0,
            "namespace_cgroup": 3.0,
            "trace_debug": 3.0,
            "aio_uring": 2.5,
            "stress_perf": 3.0,
        },
        1.00,
        "完整 Linux 兼容测试面，包含高级语义和跨子系统语义。",
    ),
)


LTP_TIER_COUNTS: dict[str, int] = {
    # From ltp_view.pdf: functional complexity distribution.
    "L1": 497,
    "L2": 1198,
    "L3": 688,
    "L4": 308,
}

LTP_TIER_WEIGHTS: dict[str, float] = {
    # L1 normal path; L2 errno/variant; L3 cross-state/concurrency; L4 subsystem-heavy.
    "L1": 1.0,
    "L2": 1.8,
    "L3": 3.2,
    "L4": 5.2,
}

LTP_FUNCTIONAL_TOPICS: tuple[LtpFunctionalTopic, ...] = (
    LtpFunctionalTopic("proc_sched", "进程/调度", 186, 1.10, "fork/clone/exec/wait/sched/prctl/resource 语义"),
    LtpFunctionalTopic("signal", "信号", 52, 1.05, "实时信号投递、mask、wait、备用栈"),
    LtpFunctionalTopic("time", "时间", 54, 0.90, "clock、timer、sleep、timerfd/setitimer 行为"),
    LtpFunctionalTopic("ipc", "IPC", 177, 1.15, "SysV IPC、POSIX mq、pipe、事件就绪"),
    LtpFunctionalTopic("vm", "VM", 251, 1.25, "mmap/brk/mprotect/mremap/madvise/mincore/hugetlb/NUMA 边界情况"),
    LtpFunctionalTopic("fs", "文件系统", 663, 1.20, "open/stat/link/rename/xattr/lock/mount/read-only/bind 文件系统语义"),
    LtpFunctionalTopic("file_io", "文件 I/O", 220, 1.00, "read/write/pread/pwrite/direct I/O/sync/AIO 相关行为"),
    LtpFunctionalTopic("net", "网络", 399, 1.35, "socket API 以及 TCP/UDP/IPv6/SCTP/NFS/RPC/IPsec 族"),
    LtpFunctionalTopic("cred", "凭据/安全", 268, 1.25, "uid/gid/groups/capabilities/keyrings/DAC 检查"),
    LtpFunctionalTopic("device", "设备/伪文件系统", 60, 0.95, "pty、input、tpm、sysfs/procfs/dev 节点"),
    LtpFunctionalTopic("sysadmin", "系统管理", 51, 1.30, "mount、reboot/module/swap/quota/sysinfo 等特权 API"),
    LtpFunctionalTopic("namespaces", "namespaces", 88, 1.45, "clone/unshare/setns namespace 隔离与清理"),
    LtpFunctionalTopic("cgroup", "cgroup", 94, 1.45, "cgroup v1/v2 controller 与资源统计"),
    LtpFunctionalTopic("trace", "trace/debug", 43, 1.50, "ptrace/perf/bpf/ftrace/kcmp/process_vm 等接口"),
    LtpFunctionalTopic("misc", "杂项", 85, 1.00, "commands、crypto、CVE、kernel misc"),
)

LTP_STRESS_GROUPS: tuple[LtpStressGroup, ...] = (
    # From ltp_view.pdf perf/stress/fuzz grouping. Stress tests receive lower
    # per-entry weight than functional L2/L3 cases, but expensive subsystems
    # still count because they require robustness, cleanup, and long-run stability.
    LtpStressGroup("net_stress", "网络压力", 588, 1.20, "IPsec TCP/UDP/SCTP/DCCP/ICMP，route/interface/multicast 压力"),
    LtpStressGroup("disk_aio", "磁盘/AIO 压力", 364, 0.95, "growfiles/aiodio/direct I/O 吞吐与长时间清理"),
    LtpStressGroup("cgroup_stress", "cgroup 压力", 253, 1.15, "资源 controller 压力与清理"),
    LtpStressGroup("block_scsi", "block/SCSI 压力", 140, 0.80, "scsi_debug/block-device 类测试"),
    LtpStressGroup("vm_stress", "VM 压力", 11, 1.10, "内存压力与分页稳定性"),
    LtpStressGroup("other_stress", "其他 stress/fuzz", 9, 0.75, "剩余小规模 crash/fuzz/perf 分组"),
)

LTP_FULL_ENTRIES = 4056
LTP_FUNCTIONAL_ENTRIES = 2691
LTP_STRESS_ENTRIES = 1365

# Calibration: one LTP topic-adjusted difficulty unit is not one capability
# model point.  A normal LTP case usually checks a concrete syscall behavior
# plus setup/cleanup and pass/fail semantics.  About eight L1-normal-path case
# units are treated as one weighted capability unit; L2-L4 cases already carry
# higher tier weights above.
LTP_CASE_UNIT_TO_CAPABILITY_POINT = 1 / 8


def cap_map() -> dict[str, Capability]:
    return {cap.key: cap for cap in CAPABILITIES}


def suite_score(suite: Suite) -> float:
    caps = cap_map()
    return suite.breadth * sum(caps[key].weight * strength for key, strength in suite.strengths.items())


def ltp_functional_difficulty_points() -> float:
    tier_total = sum(LTP_TIER_COUNTS[tier] * LTP_TIER_WEIGHTS[tier] for tier in LTP_TIER_COUNTS)
    average_tier = tier_total / LTP_FUNCTIONAL_ENTRIES
    topic_total = sum(topic.cases * topic.capability_factor for topic in LTP_FUNCTIONAL_TOPICS)
    return topic_total * average_tier


def ltp_stress_difficulty_points() -> float:
    return sum(group.cases * group.weight for group in LTP_STRESS_GROUPS)


def ltp_score() -> float:
    return (ltp_functional_difficulty_points() + ltp_stress_difficulty_points()) * LTP_CASE_UNIT_TO_CAPABILITY_POINT


def score_rows(suites: Iterable[Suite]) -> list[tuple[Suite, float]]:
    rows: list[tuple[Suite, float]] = []
    for suite in suites:
        score = ltp_score() if suite.key == "ltp" else suite_score(suite)
        rows.append((suite, score))
    return rows


def non_ltp_union_score() -> float:
    """Estimate implementation work once overlapping non-LTP coverage is deduped.

    Multiple suites using open/read/write/fork/exec do not require reimplementing
    those syscalls from scratch.  The strongest coverage of a capability counts
    fully; additional suites still count a little because they add integration
    shape, stress, libc differences, and shell/application paths.
    """

    caps = cap_map()
    total = 0.0
    for cap in CAPABILITIES:
        effective_strengths = [
            suite.strengths[cap.key] * suite.breadth
            for suite in SUITES
            if suite.key != "ltp" and cap.key in suite.strengths
        ]
        if not effective_strengths:
            continue
        strongest = max(effective_strengths)
        overlap = sum(effective_strengths) - strongest
        total += caps[cap.key].weight * (strongest + 0.25 * overlap)
    return total


def recommended_points(total_non_ltp_points: float = 3000.0) -> dict[str, float]:
    rows = score_rows(SUITES)
    standalone_non_ltp_work = sum(score for suite, score in rows if suite.key != "ltp")
    scale = total_non_ltp_points / standalone_non_ltp_work
    return {suite.key: score * scale for suite, score in rows}


def recommended_ltp_total(total_non_ltp_points: float = 3000.0) -> float:
    return ltp_score() * total_non_ltp_points / non_ltp_union_score()


def markdown() -> str:
    rows = score_rows(SUITES)
    points = recommended_points()
    standalone_non_ltp_work = sum(score for suite, score in rows if suite.key != "ltp")
    union_non_ltp_work = non_ltp_union_score()
    ltp_work = dict((suite.key, score) for suite, score in rows)["ltp"]
    ltp_ratio = ltp_work / union_non_ltp_work
    ltp_total_points = recommended_ltp_total()

    out: list[str] = []
    out.append("# OSComp 测试工作量评分模型")
    out.append("")
    out.append("这份报告用内核能力覆盖面来量化完整 OSComp 测试的实现工作量。")
    out.append("")
    out.append("主要本地依据：")
    out.append("")
    out.append("- `ltp_view.pdf`：完整 LTP 的 topic 数量、L1-L4 难度分桶、stress/fuzz 分组。")
    out.append("- `Txv2/docs/ljs/LTP_IMAGE_TEST_CONTENTS_2026-06-03.md`：LTP 镜像清单，67 个 runtest 文件，4056 条 entry。")
    out.append("- `Txv2/external/oscomp-autotest/kernel/judge/*.py`：非 LTP OSComp 测试套件的命令和计分目标。")
    out.append("- `Txv2/docs/LTP/ltp-batches.md`：syscalls 子集规模和 LTP syscall-family 分组。")
    out.append("")
    out.append("非 LTP 分数 = 能力权重 x 覆盖强度 x suite 宽度。")
    out.append("LTP 分数 = `ltp_view.pdf` 中的 case 数量 x L1-L4 难度 x topic/stress 系数，再校准回同一套模型单位。")
    out.append("覆盖强度含义：0 表示不覆盖，1 表示正常路径，2 表示多场景/较宽覆盖，3 表示深入边界/并发/Linux 语义。")
    out.append("")
    out.append("## 能力权重")
    out.append("")
    out.append("| 能力 | 权重 | 例子 |")
    out.append("| --- | ---: | --- |")
    for cap in CAPABILITIES:
        out.append(f"| {cap.label} | {cap.weight:g} | {cap.examples} |")
    out.append("")
    out.append("## 测试套件分数")
    out.append("")
    out.append("| 测试套件 | 宽度 | 工作量单位 | 非 LTP 总池为 3000 时 | 说明 |")
    out.append("| --- | ---: | ---: | ---: | --- |")
    for suite, score in rows:
        breadth = "case 模型" if suite.key == "ltp" else f"{suite.breadth:g}"
        out.append(f"| {suite.label} | {breadth} | {score:.1f} | {points[suite.key]:.0f} | {suite.note} |")
    out.append("")
    out.append(f"非 LTP 各 suite 独立相加：`{standalone_non_ltp_work:.1f}` 模型单位。")
    out.append(f"非 LTP 按实现能力去重并折扣重叠后：`{union_non_ltp_work:.1f}` 模型单位。")
    out.append(f"完整 LTP 工作量：`{ltp_work:.1f}` 模型单位。")
    out.append(f"LTP / 去重后非 LTP 比值：`{ltp_ratio:.2f}x`。")
    out.append("")
    out.append("如果整个非 LTP 分池归一化为约 `3000` 分，按去重后的实现工作量模型，完整 LTP 对应 "
               f"`{ltp_total_points:.0f}` 分。")
    out.append("")
    out.append("敏感性检查：如果校准收紧为 7 个 L1 正常路径 case 单位对应 1 个能力单位，"
               f"LTP 约为 `{ltp_total_points * 8 / 7:.0f}` 分；如果放宽为 9 个，则约为 "
               f"`{ltp_total_points * 8 / 9:.0f}` 分。因此合理区间约为 "
               f"`{ltp_total_points * 8 / 9:.0f}-{ltp_total_points * 8 / 7:.0f}` 分。")
    out.append("")
    out.append("## 推荐 LTP 分值拆分")
    out.append("")
    out.append("这里的 `~10000` 分建议是四个完整 LTP 视图合计，不是每个视图各自 `10000` 分。")
    out.append("四个视图的完整 LTP case 清单基本相同：RV/LA 都包含同一批 runtest entry，musl/glibc 主要是在同一内核能力面之上测试不同 libc/loader/runtime 兼容路径。")
    out.append("如果每个视图都再给完整 `10000` 分，LTP 总池会变成 `40000` 分，会明显重复计算共享的内核实现工作量。")
    out.append("")
    out.append("| LTP 视图 | 推荐分值 | 理由 |")
    out.append("| --- | ---: | --- |")
    out.append("| `ltp-rv-musl` | 2500 | RV64 musl 的 LTP 基线兼容性。 |")
    out.append("| `ltp-rv-glibc` | 2500 | 同一 LTP 测试面上的 RV64 glibc/loader/runtime 兼容性。 |")
    out.append("| `ltp-la-musl` | 2500 | 同一 LTP 测试面上的 LA64 musl 架构移植兼容性。 |")
    out.append("| `ltp-la-glibc` | 2500 | 同一 LTP 测试面上的 LA64 glibc/loader/runtime 兼容性。 |")
    out.append("| **合计** | **10000** | 与模型中心值约 `10137` 分匹配。 |")
    out.append("")
    out.append("## LTP 内部计分建议")
    out.append("")
    out.append("LTP 内部不建议继续用原始 `TPASS` 数直接累加作为最终分数。原因是不同 case 的 `TPASS` 粒度差异很大：有些 case 只有 1 个 `TPASS`，但验证的是复杂权限、namespace、IPC 或 VM 语义；有些 case 会在循环里产生几十到几百个 `TPASS`，例如当前记录里 `splice07` 可到 591 个子项、`epoll_ctl03` 有 256 个子项、`access01` 有 199 个子项。直接按 `TPASS` 计分会奖励“断言数量多”，而不是奖励“实现难度高”。")
    out.append("")
    out.append("更合理的做法是两层计分：")
    out.append("")
    out.append("1. 先给每个 runtest entry 一个满分预算 `case_points(case)`。这个预算由 case 所属 LTP topic、L1-L4 难度、stress/fuzz 类型决定，四个视图各自归一化到 `2500` 分。")
    out.append("2. 再在 case 内部按通过比例拿分：`earned(case) = case_points(case) x min(passed / expected_passes, 1)`。如果无法预先知道 `expected_passes`，可以用官方参考日志或 x86/Linux 参考运行得到；临时实现也可以用本次日志里的 `passed + failed + broken + skipped + warnings` 作为上限估计，但最终应固定成参考表，避免分母随提交变化。")
    out.append("")
    out.append("推荐公式：")
    out.append("")
    out.append("```text")
    out.append("raw_weight(case) = tier_weight(case) x topic_factor(case) x stress_factor(case) x duplicate_factor(case)")
    out.append("case_points(case) = 2500 x raw_weight(case) / sum(raw_weight(all entries in this LTP view))")
    out.append("earned(case) = case_points(case) x min(passed(case) / expected_passes(case), 1)")
    out.append("ltp_view_score = sum(earned(case))")
    out.append("```")
    out.append("")
    out.append("建议权重：")
    out.append("")
    out.append("| 维度 | 权重/规则 | 说明 |")
    out.append("| --- | --- | --- |")
    out.append("| 难度层级 | L1=`1.0`，L2=`1.8`，L3=`3.2`，L4=`5.2` | 来自 `ltp_view.pdf` 的 L1-L4 分类；L3/L4 明显高于普通正常路径。 |")
    out.append("| topic 系数 | 使用本文 LTP topic 表中的系数 | fs/vm/net/cred/namespace/cgroup/trace 等高实现成本 topic 权重更高。 |")
    out.append("| stress/fuzz | 普通功能 case 为 `1.0`；stress/fuzz 按分组使用 `0.75-1.20` | stress 主要验证稳定性和清理，不应完全等同于功能语义，但网络/cgroup 压力仍然很贵。 |")
    out.append("| 重复 entry | 同一 tag 第一次 `1.0`，重复 entry 可取 `0.6-0.8` | 同一可执行文件不同参数仍有价值，但不要把完全重复 tag 当成全新实现面。 |")
    out.append("| case 内部 TPASS | 只用于该 case 预算内部分摊 | `TPASS` 多的 case 最多拿满自己的 `case_points`，不能因为断言多突破 case 满分。 |")
    out.append("")
    out.append("这样做后，每个 LTP 视图仍然可以继续解析日志里的 `TPASS` 数，但 `TPASS` 只是 case 内部的完成比例，不再是跨 case 的直接货币。举例：如果某个 L2/fs case 满分预算是 `0.8` 分，参考通过项是 20 个，本次通过 15 个，则得 `0.8 x 15/20 = 0.6` 分；如果某个 L4/namespace case 只有 1 个 `TPASS`，但满分预算是 `2.5` 分，通过后就拿 `2.5` 分。")
    out.append("")
    out.append("## LTP Case 模型")
    out.append("")
    out.append(f"- 完整 LTP runtest entry：`{LTP_FULL_ENTRIES}` = `{LTP_FUNCTIONAL_ENTRIES}` 条功能测试 + `{LTP_STRESS_ENTRIES}` 条 perf/stress/fuzz。")
    out.append("- `ltp_view.pdf` 给出的功能难度："
               + ", ".join(f"{tier} {count} x {LTP_TIER_WEIGHTS[tier]:g}" for tier, count in LTP_TIER_COUNTS.items())
               + f" => `{sum(LTP_TIER_COUNTS[tier] * LTP_TIER_WEIGHTS[tier] for tier in LTP_TIER_COUNTS):.1f}` 难度点。")
    out.append(f"- 功能测试经 topic 系数调整后的难度：`{ltp_functional_difficulty_points():.1f}` case-difficulty 单位。")
    out.append(f"- Stress/fuzz 难度：`{ltp_stress_difficulty_points():.1f}` case-difficulty 单位。")
    out.append(f"- 校准：`{1 / LTP_CASE_UNIT_TO_CAPABILITY_POINT:.0f}` 个 L1 正常路径 case 单位约等于 1 个非 LTP 能力单位。")
    out.append("- 非 LTP 汇总时会折扣重复 syscall 面：最强 suite 覆盖完整计入，额外重叠覆盖按 `25%` 计入。")
    out.append("")
    out.append("| LTP 功能 topic | Case 数 | 系数 | 工作量含义 |")
    out.append("| --- | ---: | ---: | --- |")
    for topic in LTP_FUNCTIONAL_TOPICS:
        out.append(f"| {topic.label} | {topic.cases} | {topic.capability_factor:g} | {topic.note} |")
    out.append("")
    out.append("| LTP stress/fuzz 分组 | Case 数 | 单 entry 权重 | 工作量含义 |")
    out.append("| --- | ---: | ---: | --- |")
    for group in LTP_STRESS_GROUPS:
        out.append(f"| {group.label} | {group.cases} | {group.weight:g} | {group.note} |")
    out.append("")
    out.append("## 各测试套件的 syscall/机制覆盖面")
    for suite, _ in rows:
        out.append("")
        out.append(f"### {suite.label}")
        out.append("")
        for surface in suite.surfaces:
            out.append(f"- {surface}")
    out.append("")
    out.append("## 覆盖矩阵")
    out.append("")
    header = "| 能力 | 权重 | " + " | ".join(suite.label for suite in SUITES) + " |"
    sep = "| --- | ---: | " + " | ".join("---:" for _ in SUITES) + " |"
    out.append(header)
    out.append(sep)
    for cap in CAPABILITIES:
        cells = [f"{suite.strengths.get(cap.key, 0):g}" for suite in SUITES]
        out.append(f"| {cap.label} | {cap.weight:g} | " + " | ".join(cells) + " |")
    out.append("")
    out.append("## 结论解释")
    out.append("")
    out.append("- `basic`、`busybox`、`lua` 建立正常路径 OS 基线。")
    out.append("- `libctest`、`libcbench` 增加 loader、TLS、pthread/futex、libc ABI、分配器压力。")
    out.append("- `iozone`、`lmbench`、`netperf`、`iperf`、`cyclictest` 都是重要的子系统压力测试，但各自比较聚焦。")
    out.append("- 完整 LTP 覆盖几乎所有 Linux syscall 家族，并额外测试 errno、权限、坏地址、并发、清理和高级子系统语义。")
    out.append("- 因此 LTP 不应该按原始 `TPASS` 数量直接计分。在这个模型里，完整 LTP 约等于整个非 LTP 分池的 "
               f"`{ltp_ratio:.1f}x`；如果非 LTP 是 `3000` 分，LTP 合计应约为 "
               f"`{ltp_total_points:.0f}` 分。")
    return "\n".join(out) + "\n"


def main() -> None:
    print(markdown())


if __name__ == "__main__":
    main()
