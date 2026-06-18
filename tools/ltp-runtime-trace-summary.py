#!/usr/bin/env python3
"""Summarize Tx native LTP runtime command traces.

Consumes serial logs containing TX-LTP-CMDWRAP / TX-LTP-SHIM begin-end
markers and reports argv-aware command costs. The post-stress loop view is
aimed at net.tcp_cmds ipneigh01_{ip,arp}, where each iteration starts with a
ping and then runs a small command/pipeline sequence.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import statistics
import sys
from dataclasses import dataclass, field
from pathlib import Path


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
BEGIN_RE = re.compile(r"TX-LTP-(CMDWRAP|SHIM) begin (\S+) (\d+)(?:\s+(.*))?$")
END_RE = re.compile(r"TX-LTP-(CMDWRAP|SHIM) end (\S+) (-?\d+) (\d+)\s*$")
PHASE_RE = re.compile(r"TX-LTP-(CMDWRAP|SHIM) phase (\S+) (\d+)(?:\s+(.*))?$")
RUNTIME_BEGIN_RE = re.compile(r"TX-LTP-RUNTIME begin (\S+) (\d+)")
RUNTIME_END_RE = re.compile(r"TX-LTP-RUNTIME end (\S+) (-?\d+) (\d+)")
STRESS_RE = re.compile(r"stress auto-creation ARP cache entry deleted with '([^']+)'")
PROFILE_RE = re.compile(r"TX-LTP-PROFILE\s+(.*)$")
TRAP_PREFIX = "txdbg:trap "
ENTRY_PREFIX = "txdbg:ent "

SYSCALL_NAMES = {
    17: "getcwd",
    23: "dup",
    24: "dup3",
    25: "fcntl",
    29: "ioctl",
    33: "mknodat",
    34: "mkdirat",
    35: "unlinkat",
    36: "symlinkat",
    37: "linkat",
    38: "renameat",
    39: "umount2",
    40: "mount",
    43: "statfs",
    44: "fstatfs",
    45: "truncate",
    46: "ftruncate",
    48: "faccessat",
    49: "chdir",
    50: "fchdir",
    53: "fchmodat",
    55: "fchownat",
    56: "openat",
    57: "close",
    59: "pipe2",
    61: "getdents64",
    62: "lseek",
    63: "read",
    64: "write",
    65: "readv",
    66: "writev",
    67: "pread64",
    68: "pwrite64",
    72: "pselect6",
    73: "ppoll",
    78: "readlinkat",
    79: "newfstatat",
    80: "fstat",
    88: "utimensat",
    93: "exit",
    94: "exit_group",
    95: "waitid",
    96: "set_tid_address",
    98: "futex",
    99: "set_robust_list",
    100: "get_robust_list",
    101: "nanosleep",
    102: "getitimer",
    103: "setitimer",
    113: "clock_gettime",
    114: "clock_getres",
    115: "clock_nanosleep",
    116: "syslog",
    124: "sched_yield",
    129: "kill",
    130: "tkill",
    131: "tgkill",
    132: "sigaltstack",
    133: "rt_sigsuspend",
    134: "rt_sigaction",
    135: "rt_sigprocmask",
    136: "rt_sigpending",
    137: "rt_sigtimedwait",
    138: "rt_sigqueueinfo",
    139: "rt_sigreturn",
    140: "setpriority",
    141: "getpriority",
    153: "times",
    154: "setpgid",
    155: "getpgid",
    156: "getsid",
    157: "setsid",
    160: "uname",
    165: "getrusage",
    166: "umask",
    172: "getpid",
    173: "getppid",
    174: "getuid",
    175: "geteuid",
    176: "getgid",
    177: "getegid",
    178: "gettid",
    179: "sysinfo",
    198: "socket",
    200: "bind",
    201: "listen",
    202: "accept",
    203: "connect",
    204: "getsockname",
    205: "getpeername",
    206: "sendto",
    207: "recvfrom",
    208: "setsockopt",
    209: "getsockopt",
    210: "shutdown",
    211: "sendmsg",
    212: "recvmsg",
    213: "epoll_create1",
    214: "brk",
    215: "munmap",
    216: "mremap",
    220: "clone",
    221: "execve",
    222: "mmap",
    226: "mprotect",
    227: "msync",
    233: "madvise",
    242: "accept4",
    260: "wait4",
    261: "prlimit64",
    276: "renameat2",
    278: "getrandom",
    291: "statx",
    435: "clone3",
    439: "faccessat2",
}


@dataclass
class Event:
    source: str
    command: str
    argv: str
    start_ts: int
    start_line: int
    phase: str
    end_ts: int | None = None
    end_line: int | None = None
    rc: int | None = None
    children: list[int] = field(default_factory=list)

    @property
    def complete(self) -> bool:
        return self.end_ts is not None

    @property
    def duration(self) -> int:
        if self.end_ts is None:
            return 0
        return max(0, self.end_ts - self.start_ts)

    @property
    def label(self) -> str:
        return normalize_label(self.command, self.argv)


@dataclass
class PhaseMarker:
    source: str
    command: str
    name: str
    ts: int
    line: int
    parent_id: int | None
    phase: str


@dataclass
class TrapEvent:
    line: int
    n: int
    kind: str
    pc: int
    syscall_nr: int | None = None
    a0: int | None = None
    a1: int | None = None
    a2: int | None = None
    ret: int | None = None
    entry_line: int | None = None
    stval: int | None = None
    ra: int | None = None

    @property
    def syscall_name(self) -> str:
        if self.syscall_nr is None:
            return ""
        return SYSCALL_NAMES.get(self.syscall_nr, f"sys_{self.syscall_nr}")

    @property
    def signed_ret(self) -> int | None:
        if self.ret is None:
            return None
        if self.ret >= (1 << 63):
            return self.ret - (1 << 64)
        return self.ret


@dataclass
class EntryEvent:
    line: int
    n: int
    a0: int


@dataclass
class ProfileMarker:
    line: int
    counters: dict[str, int]


def strip_ansi(line: str) -> str:
    return ANSI_RE.sub("", line).replace("\r", "")


def parse_hex_kv(text: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for token in text.split():
        if "=" not in token:
            continue
        key, raw_value = token.split("=", 1)
        value = raw_value.strip()
        if value.startswith("0x"):
            value = value[2:]
        try:
            out[key] = int(value, 16)
        except ValueError:
            continue
    return out


def parse_trap_line(line: str, lineno: int) -> TrapEvent | None:
    if not line.startswith(TRAP_PREFIX):
        return None
    rest = line[len(TRAP_PREFIX) :]
    kv = parse_hex_kv(rest)
    kind = ""
    for token in rest.split():
        if token.startswith("kind="):
            kind = token.split("=", 1)[1]
            break
    if not kind:
        return None
    if kind == "SY":
        return TrapEvent(
            line=lineno,
            n=kv.get("n", 0),
            kind=kind,
            pc=kv.get("pc", 0),
            syscall_nr=kv.get("a7", 0),
            a0=kv.get("a0", 0),
            a1=kv.get("a1", 0),
            a2=kv.get("a2", 0),
        )
    return TrapEvent(
        line=lineno,
        n=kv.get("n", 0),
        kind=kind,
        pc=kv.get("pc", 0),
        stval=kv.get("stval", 0),
        ra=kv.get("ra", 0),
    )


def parse_entry_line(line: str, lineno: int) -> EntryEvent | None:
    if not line.startswith(ENTRY_PREFIX):
        return None
    kv = parse_hex_kv(line[len(ENTRY_PREFIX) :])
    return EntryEvent(line=lineno, n=kv.get("n", 0), a0=kv.get("a0", 0))


def parse_decimal_kv(text: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for token in text.split():
        if "=" not in token:
            continue
        key, raw_value = token.split("=", 1)
        try:
            out[key] = int(raw_value, 10)
        except ValueError:
            continue
    return out


def normalize_label(command: str, argv: str) -> str:
    words = argv.split()
    if command == "ip":
        if len(words) >= 2 and words[0] in {"-4", "-6"}:
            words = words[1:]
        if len(words) >= 2 and words[0] == "neigh":
            if words[1] == "show":
                return "ip neigh show"
            if words[1] == "del":
                return "ip neigh del"
            if words[1] in {"add", "replace"}:
                return f"ip neigh {words[1]}"
        if len(words) >= 2:
            return f"ip {words[0]} {words[1]}"
        if words:
            return f"ip {words[0]}"
        return "ip"
    if command == "grep":
        if len(words) >= 2 and words[0].startswith("-"):
            return f"grep {words[0]} {words[1]}"
        if words:
            return f"grep {words[0]}"
        return "grep"
    if command == "ping":
        return "ping"
    if command == "arp":
        if words:
            return f"arp {words[0]}"
        return "arp"
    if command == "tst_ns_exec":
        if len(words) >= 4 and words[2] == "sh" and words[3] == "-c":
            inner = " ".join(words[4:])
            return "tst_ns_exec sh -c " + inner[:80]
        if len(words) >= 3:
            return f"tst_ns_exec {' '.join(words[2:4])}"
        return "tst_ns_exec"
    if command in {"cat", "cut", "id", "ln", "mkdir", "mount", "readlink", "seq", "sysctl"}:
        if words:
            return f"{command} {' '.join(words[:2])}"
        return command
    return command


def phase_name_label(name: str) -> str:
    if name.startswith("neigh-show "):
        return "show " + name.removeprefix("neigh-show ")
    if name.startswith("neigh-del "):
        return "del " + name.removeprefix("neigh-del ")
    return name


def parse_log(path: Path) -> dict[str, object]:
    events: list[Event] = []
    phases: list[PhaseMarker] = []
    traps: list[TrapEvent] = []
    entries: dict[int, EntryEvent] = {}
    profiles: list[ProfileMarker] = []
    open_by_key: dict[tuple[str, str], list[int]] = collections.defaultdict(list)
    active: list[int] = []
    runtime: list[dict[str, object]] = []
    stress_line: int | None = None
    stress_ts: int | None = None
    stress_command: str | None = None

    for lineno, raw in enumerate(path.read_text(errors="ignore").splitlines(), 1):
        line = strip_ansi(raw)
        trap = parse_trap_line(line.strip(), lineno)
        if trap is not None:
            traps.append(trap)
            continue
        entry = parse_entry_line(line.strip(), lineno)
        if entry is not None:
            entries[entry.n] = entry
            continue
        m = PROFILE_RE.search(line)
        if m:
            profiles.append(ProfileMarker(lineno, parse_decimal_kv(m.group(1))))
            continue

        stress = STRESS_RE.search(line)
        if stress and stress_line is None:
            stress_line = lineno
            stress_command = stress.group(1)
            # Use the previous event timestamp as a coarse marker if the LTP
            # line itself has no timestamp.
            stress_ts = events[-1].start_ts if events else None

        m = RUNTIME_BEGIN_RE.search(line)
        if m:
            runtime.append(
                {
                    "case": m.group(1),
                    "start_ts": int(m.group(2)),
                    "start_line": lineno,
                    "end_ts": None,
                    "end_line": None,
                    "rc": None,
                }
            )
        m = RUNTIME_END_RE.search(line)
        if m:
            for entry in reversed(runtime):
                if entry["case"] == m.group(1) and entry["end_ts"] is None:
                    entry["rc"] = int(m.group(2))
                    entry["end_ts"] = int(m.group(3))
                    entry["end_line"] = lineno
                    break

        m = BEGIN_RE.search(line)
        if m:
            source = m.group(1).lower()
            command = m.group(2)
            start_ts = int(m.group(3))
            argv = (m.group(4) or "").strip()
            phase = "post-stress" if stress_line is not None and lineno > stress_line else "pre-stress"
            event = Event(source, command, argv, start_ts, lineno, phase)
            event_id = len(events)
            for parent_id in active:
                parent = events[parent_id]
                if parent.end_ts is None:
                    parent.children.append(event_id)
            events.append(event)
            open_by_key[(source, command)].append(event_id)
            active.append(event_id)
            continue

        m = END_RE.search(line)
        if m:
            source = m.group(1).lower()
            command = m.group(2)
            rc = int(m.group(3))
            end_ts = int(m.group(4))
            stack = open_by_key.get((source, command), [])
            if stack:
                event_id = stack.pop()
                event = events[event_id]
                event.rc = rc
                event.end_ts = end_ts
                event.end_line = lineno
                if event_id in active:
                    active.remove(event_id)
            continue

        m = PHASE_RE.search(line)
        if m:
            source = m.group(1).lower()
            command = m.group(2)
            marker_ts = int(m.group(3))
            name = (m.group(4) or "").strip()
            parent_id = None
            for candidate_id in reversed(active):
                candidate = events[candidate_id]
                if candidate.source == source and candidate.command == command:
                    parent_id = candidate_id
                    break
            phase = "post-stress" if stress_line is not None and lineno > stress_line else "pre-stress"
            phases.append(PhaseMarker(source, command, name, marker_ts, lineno, parent_id, phase))

    for trap in traps:
        if trap.kind != "SY":
            continue
        entry = entries.get(trap.n + 1)
        if entry is not None:
            trap.ret = entry.a0
            trap.entry_line = entry.line

    return {
        "events": events,
        "phases": phases,
        "traps": traps,
        "entries": list(entries.values()),
        "profiles": profiles,
        "runtime": runtime,
        "stress_line": stress_line,
        "stress_ts": stress_ts,
        "stress_command": stress_command,
        "unmatched": [event for event in events if not event.complete],
    }


def summarize(events: list[Event], phase: str | None = None) -> list[dict[str, object]]:
    groups: dict[str, list[Event]] = collections.defaultdict(list)
    for event in events:
        if not event.complete:
            continue
        if phase is not None and event.phase != phase:
            continue
        groups[event.label].append(event)

    rows: list[dict[str, object]] = []
    for label, items in groups.items():
        durations = [event.duration for event in items]
        rows.append(
            {
                "label": label,
                "count": len(items),
                "total_s": sum(durations),
                "avg_s": statistics.mean(durations) if durations else 0,
                "max_s": max(durations) if durations else 0,
                "rc": dict(collections.Counter(str(event.rc) for event in items)),
                "sample": items[0].argv,
            }
        )
    rows.sort(key=lambda row: (-int(row["total_s"]), -int(row["max_s"]), str(row["label"])))
    return rows


def summarize_phase_spans(
    events: list[Event],
    phases: list[PhaseMarker],
    phase: str | None = None,
) -> list[dict[str, object]]:
    markers_by_parent: dict[int, list[PhaseMarker]] = collections.defaultdict(list)
    for marker in phases:
        if marker.parent_id is None:
            continue
        if phase is not None and marker.phase != phase:
            continue
        markers_by_parent[marker.parent_id].append(marker)

    groups: dict[str, list[int]] = collections.defaultdict(list)
    for parent_id, markers in markers_by_parent.items():
        if parent_id >= len(events):
            continue
        event = events[parent_id]
        if not event.complete:
            continue
        sorted_markers = sorted(markers, key=lambda marker: marker.line)
        for prev, current in zip(sorted_markers, sorted_markers[1:]):
            label = (
                f"{event.label} :: {phase_name_label(prev.name)}"
                f" -> {phase_name_label(current.name)}"
            )
            groups[label].append(max(0, current.ts - prev.ts))
        if sorted_markers and event.end_ts is not None:
            last = sorted_markers[-1]
            label = f"{event.label} :: {phase_name_label(last.name)} -> command-end"
            groups[label].append(max(0, event.end_ts - last.ts))

    rows: list[dict[str, object]] = []
    for label, durations in groups.items():
        rows.append(
            {
                "label": label,
                "count": len(durations),
                "total_s": sum(durations),
                "avg_s": statistics.mean(durations) if durations else 0,
                "max_s": max(durations) if durations else 0,
            }
        )
    rows.sort(key=lambda row: (-int(row["total_s"]), -int(row["max_s"]), str(row["label"])))
    return rows


def loop_rows(events: list[Event], stress_line: int | None) -> list[dict[str, object]]:
    if stress_line is None:
        return []
    post = [event for event in events if event.start_line > stress_line and event.complete]
    starts = [event for event in post if event.command == "ping"]
    rows: list[dict[str, object]] = []
    for idx, start in enumerate(starts, 1):
        next_line = starts[idx].start_line if idx < len(starts) else None
        members = [
            event
            for event in post
            if event.start_line >= start.start_line
            and (next_line is None or event.start_line < next_line)
        ]
        last_end = max((event.end_ts or event.start_ts) for event in members) if members else start.start_ts
        by_label = collections.Counter()
        by_label_time = collections.Counter()
        for event in members:
            by_label[event.label] += 1
            by_label_time[event.label] += event.duration
        rows.append(
            {
                "iteration": idx,
                "start_ts": start.start_ts,
                "line": start.start_line,
                "wall_s": max(0, last_end - start.start_ts),
                "commands": dict(by_label),
                "time_by_command": dict(by_label_time),
            }
        )
    return rows


def active_event_ids_at_line(events: list[Event], line: int, phase: str | None) -> list[int]:
    active_ids: list[int] = []
    for event_id, event in enumerate(events):
        if not event.complete or event.end_line is None:
            continue
        if phase is not None and event.phase != phase:
            continue
        if event.start_line <= line <= event.end_line:
            active_ids.append(event_id)
    return active_ids


def counter_text(counter: collections.Counter[str], limit: int = 6) -> str:
    if not counter:
        return "-"
    return ", ".join(f"{key}:{value}" for key, value in counter.most_common(limit))


def trap_counters(traps: list[TrapEvent]) -> dict[str, object]:
    syscall_counts: collections.Counter[str] = collections.Counter()
    fault_counts: collections.Counter[str] = collections.Counter()
    key_counts: collections.Counter[str] = collections.Counter()
    read_one_byte = 0
    read_requested = 0
    read_nonzero_returns = 0
    read_zero_returns = 0
    read_again_returns = 0
    wait4_zero_returns = 0
    wait4_child_returns = 0

    for trap in traps:
        if trap.kind == "SY":
            name = trap.syscall_name
            syscall_counts[name] += 1
            if name in {
                "read",
                "write",
                "ppoll",
                "pselect6",
                "wait4",
                "clone",
                "execve",
                "pipe2",
                "openat",
                "close",
                "newfstatat",
                "mmap",
                "mprotect",
                "munmap",
                "brk",
                "futex",
                "sched_yield",
            }:
                key_counts[name] += 1
            if name == "read":
                requested = trap.a2 or 0
                read_requested += requested
                if requested <= 1:
                    read_one_byte += 1
                signed_ret = trap.signed_ret
                if signed_ret is not None:
                    if signed_ret > 0:
                        read_nonzero_returns += 1
                    elif signed_ret == 0:
                        read_zero_returns += 1
                    elif signed_ret == -11:
                        read_again_returns += 1
            if name == "wait4":
                signed_ret = trap.signed_ret
                if signed_ret == 0:
                    wait4_zero_returns += 1
                elif signed_ret is not None and signed_ret > 0:
                    wait4_child_returns += 1
        else:
            fault_counts[trap.kind] += 1

    return {
        "traps": len(traps),
        "syscalls": sum(syscall_counts.values()),
        "faults": sum(fault_counts.values()),
        "syscall_counts": syscall_counts,
        "fault_counts": fault_counts,
        "key_counts": key_counts,
        "read_one_byte": read_one_byte,
        "read_requested": read_requested,
        "read_nonzero_returns": read_nonzero_returns,
        "read_zero_returns": read_zero_returns,
        "read_again_returns": read_again_returns,
        "wait4_zero_returns": wait4_zero_returns,
        "wait4_child_returns": wait4_child_returns,
    }


def summarize_traps_by_command(
    events: list[Event],
    traps: list[TrapEvent],
    phase: str | None = None,
) -> list[dict[str, object]]:
    interval_traps: dict[int, list[TrapEvent]] = collections.defaultdict(list)
    exclusive_traps: dict[int, list[TrapEvent]] = collections.defaultdict(list)
    for trap in traps:
        active_ids = active_event_ids_at_line(events, trap.line, phase)
        for event_id in active_ids:
            interval_traps[event_id].append(trap)
        if len(active_ids) == 1:
            exclusive_traps[active_ids[0]].append(trap)

    grouped_events: dict[str, list[int]] = collections.defaultdict(list)
    for event_id, event in enumerate(events):
        if not event.complete:
            continue
        if phase is not None and event.phase != phase:
            continue
        grouped_events[event.label].append(event_id)

    rows: list[dict[str, object]] = []
    for label, event_ids in grouped_events.items():
        durations = [events[event_id].duration for event_id in event_ids]
        interval = [trap for event_id in event_ids for trap in interval_traps.get(event_id, [])]
        exclusive = [trap for event_id in event_ids for trap in exclusive_traps.get(event_id, [])]
        interval_stats = trap_counters(interval)
        exclusive_stats = trap_counters(exclusive)
        rows.append(
            {
                "label": label,
                "count": len(event_ids),
                "total_s": sum(durations),
                "avg_s": statistics.mean(durations) if durations else 0,
                "max_s": max(durations) if durations else 0,
                "interval_traps": interval_stats["traps"],
                "interval_syscalls": interval_stats["syscalls"],
                "interval_faults": interval_stats["faults"],
                "interval_top_syscalls": dict(interval_stats["syscall_counts"]),
                "interval_top_faults": dict(interval_stats["fault_counts"]),
                "exclusive_traps": exclusive_stats["traps"],
                "exclusive_syscalls": exclusive_stats["syscalls"],
                "exclusive_faults": exclusive_stats["faults"],
                "exclusive_top_syscalls": dict(exclusive_stats["syscall_counts"]),
                "key_syscalls": dict(interval_stats["key_counts"]),
                "read_one_byte": interval_stats["read_one_byte"],
                "read_requested": interval_stats["read_requested"],
                "read_nonzero_returns": interval_stats["read_nonzero_returns"],
                "read_zero_returns": interval_stats["read_zero_returns"],
                "read_again_returns": interval_stats["read_again_returns"],
                "wait4_zero_returns": interval_stats["wait4_zero_returns"],
                "wait4_child_returns": interval_stats["wait4_child_returns"],
            }
        )
    rows.sort(
        key=lambda row: (
            -int(row["total_s"]),
            -int(row["interval_traps"]),
            -int(row["exclusive_traps"]),
            str(row["label"]),
        )
    )
    return rows


def event_trap_rows(
    events: list[Event],
    traps: list[TrapEvent],
    phase: str | None = None,
) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for event in events:
        if not event.complete or event.end_line is None:
            continue
        if phase is not None and event.phase != phase:
            continue
        interval = [trap for trap in traps if event.start_line <= trap.line <= event.end_line]
        if not interval:
            continue
        stats = trap_counters(interval)
        rows.append(
            {
                "line": event.start_line,
                "label": event.label,
                "duration_s": event.duration,
                "traps": stats["traps"],
                "syscalls": stats["syscalls"],
                "faults": stats["faults"],
                "top_syscalls": dict(stats["syscall_counts"]),
                "top_faults": dict(stats["fault_counts"]),
                "key_syscalls": dict(stats["key_counts"]),
                "read_one_byte": stats["read_one_byte"],
                "read_requested": stats["read_requested"],
                "argv": event.argv,
            }
        )
    rows.sort(
        key=lambda row: (
            -int(row["duration_s"]),
            -int(row["traps"]),
            str(row["label"]),
            int(row["line"]),
        )
    )
    return rows


def loop_trap_rows(
    events: list[Event],
    traps: list[TrapEvent],
    stress_line: int | None,
) -> list[dict[str, object]]:
    if stress_line is None:
        return []
    post = [event for event in events if event.start_line > stress_line and event.complete]
    starts = [event for event in post if event.command == "ping"]
    rows: list[dict[str, object]] = []
    for idx, start in enumerate(starts, 1):
        next_line = starts[idx].start_line if idx < len(starts) else None
        loop_traps = [
            trap
            for trap in traps
            if trap.line >= start.start_line and (next_line is None or trap.line < next_line)
        ]
        stats = trap_counters(loop_traps)
        rows.append(
            {
                "iteration": idx,
                "line": start.start_line,
                "traps": stats["traps"],
                "syscalls": stats["syscalls"],
                "faults": stats["faults"],
                "top_syscalls": dict(stats["syscall_counts"]),
                "top_faults": dict(stats["fault_counts"]),
                "key_syscalls": dict(stats["key_counts"]),
                "read_one_byte": stats["read_one_byte"],
                "read_requested": stats["read_requested"],
            }
        )
    return rows


PROFILE_KEYS = [
    "syscalls",
    "faults",
    "iPF",
    "lPF",
    "sPF",
    "uPF",
    "read",
    "read_le1",
    "read_requested",
    "write",
    "write_requested",
    "ppoll",
    "pselect6",
    "wait4",
    "clone",
    "execve",
    "openat",
    "close",
    "dup3",
    "fcntl",
    "pipe2",
    "newfstatat",
    "brk",
    "mmap",
    "mprotect",
    "munmap",
]


def first_profile_after(profiles: list[ProfileMarker], line: int) -> ProfileMarker | None:
    for profile in profiles:
        if profile.line > line:
            return profile
    return None


def last_profile_before(profiles: list[ProfileMarker], line: int) -> ProfileMarker | None:
    for profile in reversed(profiles):
        if profile.line < line:
            return profile
    return None


def profile_near_marker(profiles: list[ProfileMarker], line: int) -> ProfileMarker | None:
    # The non-invasive kernel-console sampler prints just before the marker.
    # Older experiments appended after the marker, so keep a narrow fallback.
    before = last_profile_before(profiles, line)
    if before is not None and line - before.line <= 2:
        return before
    after = first_profile_after(profiles, line)
    if after is not None and after.line - line <= 2:
        return after
    return before


def profile_delta(begin: ProfileMarker, end: ProfileMarker) -> dict[str, int]:
    return {
        key: end.counters.get(key, 0) - begin.counters.get(key, 0)
        for key in PROFILE_KEYS
    }


def summarize_profiles_by_command(
    events: list[Event],
    profiles: list[ProfileMarker],
    phase: str | None = None,
) -> list[dict[str, object]]:
    if not profiles:
        return []
    groups: dict[str, list[tuple[Event, dict[str, int]]]] = collections.defaultdict(list)
    for event in events:
        if not event.complete or event.end_line is None:
            continue
        if phase is not None and event.phase != phase:
            continue
        begin_profile = profile_near_marker(profiles, event.start_line)
        end_profile = profile_near_marker(profiles, event.end_line)
        if begin_profile is None or end_profile is None:
            continue
        groups[event.label].append((event, profile_delta(begin_profile, end_profile)))

    rows: list[dict[str, object]] = []
    for label, items in groups.items():
        durations = [event.duration for event, _delta in items]
        totals = collections.Counter()
        for _event, delta in items:
            totals.update(delta)
        rows.append(
            {
                "label": label,
                "count": len(items),
                "total_s": sum(durations),
                "avg_s": statistics.mean(durations) if durations else 0,
                "max_s": max(durations) if durations else 0,
                "profile": dict(totals),
            }
        )
    rows.sort(
        key=lambda row: (
            -int(row["total_s"]),
            -int(row["profile"].get("faults", 0)),  # type: ignore[union-attr]
            -int(row["profile"].get("syscalls", 0)),  # type: ignore[union-attr]
            str(row["label"]),
        )
    )
    return rows


def summarize_profiles_by_phase_span(
    events: list[Event],
    phases: list[PhaseMarker],
    profiles: list[ProfileMarker],
    phase: str | None = None,
) -> list[dict[str, object]]:
    if not profiles:
        return []

    markers_by_parent: dict[int, list[PhaseMarker]] = collections.defaultdict(list)
    for marker in phases:
        if marker.parent_id is None:
            continue
        if phase is not None and marker.phase != phase:
            continue
        markers_by_parent[marker.parent_id].append(marker)

    groups: dict[str, list[tuple[int, dict[str, int]]]] = collections.defaultdict(list)
    for parent_id, markers in markers_by_parent.items():
        if parent_id >= len(events):
            continue
        event = events[parent_id]
        if not event.complete or event.end_line is None or event.end_ts is None:
            continue
        sorted_markers = sorted(markers, key=lambda marker: marker.line)
        for prev, current in zip(sorted_markers, sorted_markers[1:]):
            begin_profile = profile_near_marker(profiles, prev.line)
            end_profile = profile_near_marker(profiles, current.line)
            if begin_profile is None or end_profile is None:
                continue
            label = (
                f"{event.label} :: {phase_name_label(prev.name)}"
                f" -> {phase_name_label(current.name)}"
            )
            groups[label].append(
                (
                    max(0, current.ts - prev.ts),
                    profile_delta(begin_profile, end_profile),
                )
            )
        if sorted_markers:
            last = sorted_markers[-1]
            begin_profile = profile_near_marker(profiles, last.line)
            end_profile = profile_near_marker(profiles, event.end_line)
            if begin_profile is None or end_profile is None:
                continue
            label = f"{event.label} :: {phase_name_label(last.name)} -> command-end"
            groups[label].append((max(0, event.end_ts - last.ts), profile_delta(begin_profile, end_profile)))

    rows: list[dict[str, object]] = []
    for label, items in groups.items():
        durations = [duration for duration, _delta in items]
        totals = collections.Counter()
        for _duration, delta in items:
            totals.update(delta)
        rows.append(
            {
                "label": label,
                "count": len(items),
                "total_s": sum(durations),
                "avg_s": statistics.mean(durations) if durations else 0,
                "max_s": max(durations) if durations else 0,
                "profile": dict(totals),
            }
        )
    rows.sort(
        key=lambda row: (
            -int(row["total_s"]),
            -int(row["profile"].get("ppoll", 0)),  # type: ignore[union-attr]
            -int(row["profile"].get("read_le1", 0)),  # type: ignore[union-attr]
            -int(row["profile"].get("faults", 0)),  # type: ignore[union-attr]
            str(row["label"]),
        )
    )
    return rows


def print_table(rows: list[dict[str, object]], limit: int) -> None:
    print("label | count | total_s | avg_s | max_s | rc | sample")
    print("--- | ---: | ---: | ---: | ---: | --- | ---")
    for row in rows[:limit]:
        print(
            f"{row['label']} | {row['count']} | {row['total_s']} | "
            f"{float(row['avg_s']):.2f} | {row['max_s']} | "
            f"{json.dumps(row['rc'], sort_keys=True)} | {row['sample']}"
        )


def print_phase_table(rows: list[dict[str, object]], limit: int) -> None:
    print("span | count | total_s | avg_s | max_s")
    print("--- | ---: | ---: | ---: | ---:")
    for row in rows[:limit]:
        print(
            f"{row['label']} | {row['count']} | {row['total_s']} | "
            f"{float(row['avg_s']):.2f} | {row['max_s']}"
        )


def print_trap_table(rows: list[dict[str, object]], limit: int) -> None:
    print(
        "label | count | total_s | traps | syscalls | faults | "
        "exclusive_traps | key syscalls | top syscalls | top faults | read<=1"
    )
    print("--- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | ---:")
    for row in rows[:limit]:
        key_syscalls = counter_text(collections.Counter(row["key_syscalls"]))  # type: ignore[arg-type]
        top_syscalls = counter_text(
            collections.Counter(row["interval_top_syscalls"]), 8  # type: ignore[arg-type]
        )
        top_faults = counter_text(collections.Counter(row["interval_top_faults"]))  # type: ignore[arg-type]
        print(
            f"{row['label']} | {row['count']} | {row['total_s']} | "
            f"{row['interval_traps']} | {row['interval_syscalls']} | "
            f"{row['interval_faults']} | {row['exclusive_traps']} | "
            f"{key_syscalls} | {top_syscalls} | {top_faults} | {row['read_one_byte']}"
        )


def print_event_trap_table(rows: list[dict[str, object]], limit: int) -> None:
    print("line | label | duration_s | traps | syscalls | faults | key syscalls | top syscalls | top faults")
    print("---: | --- | ---: | ---: | ---: | ---: | --- | --- | ---")
    for row in rows[:limit]:
        key_syscalls = counter_text(collections.Counter(row["key_syscalls"]))  # type: ignore[arg-type]
        top_syscalls = counter_text(collections.Counter(row["top_syscalls"]), 8)  # type: ignore[arg-type]
        top_faults = counter_text(collections.Counter(row["top_faults"]))  # type: ignore[arg-type]
        print(
            f"{row['line']} | {row['label']} | {row['duration_s']} | "
            f"{row['traps']} | {row['syscalls']} | {row['faults']} | "
            f"{key_syscalls} | {top_syscalls} | {top_faults}"
        )


def print_profile_table(rows: list[dict[str, object]], limit: int) -> None:
    print(
        "label | count | total_s | syscalls | faults | iPF/lPF/sPF | "
        "read<=1 | key syscalls"
    )
    print("--- | ---: | ---: | ---: | ---: | --- | ---: | ---")
    for row in rows[:limit]:
        profile: dict[str, int] = row["profile"]  # type: ignore[assignment]
        key_counts = {
            key: profile.get(key, 0)
            for key in [
                "read",
                "write",
                "ppoll",
                "pselect6",
                "wait4",
                "clone",
                "execve",
                "openat",
                "close",
                "dup3",
                "fcntl",
                "pipe2",
                "newfstatat",
                "brk",
                "mmap",
                "mprotect",
                "munmap",
            ]
            if profile.get(key, 0)
        }
        print(
            f"{row['label']} | {row['count']} | {row['total_s']} | "
            f"{profile.get('syscalls', 0)} | {profile.get('faults', 0)} | "
            f"{profile.get('iPF', 0)}/{profile.get('lPF', 0)}/{profile.get('sPF', 0)} | "
            f"{profile.get('read_le1', 0)} | "
            f"{counter_text(collections.Counter(key_counts), 10)}"
        )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--limit", type=int, default=24)
    parser.add_argument("--phase", choices=["pre-stress", "post-stress"], default=None)
    parser.add_argument("--json", action="store_true", dest="json_out")
    args = parser.parse_args(argv)

    parsed = parse_log(args.log)
    events: list[Event] = parsed["events"]  # type: ignore[assignment]
    phases: list[PhaseMarker] = parsed["phases"]  # type: ignore[assignment]
    traps: list[TrapEvent] = parsed["traps"]  # type: ignore[assignment]
    profiles: list[ProfileMarker] = parsed["profiles"]  # type: ignore[assignment]
    rows = summarize(events, args.phase)
    phase_spans = summarize_phase_spans(events, phases, args.phase)
    loops = loop_rows(events, parsed["stress_line"])  # type: ignore[arg-type]
    trap_rows = summarize_traps_by_command(events, traps, args.phase)
    trap_events = event_trap_rows(events, traps, args.phase)
    trap_loops = loop_trap_rows(events, traps, parsed["stress_line"])  # type: ignore[arg-type]
    profile_rows = summarize_profiles_by_command(events, profiles, args.phase)
    phase_profile_rows = summarize_profiles_by_phase_span(events, phases, profiles, args.phase)
    unmatched: list[Event] = parsed["unmatched"]  # type: ignore[assignment]

    if args.json_out:
        print(
            json.dumps(
                {
                    "log": str(args.log),
                    "events": len(events),
                    "phase_markers": len(phases),
                    "traps": len(traps),
                    "profiles": len(profiles),
                    "syscalls": sum(1 for trap in traps if trap.kind == "SY"),
                    "faults": sum(1 for trap in traps if trap.kind != "SY"),
                    "stress_line": parsed["stress_line"],
                    "stress_command": parsed["stress_command"],
                    "summary": rows,
                    "phase_spans": phase_spans,
                    "loops": loops,
                    "trap_summary": trap_rows,
                    "trap_events": trap_events,
                    "trap_loops": trap_loops,
                    "profile_summary": profile_rows,
                    "phase_profile_summary": phase_profile_rows,
                    "unmatched": [
                        {
                            "source": event.source,
                            "command": event.command,
                            "argv": event.argv,
                            "start_ts": event.start_ts,
                            "start_line": event.start_line,
                        }
                        for event in unmatched
                    ],
                    "phases": [
                        {
                            "source": marker.source,
                            "command": marker.command,
                            "name": marker.name,
                            "ts": marker.ts,
                            "line": marker.line,
                            "parent_id": marker.parent_id,
                            "phase": marker.phase,
                        }
                        for marker in phases
                    ],
                    "runtime": parsed["runtime"],
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0

    print(f"log: {args.log}")
    print(
        f"events: {len(events)} complete={sum(1 for event in events if event.complete)} "
        f"unmatched={len(unmatched)} phase_markers={len(phases)}"
    )
    if traps:
        syscall_count = sum(1 for trap in traps if trap.kind == "SY")
        print(f"traps: {len(traps)} syscalls={syscall_count} faults={len(traps) - syscall_count}")
    if profiles:
        print(f"profiles: {len(profiles)}")
    print(f"stress: line={parsed['stress_line']} command={parsed['stress_command']}")
    print()
    print(f"command summary phase={args.phase or 'all'}")
    print_table(rows, args.limit)
    if phase_spans:
        print()
        print(f"phase span summary phase={args.phase or 'all'}")
        print_phase_table(phase_spans, args.limit)
    if profile_rows:
        print()
        print(f"counter profile summary phase={args.phase or 'all'}")
        print_profile_table(profile_rows, args.limit)
    if phase_profile_rows:
        print()
        print(f"phase counter profile summary phase={args.phase or 'all'}")
        print_profile_table(phase_profile_rows, args.limit)
    if trap_rows and traps:
        print()
        print(
            f"trap/syscall interval summary phase={args.phase or 'all'} "
            "(interval counts may overlap for pipelines)"
        )
        print_trap_table(trap_rows, args.limit)
    if trap_events:
        print()
        print(f"slowest command intervals with trap detail phase={args.phase or 'all'}")
        print_event_trap_table(trap_events, args.limit)
    if loops:
        print()
        print("post-stress loop summary")
        print("iter | line | wall_s | top commands")
        print("---: | ---: | ---: | ---")
        for row in loops[: args.limit]:
            times = collections.Counter(row["time_by_command"])  # type: ignore[arg-type]
            top = ", ".join(f"{key}:{value}s" for key, value in times.most_common(5))
            print(f"{row['iteration']} | {row['line']} | {row['wall_s']} | {top}")
    if trap_loops:
        print()
        print("post-stress loop trap summary")
        print("iter | line | traps | syscalls | faults | key syscalls | top syscalls | top faults")
        print("---: | ---: | ---: | ---: | ---: | --- | --- | ---")
        for row in trap_loops[: args.limit]:
            key_syscalls = counter_text(collections.Counter(row["key_syscalls"]))  # type: ignore[arg-type]
            top_syscalls = counter_text(collections.Counter(row["top_syscalls"]), 8)  # type: ignore[arg-type]
            top_faults = counter_text(collections.Counter(row["top_faults"]))  # type: ignore[arg-type]
            print(
                f"{row['iteration']} | {row['line']} | {row['traps']} | "
                f"{row['syscalls']} | {row['faults']} | {key_syscalls} | "
                f"{top_syscalls} | {top_faults}"
            )
    if unmatched:
        print()
        print("unmatched begins")
        for event in unmatched[: args.limit]:
            print(f"line {event.start_line}: {event.source} {event.command} {event.argv}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
